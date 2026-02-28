use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use tokio::sync::broadcast;

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
    // We add this field so we can return the song name in analytics
    pub song_name: String,
    pub playback_time: f64, // Current position in seconds
    pub is_playing: bool,
    pub server_timestamp_ms: u128, // When this state was recorded
    pub listener_count: usize,
}

// DashMap shards the map across multiple independent RwLocks (one per shard, ~4× cpu count),
// so concurrent operations on different keys never block each other — unlike a single
// RwLock<HashMap> where every heartbeat, TuneIn, and BroadcastUpdate serialises globally.
pub struct AppState {
    pub playlist: Arc<Vec<SongInfo>>,
    pub music_folder: Arc<PathBuf>,

    pub sessions: DashMap<String, PlayerSession>,

    /// Maps broadcaster_id(session id) -> their current state
    pub broadcast_states: DashMap<String, BroadcastState>,

    /// Global broadcast channel for system-wide announcements.
    /// Used for BroadcasterOnline/Offline messages that all clients should see,
    /// regardless of which broadcaster they're tuned to.
    /// The message is wrapped in an Arc to avoid sending the fat message but just a pointer
    pub global_broadcast_tx: broadcast::Sender<Arc<RadioMessage>>,

    /// Per-broadcaster channels for targeted playback sync.
    /// Each broadcaster has their own channel that only their listeners subscribe to.
    /// The message is wrapped in an Arc to avoid sending the fat message but just a pointer
    pub broadcast_channels: DashMap<String, broadcast::Sender<Arc<RadioMessage>>>,

    /// Per broadcaster listener count
    pub broadcaster_listeners: DashMap<String, HashSet<String>>,

    /// Reverse map: session_id -> broadcaster_id they are currently tuned to.
    /// Makes TuneOut O(1) instead of scanning every broadcaster's listener set.
    pub session_tuned_to: DashMap<String, String>,

    /// Counts live WebSocket connections — incremented on connect, decremented on disconnect.
    pub active_connections: AtomicUsize,

    /// Timestamp (ms) of the last analytics broadcast.
    /// Used to throttle analytics on high-frequency paths like BroadcastUpdate.
    pub last_analytics_ms: AtomicU64,
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

    /// Broadcaster sends this every 2-3 seconds
    Heartbeat {
        broadcaster_id: String,
        playback_time: f64,
    },

    /// Listener sends this for initial tune in to broadcaster, gets Sync back
    TuneIn {
        broadcaster_id: String,
    },

    /// Listener tune out
    TuneOut,

    /// Broadcaster sends this on play/pause/seek/next/prev, and server sends Sync to all tuned in
    BroadcastUpdate {
        broadcaster_id: String,
        song_index: usize,
        playback_time: f64,
        is_playing: bool,
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
}