// ─── ntv-verify.js ───────────────────────────────────────────────────────────
//
// Handles ntv.cx embed URLs. These embeds run reCAPTCHA v3 internally and
// whitelist your IP on chevy.soyspace.cyou.
//
// Flow:
//   1. User pastes ntv.cx iframe code or URL into the Embed Stream box
//   2. Embed loads in a small PERSISTENT widget below the Embed Stream section
//      — the embed's built-in JS re-verifies every 20 min automatically,
//        keeping the IP whitelisted for as long as the page is open
//   3. Channel ID row appears (pre-filled from localStorage if used before)
//   4. server_lookup called immediately — no whitelist needed for this
//   5. Manifest probed every 5s until it returns #EXTM3U (IP confirmed whitelisted)
//   6. HLS loads in the main video player; widget stays alive in background

const NTV_M3U8_SERVER    = "chevy.soyspace.cyou";
const NTV_CHANNEL_ID_KEY = "ntv_last_channel_id";

function isNtvUrl(text) {
  if (!text) return false;
  return /ntv\.cx\/embed/i.test(text);
}

function _extractNtvSrc(text) {
  const m = text.match(/src=["']([^"']*ntv\.cx[^"']+)["']/i);
  if (m) return m[1];
  if (/^https?:\/\/ntv\.cx/i.test(text.trim())) return text.trim();
  return null;
}

// ─── Persistent widget ────────────────────────────────────────────────────────
function _getOrCreateWidget() {
  let widget = document.getElementById("ntv-widget");
  if (widget) return widget;

  widget = document.createElement("div");
  widget.id = "ntv-widget";
  widget.style.cssText = "border:1px solid #333;border-radius:6px;padding:10px;"
    + "margin-top:12px;background:#111;display:none;";

  widget.innerHTML = `
    <div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:8px;">
      <span style="font-size:13px;font-weight:bold;color:#ffc107;">📡 Live Stream Verification</span>
      <span id="ntv-status" style="font-size:12px;color:#aaa;">Idle</span>
    </div>
    <iframe id="ntv-verify-iframe"
      style="width:100%;height:180px;border:none;border-radius:4px;background:#000;"
      allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
      allowfullscreen></iframe>
    <div style="display:flex;align-items:center;gap:8px;margin-top:8px;">
      <label style="font-size:12px;color:#aaa;white-space:nowrap;">Channel ID:</label>
      <input id="ntv-channel-input" type="text" placeholder="e.g. premium51"
        style="flex:1;padding:4px 8px;border-radius:3px;border:1px solid #444;
               background:#1a1a1a;color:#fff;font-size:13px;" />
      <button id="ntv-play-btn"
        style="padding:4px 14px;background:#ffc107;color:#000;border:none;
               border-radius:3px;font-size:13px;cursor:pointer;font-weight:bold;">
        ▶ Play
      </button>
    </div>
    <div id="ntv-progress" style="font-size:11px;color:#888;margin-top:6px;min-height:16px;"></div>
  `;

  // Insert after the Embed Stream upload-container
  const embedContainer = document.getElementById("iframe-input")
    ?.closest?.(".upload-container");
  if (embedContainer) embedContainer.after(widget);
  else document.body.appendChild(widget);

  // Event wiring
  widget.querySelector("#ntv-channel-input").addEventListener("keydown", e => {
    if (e.key === "Enter") widget.querySelector("#ntv-play-btn").click();
  });
  widget.querySelector("#ntv-play-btn").addEventListener("click", () => {
    const id = widget.querySelector("#ntv-channel-input").value.trim();
    if (!id) { widget.querySelector("#ntv-channel-input").focus(); return; }
    localStorage.setItem(NTV_CHANNEL_ID_KEY, id);
    _startLookupAndProbe(id);
  });

  return widget;
}

function _setStatus(text, color) {
  const el = document.getElementById("ntv-status");
  if (el) { el.textContent = text; el.style.color = color || "#aaa"; }
}

function _setProgress(text) {
  const el = document.getElementById("ntv-progress");
  if (el) el.textContent = text;
}

// ─── Main entry ───────────────────────────────────────────────────────────────
async function loadNtvEmbed(rawInput) {
  const embedUrl = _extractNtvSrc(rawInput);
  if (!embedUrl) { debugLog("NTV: could not extract embed URL"); return; }

  // Optional inline channel ID: "ntv.cx/embed?t=... | premium51"
  const pipeIdx = rawInput.lastIndexOf("|");
  const inlineId = pipeIdx !== -1 ? rawInput.slice(pipeIdx + 1).trim() : null;

  const widget = _getOrCreateWidget();
  widget.style.display = "block";

  // Load the embed — it runs reCAPTCHA internally and keeps re-verifying
  document.getElementById("ntv-verify-iframe").src = embedUrl;
  _setStatus("Verifying IP...", "#ffc107");
  debugLog("NTV: embed loaded in persistent widget: " + embedUrl);

  // Pre-fill channel ID
  const saved = localStorage.getItem(NTV_CHANNEL_ID_KEY);
  const knownId = inlineId || saved;
  const input = document.getElementById("ntv-channel-input");
  if (knownId) {
    input.value = knownId;
    _setProgress("Waiting for reCAPTCHA to whitelist your IP, then auto-starting...");
    _startLookupAndProbe(knownId);
  } else {
    _setProgress("Enter the channel ID (e.g. premium51) then hit ▶ Play.");
  }
}

// ─── Lookup + probe + launch ──────────────────────────────────────────────────
async function _startLookupAndProbe(channelId) {
  debugLog("NTV: server_lookup for channel_id=" + channelId);
  _setStatus("Looking up server...", "#ffc107");

  let serverKey = null;
  try {
    const r = await fetch(
      "https://" + NTV_M3U8_SERVER + "/server_lookup?channel_id="
        + encodeURIComponent(channelId),
      { credentials: "omit" }
    );
    if (!r.ok) throw new Error("HTTP " + r.status);
    const data = await r.json();
    serverKey = data.server_key;
    debugLog("NTV: server_key = " + serverKey);
  } catch(e) {
    debugLog("NTV: server_lookup failed — " + e.message + " — using nfs fallback");
  }

  const m3u8Url = serverKey === "top1/cdn"
    ? "https://" + NTV_M3U8_SERVER + "/proxy/top1/cdn/" + channelId + "/mono.css"
    : serverKey
      ? "https://" + NTV_M3U8_SERVER + "/proxy/" + serverKey + "/" + channelId + "/mono.css"
      : "https://" + NTV_M3U8_SERVER + "/proxy/nfs/" + channelId + "/mono.css";

  debugLog("NTV: probing " + m3u8Url);

  // Probe every 5s for up to 60s until manifest returns #EXTM3U
  const MAX_ATTEMPTS = 12; // 12 × 5s = 60s
  for (let i = 1; i <= MAX_ATTEMPTS; i++) {
    _setStatus("Verifying... (" + i + "/" + MAX_ATTEMPTS + ")", "#ffc107");
    _setProgress("Probing manifest (attempt " + i + "/" + MAX_ATTEMPTS + ")...");
    try {
      const probe = await fetch(m3u8Url, { credentials: "omit" });
      const text  = await probe.text();
      if (text.trim().startsWith("#EXTM3U")) {
        debugLog("NTV: manifest OK on attempt " + i + " — launching HLS");
        _setStatus("\u2713 Live", "#28a745");
        window.loadHlsStream(m3u8Url);
        // Blank the iframe — reCAPTCHA done, no need to stream video.
        // Pulse it for 20s every 18 min to re-verify without wasting bandwidth.
        const iframe = document.getElementById("ntv-verify-iframe");
        const verifyUrl = iframe ? iframe.src : null;
        if (iframe) iframe.src = "";
        _setProgress("Stream active. Re-verifying silently every 18 min.");
        debugLog("NTV: iframe blanked — no double-streaming");
        if (verifyUrl) _startPulseKeepalive(verifyUrl);
        return;
      }
      const snippet = text.slice(0, 80).replace(/\n/g, " ").trim();
      debugLog("NTV: not ready (attempt " + i + "): " + snippet);
      _setProgress("Not ready yet (" + i + "/" + MAX_ATTEMPTS + "): " + snippet.slice(0, 60));
    } catch(e) {
      debugLog("NTV: probe error (attempt " + i + "): " + e.message);
      _setProgress("Probe error: " + e.message);
    }
    if (i < MAX_ATTEMPTS) await new Promise(r => setTimeout(r, 5000));
  }

  // Give up waiting, try anyway
  debugLog("NTV: verification timed out — loading stream anyway");
  _setStatus("Loading...", "#ffc107");
  _setProgress("Timed out waiting — loading anyway. Try ▶ Play again if it fails.");
  window.loadHlsStream(m3u8Url);
}

// Console shortcut: window.ntvPlay("premium51")
window.ntvPlay = function(channelId) {
  if (channelId) {
    const input = document.getElementById("ntv-channel-input");
    if (input) input.value = channelId;
    localStorage.setItem(NTV_CHANNEL_ID_KEY, channelId);
  }
  const id = channelId
    || document.getElementById("ntv-channel-input")?.value?.trim()
    || localStorage.getItem(NTV_CHANNEL_ID_KEY);
  if (id) _startLookupAndProbe(id);
  else debugLog("NTV: no channel ID — call ntvPlay('premiumXXX')");
};


// ─── Pulse keepalive ─────────────────────────────────────────────────────────
// Reloads ntv.cx iframe for 35s every 15 min — just long enough for reCAPTCHA
// to re-verify, then blanks it. Uses almost no bandwidth vs streaming continuously.
const PULSE_INTERVAL_MS = 15 * 60 * 1000;
const PULSE_ACTIVE_MS   = 35 * 1000;
let _pulseTimer    = null;
let _pulseOffTimer = null;

function _startPulseKeepalive(verifyUrl) {
  if (_pulseTimer) clearInterval(_pulseTimer);
  _pulseTimer = setInterval(function() {
    debugLog("NTV keepalive: pulsing iframe for " + (PULSE_ACTIVE_MS/1000) + "s");
    _setStatus("Re-verifying...", "#ffc107");
    const f = document.getElementById("ntv-verify-iframe");
    if (!f) return;
    f.src = verifyUrl;
    if (_pulseOffTimer) clearTimeout(_pulseOffTimer);
    _pulseOffTimer = setTimeout(function() {
      const ff = document.getElementById("ntv-verify-iframe");
      if (ff) ff.src = "";
      _setStatus("\u2713 Live", "#28a745");
      debugLog("NTV keepalive: iframe blanked again");
    }, PULSE_ACTIVE_MS);
  }, PULSE_INTERVAL_MS);
}

function stopNtvWidget() {
  if (_pulseTimer)    { clearInterval(_pulseTimer);   _pulseTimer    = null; }
  if (_pulseOffTimer) { clearTimeout(_pulseOffTimer); _pulseOffTimer = null; }
  const f = document.getElementById("ntv-verify-iframe");
  if (f) f.src = "";
  const w = document.getElementById("ntv-widget");
  if (w) w.style.display = "none";
}

window.isNtvUrl      = isNtvUrl;
window.loadNtvEmbed  = loadNtvEmbed;
window.stopNtvWidget = stopNtvWidget;