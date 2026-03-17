const HLS_JS_CDN = "https://cdn.jsdelivr.net/npm/hls.js@1.5.13/dist/hls.min.js";

let _hlsInstance = null;
let _hlsVideoEl = null;
let _preHlsMedia = null;
let _hlsRetryTimer = null;
let _stallWatchdog = null;
let _lastProgress = 0;
let _hlsSrc = null;
let _keyCache = {};

// Guards against concurrent recovery attempts stacking on each other.
// When _hardRecoverHls fires it sets this true; it's cleared once the
// reattach completes (or fails). Any recovery path that finds this true
// bails out immediately, preventing the rapid-fire NS_BINDING_ABORTED loop
// where each new loadSource() aborts the previous in-flight request.
let _recovering = false;

// ─── Cache-busting helper ─────────────────────────────────────────────────────
//
// CRITICAL: only ever call this on FRAGMENT URLs, never on the manifest.
//
// The manifest URL (e.g. chevy.soyspace.cyou/proxy/.../mono.css) has a
// server-side session tied to the exact URL string. Appending ?_cb=timestamp
// produces a URL the server has never whitelisted → it aborts the connection
// before any bytes are sent (NS_BINDING_ABORTED in Firefox).
//
// Segments (*.ts, *.png-disguised TS) have static-looking filenames and ARE
// cached by the browser. Without busting them every fetch returns the same
// bytes, causing HLS.js to decode the same audio/video in a loop.
//
// HLS.js handles manifest cache-invalidation internally (it sets Cache-Control
// headers and uses its own polling timer) so we don't need to touch it.
function _cacheBust(url) {
  const sep = url.includes("?") ? "&" : "?";
  return url + sep + "_cb=" + Date.now();
}

function isHlsUrl(text) {
  if (!text) return false;
  const t = text.trim();
  if (/\.m3u8(\?|$)/i.test(t)) return true;
  if (/\/mono\.css(\?|$)/i.test(t)) return true;
  if (t.startsWith("hls://")) return true;
  return false;
}

function loadHlsLibrary() {
  return new Promise((resolve, reject) => {
    if (typeof Hls !== "undefined") {
      resolve();
      return;
    }
    const s = document.createElement("script");
    s.src = HLS_JS_CDN;
    s.onload = () => {
      debugLog("HLS.js loaded");
      resolve();
    };
    s.onerror = () => reject(new Error("Failed to load HLS.js from CDN"));
    document.head.appendChild(s);
  });
}

// ─── Custom key loader ────────────────────────────────────────────────────────
function makeKeyLoader() {
  function KeyLoader(config) {
    this.config = config;
  }
  KeyLoader.prototype.destroy = function () {};
  KeyLoader.prototype.abort = function () {};
  KeyLoader.prototype.load = function (context, config, callbacks) {
    const url = context.url;
    if (_keyCache[url]) {
      callbacks.onSuccess({ data: _keyCache[url], url: url }, {}, context);
      return;
    }
    debugLog("HLS key: fetching " + url.split("/").pop());
    fetch(url, { credentials: "include" })
      .then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.arrayBuffer();
      })
      .then(function (buf) {
        const key = new Uint8Array(buf);
        _keyCache[url] = key;
        debugLog("HLS key: OK " + key.length + " bytes");
        callbacks.onSuccess({ data: key, url: url }, {}, context);
      })
      .catch(function (err) {
        debugLog("HLS key: FAILED — " + err.message);
        callbacks.onError({ code: 0, text: err.message }, context, null);
      });
  };
  return KeyLoader;
}

// ─── Fragment loader factory ──────────────────────────────────────────────────
// Two jobs:
//   1. Force responseType=arraybuffer so .png-disguised TS segments pass
//      through the browser's MIME type check and reach the MSE decoder.
//   2. Cache-bust FRAGMENT URLs ONLY. See _cacheBust() comment above for why
//      we must never apply this to the manifest.
function makeFragLoaderClass() {
  const DefaultLoader = Hls.DefaultConfig.loader;

  class FragLoader extends DefaultLoader {
    constructor(config) {
      super(config);
    }

    load(context, config, callbacks) {
      if (context.type !== "fragment") {
        // Manifest, playlist, key — pass through completely unmodified.
        // The manifest URL must stay byte-for-byte identical to what the
        // server whitelisted during session setup.
        return super.load(context, config, callbacks);
      }

      // Cache-bust fragment URLs so the browser never serves a stale copy.
      // Shallow-clone context to avoid mutating HLS.js's internal object.
      const bustedContext = Object.assign({}, context, {
        url: _cacheBust(context.url),
      });

      const origSuccess = callbacks.onSuccess;
      const wrappedCallbacks = Object.assign({}, callbacks, {
        onSuccess: function (response, stats, ctx, networkDetails) {
          origSuccess(response, stats, ctx, networkDetails);
        },
      });

      const origXhrSetup = config.xhrSetup;
      const patchedConfig = Object.assign({}, config, {
        xhrSetup: function (xhr, url) {
          // Force arraybuffer so .png-labelled TS content isn't mangled
          // by the browser's MIME-based response handling.
          xhr.responseType = "arraybuffer";
          if (origXhrSetup) origXhrSetup(xhr, url);
        },
      });

      super.load(bustedContext, patchedConfig, wrappedCallbacks);
    }
  }

  return FragLoader;
}

// ─── Hard recovery: detach + reattach ────────────────────────────────────────
//
// Used when startLoad(-1) didn't produce any new fragments (MSE SourceBuffer
// state has diverged). detachMedia() nukes the MediaSource and SourceBuffers;
// attachMedia() rebuilds and fires MEDIA_ATTACHED → loadSource(src).
//
// _recovering guards against concurrent calls. Without it, each aborted
// manifest request fires bufferStalledError → another _hardRecoverHls → another
// loadSource which aborts the previous one → tight NS_BINDING_ABORTED loop.
//
// The 300ms delay prevents InvalidStateError from browsers that close the old
// MediaSource asynchronously.
function _hardRecoverHls(src) {
  if (_recovering) {
    debugLog("HLS: hard recovery already in progress, skipping");
    return;
  }
  _recovering = true;
  debugLog("HLS: hard recovery — detach/reattach MSE");
  _showHlsBadge("Recovering...", "#ffc107");
  try {
    _hlsInstance.stopLoad();
    _hlsInstance.detachMedia();
    setTimeout(function () {
      try {
        _hlsInstance.attachMedia(_hlsVideoEl);
        // MEDIA_ATTACHED fires → loadSource(src) called by existing listener.
        // _recovering is cleared there, not here, so the guard stays active
        // for the full async round-trip.
        _lastProgress = Date.now();
      } catch (e) {
        debugLog(
          "HLS: reattach failed (" + e.message + "), full restart in 1s",
        );
        _recovering = false;
        if (_hlsRetryTimer) clearTimeout(_hlsRetryTimer);
        _hlsRetryTimer = setTimeout(function () {
          loadHlsStream(src);
        }, 1000);
      }
    }, 300);
  } catch (e) {
    debugLog("HLS: detach failed (" + e.message + "), full restart in 1s");
    _recovering = false;
    if (_hlsRetryTimer) clearTimeout(_hlsRetryTimer);
    _hlsRetryTimer = setTimeout(function () {
      loadHlsStream(src);
    }, 1000);
  }
}

async function loadHlsStream(src) {
  _hlsSrc = src;
  _keyCache = {};
  _recovering = false; // reset on every fresh load
  debugLog("HLS: loading stream: " + src);
  _showHlsBadge("Loading...", "#ffc107");

  try {
    await loadHlsLibrary();
  } catch (e) {
    _showHlsBadge("HLS.js failed to load", "#dc3545");
    debugLog("HLS: " + e.message);
    return;
  }

  const audio = document.getElementById("audio-player");
  const video = document.getElementById("video-player");
  window._activeMedia?.pause();

  _preHlsMedia = {
    audioSrc: audio.src,
    currentTime: window._activeMedia?.currentTime ?? 0,
    paused: window._activeMedia?.paused ?? true,
    mediaId: window.playlistManager?.currentMediaId ?? null,
  };

  audio.classList.add("hidden");
  audio.pause();
  const iframeEl = document.getElementById("iframe-player");
  if (iframeEl) {
    iframeEl.src = "";
    iframeEl.classList.add("hidden");
  }
  document.getElementById("media-resync-btn")?.classList.add("hidden");

  video.pause();
  video.removeAttribute("src");
  while (video.firstChild) video.removeChild(video.firstChild);
  video.load();
  video.classList.remove("hidden");
  video.controls = true;

  window._activeMedia = video;
  if (window.player) window.player.audio = video;

  document.getElementById("prev-btn").disabled = true;
  document.getElementById("next-btn").disabled = true;

  const closeBtn = document.getElementById("iframe-close-btn");
  if (closeBtn) {
    closeBtn.textContent = "Stop Stream";
    closeBtn._hlsClose = true;
    closeBtn.classList.remove("hidden");
  }

  _destroyHls();
  _stopStallWatchdog();

  if (Hls.isSupported()) {
    debugLog("HLS: using HLS.js");

    _hlsInstance = new Hls({
      liveSyncDurationCount: 3,
      liveMaxLatencyDurationCount: 6,
      liveBackBufferLength: 10,
      maxBufferLength: 8,
      maxMaxBufferLength: 16,
      manifestLoadingMaxRetry: 999,
      manifestLoadingRetryDelay: 1000,
      manifestLoadingMaxRetryTimeout: 4000,
      fragLoadingMaxRetry: 10,
      fragLoadingRetryDelay: 500,
      fragLoadingMaxRetryTimeout: 8000,
      keyLoader: makeKeyLoader(),
      loader: makeFragLoaderClass(),
      enableWorker: true,
      lowLatencyMode: false,
    });

    _hlsVideoEl = video;
    _hlsInstance.attachMedia(video);

    _hlsInstance.on(Hls.Events.MEDIA_ATTACHED, function () {
      debugLog("HLS: media attached, loading source");
      // Pass src UNMODIFIED. The server ties a reCAPTCHA/IP session to the
      // exact URL; any query param we add produces an unknown URL → abort.
      _hlsInstance.loadSource(src);
      // Clear the recovery guard here — we're back to a clean state.
      _recovering = false;
    });

    _hlsInstance.on(Hls.Events.MANIFEST_PARSED, function (ev, data) {
      debugLog("HLS: manifest parsed, " + data.levels.length + " level(s)");
      _showHlsBadge("LIVE", "#dc3545");
      video.play().catch(function (e) {
        debugLog("HLS autoplay blocked: " + e.message);
      });
      _startStallWatchdog(video, src);
    });

    _hlsInstance.on(Hls.Events.LEVEL_LOADED, function (ev, data) {
      debugLog(
        "HLS: playlist refreshed, " + data.details.fragments.length + " frags",
      );
    });

    _hlsInstance.on(Hls.Events.FRAG_LOADED, function (ev, data) {
      _lastProgress = Date.now();
      _showHlsBadge("LIVE", "#dc3545");
      debugLog(
        "HLS: frag OK — " +
          (data.frag?.relurl || "").split("?")[0].split("/").pop(),
      );
    });

    _hlsInstance.on(Hls.Events.ERROR, function (ev, data) {
      const loc = data.frag?.relurl
        ? " @ " + data.frag.relurl.split("?")[0].split("/").pop()
        : data.url
          ? " @ " + data.url.split("?")[0].split("/").pop()
          : "";
      debugLog(
        "HLS [" +
          (data.fatal ? "FATAL" : "warn") +
          "] " +
          data.type +
          " / " +
          data.details +
          loc +
          (data.response ? " HTTP " + data.response.code : ""),
      );

      // bufferStalledError: first-pass live-edge jump.
      // The _recovering guard prevents this from firing during an active
      // hard recovery (which itself triggered the stall via abort).
      if (
        !data.fatal &&
        data.details === Hls.ErrorDetails.BUFFER_STALLED_ERROR
      ) {
        if (_recovering) {
          debugLog(
            "HLS: buffer stalled during recovery — ignoring, waiting for reattach",
          );
          return;
        }
        debugLog("HLS: buffer stalled — live-edge jump (startLoad -1)");
        _showHlsBadge("Recovering...", "#ffc107");
        _lastProgress = Date.now();
        try {
          _hlsInstance.stopLoad();
          _hlsInstance.startLoad(-1);
        } catch (e) {
          debugLog("HLS: startLoad(-1) threw, escalating immediately");
          _hardRecoverHls(src);
        }
        return;
      }

      if (!data.fatal && data.details === Hls.ErrorDetails.FRAG_LOAD_ERROR) {
        debugLog("HLS: frag load error — proxy may have auth/token issues");
        return;
      }
      if (!data.fatal && data.details === Hls.ErrorDetails.FRAG_LOAD_TIMEOUT) {
        debugLog("HLS: frag load timeout");
        return;
      }

      if (
        !data.fatal &&
        data.details === Hls.ErrorDetails.BUFFER_APPENDING_ERROR
      ) {
        debugLog(
          "HLS: MSE append error — SourceBuffer rejected segment, hard recovering",
        );
        _hardRecoverHls(src);
        return;
      }
      if (!data.fatal && data.details === Hls.ErrorDetails.BUFFER_FULL_ERROR) {
        debugLog("HLS: MSE buffer full — jumping to live edge");
        try {
          _hlsInstance.stopLoad();
          _hlsInstance.startLoad(-1);
        } catch (e) {}
        return;
      }

      if (!data.fatal) return;

      if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
        debugLog("HLS: fatal network, recovering");
        _showHlsBadge("Reconnecting...", "#ffc107");
        _hlsInstance.startLoad();
      } else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
        debugLog("HLS: fatal media, recovering");
        _hlsInstance.recoverMediaError();
      } else {
        debugLog("HLS: unrecoverable, restart in 3s");
        _showHlsBadge("Restarting...", "#dc3545");
        if (_hlsRetryTimer) clearTimeout(_hlsRetryTimer);
        _hlsRetryTimer = setTimeout(function () {
          loadHlsStream(src);
        }, 3000);
      }
    });
  } else if (video.canPlayType("application/vnd.apple.mpegurl")) {
    debugLog("HLS: native Safari");
    _hlsVideoEl = video;
    video.src = src;
    video.play().catch(function (e) {
      debugLog("HLS native: " + e.message);
    });
    _showHlsBadge("LIVE", "#dc3545");
  } else {
    _showHlsBadge("HLS not supported", "#dc3545");
  }
}

function _startStallWatchdog(video, src) {
  _lastProgress = Date.now();
  _stopStallWatchdog();
  _stallWatchdog = setInterval(function () {
    if (video.paused || video.ended) return;
    if (_recovering) return; // don't fire during active recovery
    const secs = (Date.now() - _lastProgress) / 1000;
    if (secs > 12) {
      debugLog(
        "HLS: stall " +
          secs.toFixed(1) +
          "s — startLoad(-1) failed, hard recovering",
      );
      _hardRecoverHls(src);
    }
  }, 3000);
}

function _stopStallWatchdog() {
  if (_stallWatchdog) {
    clearInterval(_stallWatchdog);
    _stallWatchdog = null;
  }
}

function closeHlsStream() {
  debugLog("HLS: closing");
  _destroyHls();
  _stopStallWatchdog();
  _removeHlsBadge();
  if (_hlsRetryTimer) {
    clearTimeout(_hlsRetryTimer);
    _hlsRetryTimer = null;
  }
  _recovering = false;

  document.getElementById("prev-btn").disabled = false;
  document.getElementById("next-btn").disabled = false;

  const closeBtn = document.getElementById("iframe-close-btn");
  if (closeBtn) {
    closeBtn.textContent = "Close";
    closeBtn.classList.add("hidden");
    closeBtn._hlsClose = false;
  }

  const audio = document.getElementById("audio-player");
  const video = document.getElementById("video-player");

  video.pause();
  video.removeAttribute("src");
  video.load();
  video.classList.add("hidden");

  audio.classList.remove("hidden");
  window._activeMedia = audio;
  if (window.player) window.player.audio = audio;

  if (_preHlsMedia) {
    const snap = _preHlsMedia;
    _preHlsMedia = null;
    if (snap.audioSrc) {
      audio.src = snap.audioSrc;
      audio.load();
      audio.addEventListener(
        "canplay",
        function () {
          audio.currentTime = snap.currentTime;
          if (!snap.paused) audio.play();
        },
        { once: true },
      );
    }
    if (window.playlistManager && snap.mediaId) {
      window.playlistManager.currentMediaId = snap.mediaId;
      window.playlistManager.render();
    }
  }
  debugLog("HLS: closed");
}

function _destroyHls() {
  if (_hlsInstance) {
    try {
      _hlsInstance.stopLoad();
      _hlsInstance.detachMedia();
      _hlsInstance.destroy();
    } catch (e) {}
    _hlsInstance = null;
  }
  if (_hlsVideoEl) {
    try {
      _hlsVideoEl.pause();
      _hlsVideoEl.removeAttribute("src");
      _hlsVideoEl.load();
    } catch (e) {}
    _hlsVideoEl = null;
  }
}

function _showHlsBadge(text, color) {
  let badge = document.getElementById("hls-live-badge");
  if (!badge) {
    badge = document.createElement("div");
    badge.id = "hls-live-badge";
    badge.style.cssText =
      "position:fixed;top:10px;left:50%;transform:translateX(-50%);background:rgba(0,0,0,0.8);color:#fff;padding:5px 14px;border-radius:4px;font-size:13px;font-weight:bold;font-family:monospace;z-index:9999;pointer-events:none;border-left:4px solid " +
      color;
    document.body.appendChild(badge);
  }
  badge.style.borderLeftColor = color;
  badge.textContent = text;
}

function _removeHlsBadge() {
  const b = document.getElementById("hls-live-badge");
  if (b) b.remove();
}

window.loadHlsStream = loadHlsStream;
window.closeHlsStream = closeHlsStream;
window.isHlsUrl = isHlsUrl;