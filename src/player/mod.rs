mod handlers;
mod radio;
mod session;
mod templates;
mod types;
mod logging;
pub mod error;
pub mod validation;
pub mod rate_limit;
pub mod reconnect;

pub use types::{AppState, BroadcastState, RadioMessage, SharedState, SongInfo};

use self::logging::init_logging;
use axum::Router;
use axum::routing::{get, post};
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tokio::sync::broadcast;
use uuid::Uuid;

use handlers::{
    get_playlist, next_song, player_controls, player_page, prev_song, stream_audio_by_id,
    stream_audio_by_index,
};
use radio::{cleanup_stale_sessions, radio_websocket};

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
                            id: Uuid::new_v4().to_string(),
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
        sessions: DashMap::new(), // tracks client sessions
        broadcast_states: DashMap::new(), // tracks broadcaster states
        broadcast_channels: DashMap::new(), // tracks broadcaster channels
        broadcaster_listeners: DashMap::new(),
        global_broadcast_tx, // used to send Sync messages to listeners
        active_connections: AtomicUsize::new(0),
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
            .route("/player/stream/{index}", get(stream_audio_by_index)) // Audio streaming route
            .route("/player/stream/id/{song_id}", get(stream_audio_by_id))
            .route("/player/radio", get(radio_websocket)) // Radio WebSocket
            .route("/player/controls", get(player_controls)) // Return current controls/status
            .route("/player/playlist", get(get_playlist))
            .with_state(state) // Attach shared state to all routes
    }
}

/// Starts the MP3 player server, binds to a TCP port, and runs Axum
pub async fn initialize(path_buf: PathBuf) {
    // Set up tracing/logging
    init_logging();

    tracing::info!("Starting MP3 Player server");

    // Bind TCP listener to localhost:8083
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8083")
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

async fn _add_ngrok_header(
    mut req: hyper::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.headers_mut()
        .insert("ngrok-skip-browser-warning", "true".parse().unwrap());

    let mut res = next.run(req).await;
    res.headers_mut()
        .insert("ngrok-skip-browser-warning", "true".parse().unwrap());
    res
}