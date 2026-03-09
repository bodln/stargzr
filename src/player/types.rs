use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use tokio::sync::{broadcast, RwLock};

use super::rate_limit::RateLimiter;

#[derive(Clone, Serialize, Deserialize)]
pub struct SongInfo {
    pub id: String,
    pub filename: String,
    pub size: u64,
}

/// Represents a single user's PRIVATE player state
pub struct PlayerSession {
    pub current_index: usize,
    pub last_activity: std::time::Instant, // Used to measure the age of the session
}

/// Represents a broadcaster's state (the authoritative source)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BroadcastState {
    pub broadcaster_id: String,
    pub song_index: usize,
    // TODO: here and in filename in SongInfo could be changed to Arc<str> so we dont copy them around needlessly
    // we use Arc<str> isntead of Arc<String> because, Arc<String> is two heap allocations, the Arc points to a String header (ptr + len + capacity), 
    // which points to the actual bytes. You pay for two pointer dereferences and two allocations.
    // Arc<str> is a single allocation. It's a fat pointer (data ptr + length) pointing directly at the bytes, with the Arc refcount living right before them in the same block. 
    // No intermediate String header, no wasted capacity field.
    // 
    // There's also a semantic point that String implies mutable and growable. 
    // Once a filename is in an Arc you're never mutating it, so the capacity tracking (keeping track how more can fit in it) that String carries is pure waste. 

    // We add this field so we can return the song name in analytics
    pub song_name: String,
    pub playback_time: f64, // Current position in seconds (raw, as reported by broadcaster)
    pub is_playing: bool,
    pub server_timestamp_ms: u128, // When this state was recorded
    pub listener_count: usize,
    // Estimated one way broadcaster to server latency from client_timestamp_ms on incoming messages.
    // Used to adjust playback_time in outgoing Sync messages so listeners stay in sync.
    pub transmission_latency_ms: u64,
}

/// A message that has been serialized once at the broadcast site.
/// Shared via Arc so every listener pays only a pointer clone, not a re-serialization.
pub struct PreparedMessage {
    pub json: String,
}

impl PreparedMessage {
    pub fn new(msg: &RadioMessage) -> Self {
        Self {
            json: serde_json::to_string(msg).expect("RadioMessage serialization is infallible"),
        }
    }
}

// DashMap shards the map across multiple independent RwLocks (one per shard, ~4x cpu count),
// so concurrent operations on different keys never block each other, unlike a single
// RwLock<HashMap> where every heartbeat, TuneIn, and BroadcastUpdate serialises globally.
pub struct AppState {
    // Wrapped in RwLock so the upload handler can insert new songs at runtime.
    // All read paths (streaming, playlist fetch, radio) acquire a read guard.
    // The upload handler acquires the write guard only during the insert.
    pub playlist: Arc<RwLock<Vec<SongInfo>>>,
    pub music_folder: Arc<PathBuf>,

    pub sessions: DashMap<String, PlayerSession>,

    /// Maps broadcaster_id (session id) to their current state
    pub broadcast_states: DashMap<String, BroadcastState>,

    /// Global broadcast channel for system-wide announcements.
    /// Used for BroadcasterOnline/Offline messages that all clients should see,
    /// regardless of which broadcaster they're tuned to.
    /// Carries Arc<PreparedMessage>, serialized once, cloned cheaply to every receiver.
    pub global_broadcast_tx: broadcast::Sender<Arc<PreparedMessage>>,

    /// Per-broadcaster channels for targeted playback sync.
    /// Each broadcaster has their own channel that only their listeners subscribe to.
    /// Carries Arc<PreparedMessage>, serialized once, cloned cheaply to every receiver.
    pub broadcast_channels: DashMap<String, broadcast::Sender<Arc<PreparedMessage>>>,

    /// Per broadcaster listener count
    pub broadcaster_listeners: DashMap<String, HashSet<String>>,

    /// Reverse map: session_id to broadcaster_id they are currently tuned to.
    /// Makes TuneOut O(1) instead of scanning every broadcaster's listener set.
    pub session_tuned_to: DashMap<String, String>,

    /// Counts live WebSocket connections, incremented on connect, decremented on disconnect.
    pub active_connections: AtomicUsize,

    /// Timestamp (ms) of the last analytics broadcast.
    /// Used to throttle analytics on high-frequency paths like BroadcastUpdate.
    pub last_analytics_ms: AtomicU64,

    /// Per-IP rate limiter for WebSocket upgrade requests.
    pub ws_rate_limiter: RateLimiter,

    /// Tracks how many bytes each IP has uploaded this server session.
    /// Resets on server restart. No persistence needed, acts as a soft abuse limit.
    pub upload_quotas: DashMap<String, u64>,
}

/// Helper type for cleaner function signatures
pub type SharedState = Arc<AppState>;

/// Messages sent over WebSocket
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RadioMessage {
    /// Broadcaster distributes this when play/pause/seek/next/prev happens
    Sync {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
        server_timestamp_ms: u128,
    },

    /// Broadcaster sends this every 2-3 seconds.
    /// client_timestamp_ms is the frontend's Date.now() at send time used by the server
    /// to estimate broadcaster to server latency for playback-time compensation
    Heartbeat {
        broadcaster_id: String,
        playback_time: f64,
        client_timestamp_ms: u64,
    },

    /// Listener sends this for initial tune in to broadcaster, gets Sync back
    TuneIn {
        broadcaster_id: String,
    },

    /// Listener tune out
    TuneOut,

    /// Broadcaster sends this on play/pause/seek/next/prev, and server sends Sync to all tuned in.
    /// client_timestamp_ms is the frontend's Date.now() at send time; used by the server
    /// to estimate broadcaster to server latency for playback-time compensation.
    BroadcastUpdate {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
        client_timestamp_ms: u64,
    },

    Error {
        message: String,
    },

    /// Explicitly register a broadcaster before they start sending updates
    /// Prevents BroadcasterNotFound errors when listeners try to tune in early
    StartBroadcasting {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
    },

    /// Explicitly unregister a broadcaster and notify listeners
    StopBroadcasting {
        broadcaster_id: String,
    },

    /// Notify all clients when a new broadcaster goes live
    BroadcasterOnline {
        broadcaster_id: String,
    },

    /// Notify listeners when their broadcaster disconnects
    BroadcasterOffline {
        broadcaster_id: String,
    },

    Analytics {
        active_connections: usize,
        active_broadcasters: usize,
        active_listeners: usize,
        broadcasters: Vec<BroadcastState>,
    },

    /// Client queries if they're currently broadcasting
    QueryBroadcastState {
        session_id: String,
    },

    /// Server responds with broadcast state
    BroadcastStateResponse {
        session_id: String,
        is_broadcasting: bool,
        current_state: Option<BroadcastState>,
    },

    /// Broadcaster's song ended naturally; listeners should finish their
    /// current playback then start the next song from the beginning.
    /// Suppresses the normal Sync jump so listeners don't lose their last few seconds.
    AutoNext {
        broadcaster_id: String,
        next_song_index: usize,
        server_timestamp_ms: u64,
    },

    /// Server is shutting down cleanly. Client should keep trying to reconnect
    /// since this may just be a restart.
    ServerShutdown {
        message: String,
    },
}

/// Snapshot of a single player session for the admin view.
#[derive(Serialize)]
pub struct AdminSession {
    pub session_id: String,
    /// Seconds since this session last did anything (range request, heartbeat, etc.)
    pub idle_secs: u64,
    /// Which broadcaster this session is tuned to, if any
    pub tuned_to: Option<String>,
}

/// Full server state snapshot returned by the admin endpoint.
#[derive(Serialize)]
pub struct AdminState {
    pub sessions: Vec<AdminSession>,
    pub broadcaster_listeners: std::collections::HashMap<String, Vec<String>>,
}