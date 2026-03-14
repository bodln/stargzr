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
pub use types::{AppState, BroadcastState, MediaType, RadioMessage, SharedState, MediaInfo};
pub use types::media_type_for;

use crate::player::handlers::{
    admin_state, check_session, get_subtitles, metrics_handler, radio_websocket, upload_file
};
use crate::player::types::PreparedMessage;

use self::logging::init_logging;
use axum::Router;
use axum::extract::DefaultBodyLimit;
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

/// Initializes the shared player state by scanning the music folder,
/// building a playlist, and setting up broadcast channels and session tracking.
async fn init_player_state(music_folder: PathBuf) -> SharedState {
    let mut playlist = Vec::new();

    // Read all files in the music folder asynchronously
    if let Ok(mut entries) = tokio::fs::read_dir(&music_folder).await {
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
    }

    // Sort the playlist alphabetically by filename for consistent ordering
    playlist.sort_by(|a, b| a.filename.cmp(&b.filename));

    // Create a broadcast channel for WebSocket messages (sync updates)
    // Capacity 100 means it can buffer up to 100 messages before dropping
    let (global_broadcast_tx, _) = broadcast::channel(100);

    // Return the shared application state wrapped in Arc for multi-threaded use
    Arc::new(AppState {
        playlist: Arc::new(RwLock::new(playlist)),
        music_folder: Arc::new(music_folder),
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
    })
}

/// Creates an Axum router with all the player routes, using the given music folder.
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
//             ## Why Static File Serving Is This Complicated

// To really understand why all of this is necessary, you need to understand what a web server actually is at a fundamental level, because most people's mental model of it is subtly wrong in a way that makes this whole topic confusing.

// ### The wrong mental model

// Most people think of a web server as something like a file browser — you put files somewhere on a machine, point a server at them, and the server exposes them at URLs that mirror the folder structure. Apache and Nginx work roughly this way by default, which is where that mental model comes from. You drop an `index.html` in a folder and it just appears at `/index.html`. This feels natural and intuitive.

// Axum doesn't work this way at all, and understanding *why* is the key to everything.

// ### What Axum actually is

// Axum is a **request router**, not a file server. Its entire job is to look at an incoming HTTP request, pattern-match the URL path against a list of routes you registered, and call the corresponding Rust function. That's it. It has no concept of a filesystem. It doesn't know where your binary lives, it doesn't scan directories, it doesn't expose anything automatically. When a request comes in for `/stargzr/css/player.css`, Axum walks through your route list looking for a match, and if it finds nothing it returns a 404. The file sitting on your disk at `src/player/static/css/player.css` is completely invisible — the disk and the HTTP server are two entirely separate worlds that have no connection unless you explicitly wire them together.

// This is actually the correct design for a production web framework. You don't want your server accidentally exposing files it shouldn't. You get to explicitly declare every single URL your server responds to, which gives you complete control over what's accessible.

// ### Why the browser needs those files separately

// When your Axum handler serves the HTML page, it sends the entire HTML document as a single HTTP response. The browser receives that HTML, parses it, and then encounters tags like `<link rel="stylesheet" href="/stargzr/static/css/player.css">` and `<script src="/stargzr/static/js/main.js">`. These tags are instructions to the browser to go fetch additional resources — the browser doesn't receive them embedded in the HTML, it makes completely separate HTTP requests for each one. So after the initial page load, the browser fires off eight more requests for your JS files and one for the CSS. Each of those hits your Axum server as a fresh HTTP request, and each one gets a 404 unless you have a route registered for it.

// ### What ServeDir does

// `ServeDir` is the bridge between the URL world and the filesystem world. It's not part of Axum itself — it lives in `tower-http`, which is a collection of HTTP middleware utilities built on Tower, the underlying service abstraction layer that Axum sits on top of. When you write:

// ```rust
// .nest_service("/static/css", ServeDir::new("src/player/static/css"))
// ```

// You're telling Axum: "any request whose URL starts with `/static/css` should be handed off to this `ServeDir` service." `ServeDir` then strips the URL prefix, takes whatever remains, and looks for a file with that name in the directory you gave it. So a request for `/static/css/player.css` becomes a filesystem lookup for `player.css` inside `src/player/static/css`. If the file exists, `ServeDir` reads it and streams it back as an HTTP response with the appropriate `Content-Type` header. If not, it returns a 404. That's the entire mechanism.

// The reason you need `--features fs` when adding `tower-http` is that it's a large crate with many optional capabilities — file serving, compression, CORS headers, request logging, and more. Cargo features let you opt into only what you need so you're not compiling code you don't use.

// ### Why the path is so fragile

// The path you pass to `ServeDir::new()` is a **relative filesystem path**, resolved from wherever the process's current working directory is at runtime. This is a standard operating system concept — every running process has a "current directory" and relative paths are resolved against it. The problem is that this current directory is not fixed. When you run `cargo run` from your project root, the current directory is your project root. When you run the compiled binary directly from somewhere else, it's wherever you ran it from. When Docker runs it, it depends entirely on the `WORKDIR` instruction in your Dockerfile.

// This is why `"src/player/static/css"` works on your machine when running with `cargo run` from `D:\Skola\.projekti\stargzr` — that path resolves to `D:\Skola\.projekti\stargzr\src\player\static\css`, which is exactly where your files are. But in Docker, if your `WORKDIR` is `/app` and your binary is at `/app/stargzr`, then that same relative path resolves to `/app/src/player/static/css`, which may or may not exist depending on how you structured your Docker image.

// ### Why CARGO_MANIFEST_DIR fixes it

// `env!("CARGO_MANIFEST_DIR")` is a Rust compile-time macro that expands to the absolute path of the directory containing your `Cargo.toml` — in your case, `D:\Skola\.projekti\stargzr`. Crucially, this is baked into the binary at compile time, so it's an absolute path that never changes regardless of what the current working directory is at runtime. The `concat!` macro then glues it together with the rest of the path into a single string literal.

// The catch, which is why Docker needs careful handling, is that this absolute path is the path *on the machine where the binary was compiled*. If you compile on your Windows machine and the path is `D:\Skola\.projekti\stargzr\src\player\static\css`, that path is baked into the binary — but that path doesn't exist inside a Linux Docker container. So for Docker you need to make sure the static files are copied into the image at a path that matches what's baked into the binary, or you need a different strategy like embedding the files directly into the binary using `include_str!` or the `rust-embed` crate, which would eliminate the runtime filesystem dependency entirely and make the whole question moot.


// And w eneed to change the docker file becuse ---------------------------------------------------------------------------------

// ## Why We Changed the Dockerfile in the First Place

// The original Dockerfile had two separate problems that were both silently wrong, and understanding each one requires understanding something fundamental about how Docker and compiled binaries work.

// ### Problem One: The glibc Version Mismatch

// Your original Dockerfile used `debian:bookworm-slim` as the runtime image and compiled the binary inside `rust:latest` as the builder image. These two images run different versions of Debian under the hood, and that difference is what caused the crash.

// When Rust compiles your binary on the builder image, it doesn't bundle the entire C standard library into the binary. Instead it produces a binary that says, at a very low level, "when you run me, go find glibc on this system and use that." This is called dynamic linking — the binary and the library stay separate, and the library is loaded from the host system at runtime. This is efficient because every program on a Linux machine can share one copy of glibc in memory rather than each carrying their own copy. But it creates a fragility: the binary was compiled against a specific version of glibc, and it will only run correctly on a system that has that version or newer.

// The `rust:latest` builder image is based on a newer Debian that ships glibc 2.38. Your `debian:bookworm-slim` runtime image has an older glibc. So when Docker starts the container, the OS looks at your binary, sees it requires symbols introduced in glibc 2.38, checks what it has available, finds those symbols missing, and refuses to run the binary entirely. That's the crash you were seeing — not a bug in your code, just an incompatibility between where the binary was compiled and where it was being run.

// The fix is to switch from dynamic linking to static linking using musl. Instead of the binary saying "go find this library at runtime," a musl-compiled binary has everything it needs bundled inside it. It becomes completely self-contained and the host system's glibc version becomes entirely irrelevant, because the binary never even looks for it.

// ### Problem Two: The Missing Static Files

// The second problem is more subtle. Your CSS and JS files live on your development machine at `src/player/static/`, and your Rust code references them using `CARGO_MANIFEST_DIR`, which gets baked in at compile time as an absolute path. When you compile inside the Docker builder container, that absolute path becomes `/app/src/player/static` — the path inside the container where the build happened.

// The original Dockerfile never copied those files into the final image. It only copied the compiled binary. So at runtime, the binary would start up, try to serve a request for `player.css`, look for the file at `/app/src/player/static/css/player.css`, find nothing there, and return a 404. The static files existed during the build stage but were silently left behind when the final image was assembled. Docker images only contain what you explicitly put in them — nothing is carried over from the build stage unless you have a `COPY --from=builder` line for it.

// ### Why Both Problems Existed Together

// Both of these problems stem from the same root cause: the Dockerfile was written assuming a simple "compile and copy" workflow, without accounting for the fact that the runtime environment is completely different from the build environment. The builder container has the Rust toolchain, your source code, and a specific version of glibc. The runtime container has none of those things — it's just a stripped-down Linux with your binary dropped into it. Anything the binary needs at runtime, whether that's a specific glibc version or static asset files, has to be explicitly provided. The Dockerfile is the document that specifies exactly what goes into that runtime environment, and the original version simply didn't account for those two dependencies.

            .nest_service(
                "/static/css",
                ServeDir::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/player/static/css"
                )),
            )
            .nest_service(
                "/static/js",
                ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/player/static/js")),
            )
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
        PeerAddr(*target.remote_addr())
    }
}

/// Starts the MP3 player server, binds to a TCP port, and runs Axum
pub async fn initialize(path_buf: PathBuf) {
    // Set up tracing/logging
    init_logging();

    // Set up Prometheus metrics recorder (global, must be called once before any metrics)
    metrics::init_metrics();

    tracing::info!("Starting MP3 Player server");

    // Bind TCP listener to localhost:8083
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8083")
        .await
        .expect("Failed to bind to port 8083");

    let addr = listener.local_addr().unwrap();

    tracing::info!("✓ MP3 Player listening on http://{}/stargzr", addr);

    // Initialize the shared state asynchronously
    let state = init_player_state(path_buf).await;

    // Create the router with async initialization
    let router = create_player_router(state.clone()).await;

    // Task for cleaning up old sessions both player and broadcast
    tokio::spawn(cleanup_stale_sessions(state.clone()));

    // When you call `axum::serve(listener, router)` with a plain `TcpListener` — as in

    // let addr = listener.local_addr().unwrap();

    // let router = create_player_router(state.clone()).await;

    // axum::serve(listener, router).await.expect("Server failed");

    // Axum takes ownership of that listener and runs its own internal accept loop. Concretely, Axum spawns a Tokio task that sits in a `loop`, calls `listener.accept().await` on each iteration, and for each accepted connection it spawns *another* task to handle that specific connection. Inside that per-connection task, Axum runs the full HTTP state machine: it reads bytes off the socket, parses the HTTP request line and headers, constructs a `Request<Body>`, walks the router to find a matching handler, calls that handler, serializes the `Response`, and writes it back to the socket. All of this happens entirely inside Axum's internals. You handed it the listener at the start and you never touch the process again. The socket goes straight from the OS kernel into Axum's machinery without passing through any code you wrote.

    // That's fine for basic use, but it means there's no configuration point on the socket itself. TCP has options — like `TCP_NODELAY` — that you set directly on the socket file descriptor after it's been accepted but before it's actively used. With the plain setup above, Axum accepts the socket and immediately starts using it, and you simply have no opportunity to call `set_nodelay(true)` in between. You're locked out.

    // `NoDelayListener` solves this by making you the accept loop instead of Axum. Axum's `serve()` function doesn't actually require a `TcpListener` specifically — it requires anything that implements its `Listener` trait. That trait has one core method: `accept()`, which must yield a `(socket, address)` pair. When you wrap your `TcpListener` in `NoDelayListener` and implement `Listener` on that wrapper, Axum calls *your* `accept()` on every iteration of its loop instead of reaching into a raw listener directly. Your implementation then manually delegates to the inner listener's `.accept()`, which gives you that socket in your own hands — however briefly — before you return it upward to Axum. That gap, between receiving the socket from the OS and handing it back to Axum, is where `set_nodelay(true)` lives. The wrapper isn't adding complex new logic; it's inserting a seam into a process that would otherwise be completely opaque to you. Every socket that Axum ever sees has already been configured, and because this happens inside the accept method itself, it's structurally impossible to miss a connection.

    // Once `NoDelayListener::accept()` returns that `(TcpStream, SocketAddr)` pair, Axum doesn't use those values raw. It bundles them together into a type called `IncomingStream<'_, NoDelayListener>`. Think of `IncomingStream` as Axum's internal envelope for a freshly accepted connection — it carries both the socket (needed for reading and writing bytes) and the peer address (needed for knowing *who* connected) as a single unit that can be passed around through Axum's middleware and service layers. The generic parameter `NoDelayListener` is significant: it tells Axum what the concrete `Io` and `Addr` types are inside the envelope, since different listener implementations could yield different socket types. The `IncomingStream` is what gets handed to the `Connected` trait, which brings us to the second problem.

    // Axum has a mechanism called `ConnectInfo<T>` that allows handlers to receive per-connection metadata as an extractor argument. If a handler function declares `ConnectInfo<PeerAddr>` among its parameters, Axum will automatically inject the client's address into that handler when it's called. For this to work, Axum needs to know how to produce a `T` from an `IncomingStream` at connection time — that's expressed through the `Connected` trait. You implement `Connected<IncomingStream<'_, YourListener>>` for your chosen `T`, and inside that impl you extract whatever information you want from the stream. Without any of this — as in the simplified code above — you simply can't access the peer address inside a handler at all. `axum::serve(listener, router)` with no `into_make_service_with_connect_info` means Axum never runs any `Connected` impl and never stores any `ConnectInfo` in the request extensions. A handler trying to extract `ConnectInfo<SocketAddr>` would get nothing.

    // The natural fix would be to implement `Connected<IncomingStream<'_, NoDelayListener>>` for `std::net::SocketAddr` directly — after all, that's exactly the type you want. But Rust's orphan rule prohibits this. The rule states that you can only implement a trait for a type if at least one of them is defined in your own crate. `Connected` is defined in Axum, and `SocketAddr` is defined in the standard library. Both are foreign types, so the compiler refuses the impl outright. There's no way around this with the types as they are.

    // The fix is a newtype: a local struct `PeerAddr(pub std::net::SocketAddr)` that wraps `SocketAddr`. Because `PeerAddr` is defined in your crate, it satisfies the orphan rule and gives the `Connected` impl a legal home. Inside that impl, you call `target.remote_addr()` on the `IncomingStream` to pull out the `SocketAddr`, and you rewrap it in `PeerAddr` before returning. Then `router.into_make_service_with_connect_info::<PeerAddr>()` tells Axum to call that impl for every accepted connection and store the resulting `ConnectInfo<PeerAddr>` in the request's extension map. A handler that declares `ConnectInfo<PeerAddr>` as an argument gets the peer address injected automatically and can reach the inner `SocketAddr` through `peer.0`. The newtype has zero runtime cost — it compiles to exactly the same memory layout as a bare `SocketAddr` — but it gives the type system a locally-owned name to anchor the impl.

    // The deeper pattern worth internalizing is that both wrappers are doing the same structural thing for completely different reasons. `NoDelayListener` exists because you need your own code to run at a specific moment in the connection lifecycle — you're inserting a behavioral seam. `PeerAddr` exists because the type system needs a locally-owned name to make an impl legal — you're inserting a compiler seam. But the mechanism is identical in both cases: take a foreign type, wrap it in a struct you own, implement the relevant trait on that wrapper, and you suddenly own a step in Axum's pipeline that was previously closed off to you entirely.

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