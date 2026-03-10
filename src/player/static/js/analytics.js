function updateAnalyticsDisplay(analytics) {
  document.getElementById("stat-connections").textContent  = analytics.active_connections;
  document.getElementById("stat-broadcasters").textContent = analytics.active_broadcasters;
  document.getElementById("stat-listeners").textContent    = analytics.active_listeners;

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
            <div class="broadcaster-id">${b.broadcaster_id}</div>
            <div class="broadcaster-media">
              <span class="play-status">${b.is_playing ? "▶️" : "⏸️"}</span>
              <span>${b.media_name}</span>
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

  document.getElementById("mode-display").textContent   = "Radio Mode";
  document.getElementById("mode-display").className     = "mode-badge radio";
  document.getElementById("broadcaster-name").textContent = broadcasterId;
  document.getElementById("broadcaster-info").classList.remove("hidden");
  document.getElementById("tune-in-btn").classList.add("hidden");
  document.getElementById("tune-out-btn").classList.remove("hidden");
  document.getElementById("prev-btn").disabled = true;
  document.getElementById("next-btn").disabled = true;

  debugLog(`Tuned into broadcaster: ${broadcasterId} via broadcaster list`);
}
