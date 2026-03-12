class RadioPlayer {
  constructor(audioElement, sessionId) {
    this.audio     = audioElement;
    this.sessionId = sessionId;
    this.mode      = "private";
    this.tunedBroadcaster   = null;
    this.ws                 = null;
    this.heartbeatInterval  = null;
    this.targetTime         = 0;
    this.isBroadcasting     = false;
    this.isStartingBroadcast = false;
    this.wakeLock           = null;
    this.pageHiddenAt       = null;

    // Queued next media received from AutoNext while listener's current track
    // is still playing. Cleared once the local 'ended' event fires.
    this.pendingAutoNextIndex = null;
    this.pendingAutoNextTime  = 0;

    // Guards against duplicate canplay handlers when the same media-change
    // produces more than one Sync before the browser fires canplay.
    this._loadingMediaIndex  = null;
    this._pendingSeekTime   = 0;
    this._pendingIsPlaying  = false;

    // Set to Date.now() when TuneIn is sent; cleared after the first Sync.
    // Used to compensate for server-to-listener transit on initial sync.
    this._tuneInSentAt = null;

    // Reconnection state
    this.reconnectAttempts    = 0;
    this.maxReconnectAttempts = 10;
    this.baseReconnectDelay   = 500;
    this.maxReconnectDelay    = 30000;
    this.reconnectTimer       = null;
    this.isIntentionalDisconnect = false;

    // Event listener tracking for cleanup
    this.boundHandlers      = new Map();
    this.audioEventHandlers = new Map();

    this.connectionState = "disconnected";

    // Debounce timer for incoming Sync messages
    this.syncTimer = null;

    debugLog(`Initialized with session ID: ${sessionId}`);
    this.setupPageVisibilityHandling();
  }

  isMobile() {
    return /Android|webOS|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(navigator.userAgent);
  }

  setupPageVisibilityHandling() {
    let visibilityProp  = "hidden";
    let visibilityEvent = "visibilitychange";
    if (typeof document.webkitHidden !== "undefined") {
      visibilityProp  = "webkitHidden";
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
    const hiddenDuration = this.pageHiddenAt ? (Date.now() - this.pageHiddenAt) / 1000 : 0;
    this.pageHiddenAt = null;
    debugLog(`👁️ Page visible again (was hidden for ${hiddenDuration.toFixed(1)}s)`);

    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      debugLog("WebSocket disconnected, reconnecting...");
      this.connectWebSocket();
    }

    if (this.isBroadcasting) {
      if (hiddenDuration < 40) {
        debugLog("✓ Resuming broadcast (short background duration)");
        this.requestWakeLock();
        if (this.ws && this.ws.readyState === WebSocket.OPEN) this.sendHeartbeat();
      } else {
        debugLog("⚠️ Long background duration - verifying broadcast state");
        this.verifyBroadcastState();
      }
    }
  }

  async requestWakeLock() {
    if (!("wakeLock" in navigator)) { debugLog("Wake Lock API not supported"); return; }
    try {
      this.wakeLock = await navigator.wakeLock.request("screen");
      debugLog("✓ Wake lock acquired - screen will stay on");
      this.wakeLock.addEventListener("release", () => debugLog("Wake lock released"));
    } catch (err) {
      debugLog(`Wake lock error: ${err.message}`);
    }
  }

  async releaseWakeLock() {
    if (!this.wakeLock) return;
    try { await this.wakeLock.release(); this.wakeLock = null; debugLog("Wake lock released"); }
    catch (err) { debugLog(`Wake lock release error: ${err.message}`); }
  }

  async verifyBroadcastState() {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    debugLog("Querying broadcast state from server");
    this.ws.send(JSON.stringify({ type: "QueryBroadcastState", session_id: this.sessionId }));
  }

  getReconnectDelay() {
    const exponential = this.baseReconnectDelay * Math.pow(2, this.reconnectAttempts);
    const jitter      = exponential * 0.2 * (Math.random() - 0.5);
    return Math.min(Math.max(this.baseReconnectDelay, exponential + jitter), this.maxReconnectDelay);
  }

  updateConnectionState(newState) {
    this.connectionState = newState;
    const states = {
      connected:    { icon: "🟢", text: "Connected",        color: "#28a745" },
      connecting:   { icon: "🟡", text: "Connecting...",    color: "#ffc107" },
      disconnected: { icon: "⚪", text: "Disconnected",     color: "#6c757d" },
      error:        { icon: "🔴", text: "Connection Error", color: "#dc3545" },
    };
    const cfg = states[newState] ?? states.disconnected;
    document.getElementById("connection-indicator").textContent  = cfg.icon;
    const text = document.getElementById("connection-text");
    text.textContent  = cfg.text;
    text.style.color  = cfg.color;

    document.dispatchEvent(new CustomEvent("connectionStateChange", {
      detail: { state: newState, attempt: this.reconnectAttempts, maxAttempts: this.maxReconnectAttempts },
    }));
  }

  connectWebSocket() {
    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      debugLog(`WebSocket already ${this.ws.readyState === WebSocket.OPEN ? "connected" : "connecting"}, skipping...`);
      return;
    }
    if (this.reconnectTimer) { clearTimeout(this.reconnectTimer); this.reconnectTimer = null; }

    this.updateConnectionState("connecting");

    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const wsUrl    = `${protocol}//${window.location.host}/stargzr/player/radio`;
    debugLog(`Connecting to WebSocket: ${wsUrl} (attempt ${this.reconnectAttempts + 1})`);

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
      try { this.handleRadioMessage(JSON.parse(event.data)); }
      catch (error) { debugLog(`Failed to parse message: ${error.message}`); }
    };

    const onError = () => { debugLog("✗ WebSocket error"); this.updateConnectionState("error"); };

    const onClose = async (event) => {
      debugLog(`WebSocket closed (code: ${event.code}, clean: ${event.wasClean})`);
      this.updateConnectionState("disconnected");
      this.removeWebSocketHandlers();

      if (event.code === 1006 && !this.isIntentionalDisconnect) {
        try {
          const resp = await fetch("/stargzr/player/session/check");
          if (resp.status === 401) { debugLog("Session expired on WS connect, reloading..."); window.location.reload(); return; }
        } catch (_) {}
      }

      if (!this.isIntentionalDisconnect) this.scheduleReconnect();
      else { debugLog("Intentional disconnect, not reconnecting"); this.isIntentionalDisconnect = false; }
    };

    this.boundHandlers.set("open",    onOpen);
    this.boundHandlers.set("message", onMessage);
    this.boundHandlers.set("error",   onError);
    this.boundHandlers.set("close",   onClose);

    this.ws.addEventListener("open",    onOpen);
    this.ws.addEventListener("message", onMessage);
    this.ws.addEventListener("error",   onError);
    this.ws.addEventListener("close",   onClose);
  }

  scheduleReconnect() {
    if (this.reconnectAttempts >= this.maxReconnectAttempts) {
      debugLog(`Max reconnection attempts (${this.maxReconnectAttempts}) reached`);
      this.updateConnectionState("error");
      alert("Unable to connect to server after multiple attempts. Please refresh the page.");
      return;
    }
    const delay = this.getReconnectDelay();
    debugLog(`Scheduling reconnection in ${delay}ms (attempt ${this.reconnectAttempts + 1}/${this.maxReconnectAttempts})`);
    this.reconnectTimer = setTimeout(() => { this.reconnectAttempts++; this.connectWebSocket(); }, delay);
  }

  removeWebSocketHandlers() {
    if (!this.ws) return;
    this.boundHandlers.forEach((handler, event) => this.ws.removeEventListener(event, handler));
    this.boundHandlers.clear();
  }

  disconnect() {
    this.isIntentionalDisconnect = true;
    if (this.reconnectTimer) { clearTimeout(this.reconnectTimer); this.reconnectTimer = null; }
    if (this.isBroadcasting) this.stopBroadcasting();

    if (this.mode === "radio" && this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: "TuneOut" }));
    }
    if (this.ws) {
      this.removeWebSocketHandlers();
      if (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING) this.ws.close();
      this.ws = null;
    }
    this.updateConnectionState("disconnected");
    debugLog("Disconnected");
  }

  handleRadioMessage(msg) {
    if (msg.type === "Sync" && msg.broadcaster_id === this.sessionId) return;
    debugLog(`Handling message type: ${msg.type}, mode: ${this.mode}`);

    if (msg.type === "Analytics") { updateAnalyticsDisplay(msg); return; }

    if (msg.type === "BroadcastStateResponse") {
      debugLog(`Server says broadcasting: ${msg.is_broadcasting}, client thinks: ${this.isBroadcasting}`);
      if (msg.is_broadcasting !== this.isBroadcasting) {
        debugLog("⚠️ State mismatch detected!");
        if (msg.is_broadcasting && !this.isBroadcasting) {
          this.isBroadcasting = true;
          this.updateBroadcastingUI(true);
          if (this.isMobile()) document.getElementById("mobile-warning").classList.remove("hidden");
        } else if (!msg.is_broadcasting && this.isBroadcasting) {
          debugLog("🔄 Server lost our session - resuming broadcast automatically");
          const initMsg = {
            type: "StartBroadcasting",
            broadcaster_id: this.sessionId,
            media_index:    this.getCurrentMediaIndex(),
            playback_time: this.audio.currentTime,
            is_playing:    !this.audio.paused,
          };
          debugLog(`Resuming broadcast: media ${initMsg.media_index}, time ${initMsg.playback_time.toFixed(2)}s`);
          if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(initMsg));
        }
      }
      return;
    }

    if (msg.type === "Error") {
      debugLog(`Server error: ${msg.message}`);
      if (msg.message.includes("BroadcasterNotFound") || msg.message.includes("not broadcasting")) {
        if (this.isBroadcasting) { this.stopBroadcasting(); alert("Your broadcast session ended. Please start broadcasting again if needed."); }
      } else {
        alert(`Error: ${msg.message}. Please try and refresh the page.`);
      }
      return;
    }

    if (msg.type === "BroadcasterOffline") {
      if (this.tunedBroadcaster === msg.broadcaster_id) {
        debugLog(`Broadcaster ${msg.broadcaster_id} went offline`);
        this.tuneOut("broadcaster_offline");
        alert("The broadcaster you were listening to has stopped broadcasting.");
      }
      return;
    }

    if (msg.type === "BroadcasterOnline") {
      if (this.tunedBroadcaster === msg.broadcaster_id) debugLog(`User ${msg.broadcaster_id} is now broadcasting`);
      return;
    }

    if (msg.type === "AutoNext") {
      if (this.mode !== "radio") return;
      debugLog(`AutoNext received: queuing media index ${msg.next_media_index} for after current track ends`);
      this.pendingAutoNextIndex = msg.next_media_index;
      this.pendingAutoNextTime  = 0;

      this.audio.addEventListener("ended", () => {
        if (this.pendingAutoNextIndex === null) return;
        const idx    = this.pendingAutoNextIndex;
        const seekTo = this.pendingAutoNextTime;
        this.pendingAutoNextIndex = null;
        this.pendingAutoNextTime  = 0;

        const mediaId = window.playlistManager?.getMediaIdByServerIndex(idx) ?? null;

        // Switch to the correct element before loading the next media
        const nextMedia = mediaId ? window.playlistManager?.getMediaById(mediaId) : window.playlistManager?.originalMedias[idx];
        window.switchMediaElement?.(nextMedia?.media_type === "video");

        this.audio.src = mediaId ? `/stargzr/player/stream/id/${mediaId}` : `/stargzr/player/stream/${idx}`;
        this._updateSubtitleTrack(mediaId, nextMedia?.media_type === "video");
        this.audio.load();

        this.audio.addEventListener("canplay", () => {
          this.audio.currentTime = seekTo;
          this.audio.play();
        }, { once: true });

        debugLog(`AutoNext: switched to media index ${idx} at ${seekTo.toFixed(2)}s after local track ended`);
      }, { once: true });
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
      clearTimeout(this.syncTimer);
      this.syncTimer = setTimeout(() => this.syncToBroadcaster(msg), 80);
    }
  }

  syncToBroadcaster(msg) {
    let { media_index, playback_time, is_playing } = msg;

    // First Sync after TuneIn: compensate for server-to-listener transit time
    if (this._tuneInSentAt !== null) {
      const elapsed  = (Date.now() - this._tuneInSentAt) / 1000;
      this._tuneInSentAt = null;
      const duration = isFinite(this.audio.duration) ? this.audio.duration : Infinity;
      const adjusted = playback_time + elapsed;
      if (adjusted < duration) playback_time = adjusted;
      debugLog(`TuneIn latency compensation: +${elapsed.toFixed(3)}s to ${playback_time.toFixed(2)}s`);
    }

    // Broadcaster is still on the pending AutoNext media: update position only
    if (this.pendingAutoNextIndex !== null && msg.media_index === this.pendingAutoNextIndex) {
      this.pendingAutoNextTime = playback_time;
      debugLog(`AutoNext position updated to ${playback_time.toFixed(2)}s`);
      return;
    }

    // Any manual broadcaster action clears the pending AutoNext
    if (this.pendingAutoNextIndex !== null && msg.media_index !== this.pendingAutoNextIndex) {
      debugLog(`Manual broadcaster action cleared pendingAutoNextIndex (was ${this.pendingAutoNextIndex})`);
      this.pendingAutoNextIndex = null;
    }

    const currentIndex = this.getCurrentMediaIndex();

    if (currentIndex !== media_index || this._loadingMediaIndex === media_index) {
      this._pendingSeekTime  = playback_time;
      this._pendingIsPlaying = is_playing;

      if (this._loadingMediaIndex !== media_index) {
        debugLog(`Switching from media ${currentIndex} to media ${media_index}`);
        this._loadingMediaIndex = media_index;

        const mediaId   = window.playlistManager?.getMediaIdByServerIndex(media_index) ?? null;
        const nextMedia = mediaId ? window.playlistManager?.getMediaById(mediaId) : window.playlistManager?.originalMedias[media_index];

        // Switch media element before loading so the browser targets the right one
        window.switchMediaElement?.(nextMedia?.media_type === "video");

        this.audio.src = mediaId ? `/stargzr/player/stream/id/${mediaId}` : `/stargzr/player/stream/${media_index}`;
        this._updateSubtitleTrack(mediaId, nextMedia?.media_type === "video");
        this.audio.load();

        this.audio.addEventListener("canplay", () => {
          this._loadingMediaIndex = null;
          const seekTo = this._pendingSeekTime < 1.0 ? 0 : this._pendingSeekTime;
          debugLog(`canplay, seeking to ${seekTo.toFixed(2)}s (broadcaster at ${this._pendingSeekTime.toFixed(2)}s)`);
          this.audio.currentTime = seekTo;
          if (this._pendingIsPlaying) this.audio.play();
        }, { once: true });
      }
    } else {
      this._loadingMediaIndex = null;
      this.audio.currentTime = playback_time;
      if (is_playing)  this.audio.play();
      if (!is_playing && !this.audio.paused) this.audio.pause();
    }
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
      const snapMedia   = snapMediaId ? window.playlistManager?.getMediaById(snapMediaId) : null;
      this.preRadioSnapshot = {
        src:         this.audio.src || this.audio.querySelector?.("source")?.getAttribute("src") || null,
        currentTime: this.audio.currentTime,
        paused:      this.audio.paused,
        mediaId:      snapMediaId,
        // Remember whether the pre-radio media was video so tuneOut can restore the right element
        isVideo:     snapMedia?.media_type === "video",
      };
      debugLog(`Saved pre-radio state: ${this.preRadioSnapshot.src} @ ${this.preRadioSnapshot.currentTime.toFixed(2)}s`);
    }

    this.mode            = "radio";
    this.tunedBroadcaster = broadcasterId;
    this._tuneInSentAt    = Date.now();

    debugLog(`Sending TuneIn: ${broadcasterId}`);
    this.ws.send(JSON.stringify({ type: "TuneIn", broadcaster_id: broadcasterId }));

    this.audio.controls = false;
    document.getElementById("broadcast-progress").classList.remove("hidden");
    window.playlistManager?.render();
  }

  tuneOut(reason = "manual") {
    debugLog(`Leaving radio mode: ${reason}`);
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify({ type: "TuneOut" }));

    this.mode             = "private";
    this.tunedBroadcaster = null;
    this.pendingAutoNextIndex = null;
    this.pendingAutoNextTime  = 0;

    document.getElementById("mode-display").textContent  = "Private Mode";
    document.getElementById("mode-display").className    = "mode-badge private";
    document.getElementById("broadcaster-info").classList.add("hidden");
    document.getElementById("tune-out-btn").classList.add("hidden");
    document.getElementById("tune-in-btn").classList.remove("hidden");
    document.getElementById("prev-btn").disabled = false;
    document.getElementById("next-btn").disabled = false;

    document.getElementById("broadcast-progress").classList.add("hidden");
    document.getElementById("progress-bar-fill").style.width = "0%";
    document.getElementById("progress-time-display").textContent = "0:00";

    // Restore private playback state from before tuning in
    if (this.preRadioSnapshot) {
      const snap = this.preRadioSnapshot;
      this.preRadioSnapshot = null;

      if (snap.src) {
        debugLog(`Restoring pre-radio state: ${snap.src} @ ${snap.currentTime.toFixed(2)}s`);
        // Restore the correct element type for the pre-radio media
        window.switchMediaElement?.(snap.isVideo);
        this.audio.src = snap.src;
        this.audio.load();
        this.audio.addEventListener("canplay", () => {
          this.audio.currentTime = snap.currentTime;
          if (!snap.paused) this.audio.play();
        }, { once: true });
      } else {
        // No saved src, default back to the audio element
        window.switchMediaElement?.(false);
      }
      if (window.playlistManager && snap.mediaId) window.playlistManager.currentMediaId = snap.mediaId;
    } else {
      window.switchMediaElement?.(false);
    }

    // Set controls after switchMediaElement so the correct element gets them
    this.audio.controls = true;
    window.playlistManager?.render();
  }

  // Removes broadcast event listeners from both media elements
  removeAudioBroadcastListeners() {
    const allMediaEls = [
      document.getElementById("audio-player"),
      document.getElementById("video-player"),
    ].filter(Boolean);
    this.audioEventHandlers.forEach((handler, event) => {
      allMediaEls.forEach(el => el.removeEventListener(event, handler));
    });
    this.audioEventHandlers.clear();
  }

  sendHeartbeat() {
    if (!this.isBroadcasting || !this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const msg = { type: "Heartbeat", broadcaster_id: this.sessionId, playback_time: this.audio.currentTime, client_timestamp_ms: Date.now() };
    debugLog(`Heartbeat: media ${this.getCurrentMediaIndex()}, time ${msg.playback_time.toFixed(2)}s`);
    this.ws.send(JSON.stringify(msg));
  }

  sendAutoNext(nextMediaIndex) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    debugLog(`Sending AutoNext: next media index ${nextMediaIndex}`);
    this.ws.send(JSON.stringify({ type: "AutoNext", broadcaster_id: this.sessionId, next_media_index: nextMediaIndex, server_timestamp_ms: 0 }));
  }

  startBroadcasting() {
    if (this.isStartingBroadcast) { debugLog("Already starting broadcast, ignoring duplicate request"); return; }
    this.isStartingBroadcast = true;

    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      debugLog("WebSocket not ready for broadcasting, connecting...");
      this.isStartingBroadcast = false;
      this.connectWebSocket();
      setTimeout(() => this.startBroadcasting(), 500);
      return;
    }
    if (this.isBroadcasting) { debugLog("Already broadcasting, cleaning up first"); this.stopBroadcasting(); }

    this.removeAudioBroadcastListeners();
    this.isBroadcasting = true;
    debugLog(`Broadcasting as: ${this.sessionId}`);

    if (this.isMobile()) document.getElementById("mobile-warning").classList.remove("hidden");
    this.requestWakeLock();

    let broadcastUpdateTimer = null;
    const sendUpdate = () => {
      if (!this.isBroadcasting || !this.ws || this.ws.readyState !== WebSocket.OPEN) return;
      clearTimeout(broadcastUpdateTimer);
      broadcastUpdateTimer = setTimeout(() => {
        if (!this.isBroadcasting || !this.ws || this.ws.readyState !== WebSocket.OPEN) return;
        const msg = {
          type: "BroadcastUpdate", broadcaster_id: this.sessionId,
          media_index: this.getCurrentMediaIndex(), playback_time: this.audio.currentTime,
          is_playing: !this.audio.paused, client_timestamp_ms: Date.now(),
        };
        debugLog(`Broadcasting: media ${msg.media_index}, time ${msg.playback_time.toFixed(2)}s`);
        this.ws.send(JSON.stringify(msg));
      }, 150);
    };

    // Skip the pause broadcast if the track ended naturally
    const sendPauseUpdate = () => { if (!this.audio.ended) sendUpdate(); };

    this.audioEventHandlers.set("play",   sendUpdate);
    this.audioEventHandlers.set("pause",  sendPauseUpdate);
    this.audioEventHandlers.set("seeked", sendUpdate);

    // Attach to both elements so switching media type mid-broadcast still fires updates
    const allMediaEls = [
      document.getElementById("audio-player"),
      document.getElementById("video-player"),
    ].filter(Boolean);

    allMediaEls.forEach(el => {
      el.addEventListener("play",   sendUpdate);
      el.addEventListener("pause",  sendPauseUpdate);
      el.addEventListener("seeked", sendUpdate);
    });

    this.heartbeatInterval = setInterval(() => this.sendHeartbeat(), 2000);

    const initMsg = { type: "StartBroadcasting", broadcaster_id: this.sessionId, media_index: this.getCurrentMediaIndex(), playback_time: this.audio.currentTime, is_playing: !this.audio.paused };
    debugLog(`Starting broadcast: media ${initMsg.media_index}, time ${initMsg.playback_time.toFixed(2)}s`);
    this.ws.send(JSON.stringify(initMsg));

    this.updateBroadcastingUI(true);
    this.isStartingBroadcast = false;
  }

  stopBroadcasting() {
    if (!this.isBroadcasting) { debugLog("Not broadcasting, nothing to stop"); return; }
    this.isBroadcasting = false;

    if (this.heartbeatInterval) { clearInterval(this.heartbeatInterval); this.heartbeatInterval = null; }
    this.removeAudioBroadcastListeners();
    this.releaseWakeLock();

    document.getElementById("mobile-warning").classList.add("hidden");

    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: "StopBroadcasting", broadcaster_id: this.sessionId }));
    }
    this.updateBroadcastingUI(false);
    debugLog("Broadcasting stopped");
  }

  updateBroadcastingUI(isBroadcasting) {
    document.getElementById("broadcast-status").classList.toggle("hidden",    !isBroadcasting);
    document.getElementById("broadcast-btn").classList.toggle("hidden",        isBroadcasting);
    document.getElementById("stop-broadcast-btn").classList.toggle("hidden",  !isBroadcasting);
  }

  // Returns the server's numeric index for the currently playing media
  getCurrentMediaIndex() {
    const idMatch = this.audio.src.match(/\/stream\/id\/([^/?]+)/);
    if (idMatch && window.playlistManager) return window.playlistManager.getServerIndexById(idMatch[1]);
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

  isInRadioMode() { return this.mode === "radio"; }
}