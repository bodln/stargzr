# stargzr

A self-hosted music and video streaming server with live radio broadcasting built in Rust.

Stream your local media library from any device on your network, or broadcast what you're listening to so others can tune in and sync in real time.

Here is the GitHub Pages documentation: [https://bodln.github.io/stargzr/]

---

## Features

- **Media Streaming** — serves your local audio and video library over HTTP with full byte-range support, enabling seeking and browser caching
- **Video Support** — MP4 and WebM files play natively in the browser; uploaded MKV, MOV, AVI, and WebM files are automatically converted to H.264/AAC MP4 via ffmpeg before being added to the playlist
- **Subtitle Support** — subtitle streams are extracted from uploaded videos as WebVTT files and served alongside the video; the player loads them automatically and falls back silently if none exist
- **Session-based Playback** — each user gets their own independent playback position tracked via cookies
- **Live Radio Broadcasting** — start a broadcast and share your session ID; listeners sync to your playback in real time over WebSocket
- **Drift Correction** — listeners receive periodic heartbeats and automatically correct position drift to stay in sync with the broadcaster
- **AutoNext** — when a broadcaster's track ends naturally, listeners are notified to finish their current track then advance to the same next track, preserving the last few seconds instead of jumping immediately
- **Drag-and-Drop Playlist** — reorder tracks in the browser; custom order is saved to localStorage and persists across sessions; touch drag-and-drop supported on mobile
- **Media Type Filter** — playlist navigation can be filtered to All, Audio, Video, or Other; next/prev respect the active filter; direct play is never gated
- **Playlist Search** — filter tracks by filename with a live search bar; clicking a result scrolls the playlist to that track and briefly highlights it
- **Play Next** — queue any track to the position immediately after the current one directly from the playlist row
- **Other Files Tab** — non-media files and subdirectories in the music folder are listed separately; plain files stream directly to the browser's download manager; folders are zipped on demand (up to 2000 MB) and streamed without buffering in RAM
- **Folder Zip Download** — folders are zipped to a temp file on a blocking thread then streamed in chunks; the temp file is deleted by a RAII guard when the stream ends or the client disconnects; folders over 1500 MB show a "Too large" indicator and cannot be downloaded
- **Media Upload** — upload audio and video (only) files directly from the browser; video files are converted to browser-compatible H.264/AAC MP4 via ffmpeg; the server enforces a 5 GB total library cap and a 200 MB per-IP quota per server session; uploaded files are inserted into the live playlist in alphabetical order without a restart; only one video conversion runs at a time via a semaphore
- **Progress Bar** — a playback progress bar is shown while tuned into a broadcaster
- **Live Analytics** — active connections, broadcasters, and listener counts pushed to all clients in real time
- **Admin Panel** — on-page admin view showing all live sessions, their idle time, who each session is tuned to, and the full broadcaster-to-listener map
- **Auto-cleanup** — stale broadcaster sessions and player sessions are automatically evicted on the server
- **Rate Limiting** — heartbeat, broadcast update, and WebSocket upgrade endpoints are rate-limited per IP to prevent abuse
- **Per-IP WebSocket Rate Limiting** — WebSocket upgrade requests are limited per client IP; behind a reverse proxy requires `X-Real-IP` header forwarding
- **Session Validation** — expired sessions are detected on page load, media play, and WebSocket upgrade, triggering an automatic reload to issue a fresh session
- **Bluetooth Mode** — optional toggle that mutes media during seeks and song changes to prevent glitches on Bluetooth DSP pipelines; unmutes once the browser confirms media output has resumed
- **Graceful Shutdown** — on `SIGINT` or `SIGTERM` the server sends a `ServerShutdown` message to all connected WebSocket clients before draining HTTP connections, so listeners reconnect automatically after a restart rather than seeing a dead connection
- **Prometheus Metrics** — active connections, broadcasters, listeners, message rates, rate limit hits, session creation and cleanup counts exposed at `/stargzr/metrics`
- **Structured Tracing** — per-session spans propagate `session_id` through all log lines automatically; media streaming spans are nested under request spans;
- **Mobile Support** — Wake Lock API support to keep the screen on while broadcasting, mobile battery-aware reconnection, page visibility handling that resumes broadcasts after backgrounding

---

## Tech Stack

| Layer | Technology |
|---|---|
| Web framework | [Axum](https://github.com/tokio-rs/axum) |
| Async runtime | [Tokio](https://tokio.rs) |
| Templates | [Askama](https://github.com/djc/askama) |
| Frontend | Vanilla JS + CSS (split into separate static files served by tower-http) |
| Concurrent state | [DashMap](https://github.com/xacrimon/dashmap) |
| Real-time sync | WebSocket (`tokio::sync::broadcast`) |
| Session IDs | UUID v4 |
| Metrics | `metrics` + `metrics-exporter-prometheus` |
| Tracing | `tracing` + `tracing-subscriber` |
| Video conversion | ffmpeg (must be installed separately) |
| Zip archives | `zip` crate |

---

## Project Structure

```
src/
  player/
    mod.rs          — server init, router, TCP listener, playlist scanning
    types.rs        — AppState, RadioMessage, MediaInfo, BroadcastState
    handlers.rs     — HTTP route handlers (page, next, prev, stream, playlist, upload, download, zip, metrics, admin)
    radio.rs        — WebSocket lifecycle, radio protocol, analytics
    session.rs      — session helpers, timestamp utils, stale session cleanup
    templates.rs    — Askama template structs and IntoResponse impls
    error.rs        — PlayerError, PlayerResult
    validation.rs   — SessionId newtype, media index validation
    rate_limit.rs   — token bucket rate limiter (heartbeat, broadcast, WebSocket)
    metrics.rs      — Prometheus metrics registry and helper functions
    logging.rs      — tracing-subscriber initialization
    reconnect.rs    — reconnection logic with exponential backoff and jitter
    static/
      css/
        player.css        — all player styles
      js/
        utils.js          — debugLog, copySessionId
        playlist.js       — PlaylistManager class, folder/file download handlers
        radio-player.js   — RadioPlayer class
        analytics.js      — updateAnalyticsDisplay, tuneInToBroadcaster
        upload.js         — upload button handler
        admin.js          — admin panel polling
        search.js         — playlist search bar
        main.js           — media element switcher, audio setup, instantiation, event bindings
    templates/
      player.html         — full player page, references external CSS and JS
      player_controls.html — HTMX partial for control updates
  main.rs
music/                  — your media files go here
```

---

## Getting Started

### Prerequisites

- Docker Desktop (for Docker usage) OR Rust (for native build)
- A folder of audio/video files
- ffmpeg (required for video uploads; must be available on PATH)

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
3. Share your Session ID with someone else (all broadcaster IDs are shown in the live broadcasts list and people can tune in directly from there)
4. They paste it into the **Tune In** field and click **Tune In**, or click **Tune In** directly from the broadcaster card
5. Their playback syncs to yours — play, pause, seek, and track changes propagate in real time

Listeners receive an initial `Sync` on tune-in with latency compensation applied, then periodic `Heartbeat` messages for drift correction. When a track ends naturally an `AutoNext` message lets listeners finish their current track before advancing. If a broadcaster disconnects, all tuned listeners are notified automatically.

While tuned in, listeners have access to:
- **Resync** — immediately re-syncs to the broadcaster's current position
- **Mute/Unmute** — toggles audio without affecting the broadcast

---

## Uploading Media

Files can be added to the library at runtime without restarting the server.

1. Click **Upload Media** at the bottom of the player page
2. Select one or more audio or video files
3. The server validates the file type, checks the per-IP and total folder quotas, writes the file to disk, converts video files to MP4 via ffmpeg, and inserts it into the live playlist in alphabetical order

Upload limits (reset on server restart):

| Limit | Value |
|---|---|
| Total library size | 5 GB |
| Per-IP upload quota | 200 MB |
| Accepted audio formats | `.mp3`, `.m4a`, `.ogg`, `.wav`, `.flac` |
| Accepted video formats | `.mp4`, `.webm`, `.mkv`, `.mov`, `.avi` |

Video files are re-encoded to H.264/AAC stereo MP4 at CRF 23 with `+faststart` so playback begins before the full file is downloaded. Subtitle streams are extracted as WebVTT alongside the video. Only one video conversion runs at a time.

---

## Other Files

The **Other** tab lists non-media files and subdirectories in the music folder.

- **Plain files** — downloaded immediately via direct browser navigation; bytes stream straight to disk, no RAM buffering
- **Folders** — zipped on demand and streamed to the browser's download manager; the zip is written to a temp file on a blocking thread (never held in RAM) then streamed in chunks; the temp file is deleted automatically when the download completes or the client disconnects
- **Folders over 1500 MB** — shown with a "Too large" indicator; zip downloads are rejected with `413` before any work is done

---

## Monitoring

Prometheus metrics are exposed at `/stargzr/metrics` in standard text format.

To scrape with Prometheus, add the following to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'stargzr'
    scrape_interval: 15s
    static_configs:
      - targets: ['localhost:8083']
    metrics_path: '/stargzr/metrics'
```

Metrics available:

| Metric | Type | Description |
|---|---|---|
| `radio_active_connections` | Gauge | Current live WebSocket connections |
| `radio_active_broadcasters` | Gauge | Current active broadcasters |
| `radio_active_listeners` | Gauge | Current tuned-in listeners |
| `radio_ws_connections_total` | Counter | Total WebSocket upgrades accepted |
| `radio_ws_rejected_total` | Counter | Total WebSocket upgrades rejected |
| `radio_messages_total{type}` | Counter | Messages handled, labeled by type |
| `radio_rate_limit_hits_total{limiter}` | Counter | Rate limit rejections by limiter |
| `radio_abrupt_disconnects_total` | Counter | Clients that dropped without TuneOut |
| `player_sessions_created_total` | Counter | New player sessions created |
| `player_sessions_cleaned_total` | Counter | Stale sessions removed by cleanup |

Visualize in Grafana by connecting it to your Prometheus instance and using PromQL queries such as `rate(radio_messages_total[1m])` labeled by `{{type}}` for a per-message-type rate graph.

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

Create a Caddy configuration file named `Caddyfile` (mine is in the root of the repo):

```
stargzr.jumpingcrab.com {
    handle /stargzr* {
        reverse_proxy 127.0.0.1:8083 {
            # Required for per-IP rate limiting to work correctly.
            # Without this, all requests appear to come from 127.0.0.1.
            header_up X-Real-IP {remote_host}
        }
    }
}
```

> **Note:** The `header_up X-Real-IP` directive is required for WebSocket per-IP rate limiting to work. Without it, every client appears to come from `127.0.0.1` and the rate limiter treats all connections as the same source.

Run Caddy from the directory containing your `Caddyfile`:

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

The Docker image is built using a two-stage build. The builder stage compiles a fully static binary targeting `x86_64-unknown-linux-musl` using `musl-tools`, which eliminates any dependency on the host system's glibc version. The runtime stage is Alpine Linux with `ca-certificates` and `ffmpeg` added. Static CSS and JS files are copied from the builder into the final image at the path baked in by `CARGO_MANIFEST_DIR` at compile time.

My ffmpeg command:

```
ffmpeg -i "\stargzr\music\A.Haunting.in.Venice.2023.1080p.AMZN.WEBRip.1400MB.DD5.1.x264-GalaxyRG.mkv" -map 0:v:0 -map 0:a:0 -c:v copy -c:a aac -b:a 192k -ac 2 -movflags +faststart "\stargzr\music\haunting.mp4" -map 0:s:0 -c:s webvtt "\stargzr\music\haunting.vtt"
```

---

<img width="2509" height="2623" alt="image" src="https://github.com/user-attachments/assets/3a8c56b3-10e2-4d54-82c3-827b637559fd" />

---

## License

MIT
