async function fetchAdminState() {
  try {
    const resp = await fetch("/stargzr/player/admin/state");
    const data = await resp.json();

    // Sessions table
    const sessionsBody = document.getElementById("admin-sessions-body");
    if (data.sessions.length === 0) {
      sessionsBody.innerHTML = '<tr><td colspan="3" style="color:#6c757d">None</td></tr>';
    } else {
      const mySessionId = document.getElementById("my-session-id").textContent;
      sessionsBody.innerHTML = data.sessions
        .map((s) => {
          const isMe  = s.session_id === mySessionId ? "is-me" : "";
          const tuned = s.tuned_to
            ? `<span style="color:#007bff">${s.tuned_to}</span>`
            : '<span style="color:#adb5bd">—</span>';
          return `<tr class="${isMe}">
            <td>${s.session_id}</td>
            <td>${s.idle_secs}s</td>
            <td>${tuned}</td>
          </tr>`;
        })
        .join("");
    }

    // Listener map table
    const listenersBody = document.getElementById("admin-listeners-body");
    const entries = Object.entries(data.broadcaster_listeners);
    if (entries.length === 0) {
      listenersBody.innerHTML = '<tr><td colspan="2" style="color:#6c757d">None</td></tr>';
    } else {
      const mySessionId = document.getElementById("my-session-id").textContent;
      listenersBody.innerHTML = entries
        .map(([bid, listeners]) => {
          const llist = listeners.length === 0
            ? '<span style="color:#adb5bd">none</span>'
            : listeners
                .map((l) => `<span style="${l === mySessionId ? "color:#007bff;font-weight:bold" : ""}">${l}</span>`)
                .join("<br>");
          return `<tr><td>${bid}</td><td>${llist}</td></tr>`;
        })
        .join("");
    }

    document.getElementById("admin-last-updated").textContent =
      `Last updated: ${new Date().toLocaleTimeString()}`;
  } catch (err) {
    debugLog(`Admin state fetch failed: ${err.message}`);
  }
}

fetchAdminState();
setInterval(fetchAdminState, 5000);
