use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use tokio::sync::{RwLock, Semaphore, broadcast};

use super::rate_limit::RateLimiter;

/// Whether a playlist entry is audio or video.
/// Stored on MediaInfo so handlers and the frontend can make format-aware
/// decisions without re-inspecting the filename extension on every request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Audio,
    Video,
}

/// Returns the MediaType for a given filename, or None if the extension is unsupported.
/// Single source of truth for what the server accepts and serves.
pub fn media_type_for(filename: &str) -> Option<MediaType> {
    let ext = filename.rsplit('.').next()?.to_lowercase();
    match ext.as_str() {
        "mp3" | "m4a" | "wav" | "flac" | "ogg" => Some(MediaType::Audio),
        "mp4" | "webm" | "mkv" | "mov" | "avi"  => Some(MediaType::Video),
        _ => None,
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub id: String,
    pub filename: String,
    pub size: u64,
    /// Serialized to JSON and read by the frontend to decide which media element to use
    pub media_type: MediaType,
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
    pub media_index: usize,
    // TODO: here and in filename in MediaInfo could be changed to Arc<str> so we dont copy them around needlessly
    // we use Arc<str> isntead of Arc<String> because, Arc<String> is two heap allocations, the Arc points to a String header (ptr + len + capacity),
    // which points to the actual bytes. You pay for two pointer dereferences and two allocations.
    // Arc<str> is a single allocation. It's a fat pointer (data ptr + length) pointing directly at the bytes, with the Arc refcount living right before them in the same block.
    // No intermediate String header, no wasted capacity field.
    //
    // There's also a semantic point that String implies mutable and growable.
    // Once a filename is in an Arc you're never mutating it, so the capacity tracking (keeping track how more can fit in it) that String carries is pure waste.

    // We add this field so we can return the media name in analytics
    pub media_name: String,
    pub playback_time: f64, // Current position in seconds (raw, as reported by broadcaster)
    pub is_playing: bool,
    pub server_timestamp_ms: u128, // When this state was recorded
    pub listener_count: usize,
    // One way broadcaster to server latency in ms, measured by the Ping/Pong round trip.
    // Added to playback_time in outgoing Sync messages so late joiners land near the live position.
    // Zero until the first Pong arrives.
    pub transmission_latency_ms: u64,
    // Account name of whoever is broadcasting, filled in fresh on every analytics
    // push from the session to username map. None when they are not logged in.
    #[serde(default)]
    pub username: Option<String>,
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
    // Wrapped in RwLock so the upload handler can insert new medias at runtime.
    // All read paths (streaming, playlist fetch, radio) acquire a read guard.
    // The upload handler acquires the write guard only during the insert.
    pub playlist: Arc<RwLock<Vec<MediaInfo>>>,
    pub media_folder: Arc<PathBuf>,

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

    /// Per-IP rate limiter for login and register, so the endpoints can't be
    /// used to grind through passwords.
    pub auth_rate_limiter: RateLimiter,

    /// Tracks how many bytes each IP has uploaded this server session.
    /// Resets on server restart. No persistence needed, acts as a soft abuse limit.
    pub upload_quotas: DashMap<String, u64>,

    /// Makes sure only one video file can be converted with ffmpeg to prevent clogging the CPU
    pub conversion_semaphore: Arc<Semaphore>,

    /// The accounts database (SQLite). Holds usernames and password hashes.
    pub db: super::auth::Db,

    /// Key the HS256 JWTs are signed with. From the JWT_SECRET env var, or a
    /// built in default with a warning.
    pub jwt_secret: Vec<u8>,

    /// Maps a browser session id to the account name logged in on it. Written on
    /// login and register, read when stamping chat lines and the broadcaster
    /// list. A session with no entry here is just anonymous.
    pub session_users: DashMap<String, String>,

    /// Short token derived from the contents of the static JS and CSS files.
    /// Goes on every asset URL in the page as ?v=... so the moment a file
    /// changes its URL changes with it, and no browser or proxy can serve back
    /// a stale copy. Computed once at startup.
    pub asset_version: String,
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
        media_index: usize,
        playback_time: f64,
        is_playing: bool,
        server_timestamp_ms: u128,
    },

    /// Broadcaster sends this every 2-3 seconds to report its current playback_time.
    Heartbeat {
        broadcaster_id: String,
        playback_time: f64,
    },

    /// Server sends this to a broadcaster every few seconds. server_ts is the server
    /// clock at send time. The broadcaster echoes it back unchanged in Pong so the
    /// server can time the round trip without trusting the broadcaster's clock.
    Ping {
        server_ts: u64,
    },

    /// Broadcaster's reply to Ping, server_ts copied straight back.
    Pong {
        server_ts: u64,
    },

    /// Listener sends this for initial tune in to broadcaster, gets Sync back
    TuneIn {
        broadcaster_id: String,
    },

    /// Listener tune out
    TuneOut,

    /// Broadcaster sends this on play/pause/seek/next/prev, and server sends Sync to all tuned in.
    BroadcastUpdate {
        broadcaster_id: String,
        media_index: usize,
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
        media_index: usize,
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

    /// Broadcaster's media ended naturally; listeners should finish their
    /// current playback then start the next media from the beginning.
    /// Suppresses the normal Sync jump so listeners don't lose their last few seconds.
    AutoNext {
        broadcaster_id: String,
        next_media_index: usize,
        server_timestamp_ms: u64,
    },

    /// Server is shutting down cleanly. Client should keep trying to reconnect
    /// since this may just be a restart.
    ServerShutdown {
        message: String,
    },

    /// A line of chat. The client sends this with only `text` filled in, the
    /// server stamps the rest and fans it out to everyone in the same room.
    ///
    /// The room comes straight from the sender's tune in state. Tuned into
    /// someone means their room, broadcasting yourself with no tune means your
    /// own room, anything else is the global room everyone shares. A broadcaster
    /// counts as sitting in their own room so they can talk to their listeners.
    Chat {
        /// "global" or the broadcaster id whose room this belongs to.
        /// Whatever the client puts here is ignored, the server sets it.
        #[serde(default)]
        room: String,
        /// Session id of whoever sent it. Set by the server so it can't be faked.
        #[serde(default)]
        from: String,
        /// Account name of the sender, empty when they are not logged in.
        /// Set by the server from the session to username map.
        #[serde(default)]
        from_name: String,
        /// The message body.
        text: String,
        /// Server clock when the line landed, milliseconds since epoch.
        #[serde(default)]
        server_timestamp_ms: u128,
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