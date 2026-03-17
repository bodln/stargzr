const HLS_JS_CDN = "https://cdn.jsdelivr.net/npm/hls.js@1.5.13/dist/hls.min.js";

let _hlsInstance = null;
let _hlsVideoEl = null;
let _preHlsMedia = null;
let _hlsRetryTimer = null;
let _stallWatchdog = null;
let _lastProgress = 0;
let _hlsSrc = null;
let _keyCache = {};
let _recovering = false;

// ─── PNG header stripping ─────────────────────────────────────────────────────
//
// The proxy serves TS segments disguised as PNG files. Some segments have a
// real PNG file header prepended before the MPEG-TS payload:
//
//   bytes 0-7:  \x89 P N G \r \n \x1a \n   ← PNG magic
//   bytes 8+:   ... PNG chunks ...
//   somewhere:  \x47 ...                    ← first TS sync byte
//
// Desktop HLS.js scans for the first sync byte so it silently works.
// Mobile HLS.js (Chrome/Firefox Android) validates byte 0 strictly,
// sees 0x89 instead of 0x47, and throws fragParsingError.
//
// We scan for the first TS sync byte PAIR (0x47 at N, 0x47 at N+188)
// which unambiguously marks the start of the TS payload, then slice off
// everything before it. If byte 0 is already 0x47 it's returned untouched.
const TS_SYNC = 0x47;
const TS_PKT_SIZE = 188;
const PNG_MAGIC = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

function _stripPngHeader(buffer) {
  const view = new Uint8Array(buffer);

  // Fast path: already starts with TS sync byte
  if (view[0] === TS_SYNC) return buffer;

  // Only bother scanning if it's actually a PNG
  const hasPngMagic = PNG_MAGIC.every((b, i) => view[i] === b);
  if (!hasPngMagic) return buffer;

  // Find first valid TS sync-byte pair
  const limit = view.length - TS_PKT_SIZE;
  for (let i = 8; i < limit; i++) {
    if (view[i] === TS_SYNC && view[i + TS_PKT_SIZE] === TS_SYNC) {
      debugLog("HLS: stripped " + i + "b PNG header from segment");
      return buffer.slice(i);
    }
  }

  debugLog(
    "HLS: PNG magic found but no TS sync pair — passing through unchanged",
  );
  return buffer;
}

// ─── Cache-bust helper ────────────────────────────────────────────────────────
// Applied to FRAGMENT URLs only. Never touch the manifest URL — the server
// ties the reCAPTCHA session to the exact URL string and will reject any
// modification of it, including added query params.
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

// ─── Unified loader ───────────────────────────────────────────────────────────
// manifest/level : add Cache-Control: no-cache (URL untouched)
// fragment       : cache-bust URL + force arraybuffer + strip PNG header
// key/other      : pass through unmodified
function makeFragLoaderClass() {
  const DefaultLoader = Hls.DefaultConfig.loader;

  class UnifiedLoader extends DefaultLoader {
    constructor(config) {
      super(config);
    }

    load(context, config, callbacks) {
      const type = context.type;

      if (type === "manifest" || type === "level") {
        const origXhrSetup = config.xhrSetup;
        return super.load(
          context,
          Object.assign({}, config, {
            xhrSetup: function (xhr, url) {
              xhr.setRequestHeader("Cache-Control", "no-cache");
              xhr.setRequestHeader("Pragma", "no-cache");
              if (origXhrSetup) origXhrSetup(xhr, url);
            },
          }),
          callbacks,
        );
      }

      if (type === "fragment") {
        const bustedContext = Object.assign({}, context, {
          url: _cacheBust(context.url),
        });

        const origSuccess = callbacks.onSuccess;
        const wrappedCallbacks = Object.assign({}, callbacks, {
          onSuccess: function (response, stats, ctx, networkDetails) {
            if (response && response.data instanceof ArrayBuffer) {
              response = Object.assign({}, response, {
                data: _stripPngHeader(response.data),
              });
            }
            origSuccess(response, stats, ctx, networkDetails);
          },
        });

        const origXhrSetup = config.xhrSetup;
        return super.load(
          bustedContext,
          Object.assign({}, config, {
            xhrSetup: function (xhr, url) {
              xhr.responseType = "arraybuffer";
              if (origXhrSetup) origXhrSetup(xhr, url);
            },
          }),
          wrappedCallbacks,
        );
      }

      return super.load(context, config, callbacks);
    }
  }

  return UnifiedLoader;
}

// ─── Hard recovery: detach + reattach ────────────────────────────────────────
function _hardRecoverHls(src) {
  if (_recovering) {
    debugLog("HLS: recovery in progress, skipping");
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
  _recovering = false;
  debugLog("HLS: loading stream: " + src);
  _showHlsBadge("Loading...", "#ffc107");

  try {
    await loadHlsLibrary();
  } catch (e) {
    _showHlsBadge("HLS.js failed to load", "#dc3545");
    debugLog("HLS: " + e.message);
    return;
  }

  // Use the URL exactly as pasted — the server tied a reCAPTCHA session to
  // this exact string. Do NOT call server_lookup or modify this URL in any way.
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
      // Must be false — when true, HLS.js transfers the ArrayBuffer to a Web
      // Worker via postMessage() as a transferable before our onSuccess callback
      // runs, so the worker receives the original PNG-wrapped bytes and throws
      // fragParsingError. With the worker disabled, demuxing happens on the
      // main thread and _stripPngHeader() is guaranteed to run first.
      enableWorker: false,
      lowLatencyMode: false,
    });

    _hlsVideoEl = video;
    _hlsInstance.attachMedia(video);

    _hlsInstance.on(Hls.Events.MEDIA_ATTACHED, function () {
      debugLog("HLS: media attached, loading source");
      _hlsInstance.loadSource(src);
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

      if (
        !data.fatal &&
        data.details === Hls.ErrorDetails.BUFFER_STALLED_ERROR
      ) {
        if (_recovering) {
          debugLog("HLS: stall during recovery — ignoring");
          return;
        }
        debugLog("HLS: buffer stalled — live-edge jump");
        _showHlsBadge("Recovering...", "#ffc107");
        _lastProgress = Date.now();
        try {
          _hlsInstance.stopLoad();
          _hlsInstance.startLoad(-1);
        } catch (e) {
          _hardRecoverHls(src);
        }
        return;
      }

      if (!data.fatal && data.details === Hls.ErrorDetails.FRAG_PARSING_ERROR) {
        debugLog("HLS: frag parsing error — hard recovering");
        _hardRecoverHls(src);
        return;
      }
      if (!data.fatal && data.details === Hls.ErrorDetails.FRAG_LOAD_ERROR) {
        debugLog("HLS: frag load error");
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
        debugLog("HLS: MSE append error — hard recovering");
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
    // iOS native HLS — PNG stripping not available (browser fetches segments
    // itself, bypassing our loader). May fail on .png-wrapped segments.
    debugLog("HLS: native Safari — PNG stripping unavailable");
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
    if (video.paused || video.ended || _recovering) return;
    const secs = (Date.now() - _lastProgress) / 1000;
    if (secs > 12) {
      debugLog("HLS: stall " + secs.toFixed(1) + "s — hard recovering");
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

//TODO
// attempt and fix this stuff for mobile
// make the hls we have more effcient somehow
// try and make it so sa y adevice with good internet can stream a hls and add a lot of buffer and broadcast such stream so that weaker internet clients can watch smoothly, basically have a device buffer as much as possible and then  distribute to weaker ones
