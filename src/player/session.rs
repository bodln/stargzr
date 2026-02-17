use axum::http::{HeaderMap, header};
use uuid::Uuid;

use super::types::{AppState, PlayerSession};

/// Extracts the session id from the request header
pub fn get_session_id(headers: &HeaderMap) -> String {
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
pub fn get_or_create_position(state: &AppState, session_id: &str) -> usize {
    let now = std::time::Instant::now();

    // First try with cheap read lock
    if let Some(session) = state.sessions.get(session_id) {
        return session.current_index;
    }

    let mut session = state.sessions
        .entry(session_id.to_string())
        .or_insert(PlayerSession {
            current_index: 0,
            last_activity: now,
        });

    // Either returns a valid session or creates a new one,
    // in the case of there not being one or it having expired
    session.last_activity = now;
    session.current_index
}

/// Changes the playlist position for the session id
pub fn update_session_index(state: &AppState, session_id: &str, new_index: usize) {
    if let Some(mut session) = state.sessions.get_mut(session_id) {
        session.current_index = new_index;
        session.last_activity = std::time::Instant::now();
    }
}

/// Changes the playlist position for the broadcaster by session id
pub fn update_broadcast_index(state: &AppState, session_id: &str, new_index: usize) {
    // Check if this session is broadcasting ( with a cheap read lock)
    if !state.broadcast_states.contains_key(session_id) {
        // Not a broadcaster, skip entirely
        return;
    }

    let server_ts = now_ms();

    // Only acquire write lock if we know they're broadcasting
    if let Some(mut broadcast) = state.broadcast_states.get_mut(session_id) {
        // Update authoritative state
        broadcast.song_index = new_index;
        broadcast.playback_time = 0.0;
        broadcast.server_timestamp_ms = server_ts;
    }
}

/// Get current server time in milliseconds (for timestamping)
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}