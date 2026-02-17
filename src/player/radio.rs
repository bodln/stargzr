use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};

use super::error::{PlayerError, PlayerResult};
use super::rate_limit::RateLimiter;
use super::validation::{SessionId, validate_song_index};

use super::session::{get_session_id, now_ms};
use super::types::{AppState, BroadcastState, RadioMessage, SharedState};

// Upgrade with WebRTC? to have p2p

/// WebSocket entrypoint for the radio synchronization system.
/// No state is modified here; it exists purely as a thin Axum integration
/// layer for the radio protocol.
pub async fn radio_websocket(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session_id_str = get_session_id(&headers);

    // This is the session id first given to the user
    // It is used so if someone tampers with their original session their calls are moot
    let validated_session_id = match SessionId::new(session_id_str) {
        Ok(id) => id,
        Err(e) => {
            // If the session ID is invalid, reject the WebSocket upgrade
            tracing::warn!(
                "Rejected WebSocket connection with invalid session ID: {}",
                e
            );
            return (StatusCode::BAD_REQUEST, "Invalid session ID").into_response();
        }
    };

    ws.on_upgrade(|socket| {
        handle_radio_connection(socket, state, validated_session_id.into_inner())
    })
}

/// Manages the full lifecycle of a radio WebSocket connection.
async fn handle_radio_connection(
    socket: WebSocket,
    state: SharedState,
    validated_session_id: String,
) {
    let (mut sender, mut receiver) = socket.split();

    // Private channel for responses from the receive task to the send task (same WebSocket)
    // For when you receive soemthing and want to return it to yourself and only yourself
    let (out_tx, mut out_rx) = mpsc::channel::<RadioMessage>(32);

    // Channel used to communicate tuned broadcaster changes
    let (tuned_tx, mut tuned_rx) = tokio::sync::watch::channel::<Option<String>>(None);

    let mut global_broadcast_rx = state.global_broadcast_tx.subscribe();

    let heartbeat_limiter = Arc::new(RateLimiter::for_heartbeat());
    let broadcast_limiter = Arc::new(RateLimiter::for_broadcast());

    state.active_connections.fetch_add(1, Relaxed);
    broadcast_analytics(&state);

    tracing::info!(
        "Client connected: {}",
        &validated_session_id
    );

    // Send task
    let state_clone = state.clone();

    let mut send_task = tokio::spawn(async move {
        let mut current_tuned_broadcaster_rx: Option<broadcast::Receiver<RadioMessage>> = None;

        loop {
            // Polls all channels simultaneously
            tokio::select! {
                // Global channel
                Ok(msg) = global_broadcast_rx.recv() => {
                    let should_forward = should_forward_message(&msg).await;

                    if should_forward {
                        if let Err(e) = send_message(&mut sender, &msg).await {
                            tracing::error!("Failed to forward broadcast: {}", e);
                            break;
                        }
                    }
                }

                // Change the channel we are listening on in the case of change
                Ok(()) = tuned_rx.changed() => {
                    match tuned_rx.borrow().clone() {
                        Some(broadcast_id) => {
                            let tx = state_clone.broadcast_channels
                                .get(&broadcast_id)
                                .map(|r| r.clone());

                            current_tuned_broadcaster_rx = tx.map(|t| t.subscribe());
                            tracing::debug!("Tuned in to {:?}", broadcast_id);
                        }
                        None => {
                            current_tuned_broadcaster_rx = None;
                            tracing::debug!("Tuned out");
                        }
                    }
                }

                // Messages from the receive task
                Some(msg) = out_rx.recv() => {
                    if let Err(e) = send_message(&mut sender, &msg).await {
                        tracing::error!("Failed to send message: {}", e);
                        break;
                    }
                }

                // Broadcast messages from other connections to the appropriate channel
                Ok(msg) = async {
                    match &mut current_tuned_broadcaster_rx {
                        Some(rx) => rx.recv().await,
                        // If current_tuned_broadcaster_rx is None, return a pending future
                        // so this branch never becomes ready and never wins select!
                        None => std::future::pending().await,
                    }
                } => {
                    // This fires when current_tuned_broadcaster_rx is some and receives something
                    if should_forward_message(&msg).await {
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
    let validated_session_id_clone = validated_session_id.clone();

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
                                &validated_session_id_clone,
                                &out_tx,
                                &tuned_tx,
                                &heartbeat_limiter,
                                &broadcast_limiter,
                            )
                            .await
                            {
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

    delete_broadcasting_session(&state, &validated_session_id);

    if state.active_connections.load(Relaxed) > 0 {
        state.active_connections.fetch_sub(1, Relaxed);
    }
    broadcast_analytics(&state);

    tracing::info!(
        "Client disconnected: {}",
        &validated_session_id,
    );
}

/// Serializes a `RadioMessage` and sends it to the client over the WebSocket.
async fn send_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &RadioMessage,
) -> PlayerResult<()> {
    let json = serde_json::to_string(msg)?;
    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| PlayerError::WebSocketError(e.to_string()))
}

/// Validates that the message is a broadcastable type.
/// All messages that arrive here have already been filtered by subscription.
async fn should_forward_message(msg: &RadioMessage) -> bool {
    matches!(
        msg,
        RadioMessage::BroadcasterOnline { .. }
            | RadioMessage::BroadcasterOffline { .. }
            | RadioMessage::Sync { .. }
            | RadioMessage::Heartbeat { .. }
            | RadioMessage::Analytics { .. }
            | RadioMessage::BroadcastStateResponse { .. }
    )
}

/// Handles an incoming `RadioMessage` from a client WebSocket.
async fn handle_client_message(
    msg: RadioMessage,
    state: &SharedState,
    validated_session_id: &str,
    // communication between receive and send tasks
    out_tx: &mpsc::Sender<RadioMessage>,
    // communication on wether the client changed who they are listening to
    tuned_tx: &tokio::sync::watch::Sender<Option<String>>,
    heartbeat_limiter: &Arc<RateLimiter>,
    broadcast_limiter: &Arc<RateLimiter>,
) -> PlayerResult<()> {
    match msg {
        RadioMessage::TuneIn { broadcaster_id } => {
            // Validate the incoming broadcaster ID as a proper SessionId (UUID)
            let session_id = SessionId::new(broadcaster_id.clone())?;

            tracing::info!("Client tuning into: {}", session_id);

            state.broadcaster_listeners
                .entry(broadcaster_id.clone())
                .or_insert_with(HashSet::new)
                .insert(validated_session_id.to_string());

            if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
                let count = state.broadcaster_listeners
                    .get(&broadcaster_id)
                    .map(|set| set.len())
                    .unwrap_or(0);
                broadcast.listener_count = count;
            }

            // Retrieve current broadcast state if it exists
            let maybe_state = state.broadcast_states.get(&broadcaster_id).map(|b| b.clone());

            if let Some(b_state) = maybe_state {
                // Send a Sync message with the current state to the newly tuned client
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

                tuned_tx.send(Some(broadcaster_id.clone())).map_err(|_| {
                    PlayerError::WebSocketError("Failed to send tune change".into())
                })?;

                out_tx
                    .send(sync_msg)
                    .await
                    .map_err(|_| PlayerError::WebSocketError("Failed to send sync".into()))?;

                broadcast_analytics(state);
            } else {
                // No broadcaster found with this ID
                return Err(PlayerError::BroadcasterNotFound(broadcaster_id));
            }
        }

        RadioMessage::TuneOut => {
            tracing::info!("Client tuned out");

            // Remove from broadcaster's listener set and update count
            let maybe_broadcaster_id = {
                let mut found_broadcaster = None;

                for mut entry in state.broadcaster_listeners.iter_mut() {
                    if entry.value_mut().remove(validated_session_id) {
                        found_broadcaster = Some((entry.key().clone(), entry.value().len()));
                        break;
                    }
                }
                found_broadcaster
            };

            // Update the broadcast state with the new count
            if let Some((broadcaster_id, count)) = maybe_broadcaster_id {
                if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
                    broadcast.listener_count = count;
                }
            }

            tuned_tx
                .send(None)
                .map_err(|_| PlayerError::WebSocketError("Failed to tune out".into()))?;

            broadcast_analytics(state);
        }

        RadioMessage::BroadcastUpdate {
            broadcaster_id,
            song_index,
            playback_time,
            is_playing,
        } => {
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Enforce broadcast update rate limits (prevents spam)
            broadcast_limiter.check_and_consume(&broadcaster_id).await?;

            // Ensure the requested song index exists in the playlist
            validate_song_index(song_index, state.playlist.len())?;

            tracing::debug!(
                "Broadcast update from {}: song={}, time={:.2}, playing={}",
                &broadcaster_id,
                song_index,
                playback_time,
                is_playing
            );

            // If the session isn't broadcasting don't update
            if !state.broadcast_states.contains_key(&broadcaster_id) {
                return Err(PlayerError::BroadcasterNotFound(broadcaster_id));
            }

            let listener_count = state
                .broadcaster_listeners
                .get(&broadcaster_id)
                .map(|set| set.len())
                .unwrap_or(0);

            let server_ts = now_ms();

            let song_name = state
                .playlist
                .get(song_index)
                .map(|song| song.filename.clone())
                .unwrap_or_else(|| format!("Unknown song #{}", song_index));

            // Update the server-side broadcast state
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                song_name,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
                listener_count: listener_count,
            };

            state.broadcast_states.insert(broadcaster_id.clone(), new_state);

            // Forward the update to all subscribed clients via Sync message
            let sync_msg = RadioMessage::Sync {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
            };

            // A return value of Err does not mean that future calls to send will fail
            if let Some(tx) = state.broadcast_channels.get(&broadcaster_id) {
                match tx.send(sync_msg) {
                    Ok(count) => {
                        tracing::trace!("Sync sent to {} listeners", count);
                    }
                    Err(_) => {
                        tracing::trace!("Sync sent but no active listeners");
                    }
                }
            }

            broadcast_analytics(state);
        }

        RadioMessage::Heartbeat {
            broadcaster_id,
            playback_time,
        } => {
            // Validate broadcaster session ID
            let session_id = SessionId::new(broadcaster_id.clone())?;

            // Enforce heartbeat rate limits
            heartbeat_limiter
                .check_and_consume(session_id.as_str())
                .await?;

            let server_ts = now_ms();

            // Update playback time for the broadcaster
            if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
                broadcast.playback_time = playback_time;
                broadcast.server_timestamp_ms = server_ts;

                tracing::trace!("Heartbeat from {}: time={:.2}", session_id, playback_time);
            }
        }

        RadioMessage::StopBroadcasting { broadcaster_id } => {
            // Validate broadcaster session ID
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            state.broadcast_states.remove(&broadcaster_id);
            state.broadcast_channels.remove(&broadcaster_id);

            let offline_msg = RadioMessage::BroadcasterOffline {
                broadcaster_id: broadcaster_id.clone(),
            };

            // A return value of Err does not mean that future calls to send will fail
            match state.global_broadcast_tx.send(offline_msg) {
                Ok(count) => {
                    tracing::debug!("BroadcasterOffline sent to {} clients", count);
                }
                Err(_) => {
                    tracing::debug!("BroadcasterOffline sent but no clients connected");
                }
            }

            broadcast_analytics(state);
        }

        RadioMessage::StartBroadcasting {
            broadcaster_id,
            song_index,
            playback_time,
            is_playing,
        } => {
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Enforce broadcast update rate limits (prevents spam)
            broadcast_limiter.check_and_consume(&broadcaster_id).await?;

            // Ensure the requested song index exists in the playlist
            validate_song_index(song_index, state.playlist.len())?;

            let was_already_broadcasting = state.broadcast_states.contains_key(&broadcaster_id);

            if was_already_broadcasting {
                tracing::debug!(
                    "Broadcaster {} already registered - updating state only (NOT incrementing counter)",
                    broadcaster_id
                );
            } else {
                tracing::debug!(
                    "New broadcaster {} starting (incrementing counter)",
                    broadcaster_id
                );
            }

            tracing::debug!(
                "Starting broadcast from {}: song={}, time={:.2}, playing={}",
                &broadcaster_id,
                song_index,
                playback_time,
                is_playing
            );

            let server_ts = now_ms();

            let song_name = state
                .playlist
                .get(song_index)
                .map(|song| song.filename.clone())
                .unwrap_or_else(|| format!("Unknown song #{}", song_index));

            // Update the server-side broadcast state
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                song_index,
                song_name,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
                listener_count: 0,
            };

            state.broadcast_states.insert(broadcaster_id.clone(), new_state);

            // Create the channel if it doesn't exist
            state.broadcast_channels.entry(broadcaster_id.clone()).or_insert_with(|| {
                let (tx, _rx) = broadcast::channel::<RadioMessage>(100);
                tracing::debug!("Created broadcast channel for session: {}", broadcaster_id);
                tx
            });

            // Send announcement through global channel, not the broadcaster's channel
            if !was_already_broadcasting {
                let broadcasting_msg = RadioMessage::BroadcasterOnline {
                    broadcaster_id: broadcaster_id.clone(),
                };

                state
                    .global_broadcast_tx
                    .send(broadcasting_msg)
                    .map_err(|_| PlayerError::BroadcastSendError)?;

                tracing::info!("Broadcaster {} came online", broadcaster_id);
            }

            broadcast_analytics(state);
        }

        RadioMessage::QueryBroadcastState { session_id } => {
            ensure_same_session(&session_id, validated_session_id)?;

            // Check if this session is currently broadcasting
            let is_broadcasting = state.broadcast_states.contains_key(&session_id);
            let current_state = state.broadcast_states.get(&session_id).map(|b| b.clone());

            tracing::debug!(
                "Query broadcast state for {}: {}",
                session_id,
                is_broadcasting
            );

            let response = RadioMessage::BroadcastStateResponse {
                session_id: session_id.clone(),
                is_broadcasting,
                current_state,
            };

            out_tx
                .send(response)
                .await
                .map_err(|_| PlayerError::WebSocketError("Failed to send state response".into()))?;
        }

        _ => {
            // Any unexpected messages are logged but ignored
            tracing::warn!("Received unexpected message type");
        }
    }

    Ok(())
}

/// Verifies that the provided identifier matches the session identifier
/// recorded when the WebSocket connection was first established.
/// This is used to ensure that a client cannot act or broadcast as another
/// session by supplying a different ID after the connection is open.
fn ensure_same_session(expected: &str, actual: &str) -> Result<(), PlayerError> {
    (expected == actual).then_some(()).ok_or_else(|| {
        tracing::warn!(
            "Session {} attempted to act as {}",
            actual,
            expected
        );
        PlayerError::BroadcastUnauthorized(
            "Trying to use session id that differs from the one established upon websocket handshake.".into())
    })
}

pub fn delete_broadcasting_session(state: &SharedState, broadcaster_id: &str) {
    if !state.broadcast_states.contains_key(broadcaster_id) {
        return;
    }

    tracing::info!(
        "Auto-cleanup: Disconnected broadcaster {} - removing state",
        broadcaster_id
    );

    state.broadcast_states.remove(broadcaster_id);

    let offline_msg = RadioMessage::BroadcasterOffline {
        broadcaster_id: broadcaster_id.to_string(),
    };

    match state.global_broadcast_tx.send(offline_msg) {
        Ok(listener_count) => {
            tracing::info!(
                "Notified {} listener(s) that {} went offline",
                listener_count,
                broadcaster_id
            );
        }
        Err(_) => {
            tracing::debug!("No listeners to notify for {}", broadcaster_id);
        }
    }

    if state.broadcast_channels.remove(broadcaster_id).is_some() {
        tracing::debug!("Removed broadcast channel for {}", broadcaster_id);
    }

    broadcast_analytics(&state);
}

/// Creates a RadioMessage::Error from a PlayerError
/// This is used to send error messages back to the client over WebSocket.
fn create_error_message(error: &PlayerError) -> RadioMessage {
    RadioMessage::Error {
        message: error.to_string(),
    }
}

// TODO this current analytics of sending broadcast state is too expensive

/// Broadcasts current analytics to all connected clients via the global channel.
/// Call this whenever any counter changes to keep all clients synchronized.
pub fn broadcast_analytics(state: &SharedState) {
    // Compute active_listeners as the total number of strings across all
    // per-broadcaster listener sets — single read lock, no atomic to drift
    let active_listeners = state.broadcaster_listeners
        .iter()
        .map(|entry| entry.value().len())
        .sum::<usize>();

    let active_connections = state.active_connections.load(Relaxed);

    let active_broadcasters = state.broadcast_channels.len();

    // Collect broadcaster states, injecting fresh listener counts so the UI
    // is always accurate regardless of whether TuneIn/TuneOut updated it
    let broadcasters: Vec<BroadcastState> = state.broadcast_states.iter().map(|entry| {
        let mut b = entry.value().clone();
        b.listener_count = state.broadcaster_listeners
            .get(&b.broadcaster_id)
            .map(|s| s.len())
            .unwrap_or(0);
        b
    }).collect();

    let analytics_msg = RadioMessage::Analytics {
        active_connections,
        active_broadcasters,
        active_listeners,
        broadcasters,
    };

    match state.global_broadcast_tx.send(analytics_msg) {
        Ok(count) => tracing::trace!("Analytics update sent to {} clients", count),
        Err(_) => tracing::trace!("Analytics update sent but no clients connected"),
    }
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
            let before_count = state.sessions.len();

            // Remove sessions older than 1 hour
            state.sessions.retain(|session_id, session| {
                let age = now.duration_since(session.last_activity);
                let should_keep = age.as_secs() < 3600;

                if !should_keep {
                    tracing::info!(
                        "Cleaning up stale player session: {} (age: {}s)",
                        session_id,
                        age.as_secs()
                    );
                }

                should_keep
            });

            let removed = before_count - state.sessions.len();
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
                tracing::info!("Removed stale broadcaster state: {}", broadcaster_id);
            }

            // Send offline notification to any remaining listeners
            // It's okay if there are no listeners - we handle that gracefully
            let maybe_tx = state.broadcast_channels.get(&broadcaster_id).map(|r| r.clone());
            if let Some(tx) = maybe_tx {
                let offline_msg = RadioMessage::BroadcasterOffline {
                    broadcaster_id: broadcaster_id.clone(),
                };

                match tx.send(offline_msg) {
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
            }

            // Remove the broadcast channel itself
            if state.broadcast_channels.remove(&broadcaster_id).is_some() {
                tracing::info!("Removed broadcast channel: {}", broadcaster_id);
            }
        }

        broadcast_analytics(&state);
    }
}