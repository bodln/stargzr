class RadioPlayer {
  constructor(audioElement, sessionId) {
    this.audio = audioElement;
    this.sessionId = sessionId;
    this.mode = "private";
    this.tunedBroadcaster = null;
    this.ws = null;
    this.heartbeatInterval = null;
    this.targetTime = 0;
    this.isBroadcasting = false;
    this.isStartingBroadcast = false;
    this.wakeLock = null;
    this.pageHiddenAt = null;

    // Queued next media received from AutoNext while listener's current track
    // is still playing. Cleared once the local 'ended' event fires.
    this.pendingAutoNextIndex = null;
    this.pendingAutoNextTime = 0;

    // Guards against duplicate canplay handlers when the same media-change
    // produces more than one Sync before the browser fires canplay.
    this._loadingMediaIndex = null;
    this._pendingSeekTime = 0;
    this._pendingIsPlaying = false;
    // Date.now() when _pendingSeekTime was last set, so the canplay handler can
    // add the media load/buffer time back onto the seek target.
    this._pendingSeekSetAt = 0;

    // Set to Date.now() when TuneIn is sent; cleared after the first Sync.
    // Used to compensate for server-to-listener transit on initial sync.
    this._tuneInSentAt = null;

    // Reconnection state
    this.reconnectAttempts = 0;
    this.maxReconnectAttempts = 10;
    this.baseReconnectDelay = 500;
    this.maxReconnectDelay = 30000;
    this.reconnectTimer = null;
    this.isIntentionalDisconnect = false;

    // Event listener tracking for cleanup
    this.boundHandlers = new Map();
    this.audioEventHandlers = new Map();

    this.connectionState = "disconnected";

    // Debounce timer for incoming Sync messages
    this.syncTimer = null;

    // Whether the listener has manually muted their own audio
    this.isMuted = false;

    // ── Perfect Sync mode ──────────────────────────────────────────────
    // Opt in, persisted like Bluetooth mode. When on, the listener locks its
    // playback clock to the broadcaster's using an estimated client<->server
    // clock offset plus the per frame transit age, and trims residual drift
    // with tiny playbackRate nudges instead of audible seeks. Everything the
    // normal path does still applies when this is off.
    this.perfectSync = localStorage.getItem("perfect_sync") === "true";
    // serverClock ≈ Date.now() + clockOffset. 0 until the first probe lands.
    this.clockOffset = 0;
    this.clockReady = false;
    this.clockRtt = null;
    this._clockSamples = [];
    this._clockProbeTimer = null;
    this._clockRefreshTimer = null;
    // Timer that returns playbackRate to 1.0 after a nudge.
    this._rateRestoreTimer = null;
    // Last authoritative position frame (Sync or PerfectPositionSync) for the
    // broadcaster we are tuned to. Used to lock on right after a media switch.
    this._lastPosMsg = null;

    debugLog(`Initialized with session ID: ${sessionId}`);
    this.setupPageVisibilityHandling();
  }

  // ── Perfect Sync helpers ─────────────────────────────────────────────

  // Manual fine tune (milliseconds) the user can dial in by ear, mainly for
  // Bluetooth output latency which cannot be measured from JS. Positive means
  // "my audio comes out late", so we aim further ahead in the track.
  _perfectOffsetSec() {
    const v = parseInt(localStorage.getItem("perfect_sync_offset_ms"), 10);
    return Number.isFinite(v) ? v / 1000 : 0;
  }

  // Best estimate of the server clock right now.
  serverNow() {
    return Date.now() + this.clockOffset;
  }

  // Where the broadcaster's playhead is at this instant, from a position frame.
  // playback_time already carries the broadcaster->server leg (server side),
  // and serverNow() - server_timestamp_ms is the server->listener leg plus
  // whatever time has passed since the frame was stamped.
  _livePos(msg) {
    const base = msg.playback_time;
    if (!msg.is_playing) return base;
    // Without a clock estimate, serverNow() is on the local clock while
    // server_timestamp_ms is on the server's, so the difference is meaningless.
    // Fall back to the stamped position and let the next frame, once the clock
    // is ready, pull us the rest of the way.
    if (!this.clockReady) return base;
    const ageSec = Math.max(0, (this.serverNow() - Number(msg.server_timestamp_ms)) / 1000);
    // A wildly large age means a bad sample slipped through; clamp so we never
    // launch playback minutes deep into a track.
    return base + Math.min(ageSec, 30);
  }

  // NTP style clock probe burst. Sends `count` ClockProbe frames spaced `gapMs`
  // apart; handleRadioMessage feeds the echoes back into _recordClockSample.
  // Safe to call repeatedly, it just refreshes the estimate.
  syncClock(count = 6, gapMs = 150) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    clearInterval(this._clockProbeTimer);
    let sent = 0;
    const fire = () => {
      if (
        sent >= count ||
        !this.ws ||
        this.ws.readyState !== WebSocket.OPEN
      ) {
        clearInterval(this._clockProbeTimer);
        this._clockProbeTimer = null;
        return;
      }
      sent++;
      this.ws.send(
        JSON.stringify({ type: "ClockProbe", client_ts: Date.now() }),
      );
    };
    fire();
    this._clockProbeTimer = setInterval(fire, gapMs);
  }

  // One ClockEcho worth of data. Keeps a small window of samples and adopts the
  // offset from the lowest RTT one, which is the least jitter-contaminated.
  _recordClockSample(clientTs, serverTs) {
    const now = Date.now();
    const rtt = now - clientTs;
    if (rtt < 0 || rtt > 5000) return; // nonsense sample, drop it
    // Server time at the midpoint of the round trip maps to now.
    const offset = serverTs + rtt / 2 - now;
    this._clockSamples.push({ rtt, offset });
    if (this._clockSamples.length > 12) this._clockSamples.shift();

    const best = this._clockSamples.reduce((a, b) => (b.rtt < a.rtt ? b : a));
    this.clockOffset = best.offset;
    this.clockRtt = best.rtt;
    this.clockReady = true;
    debugLog(
      `Clock sample rtt=${rtt}ms offset=${offset.toFixed(1)}ms (using rtt=${best.rtt}ms offset=${best.offset.toFixed(1)}ms)`,
    );
  }

  // Start Perfect Sync bookkeeping for a tune in: prime the clock estimate and
  // keep refreshing it so it tracks slow drift between the two machines.
  _startPerfectSync() {
    this.syncClock();
    clearInterval(this._clockRefreshTimer);
    this._clockRefreshTimer = setInterval(() => this.syncClock(3, 200), 20000);
  }

  // Tear down Perfect Sync timers and undo any rate nudge. Called on tune out
  // and disconnect so a normal-mode session never inherits a stretched clock.
  _stopPerfectSync() {
    clearInterval(this._clockProbeTimer);
    clearInterval(this._clockRefreshTimer);
    clearTimeout(this._rateRestoreTimer);
    this._clockProbeTimer = null;
    this._clockRefreshTimer = null;
    this._rateRestoreTimer = null;
    this._clockSamples = [];
    this.clockReady = false;
    this._lastPosMsg = null;
    if (this.audio) this.audio.playbackRate = 1.0;
  }

  // Pull the listener onto the broadcaster's playhead from a position frame.
  // Large gap: one quick seek (muted under Bluetooth mode). Small gap: a brief
  // sub-6% playbackRate change that eases back into phase inaudibly. Tiny gap:
  // leave it alone. Only runs while both sides are playing the same media.
  _perfectLockOn(msg) {
    if (this.mode !== "radio" || !this.perfectSync) return;
    if (this._loadingMediaIndex !== null) return;
    if (msg.media_index !== this.getCurrentMediaIndex()) return;

    const off = this._perfectOffsetSec();

    if (!msg.is_playing) {
      if (!this.audio.paused) this.audio.pause();
      this.audio.playbackRate = 1.0;
      const want = msg.playback_time + off;
      if (Math.abs(this.audio.currentTime - want) > 0.25) this.audio.currentTime = want;
      return;
    }

    const expected = this._livePos(msg) + off;

    if (this.audio.paused) {
      this.audio.currentTime = expected;
      this.audio.playbackRate = 1.0;
      this._radioPlay();
      return;
    }

    // No clock estimate yet: don't chase drift off a position we can't trust,
    // just keep playing. The next frame after the clock settles corrects us.
    if (!this.clockReady) {
      this.audio.playbackRate = 1.0;
      return;
    }

    const err = this.audio.currentTime - expected; // >0 => listener is ahead
    const drift = Math.abs(err);

    if (drift > 0.6) {
      // Too far off to fix smoothly. Snap, muting the blip under Bluetooth mode.
      const btOn = document.getElementById("bt-mode-toggle")?.checked;
      if (btOn && !this.isMuted) this.audio.muted = true;
      this.audio.currentTime = expected + 0.05;
      this.audio.playbackRate = 1.0;
      if (btOn && !this.isMuted)
        setTimeout(() => {
          if (!this.isMuted) this.audio.muted = false;
        }, 250);
      debugLog(`Perfect Sync hard snap, drift was ${err.toFixed(3)}s`);
    } else if (drift > 0.012) {
      // Ease back over roughly a second. Negative err (we are behind) speeds up.
      const rate = Math.min(1.06, Math.max(0.94, 1 - err / 1.1));
      this.audio.playbackRate = rate;
      clearTimeout(this._rateRestoreTimer);
      this._rateRestoreTimer = setTimeout(() => {
        this.audio.playbackRate = 1.0;
      }, 1200);
      debugLog(`Perfect Sync nudge rate=${rate.toFixed(3)} drift=${err.toFixed(3)}s`);
    } else {
      this.audio.playbackRate = 1.0;
      clearTimeout(this._rateRestoreTimer);
      this._rateRestoreTimer = null;
    }
  }

  isMobile() {
    return /Android|webOS|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(
      navigator.userAgent,
    );
  }

  setupPageVisibilityHandling() {
    let visibilityProp = "hidden";
    let visibilityEvent = "visibilitychange";
    if (typeof document.webkitHidden !== "undefined") {
      visibilityProp = "webkitHidden";
      visibilityEvent = "webkitvisibilitychange";
    }
    document.addEventListener(visibilityEvent, () => {
      if (document[visibilityProp]) {
        debugLog("📱 Page hidden - broadcasting may be affected on mobile");
        this.onPageHidden();
      } else {
        debugLog("👁️ Page visible again");
        this.onPageVisible();
      }
    });
  }

  onPageHidden() {
    this.pageHiddenAt = Date.now();
    if (this.isBroadcasting) {
      debugLog("📱 Page backgrounded - JavaScript will be throttled");
      this.releaseWakeLock();
    }
  }

  onPageVisible() {
    const hiddenDuration = this.pageHiddenAt
      ? (Date.now() - this.pageHiddenAt) / 1000
      : 0;
    this.pageHiddenAt = null;
    debugLog(
      `👁️ Page visible again (was hidden for ${hiddenDuration.toFixed(1)}s)`,
    );

    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      debugLog("WebSocket disconnected, reconnecting...");
      this.connectWebSocket();
    }

    if (this.isBroadcasting) {
      if (hiddenDuration < 40) {
        debugLog("✓ Resuming broadcast (short background duration)");
        this.requestWakeLock();
        if (this.ws && this.ws.readyState === WebSocket.OPEN)
          this.sendHeartbeat();
      } else {
        debugLog("⚠️ Long background duration - verifying broadcast state");
        this.verifyBroadcastState();
      }
    }
  }

  async requestWakeLock() {
    if (!("wakeLock" in navigator)) {
      debugLog("Wake Lock API not supported");
      return;
    }
    try {
      this.wakeLock = await navigator.wakeLock.request("screen");
      debugLog("✓ Wake lock acquired - screen will stay on");
      this.wakeLock.addEventListener("release", () =>
        debugLog("Wake lock released"),
      );
    } catch (err) {
      debugLog(`Wake lock error: ${err.message}`);
    }
  }

  async releaseWakeLock() {
    if (!this.wakeLock) return;
    try {
      await this.wakeLock.release();
      this.wakeLock = null;
      debugLog("Wake lock released");
    } catch (err) {
      debugLog(`Wake lock release error: ${err.message}`);
    }
  }

  async verifyBroadcastState() {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    debugLog("Querying broadcast state from server");
    this.ws.send(
      JSON.stringify({
        type: "QueryBroadcastState",
        session_id: this.sessionId,
      }),
    );
  }

  getReconnectDelay() {
    const exponential =
      this.baseReconnectDelay * Math.pow(2, this.reconnectAttempts);
    const jitter = exponential * 0.2 * (Math.random() - 0.5);
    return Math.min(
      Math.max(this.baseReconnectDelay, exponential + jitter),
      this.maxReconnectDelay,
    );
  }

  updateConnectionState(newState) {
    this.connectionState = newState;
    const states = {
      connected: { icon: "🟢", text: "Connected", color: "#28a745" },
      connecting: { icon: "🟡", text: "Connecting...", color: "#ffc107" },
      disconnected: { icon: "⚪", text: "Disconnected", color: "#6c757d" },
      error: { icon: "🔴", text: "Connection Error", color: "#dc3545" },
    };
    const cfg = states[newState] ?? states.disconnected;
    document.getElementById("connection-indicator").textContent = cfg.icon;
    const text = document.getElementById("connection-text");
    text.textContent = cfg.text;
    text.style.color = cfg.color;

    document.dispatchEvent(
      new CustomEvent("connectionStateChange", {
        detail: {
          state: newState,
          attempt: this.reconnectAttempts,
          maxAttempts: this.maxReconnectAttempts,
        },
      }),
    );
  }

  connectWebSocket() {
    if (
      this.ws &&
      (this.ws.readyState === WebSocket.OPEN ||
        this.ws.readyState === WebSocket.CONNECTING)
    ) {
      debugLog(
        `WebSocket already ${this.ws.readyState === WebSocket.OPEN ? "connected" : "connecting"}, skipping...`,
      );
      return;
    }
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }

    this.updateConnectionState("connecting");

    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const wsUrl = `${protocol}//${window.location.host}/stargzr/player/radio`;
    debugLog(
      `Connecting to WebSocket: ${wsUrl} (attempt ${this.reconnectAttempts + 1})`,
    );

    try {
      this.ws = new WebSocket(wsUrl);
    } catch (error) {
      debugLog(`Failed to create WebSocket: ${error.message}`);
      this.scheduleReconnect();
      return;
    }

    const onOpen = () => {
      debugLog("✓ WebSocket connected");
      this.reconnectAttempts = 0;
      this.updateConnectionState("connected");
      if (this.isBroadcasting) {
        debugLog("Client thinks it's broadcasting - verifying with server");
        this.verifyBroadcastState();
      }
      if (this.tunedBroadcaster) {
        debugLog(`Re-tuning to ${this.tunedBroadcaster} after reconnection`);
        this.tuneIn(this.tunedBroadcaster);
      }
    };

    const onMessage = (event) => {
      debugLog(`Received: ${event.data}`);
      try {
        this.handleRadioMessage(JSON.parse(event.data));
      } catch (error) {
        debugLog(`Failed to parse message: ${error.message}`);
      }
    };

    const onError = () => {
      debugLog("✗ WebSocket error");
      this.updateConnectionState("error");
    };

    const onClose = async (event) => {
      debugLog(
        `WebSocket closed (code: ${event.code}, clean: ${event.wasClean})`,
      );
      this.updateConnectionState("disconnected");
      this.removeWebSocketHandlers();

      if (event.code === 1006 && !this.isIntentionalDisconnect) {
        try {
          const resp = await fetch("/stargzr/player/session/check");
          if (resp.status === 401) {
            debugLog("Session expired on WS connect, reloading...");
            window.location.reload();
            return;
          }
        } catch (_) {}
      }

      if (!this.isIntentionalDisconnect) this.scheduleReconnect();
      else {
        debugLog("Intentional disconnect, not reconnecting");
        this.isIntentionalDisconnect = false;
      }
    };

    this.boundHandlers.set("open", onOpen);
    this.boundHandlers.set("message", onMessage);
    this.boundHandlers.set("error", onError);
    this.boundHandlers.set("close", onClose);

    this.ws.addEventListener("open", onOpen);
    this.ws.addEventListener("message", onMessage);
    this.ws.addEventListener("error", onError);
    this.ws.addEventListener("close", onClose);
  }

  scheduleReconnect() {
    if (this.reconnectAttempts >= this.maxReconnectAttempts) {
      debugLog(
        `Max reconnection attempts (${this.maxReconnectAttempts}) reached`,
      );
      this.updateConnectionState("error");
      alert(
        "Unable to connect to server after multiple attempts. Please refresh the page.",
      );
      return;
    }
    const delay = this.getReconnectDelay();
    debugLog(
      `Scheduling reconnection in ${delay}ms (attempt ${this.reconnectAttempts + 1}/${this.maxReconnectAttempts})`,
    );
    this.reconnectTimer = setTimeout(() => {
      this.reconnectAttempts++;
      this.connectWebSocket();
    }, delay);
  }

  removeWebSocketHandlers() {
    if (!this.ws) return;
    this.boundHandlers.forEach((handler, event) =>
      this.ws.removeEventListener(event, handler),
    );
    this.boundHandlers.clear();
  }

  disconnect() {
    this.isIntentionalDisconnect = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this._stopPerfectSync();
    if (this.isBroadcasting) this.stopBroadcasting();

    if (this.mode === "radio" && this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: "TuneOut" }));
    }
    if (this.ws) {
      this.removeWebSocketHandlers();
      if (
        this.ws.readyState === WebSocket.OPEN ||
        this.ws.readyState === WebSocket.CONNECTING
      )
        this.ws.close();
      this.ws = null;
    }
    this.updateConnectionState("disconnected");
    debugLog("Disconnected");
  }

  handleRadioMessage(msg) {
    if (msg.type === "Sync" && msg.broadcaster_id === this.sessionId) return;
    if (
      msg.type === "PerfectPositionSync" &&
      msg.broadcaster_id === this.sessionId
    )
      return;

    // Bounce server_ts straight back so the server can time the round trip.
    // We do no math here, the server owns the measurement.
    if (msg.type === "Ping") {
      if (this.ws?.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify({ type: "Pong", server_ts: msg.server_ts }));
      }
      return;
    }

    // Clock probe echo. Only meaningful in Perfect Sync mode; feed it to the
    // offset estimator and stop.
    if (msg.type === "ClockEcho") {
      this._recordClockSample(msg.client_ts, msg.server_ts);
      return;
    }

    // Fresh authoritative position, sent every heartbeat while in Perfect Sync.
    // Normal-mode listeners ignore it and keep riding the sparse Syncs.
    if (msg.type === "PerfectPositionSync") {
      if (
        this.perfectSync &&
        this.mode === "radio" &&
        msg.broadcaster_id === this.tunedBroadcaster
      ) {
        this._lastPosMsg = msg;
        this._perfectLockOn(msg);
      }
      return;
    }

    debugLog(`Handling message type: ${msg.type}, mode: ${this.mode}`);

    if (msg.type === "Analytics") {
      updateAnalyticsDisplay(msg);
      return;
    }

    // Chat works in every mode, the server already routed it to the right room
    // so we just hand it to the chat panel to paint.
    if (msg.type === "Chat") {
      window.chatUI?.onMessage(msg);
      return;
    }

    if (msg.type === "BroadcastStateResponse") {
      debugLog(
        `Server says broadcasting: ${msg.is_broadcasting}, client thinks: ${this.isBroadcasting}`,
      );
      if (msg.is_broadcasting !== this.isBroadcasting) {
        debugLog("⚠️ State mismatch detected!");
        if (msg.is_broadcasting && !this.isBroadcasting) {
          this.isBroadcasting = true;
          this.updateBroadcastingUI(true);
          this._notifyRoomChange();
          if (this.isMobile())
            document
              .getElementById("mobile-warning")
              .classList.remove("hidden");
        } else if (!msg.is_broadcasting && this.isBroadcasting) {
          debugLog(
            "🔄 Server lost our session - resuming broadcast automatically",
          );
          const initMsg = {
            type: "StartBroadcasting",
            broadcaster_id: this.sessionId,
            media_index: this.getCurrentMediaIndex(),
            playback_time: this.audio.currentTime,
            is_playing: !this.audio.paused,
          };
          debugLog(
            `Resuming broadcast: media ${initMsg.media_index}, time ${initMsg.playback_time.toFixed(2)}s`,
          );
          if (this.ws?.readyState === WebSocket.OPEN)
            this.ws.send(JSON.stringify(initMsg));
        }
      }
      return;
    }

    if (msg.type === "Error") {
      debugLog(`Server error: ${msg.message}`);
      if (
        msg.message.includes("BroadcasterNotFound") ||
        msg.message.includes("not broadcasting")
      ) {
        if (this.isBroadcasting) {
          this.stopBroadcasting();
          alert(
            "Your broadcast session ended. Please start broadcasting again if needed.",
          );
        }
      } else {
        alert(`Error: ${msg.message}. Please try and refresh the page.`);
      }
      return;
    }

    if (msg.type === "BroadcasterOffline") {
      if (this.tunedBroadcaster === msg.broadcaster_id) {
        debugLog(`Broadcaster ${msg.broadcaster_id} went offline`);
        this.tuneOut("broadcaster_offline");
        alert(
          "The broadcaster you were listening to has stopped broadcasting.",
        );
      }
      return;
    }

    if (msg.type === "BroadcasterOnline") {
      if (this.tunedBroadcaster === msg.broadcaster_id)
        debugLog(`User ${msg.broadcaster_id} is now broadcasting`);
      return;
    }

    if (msg.type === "AutoNext") {
      if (this.mode !== "radio") return;

      const idx = msg.next_media_index;

      // Already on (or past) the track AutoNext points at, nothing to queue.
      // A later Sync keeps us aligned from here.
      if (this.getCurrentMediaIndex() === idx) {
        this.pendingAutoNextIndex = null;
        this.pendingAutoNextTime = 0;
        return;
      }

      this.pendingAutoNextIndex = idx;
      this.pendingAutoNextTime = 0;

      // Listeners run slightly ahead of the broadcaster (tune in latency plus
      // the server side latency added to every Sync), so our local track has
      // usually already ended by the time AutoNext reaches us. When it has, the
      // 'ended' event has fired and will not fire again, so waiting on it would
      // strand us on the finished track. Switch right now instead.
      const finished =
        this.audio.ended ||
        (isFinite(this.audio.duration) &&
          this.audio.duration > 0 &&
          this.audio.currentTime >= this.audio.duration - 0.25);

      if (finished) {
        debugLog(
          `AutoNext received after local track already ended, switching to media ${idx} now`,
        );
        this._switchToAutoNext(idx, this.pendingAutoNextTime);
      } else {
        debugLog(
          `AutoNext received: queuing media index ${idx} for when the current track ends`,
        );
        this.audio.addEventListener(
          "ended",
          () => {
            if (this.pendingAutoNextIndex === null) return;
            this._switchToAutoNext(
              this.pendingAutoNextIndex,
              this.pendingAutoNextTime,
            );
          },
          { once: true },
        );
      }
      return;
    }

    if (msg.type === "ServerShutdown") {
      debugLog(`Server shutting down: ${msg.message}`);
      const text = document.getElementById("connection-text");
      text.textContent = msg.message;
      text.style.color = "#ffc107";
      return;
    }

    if (this.mode !== "radio") return;

    if (msg.type === "Sync") {
      if (this.perfectSync && msg.broadcaster_id === this.tunedBroadcaster)
        this._lastPosMsg = msg;
      clearTimeout(this.syncTimer);
      this.syncTimer = setTimeout(() => this.syncToBroadcaster(msg), 80);
    }
  }

  syncToBroadcaster(msg) {
    let { media_index, playback_time, is_playing } = msg;

    // Perfect Sync owns position via the clock offset and the per frame age, so
    // it uses none of the coarse compensations below.
    const perfect =
      this.perfectSync &&
      this.mode === "radio" &&
      msg.broadcaster_id === this.tunedBroadcaster;

    // First Sync after TuneIn: compensate for server to listener transit time.
    // Date.now() - _tuneInSentAt is a whole round trip, TuneIn out and Sync back,
    // and we only want the return leg, so take half of it.
    if (this._tuneInSentAt !== null) {
      if (perfect) {
        this._tuneInSentAt = null;
      } else {
        const elapsed = (Date.now() - this._tuneInSentAt) / 1000 / 2;
        this._tuneInSentAt = null;
        const duration = isFinite(this.audio.duration)
          ? this.audio.duration
          : Infinity;
        const adjusted = playback_time + elapsed;
        if (adjusted < duration) playback_time = adjusted;
        debugLog(
          `TuneIn latency compensation: +${elapsed.toFixed(3)}s to ${playback_time.toFixed(2)}s`,
        );
      }
    }

    // Broadcaster is still on the pending AutoNext media: update position only
    if (
      this.pendingAutoNextIndex !== null &&
      msg.media_index === this.pendingAutoNextIndex
    ) {
      this.pendingAutoNextTime = playback_time;
      debugLog(`AutoNext position updated to ${playback_time.toFixed(2)}s`);
      return;
    }

    // Any manual broadcaster action clears the pending AutoNext
    if (
      this.pendingAutoNextIndex !== null &&
      msg.media_index !== this.pendingAutoNextIndex
    ) {
      debugLog(
        `Manual broadcaster action cleared pendingAutoNextIndex (was ${this.pendingAutoNextIndex})`,
      );
      this.pendingAutoNextIndex = null;
    }

    const currentIndex = this.getCurrentMediaIndex();

    if (
      currentIndex !== media_index ||
      this._loadingMediaIndex === media_index
    ) {
      this._pendingSeekTime = playback_time;
      this._pendingIsPlaying = is_playing;
      this._pendingSeekSetAt = Date.now();

      if (this._loadingMediaIndex !== media_index) {
        debugLog(
          `Switching from media ${currentIndex} to media ${media_index}`,
        );
        this._loadingMediaIndex = media_index;

        const mediaId =
          window.playlistManager?.getMediaIdByServerIndex(media_index) ?? null;
        const nextMedia = mediaId
          ? window.playlistManager?.getMediaById(mediaId)
          : window.playlistManager?.originalMedias[media_index];

        // Switch media element before loading so the browser targets the right one
        window.switchMediaElement?.(nextMedia?.media_type === "video");

        this.audio.src = mediaId
          ? `/stargzr/player/stream/id/${mediaId}`
          : `/stargzr/player/stream/${media_index}`;
        this._updateSubtitleTrack(mediaId, nextMedia?.media_type === "video");
        this.audio.load();

        this.audio.addEventListener(
          "canplay",
          () => {
            this._loadingMediaIndex = null;

            // Perfect Sync: re-derive the live playhead now, at canplay time,
            // from the freshest position frame we hold, then aim a hair ahead
            // so playback that starts a few ms late still lands in phase. The
            // opening moments may be clipped, which Perfect Sync accepts.
            if (perfect) {
              const ref =
                this._lastPosMsg &&
                this._lastPosMsg.media_index === media_index
                  ? this._lastPosMsg
                  : msg;
              const dur = isFinite(this.audio.duration)
                ? this.audio.duration
                : Infinity;
              let target = this._pendingIsPlaying
                ? this._livePos(ref) + 0.15 + this._perfectOffsetSec()
                : ref.playback_time + this._perfectOffsetSec();
              if (!(target < dur)) target = Math.max(0, dur - 0.1);
              this.audio.currentTime = Math.max(0, target);
              debugLog(
                `Perfect Sync media switch, starting at ${this.audio.currentTime.toFixed(3)}s`,
              );
              if (this._pendingIsPlaying) this._radioPlay();
              // Tighten up once the pipeline has settled and again on the next
              // heartbeat frame.
              setTimeout(() => this._perfectLockOn(this._lastPosMsg || ref), 350);
              return;
            }

            let seekTo =
              this._pendingSeekTime < 1.0 ? 0 : this._pendingSeekTime;

            // Loading a long file can take a few seconds. If the broadcaster is
            // playing, that time passed for them too, so add it to the seek target
            // or the listener starts out that far behind.
            if (this._pendingIsPlaying && this._pendingSeekSetAt && seekTo > 0) {
              const loadElapsed = (Date.now() - this._pendingSeekSetAt) / 1000;
              const dur = isFinite(this.audio.duration)
                ? this.audio.duration
                : Infinity;
              if (seekTo + loadElapsed < dur) seekTo += loadElapsed;
            }
            this._pendingSeekSetAt = 0;

            debugLog(
              `canplay, seeking to ${seekTo.toFixed(2)}s (broadcaster at ${this._pendingSeekTime.toFixed(2)}s)`,
            );
            this.audio.currentTime = seekTo;
            if (this._pendingIsPlaying) this._radioPlay();
          },
          { once: true },
        );
      }
    } else {
      this._loadingMediaIndex = null;

      // Perfect Sync: no blunt currentTime assignment. Let the lock-on routine
      // decide between a nudge and a snap so a routine Sync never jolts audio.
      if (perfect) {
        this._perfectLockOn(msg);
        return;
      }

      this.audio.currentTime = playback_time;
      if (is_playing) this._radioPlay();
      if (!is_playing && !this.audio.paused) this.audio.pause();
    }
  }

  // Loads the media an AutoNext pointed at and starts playback from seekTo.
  // Shared by both AutoNext paths: the local track already ended before the
  // message arrived, or it ended afterwards and this runs from the 'ended'
  // handler. Clears the pending state so a stale 'ended' listener that fires
  // later is a no-op.
  _switchToAutoNext(idx, seekTo) {
    this.pendingAutoNextIndex = null;
    this.pendingAutoNextTime = 0;

    const mediaId = window.playlistManager?.getMediaIdByServerIndex(idx) ?? null;
    const nextMedia = mediaId
      ? window.playlistManager?.getMediaById(mediaId)
      : window.playlistManager?.originalMedias[idx];

    // Switch to the correct element before loading the next media
    window.switchMediaElement?.(nextMedia?.media_type === "video");

    this.audio.src = mediaId
      ? `/stargzr/player/stream/id/${mediaId}`
      : `/stargzr/player/stream/${idx}`;
    this._updateSubtitleTrack(mediaId, nextMedia?.media_type === "video");
    this.audio.load();

    const perfect =
      this.perfectSync &&
      this.mode === "radio" &&
      this.tunedBroadcaster != null;

    this.audio.addEventListener(
      "canplay",
      () => {
        // Perfect Sync: if we already have a position frame for the new track,
        // lock straight onto it; otherwise start near the top and let the next
        // heartbeat frame lock us in. Same "may clip the intro" trade as a seek.
        if (perfect) {
          const ref =
            this._lastPosMsg && this._lastPosMsg.media_index === idx
              ? this._lastPosMsg
              : null;
          const start = ref
            ? Math.max(0, this._livePos(ref) + 0.15 + this._perfectOffsetSec())
            : Math.max(0, seekTo);
          this.audio.currentTime = start;
          this._radioPlay();
          if (ref) setTimeout(() => this._perfectLockOn(this._lastPosMsg || ref), 350);
          debugLog(
            `AutoNext (Perfect Sync): media ${idx} starting at ${start.toFixed(3)}s`,
          );
          return;
        }
        this.audio.currentTime = seekTo;
        this._radioPlay();
      },
      { once: true },
    );

    debugLog(
      `AutoNext: switched to media index ${idx} at ${seekTo.toFixed(2)}s`,
    );
  }

  tuneIn(broadcasterId) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      debugLog("WebSocket not ready, connecting...");
      this.connectWebSocket();
      setTimeout(() => this.tuneIn(broadcasterId), 500);
      return;
    }

    if (this.tunedBroadcaster && this.tunedBroadcaster !== broadcasterId) {
      debugLog(`Switching from ${this.tunedBroadcaster} to ${broadcasterId}`);
      this.ws.send(JSON.stringify({ type: "TuneOut" }));
    }

    // Snapshot private playback state before entering radio mode for the first time
    if (this.mode !== "radio") {
      const snapMediaId = window.playlistManager?.currentMediaId ?? null;
      const snapMedia = snapMediaId
        ? window.playlistManager?.getMediaById(snapMediaId)
        : null;
      this.preRadioSnapshot = {
        src:
          this.audio.src ||
          this.audio.querySelector?.("source")?.getAttribute("src") ||
          null,
        currentTime: this.audio.currentTime,
        paused: this.audio.paused,
        mediaId: snapMediaId,
        // Remember whether the pre-radio media was video so tuneOut can restore the right element
        isVideo: snapMedia?.media_type === "video",
      };
      debugLog(
        `Saved pre-radio state: ${this.preRadioSnapshot.src} @ ${this.preRadioSnapshot.currentTime.toFixed(2)}s`,
      );

      // Show listener controls when first entering radio mode
      document
        .getElementById("radio-listener-controls")
        ?.classList.remove("hidden");

      // Unmute on tune in whatever mute state the user had in private mode
      // does not carry over into radio mode. The mute button starts fresh.
      this.isMuted = false;
      const allMediaEls = [
        document.getElementById("audio-player"),
        document.getElementById("video-player"),
      ].filter(Boolean);
      allMediaEls.forEach((el) => (el.muted = false));
      const muteBtn = document.getElementById("mute-btn");
      if (muteBtn) muteBtn.textContent = "🔇 Mute";

      // Move the resync button next to whichever element is active and show it
      const resyncBtn = document.getElementById("media-resync-btn");
      if (resyncBtn) {
        window._activeMedia?.after(resyncBtn);
        resyncBtn.classList.remove("hidden");
      }
    }

    this.mode = "radio";
    this.tunedBroadcaster = broadcasterId;
    this._tuneInSentAt = Date.now();
    this._lastPosMsg = null;

    // Perfect Sync: prime the clock estimate before the first Sync lands and
    // keep it refreshed for the life of the tune in.
    if (this.perfectSync) this._startPerfectSync();

    debugLog(`Sending TuneIn: ${broadcasterId}`);
    this.ws.send(
      JSON.stringify({ type: "TuneIn", broadcaster_id: broadcasterId }),
    );

    this.audio.controls = false;
    document.getElementById("broadcast-progress").classList.remove("hidden");
    window.playlistManager?.render();
    this._notifyRoomChange();
  }

  tuneOut(reason = "manual") {
    debugLog(`Leaving radio mode: ${reason}`);
    if (this.ws?.readyState === WebSocket.OPEN)
      this.ws.send(JSON.stringify({ type: "TuneOut" }));

    this.mode = "private";
    this.tunedBroadcaster = null;
    this.pendingAutoNextIndex = null;
    this.pendingAutoNextTime = 0;

    // Stop Perfect Sync timers and restore normal playback rate.
    this._stopPerfectSync();

    document.getElementById("mode-display").textContent = "Private Mode";
    document.getElementById("mode-display").className = "mode-badge private";
    document.getElementById("broadcaster-info").classList.add("hidden");
    document.getElementById("tune-out-btn").classList.add("hidden");
    document.getElementById("tune-in-btn").classList.remove("hidden");
    document.getElementById("prev-btn").disabled = false;
    document.getElementById("next-btn").disabled = false;

    document.getElementById("broadcast-progress").classList.add("hidden");
    document.getElementById("progress-bar-fill").style.width = "0%";
    document.getElementById("progress-time-display").textContent = "0:00";

    // Hide listener controls and reset mute state
    document.getElementById("radio-listener-controls")?.classList.add("hidden");
    document.getElementById("media-resync-btn")?.classList.add("hidden");
    this.isMuted = false;
    const allMediaEls = [
      document.getElementById("audio-player"),
      document.getElementById("video-player"),
    ].filter(Boolean);
    allMediaEls.forEach((el) => (el.muted = false));
    const muteBtn = document.getElementById("mute-btn");
    if (muteBtn) muteBtn.textContent = "🔇 Mute";

    // Restore private playback state from before tuning in
    if (this.preRadioSnapshot) {
      const snap = this.preRadioSnapshot;
      this.preRadioSnapshot = null;

      if (snap.src) {
        debugLog(
          `Restoring pre-radio state: ${snap.src} @ ${snap.currentTime.toFixed(2)}s`,
        );
        // Restore the correct element type for the pre-radio media
        window.switchMediaElement?.(snap.isVideo);
        this.audio.src = snap.src;
        this.audio.load();
        this.audio.addEventListener(
          "canplay",
          () => {
            this.audio.currentTime = snap.currentTime;
            if (!snap.paused) this.audio.play();
          },
          { once: true },
        );
      } else {
        // No saved src, default back to the audio element
        window.switchMediaElement?.(false);
      }
      if (window.playlistManager && snap.mediaId)
        window.playlistManager.currentMediaId = snap.mediaId;
    } else {
      window.switchMediaElement?.(false);
    }

    // Set controls after switchMediaElement so the correct element gets them
    this.audio.controls = true;
    window.playlistManager?.render();
    this._notifyRoomChange();
  }

  // Removes broadcast event listeners from both media elements
  removeAudioBroadcastListeners() {
    const allMediaEls = [
      document.getElementById("audio-player"),
      document.getElementById("video-player"),
    ].filter(Boolean);
    this.audioEventHandlers.forEach((handler, event) => {
      allMediaEls.forEach((el) => el.removeEventListener(event, handler));
    });
    this.audioEventHandlers.clear();
  }

  sendHeartbeat() {
    if (
      !this.isBroadcasting ||
      !this.ws ||
      this.ws.readyState !== WebSocket.OPEN
    )
      return;
    const msg = {
      type: "Heartbeat",
      broadcaster_id: this.sessionId,
      playback_time: this.audio.currentTime,
    };
    debugLog(
      `Heartbeat: media ${this.getCurrentMediaIndex()}, time ${msg.playback_time.toFixed(2)}s`,
    );
    this.ws.send(JSON.stringify(msg));
  }

  sendAutoNext(nextMediaIndex) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    debugLog(`Sending AutoNext: next media index ${nextMediaIndex}`);
    this.ws.send(
      JSON.stringify({
        type: "AutoNext",
        broadcaster_id: this.sessionId,
        next_media_index: nextMediaIndex,
        server_timestamp_ms: 0,
      }),
    );
  }

  startBroadcasting() {
    if (this.isStartingBroadcast) {
      debugLog("Already starting broadcast, ignoring duplicate request");
      return;
    }
    this.isStartingBroadcast = true;

    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      debugLog("WebSocket not ready for broadcasting, connecting...");
      this.isStartingBroadcast = false;
      this.connectWebSocket();
      setTimeout(() => this.startBroadcasting(), 500);
      return;
    }
    if (this.isBroadcasting) {
      debugLog("Already broadcasting, cleaning up first");
      this.stopBroadcasting();
    }

    this.removeAudioBroadcastListeners();
    this.isBroadcasting = true;
    debugLog(`Broadcasting as: ${this.sessionId}`);

    if (this.isMobile())
      document.getElementById("mobile-warning").classList.remove("hidden");
    this.requestWakeLock();

    let broadcastUpdateTimer = null;
    const sendUpdate = () => {
      if (
        !this.isBroadcasting ||
        !this.ws ||
        this.ws.readyState !== WebSocket.OPEN
      )
        return;
      clearTimeout(broadcastUpdateTimer);
      broadcastUpdateTimer = setTimeout(() => {
        if (
          !this.isBroadcasting ||
          !this.ws ||
          this.ws.readyState !== WebSocket.OPEN
        )
          return;
        const msg = {
          type: "BroadcastUpdate",
          broadcaster_id: this.sessionId,
          media_index: this.getCurrentMediaIndex(),
          playback_time: this.audio.currentTime,
          is_playing: !this.audio.paused,
        };
        debugLog(
          `Broadcasting: media ${msg.media_index}, time ${msg.playback_time.toFixed(2)}s`,
        );
        this.ws.send(JSON.stringify(msg));
      }, 150);
    };

    // Skip the pause broadcast if the track ended naturally
    const sendPauseUpdate = () => {
      if (!this.audio.ended) sendUpdate();
    };

    this.audioEventHandlers.set("play", sendUpdate);
    this.audioEventHandlers.set("pause", sendPauseUpdate);
    this.audioEventHandlers.set("seeked", sendUpdate);

    // Attach to both elements so switching media type mid-broadcast still fires updates
    const allMediaEls = [
      document.getElementById("audio-player"),
      document.getElementById("video-player"),
    ].filter(Boolean);

    allMediaEls.forEach((el) => {
      el.addEventListener("play", sendUpdate);
      el.addEventListener("pause", sendPauseUpdate);
      el.addEventListener("seeked", sendUpdate);
    });

    this.heartbeatInterval = setInterval(() => this.sendHeartbeat(), 2000);

    const initMsg = {
      type: "StartBroadcasting",
      broadcaster_id: this.sessionId,
      media_index: this.getCurrentMediaIndex(),
      playback_time: this.audio.currentTime,
      is_playing: !this.audio.paused,
    };
    debugLog(
      `Starting broadcast: media ${initMsg.media_index}, time ${initMsg.playback_time.toFixed(2)}s`,
    );
    this.ws.send(JSON.stringify(initMsg));

    this.updateBroadcastingUI(true);
    this.isStartingBroadcast = false;
    this._notifyRoomChange();
  }

  stopBroadcasting() {
    if (!this.isBroadcasting) {
      debugLog("Not broadcasting, nothing to stop");
      return;
    }
    
    this.isBroadcasting = false;

    if (this.heartbeatInterval) {
      clearInterval(this.heartbeatInterval);
      this.heartbeatInterval = null;
    }
    this.removeAudioBroadcastListeners();
    this.releaseWakeLock();

    document.getElementById("mobile-warning").classList.add("hidden");

    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(
        JSON.stringify({
          type: "StopBroadcasting",
          broadcaster_id: this.sessionId,
        }),
      );
    }
    this.updateBroadcastingUI(false);
    this._notifyRoomChange();
    debugLog("Broadcasting stopped");
  }

  updateBroadcastingUI(isBroadcasting) {
    document
      .getElementById("broadcast-status")
      .classList.toggle("hidden", !isBroadcasting);
    document
      .getElementById("broadcast-btn")
      .classList.toggle("hidden", isBroadcasting);
    document
      .getElementById("stop-broadcast-btn")
      .classList.toggle("hidden", !isBroadcasting);
  }

  // Wrapper around play() used by all radio playback paths.
  // Ensures the element is unmuted and isMuted/button are reset before
  // the browser outputs any audio, this is the last possible moment to
  // correct a muted state that carried over from private mode or from
  // load() resetting the element.
  _radioPlay() {
    if (this.audio.muted) {
      this.audio.muted = false;
      this.isMuted = false;
      const btn = document.getElementById("mute-btn");
      if (btn) btn.textContent = "🔇 Mute";
      debugLog("Corrected muted state before radio playback");
    }
    this.audio.play();
  }

  // Toggles mute on the active element. The volumechange listener in main.js
  // mirrors the state to the other element, updates this.isMuted, and updates
  // the button label, so this only needs to flip the element's muted property.
  toggleMute() {
    this.audio.muted = !this.audio.muted;
    debugLog(`Audio ${this.audio.muted ? "muted" : "unmuted"}`);
  }

  // Re-sends TuneIn to the current broadcaster so the server issues a fresh
  // Sync with the broadcaster's live position. Useful when drift accumulates
  // or after a network hiccup that didn't fully drop the WebSocket.
  resync() {
    if (!this.tunedBroadcaster || this.mode !== "radio") return;
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      debugLog("WebSocket not ready for resync");
      return;
    }
    debugLog("Resyncing to broadcaster position...");
    // Clear any stale loading state so the incoming Sync is handled cleanly
    this._loadingMediaIndex = null;
    this._pendingSeekTime = 0;
    this._pendingIsPlaying = false;
    this.pendingAutoNextIndex = null;
    this.pendingAutoNextTime = 0;
    // Set tuneInSentAt so latency compensation runs on the fresh Sync
    this._tuneInSentAt = Date.now();
    // Perfect Sync: refresh the clock estimate alongside the position resync so
    // a longer, more deliberate correction is fine here.
    if (this.perfectSync) this.syncClock();
    this.ws.send(
      JSON.stringify({ type: "TuneIn", broadcaster_id: this.tunedBroadcaster }),
    );
  }

  // Returns the server's numeric index for the currently playing media
  getCurrentMediaIndex() {
    const idMatch = this.audio.src.match(/\/stream\/id\/([^/?]+)/);
    if (idMatch && window.playlistManager)
      return window.playlistManager.getServerIndexById(idMatch[1]);
    const indexMatch = this.audio.src.match(/\/stream\/(\d+)(?:\?|$)/);
    return indexMatch ? parseInt(indexMatch[1]) : 0;
  }

  // Updates the subtitle track src whenever a video is loaded.
  // Clears the track for audio media so stale subtitles from the previous video don't linger.
  // 404 responses (no subtitles for this file) are silently ignored by the browser.
  _updateSubtitleTrack(mediaId, isVideo) {
    const track = document.getElementById("subtitle-track");
    if (!track) return;
    if (isVideo && mediaId) {
      track.src = `/stargzr/player/subtitles/${mediaId}`;
    } else {
      track.src = "";
    }
  }

  isInRadioMode() {
    return this.mode === "radio";
  }

  // Tells the chat panel the room it should be showing has moved. Fired whenever
  // tune in state or broadcasting state changes, since those are exactly what
  // the server uses to pick a chat room.
  _notifyRoomChange() {
    document.dispatchEvent(new CustomEvent("radioRoomChange"));
  }
}
