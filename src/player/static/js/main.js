// ─── Media element switcher ──────────────────────────────────────────────────
//
// The page has both an <audio> and a <video> element. Only one is visible at a
// time. switchMediaElement() swaps them and keeps window._activeMedia up to
// date so every other piece of code can talk to one reference and not care
// which element is underneath.

const audioEl = document.getElementById("audio-player");
const videoEl = document.getElementById("video-player");

window._activeMedia = audioEl;
videoEl.classList.add("hidden");

window.switchMediaElement = function switchMediaElement(toVideo) {
  const incoming = toVideo ? videoEl : audioEl;
  const outgoing = toVideo ? audioEl : videoEl;

  if (incoming === window._activeMedia) return;

  outgoing.pause();
  outgoing.src = "";
  outgoing.classList.add("hidden");

  incoming.classList.remove("hidden");
  window._activeMedia = incoming;

  // Keep RadioPlayer pointed at the active element
  if (window.player) window.player.audio = incoming;

  // Move the single resync button to sit after whichever element just became active,
  // and keep it hidden unless we're already in radio mode
  const resyncBtn = document.getElementById("media-resync-btn");
  if (resyncBtn) {
    incoming.after(resyncBtn);
    resyncBtn.classList.toggle("hidden", !window.player?.isInRadioMode());
  }

  debugLog(`Switched to ${toVideo ? "video" : "audio"} element`);
};

// ─── Audio element & seek-aware play() patch ──────────────────────────────────────────────────────

const originalPlayAudio = audioEl.play.bind(audioEl);
const originalPlayVideo = videoEl.play.bind(videoEl);
let seekPending = false;

// Bluetooth mode: mute during seeks/media-changes to prevent DSP glitches.
// Persisted in localStorage so it survives page reload.
const btToggle = document.getElementById("bt-mode-toggle");
btToggle.checked = localStorage.getItem("bt_mode") === "true";
btToggle.addEventListener("change", () => {
  localStorage.setItem("bt_mode", btToggle.checked);
  debugLog(`Bluetooth mode ${btToggle.checked ? "enabled" : "disabled"}`);
});

let btMuted = false;
const btMute = () => {
  if (!btToggle.checked) return;
  btMuted = true;
  window._activeMedia.muted = true;
};
const btUnmuteOnPlaying = () => {
  if (!btMuted) return;
  setTimeout(() => {
    btMuted = false;
    window._activeMedia.muted = false;
  }, 1000);
};

// Patch both elements so the seek-aware deferred play works regardless of which is active
function patchPlay(el, originalPlay) {
  el.play = () => {
    if (!seekPending) return originalPlay();
    return new Promise((resolve, reject) => {
      el.addEventListener(
        "seeked",
        () => originalPlay().then(resolve).catch(reject),
        { once: true },
      );
    });
  };
}

patchPlay(audioEl, originalPlayAudio);
patchPlay(videoEl, originalPlayVideo);

// ─── Shared event handler wiring ──────────────────────────────────────────────────────────────────
//
// Both elements need the same seeking/seeked/playing/ended/etc. handlers.
// We wire them to a shared set of callbacks so behaviour is identical
// regardless of which element is active.

function wireMediaEvents(el) {
  el.addEventListener("seeking", () => {
    seekPending = true;
    // audio.currentTime still holds the old position when seeking fires.
    // Deferring with setTimeout(0) lets the browser update currentTime to the
    // seek target first, so we can accurately skip muting when seeking to the start.
    setTimeout(() => {
      if (el.currentTime > 0.5) btMute();
    }, 0);
  });

  el.addEventListener("seeked", () => {
    seekPending = false;
  });
  el.addEventListener("playing", () => btUnmuteOnPlaying());

  el.addEventListener("ended", () => {
    if (window.player?.isInRadioMode()) {
      debugLog("Track ended in radio mode, waiting for AutoNext or next Sync");
      return;
    }

    if (window.player?.isBroadcasting) {
      const nextMedia = window.playlistManager?.getNextMedia();
      if (nextMedia)
        window.player.sendAutoNext(
          window.playlistManager.getServerIndexById(nextMedia.id),
        );
    }

    debugLog("Track ended, moving to next track");
    window.playlistManager?.playNext();
  });

  // Sync playlist highlight whenever src changes
  el.addEventListener("loadstart", () =>
    window.playlistManager?.updateCurrentFromAudioSrc(),
  );

  // Progress bar while tuned in
  el.addEventListener("timeupdate", () => {
    if (!window.player?.isInRadioMode()) return;
    const time = el.currentTime;
    const duration = isFinite(el.duration) ? el.duration : 0;
    if (duration > 0) {
      document.getElementById("progress-bar-fill").style.width =
        (time / duration) * 100 + "%";
    }
    const mins = Math.floor(time / 60);
    const secs = Math.floor(time % 60)
      .toString()
      .padStart(2, "0");
    document.getElementById("progress-time-display").textContent =
      `${mins}:${secs}`;
  });

  // Session check on play
  el.addEventListener("play", async () => {
    try {
      const resp = await fetch("/stargzr/player/session/check");
      if (resp.status === 401) {
        el.pause();
        alert("Your session has expired. Please refresh the page.");
      }
    } catch (_) {}
  });
}

wireMediaEvents(audioEl);
wireMediaEvents(videoEl);

// ─── Core instances ────────────────────────────────────────────────────────────────────────────────

const sessionId = document.getElementById("my-session-id").textContent;
const player = new RadioPlayer(audioEl, sessionId);
const playlistManager = new PlaylistManager(sessionId);

window.player = player;
window.playlistManager = playlistManager;

player.connectWebSocket();

// Check session validity on page load
(async () => {
  try {
    const resp = await fetch("/stargzr/player/session/check");
    if (resp.status === 401) {
      debugLog("Session expired, reloading to get a fresh one...");
      window.location.reload();
    }
  } catch (_) {}
})();

// ─── Button event bindings ────────────────────────────────────────────────────────────────────────

document.getElementById("tune-in-btn").addEventListener("click", () => {
  const broadcasterId = document
    .getElementById("broadcaster-input")
    .value.trim();
  if (!broadcasterId) {
    alert("Please enter a broadcaster session ID");
    return;
  }

  debugLog(`Tuning into broadcaster: ${broadcasterId}`);
  player.tuneIn(broadcasterId);

  document.getElementById("mode-display").textContent = "Radio Mode";
  document.getElementById("mode-display").className = "mode-badge radio";
  document.getElementById("broadcaster-name").textContent = broadcasterId;
  document.getElementById("broadcaster-info").classList.remove("hidden");
  document.getElementById("tune-in-btn").classList.add("hidden");
  document.getElementById("tune-out-btn").classList.remove("hidden");
  document.getElementById("prev-btn").disabled = true;
  document.getElementById("next-btn").disabled = true;
});

document
  .getElementById("tune-out-btn")
  .addEventListener("click", () => player.tuneOut());
document
  .getElementById("broadcast-btn")
  .addEventListener("click", () => player.startBroadcasting());
document
  .getElementById("stop-broadcast-btn")
  .addEventListener("click", () => player.stopBroadcasting());

document.getElementById("prev-btn").addEventListener("click", () => {
  if (player.isInRadioMode()) {
    alert("Controls disabled while tuned into a radio");
    return;
  }
  playlistManager.playPrev();
});

document.getElementById("next-btn").addEventListener("click", () => {
  if (player.isInRadioMode()) {
    alert("Controls disabled while tuned into a radio");
    return;
  }
  playlistManager.playNext();
});

// ─── Page lifecycle ────────────────────────────────────────────────────────────────────────────────

window.addEventListener("beforeunload", () => window.player?.disconnect());
window.addEventListener("pagehide", () => {
  if (!window.player) return;
  debugLog("Page hide detected - cleaning up");
  if (window.player.isBroadcasting) window.player.stopBroadcasting();
  window.player.disconnect();
});

// ─── Bootstrap ────────────────────────────────────────────────────────────────────────────────────

playlistManager.loadPlaylist();