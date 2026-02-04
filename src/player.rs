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
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Clone)]
struct SongInfo {
    filename: String,
    size: u64,
    _duration_secs: Option<u32>,
}

// Represents a single user's PRIVATE player state
struct PlayerSession {
    current_index: usize,
    last_activity: std::time::Instant,
}

// Represents a broadcaster's state (the authoritative source)
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

    // Private mode sessions
    sessions: RwLock<HashMap<String, PlayerSession>>,

    // Radio mode state
    // Maps broadcaster_id -> their current state
    broadcast_states: RwLock<HashMap<String, BroadcastState>>,

    // Broadcast channel for real-time updates
    // When a broadcaster updates their state, we send it here
    // and all their listeners receive it
    broadcast_tx: broadcast::Sender<RadioMessage>,
}

type SharedState = Arc<AppState>;

// Messages sent over WebSocket
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum RadioMessage {
    // Leader sends this when play/pause/seek/next/prev happens
    Sync {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
        server_timestamp_ms: u128,
    },

    // Leader sends this every 2-3 seconds to prevent drift
    Heartbeat {
        broadcaster_id: String,
        playback_time: f64,
    },

    // Client -> Server: "I want to tune into this broadcaster"
    TuneIn {
        broadcaster_id: String,
    },

    // Client -> Server: "I'm going back to private mode"
    TuneOut,

    // Client -> Server: "I'm broadcasting, here's my state"
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

fn get_or_create_position(state: &AppState, session_id: &str) -> usize {
    let mut sessions = state.sessions.write();
    let now = std::time::Instant::now();
    sessions.retain(|_, session| now.duration_since(session.last_activity).as_secs() < 3600);

    let session = sessions
        .entry(session_id.to_string())
        .or_insert(PlayerSession {
            current_index: 0,
            last_activity: now,
        });

    session.last_activity = now;
    session.current_index
}

fn update_session_index(state: &AppState, session_id: &str, new_index: usize) {
    let mut sessions = state.sessions.write();

    if let Some(session) = sessions.get_mut(session_id) {
        session.current_index = new_index;
        session.last_activity = std::time::Instant::now();
    }
}

fn update_broadcast_index(state: &AppState, session_id: &str, new_index: usize) {
    let server_ts = now_ms();

    let mut broadcasts = state.broadcast_states.write();

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

// Get current server time in milliseconds (for timestamping)
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

async fn player_page(State(state): State<SharedState>, headers: HeaderMap) -> PlayerTemplate {
    let session_id = get_session_id(&headers);
    let current_index = get_or_create_position(&state, &session_id);

    let current_song = state
        .playlist
        .get(current_index)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "No songs found".to_string());

    PlayerTemplate {
        current_song,
        current_index,
        total_songs: state.playlist.len(),
        session_id,
    }
}

async fn next_song(State(state): State<SharedState>, headers: HeaderMap) -> PlayerControlsTemplate {
    let session_id = get_session_id(&headers);
    let current_index = get_or_create_position(&state, &session_id);

    let new_index = (current_index + 1).min(state.playlist.len().saturating_sub(1));
    update_session_index(&state, &session_id, new_index);
    update_broadcast_index(&state, &session_id, new_index);

    let current_song = state
        .playlist
        .get(new_index)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "No songs found".to_string());

    PlayerControlsTemplate {
        current_song,
        current_index: new_index,
        total_songs: state.playlist.len(),
    }
}

async fn prev_song(State(state): State<SharedState>, headers: HeaderMap) -> PlayerControlsTemplate {
    let session_id = get_session_id(&headers);
    let current_index = get_or_create_position(&state, &session_id);

    let new_index = current_index.saturating_sub(1);
    update_session_index(&state, &session_id, new_index);
    update_broadcast_index(&state, &session_id, new_index);

    let current_song = state
        .playlist
        .get(new_index)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "No songs found".to_string());

    PlayerControlsTemplate {
        current_song,
        current_index: new_index,
        total_songs: state.playlist.len(),
    }
}

#[derive(Deserialize)]
struct ControlsQuery {
    broadcaster: Option<String>,
}

async fn player_controls(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<ControlsQuery>,
) -> PlayerControlsTemplate {
    // RADIO MODE
    if let Some(broadcaster_id) = query.broadcaster {
        if let Some(broadcast) = state.broadcast_states.read().get(&broadcaster_id) {
            let index = broadcast.song_index;

            let song = state
                .playlist
                .get(index)
                .map(|s| s.filename.clone())
                .unwrap_or_else(|| "No songs found".to_string());

            return PlayerControlsTemplate {
                current_song: song,
                current_index: index,
                total_songs: state.playlist.len(),
            };
        }
    }

    // PRIVATE MODE (fallback)
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

// WebSocket handler
async fn radio_websocket(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_radio_connection(socket, state))
}

use tokio::sync::{Mutex, mpsc};

use crate::error::{PlayerError, PlayerResult};
use crate::logging::init_logging;
use crate::rate_limit::RateLimiter;
use crate::validation::{SessionId, validate_song_index};

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
                            ).await {
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

async fn send_message(sender: &mut futures_util::stream::SplitSink<WebSocket, Message>, msg: &RadioMessage) -> PlayerResult<()> {
    let json = serde_json::to_string(msg)?;
    sender.send(Message::Text(json.into()))
        .await
        .map_err(|e| PlayerError::WebSocketError(e.to_string()))
}

async fn should_forward_message(msg: &RadioMessage, tuned_broadcaster: &Arc<Mutex<Option<String>>>) -> bool {
    let broadcaster_id = match msg {
        RadioMessage::Sync { broadcaster_id, .. } => Some(broadcaster_id.as_str()),
        RadioMessage::Heartbeat { broadcaster_id, .. } => Some(broadcaster_id.as_str()),
        _ => None,
    };
    
    if let Some(id) = broadcaster_id {
        let guard = tuned_broadcaster.lock().await;
        return guard.as_ref().map(|s| s.as_str()) == Some(id);
    }
    
    false
}

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
            // Validate the session ID
            let session_id = SessionId::new(broadcaster_id.clone())?;
            
            tracing::info!("Client tuning into: {}", session_id);
            
            // Update tuned broadcaster (store as String after validation)
            {
                let mut guard = tuned_broadcaster.lock().await;
                *guard = Some(broadcaster_id.clone());
            }
            
            // Send current broadcaster state if available
            let maybe_state = {
                let broadcasts = state.broadcast_states.read();
                broadcasts.get(&broadcaster_id).cloned()
            };
            
            if let Some(b_state) = maybe_state {
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
                
                out_tx.send(sync_msg).await
                    .map_err(|_| PlayerError::WebSocketError("Failed to send sync".into()))?;
            } else {
                return Err(PlayerError::BroadcasterNotFound(broadcaster_id));
            }
        }
        
        RadioMessage::TuneOut => {
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
            // Validate session ID
            let session_id = SessionId::new(broadcaster_id.clone())?;
            
            // Check rate limit
            broadcast_limiter.check_and_consume(session_id.as_str()).await?;
            
            // Validate song index
            validate_song_index(song_index, state.playlist.len())?;
            
            tracing::debug!(
                "Broadcast update from {}: song={}, time={:.2}, playing={}",
                session_id,
                song_index,
                playback_time,
                is_playing
            );
            
            let server_ts = now_ms();
            
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
            };
            
            state.broadcast_states.write()
                .insert(broadcaster_id.clone(), new_state);
            
            let sync_msg = RadioMessage::Sync {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
            };
            
            state.broadcast_tx.send(sync_msg)
                .map_err(|_| PlayerError::BroadcastSendError)?;
        }
        
        RadioMessage::Heartbeat {
            broadcaster_id,
            playback_time,
        } => {
            let session_id = SessionId::new(broadcaster_id.clone())?;
            
            // Check rate limit for heartbeats
            heartbeat_limiter.check_and_consume(session_id.as_str()).await?;
            
            let server_ts = now_ms();
            
            let mut broadcasts = state.broadcast_states.write();
            
            if let Some(broadcast) = broadcasts.get_mut(&broadcaster_id) {
                broadcast.playback_time = playback_time;
                broadcast.server_timestamp_ms = server_ts;
                
                tracing::trace!(
                    "Heartbeat from {}: time={:.2}",
                    session_id,
                    playback_time
                );
            }
        }
        
        _ => {
            tracing::warn!("Received unexpected message type");
        }
    }
    
    Ok(())
}

fn create_error_message(error: &PlayerError) -> RadioMessage {
    RadioMessage::Error {
        message: error.to_string(),
    }
}

async fn init_player_state(music_folder: PathBuf) -> SharedState {
    let mut playlist = Vec::new();

    if let Ok(mut entries) = tokio::fs::read_dir(&music_folder).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(filename) = entry.file_name().to_str() {
                if filename.ends_with(".mp3") {
                    if let Ok(metadata) = entry.metadata().await {
                        playlist.push(SongInfo {
                            filename: filename.to_string(),
                            size: metadata.len(),
                            _duration_secs: None,
                        });
                    }
                }
            }
        }
    }

    playlist.sort_by(|a, b| a.filename.cmp(&b.filename));

    // Create a broadcast channel with capacity for 100 messages
    let (broadcast_tx, _) = broadcast::channel(100);

    Arc::new(AppState {
        playlist: Arc::new(playlist),
        music_folder: Arc::new(music_folder),
        sessions: RwLock::new(HashMap::new()),
        broadcast_states: RwLock::new(HashMap::new()),
        broadcast_tx,
    })
}

pub fn create_player_router(music_folder: PathBuf) -> impl std::future::Future<Output = Router> {
    async move {
        let state = init_player_state(music_folder).await;

        Router::new()
            .route("/", get(player_page))
            .route("/player", get(player_page))
            .route("/player/next", post(next_song))
            .route("/player/prev", post(prev_song))
            .route("/player/stream/{index}", get(stream_audio))
            .route("/player/radio", get(radio_websocket))
            .route("/player/controls", get(player_controls))
            .with_state(state)
    }
}

pub async fn initialize(path_buf: PathBuf) {
    init_logging();
    
    tracing::info!("Starting MP3 Player server");
    
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8083")
        .await
        .expect("Failed to bind to port 8083");
    
    let addr = listener.local_addr().unwrap();
    
    tracing::info!(
        "✓ MP3 Player listening on http://{} -> https://evolved-gladly-possum.ngrok-free.app/player",
        addr
    );
    
    let router = create_player_router(path_buf).await;
    
    axum::serve(listener, router)
        .await
        .expect("Server failed");
}
