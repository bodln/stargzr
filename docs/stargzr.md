# stargzr — how it works

> a deep dive into a self-hosted media player with live radio broadcasting, written in Rust and vanilla JavaScript.

---

## what is stargzr?

stargzr is a web app that lets you stream your own audio and video files from a server directly in the browser. But it has a trick: any user can start "broadcasting" — sharing their playback state in real time — and other users on the same server can "tune in" and hear exactly what the broadcaster is hearing, perfectly synchronized, like a private radio station.

the whole thing runs as a single Rust binary. There is no database, no external message broker, no React. The server manages sessions, streams files, and pushes sync messages over WebSockets. The browser does everything else with plain JavaScript classes.

---

## the big picture

before going file by file, it helps to understand the two separate concerns the app juggles simultaneously.

**private mode** is just a personal media player. You have a playlist, you can play, skip, seek, upload files, and reorder tracks. Nothing is shared with anyone.

**radio mode** is what makes stargzr interesting. One user becomes a broadcaster. Every play, pause, seek, or track change they make is sent to the server as a `BroadcastUpdate` message. The server immediately fans that message out to every listener tuned in to that broadcaster as a `Sync` message. Each listener's browser then seeks its own audio element to match. The result is that all listeners hear the same thing at (approximately) the same moment.

the architecture looks roughly like this:

```
Browser (Broadcaster)
  │  BroadcastUpdate (WebSocket)
  ▼
Rust Server
  │  Sync (WebSocket broadcast channel)
  ▼
Browser (Listener A), Browser (Listener B), ...
```

everything interesting happens in the gap between sending and receiving — latency compensation, reconnection, state recovery — and that is what most of the code is actually about.

---

## the backend — Rust with Axum

the server is built on [Axum](https://github.com/tokio-rs/axum), a Rust web framework built on top of Tokio (async runtime) and Hyper (HTTP). If you have not used Rust before, the key things to know are: memory is managed at compile time (no garbage collector), async code uses `.await` instead of `Promise.then`, and `Arc<T>` is a reference-counted pointer that lets multiple threads share the same data safely.

### shared state — `types.rs`

   every handler and every WebSocket task needs access to the same data. In Rust you cannot just use a global variable — you have to be explicit about sharing. The app does this through a single `AppState` struct wrapped in `Arc` (atomic reference count), which lets it be cloned cheaply and passed into every handler.

   ```rust
   pub type SharedState = Arc<AppState>;
   ```

   `AppState` holds everything the server needs to keep running:

   - `playlist: Arc<RwLock<Vec<MediaInfo>>>` — the list of media files. Wrapped in `RwLock` so many handlers can read it simultaneously (for streaming, playlist fetches) while the upload handler acquires an exclusive write lock only during the brief moment it inserts a new entry.

   - `sessions: DashMap<String, PlayerSession>` — one entry per browser tab. Tracks which playlist index that session is at and when it last did something.

   - `broadcast_states: DashMap<String, BroadcastState>` — the authoritative playback state for every active broadcaster: which track, what timestamp, whether playing, and estimated network latency to that broadcaster.

   - `broadcast_channels: DashMap<String, broadcast::Sender<Arc<PreparedMessage>>>` — each broadcaster gets their own Tokio broadcast channel. Listeners subscribe to the channel of whoever they are tuned to.

   - `broadcaster_listeners: DashMap<String, HashSet<String>>` — who is listening to whom.

   - `session_tuned_to: DashMap<String, String>` — the reverse of the above. Given a listener session ID, which broadcaster are they tuned to? This exists purely for O(1) cleanup when a listener disconnects without sending `TuneOut` first.

   - `global_broadcast_tx: broadcast::Sender<Arc<PreparedMessage>>` — one channel that goes to every connected client regardless of radio state. Used for system-wide announcements: `BroadcasterOnline`, `BroadcasterOffline`, `Analytics`, `ServerShutdown`.

   `DashMap` is used throughout instead of `RwLock<HashMap>`. The difference matters: a single `RwLock<HashMap>` serializes every write across the whole map, so a heartbeat from broadcaster A blocks a TuneIn from listener B even though they touch different keys. `DashMap` shards internally — by default into roughly four times the number of CPU cores — so operations on different keys almost never contend.

   one interesting type in here is `PreparedMessage`:

   ```rust
   pub struct PreparedMessage {
       pub json: String,
   }
   ```

   when a broadcaster sends a `BroadcastUpdate`, the server needs to push a `Sync` to potentially hundreds of listeners. Serializing the same struct once per listener would be wasteful. Instead the server serializes once into a `PreparedMessage`, wraps it in `Arc`, and sends that arc through the broadcast channel. Each listener's send task receives a clone of the arc (a pointer increment, not a copy of the string) and writes the same bytes to their socket.

### sessions — `session.rs`

   the session system is deliberately simple. When a browser first loads the player page, the server looks for a `player_session` cookie. If there is none, it generates a UUID and sets that cookie in the response. That UUID is the session ID for the lifetime of the tab.

   `get_or_create_position` either returns the existing session's current playlist index or creates a new one at index 0:

   ```rust
   pub fn get_or_create_position(state: &AppState, session_id: &str) -> usize {
       if let Some(session) = state.sessions.get(session_id) {
           return session.current_index;
       }
       state.sessions.entry(session_id.to_string())
           .or_insert_with(|| PlayerSession { current_index: 0, last_activity: now })
           .current_index
   }
   ```

   every time a range request comes in (every seek during playback), the handler touches `last_activity` on the session. This keeps active sessions alive. A background task (`cleanup_stale_sessions`) runs hourly and removes sessions that have been idle for more than a day. Broadcaster sessions have a much shorter timeout — 30 seconds — because a broadcaster who disconnects without sending `StopBroadcasting` should be cleaned up quickly rather than leaving ghost sessions in the admin panel.

### streaming — `handlers.rs`

   streaming is where the HTTP layer earns its keep. Browsers do not just download an entire audio or video file up front. They issue range requests: "give me bytes 2,000,000 through 3,000,000." This is what enables seeking — the browser asks for the part of the file it needs.

   the `parse_range_header` function pulls the `Range: bytes=start-end` header apart:

   ```rust
   fn parse_range_header(headers: &HeaderMap, file_size: u64) -> Option<(u64, u64)> {
       let range_str = headers.get(header::RANGE)?.to_str().ok()?;
       let range_str = range_str.strip_prefix("bytes=")?;
       let (start_str, end_str) = range_str.split_once('-')?;
       let start: u64 = start_str.parse().ok()?;
       let end: u64 = if end_str.is_empty() {
           file_size - 1
       } else {
           end_str.parse::<u64>().ok()?.min(file_size - 1)
       };
       Some((start, end))
   }
   ```

   if a range is present, the server opens the file, seeks to `start`, and wraps the remaining bytes in a `ReaderStream` limited to `end - start + 1` bytes. It responds with HTTP `206 Partial Content` and a `Content-Range` header. If no range is present, the server still responds with 206 rather than 200 — this is deliberate. A plain 200 response causes some browsers to abort the stream and restart from scratch when seeking, introducing a noticeable delay before MP4 playback begins.

   files are streamed in 1 MB chunks:

   ```rust
   const STREAMING_CHUNK_BYTES: usize = 1 * 1024 * 1024;
   let stream = ReaderStream::with_capacity(file, STREAMING_CHUNK_BYTES);
   ```

   this means at most 1 MB lives in RAM per in-flight request, regardless of how large the file is.

   every served file gets an aggressive cache header:

   ```
   Cache-Control: public, max-age=31536000
   ```

   this tells the browser it can cache the bytes for a year. Once a user has seeked through a file once, subsequent seeks to already-loaded regions cost nothing network-wise.

### the radio protocol — `radio.rs`

   this is the heart of stargzr. Each WebSocket connection is handled by `handle_radio_connection`, which immediately splits the socket into a sender half and a receiver half, then spawns two tasks — a send task and a receive task — that run concurrently for the lifetime of the connection.

   the receive task reads incoming messages from the browser, parses them as `RadioMessage` (a serde-tagged enum), and calls `handle_client_message`. The send task sits in a `tokio::select!` loop waiting on three different sources:

   1. **the tuned broadcaster's channel** — `Sync` messages forwarded from the broadcaster's `BroadcastUpdate`
   2. **the global channel** — system-wide announcements like `BroadcasterOffline`
   3. **`out_rx`** — direct replies to this specific client (e.g., the initial `Sync` on `TuneIn`)

   `select!` with `biased` ordering means the tuned broadcaster's channel is always checked first when multiple sources are ready simultaneously. This minimizes the latency between a broadcaster's seek and a listener hearing it.

   ```rust
   tokio::select! {
       biased;
       Ok(msg) = async { match &mut current_tuned_broadcaster_rx { ... } } => { ... }
       Ok(msg) = global_broadcast_rx.recv() => { ... }
       Some(msg) = out_rx.recv() => { ... }
       Ok(()) = tuned_rx.changed() => { ... }
   }
   ```

   the `tuned_tx` / `tuned_rx` pair is a `watch` channel — a special channel that always holds the latest value, and notifies the receiver when it changes. When a listener sends `TuneIn`, the receive task writes the broadcaster ID to `tuned_tx`. The send task's `tuned_rx.changed()` branch fires, looks up the broadcaster's channel, subscribes to it, and stores the subscription as `current_tuned_broadcaster_rx`. From that moment on, every `Sync` the broadcaster triggers lands directly in this listener's send loop.

   **latency compensation** is applied in two places. When a broadcaster sends `BroadcastUpdate`, the server estimates the broadcaster-to-server transit time:

   ```rust
   let latency_ms = (now_ms() as u64).saturating_sub(client_timestamp_ms);
   ```

   `client_timestamp_ms` is `Date.now()` stamped by the browser at send time. Subtracting it from the server's wall clock gives an approximation of how long the message spent in flight. The server adds this to `playback_time` before writing it into the outgoing `Sync`, so listeners aim slightly ahead of where the broadcaster actually was when they sent the update.

   the second compensation happens client-side on `TuneIn`. The browser records `Date.now()` as `_tuneInSentAt` when sending `TuneIn`. The first `Sync` it receives advances `playback_time` by the elapsed milliseconds since `TuneInSentAt`. This corrects for the server-to-listener leg of the trip that the server has no visibility into.

### validation — `validation.rs`

   every broadcaster ID that arrives over a WebSocket is parsed as a UUID before being used:

   ```rust
   pub struct SessionId(String);

   impl SessionId {
       pub fn new(id: String) -> PlayerResult<Self> {
           uuid::Uuid::parse_str(&id).map_err(|_| PlayerError::InvalidSessionId(...))?;
           Ok(SessionId(id))
       }
   }
   ```

   this is a [newtype pattern](https://doc.rust-lang.org/rust-by-example/generics/new_types.html). `SessionId` is structurally just a `String`, but the type system enforces that you can only construct one by going through `SessionId::new`, which validates the UUID. Any function that accepts a `SessionId` parameter can assume it has already been validated — no re-checking needed.

   `ensure_same_session` is called on every broadcasted message to prevent session impersonation: a client that connects as session `A` cannot then send messages claiming to be session `B`.

### rate limiting — `rate_limit.rs`

   rate limiting is implemented as a token bucket. Each session (or IP, for WebSocket upgrades) gets a bucket with a capacity and a refill rate. Each request consumes one token. If the bucket is empty the request is rejected. The bucket refills continuously at the configured rate.

   ```rust
   pub fn for_heartbeat() -> Self { Self::new(5.0, 0.5) }   // burst 5, 1 per 2s
   pub fn for_broadcast() -> Self { Self::new(10.0, 2.0) }  // burst 10, 2 per 1s
   pub fn for_websocket() -> Self { Self::new(5.0, 0.1) }   // burst 5, 1 per 10s
   ```

   heartbeats are intentionally strict (one every two seconds) because they fire on a timer and a misconfigured or malicious client could otherwise flood the server with position updates.

### error handling — `error.rs`

   the server uses a custom `PlayerError` enum that implements Axum's `IntoResponse` trait. This means handler functions can return `Result<T, PlayerError>` and the framework automatically converts errors into appropriate HTTP responses without any extra boilerplate:

   ```rust
   PlayerError::BroadcasterNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
   PlayerError::RateLimitExceeded(_)   => (StatusCode::TOO_MANY_REQUESTS, ...),
   ```

   the `#[from]` attribute on some variants means `?` operator can automatically convert `std::io::Error` or `serde_json::Error` into a `PlayerError` — so code that does file IO or serialization can return early on failure without manually wrapping errors.

---

## the frontend — vanilla JavaScript

the browser side is split into seven files loaded in a specific order that matters. `utils.js` must be first because `debugLog` is called at parse time in other files. `main.js` must be last because it instantiates the classes defined in all the preceding files.

```html
<script src="utils.js" defer></script>
<script src="playlist.js" defer></script>
<script src="radio-player.js" defer></script>
<script src="analytics.js" defer></script>
<script src="upload.js" defer></script>
<script src="admin.js" defer></script>
<script src="search.js" defer></script>
<script src="main.js" defer></script>
```

all scripts are `defer` — they execute after the HTML is parsed, in document order.

### `radio-player.js` — `RadioPlayer` class

   this is the largest and most complex piece of frontend code. `RadioPlayer` owns the WebSocket connection and all radio-mode logic.

   **WebSocket lifecycle.** `connectWebSocket()` opens a WebSocket and registers four event handlers stored in `this.boundHandlers`. Storing them is important because `removeWebSocketHandlers()` needs to remove them later — passing an anonymous function to `addEventListener` and then trying to remove it does not work. On close, if the disconnect was not intentional, `scheduleReconnect()` fires with exponential backoff and jitter:

   ```javascript
   getReconnectDelay() {
       const exponential = this.baseReconnectDelay * Math.pow(2, this.reconnectAttempts);
       const jitter = exponential * 0.2 * (Math.random() - 0.5);
       return Math.min(Math.max(this.baseReconnectDelay, exponential + jitter), this.maxReconnectDelay);
   }
   ```

   jitter is added so that if fifty clients all lose their connection at the same moment (e.g., a server restart), they do not all reconnect at the same instant and overwhelm the server.

   **`tuneIn(broadcasterId)`.** before switching to radio mode, the player snapshots the current private playback state into `this.preRadioSnapshot`:

   ```javascript
   this.preRadioSnapshot = {
       src: this.audio.src,
       currentTime: this.audio.currentTime,
       paused: this.audio.paused,
       mediaId: window.playlistManager?.currentMediaId,
       isVideo: snapMedia?.media_type === "video",
   };
   ```

   this is what allows `tuneOut` to restore exactly where you were before you tuned in — including whether it was an audio or video file.

   **`syncToBroadcaster(msg)`.** this is called when a `Sync` message arrives. The logic handles three cases:

   1. the broadcaster is on a different track — load the new src, wait for `canplay`, then seek and play.
   2. the broadcaster is on the same track and already loading — update `_pendingSeekTime` so the in-flight `canplay` handler picks up the latest position.
   3. the broadcaster is on the same track and idle — directly set `currentTime` and play.

   a `syncTimer` debounce of 80ms is applied so that rapid `Sync` messages (which can burst when a broadcaster seeks) collapse into a single operation.

   **`AutoNext`.** when a broadcaster's track ends naturally, they send `AutoNext` with the index of the next track. Listeners do not immediately jump — they finish their own copy of the current track (which may have a few seconds left due to buffering differences) and then switch. The `pendingAutoNextIndex` field holds the queued index, and an `ended` event listener on the audio element triggers the switch.

   **Bluetooth mode.** an optional mode that briefly mutes the audio during seeks. Some Bluetooth codecs (particularly on older headphones) produce a harsh click or dropout when audio is interrupted mid-stream. Muting for the seek and then unmuting after 1 second on the `playing` event covers the codec re-negotiation gap.

### `playlist.js` — `PlaylistManager` class

   `PlaylistManager` handles everything about the user's local playlist view: loading from the server, rendering the list, drag-and-drop reordering, filter tabs, search, and download.

   **custom ordering.** the server always returns files in alphabetical order. The user can drag tracks into any order they like. That order is saved to `localStorage` as an array of media IDs:

   ```javascript
   saveCustomOrder() {
       const orderIds = this.medias.map((s) => s.id);
       localStorage.setItem(this.storageKey, JSON.stringify(orderIds));
   }
   ```

   on the next page load, `loadCustomOrder()` reads this back, builds a new array in the saved order, and appends any IDs that exist on the server but not in the saved list (new uploads) at the end.

   importantly, `originalMedias` always mirrors the server's order. The `/stream/{index}` endpoint is index-based, so when broadcasting, the app needs to report the *server's* index for a given file, not the user's reordered index. `getServerIndexById()` translates between them.

   **filter bar.** the "Navigate" filter (`all`, `audio`, `video`, `other`) controls what `playNext` and `playPrev` traverse. It never blocks a direct click — you can always click any item to play it. The filter only governs which items the next/prev buttons skip over.

   **drag and drop — desktop vs mobile.** desktop drag uses the browser's native drag-and-drop API (`dragstart`, `dragover`, `drop`). Mobile drag uses `touchstart`/`touchmove`/`touchend` on the drag handle only (not the whole row), because attaching touch listeners to the entire row would break scrolling. On mobile, the handler creates a clone of the dragged row, positions it absolutely under the finger, and reorders the underlying array in real time as the clone crosses the midpoint of other rows. `navigator.vibrate(15)` gives haptic feedback on each reorder step.

   **folder downloads.** when a user downloads a folder, the browser navigates to `/player/download-folder/{name}`. On the server side, the folder is zipped to a temporary file (on a blocking thread so the async runtime is not blocked), then streamed back. The temp file is guarded by a `DeleteOnDrop` RAII struct — if anything goes wrong between creation and streaming, the file is automatically deleted.

### `main.js` — wiring everything together

   `main.js` is the bootstrap. It sets up the dual audio/video element system, patches both elements' `play()` methods to defer playback until a pending seek completes, wires all the button event listeners, and sets up the Media Session API.

   **dual media elements.** the page has both an `<audio>` and a `<video>` element. Only one is visible at a time. `switchMediaElement(toVideo)` pauses the outgoing element, clears its `src`, hides it, then shows the incoming one and updates `window._activeMedia`. Every other piece of code that needs to control playback reads `window._activeMedia` rather than hardcoding `document.getElementById("audio-player")`.

   the initial state is set server-side: if the current playlist track is a video file, the server renders `class="hidden"` on the audio element and omits the `<source>` tag. This prevents the browser from ever trying to parse a video file as audio before JavaScript runs.

   **seek-aware play patch.** HTML5 audio has a race condition: if you set `src`, call `load()`, set `currentTime`, and immediately call `play()`, the play often fails because the seek has not completed yet. The fix is to defer `play()` until the `seeked` event fires:

   ```javascript
   el.play = () => {
       if (!seekPending) return originalPlay();
       return new Promise((resolve, reject) => {
           el.addEventListener("seeked",
               () => originalPlay().then(resolve).catch(reject),
               { once: true });
       });
   };
   ```

   both elements are patched at startup so this works regardless of which is currently active.

   **Media Session API.** this registers the current track title with the browser's OS-level media controls (lock screen on mobile, notification shade, hardware media keys). The action handlers (`nexttrack`, `previoustrack`) are registered once at startup and check `isInRadioMode()` before acting — listeners should not be able to skip the broadcaster's track.

### `analytics.js`

   `updateAnalyticsDisplay(analytics)` is called every time the server sends an `Analytics` message over the WebSocket. It updates the connection/broadcaster/listener counters at the top of the page and re-renders the "Live Broadcasts" list. Each broadcaster card shows their current track, playback position, and listener count in real time.

   the "Tune In" button in each card calls `tuneInToBroadcaster(broadcasterId)` which updates the input field and calls the same `player.tuneIn()` path as the manual tune-in button — there is no special code path for clicking a card vs pasting an ID.

### `upload.js`

   file upload uses `XMLHttpRequest` rather than `fetch` because XHR exposes `upload.progress` events that `fetch` does not (at least not without `ReadableStream` gymnastics). The progress bar is driven by these events. After a successful upload, `playlistManager.loadPlaylist()` is called to refresh the list without a page reload.

   the server returns an empty body on full success, or newline-separated per-file error messages when some files in a batch failed. This means the upload handler can report "3 of 5 files succeeded" rather than failing the whole batch on the first bad file.

### `search.js`

   playlist search is entirely client-side. The `input` event listener filters `window.playlistManager.medias` on every keystroke against a lowercased `filename.includes(query)` check, then renders matching items as a dropdown. Clicking a result scrolls the main playlist list to that item and briefly applies a `search-highlight` CSS class (which fades out after 1.5 seconds via a `setTimeout`). No server requests are made.

---

## how a broadcast session works end-to-end

to make the flow concrete, here is what happens from the moment a broadcaster clicks "Start Broadcasting" to a listener hearing their music:

1. **broadcaster clicks Start Broadcasting.** `startBroadcasting()` sends `StartBroadcasting` over the WebSocket with the current media index and playback time.

2. **server records the state.** `StartBroadcasting` handler inserts a `BroadcastState` entry, creates a broadcast channel, and sends `BroadcasterOnline` through the global channel to every connected client.

3. **all connected browsers receive `BroadcasterOnline`.** `handleRadioMessage` in each browser passes it to `updateAnalyticsDisplay`, which adds the new broadcaster to the list.

4. **broadcaster plays, pauses, or seeks.** `sendUpdate()` (a debounced 150ms function) fires and sends `BroadcastUpdate` to the server.

5. **server receives `BroadcastUpdate`.** it estimates latency, updates `BroadcastState`, serializes a `Sync` message *once* into a `PreparedMessage`, and sends the arc through the broadcaster's channel. Every subscriber (listener) receives it simultaneously.

6. **listener receives `Sync`.** `syncToBroadcaster` checks whether a media change is needed, seeks the audio element, and calls `_radioPlay()` which ensures the element is unmuted before playing.

7. **broadcaster's track ends.** `ended` event fires in the broadcaster's browser. `PlaylistManager.playNext()` is called. Before doing so, `player.sendAutoNext(nextIndex)` sends `AutoNext` to the server.

8. **server fans out `AutoNext`.** each listener's browser sets `pendingAutoNextIndex`. When their local copy of the track ends, they load the next track and seek to the queued position.

9. **broadcaster disconnects (tab closes).** the WebSocket `close` event fires. `delete_broadcasting_session` removes state and sends `BroadcasterOffline` through the global channel. All listeners' browsers call `tuneOut("broadcaster_offline")` and return to private mode.

---

## things worth noticing

**the server never stores any media metadata.** there is no database, no title tags, no artwork. The filename is the track name. This is a deliberate simplicity tradeoff — adding a SQLite database would let you store metadata, but it would also add a dependency and a migration story.

**sessions are ephemeral.** `player_session` cookies expire after one day and sessions are cleaned up server-side after the same window. Closing a tab and coming back tomorrow starts a fresh session at index 0. Custom playlist order persists (it is in `localStorage`), but your position in the playlist does not.

**video conversion happens at upload time, not at stream time.** when you upload an MKV, MOV, or AVI file, ffmpeg converts it to H.264/AAC MP4 immediately. The server enforces a semaphore so only one conversion runs at a time (preventing CPU saturation on a small VPS). The original file is deleted after conversion regardless of whether the conversion succeeded, to avoid orphaned files.

**the admin panel is public.** every user who loads the player page can see the admin panel showing all sessions, their idle times, and who they are tuned to. Session IDs are UUIDs so they are not guessable, but the information is not hidden behind any authentication. For a private home-server use case this is fine; for a multi-tenant deployment it would need a gate.

**TCP_NODELAY is set on every accepted socket.** Nagle's algorithm batches small writes to improve network efficiency on bulk transfers. For WebSockets carrying tiny JSON frames, that batching adds unnecessary latency. The `NoDelayListener` wrapper sets `TCP_NODELAY` on every socket at accept time, so sync messages are flushed immediately.

---

## summary

stargzr is a good example of how far you can get without a framework or a database when the problem is well-scoped. The Rust backend handles file streaming, session management, and WebSocket message routing with explicit data structures and no hidden magic. The JavaScript frontend is organized into classes with clear responsibilities. The protocol between them is a small set of tagged JSON messages.

the interesting engineering is almost entirely in the edges: latency compensation so listeners stay in sync, debouncing so a seek does not fire a hundred updates, RAII cleanup so temp files never leak, token buckets so a misbehaving client cannot flood the server, and exponential backoff so a server restart does not cause a thundering herd. None of these are exotic — they are standard patterns. What makes the code readable is that each one is in the right place and clearly named.
