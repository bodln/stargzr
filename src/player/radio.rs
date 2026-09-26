use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tracing::Instrument;

use crate::player::session::remove_session_from_all_listeners;

use super::error::{PlayerError, PlayerResult};
use super::rate_limit::RateLimiter;
use super::validation::{SessionId, validate_media_index};

use super::session::now_ms;
use super::types::{BroadcastQueueInfo, BroadcastState, PreparedMessage, RadioMessage, SharedState};

// Analytics are expensive, don't send more often than this on high-frequency paths
const ANALYTICS_THROTTLE_MS: u64 = 500;

// Cap on the measured one way broadcaster to server latency.
// A real network leg is never longer than this, so a bigger sample means
// something went wrong (a stalled socket, a delayed Pong) and we clamp it
// instead of adding a huge number onto everyone's playback position.
const MAX_PLAUSIBLE_LATENCY_MS: u64 = 2000;

// How often the server pings a broadcaster to measure round trip time.
const BROADCASTER_PING_INTERVAL_SECS: u64 = 4;

// Longest chat line we keep. Anything past this gets snipped at a char boundary
// so nobody can shove a wall of text into everyone else's chat.
const MAX_CHAT_LEN: usize = 500;

/// Longest broadcast queue kept, and the longest name it can carry.
const MAX_QUEUE_LEN: usize = 5000;
const MAX_QUEUE_NAME_LEN: usize = 80;

/// Manages the full lifecycle of a radio WebSocket connection.
pub async fn handle_radio_connection(
    socket: WebSocket,
    state: SharedState,
    validated_session_id: String,
    ip: String,
) {
    let (mut sender, mut receiver) = socket.split();

    // Private channel for responses from the receive task to the send task (same WebSocket)
    // For when you receive something and want to return it to yourself and only yourself
    let (out_tx, mut out_rx) = mpsc::channel::<RadioMessage>(32);

    // Channel used to communicate tuned broadcaster changes
    let (tuned_tx, mut tuned_rx) = tokio::sync::watch::channel::<Option<String>>(None);

    // Flips to true while this session is broadcasting. It tells the send task
    // to also listen on our own room channel, so chat from our listeners reaches
    // us and not just each other. A broadcaster is never tuned into themselves,
    // so without this they would never see their own room's chat.
    let (own_room_tx, mut own_room_flag_rx) = tokio::sync::watch::channel::<bool>(false);

    let mut global_broadcast_rx = state.global_broadcast_tx.subscribe();

    let heartbeat_limiter = Arc::new(RateLimiter::for_heartbeat());
    let broadcast_limiter = Arc::new(RateLimiter::for_broadcast());
    let chat_limiter = Arc::new(RateLimiter::for_chat());
    let room_relay_limiter = Arc::new(RateLimiter::for_room_relay());

    state.active_connections.fetch_add(1, Relaxed);

    // Presence, and a way to reach this socket by name later. `live_sessions`
    // is what turns "this session logged in at some point" into "this account
    // is online right now"; `session_outbox` lets a direct message arriving
    // over HTTP find its recipient's sockets, which the room channels can't do
    // since they're keyed by room rather than by who is in them. Both are
    // undone at the bottom of this function, and only as far as they are
    // still ours: a newer socket on the same session may be open by then.
    *state
        .live_sessions
        .entry(validated_session_id.clone())
        .or_insert(0) += 1;
    let my_outbox = out_tx.clone();
    state
        .session_outbox
        .insert(validated_session_id.clone(), out_tx.clone());

    broadcast_analytics(&state);

    tracing::info!("Client connected: {}", &validated_session_id);

    // Root span for this session, propagated into both spawned tasks so all logs
    // carry session_id without having to thread it through every function call
    let session_span = tracing::info_span!(
        "ws_session",
        session_id = %validated_session_id,
        ip = %ip,
    );

    // Send task
    let state_clone = state.clone();
    let send_session_id = validated_session_id.clone();

    let mut send_task = tokio::spawn(
        async move {
            let mut current_tuned_broadcaster_rx: Option<
                broadcast::Receiver<Arc<PreparedMessage>>,
            > = None;

            // Our own room channel, only wired up while we are broadcasting.
            // It carries everything our listeners get, but the only thing we
            // actually want off it is Chat. The client already drops the Sync
            // and AutoNext copies that carry our own id.
            let mut own_room_rx: Option<broadcast::Receiver<Arc<PreparedMessage>>> = None;

            // Latency probe for this session. Fires a Ping once the session is
            // either broadcasting or tuned in as a listener — either way the
            // result is a clock-safe one-way latency figure the recipient can
            // anchor Perfect Sync's position math on. The first tick lands
            // immediately and is skipped.
            let mut ping_interval = tokio::time::interval(
                std::time::Duration::from_secs(BROADCASTER_PING_INTERVAL_SECS),
            );

            loop {
                // biased: skips random branch selection, tuned broadcaster is the hottest path
                // and gets priority when multiple branches are ready simultaneously
                tokio::select! {
                    biased;

                    // Broadcast messages from the tuned broadcaster, hottest path, checked first
                    Ok(msg) = async {
                        match &mut current_tuned_broadcaster_rx {
                            Some(rx) => rx.recv().await,
                            // If current_tuned_broadcaster_rx is None, return a pending future
                            // so this branch never becomes ready and never wins select!
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Err(e) = send_prepared(&mut sender, &msg).await {
                            tracing::error!("Failed to forward broadcast: {}", e);
                            break;
                        }
                    }

                    // Our own room channel, live only while we are broadcasting.
                    // This is how a broadcaster hears the chat from the people
                    // tuned into them. Listeners get the same lines through the
                    // tuned branch above.
                    Ok(msg) = async {
                        match &mut own_room_rx {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Err(e) = send_prepared(&mut sender, &msg).await {
                            tracing::error!("Failed to forward own room message: {}", e);
                            break;
                        }
                    }

                    // Global channel
                    Ok(msg) = global_broadcast_rx.recv() => {
                        if let Err(e) = send_prepared(&mut sender, &msg).await {
                            tracing::error!("Failed to forward broadcast: {}", e);
                            break;
                        }
                    }

                    // Messages from the receive task
                    Some(msg) = out_rx.recv() => {
                        if let Err(e) = send_message(&mut sender, &msg).await {
                            tracing::error!("Failed to send message: {}", e);
                            break;
                        }
                    }

                    // Ping the broadcaster or listener so their Pong lets us time
                    // the round trip. Skipped for a session that's neither.
                    _ = ping_interval.tick() => {
                        if state_clone.broadcast_states.contains_key(&send_session_id)
                            || state_clone.session_tuned_to.contains_key(&send_session_id)
                        {
                            let ping = RadioMessage::Ping { server_ts: now_ms() as u64 };
                            if let Err(e) = send_message(&mut sender, &ping).await {
                                tracing::error!("Failed to send ping: {}", e);
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
                                tracing::debug!(broadcaster_id = %broadcast_id, "Tuned in");
                            }
                            None => {
                                current_tuned_broadcaster_rx = None;
                                tracing::debug!("Tuned out");
                            }
                        }
                    }

                    // Broadcasting started or stopped, wire our own room channel
                    // in or drop it so we track our listeners' chat while live.
                    Ok(()) = own_room_flag_rx.changed() => {
                        if *own_room_flag_rx.borrow() {
                            own_room_rx = state_clone.broadcast_channels
                                .get(&send_session_id)
                                .map(|tx| tx.subscribe());
                            tracing::debug!("Listening on own room channel for chat");
                        } else {
                            own_room_rx = None;
                            tracing::debug!("Dropped own room channel");
                        }
                    }

                    else => {
                        tracing::debug!("Send task channel closed");
                        break;
                    }
                }
            }
        }
        .instrument(session_span.clone()),
    );

    // Receive task
    let state_clone = state.clone();
    let validated_session_id_clone = validated_session_id.clone();

    let mut receive_task = tokio::spawn(
        async move {
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
                                    &own_room_tx,
                                    &heartbeat_limiter,
                                    &broadcast_limiter,
                                    &chat_limiter,
                                    &room_relay_limiter,
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
        }
        .instrument(session_span),
    );

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

    remove_session_from_all_listeners(&state, &validated_session_id);

    delete_broadcasting_session(&state, &validated_session_id);

    state.session_latency_ms.remove(&validated_session_id);
    if let Some(mut open) = state.live_sessions.get_mut(&validated_session_id) {
        *open = open.saturating_sub(1);
    }
    state
        .live_sessions
        .remove_if(&validated_session_id, |_, open| *open == 0);
    state
        .session_outbox
        .remove_if(&validated_session_id, |_, tx| tx.same_channel(&my_outbox));

    if state.active_connections.load(Relaxed) > 0 {
        state.active_connections.fetch_sub(1, Relaxed);
    }

    broadcast_analytics(&state);

    tracing::info!("Client disconnected: {}", &validated_session_id,);
}

/// Serializes a `RadioMessage` and sends it to the client over the WebSocket.
/// Used for self-directed messages (out_rx path) that are never shared across listeners.
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

/// Sends a pre-serialized message to the client.
/// Used for broadcast paths where the same message is shared across many listeners.
async fn send_prepared(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    prepared: &PreparedMessage,
) -> PlayerResult<()> {
    sender
        .send(Message::Text(prepared.json.clone().into()))
        .await
        .map_err(|e| PlayerError::WebSocketError(e.to_string()))
}

/// Handles an incoming `RadioMessage` from a client WebSocket.
async fn handle_client_message(
    msg: RadioMessage,
    state: &SharedState,
    validated_session_id: &str,
    // communication between receive and send tasks
    out_tx: &mpsc::Sender<RadioMessage>,
    // communication on whether the client changed who they are listening to
    tuned_tx: &tokio::sync::watch::Sender<Option<String>>,
    // flips true/false as this session starts and stops broadcasting, so the
    // send task knows whether to also listen on our own room channel
    own_room_tx: &tokio::sync::watch::Sender<bool>,
    heartbeat_limiter: &Arc<RateLimiter>,
    broadcast_limiter: &Arc<RateLimiter>,
    chat_limiter: &Arc<RateLimiter>,
    room_relay_limiter: &Arc<RateLimiter>,
) -> PlayerResult<()> {
    match msg {
        RadioMessage::TuneIn { broadcaster_id } => {
            crate::player::metrics::inc_messages("TuneIn");

            // Validate the incoming broadcaster ID as a proper SessionId (UUID)
            let session_id = SessionId::new(broadcaster_id.clone())?;

            tracing::info!(broadcaster_id = %session_id, "Client tuning in");

            // Retrieve current broadcast state if it exists, return early if not found
            // before touching any listener sets so we don't leave orphaned entries on failure
            let maybe_state = state
                .broadcast_states
                .get(&broadcaster_id)
                .map(|b| b.clone());

            let b_state = match maybe_state {
                Some(s) => s,
                None => return Err(PlayerError::BroadcasterNotFound(broadcaster_id)),
            };

            // If already tuned to someone, remove from their listener set first
            if let Some((_, old_broadcaster)) = state.session_tuned_to.remove(validated_session_id)
            {
                if let Some(mut listeners) = state.broadcaster_listeners.get_mut(&old_broadcaster) {
                    listeners.remove(validated_session_id);
                }
            }

            state
                .broadcaster_listeners
                .entry(broadcaster_id.clone())
                .or_insert_with(HashSet::new)
                .insert(validated_session_id.to_string());

            // Track reverse mapping for O(1) TuneOut
            state
                .session_tuned_to
                .insert(validated_session_id.to_string(), broadcaster_id.clone());

            if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
                let count = state
                    .broadcaster_listeners
                    .get(&broadcaster_id)
                    .map(|set| set.len())
                    .unwrap_or(0);
                broadcast.listener_count = count;
            }

            // Estimate the broadcaster's real position at this moment.
            // Start from the last playback_time they reported, add the time that has
            // passed since (only if playing), then add the broadcaster to server leg
            // measured by Ping/Pong. The frontend adds the server to listener leg on top.
            let time_since_last_update_secs = if b_state.is_playing {
                (now_ms().saturating_sub(b_state.server_timestamp_ms)) as f64 / 1000.0
            } else {
                0.0
            };
            let adjusted_playback_time = b_state.playback_time
                + time_since_last_update_secs
                + (b_state.transmission_latency_ms as f64 / 1000.0);

            // Send a Sync message with the current state to the newly tuned client
            tracing::debug!(
                broadcaster_id = %broadcaster_id,
                media_index = b_state.media_index,
                playback_time = adjusted_playback_time,
                "Sending initial sync"
            );

            let sync_msg = RadioMessage::Sync {
                broadcaster_id: broadcaster_id.clone(),
                media_index: b_state.media_index,
                playback_time: adjusted_playback_time,
                is_playing: b_state.is_playing,
                server_timestamp_ms: now_ms(),
                broadcaster_out_latency_ms: b_state.broadcaster_out_latency_ms,
            };

            tuned_tx.send(Some(broadcaster_id.clone())).map_err(|_| {
                PlayerError::WebSocketError("Failed to send tune change".into())
            })?;

            out_tx
                .send(sync_msg)
                .await
                .map_err(|_| PlayerError::WebSocketError("Failed to send sync".into()))?;

            // Kick off this listener's own one-way latency measurement right
            // away instead of waiting for the regular ping_interval tick (up
            // to BROADCASTER_PING_INTERVAL_SECS away) — Perfect Sync's first
            // position calculation on this tune-in wants it as soon as
            // possible, not several seconds from now.
            out_tx
                .send(RadioMessage::Ping { server_ts: now_ms() as u64 })
                .await
                .map_err(|_| PlayerError::WebSocketError("Failed to send ping".into()))?;

            if let Some(queue) = b_state.queue {
                out_tx
                    .send(RadioMessage::BroadcastQueue {
                        owner: owner_name(state, &broadcaster_id),
                        broadcaster_id: broadcaster_id.clone(),
                        name: queue.name,
                        media_ids: queue.media_ids,
                    })
                    .await
                    .map_err(|_| PlayerError::WebSocketError("Failed to send queue".into()))?;
            }

            broadcast_analytics_throttled(state);
        }

        RadioMessage::TuneOut => {
            crate::player::metrics::inc_messages("TuneOut");
            tracing::info!("Client tuned out");

            // O(1) reverse lookup instead of scanning every broadcaster's listener set
            if let Some((_, broadcaster_id)) = state.session_tuned_to.remove(validated_session_id) {
                if let Some(mut listeners) = state.broadcaster_listeners.get_mut(&broadcaster_id) {
                    listeners.remove(validated_session_id);
                    let count = listeners.len();
                    drop(listeners);
                    if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
                        broadcast.listener_count = count;
                    }
                }
            }

            tuned_tx
                .send(None)
                .map_err(|_| PlayerError::WebSocketError("Failed to tune out".into()))?;

            broadcast_analytics_throttled(state);
        }

        RadioMessage::BroadcastUpdate {
            broadcaster_id,
            media_index,
            playback_time,
            is_playing,
            out_latency_ms,
        } => {
            crate::player::metrics::inc_messages("BroadcastUpdate");
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Enforce broadcast update rate limits (prevents spam)
            if let Err(e) = broadcast_limiter.check_and_consume(&broadcaster_id) {
                crate::player::metrics::inc_rate_limit_hits("broadcast");
                return Err(e);
            }

            // Acquire a read lock, get the playlist length, then drop the guard before any await
            let playlist_len = state.playlist.read().await.len();
            validate_media_index(media_index, playlist_len)?;

            let media_name = {
                let playlist = state.playlist.read().await;
                playlist
                    .get(media_index)
                    .map(|media| media.filename.clone())
                    .unwrap_or_else(|| format!("Unknown media #{}", media_index))
            };

            tracing::debug!(
                media_name,
                media_index,
                playback_time,
                is_playing,
                "Broadcast update"
            );

            // If the session isn't broadcasting don't update
            if !state.broadcast_states.contains_key(&broadcaster_id) {
                return Err(PlayerError::BroadcasterNotFound(broadcaster_id));
            }

            // Latency is the broadcaster to server leg measured by Ping/Pong, 0 until the
            // first Pong lands. is_same_media guards the intro: a media switch starts
            // listeners at 0 on purpose, so we don't add latency across it.
            let (latency_ms, is_same_media) = state
                .broadcast_states
                .get(&broadcaster_id)
                .map(|b| (b.transmission_latency_ms, b.media_index == media_index))
                .unwrap_or((0, false));

            let adjusted_playback_time = if is_same_media {
                playback_time + (latency_ms as f64 / 1000.0)
            } else {
                playback_time
            };

            let listener_count = state
                .broadcaster_listeners
                .get(&broadcaster_id)
                .map(|set| set.len())
                .unwrap_or(0);

            let server_ts = now_ms();

            // Acquire a read lock, get the media name, then drop the guard before any await
            let media_name = {
                let playlist = state.playlist.read().await;
                playlist
                    .get(media_index)
                    .map(|media| media.filename.clone())
                    .unwrap_or_else(|| format!("Unknown media #{}", media_index))
            };

            // Keep the last non-zero output-latency figure the broadcaster reported.
            let broadcaster_out_latency_ms = if out_latency_ms > 0 {
                out_latency_ms
            } else {
                state
                    .broadcast_states
                    .get(&broadcaster_id)
                    .map(|b| b.broadcaster_out_latency_ms)
                    .unwrap_or(0)
            };

            // Update the server-side broadcast state (raw playback_time, not adjusted)
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                media_index,
                media_name,
                playback_time, // store raw so TuneIn can apply latency consistently
                is_playing,
                server_timestamp_ms: server_ts,
                listener_count,
                transmission_latency_ms: latency_ms,
                broadcaster_out_latency_ms,
                // Filled in fresh on every analytics push, not stored here.
                username: None,
                queue: state
                    .broadcast_states
                    .get(&broadcaster_id)
                    .and_then(|b| b.queue.clone()),
            };

            state
                .broadcast_states
                .insert(broadcaster_id.clone(), new_state);

            // Serialize once here, all N listeners share the same Arc<PreparedMessage>
            // Sync carries the latency adjusted time so listeners land near the right position.
            let sync_msg = Arc::new(PreparedMessage::new(&RadioMessage::Sync {
                broadcaster_id: broadcaster_id.clone(),
                media_index,
                playback_time: adjusted_playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
                broadcaster_out_latency_ms,
            }));

            // A return value of Err does not mean that future calls to send will fail
            if let Some(tx) = state.broadcast_channels.get(&broadcaster_id) {
                match tx.send(sync_msg) {
                    Ok(count) => {
                        tracing::trace!(listeners = count, "Sync sent");
                    }
                    Err(_) => {
                        tracing::trace!("Sync sent but no active listeners");
                    }
                }
            }

            // Throttled, BroadcastUpdate fires on every play/pause/seek
            broadcast_analytics_throttled(state);
        }

        RadioMessage::Heartbeat {
            broadcaster_id,
            playback_time,
        } => {
            crate::player::metrics::inc_messages("Heartbeat");

            // Validate broadcaster session ID
            let session_id = SessionId::new(broadcaster_id.clone())?;

            // Enforce heartbeat rate limits
            if let Err(e) = heartbeat_limiter.check_and_consume(session_id.as_str()) {
                crate::player::metrics::inc_rate_limit_hits("heartbeat");
                return Err(e);
            }

            let server_ts = now_ms();

            // Touch the session so passive listeners don't get cleaned up mid-media.
            // Range requests fire on every seek so this naturally stays fresh during playback.
            if let Some(mut session) = state.sessions.get_mut(session_id.as_str()) {
                session.last_activity = std::time::Instant::now();
            }

            // Latency is tracked by Ping/Pong, the heartbeat only moves the
            // authoritative playback clock forward so a late-joining TuneIn gets
            // a fresh position. It is NOT fanned out to listeners: Perfect Sync
            // listeners re-anchor only on real Sync events (play/pause/seek/next)
            // and otherwise play the file straight — a periodic fan-out was
            // tried here and reverted, it caused visible/audible position jumps
            // every couple seconds during otherwise-steady playback, which is
            // worse than the (much smaller) clock drift it was meant to fix.
            if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
                broadcast.playback_time = playback_time;
                broadcast.server_timestamp_ms = server_ts;
                tracing::trace!(broadcaster_id = %session_id, playback_time, "Heartbeat");
            }
        }

        RadioMessage::ClockProbe { client_ts } => {
            crate::player::metrics::inc_messages("ClockProbe");

            // Pure echo. The client does all the offset/RTT math so nothing here
            // has to trust or read the client's clock, it just stamps its own.
            out_tx
                .send(RadioMessage::ClockEcho {
                    client_ts,
                    server_ts: now_ms() as u64,
                })
                .await
                .map_err(|_| {
                    PlayerError::WebSocketError("Failed to send clock echo".into())
                })?;
        }

        RadioMessage::Pong { server_ts } => {
            crate::player::metrics::inc_messages("Pong");

            // rtt is timed start to finish on the server clock, so half of it is the
            // one way latency for whichever leg this session is on. Nothing here
            // reads the client's clock, which is what let the old estimate blow up
            // when the clocks drifted.
            let rtt_ms = (now_ms() as u64).saturating_sub(server_ts);
            let one_way_ms = (rtt_ms / 2).min(MAX_PLAUSIBLE_LATENCY_MS);

            // Broadcasters keep their figure on BroadcastState too — that's what
            // feeds the latency adjustment already baked into outgoing Sync
            // messages. Every session (broadcaster or listener) also gets it here,
            // generically, since a listener has no BroadcastState of their own.
            if let Some(mut broadcast) = state.broadcast_states.get_mut(validated_session_id) {
                broadcast.transmission_latency_ms = one_way_ms;
            }
            state
                .session_latency_ms
                .insert(validated_session_id.to_string(), one_way_ms);
            tracing::trace!(rtt_ms, one_way_ms, "Latency updated from Pong");

            out_tx
                .send(RadioMessage::YourLatency { one_way_ms })
                .await
                .map_err(|_| {
                    PlayerError::WebSocketError("Failed to send latency update".into())
                })?;
        }

        RadioMessage::BroadcastQueue { broadcaster_id, name, media_ids, .. } => {
            crate::player::metrics::inc_messages("BroadcastQueue");
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Only ids the server actually has, so a listener never gets handed
            // something it can't stream. Duplicates dropped too, since the
            // client keys its list rows by id. Anything past the cap is cut
            // off, the same way an overlong chat line is.
            let media_ids: Vec<String> = {
                let playlist = state.playlist.read().await;
                let known: HashSet<&str> = playlist.iter().map(|m| m.id.as_str()).collect();
                let mut seen = HashSet::new();
                media_ids
                    .into_iter()
                    .filter(|id| known.contains(id.as_str()) && seen.insert(id.clone()))
                    .take(MAX_QUEUE_LEN)
                    .collect()
            };
            let name: String = name.trim().chars().take(MAX_QUEUE_NAME_LEN).collect();

            // Only a live broadcaster has somewhere to keep this.
            match state.broadcast_states.get_mut(&broadcaster_id) {
                Some(mut b) => {
                    b.queue = Some(BroadcastQueueInfo {
                        name: name.clone(),
                        media_ids: media_ids.clone(),
                    });
                }
                None => return Err(PlayerError::BroadcasterNotFound(broadcaster_id)),
            }

            tracing::debug!(tracks = media_ids.len(), name = %name, "Broadcast queue updated");

            let msg = Arc::new(PreparedMessage::new(&RadioMessage::BroadcastQueue {
                owner: owner_name(state, &broadcaster_id),
                broadcaster_id: broadcaster_id.clone(),
                name,
                media_ids,
            }));
            if let Some(tx) = state.broadcast_channels.get(&broadcaster_id) {
                let _ = tx.send(msg);
            }
        }

        RadioMessage::StopBroadcasting { broadcaster_id } => {
            crate::player::metrics::inc_messages("StopBroadcasting");

            // Validate broadcaster session ID
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            tracing::info!("Broadcaster stopping");

            state.broadcast_states.remove(&broadcaster_id);
            state.broadcast_channels.remove(&broadcaster_id);

            // No room of our own any more, stop the send task listening on it.
            let _ = own_room_tx.send(false);

            let offline_msg = Arc::new(PreparedMessage::new(&RadioMessage::BroadcasterOffline {
                broadcaster_id: broadcaster_id.clone(),
            }));

            // A return value of Err does not mean that future calls to send will fail
            match state.global_broadcast_tx.send(offline_msg) {
                Ok(count) => {
                    tracing::debug!(clients = count, "BroadcasterOffline sent");
                }
                Err(_) => {
                    tracing::debug!("BroadcasterOffline sent but no clients connected");
                }
            }

            broadcast_analytics(state);
        }

        RadioMessage::StartBroadcasting {
            broadcaster_id,
            media_index,
            playback_time,
            is_playing,
            out_latency_ms,
        } => {
            crate::player::metrics::inc_messages("StartBroadcasting");
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Enforce broadcast update rate limits (prevents spam)
            if let Err(e) = broadcast_limiter.check_and_consume(&broadcaster_id) {
                crate::player::metrics::inc_rate_limit_hits("broadcast");
                return Err(e);
            }

            // Acquire a read lock, get the playlist length, then drop the guard before any await
            let playlist_len = state.playlist.read().await.len();
            validate_media_index(media_index, playlist_len)?;

            let was_already_broadcasting = state.broadcast_states.contains_key(&broadcaster_id);

            if was_already_broadcasting {
                tracing::debug!(
                    "Already registered - updating state only",
                );
            } else {
                tracing::debug!("New broadcaster starting updating broadcaster count too",);
            }

            tracing::info!(media_index, playback_time, is_playing, "Starting broadcast");

            let server_ts = now_ms();

            // Acquire a read lock, get the media name, then drop the guard before any await
            let media_name = {
                let playlist = state.playlist.read().await;
                playlist
                    .get(media_index)
                    .map(|media| media.filename.clone())
                    .unwrap_or_else(|| format!("Unknown media #{}", media_index))
            };

            // Preserve a previously reported output latency on a resume.
            let broadcaster_out_latency_ms = if out_latency_ms > 0 {
                out_latency_ms
            } else {
                state
                    .broadcast_states
                    .get(&broadcaster_id)
                    .map(|b| b.broadcaster_out_latency_ms)
                    .unwrap_or(0)
            };

            // Latency stays 0 until the first Pong comes back
            let new_state = BroadcastState {
                broadcaster_id: broadcaster_id.clone(),
                media_index,
                media_name,
                playback_time,
                is_playing,
                server_timestamp_ms: server_ts,
                listener_count: 0,
                transmission_latency_ms: 0,
                broadcaster_out_latency_ms,
                // Filled in fresh on every analytics push, not stored here.
                username: None,
                queue: state
                    .broadcast_states
                    .get(&broadcaster_id)
                    .and_then(|b| b.queue.clone()),
            };

            state
                .broadcast_states
                .insert(broadcaster_id.clone(), new_state);

            // Create the channel if it doesn't exist
            state
                .broadcast_channels
                .entry(broadcaster_id.clone())
                .or_insert_with(|| {
                    let (tx, _rx) = broadcast::channel::<Arc<PreparedMessage>>(100);
                    tracing::debug!("Created broadcast channel");
                    tx
                });

            // Now that the channel exists, tell our own send task to listen on
            // it too so we pick up chat from anyone tuned into us. Safe to fire
            // on a resume as well, the send task just resubscribes.
            let _ = own_room_tx.send(true);

            // Send announcement through global channel, not the broadcaster's channel
            if !was_already_broadcasting {
                let broadcasting_msg =
                    Arc::new(PreparedMessage::new(&RadioMessage::BroadcasterOnline {
                        broadcaster_id: broadcaster_id.clone(),
                    }));

                match state.global_broadcast_tx.send(broadcasting_msg) {
                    Ok(count) => tracing::info!(clients = count, "BroadcasterOnline sent"),
                    Err(_) => tracing::debug!("BroadcasterOnline: no listeners yet"),
                }
                tracing::info!("Broadcaster came online");

                broadcast_analytics(state);
            }

            broadcast_analytics(state);
        }

        // On a page visibility reload checks if our session is alive and broadcasting,
        // if it is, returns the state so the sessions can resume its broadcasting (this is used for mobile visibility mode)
        RadioMessage::QueryBroadcastState { session_id } => {
            crate::player::metrics::inc_messages("QueryBroadcastState");
            ensure_same_session(&session_id, validated_session_id)?;

            // Check if this session is currently broadcasting, by making sure that the current session is both alive and broadcasting
            let is_broadcasting = state.broadcast_states.contains_key(&session_id)
                && state.sessions.contains_key(&session_id);
            let current_state = state.broadcast_states.get(&session_id).map(|b| b.clone());

            tracing::debug!(
                session_id = %session_id,
                is_broadcasting,
                "Query broadcast state"
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

        RadioMessage::AutoNext {
            broadcaster_id,
            next_media_index,
            ..
        } => {
            crate::player::metrics::inc_messages("AutoNext");
            ensure_same_session(&broadcaster_id, validated_session_id)?;

            // Acquire a read lock, get the playlist length, then drop the guard before any await
            let playlist_len = state.playlist.read().await.len();
            validate_media_index(next_media_index, playlist_len)?;

            let server_ts = now_ms();

            // Acquire a read lock, get the media name, then drop the guard before any await
            let media_name = {
                let playlist = state.playlist.read().await;
                playlist
                    .get(next_media_index)
                    .map(|s| s.filename.clone())
                    .unwrap_or_else(|| format!("Unknown media #{}", next_media_index))
            };

            // Advance authoritative state so late-joining listeners get the right media
            if let Some(mut broadcast) = state.broadcast_states.get_mut(&broadcaster_id) {
                broadcast.media_index = next_media_index;
                broadcast.media_name = media_name;
                broadcast.playback_time = 0.0;
                broadcast.server_timestamp_ms = server_ts;
                // transmission_latency_ms is left as is, it's a property of the socket
                // not the media, and Ping/Pong keeps it current on its own
            }

            // Fan out AutoNext as-is so listeners can handle it differently from Sync
            let msg = Arc::new(PreparedMessage::new(&RadioMessage::AutoNext {
                broadcaster_id: broadcaster_id.clone(),
                next_media_index,
                server_timestamp_ms: server_ts as u64,
            }));

            if let Some(tx) = state.broadcast_channels.get(&broadcaster_id) {
                match tx.send(msg) {
                    Ok(n) => tracing::debug!(listeners = n, "AutoNext fanned out"),
                    Err(_) => tracing::debug!("AutoNext sent but no active listeners"),
                }
            }

            broadcast_analytics_throttled(state);
        }

        RadioMessage::Chat { text, room: requested_room, .. } => {
            crate::player::metrics::inc_messages("Chat");

            // Same idea as the broadcast limiter, keyed per session so one person
            // spamming the box only slows themselves down.
            if let Err(e) = chat_limiter.check_and_consume(validated_session_id) {
                crate::player::metrics::inc_rate_limit_hits("chat");
                return Err(e);
            }

            // Trim first so a line of nothing but spaces counts as empty and
            // gets dropped without a fuss.
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(());
            }

            // Clamp the length, walking back to a char boundary so we never cut
            // a multibyte character in half.
            let body = if trimmed.len() > MAX_CHAT_LEN {
                let mut end = MAX_CHAT_LEN;
                while !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                &trimmed[..end]
            } else {
                trimmed
            };

            let room = resolve_room(state, validated_session_id, &requested_room);

            // Account name for this session, empty if they never logged in. The
            // client shows this instead of the raw session id when it's set.
            let from_name = state
                .session_users
                .get(validated_session_id)
                .map(|u| u.clone())
                .unwrap_or_default();

            // Stamp the message once here so every listener shares the same bytes.
            let outgoing = Arc::new(PreparedMessage::new(&RadioMessage::Chat {
                room: room.clone(),
                from: validated_session_id.to_string(),
                from_name,
                text: body.to_string(),
                server_timestamp_ms: now_ms(),
            }));

            if room == "global" {
                match state.global_broadcast_tx.send(outgoing) {
                    Ok(n) => tracing::debug!(recipients = n, "Global chat sent"),
                    Err(_) => tracing::debug!("Global chat sent but nobody is connected"),
                }
            } else {
                // One send covers the whole room. Listeners are already
                // subscribed to this channel from tuning in, and the broadcaster
                // picks it up through the own room branch in the send task.
                match state.broadcast_channels.get(&room) {
                    Some(tx) => match tx.send(outgoing) {
                        Ok(n) => tracing::debug!(room = %room, recipients = n, "Room chat sent"),
                        Err(_) => tracing::debug!(room = %room, "Room chat sent but the room is empty"),
                    },
                    None => {
                        // Room went away between tuning in and now, for instance
                        // the broadcaster just stopped. Drop it quietly.
                        tracing::debug!(room = %room, "Room chat dropped, no channel");
                    }
                }
            }
        }

        RadioMessage::RoomRelay {
            room: requested_room,
            room_code,
            payload,
            ..
        } => {
            crate::player::metrics::inc_messages("RoomRelay");

            // Same shape as the chat limiter: keyed per session, so one
            // misbehaving room-sync client can't flood everyone else's room.
            if let Err(e) = room_relay_limiter.check_and_consume(validated_session_id) {
                crate::player::metrics::inc_rate_limit_hits("room_relay");
                return Err(e);
            }

            // Room Sync rides on the same access rules as Chat: a room is
            // whatever the sender is tuned into, their own room if they are
            // broadcasting, or global, and a requested room is only honored if
            // they may actually post there.
            let room = resolve_room(state, validated_session_id, &requested_room);

            // Stamped once here so every recipient shares the same bytes.
            // `payload` is passed through untouched, its "kind" (presence /
            // calibrate_chirp / tick) is meaningless to the server.
            let outgoing = Arc::new(PreparedMessage::new(&RadioMessage::RoomRelay {
                room: room.clone(),
                from: validated_session_id.to_string(),
                room_code,
                payload,
                server_timestamp_ms: now_ms(),
            }));

            if room == "global" {
                match state.global_broadcast_tx.send(outgoing) {
                    Ok(n) => tracing::trace!(recipients = n, "Room relay sent (global)"),
                    Err(_) => tracing::trace!("Room relay sent but nobody is connected"),
                }
            } else {
                match state.broadcast_channels.get(&room) {
                    Some(tx) => match tx.send(outgoing) {
                        Ok(n) => tracing::trace!(room = %room, recipients = n, "Room relay sent"),
                        Err(_) => {
                            tracing::trace!(room = %room, "Room relay sent but the room is empty")
                        }
                    },
                    None => {
                        tracing::trace!(room = %room, "Room relay dropped, no channel");
                    }
                }
            }
        }

        _ => {
            // Any unexpected messages are logged but ignored
            tracing::warn!("Received unexpected message type");
        }
    }

    Ok(())
}

/// Resolves which room a message from `sender_id` should land in, given the
/// room it asked for (may be empty). Shared by `Chat` and `RoomRelay` so both
/// obey identical access rules:
///   - empty request -> sender's default: tuned into someone means their
///     room, broadcasting with no tune means the sender's own room, otherwise
///     the global room everyone shares.
///   - non-empty request -> honored only if the sender may actually post
///     there (global is open to all, a broadcaster room only to its listeners
///     and the broadcaster themselves); otherwise falls back to the default.
fn resolve_room(state: &SharedState, sender_id: &str, requested: &str) -> String {
    let tuned_to = state.session_tuned_to.get(sender_id).map(|r| r.clone());

    let default_room = || match &tuned_to {
        Some(broadcaster_id) => broadcaster_id.clone(),
        None if state.broadcast_channels.contains_key(sender_id) => sender_id.to_string(),
        None => "global".to_string(),
    };

    let requested = requested.trim();
    if requested.is_empty() {
        default_room()
    } else if requested == "global"
        || tuned_to.as_deref() == Some(requested)
        || (requested == sender_id && state.broadcast_channels.contains_key(sender_id))
    {
        requested.to_string()
    } else {
        tracing::warn!(
            requested_room = %requested,
            "Room not accessible to sender, using their default room"
        );
        default_room()
    }
}

/// Verifies that the provided identifier matches the session identifier
/// recorded when the WebSocket connection was first established.
/// This is used to ensure that a client cannot act or broadcast as another
/// session by supplying a different ID after the connection is open.
fn ensure_same_session(expected: &str, actual: &str) -> Result<(), PlayerError> {
    (expected == actual).then_some(()).ok_or_else(|| {
        tracing::warn!(
            session_id = %actual,
            attempted_as = %expected,
            "Session impersonation attempt"
        );
        PlayerError::BroadcastUnauthorized(
            "Trying to use session id that differs from the one established upon websocket handshake.".into())
    })
}

/// Account name behind a session, empty when it isn't logged in.
fn owner_name(state: &SharedState, session_id: &str) -> String {
    state
        .session_users
        .get(session_id)
        .map(|u| u.clone())
        .unwrap_or_default()
}

pub fn delete_broadcasting_session(state: &SharedState, broadcaster_id: &str) {
    if !state.broadcast_states.contains_key(broadcaster_id) {
        return;
    }

    tracing::info!(
        broadcaster_id = %broadcaster_id,
        "Auto-cleanup: Disconnected broadcaster - removing state",
    );

    state.broadcast_states.remove(broadcaster_id);
    state.broadcaster_listeners.remove(broadcaster_id);

    let offline_msg = Arc::new(PreparedMessage::new(&RadioMessage::BroadcasterOffline {
        broadcaster_id: broadcaster_id.to_string(),
    }));

    match state.global_broadcast_tx.send(offline_msg) {
        Ok(listener_count) => {
            tracing::info!(
                broadcaster_id = %broadcaster_id,
                listeners = listener_count,
                "Notified listeners that broadcaster went offline"
            );
        }
        Err(_) => {
            tracing::debug!(broadcaster_id = %broadcaster_id, "No listeners to notify");
        }
    }

    if state.broadcast_channels.remove(broadcaster_id).is_some() {
        tracing::debug!(broadcaster_id = %broadcaster_id, "Removed broadcast channel");
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

/// Throttled analytics, skips the broadcast if one was sent within ANALYTICS_THROTTLE_MS.
/// Use this on high-frequency paths (TuneIn, TuneOut, BroadcastUpdate).
/// Discrete state-change events (connect, disconnect, start/stop) should call broadcast_analytics directly.
pub fn broadcast_analytics_throttled(state: &SharedState) {
    let now = now_ms() as u64;
    let last = state.last_analytics_ms.load(Relaxed);

    if now.saturating_sub(last) < ANALYTICS_THROTTLE_MS {
        return;
    }

    // compare_exchange prevents a thundering herd of concurrent updates all firing at once
    if state
        .last_analytics_ms
        .compare_exchange(last, now, Relaxed, Relaxed)
        .is_ok()
    {
        broadcast_analytics(state);
    }
}

/// Broadcasts current analytics to all connected clients via the global channel.
/// Call this whenever any counter changes to keep all clients synchronized.
pub fn broadcast_analytics(state: &SharedState) {
    // Skip the entire scan and allocation when no one is connected
    if state.active_connections.load(Relaxed) == 0 {
        return;
    }

    // Compute active_listeners as the total number of strings across all
    // per-broadcaster listener sets, single read lock, no atomic to drift
    let active_listeners = state
        .broadcaster_listeners
        .iter()
        .map(|entry| entry.value().len())
        .sum::<usize>();

    let active_connections = state.active_connections.load(Relaxed);

    let active_broadcasters = state.broadcast_channels.len();

    // Update Prometheus gauges alongside the WebSocket analytics push
    crate::player::metrics::set_active(active_connections, active_broadcasters, active_listeners);

    // Collect broadcaster states, injecting fresh listener counts so the UI
    // is always accurate regardless of whether TuneIn/TuneOut updated it
    let broadcasters: Vec<BroadcastState> = state
        .broadcast_states
        .iter()
        .map(|entry| {
            let mut b = entry.value().clone();
            b.listener_count = state
                .broadcaster_listeners
                .get(&b.broadcaster_id)
                .map(|s| s.len())
                .unwrap_or(0);
            // Look the account name up now rather than trusting whatever was
            // stored, so it turns up as soon as the broadcaster logs in.
            b.username = state
                .session_users
                .get(&b.broadcaster_id)
                .map(|u| u.clone());
            b
        })
        .collect();

    let analytics_msg = Arc::new(PreparedMessage::new(&RadioMessage::Analytics {
        active_connections,
        active_broadcasters,
        active_listeners,
        broadcasters,
    }));

    match state.global_broadcast_tx.send(analytics_msg) {
        Ok(count) => tracing::trace!("Analytics update sent to {} clients", count),
        Err(_) => tracing::trace!("Analytics update sent but no clients connected"),
    }
}
