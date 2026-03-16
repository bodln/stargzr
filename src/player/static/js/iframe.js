// ─── iframe.js ───────────────────────────────────────────────────────────────
//
// Handles both:
//   1. Regular iframe embeds (YouTube, Twitch, etc.)
//   2. HLS / M3U8 streams — routed to hls-stream.js instead of an iframe
//
// HLS URLs detected:
//   - Anything ending in .m3u8
//   - chevy.soyspace.cyou/proxy/...  (disguised as .css)
//   - Any URL containing /mono.css

function extractIframeSrc(text) {
  const m = text.match(/src=["']([^"']+)["']/i);
  if (m) return m[1];
  if (/^https?:\/\//.test(text.trim())) return text.trim();
  return null;
}

// Remembers which media element was visible before the iframe took over
let _preIframeMedia = null;

function loadIframe(src) {
  // ── Route HLS URLs to the dedicated HLS player ──────────────────────────
  if (window.isHlsUrl?.(src)) {
    debugLog(`HLS URL detected, routing to HLS player: ${src}`);
    window.loadHlsStream(src);
    // Clear the input so the user sees it was accepted
    document.getElementById("iframe-input").value = "";
    return;
  }

  // ── Regular iframe embed ─────────────────────────────────────────────────
  window._activeMedia?.pause();

  // Save which element is currently shown so we can restore it on close
  const audio = document.getElementById("audio-player");
  const video = document.getElementById("video-player");
  _preIframeMedia =
    !audio.classList.contains("hidden") ? audio :
    !video.classList.contains("hidden") ? video :
    audio; // fallback

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

  debugLog(`Iframe loaded: ${src}`);
}

function closeIframe() {
  // ── If the close button was claimed by HLS, delegate to HLS close ────────
  const closeBtn = document.getElementById("iframe-close-btn");
  if (closeBtn?._hlsClose) {
    window.closeHlsStream();
    return;
  }

  // ── Regular iframe close ─────────────────────────────────────────────────
  const el = document.getElementById("iframe-player");
  el.src = "";
  el.classList.add("hidden");
  closeBtn?.classList.add("hidden");
  document.getElementById("iframe-input").value = "";

  // Restore whichever element was visible before the iframe
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

// ─── Button event bindings ────────────────────────────────────────────────────

document.getElementById("iframe-load-btn").addEventListener("click", () => {
  const src = extractIframeSrc(
    document.getElementById("iframe-input").value.trim(),
  );
  if (src) loadIframe(src);
  else debugLog("Could not extract src from input");
});

document.getElementById("iframe-close-btn").addEventListener("click", closeIframe);

document.getElementById("iframe-input").addEventListener("paste", () => {
  setTimeout(() => {
    const src = extractIframeSrc(
      document.getElementById("iframe-input").value.trim(),
    );
    if (src) loadIframe(src);
  }, 0);
});