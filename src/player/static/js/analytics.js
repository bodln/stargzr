// Turns an Analytics frame into the stat counters and the live broadcasts list.
// Each broadcaster now carries a username when they are logged in, so the list
// leads with that and keeps the session id as a smaller second line.

// Minimal HTML escape for the few user controlled strings we drop into innerHTML
// here (account names, media filenames).
function esc(s) {
  return String(s ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
      })[c],
  );
}

function shortId(id) {
  return id ? id.slice(0, 8) + "…" : "someone";
}

function updateAnalyticsDisplay(analytics) {
  document.getElementById("stat-connections").textContent  = analytics.active_connections;
  document.getElementById("stat-broadcasters").textContent = analytics.active_broadcasters;
  document.getElementById("stat-listeners").textContent    = analytics.active_listeners;

  // Session id -> account name, read by chat.js and the tune in path so they can
  // show a name instead of a raw id.
  window._broadcasterNames = {};
  for (const b of analytics.broadcasters) {
    if (b.username) window._broadcasterNames[b.broadcaster_id] = b.username;
  }

  // Let other panels know the names may have moved
  document.dispatchEvent(new CustomEvent("broadcastersUpdated"));

  // Keep the "Tuned into" label in step with the names as they arrive
  if (window.player?.tunedBroadcaster) {
    const el = document.getElementById("broadcaster-name");
    if (el) {
      el.textContent =
        window._broadcasterNames[window.player.tunedBroadcaster] ||
        window.player.tunedBroadcaster;
    }
  }

  const broadcastersList = document.getElementById("broadcasters-list");
  if (analytics.broadcasters.length === 0) {
    broadcastersList.innerHTML = '<div class="no-broadcasters">No one is broadcasting right now</div>';
    return;
  }

  broadcastersList.innerHTML = analytics.broadcasters
    .map((b) => {
      const mins    = Math.floor(b.playback_time / 60);
      const secs    = Math.floor(b.playback_time % 60);
      const timeStr = `${mins}:${secs.toString().padStart(2, "0")}`;
      const isSelf    = b.broadcaster_id === window.player?.sessionId;
      const isTunedIn = window.player?.tunedBroadcaster === b.broadcaster_id;

      // Name on top, id underneath. If they never logged in the id is all we
      // have, so it moves up and the second line is dropped.
      const name   = b.username ? esc(b.username) : shortId(b.broadcaster_id);
      const idLine = b.username
        ? `<div class="broadcaster-uuid">${b.broadcaster_id}</div>`
        : "";

      let cardClass = "broadcaster-card";
      if (isSelf)         cardClass += " self-broadcasting";
      else if (isTunedIn) cardClass += " currently-tuned";

      let buttonHTML;
      if (isSelf)         buttonHTML = '<button class="tune-in-btn" disabled>You</button>';
      else if (isTunedIn) buttonHTML = '<button class="tune-in-btn" disabled>Tuned In ✓</button>';
      else                buttonHTML = `<button class="tune-in-btn" onclick="tuneInToBroadcaster('${b.broadcaster_id}')">Tune In 📻</button>`;

      return `
        <div class="${cardClass}">
          <div class="broadcaster-info-card">
            <div class="broadcaster-id">${name}</div>
            ${idLine}
            <div class="broadcaster-media">
              <span class="play-status">${b.is_playing ? "▶️" : "⏸️"}</span>
              <span>${esc(b.media_name)}</span>
            </div>
            <div class="broadcaster-time">
              Track ${b.media_index + 1} • ${timeStr} •
              <strong>${b.listener_count || 0} 👥</strong>
            </div>
          </div>
          ${buttonHTML}
        </div>
      `;
    })
    .join("");
}

function tuneInToBroadcaster(broadcasterId) {
  document.getElementById("broadcaster-input").value = broadcasterId;
  document.getElementById("broadcast-progress").classList.remove("hidden");

  window.player.tuneIn(broadcasterId);

  // Prefer the account name if we have it from the last broadcasts frame
  const label = window._broadcasterNames?.[broadcasterId] || broadcasterId;

  document.getElementById("mode-display").textContent   = "Radio Mode";
  document.getElementById("mode-display").className     = "mode-badge radio";
  document.getElementById("broadcaster-name").textContent = label;
  document.getElementById("broadcaster-info").classList.remove("hidden");
  document.getElementById("tune-in-btn").classList.add("hidden");
  document.getElementById("tune-out-btn").classList.remove("hidden");
  document.getElementById("prev-btn").disabled = true;
  document.getElementById("next-btn").disabled = true;

  debugLog(`Tuned into broadcaster: ${broadcasterId} via broadcaster list`);
}
