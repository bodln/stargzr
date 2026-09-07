// Chat rides on the same WebSocket as the radio sync. The server decides which
// room a line belongs in from the sender's tune in state, so this file never
// picks a room itself. All it does is send what you type and paint what comes
// back.
//
// The room you are in, and the label shown above the box:
//   tuned into someone      -> their room, shared with everyone tuned to them
//   broadcasting, no tune    -> your own room, shared with your listeners
//   neither                  -> the global room everyone connected shares
//
// radio-player.js calls window.chatUI.onMessage() for every Chat frame and
// fires a "radioRoomChange" event whenever the tune in state moves.

(function () {
  // Matches MAX_CHAT_LEN on the server. The input also has a maxlength, this is
  // just a second guard before we put it on the wire.
  const MAX_LEN = 500;

  // How many lines we keep in the DOM before dropping the oldest.
  const MAX_LINES = 200;

  const messagesEl = () => document.getElementById("chat-messages");
  const inputEl = () => document.getElementById("chat-input");
  const labelEl = () => document.getElementById("chat-room-label");

  // First chunk of a session id, enough to tell people apart without printing a
  // full UUID on every line.
  function shortId(id) {
    return id ? id.slice(0, 8) : "someone";
  }

  // What to call the sender of a line. The server fills from_name when they are
  // logged in, otherwise we fall back to a slice of their session id.
  function senderName(msg) {
    return msg.from_name && msg.from_name.length ? msg.from_name : shortId(msg.from);
  }

  // Name of the broadcaster whose room we are in, preferring their account name
  // from the last broadcasts list.
  function broadcasterName(id) {
    return window._broadcasterNames?.[id] || shortId(id);
  }

  // What room the player is in right now, in the same terms the server uses.
  function currentRoomId() {
    const p = window.player;
    if (!p) return "global";
    if (p.tunedBroadcaster) return p.tunedBroadcaster;
    if (p.isBroadcasting) return p.sessionId;
    return "global";
  }

  function roomLabel() {
    const p = window.player;
    if (p?.tunedBroadcaster) return "Room of " + broadcasterName(p.tunedBroadcaster);
    if (p?.isBroadcasting) return "Your broadcast room";
    return "Global chat";
  }

  // HH:MM off the server timestamp, or now if it is missing.
  function timeStr(ms) {
    const d = ms ? new Date(ms) : new Date();
    return d.toTimeString().slice(0, 5);
  }

  function resetToPlaceholder() {
    const box = messagesEl();
    if (box) {
      box.innerHTML =
        '<div class="chat-empty">No messages yet. Say something.</div>';
    }
  }

  function render(msg) {
    const box = messagesEl();
    if (!box) return;

    // Kick out the placeholder the first time a real line arrives.
    box.querySelector(".chat-empty")?.remove();

    const mine = msg.from === window.player?.sessionId;

    const line = document.createElement("div");
    line.className = "chat-line" + (mine ? " chat-mine" : "");

    const meta = document.createElement("span");
    meta.className = "chat-meta";
    meta.textContent =
      timeStr(msg.server_timestamp_ms) +
      " " +
      senderName(msg) +
      (mine ? " (you)" : "");

    // textContent, never innerHTML, so a message body can never inject markup.
    const text = document.createElement("span");
    text.className = "chat-text";
    text.textContent = msg.text;

    line.append(meta, text);
    box.appendChild(line);

    // Trim the oldest lines so a long lived tab does not grow forever.
    while (box.children.length > MAX_LINES) box.removeChild(box.firstChild);

    box.scrollTop = box.scrollHeight;
  }

  function send() {
    const input = inputEl();
    if (!input) return;

    const text = input.value.trim();
    if (!text) return;

    const ws = window.player?.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      debugLog("Chat not sent, socket is not open");
      return;
    }

    ws.send(JSON.stringify({ type: "Chat", text: text.slice(0, MAX_LEN) }));
    input.value = "";
    input.focus();
  }

  window.chatUI = {
    onMessage(msg) {
      // The server only sends us our own room's chat, but a line can still be
      // in flight when the room changes under us. Drop anything that is not for
      // the room we are looking at now.
      if (msg.room && msg.room !== currentRoomId()) return;
      render(msg);
    },

    refreshRoom() {
      const label = labelEl();
      if (label) label.textContent = roomLabel();
    },
  };

  document.getElementById("chat-send-btn")?.addEventListener("click", send);
  inputEl()?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      send();
    }
  });

  // Wipe the log and relabel on a room change so lines from the room you just
  // left do not sit above the ones from the room you just joined.
  document.addEventListener("radioRoomChange", () => {
    resetToPlaceholder();
    window.chatUI.refreshRoom();
  });

  // The broadcasts list carries account names, so relabel when it refreshes in
  // case the broadcaster of the room we are in just logged in.
  document.addEventListener("broadcastersUpdated", () => window.chatUI.refreshRoom());

  window.chatUI.refreshRoom();
})();
