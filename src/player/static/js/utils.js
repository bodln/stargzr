function debugLog(message) {
  const log = document.getElementById("debug-log");
  const time = new Date().toLocaleTimeString();
  log.innerHTML += `[${time}] ${message}<br>`;
  log.scrollTop = log.scrollHeight;
  console.log(message);
}

function copySessionId() {
  const sessionId = document.getElementById("my-session-id").textContent;
  navigator.clipboard.writeText(sessionId);
  debugLog("Session ID copied to clipboard");
}

/**
 * Shares a link that tunes the recipient straight in —
 * <origin>/stargzr/player?tune=<broadcaster_id> — same link and same
 * ?tune= param the Android app's share button builds and both the web
 * player (see RadioPlayer's pendingUrlTune) and Android's own deep link
 * handling already know how to consume.
 */
async function shareBroadcastLink() {
  const sessionId = document.getElementById("my-session-id").textContent;
  const link = `${window.location.origin}/stargzr/player?tune=${sessionId}`;
  const text = `Tune in to my stargzr broadcast: ${link}`;

  // Web Share API — the native share sheet, where the browser supports it
  // (mobile browsers mainly; most desktop browsers don't).
  if (navigator.share) {
    try {
      await navigator.share({ text, url: link });
      return;
    } catch (err) {
      // AbortError just means the user closed the share sheet — nothing
      // went wrong, don't also dump it to the clipboard on top of that.
      if (err?.name === "AbortError") return;
      debugLog(`Web Share failed, falling back to clipboard: ${err.message}`);
    }
  }

  try {
    await navigator.clipboard.writeText(link);
    debugLog("Broadcast link copied to clipboard");
    alert("Broadcast link copied to clipboard!");
  } catch (err) {
    debugLog(`Could not copy broadcast link: ${err.message}`);
  }
}
