pub mod error;
mod handlers;
mod logging;
pub mod metrics;
pub mod radio;
pub mod rate_limit;
pub mod reconnect;
mod session;
mod templates;
mod types;
pub mod validation;

use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
pub use types::{AppState, BroadcastState, MediaType, RadioMessage, SharedState, MediaInfo};
pub use types::media_type_for;

use crate::player::handlers::{
    admin_state, check_session, get_subtitles, metrics_handler, radio_websocket, upload_file
};
use crate::player::types::PreparedMessage;

use self::logging::init_logging;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, header};
use axum::routing::{get, post};
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use tokio::sync::{RwLock, Semaphore, broadcast};
use uuid::Uuid;

use handlers::{
    download_file, download_folder, get_other_files, get_playlist, next_media, player_controls,
    player_page, prev_media, stream_audio_by_id, stream_audio_by_index,
};
use rate_limit::RateLimiter;
use session::cleanup_stale_sessions;

/// Initializes the shared player state by scanning the media folder,
/// building a playlist, and setting up broadcast channels and session tracking.
async fn init_player_state(media_folder: PathBuf) -> SharedState {
    let mut playlist = Vec::new();

    // Read all files in the media folder asynchronously
    if let Ok(mut entries) = tokio::fs::read_dir(&media_folder).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(filename) = entry.file_name().to_str() {
                // Accept all supported audio and video formats
                if let Some(media_type) = media_type_for(filename) {
                    if let Ok(metadata) = entry.metadata().await {
                        playlist.push(MediaInfo {
                            id: Uuid::new_v4().to_string(),
                            filename: filename.to_string(),
                            size: metadata.len(),
                            media_type,
                        });
                    }
                }
            }
        }
    }else {
        tracing::error!("Failed to read media folder: {}", media_folder.display());
    }       

    // Sort the playlist alphabetically by filename for consistent ordering
    playlist.sort_by(|a, b| a.filename.cmp(&b.filename));

    // Create a broadcast channel for WebSocket messages (sync updates)
    // Capacity 100 means it can buffer up to 100 messages before dropping
    let (global_broadcast_tx, _) = broadcast::channel(100);

    // Return the shared application state wrapped in Arc for multi-threaded use
    Arc::new(AppState {
        playlist: Arc::new(RwLock::new(playlist)),
        media_folder: Arc::new(media_folder),
        sessions: DashMap::new(),
        broadcast_states: DashMap::new(),
        broadcast_channels: DashMap::new(),
        broadcaster_listeners: DashMap::new(),
        session_tuned_to: DashMap::new(),
        global_broadcast_tx,
        active_connections: AtomicUsize::new(0),
        last_analytics_ms: AtomicU64::new(0),
        ws_rate_limiter: RateLimiter::for_websocket(),
        upload_quotas: DashMap::new(),
        conversion_semaphore: Arc::new(Semaphore::new(1)),
        asset_version: compute_asset_version(),
    })
}

/// Hashes the bytes of every static JS and CSS file into a short token.
///
/// It goes on every asset URL in the page as ?v=... . The cache key a browser
/// or proxy stores includes the query string, so the instant a file's contents
/// change the token changes, the URL changes, and there is no way for anything
/// in the middle to serve back the old bytes. Identical contents hash to the
/// same token across restarts, so a plain redeploy that didn't touch the assets
/// doesn't needlessly bust everyone's cache.
///
/// If the asset folder can't be read we fall back to a fresh token per boot,
/// which is worse for caching but still can never serve something stale.
fn compute_asset_version() -> String {
    use std::hash::{Hash, Hasher};

    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/player/static"));

    let mut files = Vec::new();
    for sub in ["js", "css"] {
        if let Ok(entries) = std::fs::read_dir(root.join(sub)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    files.push(path);
                }
            }
        }
    }

    if files.is_empty() {
        tracing::error!("Could not read static asset folder, using a per-boot asset version");
        return Uuid::new_v4().simple().to_string();
    }

    // Sort so the order the OS hands us the entries in doesn't change the hash
    files.sort();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in &files {
        match std::fs::read(path) {
            Ok(bytes) => {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .hash(&mut hasher);
                bytes.hash(&mut hasher);
            }
            Err(e) => tracing::warn!("Skipping {} while hashing assets: {}", path.display(), e),
        }
    }

    format!("{:016x}", hasher.finish())
}

/// Serves the CSS and JS off disk with a no-cache header.
///
/// These files change on every deploy but keep the same URLs, so without a
/// cache header a browser can sit on a stale copy for days off its own
/// heuristics, which is exactly how one phone ends up running last week's
/// script while another runs today's. no-cache still lets the browser keep the
/// file, it just has to check with a conditional request before using it, so
/// the normal case is a tiny 304 and a changed file lands immediately.
fn static_files() -> Router {
    Router::new()
        .nest_service(
            "/css",
            ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/player/static/css")),
        )
        .nest_service(
            "/js",
            ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/player/static/js")),
        )
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
}

/// Creates an Axum router with all the player routes, using the given media folder.
/// Returns a future because state initialization is async.
pub fn create_player_router(state: Arc<AppState>) -> impl std::future::Future<Output = Router> {
    async move {
        // Build the inner router with all your routes
        let inner = Router::new()
            .route("/", get(player_page)) // Root page
            .route("/player", get(player_page)) // Player main page
            .route("/player/next", post(next_media)) // Next media action
            .route("/player/prev", post(prev_media)) // Previous media action
            .route("/player/stream/{index}", get(stream_audio_by_index)) // Audio streaming route
            .route("/player/stream/id/{media_id}", get(stream_audio_by_id))
            .route("/player/radio", get(radio_websocket)) // Radio WebSocket
            .route("/player/controls", get(player_controls)) // Return current controls/status
            .route("/player/playlist", get(get_playlist))
            .route("/player/other-files", get(get_other_files))
            .route("/player/download/{filename}", get(download_file))
            .route("/player/download-folder/{foldername}", get(download_folder))
            .route("/player/session/check", get(check_session))
            .route("/player/subtitles/{media_id}", get(get_subtitles))
            // Override the default 2 MB body limit for the upload route only.
            // The outer DefaultBodyLimit still applies to every other route.
            .route(
                "/player/upload",
                post(upload_file).layer(DefaultBodyLimit::max(200 * 1024 * 1024)),
            )
            .route("/metrics", get(metrics_handler))
            .route("/player/admin/state", get(admin_state)) // Information from the DashMaps in state
            .nest_service("/static", static_files())
            .with_state(state.clone()); // Attach shared state

        // Nest the inner router under "/stargzr" so all routes are prefixed
        Router::new().nest("/stargzr", inner)
    }
}

/// Thin wrapper around TcpListener that sets TCP_NODELAY on every accepted socket.
/// Disables Nagle's algorithm, a TCP/IP congestion control mechanism that improves network efficiency by combining multiple small,
/// outgoing data packets into fewer, larger packets before transmission.
/// Small WebSocket frames are sent immediately
/// instead of being held in the kernel buffer waiting to be batched.
struct NoDelayListener(tokio::net::TcpListener);

impl axum::serve::Listener for NoDelayListener {
    type Io = tokio::net::TcpStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((stream, addr)) => {
                    if let Err(e) = stream.set_nodelay(true) {
                        tracing::warn!("Failed to set TCP_NODELAY: {}", e);
                    }
                    return (stream, addr);
                }
                Err(e) => {
                    tracing::error!("Accept error: {}", e);
                }
            }
        }
    }

    fn local_addr(&self) -> tokio::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

// Rust's orphan rule: you can only impl a trait for a type if either the trait or the type
// is defined in your crate. Both Connected (axum) and SocketAddr (std) are foreign, so a
// direct impl is illegal. The fix is a local newtype wrapper - it's defined in this crate,
// which satisfies the orphan rule and lets us anchor the impl here.
#[derive(Clone)]
pub struct PeerAddr(pub std::net::SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, NoDelayListener>>
    for PeerAddr
{
    fn connect_info(target: axum::serve::IncomingStream<'_, NoDelayListener>) -> Self {
        // IncomingStream wraps the (TcpStream, SocketAddr) pair that NoDelayListener::accept returns.
        // remote_addr() gives us the peer's address which is then stored in ConnectInfo<PeerAddr>
        // and made available to handlers via the ConnectInfo extractor - used by the WS rate limiter.
        //
        // Axum is designed to take the connection info, package it up, and hand it off to your route handlers as an independent, standalone piece of data. 
        // It cannot do that if the data is tethered to a temporary reference from the initial TCP handshake, which would cause a lifetime issue and possible weird threading issues.
        // That is why we dereference the remote_addr() here and store it directly in the PeerAddr struct, ensuring it lives independently of the IncomingStream's lifetime.
        // And that is why SocketAddr implments Copy. (cause it is cheap to copy, only 16 bytes)
        PeerAddr(*target.remote_addr())
    }
}

/// Starts the MP3 player server, binds to a TCP port, and runs Axum
pub async fn initialize(path_buf: PathBuf) {
    // Set up tracing/logging
    init_logging();

    // Set up Prometheus metrics recorder (global, must be called once before any metrics)
    metrics::init_metrics();

    tracing::info!("Starting stargzr server");

    // Bind TCP listener to localhost:8083
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8083")
        .await
        .expect("Failed to bind to port 8083");

    let addr = listener.local_addr().unwrap();

    tracing::info!("✓ stargzr listening on http://{}/stargzr", addr);

    // Initialize the shared state asynchronously
    let state = init_player_state(path_buf).await;

    // Create the router with async initialization
    let router = create_player_router(state.clone()).await;

    // Task for cleaning up old sessions both player and broadcast
    tokio::spawn(cleanup_stale_sessions(state.clone()));

    // into_make_service_with_connect_info propagates the peer address into handlers.
    // PeerAddr instead of SocketAddr because of the orphan rule, see Connected impl above.
    axum::serve(
        NoDelayListener(listener),
        router.into_make_service_with_connect_info::<PeerAddr>(),
    )
    // Once shutdown_signal() resolves, Axum knows to start shutting down
    // Axum stops accepting new TCP connections but keeps existing ones alive until the response completes.
    .with_graceful_shutdown(shutdown_signal(state.clone()))
    .await
    .expect("Server failed");
}

async fn shutdown_signal(state: SharedState) {
    use tokio::signal;

    // SIGINT works on all platforms
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    // SIGTERM Docker stop, systemctl stop, etc. (Unix only)
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    // On Windows, SIGTERM doesn't exist only wait for Ctrl+C
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl+C") },
        _ = terminate => { tracing::info!("Received SIGTERM") },
    }

    tracing::info!("Notifying all active broadcasters before shutdown...");

    let shutdown_msg = Arc::new(PreparedMessage::new(&RadioMessage::ServerShutdown {
        message: "Server is restarting, reconnecting automatically...".to_string(),
    }));

    match state.global_broadcast_tx.send(shutdown_msg) {
        Ok(count) => tracing::info!(clients = count, "Sent ServerShutdown to all clients"),
        Err(_) => tracing::debug!("No clients connected at shutdown"),
    }

    // Collect broadcaster IDs first to avoid holding DashMap refs across awaits
    let broadcaster_ids: Vec<String> = state
        .broadcast_states
        .iter()
        .map(|e| e.key().clone())
        .collect();

    for broadcaster_id in broadcaster_ids {
        state.broadcast_states.remove(&broadcaster_id);
        state.broadcast_channels.remove(&broadcaster_id);

        let offline_msg = Arc::new(PreparedMessage::new(&RadioMessage::BroadcasterOffline {
            broadcaster_id: broadcaster_id.clone(),
        }));

        match state.global_broadcast_tx.send(offline_msg) {
            Ok(count) => tracing::info!(
                broadcaster_id = %broadcaster_id,
                listeners = count,
                "Sent BroadcasterOffline"
            ),
            Err(_) => tracing::debug!(
                broadcaster_id = %broadcaster_id,
                "No listeners to notify"
            ),
        }
    }

    // Give the WebSocket send tasks time to flush BroadcasterOffline to clients
    // before Axum starts dropping connections
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    tracing::info!("Shutdown cleanup complete, draining HTTP connections...");
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