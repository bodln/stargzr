use axum::extract::{ConnectInfo, Multipart, Path, Query, State, WebSocketUpgrade};
use axum::http::header;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::player::PeerAddr;
use crate::player::error::PlayerError;
use crate::player::radio::handle_radio_connection;
use crate::player::types::{AdminSession, AdminState, MediaType};
use crate::player::types::media_type_for;
use crate::player::validation::SessionId;

use super::session::{
    get_or_create_position, get_session_id, update_broadcast_index, update_session_index,
};
use super::templates::{PlayerControlsTemplate, PlayerTemplate};
use super::types::{SharedState, MediaInfo};

// 5 GB total folder cap
const MAX_FOLDER_BYTES: u64 = 5 * 1024 * 1024 * 1024;
// 200 MB per IP per server session
const MAX_IP_QUOTA_BYTES: u64 = 200 * 1024 * 1024;

/// Maps a filename extension to the correct HTTP Content-Type.
/// Browsers use this to decide how to handle the response, incorrect values
/// will cause the browser to refuse to play the file even if the bytes are valid.
/// Video uploads are always converted to MP4 on ingest, so only video/mp4 is needed here.
/// MKV, MOV, and AVI are accepted as uploads but never stored or served,
/// they are replaced by a converted .mp4 before the playlist entry is created.
fn content_type_for(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mp3"  => "audio/mpeg",
        "m4a"  => "audio/mp4",
        "ogg"  => "audio/ogg",
        "wav"  => "audio/wav",
        "flac" => "audio/flac",
        "mp4"  => "video/mp4",
        "webm" => "video/webm",
        _      => "application/octet-stream",
    }
}

/// Renders the main player page for the current user session.
pub async fn player_page(State(state): State<SharedState>, headers: HeaderMap) -> PlayerTemplate {
    // Identify the user session (cookie-based, falls back to UUID)
    let session_id = get_session_id(&headers);

    // Fetch or initialize the session's current playlist index
    let current_index = get_or_create_position(&state, &session_id);

    // Acquire a read lock, clone what we need, then drop the guard before any await
    let (current_media, total_medias) = {
        let playlist = state.playlist.read().await;
        let media = playlist
            .get(current_index)
            .map(|s| s.filename.clone())
            .unwrap_or_else(|| "No medias found".to_string());
        (media, playlist.len())
    };

    // Render the full player page
    PlayerTemplate {
        current_media,
        current_index,
        total_medias,
        session_id,
    }
}

/// Advances the current session to the next media in the playlist.
pub async fn next_media(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> PlayerControlsTemplate {
    // Identify the user session
    let session_id = get_session_id(&headers);

    // Get the current position for this session
    let current_index = get_or_create_position(&state, &session_id);

    // Acquire a read lock, compute new index and media name, then drop the guard
    let (new_index, current_media, total_medias) = {
        let playlist = state.playlist.read().await;
        let new_index = (current_index + 1).min(playlist.len().saturating_sub(1));
        let media = playlist
            .get(new_index)
            .map(|s| s.filename.clone())
            .unwrap_or_else(|| "No medias found".to_string());
        (new_index, media, playlist.len())
    };

    // Update private playback position
    update_session_index(&state, &session_id, new_index);

    // If this session is a broadcaster, propagate the change to listeners
    update_broadcast_index(&state, &session_id, new_index);

    // Touch the session so passive listeners don't get cleaned up mid-media.
    // Range requests fire on every seek so this naturally stays fresh during playback.
    if let Some(mut session) = state.sessions.get_mut(&session_id) {
        session.last_activity = std::time::Instant::now();
    }

    // Return updated control state (used for partial page updates)
    PlayerControlsTemplate {
        current_media,
        current_index: new_index,
        total_medias,
    }
}

/// Moves the current session to the previous media in the playlist.
pub async fn prev_media(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> PlayerControlsTemplate {
    // Identify the user session
    let session_id = get_session_id(&headers);

    // Get the current position for this session
    let current_index = get_or_create_position(&state, &session_id);

    // Acquire a read lock, compute new index and media name, then drop the guard
    let (new_index, current_media, total_medias) = {
        let playlist = state.playlist.read().await;
        let new_index = current_index.saturating_sub(1);
        let media = playlist
            .get(new_index)
            .map(|s| s.filename.clone())
            .unwrap_or_else(|| "No medias found".to_string());
        (new_index, media, playlist.len())
    };

    // Update private playback position
    update_session_index(&state, &session_id, new_index);

    // If this session is a broadcaster, propagate the change to listeners
    update_broadcast_index(&state, &session_id, new_index);

    // Touch the session so passive listeners don't get cleaned up mid-media.
    // Range requests fire on every seek so this naturally stays fresh during playback.
    if let Some(mut session) = state.sessions.get_mut(&session_id) {
        session.last_activity = std::time::Instant::now();
    }

    // Return updated control state (used for partial page updates)
    PlayerControlsTemplate {
        current_media,
        current_index: new_index,
        total_medias,
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
        // Extract the media index without holding the DashMap ref across an await
        let maybe_index = state
            .broadcast_states
            .get(&broadcaster_id)
            .map(|b| b.media_index);

        if let Some(index) = maybe_index {
            let (current_media, total_medias) = {
                let playlist = state.playlist.read().await;
                let media = playlist
                    .get(index)
                    .map(|s| s.filename.clone())
                    .unwrap_or_else(|| "No medias found".to_string());
                (media, playlist.len())
            };

            // Return controls reflecting the broadcaster's state
            return PlayerControlsTemplate {
                current_media,
                current_index: index,
                total_medias,
            };
        }
    }

    // No broadcaster specified (or broadcaster missing), so render controls
    // based on the caller's own session state.
    let session_id = get_session_id(&headers);
    let index = get_or_create_position(&state, &session_id);

    let (current_media, total_medias) = {
        let playlist = state.playlist.read().await;
        let media = playlist
            .get(index)
            .map(|s| s.filename.clone())
            .unwrap_or_else(|| "No medias found".to_string());
        (media, playlist.len())
    };

    PlayerControlsTemplate {
        current_media,
        current_index: index,
        total_medias,
    }
}

/// Stream audio by media ID instead of index
// #[tracing::instrument] creates a span for this function and correctly re-enters it on every
// poll across .await points using span.enter() + _guard directly in async code is wrong
// because tokio can resume the future on a different thread, entering/exiting on mismatched threads.
// skip(state, headers) because they don't implement Debug and would otherwise cause a compile error.
// fields() picks exactly what gets attached to the span, everything else is excluded.
#[tracing::instrument(skip(state, headers), fields(media_id = %media_id, session_id = tracing::field::Empty, ip = tracing::field::Empty))]
pub async fn stream_audio_by_id(
    State(state): State<SharedState>,
    Path(media_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Record the session so streaming logs are tied to who requested it
    let session_id = get_session_id(&headers);
    tracing::Span::current().record("session_id", &session_id.as_str());

    let ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    tracing::Span::current().record("ip", ip);

    // Clone the media out before the guard drops so no reference crosses an await point
    let media = {
        let playlist = state.playlist.read().await;
        playlist
            .iter()
            .find(|s| s.id == media_id)
            .cloned()
            .ok_or_else(|| {
                tracing::warn!("Media not found for id: {}", media_id);
                StatusCode::NOT_FOUND
            })?
    };

    stream_audio_internal(&state, &media, &headers, &session_id).await
}

/// Stream audio by media index
// Same async-safe span reasoning as stream_audio_by_id above.
// skip(state, headers) because they don't implement Debug and we don't need them in the span.
#[tracing::instrument(skip(state, headers), fields(index = tracing::field::Empty, session_id = tracing::field::Empty, ip = tracing::field::Empty))]
pub async fn stream_audio_by_index(
    State(state): State<SharedState>,
    Path(index): Path<usize>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Record the session so streaming logs are tied to who requested it
    let session_id = get_session_id(&headers);
    tracing::Span::current().record("session_id", &session_id.as_str());
    tracing::Span::current().record("index", index);

    let ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    tracing::Span::current().record("ip", ip);

    // Clone the media out before the guard drops so no reference crosses an await point
    let media = {
        let playlist = state.playlist.read().await;
        playlist.get(index).cloned().ok_or_else(|| {
            tracing::warn!("Media not found at index: {}", index);
            StatusCode::NOT_FOUND
        })?
    };

    stream_audio_internal(&state, &media, &headers, &session_id).await
}

/// Parses a "bytes=start-end" range header into (start, end) byte offsets.
/// Returns None if the header is missing or unparseable, callers fall back to full file.
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
// Inner span automatically nested under stream_by_id or stream_by_index because instrument
// propagates the parent span context through the call, so logs show both how the media was
// requested and what happened serving it.
// debug level only: every seek during playback fires a range request so this would be
// extremely noisy at info.
#[tracing::instrument(skip(state, headers, session_id), fields(media = %media.filename, size = media.size))]
pub async fn stream_audio_internal(
    state: &SharedState,
    media: &MediaInfo,
    headers: &HeaderMap,
    session_id: &str,
) -> Result<Response, StatusCode> {
    let file_path = state.music_folder.join(&media.filename);
    let file_size = media.size;

    // Touch the session so passive listeners don't get cleaned up mid-media.
    // Range requests fire on every seek so this naturally stays fresh during playback.
    if let Some(mut session) = state.sessions.get_mut(session_id) {
        session.last_activity = std::time::Instant::now();
    }

    // Derived from the file extension so audio and video files both get the right MIME type
    let content_type = content_type_for(&media.filename);

    if let Some((start, end)) = parse_range_header(headers, file_size) {
        let mut file = File::open(&file_path).await.map_err(|e| {
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
        let stream = ReaderStream::with_capacity(limited_file, 512 * 1024); // we read 128 kilobytes at a time
        let body = axum::body::Body::from_stream(stream);

        return Ok(Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, content_type)
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
            // (seeking to previously loaded part of the media)
            .header(header::CACHE_CONTROL, "public, max-age=31536000")
            .body(body)
            .unwrap());
    }

    // If there is no range specified just start from the beginning
    tracing::debug!("Serving full file");

    let file = File::open(&file_path).await.map_err(|e| {
        tracing::error!("Failed to open file: {}", e);
        StatusCode::NOT_FOUND
    })?;

    let stream = ReaderStream::with_capacity(file, 512 * 1024);
    let body = axum::body::Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::CACHE_CONTROL, "public, max-age=31536000")
        .body(body)
        .unwrap())
}

/// Serves the WebVTT subtitle file for a given media ID.
/// Returns 404 if the media doesn't exist or has no associated .vtt file.
/// Not every file has subtitles, the frontend should handle 404 gracefully
/// by simply not showing a subtitle track.
pub async fn get_subtitles(
    State(state): State<SharedState>,
    Path(media_id): Path<String>,
) -> Result<Response, StatusCode> {
    // Resolve the media filename from the playlist, then derive the .vtt path from it
    let vtt_filename = {
        let playlist = state.playlist.read().await;
        playlist
            .iter()
            .find(|s| s.id == media_id)
            .map(|s| {
                s.filename
                    .rsplit_once('.')
                    .map(|(stem, _)| format!("{}.vtt", stem))
                    .unwrap_or_default()
            })
            .ok_or(StatusCode::NOT_FOUND)?
    };

    let path = state.music_folder.join(&vtt_filename);

    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/vtt")
        // Subtitles are small and don't change, cache aggressively like the media files
        .header(header::CACHE_CONTROL, "public, max-age=31536000")
        .body(axum::body::Body::from(data))
        .unwrap())
}

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
    // Prefer X-Real-IP set by Caddy over the peer address.
    // Behind a reverse proxy the peer is always 127.0.0.1 so ConnectInfo
    // is useless for rate limiting - the real client IP comes from the header.
    let ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| addr.0.ip().to_string());

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

    // Reject the upgrade if the session cookie doesn't map to a live server session.
    // This catches expired sessions that passed cookie validation but were cleaned up.
    // The frontend detects the 401 and reloads to get a fresh session.
    if !state.sessions.contains_key(validated_session_id.as_str()) {
        tracing::warn!(ip = %ip, session_id = %validated_session_id, "WebSocket upgrade rejected: session not found");
        return (StatusCode::UNAUTHORIZED, "Session expired").into_response();
    }

    crate::player::metrics::inc_ws_connections();

    tracing::info!(ip = %ip, "IP attempting to upgrade connection.");

    ws.on_upgrade(|socket| {
        handle_radio_connection(socket, state, validated_session_id.into_inner(), ip)
    })
}

/// Returns the full playlist with media IDs for client-side management
pub async fn get_playlist(State(state): State<SharedState>) -> impl IntoResponse {
    let playlist = state.playlist.read().await;
    let medias: Vec<MediaInfo> = playlist.iter().cloned().collect();
    axum::Json(medias)
}

/// Returns all non-media files and subdirectories inside the music folder.
/// Files carry their byte size; directories carry size 0 and is_dir: true.
/// Directories sort before files, then everything sorts alphabetically.
pub async fn get_other_files(State(state): State<SharedState>) -> impl IntoResponse {
    #[derive(serde::Serialize)]
    struct OtherFile {
        filename: String,
        size: u64,
        is_dir: bool,
    }

    let mut entries: Vec<OtherFile> = Vec::new();

    if let Ok(mut read_dir) = tokio::fs::read_dir(&*state.music_folder).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                    if let Ok(meta) = entry.metadata().await {
                    if meta.is_dir() {
                        // Include subdirectories so the frontend can offer a zip download
                        entries.push(OtherFile {
                            filename: name.to_string(),
                            size: 0,
                            is_dir: true,
                        });
                    } else if meta.is_file() && crate::player::types::media_type_for(name).is_none() {
                        // Include non-media files (PDFs, ZIPs, etc.)
                        entries.push(OtherFile {
                                filename: name.to_string(),
                                size: meta.len(),
                            is_dir: false,
                            });
                    }
                }
            }
        }
    }

    // Directories first, then files, both groups sorted alphabetically
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.filename.cmp(&b.filename)));

    axum::Json(entries)
}

/// Serves a non-media file from the music folder as a download.
/// Used by the "Other" tab for files that don't have playlist IDs.
pub async fn download_file(
    State(state): State<SharedState>,
    Path(filename): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let safe = sanitize_filename(&filename);
    let path = state.music_folder.join(&safe);

    if !path.exists() || !path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }

    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(ip = %ip, file = %safe, bytes = data.len(), "Serving folder zip");

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", safe),
        )
        .body(axum::body::Body::from(data))
        .unwrap())
}

/// Zips an entire subdirectory of the music folder and serves it as a download.
/// The zip is written to a temp file on a blocking thread so the async runtime
/// isn't blocked and so the zip crate can seek (it must seek to write the
/// central directory).  The temp file is streamed back in 512 KB chunks and
/// deleted automatically once the response body is fully consumed, the client
/// disconnects, or zipping fails partway through, it can never leak.
pub async fn download_folder(
    State(state): State<SharedState>,
    Path(foldername): Path<String>,
) -> Result<Response, StatusCode> {
    let safe = sanitize_filename(&foldername);
    let path = state.music_folder.join(&safe);

    if !path.exists() || !path.is_dir() {
        return Err(StatusCode::NOT_FOUND);
    }

    tracing::info!(folder = %safe, "Folder zip requested, starting zip...");

    // DeleteOnDrop is created immediately when the temp file is created so it
    // is cleaned up on every exit path: success, zip error, and panic.
    // On success we call .into_path() which moves ownership out and disarms it,
    // then pass the path to a second DeleteOnDrop in the stream closure.
    struct DeleteOnDrop(Option<std::path::PathBuf>);

    impl DeleteOnDrop {
        fn new(p: std::path::PathBuf) -> Self { Self(Some(p)) }
        // Disarms the guard and returns the path, caller takes ownership of cleanup
        fn into_path(mut self) -> std::path::PathBuf { self.0.take().unwrap() }
    }
    
    impl Drop for DeleteOnDrop {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                if let Err(e) = std::fs::remove_file(&p) {
                    tracing::warn!(path = %p.display(), "Failed to delete temp zip: {}", e);
                } else {
                    tracing::debug!(path = %p.display(), "Deleted temp zip");
                }
            }
        }
    }

    let safe_clone = safe.clone();
    let tmp_path = tokio::task::spawn_blocking(move || -> std::io::Result<std::path::PathBuf> {
        let tmp = std::env::temp_dir().join(format!("stargzr_{}.zip", uuid::Uuid::new_v4()));

        tracing::info!(folder = %safe_clone, tmp = %tmp.display(), "Zipping to temp file");

        // Guard is armed immediately — any early return from this closure
        // (zip error, IO error, panic) will delete the partially-written file
        let guard = DeleteOnDrop::new(tmp.clone());

        let file = std::fs::File::create(&tmp)?;
        // BufWriter reduces the number of seek + write syscalls the zip crate issues
        let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
        zip_dir_recursive(&mut zip, &path, "")?;
        zip.finish().map_err(|e| std::io::Error::other(e.to_string()))?;

        tracing::info!(folder = %safe_clone, "Zip complete, returning temp path");

        // Disarm: zipping succeeded, hand path back to the async side which
        // will install its own DeleteOnDrop on the stream closure
        Ok(guard.into_path())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|e| {
        tracing::error!("Failed to zip directory: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let zip_size = tokio::fs::metadata(&tmp_path)
        .await
        .map(|m| m.len())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(folder = %safe, bytes = zip_size, "Zip ready, starting download stream");

    let file = tokio::fs::File::open(&tmp_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Second guard on the stream side: deleted when the last byte is sent
    // or when the client disconnects before the download finishes.
    let stream_guard = DeleteOnDrop::new(tmp_path);

    let stream = ReaderStream::with_capacity(file, 512 * 1024)
        // The move keeps `stream_guard` alive for the lifetime of the stream.
        // When the stream is dropped, `stream_guard` is dropped, deleting the temp file.
        .map(move |chunk| { let _ = &stream_guard; chunk });

    let body = axum::body::Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.zip\"", safe),
        )
        .header(header::CONTENT_LENGTH, zip_size.to_string())
        .body(body)
        .unwrap())
}

/// Walks `dir` recursively and writes every file into a zip archive.
/// Called with a File-backed ZipWriter so the archive is never held in memory.
fn zip_dir_recursive<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &std::path::Path,
    prefix: &str,
) -> std::io::Result<()> {
    use std::io::Write;

    let options = zip::write::SimpleFileOptions::default();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta  = entry.metadata()?;
        let name  = entry.file_name();
        let name_str = name.to_string_lossy();

        // Build the archive-internal path, e.g. "subfolder/track.mp3"
        let zip_path = if prefix.is_empty() {
            name_str.to_string()
        } else {
            format!("{}/{}", prefix, name_str)
        };

        if meta.is_file() {
            zip.start_file(&zip_path, options)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let data = std::fs::read(entry.path())?;
            zip.write_all(&data)?;
        } else if meta.is_dir() {
            zip.add_directory(&zip_path, options)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            zip_dir_recursive(zip, &entry.path(), &zip_path)?;
        }
    }

    Ok(())
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
            cookies
                .split(';')
                .find_map(|c| c.trim().strip_prefix("player_session="))
        })
        .map(|id| state.sessions.contains_key(id))
        .unwrap_or(false);
    if valid {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    }
}

/// Serves the Prometheus metrics scrape endpoint.
pub async fn metrics_handler() -> impl IntoResponse {
    crate::player::metrics::render()
}

/// Returns a full snapshot of all sessions, broadcasters, and listener relationships.
/// Intended for the on-page admin panel.
pub async fn admin_state(State(state): State<SharedState>) -> impl IntoResponse {
    let now = std::time::Instant::now();

    let sessions = state
        .sessions
        .iter()
        .map(|entry| {
            let session_id = entry.key().clone();
            let tuned_to = state.session_tuned_to.get(&session_id).map(|v| v.clone());

            AdminSession {
                idle_secs: now.duration_since(entry.value().last_activity).as_secs(),
                session_id,
                tuned_to,
            }
        })
        .collect();

    let broadcaster_listeners = state
        .broadcaster_listeners
        .iter()
        .map(|e| (e.key().clone(), e.value().iter().cloned().collect()))
        .collect();

    axum::Json(AdminState {
        sessions,
        broadcaster_listeners,
    })
}

/// Accepts media uploads, enforces a 5 GB total folder cap and a 200 MB per-IP cap
/// (tracked in memory, resets on server restart), then writes the file and inserts
/// it into the live playlist in sorted order.
///
/// Video uploads (mkv, mov, avi, webm, mp4) are converted to H.264/AAC stereo MP4
/// via ffmpeg before being added to the playlist. The original file is deleted after
/// a successful conversion. Audio files are stored as-is.
pub async fn upload_file(
    State(state): State<SharedState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, PlayerError> {
    // Prefer X-Real-IP set by a reverse proxy so the quota tracks the real client
    let ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Check the total folder size before accepting any data
    let folder_size = dir_size(&state.music_folder).await?;
    if folder_size >= MAX_FOLDER_BYTES {
        return Err(PlayerError::UploadFailed(
            "Server library is full (5 GB limit reached)".into(),
        ));
    }

    // Read per-IP usage from the DashMap without holding the ref across an await
    let ip_used = state.upload_quotas.get(&ip).map(|v| *v).unwrap_or(0u64);

    if ip_used >= MAX_IP_QUOTA_BYTES {
        return Err(PlayerError::UploadFailed(format!(
            "Your upload quota is full ({} MB used out of 200 MB, resets on server restart)",
            ip_used / 1024 / 1024
        )));
    }

    let remaining_quota = MAX_IP_QUOTA_BYTES.saturating_sub(ip_used);
    let remaining_folder = MAX_FOLDER_BYTES.saturating_sub(folder_size);

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| PlayerError::UploadFailed(e.to_string()))?
    {
        let raw_name = field
            .file_name()
            .ok_or_else(|| PlayerError::UploadFailed("Missing filename in upload".into()))?
            .to_string();

        // Accept all supported audio and video formats
        let upload_media_type = media_type_for(&raw_name).ok_or_else(|| {
            PlayerError::UploadFailed(
                "Unsupported file type. Accepted: mp3, m4a, ogg, wav, flac, mp4, webm, mkv, mov, avi".into(),
            )
        })?;

        let safe_name = sanitize_filename(&raw_name);

        // Buffer the whole file so we can check size before writing to disk
        let data = field
            .bytes()
            .await
            .map_err(|e| PlayerError::UploadFailed(e.to_string()))?;

        let file_size = data.len() as u64;

        if file_size > remaining_quota {
            return Err(PlayerError::UploadFailed(format!(
                "'{}' is too large, you have {} MB of upload quota remaining",
                safe_name,
                remaining_quota / 1024 / 1024
            )));
        }

        if file_size > remaining_folder {
            return Err(PlayerError::UploadFailed(
                "Not enough space on the server for this file".into(),
            ));
        }

        let dest = state.music_folder.join(&safe_name);
        if dest.exists() {
            return Err(PlayerError::UploadFailed(format!(
                "'{}' already exists in the library",
                safe_name
            )));
        }

        tokio::fs::write(&dest, &data).await?;

        tracing::info!(
            ip = %ip,
            filename = %safe_name,
            bytes = file_size,
            "File uploaded"
        );

        // Quota is tracked against the upload size, not the converted size,
        // so users can't game the quota by uploading large files.
        *state.upload_quotas.entry(ip.clone()).or_insert(0) += file_size;

        // Video files are converted to browser-compatible H.264/AAC MP4.
        // Audio files are stored as is, browsers handle mp3/ogg/flac/wav natively.
        let (final_name, final_media_type, final_size) = match upload_media_type {
            MediaType::Video => {
                let stem = safe_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&safe_name);
                let mp4_name = format!("{}.mp4", stem);
                let mp4_dest = state.music_folder.join(&mp4_name);
                let vtt_dest = state.music_folder.join(format!("{}.vtt", stem));

                tracing::info!(
                    ip = %ip,
                    input = %safe_name,
                    output = %mp4_name,
                    "Starting ffmpeg conversion"
                );

                // Semaphore to make sure only one video can be converted to teh appropriate type at a time
                let _permit = state.conversion_semaphore.acquire().await.unwrap();

                // Re-encode video to H.264 so any source codec (VP9, HEVC, AV1) works in browsers.
                // -crf 23 targets constant quality; -maxrate 2M/-bufsize 4M caps bitrate spikes
                // that cause buffer underruns during streaming. -ac 2 avoids the AAC 5.1 PCE
                // issue (silent playback in Chrome/Firefox). -movflags +faststart moves the moov
                // atom to the front so playback starts before the full file is downloaded.
                // Subtitles are extracted as a separate .vtt file in a second output, MP4 cannot
                // embed WebVTT, so they must live alongside the video and are served via a
                // dedicated /player/subtitles/{id} endpoint. The subtitle extraction uses .ok()
                // so a missing subtitle stream does not fail the whole upload.
                let output = tokio::process::Command::new("ffmpeg")
                    .args([
                        "-i",  dest.to_str().unwrap(),
                        "-map", "0:v:0",
                        "-map", "0:a:0",
                        "-c:v", "libx264",
                        "-crf", "23",
                        "-maxrate", "2M",
                        "-bufsize", "4M",
                        "-c:a", "aac",
                        "-b:a", "192k",
                        "-ac",  "2",
                        "-movflags", "+faststart",
                        mp4_dest.to_str().unwrap(),
                        "-map", "0:s:0",
                        "-c:s", "webvtt",
                        vtt_dest.to_str().unwrap(),
                    ])
                    .output()
                    .await
                    .map_err(|e| PlayerError::UploadFailed(format!("ffmpeg not found: {}", e)))?;

                // Always remove the original regardless of outcome to avoid orphaned files
                tokio::fs::remove_file(&dest).await.ok();

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::error!(stderr = %stderr, "ffmpeg conversion failed");
                    return Err(PlayerError::UploadFailed(
                        "Video conversion failed. Is the file a valid video?".into(),
                    ));
                }

                // Use the actual converted file size for the playlist entry
                let converted_size = tokio::fs::metadata(&mp4_dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(file_size);

                tracing::info!(
                    ip = %ip,
                    filename = %mp4_name,
                    bytes = converted_size,
                    "ffmpeg conversion complete"
                );

                (mp4_name, MediaType::Video, converted_size)
            }
            MediaType::Audio => (safe_name.clone(), MediaType::Audio, file_size),
        };

        // Insert the new entry into the live playlist in alphabetical order.
        // Write lock is held only for the insert, then released immediately.
        let new_media = MediaInfo {
            id: Uuid::new_v4().to_string(),
            filename: final_name.clone(),
            size: final_size,
            media_type: final_media_type,
        };

        {
            let mut playlist = state.playlist.write().await;
            let pos = playlist.partition_point(|s| s.filename <= final_name);
            playlist.insert(pos, new_media);
            tracing::info!(
                filename = %final_name,
                position = pos + 1,
                "Inserted new entry into playlist"
            );
        }
    }

    Ok(StatusCode::OK)
}

/// Strips path separators and special characters from an upload filename.
/// Keeps letters, digits, spaces, dashes, underscores, and dots.
/// Takes only the last path component so paths like ../../etc/passwd
/// are reduced to just the filename portion before sanitizing.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);

    base.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | ' ' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Sums the sizes of all files directly inside a directory (non-recursive).
async fn dir_size(path: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut entries = tokio::fs::read_dir(path).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(meta) = entry.metadata().await {
            if meta.is_file() {
                total += meta.len();
            }
        }
    }
    Ok(total)
}