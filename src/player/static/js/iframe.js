// ─── iframe.js ───────────────────────────────────────────────────────────────
//
// Routes:
//   ntv.cx embed code/URL → ntv-verify.js (persistent widget + HLS)
//   HLS / M3U8 URLs       → hls-stream.js
//   Everything else       → regular iframe embed

function extractIframeSrc(text) {
  const m = text.match(/src=["']([^"']+)["']/i);
  if (m) return m[1];
  if (/^https?:\/\//.test(text.trim())) return text.trim();
  return null;
}

let _preIframeMedia = null;

function loadIframe(src, rawInput) {
  // ── ntv.cx embed → verification widget + HLS ────────────────────────────
  if (window.isNtvUrl?.(rawInput || src)) {
    debugLog("ntv.cx embed detected, routing to verification flow");
    window.loadNtvEmbed(rawInput || src);
    document.getElementById("iframe-input").value = "";
    return;
  }

  // ── HLS / M3U8 → HLS player ─────────────────────────────────────────────
  if (window.isHlsUrl?.(src)) {
    debugLog("HLS URL detected, routing to HLS player: " + src);
    window.loadHlsStream(src);
    document.getElementById("iframe-input").value = "";
    return;
  }

  // ── Regular iframe embed ─────────────────────────────────────────────────
  window._activeMedia?.pause();

  const audio = document.getElementById("audio-player");
  const video = document.getElementById("video-player");
  _preIframeMedia =
    !audio.classList.contains("hidden") ? audio :
    !video.classList.contains("hidden") ? video :
    audio;

  audio.classList.add("hidden");
  video.classList.add("hidden");
  document.getElementById("media-resync-btn")?.classList.add("hidden");

  const el = document.getElementById("iframe-player");
  el.src = src;
  el.classList.remove("hidden");

  const closeBtn = document.getElementById("iframe-close-btn");
  if (closeBtn) {
    closeBtn.textContent = "✕ Close";
    closeBtn._hlsClose = false;
    closeBtn.classList.remove("hidden");
  }

  if (!window.player?.isInRadioMode()) {
    document.getElementById("prev-btn").disabled = true;
    document.getElementById("next-btn").disabled = true;
  }

  debugLog("Iframe loaded: " + src);
}

function closeIframe() {
  const closeBtn = document.getElementById("iframe-close-btn");
  if (closeBtn?._hlsClose) {
    window.closeHlsStream();
    return;
  }

  const el = document.getElementById("iframe-player");
  el.src = "";
  el.classList.add("hidden");
  closeBtn?.classList.add("hidden");
  document.getElementById("iframe-input").value = "";

  if (_preIframeMedia) {
    _preIframeMedia.classList.remove("hidden");
    _preIframeMedia = null;
  } else {
    window.switchMediaElement?.(false);
  }

  if (!window.player?.isInRadioMode()) {
    document.getElementById("prev-btn").disabled = false;
    document.getElementById("next-btn").disabled = false;
  }

  debugLog("Iframe closed");
}

window.isIframeActive = () =>
  !document.getElementById("iframe-player").classList.contains("hidden");

// ─── Event bindings ───────────────────────────────────────────────────────────

document.getElementById("iframe-load-btn").addEventListener("click", () => {
  const raw = document.getElementById("iframe-input").value.trim();
  const src = extractIframeSrc(raw);
  if (raw && (window.isNtvUrl?.(raw) || src)) loadIframe(src || raw, raw);
  else debugLog("Could not extract src from input");
});

document.getElementById("iframe-close-btn").addEventListener("click", closeIframe);

document.getElementById("iframe-input").addEventListener("paste", () => {
  setTimeout(() => {
    const raw = document.getElementById("iframe-input").value.trim();
    const src = extractIframeSrc(raw);
    if (raw && (window.isNtvUrl?.(raw) || src)) loadIframe(src || raw, raw);
  }, 0);
});