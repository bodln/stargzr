use crate::error::{PlayerError, PlayerResult};
use crate::logging::init_logging;
use crate::rate_limit::RateLimiter;
use crate::validation::{SessionId, validate_song_index};
use askama::Template;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{
    Router,
    http::{HeaderMap, header},
    response::Html,
    response::Response,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use hyper::StatusCode;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Clone)]
struct SongInfo {
    filename: String,
    size: u64,
}

/// Represents a single user's PRIVATE player state
struct PlayerSession {
    current_index: usize,
    last_activity: std::time::Instant, // Used to measure the age of the session
}

/// Represents a broadcaster's state (the authoritative source)
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BroadcastState {
    broadcaster_id: String,
    song_index: usize,
    // We add this field so we can return the song name in analytics
    song_name: String,
    playback_time: f64, // Current position in seconds
    is_playing: bool,
    server_timestamp_ms: u128, // When this state was recorded
}

pub struct AppState {
    playlist: Arc<Vec<SongInfo>>,
    music_folder: Arc<PathBuf>,

    sessions: RwLock<HashMap<String, PlayerSession>>,

    /// Maps broadcaster_id(session id) -> their current state
    broadcast_states: RwLock<HashMap<String, BroadcastState>>,

    /// Global broadcast channel for system-wide announcements.
    /// Used for BroadcasterOnline/Offline messages that all clients should see,
    /// regardless of which broadcaster they're tuned to.
    global_broadcast_tx: broadcast::Sender<RadioMessage>,

    /// Per-broadcaster channels for targeted playback sync.
    /// Each broadcaster has their own channel that only their listeners subscribe to.
    broadcast_channels: RwLock<HashMap<String, broadcast::Sender<RadioMessage>>>,

    // Real-time analytics counters
    active_connections: AtomicUsize,
    active_broadcasters: AtomicUsize,
    active_listeners: AtomicUsize,
}

/// Helper type for cleaner function signatures
type SharedState = Arc<AppState>;

/// Messages sent over WebSocket
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum RadioMessage {
    /// Broadcaster distributes this when play/pause/seek/next/prev happens
    Sync {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
        server_timestamp_ms: u128,
    },

    /// Broadcaster sends this every 2-3 seconds
    Heartbeat {
        broadcaster_id: String,
        playback_time: f64,
    },

    /// Listener sends this for initial tune in to broadcaster, gets Sync back
    TuneIn {
        broadcaster_id: String,
    },

    /// Listener tune out
    TuneOut,

    /// Broadcaster sends this on play/pause/seek/next/prev, and server sends Sync to all tuned in
    BroadcastUpdate {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
    },

    Error {
        message: String,
    },

    /// Explicitly register a broadcaster before they start sending updates
    /// Prevents BroadcasterNotFound errors when listeners try to tune in early
    StartBroadcasting {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
    },

    /// Explicitly unregister a broadcaster and notify listeners
    StopBroadcasting {
        broadcaster_id: String,
    },

    /// Notify all clients when a new broadcaster goes live
    BroadcasterOnline {
        broadcaster_id: String,
    },

    /// Notify listeners when their broadcaster disconnects
    BroadcasterOffline {
        broadcaster_id: String,
    },

    Analytics {
        active_connections: usize,
        active_broadcasters: usize,
        active_listeners: usize,
        broadcasters: Vec<BroadcastState>,
    },

    /// Client queries if they're currently broadcasting
    QueryBroadcastState {
        session_id: String,
    },

    /// Server responds with broadcast state
    BroadcastStateResponse {
        session_id: String,
        is_broadcasting: bool,
        current_state: Option<BroadcastState>,
    },
}

#[derive(Template)]
#[template(path = "player.html")]
struct PlayerTemplate {
    current_song: String,
    current_index: usize,
    total_songs: usize,
    session_id: String,
}

#[derive(Template)]
#[template(path = "player_controls.html")]
struct PlayerControlsTemplate {
    current_song: String,
    current_index: usize,
    total_songs: usize,
}

/// Makes returning PlayerTemplate returnable as an axum response
impl IntoResponse for PlayerTemplate {
    fn into_response(self) -> Response {
        match self.render() {
            Ok(html) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(
                    header::SET_COOKIE,
                    format!(
                        "player_session={}; Path=/player; HttpOnly; SameSite=Strict; Max-Age=3600",
                        self.session_id
                    ),
                )
                .body(axum::body::Body::from(html))
                .unwrap(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {}", e),
            )
                .into_response(),
        }
    }
}

/// Makes returning PlayerControlsTemplate returnable as an axum response
impl IntoResponse for PlayerControlsTemplate {
    fn into_response(self) -> Response {
        match self.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {}", e),
            )
                .into_response(),
        }
    }
}

/// Extracts the session id from the request header
fn get_session_id(headers: &HeaderMap) -> String {
    headers
        .get(header::COOKIE)
        .and_then(|cookie| cookie.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let cookie = cookie.trim();
                cookie.strip_prefix("player_session=")
            })
        })
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

/// Based on the session id extracts the current playlist position
fn get_or_create_position(state: &AppState, session_id: &str) -> usize {
    let now = std::time::Instant::now();

    // First try with cheap read lock
    {
        let sessions = state.sessions.read();
        if let Some(session) = sessions.get(session_id) {
            return session.current_index;
        }
    }

    let mut sessions = state.sessions.write();

    // Either returns a valid session or creates a new one,
    // in the case of there not being one or it having expired
    let session = sessions
        .entry(session_id.to_string())
        .or_insert(PlayerSession {
            current_index: 0,
            last_activity: now,
        });

    session.last_activity = now;
    session.current_index
}

/// Changes the playlist position for the session id
fn update_session_index(state: &AppState, session_id: &str, new_index: usize) {
    let mut sessions = state.sessions.write();

    if let Some(session) = sessions.get_mut(session_id) {
        session.current_index = new_index;
        session.last_activity = std::time::Instant::now();
    }
}

/// Changes the playlist position for the broadcaster by session id
fn update_broadcast_index(state: &AppState, session_id: &str, new_index: usize) {
    // Check if this session is broadcasting ( with a cheap read lock)
    {
        let broadcasts = state.broadcast_states.read();
        if !broadcasts.contains_key(session_id) {
            // Not a broadcaster, skip entirely
            return;
        }
    }

    // Only acquire write lock if we know they're broadcasting
    let mut broadcasts = state.broadcast_states.write();
    let server_ts = now_ms();

    if let Some(broadcast) = broadcasts.get_mut(session_id) {
        // Update authoritative state
        broadcast.song_index = new_index;
        broadcast.playback_time = 0.0;
        broadcast.server_timestamp_ms = server_ts;
    }
}

/// Get current server time in milliseconds (for timestamping)
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// Renders the main player page for the current user session.
async fn player_page(State(state): State<SharedState>, headers: HeaderMap) -> PlayerTemplate {
    // Identify the user session (cookie-based, falls back to UUID)
    let session_id = get_session_id(&headers);

    // Fetch or initialize the session's current playlist index
    let current_index = get_or_create_position(&state, &session_id);

    // Resolve the filename for the current song, if any
    let current_song = state
        .playlist
        .get(current_index)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "No songs found".to_string());

    // Render the full player page
    PlayerTemplate {
        current_song,
        current_index,
        total_songs: state.playlist.len(),
        session_id,
    }
}

/// Advances the current session to the next song in the playlist.
async fn next_song(State(state): State<SharedState>, headers: HeaderMap) -> PlayerControlsTemplate {
    // Identify the user session
    let session_id = get_session_id(&headers);

    // Get the current position for this session
    let current_index = get_or_create_position(&state, &session_id);

    // Move forward one song, clamped to the last valid index
    let new_index = (current_index + 1).min(state.playlist.len().saturating_sub(1));

    // Update private playback position
    update_session_index(&state, &session_id, new_index);

    // If this session is a broadcaster, propagate the change to listeners
    update_broadcast_index(&state, &session_id, new_index);

    // Resolve the new current song
    let current_song = state
        .playlist
        .get(new_index)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "No songs found".to_string());

    // Return updated control state (used for partial page updates)
    PlayerControlsTemplate {
        current_song,
        current_index: new_index,
        total_songs: state.playlist.len(),
    }
}

/// Moves the current session to the previous song in the playlist.
async fn prev_song(State(state): State<SharedState>, headers: HeaderMap) -> PlayerControlsTemplate {
    // Identify the user session
    let session_id = get_session_id(&headers);

    // Get the current position for this session
    let current_index = get_or_create_position(&state, &session_id);

    // Move back one song, clamping at index 0
    let new_index = current_index.saturating_sub(1);

    // Update private playback position
    update_session_index(&state, &session_id, new_index);

    // If this session is a broadcaster, propagate the change to listeners
    update_broadcast_index(&state, &session_id, new_index);

    // Resolve the new current song
    let current_song = state
        .playlist
        .get(new_index)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "No songs found".to_string());

    // Return updated control state (used for partial page updates)
    PlayerControlsTemplate {
        current_song,
        current_index: new_index,
        total_songs: state.playlist.len(),
    }
}

/// Query parameters accepted by the `/player/controls` endpoint.
///
/// This is primarily used by the radio client when it needs to
/// re-render the control panel while tuned into a broadcaster.
#[derive(Deserialize)]
struct ControlsQuery {
    broadcaster: Option<String>,
}

/// Returns the current player control state as an HTML fragment.
///
/// This handler is designed to be HTMX-friendly:
/// it returns only the inner player controls markup, not a full page.
async fn player_controls(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<ControlsQuery>,
) -> PlayerControlsTemplate {
    // If a broadcaster ID is provided, attempt to mirror its playback state
    if let Some(broadcaster_id) = query.broadcaster {
        // Broadcast state is shared, read-only, and authoritative for listeners
        if let Some(broadcast) = state.broadcast_states.read().get(&broadcaster_id) {
            let index = broadcast.song_index;

            let song = state
                .playlist
                .get(index)
                .map(|s| s.filename.clone())
                .unwrap_or_else(|| "No songs found".to_string());

            // Return controls reflecting the broadcaster's state
            return PlayerControlsTemplate {
                current_song: song,
                current_index: index,
                total_songs: state.playlist.len(),
            };
        }
    }

    // No broadcaster specified (or broadcaster missing), so render controls
    // based on the caller's own session state.
    let session_id = get_session_id(&headers);
    let index = get_or_create_position(&state, &session_id);

    let song = state
        .playlist
        .get(index)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "No songs found".to_string());

    PlayerControlsTemplate {
        current_song: song,
        current_index: index,
        total_songs: state.playlist.len(),
    }
}

/// Streams an audio file to the client, with full support for HTTP byte-range requests.
/// This handler is intentionally stateless: it does not modify playback state
/// or session position, it only serves file data.
async fn stream_audio(
    State(state): State<SharedState>,
    Path(index): Path<usize>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Get song info
    let song = state.playlist.get(index).ok_or(StatusCode::NOT_FOUND)?;
    let file_path = state.music_folder.join(&song.filename);
    let file_size = song.size;

    // Parse Range header if present
    if let Some(range_value) = headers.get(header::RANGE) {
        if let Ok(range_str) = range_value.to_str() {
            if let Some(range_str) = range_str.strip_prefix("bytes=") {
                // Range requests have the format "start-end", and both parts are optional.
                // You might see "bytes=0-999" (first 1000 bytes), "bytes=2000000-" (from byte 2 million to the end),
                // or even "bytes=-1000" (last 1000 bytes, though we don't handle this case)
                let parts: Vec<&str> = range_str.split('-').collect();

                if parts.len() == 2 {
                    let start: u64 = parts[0].parse().unwrap_or(0);
                    let end: u64 = if parts[1].is_empty() {
                        file_size - 1
                    } else {
                        parts[1].parse().unwrap_or(file_size - 1).min(file_size - 1)
                    };

                    let mut file = File::open(&file_path)
                        .await
                        .map_err(|_| StatusCode::NOT_FOUND)?;

                    file.seek(std::io::SeekFrom::Start(start))
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

                    let length = end - start + 1; // example end = 0, start = 0, that is counted as one byte (the first one)
                    let limited_file = file.take(length);
                    let stream = ReaderStream::with_capacity(limited_file, 128 * 1024); // we read 128 kilobytes at a time
                    let body = axum::body::Body::from_stream(stream);

                    return Ok(Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_TYPE, "audio/mpeg")
                        // We tell the browser that this server supports range requests on this resource,
                        // which enables the seeking functionality in the HTML5 audio player.
                        .header(header::ACCEPT_RANGES, "bytes") // required for partial content responses
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {}-{}/{}", start, end, file_size),
                        )
                        .header(header::CONTENT_LENGTH, length.to_string()) // specifies how many bytes are in this particular response body (not the whole file)
                        // CACHE_CONTROL header tells browsers and intermediate proxies that this content can be cached
                        // for up to 31,536,000 seconds (one year).
                        // Since MP3 files don't change, aggressive caching dramatically improves performance for repeated playback.
                        // (seeking to previously loaded part of the song)
                        .header(header::CACHE_CONTROL, "public, max-age=31536000")
                        .body(body)
                        .unwrap());
                }
            }
        }
    }

    // If there is no range specified just start from the beginning
    let file = File::open(&file_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let stream = ReaderStream::with_capacity(file, 128 * 1024);
    let body = axum::body::Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/mpeg")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::CACHE_CONTROL, "public, max-age=31536000")
        .body(body)
        .unwrap())
}

// Upgrade with WebRTC? to have p2p

/// WebSocket entrypoint for the radio synchronization system.
/// No state is modified here; it exists purely as a thin Axum integration
/// layer for the radio protocol.
async fn radio_websocket(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session_id_str = get_session_id(&headers);

    // This is the session id first given to the user
    // It is used so if someone tampers with their original session their calls are moot
    let validated_session_id = match SessionId::new(session_id_str) {
        Ok(id) => id,
        Err(e) => {
            // If the session ID is invalid, reject the WebSocket upgrade
            tracing::warn!(
                "Rejected WebSocket connection with invalid session ID: {}",
                e
            );
            return (StatusCode::BAD_REQUEST, "Invalid session ID").into_response();
        }
    };

    ws.on_upgrade(|socket| {
        handle_radio_connection(socket, state, validated_session_id.into_inner())
    })
}

/// Manages the full lifecycle of a radio WebSocket connection.
async fn handle_radio_connection(
    socket: WebSocket,
    state: SharedState,
    validated_session_id: String,
) {
    let (mut sender, mut receiver) = socket.split();

    // Private channel for responses from the receive task to the send task (same WebSocket)
    // For when you receive soemthing and want to return it to yourself and only yourself
    let (out_tx, mut out_rx) = mpsc::channel::<RadioMessage>(32);

    // Channel used to communicate tuned broadcaster changes
    let (tuned_tx, mut tuned_rx) = tokio::sync::watch::channel::<Option<String>>(None);

    let mut global_broadcast_rx = state.global_broadcast_tx.subscribe();

    let heartbeat_limiter = Arc::new(RateLimiter::for_heartbeat());
    let broadcast_limiter = Arc::new(RateLimiter::for_broadcast());

    state.active_connections.fetch_add(1, Relaxed);
    broadcast_analytics(&state);

    tracing::info!(
        "Client connected: {} (total: {})",
        &validated_session_id,
        state.active_connections.load(Relaxed)
    );

    // Send task
    let state_clone = state.clone();

    let mut send_task = tokio::spawn(async move {
        let mut current_tuned_broadcaster_rx: Option<broadcast::Receiver<RadioMessage>> = None;

        loop {
            // Polls all channels simultaneously
            tokio::select! {
                // Global channel
                Ok(msg) = global_broadcast_rx.recv() => {
                    let should_forward = should_forward_message(&msg).await;

                    if should_forward {
                        if let Err(e) = send_message(&mut sender, &msg).await {
                            tracing::error!("Failed to forward broadcast: {}", e);
                            break;
                        }
                    }
                }

                // Change the channel we are listening on in the case of change
                Ok(()) = tuned_rx.changed() => {
                    match tuned_rx.borrow().clone() {
                        Some(broadcast_id) => {
                            let tx = {
                                let channels = state_clone.broadcast_channels.read();
                                channels.get(&broadcast_id).cloned()
                            };

                            current_tuned_broadcaster_rx = tx.map(|t| t.subscribe());
                            tracing::debug!("Tuned in to {:?}", broadcast_id);
                        }
                        None => {
                            current_tuned_broadcaster_rx = None;
                            tracing::debug!("Tuned out");
                        }
                    }
                }

                // Messages from the receive task
                Some(msg) = out_rx.recv() => {
                    if let Err(e) = send_message(&mut sender, &msg).await {
                        tracing::error!("Failed to send message: {}", e);
                        break;
                    }
                }

                // Broadcast messages from other connections to the appropriate channel
                Ok(msg) = async {
                    match &mut current_tuned_broadcaster_rx {
                        Some(rx) => rx.recv().await,
                        // If current_tuned_broadcaster_rx is None, return a pending future
                        // so this branch never becomes ready and never wins select!
                        None => std::future::pending().await,
                    }
                } => {
                    // This fires when current_tuned_broadcaster_rx is some and receives something
                    if should_forward_message(&msg).await {
                        if let Err(e) = send_message(&mut sender, &msg).await {
                            tracing::error!("Failed to forward broadcast: {}", e);
                            break;
                        }
                    }
                }

                else => {
                    tracing::debug!("Send task channel closed");
                    break;
                }
            }
        }
    });

    // Receive task
    let state_clone = state.clone();
    let validated_session_id_clone = validated_session_id.clone();

    let mut receive_task = tokio::spawn(async move {
        while let Some(msg_result) = receiver.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    tracing::trace!("Received message: {}", text);

                    match serde_json::from_str::<RadioMessage>(&text) {
                        Ok(radio_msg) => {
                            if let Err(e) = handle_client_message(
                                radio_msg,
                                &state_clone,
                                &validated_session_id_clone,
                                &out_tx,
                                &tuned_tx,
                                &heartbeat_limiter,
                                &broadcast_limiter,
                            )
                            .await
                            {
                                tracing::error!("Failed to handle message: {}", e);
                                // Send error to client
                                let error_msg = create_error_message(&e);
                                let _ = out_tx.send(error_msg).await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse message: {}", e);
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    tracing::info!("Client closed connection");
                    break;
                }
                Err(e) => {
                    tracing::error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        tracing::debug!("Receive task ended");
    });

    // Wait for either task to complete, then abort the other too
    tokio::select! {
        _ = &mut send_task => {
            tracing::debug!("Send task completed, aborting receive task");
            receive_task.abort();
        }
        _ = &mut receive_task => {
            tracing::debug!("Receive task completed, aborting send task");
            send_task.abort();
        }
    }

    delete_broadcasting_session(&state, &validated_session_id);

    if state.active_connections.load(Relaxed) > 0 {
        state.active_connections.fetch_sub(1, Relaxed);
        broadcast_analytics(&state);
    }

    tracing::info!(
        "Client disconnected: {} (total: {})",
        &validated_session_id,
        state.active_connections.load(Relaxed)
    );
}

/// Serializes a `RadioMessage` and sends it to the client over the WebSocket.
async fn send_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &RadioMessage,
) -> PlayerResult<()> {
    let json = serde_json::to_string(msg)?;
    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| PlayerError::WebSocketError(e.to_string()))
}

/// Validates that the message is a broadcastable type.
/// All messages that arrive here have already been filtered by subscription.
async fn should_forward_message(msg: &RadioMessage) -> bool {
    matches!(
        msg,
        RadioMessage::BroadcasterOnline { .. }
            | RadioMessage::BroadcasterOffline { .. }
            | RadioMessage::Sync { .. }
            | RadioMessage::Heartbeat { .. }
            | RadioMessage::Analytics { .. }
            | RadioMessage::BroadcastStateResponse { .. }
    )
}

/// Handles an incoming `RadioMessage` from a client WebSocket.
async fn handle_client_message(
    msg: RadioMessage,
    state: &SharedState,
    validated_session_id: &str,
    // communication between receive and send tasks
    out_tx: &mpsc::Sender<RadioMessage>,
    // communication on wether the client changed who they are listening to
    tuned_tx: &tokio::sync::watch::Sender<Option<String>>,
    heartbeat_limiter: &Arc<RateLimiter>,
    broadcast_limiter: &Arc<RateLimiter>,
) -> PlayerResult<()> {
    match msg {
        RadioMessage::TuneIn { broadcaster_id } => {
            // Validate the incoming broadcaster ID as a proper SessionId (UUID)
            let session_id = SessionId::new(broadcaster_id.clone())?;

            tracing::info!("Client tuning into: {}", session_id);

            // Retrieve current broadcast state if it exists
            let maybe_state = {
                let broadcasts = state.broadcast_states.read();
                broadcasts.get(&broadcaster_id).cloned()
            };

            if let Some(b_state) = maybe_state {
                // Send a Sync message with the current state to the newly tuned client
                tracing::debug!(
                    "Sending initial sync: song={}, time={:.2}",
                    b_state.song_index,
                    b_state.playback_time
                );

                let sync_msg = RadioMessage::Sync {
                    broadcaster_id: broadcaster_id.clone(),
                    song_index: b_state.song_index,
                    playback_time: b_state.playback_time,
                    is_playing: b_state.is_playing,
                    server_timestamp_ms: now_ms(),
                };

                tuned_tx.send(Some(broadcaster_id.clone())).map_err(|_| {
                    PlayerError::WebSocketError("Failed to send tune change".into())
                })?;

                out_tx
                    .send(sync_msg)
                    .await
                    .map_err(|_| PlayerError::WebSocketError("Failed to send sync".into()))?;

                state.active_listeners.fetch_add(1, Relaxed);
                broadcast_analytics(state);
            } else {
                // No broadcaster found with this ID
                return Err(PlayerError::BroadcasterNotFound(broadcaster_id));
            }
        }

        RadioMessage::TuneOut => {
            // Client wants to stop listening to any broadcaster
            tracing::info!("Client tuned out");
            tuned_tx
                .send(None)
                .map_err(|_| PlayerError::WebSocketError("Failed to tune out".into()))?;

            if state.active_listeners.load(Relaxed) > 0 {
                state.active_listeners.fetch_sub(1, Relaxed);
                broadcast_analytics(state);
            }
        }

        RadioMessage::BroadcastUpdate {
            broadcaster_id,
            song_index,
            playback_time,
            is_playing,
        } => {
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Enforce broadcast update rate limits (prevents spam)
            broadcast_limiter.check_and_consume(&broadcaster_id).await?;

            // Ensure the requested song index exists in the playlist
            validate_song_index(song_index, state.playlist.len())?;

            tracing::debug!(
                "Broadcast update from {}: song={}, time={:.2}, playing={}",
                &broadcaster_id,
                song_index,
                playback_time,
                is_playing
            );

            // If the session isn't broadcasting don't update
            if !state.broadcast_states.read().contains_key(&broadcaster_id) {
                return Err(PlayerError::BroadcasterNotFound(broadcaster_id));
            }

            let server_ts = now_ms();

            let song_name = state
                .playlist
                .get(song_index)
                .map(|song| song.filename.clone())
                .unwrap_or_else(|| format!("Unknown song #{}", song_index));

            // Update the server-side broadcast state
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                song_name,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
            };

            state
                .broadcast_states
                .write()
                .insert(broadcaster_id.clone(), new_state);

            // Forward the update to all subscribed clients via Sync message
            let sync_msg = RadioMessage::Sync {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
            };

            // A return value of Err does not mean that future calls to send will fail
            if let Some(tx) = state.broadcast_channels.read().get(&broadcaster_id) {
                match tx.send(sync_msg) {
                    Ok(count) => {
                        tracing::trace!("Sync sent to {} listeners", count);
                    }
                    Err(_) => {
                        tracing::trace!("Sync sent but no active listeners");
                    }
                }
            }

            broadcast_analytics(state);
        }

        RadioMessage::Heartbeat {
            broadcaster_id,
            playback_time,
        } => {
            // Validate broadcaster session ID
            let session_id = SessionId::new(broadcaster_id.clone())?;

            // Enforce heartbeat rate limits
            heartbeat_limiter
                .check_and_consume(session_id.as_str())
                .await?;

            let server_ts = now_ms();

            // Update playback time for the broadcaster
            let mut broadcasts = state.broadcast_states.write();
            if let Some(broadcast) = broadcasts.get_mut(&broadcaster_id) {
                broadcast.playback_time = playback_time;
                broadcast.server_timestamp_ms = server_ts;

                tracing::trace!("Heartbeat from {}: time={:.2}", session_id, playback_time);
            }
        }

        RadioMessage::StopBroadcasting { broadcaster_id } => {
            // Validate broadcaster session ID
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            let mut broadcasts = state.broadcast_states.write();
            broadcasts.remove(&broadcaster_id);
            drop(broadcasts);

            let mut broadcast_channel = state.broadcast_channels.write();
            broadcast_channel.remove(&broadcaster_id);
            drop(broadcast_channel);

            let offline_msg = RadioMessage::BroadcasterOffline {
                broadcaster_id: broadcaster_id.clone(),
            };

            // A return value of Err does not mean that future calls to send will fail
            match state.global_broadcast_tx.send(offline_msg) {
                Ok(count) => {
                    tracing::debug!("BroadcasterOffline sent to {} clients", count);
                }
                Err(_) => {
                    tracing::debug!("BroadcasterOffline sent but no clients connected");
                }
            }

            if state.active_broadcasters.load(Relaxed) > 0 {
                state.active_broadcasters.fetch_sub(1, Relaxed);
            }

            broadcast_analytics(state);
        }

        RadioMessage::StartBroadcasting {
            broadcaster_id,
            song_index,
            playback_time,
            is_playing,
        } => {
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Enforce broadcast update rate limits (prevents spam)
            broadcast_limiter.check_and_consume(&broadcaster_id).await?;

            // Ensure the requested song index exists in the playlist
            validate_song_index(song_index, state.playlist.len())?;

            let was_already_broadcasting = {
                let broadcasts = state.broadcast_states.read();

                broadcasts.contains_key(&broadcaster_id)
            };

            if was_already_broadcasting {
                tracing::debug!(
                    "Broadcaster {} already registered - updating state only (NOT incrementing counter)",
                    broadcaster_id
                );
            } else {
                tracing::debug!(
                    "New broadcaster {} starting (incrementing counter)",
                    broadcaster_id
                );
                state.active_broadcasters.fetch_add(1, Relaxed);
            }

            tracing::debug!(
                "Starting broadcast from {}: song={}, time={:.2}, playing={}",
                &broadcaster_id,
                song_index,
                playback_time,
                is_playing
            );

            let server_ts = now_ms();

            let song_name = state
                .playlist
                .get(song_index)
                .map(|song| song.filename.clone())
                .unwrap_or_else(|| format!("Unknown song #{}", song_index));

            // Update the server-side broadcast state
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                song_name,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
            };

            state
                .broadcast_states
                .write()
                .insert(broadcaster_id.clone(), new_state);

            // Create the channel if it doesn't exist
            {
                let mut channels = state.broadcast_channels.write();
                channels.entry(broadcaster_id.clone()).or_insert_with(|| {
                    let (tx, _rx) = broadcast::channel::<RadioMessage>(100);
                    tracing::debug!("Created broadcast channel for session: {}", broadcaster_id);
                    tx
                });
            }

            // Send announcement through global channel, not the broadcaster's channel
            if !was_already_broadcasting {
                let broadcasting_msg = RadioMessage::BroadcasterOnline {
                    broadcaster_id: broadcaster_id.clone(),
                };

                state
                    .global_broadcast_tx
                    .send(broadcasting_msg)
                    .map_err(|_| PlayerError::BroadcastSendError)?;

                tracing::info!("Broadcaster {} came online", broadcaster_id);
            }

            broadcast_analytics(state);
        }

        RadioMessage::QueryBroadcastState { session_id } => {
            ensure_same_session(&session_id, validated_session_id)?;
            
            // Check if this session is currently broadcasting
            let (is_broadcasting, current_state) = {
                let broadcasts = state.broadcast_states.read();
                let is_broadcasting = broadcasts.contains_key(&session_id);
                let current_state = broadcasts.get(&session_id).cloned();
                (is_broadcasting, current_state)
            };
            
            tracing::debug!(
                "Query broadcast state for {}: {}",
                session_id,
                is_broadcasting
            );
            
            let response = RadioMessage::BroadcastStateResponse {
                session_id: session_id.clone(),
                is_broadcasting,
                current_state,
            };
            
            out_tx
                .send(response)
                .await
                .map_err(|_| PlayerError::WebSocketError("Failed to send state response".into()))?;
        }

        _ => {
            // Any unexpected messages are logged but ignored
            tracing::warn!("Received unexpected message type");
        }
    }

    Ok(())
}

/// Verifies that the provided identifier matches the session identifier
/// recorded when the WebSocket connection was first established.
/// This is used to ensure that a client cannot act or broadcast as another
/// session by supplying a different ID after the connection is open.
fn ensure_same_session(expected: &str, actual: &str) -> Result<(), PlayerError> {
    (expected == actual).then_some(()).ok_or_else(|| {
        tracing::warn!(
            "Session {} attempted to act as {}",
            actual,
            expected
        );
        PlayerError::BroadcastUnauthorized(
            "Trying to use session id that differs from the one established upon websocket handshake.".into())
    })
}

fn delete_broadcasting_session(state: &SharedState, broadcaster_id: &str) {
    let broadcaster_id_to_cleanup = {
        let broadcasts = state.broadcast_states.read();
        if broadcasts.contains_key(broadcaster_id) {
            Some(broadcaster_id)
        } else {
            None
        }
    };

    if let Some(broadcaster_id) = broadcaster_id_to_cleanup {
        tracing::info!(
            "Auto-cleanup: Disconnected broadcaster {} - removing state",
            broadcaster_id
        );

        {
            let mut broadcasts = state.broadcast_states.write();
            broadcasts.remove(broadcaster_id);
        }

        if state.active_broadcasters.load(Relaxed) > 0 {
            state.active_broadcasters.fetch_sub(1, Relaxed);
            tracing::debug!("Decremented active_broadcasters for {}", broadcaster_id);
        }

        let broadcaster_msg_id = broadcaster_id;

        let offline_msg = RadioMessage::BroadcasterOffline {
            broadcaster_id: broadcaster_msg_id.to_string(),
        };

        match state.global_broadcast_tx.send(offline_msg) {
            Ok(listener_count) => {
                tracing::info!(
                    "Notified {} listener(s) that {} went offline",
                    listener_count,
                    broadcaster_id
                );
            }
            Err(_) => {
                tracing::debug!("No listeners to notify for {}", broadcaster_id);
            }
        }

        {
            let mut channels = state.broadcast_channels.write();
            if channels.remove(broadcaster_id).is_some() {
                tracing::debug!("Removed broadcast channel for {}", broadcaster_id);
            }
        }

        broadcast_analytics(&state);
    }
}

/// Creates a RadioMessage::Error from a PlayerError
/// This is used to send error messages back to the client over WebSocket.
fn create_error_message(error: &PlayerError) -> RadioMessage {
    RadioMessage::Error {
        message: error.to_string(),
    }
}

// TODO this current analytics of sending broadcast state is too expensive

/// Broadcasts current analytics to all connected clients via the global channel.
/// Call this whenever any counter changes to keep all clients synchronized.
fn broadcast_analytics(state: &SharedState) {
    // Collect all broadcaster states
    let broadcasters: Vec<BroadcastState> = {
        let broadcasts = state.broadcast_states.read();
        broadcasts.values().cloned().collect()
    };

    let analytics_msg = RadioMessage::Analytics {
        active_connections: state.active_connections.load(Relaxed),
        active_broadcasters: state.broadcast_channels.read().len(),
        active_listeners: state.active_listeners.load(Relaxed),
        broadcasters,
    };

    match state.global_broadcast_tx.send(analytics_msg) {
        Ok(count) => {
            tracing::trace!("Analytics update sent to {} clients", count);
        }
        Err(_) => {
            tracing::trace!("Analytics update sent but no clients connected");
        }
    }
}

/// Periodically cleans up stale player sessions and broadcaster channels.
async fn cleanup_stale_sessions(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        // Wait for the next tick (initially fires immediately, then every 60s)
        interval.tick().await;

        let now = std::time::Instant::now();
        let now_ms = now_ms();

        // Cleanup Player Sessions
        // These are private playback sessions for users not in radio mode
        {
            let mut sessions = state.sessions.write();
            let before_count = sessions.len();

            // Remove sessions older than 1 hour
            sessions.retain(|session_id, session| {
                let age = now.duration_since(session.last_activity);
                let should_keep = age.as_secs() < 3600;

                if !should_keep {
                    tracing::info!(
                        "Cleaning up stale player session: {} (age: {}s)",
                        session_id,
                        age.as_secs()
                    );
                }

                should_keep
            });

            let removed = before_count - sessions.len();
            if removed > 0 {
                tracing::info!("Cleaned up {} stale player session(s)", removed);
            }
        }

        // Cleanup Broadcaster Sessions
        // These are active radio broadcasts that should be recent
        let mut stale_broadcasters = Vec::new();

        {
            let broadcasts = state.broadcast_states.read();

            // Find broadcasters that haven't updated in 30+ seconds
            for (broadcaster_id, broadcast_state) in broadcasts.iter() {
                let age_ms = now_ms.saturating_sub(broadcast_state.server_timestamp_ms);
                let age_secs = age_ms / 1000;

                // 30 seconds is chosen because:
                // - Heartbeats come every 2-3 seconds normally
                // - 30 seconds allows for network hiccups and reconnects
                // - But catches truly disconnected broadcasters quickly
                if age_secs > 30 {
                    tracing::info!(
                        "Found stale broadcaster: {} (age: {}s)",
                        broadcaster_id,
                        age_secs
                    );
                    stale_broadcasters.push(broadcaster_id.clone());
                }
            }
        }

        // Now remove the stale broadcasters and notify listeners
        for broadcaster_id in stale_broadcasters {
            {
                let mut broadcasts = state.broadcast_states.write();
                if broadcasts.remove(&broadcaster_id).is_some() {
                    tracing::info!("Removed stale broadcaster state: {}", broadcaster_id);
                }
            }

            // Decrement the broadcaster count for analytics
            state.active_broadcasters.fetch_sub(1, Relaxed);

            // Send offline notification to any remaining listeners
            // It's okay if there are no listeners - we handle that gracefully
            if let Some(tx) = state.broadcast_channels.read().get(&broadcaster_id) {
                let offline_msg = RadioMessage::BroadcasterOffline {
                    broadcaster_id: broadcaster_id.clone(),
                };

                match tx.send(offline_msg) {
                    Ok(listener_count) => {
                        tracing::info!(
                            "Notified {} listener(s) that broadcaster {} went offline",
                            listener_count,
                            broadcaster_id
                        );
                    }
                    Err(_) => {
                        tracing::debug!(
                            "No listeners to notify for offline broadcaster: {}",
                            broadcaster_id
                        );
                    }
                }
            }

            // Remove the broadcast channel itself
            {
                let mut channels = state.broadcast_channels.write();
                if channels.remove(&broadcaster_id).is_some() {
                    tracing::info!("Removed broadcast channel: {}", broadcaster_id);
                }
            }
        }

        broadcast_analytics(&state);
    }
}

/// Initializes the shared player state by scanning the music folder,
/// building a playlist, and setting up broadcast channels and session tracking.
async fn init_player_state(music_folder: PathBuf) -> SharedState {
    let mut playlist = Vec::new();

    // Read all files in the music folder asynchronously
    if let Ok(mut entries) = tokio::fs::read_dir(&music_folder).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(filename) = entry.file_name().to_str() {
                // Only include MP3 files
                if filename.ends_with(".mp3") {
                    if let Ok(metadata) = entry.metadata().await {
                        playlist.push(SongInfo {
                            filename: filename.to_string(),
                            size: metadata.len(),
                        });
                    }
                }
            }
        }
    }

    // Sort the playlist alphabetically by filename for consistent ordering
    playlist.sort_by(|a, b| a.filename.cmp(&b.filename));

    // Create a broadcast channel for WebSocket messages (sync updates)
    // Capacity 100 means it can buffer up to 100 messages before dropping
    let (global_broadcast_tx, _) = broadcast::channel(100);

    // Return the shared application state wrapped in Arc for multi-threaded use
    Arc::new(AppState {
        playlist: Arc::new(playlist),
        music_folder: Arc::new(music_folder),
        sessions: RwLock::new(HashMap::new()), // tracks client sessions
        broadcast_states: RwLock::new(HashMap::new()), // tracks broadcaster states
        broadcast_channels: RwLock::new(HashMap::new()), // tracks broadcaster channels
        global_broadcast_tx,                   // used to send Sync messages to listeners
        active_connections: AtomicUsize::new(0),
        active_broadcasters: AtomicUsize::new(0),
        active_listeners: AtomicUsize::new(0),
    })
}

/// Creates an Axum router with all the player routes, using the given music folder.
/// Returns a future because state initialization is async.
pub fn create_player_router(state: Arc<AppState>) -> impl std::future::Future<Output = Router> {
    async move {
        // Build the router with all routes
        Router::new()
            .route("/", get(player_page)) // Root page
            .route("/player", get(player_page)) // Player main page
            .route("/player/next", post(next_song)) // Next song action
            .route("/player/prev", post(prev_song)) // Previous song action
            .route("/player/stream/{index}", get(stream_audio)) // Audio streaming route
            .route("/player/radio", get(radio_websocket)) // Radio WebSocket
            .route("/player/controls", get(player_controls)) // Return current controls/status
            .with_state(state) // Attach shared state to all routes
    }
}

/// Starts the MP3 player server, binds to a TCP port, and runs Axum
pub async fn initialize(path_buf: PathBuf) {
    // Set up tracing/logging
    init_logging();

    tracing::info!("Starting MP3 Player server");

    // Bind TCP listener to localhost:8083
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8083")
        .await
        .expect("Failed to bind to port 8083");

    let addr = listener.local_addr().unwrap();

    tracing::info!(
        "✓ MP3 Player listening on http://{} -> https://evolved-gladly-possum.ngrok-free.app/player",
        addr
    );

    // Initialize the shared state asynchronously
    let state = init_player_state(path_buf).await;

    // Create the router with async initialization
    let router = create_player_router(state.clone()).await;

    // Task for cleaning up old sessions both player and broadcast
    tokio::spawn(cleanup_stale_sessions(state.clone()));

    // TODO: Add graceful shutdown handling:
    // Set up signal handler for Ctrl+C and shutdown_rx channel
    // Use: axum::serve(listener, router).with_graceful_shutdown(async { shutdown_rx.await.ok(); })

    // Start serving requests using the Axum router
    axum::serve(listener, router).await.expect("Server failed");
}
