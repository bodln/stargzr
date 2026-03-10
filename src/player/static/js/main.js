// ─── Audio element & seek-aware play() patch ───────────────────────────────

const audio        = document.getElementById("audio-player");
const originalPlay = audio.play.bind(audio);
let seekPending    = false;

// Bluetooth mode: mute during seeks/song-changes to prevent DSP glitches.
// Persisted in localStorage so it survives page reload.
const btToggle = document.getElementById("bt-mode-toggle");
btToggle.checked = localStorage.getItem("bt_mode") === "true";
btToggle.addEventListener("change", () => {
  localStorage.setItem("bt_mode", btToggle.checked);
  debugLog(`Bluetooth mode ${btToggle.checked ? "enabled" : "disabled"}`);
});

let btMuted = false;
const btMute = () => { if (!btToggle.checked) return; btMuted = true; audio.muted = true; };
const btUnmuteOnPlaying = () => {
  if (!btMuted) return;
  setTimeout(() => { btMuted = false; audio.muted = false; }, 1000);
};

audio.addEventListener("seeking", () => { seekPending = true; btMute(); });
audio.addEventListener("seeked",  () => { seekPending = false; });
audio.addEventListener("playing", () => btUnmuteOnPlaying());

// Defer play() until the browser has landed on the seek target,
// preventing a bump when play is called right after setting currentTime.
audio.play = () => {
  if (!seekPending) return originalPlay();
  return new Promise((resolve, reject) => {
    audio.addEventListener("seeked", () => originalPlay().then(resolve).catch(reject), { once: true });
  });
};

// ─── Core instances ────────────────────────────────────────────────────────

const sessionId     = document.getElementById("my-session-id").textContent;
const player        = new RadioPlayer(audio, sessionId);
const playlistManager = new PlaylistManager(sessionId);

window.player          = player;
window.playlistManager = playlistManager;

player.connectWebSocket();

// Check session validity on page load
(async () => {
  try {
    const resp = await fetch("/stargzr/player/session/check");
    if (resp.status === 401) { debugLog("Session expired, reloading to get a fresh one..."); window.location.reload(); }
  } catch (_) {}
})();

// ─── Button event bindings ─────────────────────────────────────────────────

document.getElementById("tune-in-btn").addEventListener("click", () => {
  const broadcasterId = document.getElementById("broadcaster-input").value.trim();
  if (!broadcasterId) { alert("Please enter a broadcaster session ID"); return; }

  debugLog(`Tuning into broadcaster: ${broadcasterId}`);
  player.tuneIn(broadcasterId);

  document.getElementById("mode-display").textContent     = "Radio Mode";
  document.getElementById("mode-display").className       = "mode-badge radio";
  document.getElementById("broadcaster-name").textContent = broadcasterId;
  document.getElementById("broadcaster-info").classList.remove("hidden");
  document.getElementById("tune-in-btn").classList.add("hidden");
  document.getElementById("tune-out-btn").classList.remove("hidden");
  document.getElementById("prev-btn").disabled = true;
  document.getElementById("next-btn").disabled = true;
});

document.getElementById("tune-out-btn").addEventListener("click",       () => player.tuneOut());
document.getElementById("broadcast-btn").addEventListener("click",      () => player.startBroadcasting());
document.getElementById("stop-broadcast-btn").addEventListener("click", () => player.stopBroadcasting());

document.getElementById("prev-btn").addEventListener("click", () => {
  if (player.isInRadioMode()) { alert("Controls disabled while tuned into a radio"); return; }
  playlistManager.playPrev();
});

document.getElementById("next-btn").addEventListener("click", () => {
  if (player.isInRadioMode()) { alert("Controls disabled while tuned into a radio"); return; }
  playlistManager.playNext();
});

// ─── Audio event handlers ──────────────────────────────────────────────────

audio.addEventListener("ended", () => {
  if (player.isInRadioMode()) { debugLog("Track ended in radio mode, waiting for AutoNext or next Sync"); return; }

  if (player.isBroadcasting) {
    const nextSong = playlistManager.getNextSong();
    if (nextSong) player.sendAutoNext(playlistManager.getServerIndexById(nextSong.id));
  }

  debugLog("Track ended, moving to next track");
  playlistManager.playNext();
});

// Sync playlist highlight whenever audio src changes
audio.addEventListener("loadstart", () => playlistManager.updateCurrentFromAudioSrc());

// Progress bar while tuned in
audio.addEventListener("timeupdate", () => {
  if (!player.isInRadioMode()) return;
  const time     = audio.currentTime;
  const duration = isFinite(audio.duration) ? audio.duration : 0;
  if (duration > 0) {
    document.getElementById("progress-bar-fill").style.width = (time / duration) * 100 + "%";
  }
  const mins = Math.floor(time / 60);
  const secs = Math.floor(time % 60).toString().padStart(2, "0");
  document.getElementById("progress-time-display").textContent = `${mins}:${secs}`;
});

// Session check on play
audio.addEventListener("play", async () => {
  try {
    const resp = await fetch("/stargzr/player/session/check");
    if (resp.status === 401) { audio.pause(); alert("Your session has expired. Please refresh the page."); }
  } catch (_) {}
});

// ─── Page lifecycle ────────────────────────────────────────────────────────

window.addEventListener("beforeunload", () => window.player?.disconnect());
window.addEventListener("pagehide", () => {
  if (!window.player) return;
  debugLog("Page hide detected - cleaning up");
  if (window.player.isBroadcasting) window.player.stopBroadcasting();
  window.player.disconnect();
});

// ─── Bootstrap ────────────────────────────────────────────────────────────

playlistManager.loadPlaylist();
