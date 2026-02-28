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
    radio.rs        — WebSocket lifecycle, radio protocol, analytics
    session.rs      — session helpers, timestamp utils, stale session cleanup
    templates.rs    — Askama template structs and IntoResponse impls
    error.rs        — PlayerError, PlayerResult
    validation.rs   — SessionId newtype, song index validation
    rate_limit.rs   — token bucket rate limiter
    reconnect.rs    — reconnection logic
    templates/
      player.html         — full player page with radio UI and playlist
      player_controls.html — HTMX partial for control updates
  main.rs
music/                  — your MP3 files go here
```

---

## Getting Started

### Prerequisites

- Docker Desktop (for Docker usage) OR Rust (for native build)
- A folder of MP3 files

### Running with Docker Compose

Edit `docker-compose.yml` to point to your music folder, then:

```bash
docker-compose up -d
```

Open `http://localhost:8083/stargzr` in your browser.

### Running with Docker (Manual Build)

```bash
# Build the image
docker build -t omersadikovic/stargzr:latest .

# Run the container (adjust the path to your music folder)
docker run -d --name stargzr -p 8083:8083 -v "/path/to/your/music:/app/music:ro" omersadikovic/stargzr:latest
```

### Running from DockerHub (No Build Required)

```bash
docker pull omersadikovic/stargzr:latest
docker run -d --name stargzr -p 8083:8083 -v "/path/to/your/music:/app/music:ro" omersadikovic/stargzr:latest
```

### Running Natively with Rust

```bash
git clone https://github.com/bodln/stargzr
cd stargzr
cargo run --release
```

Edit the path in `main.rs` to point to your music folder before running.

---

## Radio Mode

1. Open the player and copy your **Session ID**
2. Click **Start Broadcasting** — you are now live
3. Share your Session ID with someone else (all broadcaster IDs will be shown in a public list and people can tune in from there too)
4. They paste it into the **Tune In** field and click **Tune In**
5. Their playback syncs to yours — play, pause, and seek updates propagate in real time

Listeners receive an initial `Sync` on tune-in, then periodic `Heartbeat` messages for drift correction. If a broadcaster disconnects, all tuned listeners are notified automatically.

---
## Sharing Your stargzr Server

stargzr can be exposed outside your local network in three primary ways:

- Direct port forwarding (public IP exposure)
- Free subdomain + reverse proxy with HTTPS
- Cloudflare Tunnel (no port forwarding)

---

### 1. Direct Port Forwarding (Public IP Exposure)

Forward port `8083` in your router to your machine's local IP.

Determine your local IP:

- Windows:
```
ipconfig
```

- Linux:
```
ip addr
```

Configure a static DHCP lease in your router by binding your machine's MAC address to its local IP to prevent IP changes.

Your server becomes accessible at:

```
http://YOUR_PUBLIC_IP:8083/stargzr
```

This method exposes your public IP address!!!




### 2. Free Subdomain + HTTPS via Caddy (Stable Setup)

Acquire a free subdomain from:

[https://freedns.afraid.org/subdomain/](https://freedns.afraid.org/subdomain/)

Example:

```
stargzr.jumpingcrab.com
```

Forward port `443` in your router to your machine.

Create a Caddy configuration file named `Caddyfile`:

```
stargzr.jumpingcrab.com {
  reverse_proxy localhost:8083
}
```

Run Caddy (if the file is named `Caddyfile`, no `--config` flag is needed):

```
caddy run
```

Or with an explicit path:

```
caddy run --config C:\caddy\Caddyfile
```

Caddy automatically provisions and renews HTTPS certificates.

Your server becomes accessible at:

[https://stargzr.jumpingcrab.com/stargzr](https://stargzr.jumpingcrab.com/stargzr)


This method provides:

- Stable domain
- Automatic HTTPS
- Public IP exposure via reverse proxy




### 3. Cloudflare Tunnel (No Port Forwarding)

Run:

```
cloudflared tunnel --url http://localhost:8083
```

Cloudflare generates a public URL:

```
https://random-name.trycloudflare.com
```
---

## Docker Image

Available at `omersadikovic/stargzr:latest` on Docker Hub.

---

<img width="2509" height="2120" alt="image" src="https://github.com/user-attachments/assets/c51672fa-8d2f-4b0b-b126-3c3d3599e0d1" />

---

## License

MIT
