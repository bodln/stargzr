use askama::Template;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{
    Router,
    http::{HeaderMap, header},
    response::Html,
    routing::get,
};
use hyper::StatusCode;
use hyper::Response;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Clone)]
struct SongInfo {
    filename: String,
    size: u64,
    _duration_secs: Option<u32>, // Could parse from ID3 tags
}

// Represents a single user's player state
struct PlayerSession {
    current_index: usize,
    last_activity: std::time::Instant,
}

struct AppState {
    // Read-only shared data (no locking needed for reads)
    playlist: Arc<Vec<SongInfo>>,
    music_folder: Arc<PathBuf>,

    // Per-user sessions (needs locking)
    sessions: RwLock<HashMap<String, PlayerSession>>,
}

type SharedState = Arc<AppState>;

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
    fn into_response(self) -> axum::response::Response {
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
    fn into_response(self) -> axum::response::Response {
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

fn get_or_create_session(state: &AppState, session_id: &str) -> usize {
    let mut sessions = state.sessions.write();

    // Cleanup old sessions (older than 1 hour)
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

async fn player_page(State(state): State<SharedState>, headers: HeaderMap) -> PlayerTemplate {
    let session_id = get_session_id(&headers);
    let current_index = get_or_create_session(&state, &session_id);

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
    let current_index = get_or_create_session(&state, &session_id);

    let new_index = (current_index + 1).min(state.playlist.len().saturating_sub(1));
    update_session_index(&state, &session_id, new_index);

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
    let current_index = get_or_create_session(&state, &session_id);

    let new_index = current_index.saturating_sub(1);
    update_session_index(&state, &session_id, new_index);

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

async fn stream_audio(
    State(state): State<SharedState>,
    Path(index): Path<usize>,
    headers: HeaderMap,
) -> Result<axum::response::Response, StatusCode> {
    // Get song info
    let song = state.playlist.get(index).ok_or(StatusCode::NOT_FOUND)?;
    let file_path = state.music_folder.join(&song.filename);
    let file_size = song.size;

    // Parse Range header if present
    if let Some(range_value) = headers.get(header::RANGE) {
        if let Ok(range_str) = range_value.to_str() {
            if let Some(range_str) = range_str.strip_prefix("bytes=") {
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

                    let length = end - start + 1;
                    let limited_file = file.take(length);
                    let stream = ReaderStream::with_capacity(limited_file, 128 * 1024);
                    let body = axum::body::Body::from_stream(stream);

                    return Ok(Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_TYPE, "audio/mpeg")
                        .header(header::ACCEPT_RANGES, "bytes")
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {}-{}/{}", start, end, file_size),
                        )
                        .header(header::CONTENT_LENGTH, length.to_string())
                        .header(header::CACHE_CONTROL, "public, max-age=31536000")
                        .body(body)
                        .unwrap());
                }
            }
        }
    }

    // No range - stream full file
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

    Arc::new(AppState {
        playlist: Arc::new(playlist),
        music_folder: Arc::new(music_folder),
        sessions: RwLock::new(HashMap::new()),
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
            .with_state(state)
    }
}

pub async fn initialize(player_router: Router) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8083")
            .await
            .unwrap();

        println!(
            "✓ MP3 Player on http://{}/player -> https://evolved-gladly-possum.ngrok-free.app/player",
            listener.local_addr().unwrap()
        );
        axum::serve(listener, player_router).await.unwrap();
}