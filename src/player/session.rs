use std::{sync::Arc, time::Duration};

use axum::http::{HeaderMap, header};
use uuid::Uuid;

use crate::player::{RadioMessage, radio::broadcast_analytics};

use super::types::{AppState, PlayerSession, PreparedMessage};

/// Extracts the session id from the request header
pub fn get_session_id(headers: &HeaderMap, state: &AppState) -> String {
    // Try to extract session ID from cookie
    let extracted_id = headers
        .get(header::COOKIE)
        .and_then(|cookie| cookie.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let cookie = cookie.trim();
                cookie.strip_prefix("player_session=")
            })
        })
        .map(|s| s.to_string());
    
    // Check if extracted ID is still valid (exists in sessions map)
    if let Some(id) = extracted_id {
        if state.sessions.contains_key(&id) {
            return id; // Valid session, no new cookie needed
        }
        // Session was cleaned up, fall through to generate new
    }
    
    // No valid session found, generate fresh UUID
    let new_id = Uuid::new_v4().to_string();
    new_id // true = send Set-Cookie header
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

/// Periodically cleans up stale player sessions and broadcaster channels.
pub async fn cleanup_stale_sessions(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        // Wait for the next tick (initially fires immediately, then every 60s)
        interval.tick().await;

        let now = std::time::Instant::now();
        let now_ms = now_ms();

        // Cleanup Player Sessions
        // These are private playback sessions for users not in radio mode
        {
            let mut removed = 0;

            state.sessions.retain(|session_id, session| {
                let age = now.duration_since(session.last_activity);
                let should_keep = age.as_secs() < 3600;

                if !should_keep {
                    removed += 1;
                    tracing::info!(
                        "Cleaning up stale player session: {} (age: {}s)",
                        session_id,
                        age.as_secs()
                    );
                }

                should_keep
            });

            if removed > 0 {
                tracing::info!("Cleaned up {} stale player session(s)", removed);
            }
        }

        // Cleanup Broadcaster Sessions
        // These are active radio broadcasts that should be recent
        let mut stale_broadcasters = Vec::new();

        {
            // Find broadcasters that haven't updated in 30+ seconds
            for entry in state.broadcast_states.iter() {
                let age_ms = now_ms.saturating_sub(entry.value().server_timestamp_ms);
                let age_secs = age_ms / 1000;

                // 30 seconds is chosen because:
                // - Heartbeats come every 2-3 seconds normally
                // - 30 seconds allows for network hiccups and reconnects
                // - But catches truly disconnected broadcasters quickly
                if age_secs > 30 {
                    tracing::info!(
                        "Found stale broadcaster: {} (age: {}s)",
                        entry.key(),
                        age_secs
                    );
                    stale_broadcasters.push(entry.key().clone());
                }
            }
        }

        // Now remove the stale broadcasters and notify listeners
        for broadcaster_id in stale_broadcasters {
            if state.broadcast_states.remove(&broadcaster_id).is_some() {
                state.broadcaster_listeners.remove(&broadcaster_id);
                tracing::info!("Removed stale broadcaster state: {}", broadcaster_id);
            }

            // Notify all clients via the global channel consistent with delete_broadcasting_session
            // and ensures the broadcaster disappears from analytics for everyone, not just tuned listeners
            let offline_msg = Arc::new(PreparedMessage::new(&RadioMessage::BroadcasterOffline {
                broadcaster_id: broadcaster_id.clone(),
            }));

            match state.global_broadcast_tx.send(offline_msg) {
                Ok(listener_count) => {
                    tracing::info!(
                        "Notified {} listener(s) that broadcaster {} went offline",
                        listener_count,
                        broadcaster_id
                    );
                }
                Err(_) => {
                    tracing::debug!(
                        "No listeners to notify for offline broadcaster: {}",
                        broadcaster_id
                    );
                }
            }

            // Remove the broadcast channel itself
            if state.broadcast_channels.remove(&broadcaster_id).is_some() {
                tracing::info!("Removed broadcast channel: {}", broadcaster_id);
            }
        }

        broadcast_analytics(&state);
    }
}

/// Removes a session from all broadcaster listener sets.
/// This handles abrupt disconnects where TuneOut was never sent.
pub fn remove_session_from_all_listeners(state: &Arc<AppState>, session_id: &str) {
    let mut affected_broadcasters = Vec::new();

    // Remove session from every listener set
    for mut entry in state.broadcaster_listeners.iter_mut() {
        if entry.value_mut().remove(session_id) {
            affected_broadcasters.push((entry.key().clone(), entry.value().len()));
        }
    }

    // Update cached listener_count if you keep it
    for (broadcaster_id, new_count) in affected_broadcasters {
        if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
            broadcast.listener_count = new_count;
        }
    }
}