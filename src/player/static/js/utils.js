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
