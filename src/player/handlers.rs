use axum::extract::{ConnectInfo, Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::http::header;
use serde::Deserialize;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::player::PeerAddr;
use crate::player::radio::handle_radio_connection;
use crate::player::validation::SessionId;

use super::session::{get_session_id, get_or_create_position, update_session_index, update_broadcast_index};
use super::templates::{PlayerTemplate, PlayerControlsTemplate};
use super::types::{SharedState, SongInfo};

/// Renders the main player page for the current user session.
pub async fn player_page(State(state): State<SharedState>, headers: HeaderMap) -> PlayerTemplate {
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
pub async fn next_song(State(state): State<SharedState>, headers: HeaderMap) -> PlayerControlsTemplate {
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
pub async fn prev_song(State(state): State<SharedState>, headers: HeaderMap) -> PlayerControlsTemplate {
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
pub struct ControlsQuery {
    broadcaster: Option<String>,
}

/// Returns the current player control state as an HTML fragment.
///
/// This handler is designed to be HTMX-friendly:
/// it returns only the inner player controls markup, not a full page.
pub async fn player_controls(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<ControlsQuery>,
) -> PlayerControlsTemplate {
    // If a broadcaster ID is provided, attempt to mirror its playback state
    if let Some(broadcaster_id) = query.broadcaster {
        // Broadcast state is shared, read-only, and authoritative for listeners
        if let Some(broadcast) = state.broadcast_states.get(&broadcaster_id) {
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

/// Stream audio by song ID instead of index
// #[tracing::instrument] creates a span for this function and correctly re-enters it on every
// poll across .await points, using span.enter() + _guard directly in async code is wrong
// because tokio can resume the future on a different thread, entering/exiting on mismatched threads.
// skip(state, headers) because they don't implement Debug and would otherwise cause a compile error.
// fields() picks exactly what gets attached to the span, everything else is excluded.
#[tracing::instrument(skip(state, headers), fields(song_id = %song_id, session_id = tracing::field::Empty))]
pub async fn stream_audio_by_id(
    State(state): State<SharedState>,
    Path(song_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Record the session so streaming logs are tied to who requested it
    let session_id = get_session_id(&headers);
    tracing::Span::current().record("session_id", &session_id.as_str());

    let song = state
        .playlist
        .iter()
        .find(|s| s.id == song_id)
        .ok_or_else(|| {
            tracing::warn!("Song not found for id: {}", song_id);
            StatusCode::NOT_FOUND
        })?;

    stream_audio_internal(&state, song, &headers).await
}

/// Stream audio by song index
// Same async-safe span reasoning as stream_audio_by_id above.
// skip(state, headers) because they don't implement Debug and we don't need them in the span.
#[tracing::instrument(skip(state, headers), fields(index, session_id = tracing::field::Empty))]
pub async fn stream_audio_by_index(
    State(state): State<SharedState>,
    Path(index): Path<usize>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Record the session so streaming logs are tied to who requested it
    let session_id = get_session_id(&headers);
    tracing::Span::current().record("session_id", &session_id.as_str());

    let song = state.playlist.get(index).ok_or_else(|| {
        tracing::warn!("Song not found at index: {}", index);
        StatusCode::NOT_FOUND
    })?;

    stream_audio_internal(&state, song, &headers).await
}

/// Parses a "bytes=start-end" range header into (start, end) byte offsets.
/// Returns None if the header is missing or unparseable callers fall back to full file.
/// The previous code silently falls through and serves the full file instead of returning a 416 Range Not Satisfiable error.
/// This change is purely about making the fallthrough behavior explicit and readable if anything fails,
/// None is returned and you know exactly why.
fn parse_range_header(headers: &HeaderMap, file_size: u64) -> Option<(u64, u64)> {
    let range_str = headers.get(header::RANGE)?.to_str().ok()?;
    let range_str = range_str.strip_prefix("bytes=")?;

    // Range requests have the format "start-end", and both parts are optional.
    // You might see "bytes=0-999" (first 1000 bytes), "bytes=2000000-" (from byte 2 million to the end),
    // or even "bytes=-1000" (last 1000 bytes, though we don't handle this case)
    let (start_str, end_str) = range_str.split_once('-')?;

    let start: u64 = start_str.parse().ok()?;
    let end: u64 = if end_str.is_empty() {
        file_size - 1
    } else {
        end_str.parse::<u64>().ok()?.min(file_size - 1)
    };

    Some((start, end))
}

/// Streams an audio file to the client, with full support for HTTP byte-range requests.
/// This handler is intentionally stateless: it does not modify playback state
/// or session position, it only serves file data.
// Inner span, automatically nested under stream_by_id or stream_by_index because instrument
// propagates the parent span context through the call, so logs show both how the song was
// requested and what happened serving it.
// debug level only: every seek during playback fires a range request so this would be
// extremely noisy at info.
#[tracing::instrument(skip(state, headers), fields(song = %song.filename, size = song.size))]
pub async fn stream_audio_internal(
    state: &SharedState,
    song: &SongInfo,
    headers: &HeaderMap,
) -> Result<Response, StatusCode> {
    let file_path = state.music_folder.join(&song.filename);
    let file_size = song.size;

    if let Some((start, end)) = parse_range_header(headers, file_size) {
        let mut file = File::open(&file_path)
            .await
            .map_err(|e| {
                tracing::error!("Failed to open file: {}", e);
                StatusCode::NOT_FOUND
            })?;

        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| {
                tracing::error!("Seek failed: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        let length = end - start + 1; // example end = 0, start = 0, that is counted as one byte (the first one)
        tracing::debug!(start, end, length, "Serving range request");

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

    // If there is no range specified just start from the beginning
    tracing::debug!("Serving full file");

    let file = File::open(&file_path)
        .await
        .map_err(|e| {
            tracing::error!("Failed to open file: {}", e);
            StatusCode::NOT_FOUND
        })?;

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
pub async fn radio_websocket(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    headers: HeaderMap,
    // ConnectInfo is populated by axum at accept time via the Connected impl on NodeDelayListener.
    // Gives us the peer's IP without touching headers, so it can't be spoofed by the client.
    // Requires into_make_service_with_connect_info in serve() - plain into_make_service won't inject this.
    ConnectInfo(addr): ConnectInfo<PeerAddr>,
) -> impl IntoResponse {
    let ip = addr.0.ip().to_string();
    // Reject upgrade if this IP is hammering connections
    if let Err(_) = state.ws_rate_limiter.check_and_consume(&ip) {
        crate::player::metrics::inc_ws_rejected();
        tracing::warn!(ip = %ip, "WebSocket upgrade rejected: rate limit exceeded");
        return (StatusCode::TOO_MANY_REQUESTS, "Too many connections").into_response();
    }
    let session_id_str = get_session_id(&headers);
    // This is the session id first given to the user
    // It is used so if someone tampers with their original session their calls are moot
    let validated_session_id = match SessionId::new(session_id_str) {
        Ok(id) => id,
        Err(e) => {
            // If the session ID is invalid, reject the WebSocket upgrade
            tracing::warn!(
                ip = %ip,
                "Rejected WebSocket connection with invalid session ID: {}",
                e
            );
            return (StatusCode::BAD_REQUEST, "Invalid session ID").into_response();
        }
    };
    crate::player::metrics::inc_ws_connections();
    ws.on_upgrade(|socket| {
        handle_radio_connection(socket, state, validated_session_id.into_inner())
    })
}

/// Returns the full playlist with song IDs for client-side management
pub async fn get_playlist(State(state): State<SharedState>) -> impl IntoResponse {
    let playlist: Vec<SongInfo> = state.playlist.iter().cloned().collect();
    axum::Json(playlist)
}

/// Returns 200 if session cookie maps to a live session, 401 otherwise.
/// Called by the frontend on audio play to detect expired sessions early.
pub async fn check_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let valid = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| c.trim().strip_prefix("player_session="))
        })
        .map(|id| state.sessions.contains_key(id))
        .unwrap_or(false);
    if valid { StatusCode::OK } else { StatusCode::UNAUTHORIZED }
}

/// Serves the Prometheus metrics scrape endpoint.
pub async fn metrics_handler() -> impl IntoResponse {
    crate::player::metrics::render()
}