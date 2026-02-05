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
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, mpsc};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Clone)]
struct SongInfo {
    filename: String,
    size: u64,
    _duration_secs: Option<u32>,
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
    playback_time: f64, // Current position in seconds
    is_playing: bool,
    server_timestamp_ms: u128, // When this state was recorded
}

struct AppState {
    playlist: Arc<Vec<SongInfo>>,
    music_folder: Arc<PathBuf>,

    /// Private mode sessions
    sessions: RwLock<HashMap<String, PlayerSession>>,

    /// Radio mode state
    /// Maps broadcaster_id -> their current state
    broadcast_states: RwLock<HashMap<String, BroadcastState>>,

    /// Broadcast channel for real-time updates
    /// When a broadcaster updates their state, we send it here
    /// and all their listeners receive it
    ///
    /// TODO!
    /// Should evntually get moved int obroadcast states,
    /// so each broadcaster has their own channel.
    broadcast_tx: broadcast::Sender<RadioMessage>,
}

/// Helper type for cleaner function signatures
type SharedState = Arc<AppState>;

/// Messages sent over WebSocket
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum RadioMessage {
    /// Leader returns this when play/pause/seek/next/prev happens
    Sync {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
        server_timestamp_ms: u128,
    },

    /// Leader sends this every 2-3 seconds
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

    /// Leader sends this on play/pause/seek/next/prev, and server sends Sync to all
    BroadcastUpdate {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
    },

    Error {
        message: String,
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
    let mut sessions = state.sessions.write();
    let now = std::time::Instant::now();

    // TODO: This should be made less aggressive nad put into a background task
    sessions.retain(|_, session| now.duration_since(session.last_activity).as_secs() < 3600);

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

        // Push sync to all listeners
        let _ = state.broadcast_tx.send(RadioMessage::Sync {
            broadcaster_id: session_id.to_string(),
            song_index: new_index,
            playback_time: 0.0,
            is_playing: broadcast.is_playing,
            server_timestamp_ms: server_ts,
        });
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
///
/// This handler:
/// - Extracts (or generates) a session ID from cookies
/// - Retrieves or initializes the session's current playlist position
/// - Resolves the currently selected song
/// - Returns an HTML page rendered via `PlayerTemplate`
///
/// The session ID is embedded in the response cookie so subsequent
/// requests (next/prev/stream) remain tied to the same private state.
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
///
/// This handler:
/// - Resolves the user's session via cookies
/// - Computes the next playlist index (clamped to the playlist bounds)
/// - Updates the private session state
/// - If the session is broadcasting, updates the authoritative broadcast state
/// - Returns a partial HTML fragment (`PlayerControlsTemplate`) for UI refresh
///
/// This endpoint is typically triggered by the "Next" button in the player UI.
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
///
/// This handler mirrors `next_song` but moves backward instead:
/// - The index is decremented using saturating arithmetic
/// - Private and broadcast states are updated identically
/// - A refreshed `PlayerControlsTemplate` is returned
///
/// This endpoint is typically triggered by the "Previous" button in the player UI.
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
///
/// - `broadcaster`:
///   - `Some(session_id)` → radio mode (follow another user's playback)
///   - `None` → private mode (local session playback)
#[derive(Deserialize)]
struct ControlsQuery {
    broadcaster: Option<String>,
}

/// Returns the current player control state as an HTML fragment.
///
/// This handler is designed to be HTMX-friendly:
/// it returns only the inner player controls markup, not a full page.
///
/// Behavior depends on mode:
/// - **Radio mode**: If a `broadcaster` query param is present and valid,
///   the UI reflects the broadcaster’s current song and index.
/// - **Private mode**: Falls back to the caller’s own session state.
///
/// This endpoint is triggered when:
/// - A listener syncs to a broadcaster
/// - The broadcaster changes tracks
/// - The client reconnects and needs to rehydrate UI state
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

            // Return controls reflecting the broadcaster’s state
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
///
/// This endpoint is used by the HTML5 `<audio>` element and must support
/// `Range` requests to allow seeking, buffering, and partial playback.
///
/// Behavior:
/// - If a valid `Range` header is present, responds with `206 Partial Content`
///   and streams only the requested byte slice.
/// - If no range is provided, streams the entire file with `200 OK`.
///
/// Notes:
/// - The `index` path parameter selects a song from the in-memory playlist.
/// - `Accept-Ranges: bytes` is always advertised so browsers know seeking is supported.
/// - Long-lived cache headers are safe because audio files are immutable.
///
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
///
/// This handler upgrades the incoming HTTP request to a WebSocket and
/// immediately delegates all connection logic to `handle_radio_connection`.
/// No state is modified here; it exists purely as a thin Axum integration
/// layer for the radio protocol.
async fn radio_websocket(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_radio_connection(socket, state))
}

/// Manages the full lifecycle of a radio WebSocket connection.
///
/// This function is responsible for:
/// - Splitting the socket into send and receive halves
/// - Spawning independent send and receive tasks
/// - Forwarding relevant broadcast messages to the client
/// - Handling incoming client commands (tuning, heartbeat, broadcast)
/// - Applying rate limiting and basic validation
///
/// Architecture:
/// - **Receive task**: Parses client messages and mutates shared state
/// - **Send task**: Pushes outbound messages to the client, sourced from:
///   - The global broadcast channel
///   - A private channel used for responses to this connection,
///     which is used for when a listener tunes in and just needs a Sync back
///
/// The connection terminates when either task exits, at which point the
/// remaining task is aborted and any per-connection state is cleaned up.
async fn handle_radio_connection(socket: WebSocket, state: SharedState) {
    let (mut sender, mut receiver) = socket.split();

    // Subscription to the global broadcast channel.
    // Receives real-time updates from all broadcasters, which the send task
    // filters based on which broadcaster this client is tuned to.
    let mut broadcast_rx = state.broadcast_tx.subscribe();

    // Private channel for responses from the receive task to the send task (same WebSocket)
    let (out_tx, mut out_rx) = mpsc::channel::<RadioMessage>(32);

    // Store broadcaster ID as String (we validate it when needed)
    let tuned_broadcaster = Arc::new(Mutex::new(None::<String>));

    let heartbeat_limiter = Arc::new(RateLimiter::for_heartbeat());
    let broadcast_limiter = Arc::new(RateLimiter::for_broadcast());

    // Send task
    let tuned_for_send = tuned_broadcaster.clone();
    let mut send_task = tokio::spawn(async move {
        loop {
            // Polls both channels simultaneously
            tokio::select! {
                // Messages from the receive task
                Some(msg) = out_rx.recv() => {
                    if let Err(e) = send_message(&mut sender, &msg).await {
                        tracing::error!("Failed to send message: {}", e);
                        break;
                    }
                }

                // Broadcast messages from other connections
                Ok(msg) = broadcast_rx.recv() => {
                    let should_forward = should_forward_message(&msg, &tuned_for_send).await;

                    if should_forward {
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
    let tuned_for_recv = tuned_broadcaster.clone();

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
                                &tuned_for_recv,
                                &out_tx,
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
                Ok(Message::Ping(_)) => {
                    // Respond to ping with pong (handled automatically by axum)
                    tracing::trace!("Received ping (auto-handled by axum)");
                }
                Ok(Message::Pong(_)) => {
                    tracing::trace!("Received pong");
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

    // Cleanup when connection closes
    if let Some(broadcaster_id) = tuned_broadcaster.lock().await.as_ref() {
        tracing::info!("Cleaning up connection tuned to: {}", broadcaster_id);
    }
}

/// Serializes a `RadioMessage` and sends it to the client over the WebSocket.
///
/// This helper centralizes outbound WebSocket message handling:
/// - Converts the strongly-typed `RadioMessage` into JSON
/// - Sends it as a text frame on the socket
/// - Normalizes serialization and send errors into `PlayerError`
///
/// Keeping this logic in one place ensures consistent error handling
/// and message formatting across all send paths.
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

/// Determines whether a given `RadioMessage` should be forwarded to the client
/// based on which broadcaster the client is currently tuned to.
///
/// Only `Sync` and `Heartbeat` messages carry a `broadcaster_id`. This ID is
/// validated as a `SessionId`. If valid, it is compared to the client's
/// currently tuned broadcaster. Returns `true` if the IDs match, meaning
/// the message should be forwarded to this client. Returns `false` otherwise.
async fn should_forward_message(
    msg: &RadioMessage,
    tuned_broadcaster: &Arc<Mutex<Option<String>>>,
) -> bool {
    // Extract the broadcaster ID from messages that contain it
    let broadcaster_id = match msg {
        RadioMessage::Sync { broadcaster_id, .. } => Some(broadcaster_id.as_str()),
        RadioMessage::Heartbeat { broadcaster_id, .. } => Some(broadcaster_id.as_str()),
        _ => None, // Other message types do not have a broadcaster ID or aren't meant to be sent, only received
    };

    if let Some(session_id_str) = broadcaster_id {
        // Attempt to validate and parse the broadcaster ID as a SessionId
        match SessionId::new(session_id_str.to_string()) {
            Ok(valid_id) => {
                // Lock the tuned_broadcaster to safely read the current client tuning
                let guard = tuned_broadcaster.lock().await;

                // Compare the tuned broadcaster with the message's broadcaster ID
                // If they match, this message should be forwarded
                return guard.as_ref().map(|s| s.as_str()) == Some(valid_id.as_str());
            }
            Err(e) => {
                // If the broadcaster ID is invalid, log the error and do not forward
                tracing::error!("Failed to parse broadcaster ID: {}", e);
            }
        }
    }

    // No broadcaster ID in message, or parsing failed, so do not forward
    false
}

/// Handles an incoming `RadioMessage` from a client WebSocket.
/// 
/// This function performs the following tasks based on message type:
/// - `TuneIn`: Validates the broadcaster session ID, updates the client's tuned broadcaster,
///   and sends the current broadcaster state as a `Sync` message.
/// - `TuneOut`: Clears the client's tuned broadcaster.
/// - `BroadcastUpdate`: Validates session ID, enforces rate limits, validates song index,
///   updates the server's broadcast state, and forwards a `Sync` message to all clients.
/// - `Heartbeat`: Validates session ID, enforces rate limits, updates broadcaster's playback time.
/// - Other messages are logged as unexpected.
///
/// Rate limiting is enforced using `RateLimiter`:
/// - `heartbeat_limiter`: Typically allows 1 heartbeat every 2 seconds.
/// - `broadcast_limiter`: Allows bursty updates but limits sustained rate.
///
/// # Errors
/// Returns a `PlayerError` if validation fails, rate limits are exceeded, or sending fails.
async fn handle_client_message(
    msg: RadioMessage,
    state: &SharedState,
    tuned_broadcaster: &Arc<Mutex<Option<String>>>,
    out_tx: &mpsc::Sender<RadioMessage>,
    heartbeat_limiter: &Arc<RateLimiter>,
    broadcast_limiter: &Arc<RateLimiter>,
) -> PlayerResult<()> {
    match msg {
        RadioMessage::TuneIn { broadcaster_id } => {
            // Validate the incoming broadcaster ID as a proper SessionId (UUID)
            let session_id = SessionId::new(broadcaster_id.clone())?;

            tracing::info!("Client tuning into: {}", session_id);

            // Store the tuned broadcaster for this client
            {
                let mut guard = tuned_broadcaster.lock().await;
                *guard = Some(session_id.as_str().to_string());
            }

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

                out_tx
                    .send(sync_msg)
                    .await
                    .map_err(|_| PlayerError::WebSocketError("Failed to send sync".into()))?;
            } else {
                // No broadcaster found with this ID
                return Err(PlayerError::BroadcasterNotFound(broadcaster_id));
            }
        }

        RadioMessage::TuneOut => {
            // Client wants to stop listening to any broadcaster
            tracing::info!("Client tuned out");
            let mut guard = tuned_broadcaster.lock().await;
            *guard = None;
        }

        RadioMessage::BroadcastUpdate {
            broadcaster_id,
            song_index,
            playback_time,
            is_playing,
        } => {
            // Validate broadcaster session ID
            let session_id = SessionId::new(broadcaster_id.clone())?;

            // Enforce broadcast update rate limits (prevents spam)
            broadcast_limiter
                .check_and_consume(session_id.as_str())
                .await?;

            // Ensure the requested song index exists in the playlist
            validate_song_index(song_index, state.playlist.len())?;

            tracing::debug!(
                "Broadcast update from {}: song={}, time={:.2}, playing={}",
                session_id,
                song_index,
                playback_time,
                is_playing
            );

            let server_ts = now_ms();

            // Update the server-side broadcast state
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
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

            state
                .broadcast_tx
                .send(sync_msg)
                .map_err(|_| PlayerError::BroadcastSendError)?;
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

        _ => {
            // Any unexpected messages are logged but ignored
            tracing::warn!("Received unexpected message type");
        }
    }

    Ok(())
}

/// Creates a RadioMessage::Error from a PlayerError
/// This is used to send error messages back to the client over WebSocket.
fn create_error_message(error: &PlayerError) -> RadioMessage {
    RadioMessage::Error {
        message: error.to_string(),
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
                            _duration_secs: None, // placeholder if you want to compute duration later
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
    let (broadcast_tx, _) = broadcast::channel(100);

    // Return the shared application state wrapped in Arc for multi-threaded use
    Arc::new(AppState {
        playlist: Arc::new(playlist),
        music_folder: Arc::new(music_folder),
        sessions: RwLock::new(HashMap::new()),           // tracks client sessions
        broadcast_states: RwLock::new(HashMap::new()),   // tracks broadcaster states
        broadcast_tx,                                    // used to send Sync messages to listeners
    })
}

/// Creates an Axum router with all the player routes, using the given music folder.
/// Returns a future because state initialization is async.
pub fn create_player_router(music_folder: PathBuf) -> impl std::future::Future<Output = Router> {
    async move {
        // Initialize the shared state asynchronously
        let state = init_player_state(music_folder).await;

        // Build the router with all routes
        Router::new()
            .route("/", get(player_page))                       // Root page
            .route("/player", get(player_page))                 // Player main page
            .route("/player/next", post(next_song))             // Next song action
            .route("/player/prev", post(prev_song))             // Previous song action
            .route("/player/stream/{index}", get(stream_audio)) // Audio streaming route
            .route("/player/radio", get(radio_websocket))       // Radio WebSocket
            .route("/player/controls", get(player_controls))    // Return current controls/status
            .with_state(state)                                  // Attach shared state to all routes
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

    // Create the router with async initialization
    let router = create_player_router(path_buf).await;

    // Start serving requests using the Axum router
    axum::serve(listener, router).await.expect("Server failed");
}

