# stargzr

A self-hosted music streaming server with live radio broadcasting built in Rust.

Stream your local MP3 library from any device on your network, or broadcast what you're listening to so others can tune in and sync in real time.

---

## Features

- **MP3 Streaming** — serves your local music library over HTTP with full byte-range support, enabling seeking and browser caching
- **Session-based Playback** — each user gets their own independent playback position tracked via cookies
- **Live Radio Broadcasting** — start a broadcast and share your session ID; listeners sync to your playback in real time over WebSocket
- **Drift Correction** — listeners receive periodic heartbeats and automatically correct position drift to stay in sync with the broadcaster
- **Drag-and-Drop Playlist** — reorder songs in the browser; custom order is saved to localStorage and persists across sessions
- **Live Analytics** — active connections, broadcasters, and listener counts pushed to all clients in real time
- **Auto-cleanup** — stale broadcaster sessions and player sessions are automatically evicted on the server
- **Rate Limiting** — heartbeat and broadcast update endpoints are rate-limited to prevent abuse
- **Mobile Support** — touch drag-and-drop for playlist reordering, Wake Lock API support to keep the screen on while broadcasting, mobile battery-aware reconnection

---

## Tech Stack

| Layer | Technology |
|---|---|
| Web framework | [Axum](https://github.com/tokio-rs/axum) |
| Async runtime | [Tokio](https://tokio.rs) |
| Templates | [Askama](https://github.com/djc/askama) |
| Frontend | [HTMX](https://htmx.org) |
| Concurrent state | [DashMap](https://github.com/xacrimon/dashmap) |
| Real-time sync | WebSocket (`tokio::sync::broadcast`) |
| Session IDs | UUID v4 |

---

## Project Structure

```
src/
  player/
    mod.rs          — server init, router, playlist scanning
    types.rs        — AppState, RadioMessage, SongInfo, BroadcastState
    handlers.rs     — HTTP route handlers (page, next, prev, stream, playlist)
    radio.rs        — WebSocket lifecycle, radio protocol, analytics, cleanup
    session.rs      — session helpers, timestamp utils
    templates.rs    — Askama template structs and IntoResponse impls
    error.rs        — PlayerError, PlayerResult
    validation.rs   — SessionId newtype, song index validation
    rate_limit.rs   — token bucket rate limiter
    templates/
      player.html         — full player page with radio UI and playlist
      player_controls.html — HTMX partial for control updates
  main.rs
music/                  — your MP3 files go here
```

---

## Getting Started

### Prerequisites

- Rust (stable)
- A folder of MP3 files

### Running

```bash
git clone https://github.com/yourusername/stargzr
cd stargzr
cargo run --release
```

By default the server binds to `0.0.0.0:8083`. Edit the path in `main.rs` to point to your music folder:

```rust
player::initialize(PathBuf::from("/path/to/your/music")).await;
```

Then open `http://localhost:8083/player` in your browser.

---

## Radio Mode

1. Open the player and copy your **Session ID**
2. Click **Start Broadcasting** — you are now live
3. Share your Session ID with someone else (all broadcaster IDs will be shown in a public list and people can tune in from there too)
4. They paste it into the **Tune In** field and click **Tune In**
5. Their playback syncs to yours — play, pause, and seek updates propagate in real time

Listeners receive an initial `Sync` on tune-in, then periodic `Heartbeat` messages for drift correction. If a broadcaster disconnects, all tuned listeners are notified automatically.

---

## Port Forwarding

To let people outside your local network tune in:

1. Forward port `8083` in your router to your machine's local IP
2. Find your local IP with `ipconfig` (Windows) or `ip addr` (Linux)
3. Set a static DHCP binding in your router using your MAC address so your IP doesn't change
4. Share your public IP or use a tunnel like [ngrok](https://ngrok.com)

---
<img width="2509" height="2120" alt="image" src="https://github.com/user-attachments/assets/c51672fa-8d2f-4b0b-b126-3c3d3599e0d1" />

---

## License

MIT
