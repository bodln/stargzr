use askama::Template;
use axum::extract::{Path, State};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Router, response::Html, routing::get};
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::body::Frame;
use hyper::body::{Body, Bytes};
use hyper::server::conn::http1;
use hyper::{Method, StatusCode};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use local_ip_address;
use network_tests::primitives::{ArcToo, MutexToo};
use tokio::fs::File;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceBuilder;
use tokio_util::io::ReaderStream;

use network_tests::middlewares::{Logger, RateLimit, SpawnRequest, empty, full};

type _BoxError = Box<dyn std::error::Error + Send + Sync>;

// Main request handler - routes incoming HTTP requests
// Takes: Request with incoming body stream
// Returns: Response with a BoxBody (type-erased body that streams Bytes chunks)
async fn echo(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // Pattern match on (method, path) for manual routing
    match (req.method(), req.uri().path()) {
        // GET / - Return simple text response
        (&Method::GET, "/") => {
            // full() creates a body with all data at once (buffered, not streamed)
            Ok(Response::new(full("Try POSTing data to /echo")))
        }

        // POST /echo - Echo the request body back (streaming, no buffering)
        (&Method::POST, "/echo") => {
            // into_body() gives us the body stream, boxed() type-erases it
            // This streams frames through as they arrive - no memory buffering
            Ok(Response::new(req.into_body().boxed()))
        }

        // POST /echo/uppercase - Transform body to uppercase while streaming
        (&Method::POST, "/echo/uppercase") => {
            // map_frame() transforms each chunk as it arrives (streaming transformation)
            let frame_stream = req.into_body().map_frame(|frame| {
                // Extract data from the frame (or empty if it's trailers/metadata)
                let frame = if let Ok(data) = frame.into_data() {
                    // data is one Bytes chunk, transform each byte to uppercase
                    data.iter()
                        .map(|byte| byte.to_ascii_uppercase())
                        .collect::<Bytes>()
                } else {
                    Bytes::new()
                };
                // Wrap back into a data frame
                Frame::data(frame)
            });

            Ok(Response::new(frame_stream.boxed()))
        }

        // POST /echo/reversed - Reverse the entire body (requires buffering)
        (&Method::POST, "/echo/reversed") => {
            // Check estimated body size to prevent OOM attacks
            let upper = req.body().size_hint().upper().unwrap_or(u64::MAX);
            if upper > 1024 * 64 {
                let mut resp = Response::new(full("Body too big"));
                *resp.status_mut() = hyper::StatusCode::PAYLOAD_TOO_LARGE;
                return Ok(resp);
            }

            // collect() waits for ALL frames and concatenates them into memory
            // This is buffering - we need the entire body to reverse it
            let whole_body = req.collect().await?.to_bytes();
            let reversed_body = whole_body.iter().rev().cloned().collect::<Vec<u8>>();

            Ok(Response::new(full(reversed_body)))
        }

        // 404 for all other routes
        _ => {
            let mut not_found = Response::new(empty());
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            println!("Not found: {} {}", req.method(), req.uri().path());
            Ok(not_found)
        }
    }
}

// App state holds the counter value
// Arc = Atomic Reference Counted (thread-safe sharing)
// Mutex = Mutual Exclusion (only one thread can modify at a time)
struct AppState {
    count: MutexToo<u32>,
}

// Full page template - used for initial page load
#[derive(Template)]
#[template(path = "counter.html")]
struct GreetingTemplate {
    message: String,
    count: u32,
}

// Partial template - used for HTMX updates
#[derive(Template)]
#[template(path = "counter_partial.html")]
struct CounterPartial {
    count: u32,
}

// Implement IntoResponse for full page
impl IntoResponse for GreetingTemplate {
    fn into_response(self) -> axum::response::Response {
        match self.render() {
            // render() generates the HTML string from the template
            Ok(html) => Html(html).into_response(),
            // Wrap in Html() to set Content-Type: text/html header
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to render template: {}", err),
            )
                .into_response(),
        }
    }
}

// Implement IntoResponse for partial
impl IntoResponse for CounterPartial {
    fn into_response(self) -> axum::response::Response {
        match self.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to render template: {}", err),
            )
                .into_response(),
        }
    }
}

// Handler for GET / - Initial page load
async fn handler(State(state): State<ArcToo<AppState>>) -> GreetingTemplate {
    // State(state) extracts the Arc<AppState> from the router

    let count = *state.count.lock();
    // .lock() acquires the mutex (waits if another thread is using it)
    // .unwrap() panics if the mutex is poisoned (thread panicked while holding it)
    // * dereferences to get the u32 value

    GreetingTemplate {
        message: "Hello from Rust!".to_string(),
        count,
    }
    // Returns the full page template
    // Axum automatically calls .into_response() on this
}

// Handler for POST /increment - HTMX calls this
async fn increment(State(state): State<ArcToo<AppState>>) -> CounterPartial {
    let mut count = state.count.lock();
    // mut because we're modifying the value

    *count += 1;
    // Increment the counter
    // The * dereferences the MutexGuard to access the u32

    CounterPartial { count: *count }
    // Return ONLY the partial template
    // This becomes: "<p>Count: 6</p>"
    // HTMX receives this and updates the page
}

async fn hello() -> Html<&'static str> {
    Html(include_str!("../templates/index.html"))
}

// This struct holds all the data about our music player's current state
// Think of it as the "brain" that remembers what's playing, what's in the playlist, etc.
struct PlayerState {
    // The list of all MP3 files we found in the folder
    playlist: Vec<String>,
    // Which song in the playlist is currently selected (0 = first song, 1 = second, etc.)
    current_index: usize,
    // The actual folder path where your MP3s live
    music_folder: PathBuf,
}

// We wrap PlayerState in Arc<Mutex<>> so multiple requests can safely share and modify it
// Arc = multiple owners can reference it
// Mutex = only one can modify at a time (prevents race conditions)
type SharedState = ArcToo<MutexToo<PlayerState>>;

// This template renders the complete player page when you first visit
#[derive(Template)]
#[template(path = "player.html")]
struct PlayerTemplate {
    current_song: String,
    current_index: usize,
    total_songs: usize,
}

// This template renders ONLY the song info section that HTMX will swap in
// It's tiny - just the song name and progress info
#[derive(Template)]
#[template(path = "player_controls.html")]
struct PlayerControlsTemplate {
    current_song: String,
    current_index: usize,
    total_songs: usize,
}

// We need to tell Axum how to turn our templates into HTTP responses
impl IntoResponse for PlayerTemplate {
    fn into_response(self) -> axum::response::Response {
        match self.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {}", err),
            ).into_response(),
        }
    }
}

impl IntoResponse for PlayerControlsTemplate {
    fn into_response(self) -> axum::response::Response {
        match self.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {}", err),
            ).into_response(),
        }
    }
}

// Handler for the initial page load - GET /player
async fn player_page(State(state): State<SharedState>) -> PlayerTemplate {
    let state = state.lock();
    
    // If we have songs in the playlist, show the current one
    // If playlist is empty, show a placeholder message
    let current_song = state.playlist
        .get(state.current_index)
        .cloned()
        .unwrap_or_else(|| "No songs found".to_string());
    
    PlayerTemplate {
        current_song,
        current_index: state.current_index,
        total_songs: state.playlist.len(),
    }
}

// Handler for clicking the "Next" button - POST /player/next
async fn next_song(State(state): State<SharedState>) -> PlayerControlsTemplate {
    let mut state = state.lock();
    
    // Only advance if we're not at the last song
    if state.current_index < state.playlist.len().saturating_sub(1) {
        state.current_index += 1;
    }
    
    let current_song = state.playlist
        .get(state.current_index)
        .cloned()
        .unwrap_or_else(|| "No songs found".to_string());
    
    PlayerControlsTemplate {
        current_song,
        current_index: state.current_index,
        total_songs: state.playlist.len(),
    }
}

// Handler for clicking the "Previous" button - POST /player/prev
async fn prev_song(State(state): State<SharedState>) -> PlayerControlsTemplate {
    let mut state = state.lock();
    
    // Only go back if we're not at the first song
    if state.current_index > 0 {
        state.current_index -= 1;
    }
    
    let current_song = state.playlist
        .get(state.current_index)
        .cloned()
        .unwrap_or_else(|| "No songs found".to_string());
    
    PlayerControlsTemplate {
        current_song,
        current_index: state.current_index,
        total_songs: state.playlist.len(),
    }
}

// This is the CRITICAL function for speed - it streams the MP3 file
// Instead of loading the entire file into memory, it reads and sends chunks
async fn stream_audio(
    State(state): State<SharedState>,
    Path(index): Path<usize>,
) -> Result<axum::response::Response, StatusCode> {
    let state = state.lock();
    
    // Get the filename from our playlist
    let filename = state.playlist
        .get(index)
        .ok_or(StatusCode::NOT_FOUND)?;
    
    // Build the full path to the MP3 file
    let file_path = state.music_folder.join(filename);
    
    // Open the file asynchronously (non-blocking)
    let file = File::open(&file_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    
    // Convert the file into a stream of chunks
    // This is what makes it fast - we send data as we read it
    let stream = ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);
    
    // Set proper headers so the browser knows it's an audio file
    // Content-Type tells the browser it's MP3
    // Accept-Ranges enables seeking (jumping to different parts of the song)
    Ok(Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "audio/mpeg")
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .body(body)
        .unwrap())
}

// Initialize the music player by scanning the folder for MP3 files
async fn init_player_state(music_folder: PathBuf) -> SharedState {
    let mut playlist = Vec::new();
    
    // Read all entries in the directory
    if let Ok(mut entries) = tokio::fs::read_dir(&music_folder).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            // Only include files that end with .mp3
            if let Some(filename) = entry.file_name().to_str() {
                if filename.ends_with(".mp3") {
                    playlist.push(filename.to_string());
                }
            }
        }
    }
    
    // Sort alphabetically so songs are in a predictable order
    playlist.sort();
    
    ArcToo::new(MutexToo::new(PlayerState {
        playlist,
        current_index: 0,
        music_folder,
    }))
}

async fn add_ngrok_header(mut req: Request<axum::body::Body>, next: Next) -> axum::response::Response {
    req.headers_mut().insert(
        "ngrok-skip-browser-warning",
        "true".parse().unwrap(),
    );

    let mut res = next.run(req).await;
    res.headers_mut().insert(
        "ngrok-skip-browser-warning",
        "true".parse().unwrap(),
    );
    res
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = 7878;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let global_count = Arc::new(Mutex::new(0));

    let listener = TcpListener::bind(addr).await.unwrap();

    let mut help_ip = String::from("oops");

    match local_ip_address::local_ip() {
        Ok(ip) => {
            help_ip = ip.to_string();
            println!("Local IP address: {}:{}", ip, port);
        }
        Err(e) => eprintln!("Error: {}", e),
    }

    let graceful = Arc::new(hyper_util::server::graceful::GracefulShutdown::new());
    let mut signal = std::pin::pin!(shutdown_signal());

    let music_folder = PathBuf::from("D:/Skola/.projekti/Tests/NetworkTests/NetworkTests/music");
    let player_state = init_player_state(music_folder).await;
    // ngrok http --url=evolved-gladly-possum.ngrok-free.app 8083
    tokio::task::spawn(async move {
        let app = Router::new()
            .route("/player", get(player_page))
            .route("/player/next", post(next_song))
            .route("/player/prev", post(prev_song))
            .route("/player/stream/{index}", get(stream_audio))
            .with_state(player_state);
        
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:8083").await {
            Ok(listener) => {
                println!(
                    "✓ MP3 Player running on http://{}/player -> https://evolved-gladly-possum.ngrok-free.app/player",
                    listener.local_addr().unwrap()
                );
                listener
            }
            Err(e) => {
                eprintln!("✗ Failed to bind player server: {}", e);
                return;
            }
        };
        
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("✗ Player server error: {}", e);
        }
    });

    // Setting up the server (inside your tokio::spawn)
    tokio::task::spawn(async {
        // Create shared state
        let app_state = ArcToo::new(AppState {
            count: MutexToo::new(0), // Start at 0
        });

        let app = Router::new()
            .route("/", get(handler))
            // GET / returns the full page
            .route("/increment", post(increment))
            // POST /increment returns just the counter partial
            .with_state(app_state);
        // Share the app_state with all handlers

        let listener = tokio::net::TcpListener::bind("127.0.0.1:8081")
            .await
            .unwrap();

        println!(
            "✓ HTML server listening on {}",
            listener.local_addr().unwrap()
        );
        axum::serve(listener, app).await.unwrap();
    });

    // Client
    tokio::task::spawn(async move {
        let uri_str = format!("http://{}:{}", help_ip, port);

        let uri: hyper::Uri = uri_str.parse().expect("Invalid URI");

        let host = uri.host().expect("Uri has no host");
        let port = uri.port_u16().unwrap_or(8080);

        let address = format!("{}:{}", host, port);

        println!("Client prepared for host: {}:{}", host, port);

        // Uncomment for testing the middlewares

        // let output = Command::new("hey")
        //     .args(["-z", "2s", "-c", "3", "http://192.168.1.2:7878/"])
        //     .output().await
        //     .expect("failed to execute hey");

        // let stdout = String::from_utf8_lossy(&output.stdout);
        // let stderr = String::from_utf8_lossy(&output.stderr);

        // println!("--- hey output ---\n{stdout}");
        // if !stderr.is_empty() {
        //     eprintln!("--- hey errors ---\n{stderr}");
        // }

        let _stream = TcpStream::connect(address).await;
    });

    loop {
        tokio::select! {
            Ok((stream, address)) = listener.accept() => {
                let io = TokioIo::new(stream);
                let global_count_clone = global_count.clone();

                let graceful_clone = graceful.clone();

                tokio::task::spawn(async move {
                    println!("Accepted connection from: {}", address);

                    let delay = Duration::from_millis(1000);
                    let requests_per_delay = 1;

                    let svc = tower::service_fn(echo);
                    let svc = ServiceBuilder::new()
                        .layer_fn(SpawnRequest::new)
                        .layer_fn(Logger::new)
                        .layer_fn(move |inner| {
                            RateLimit::with_shared_counter(inner, requests_per_delay, delay, address, global_count_clone.clone())
                        })
                        .service(svc);

                    let svc = TowerToHyperService::new(svc);

                    let conn = http1::Builder::new().serve_connection(io, svc);

                    let shutdown = graceful_clone.watch(conn);

                    if let Err(err) = shutdown.await {
                        eprintln!("Server error serving connection: {}", err);
                    }
                });
            },

            _ = &mut signal => {
                drop(listener);
                eprintln!("Graceful shutdown signal received");
                break;
            },
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler");
}
