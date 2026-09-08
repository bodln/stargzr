// Chat rides on the same WebSocket as the radio sync. Every client already
// receives the global room plus, while tuned in or broadcasting, the matching
// broadcaster room, so switching which one you are looking at is a purely local
// choice. This file owns that choice, sends what you type to the room you have
// selected, and paints only the lines for that room.
//
// The rooms you can reach:
//   always                 -> the global room everyone connected shares
//   tuned into someone      -> their room, shared with everyone tuned to them
//   broadcasting            -> your own room, shared with your listeners
//
// Tuning in only unlocks a room. It does not pin you to it: you can tune in for
// the audio and still sit in global chat. The server validates the room on every
// line, so a client cannot post somewhere it has no access to.
//
// radio-player.js calls window.chatUI.onMessage() for every Chat frame and
// fires a "radioRoomChange" event whenever the tune in / broadcasting state
// moves, which is exactly when the set of reachable rooms changes.

(function () {
  // Matches MAX_CHAT_LEN on the server. The input also has a maxlength, this is
  // just a second guard before we put it on the wire.
  const MAX_LEN = 500;

  // How many lines we keep in the DOM before dropping the oldest.
  const MAX_LINES = 200;

  const messagesEl = () => document.getElementById("chat-messages");
  const inputEl = () => document.getElementById("chat-input");
  const selectEl = () => document.getElementById("chat-room-select");

  // The room the user is currently looking at and posting to. One of "global"
  // or a broadcaster session id. Always kept to a room we can actually reach.
  let activeRoom = "global";
  // True once the user has picked a room from the dropdown by hand. While set we
  // leave their choice alone until it becomes unreachable. Cleared whenever the
  // tune in target changes, since that is an explicit "take me there" action.
  let userPicked = false;
  // Previous tune / broadcast state, so reconcile() can spot the transitions.
  let lastTuned = null;
  let lastBroadcasting = false;

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

  // Name of the broadcaster whose room this is, preferring their account name
  // from the last broadcasts list.
  function broadcasterName(id) {
    return window._broadcasterNames?.[id] || shortId(id);
  }

  // Every room the player can reach right now, in display order, each as
  // { id, label }. Global is always first.
  function accessibleRooms() {
    const p = window.player;
    const rooms = [{ id: "global", label: "Global chat" }];
    if (p?.tunedBroadcaster) {
      rooms.push({
        id: p.tunedBroadcaster,
        label: "Room of " + broadcasterName(p.tunedBroadcaster),
      });
    }
    if (p?.isBroadcasting && p.sessionId && p.sessionId !== p?.tunedBroadcaster) {
      rooms.push({ id: p.sessionId, label: "Your broadcast room" });
    }
    return rooms;
  }

  // What room the player is looking at right now.
  function currentRoomId() {
    return activeRoom;
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

  // Repaint the dropdown from the reachable rooms and select activeRoom. With
  // only the global room to offer we disable it so it reads as a plain label.
  function rebuildSelector() {
    const sel = selectEl();
    if (!sel) return;
    const rooms = accessibleRooms();

    sel.innerHTML = "";
    for (const room of rooms) {
      const opt = document.createElement("option");
      opt.value = room.id;
      opt.textContent = room.label;
      sel.appendChild(opt);
    }
    sel.value = activeRoom;
    sel.disabled = rooms.length <= 1;
  }

  // Keep activeRoom pointed at a room we can reach, following the natural
  // default as tune in / broadcasting state changes, then repaint the dropdown.
  // Wipes the log when the room we are showing actually changes so lines from
  // the room we just left do not sit above the new one.
  function reconcile() {
    const p = window.player;
    const tuned = p?.tunedBroadcaster || null;
    const broadcasting = !!p?.isBroadcasting;
    const ids = accessibleRooms().map((r) => r.id);
    const previous = activeRoom;

    // Tuned into someone new: go to their room. Switching stations is an
    // explicit act, so it overrides an earlier manual pick.
    if (tuned && tuned !== lastTuned) {
      activeRoom = tuned;
      userPicked = false;
    }
    // Started broadcasting without being tuned anywhere: drop into our own room,
    // the same place the old tune-state-only routing would have put us.
    else if (broadcasting && !lastBroadcasting && !tuned && !userPicked) {
      activeRoom = p.sessionId;
    }

    lastTuned = tuned;
    lastBroadcasting = broadcasting;

    // Room we were showing went away (tuned out, broadcaster stopped): fall back
    // to global rather than talking into a room that no longer exists.
    if (!ids.includes(activeRoom)) {
      activeRoom = "global";
      userPicked = false;
    }

    rebuildSelector();

    if (activeRoom !== previous) resetToPlaceholder();
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

    // Name the room explicitly. The server still validates it and will bounce
    // the line to our default room if we somehow lost access in flight.
    ws.send(
      JSON.stringify({
        type: "Chat",
        room: activeRoom,
        text: text.slice(0, MAX_LEN),
      }),
    );
    input.value = "";
    input.focus();
  }

  window.chatUI = {
    onMessage(msg) {
      // We receive every room we can reach, so drop anything that is not for the
      // room we are looking at now.
      if (msg.room && msg.room !== currentRoomId()) return;
      render(msg);
    },

    // Kept for callers that just want the dropdown labels refreshed.
    refreshRoom() {
      rebuildSelector();
    },
  };

  document.getElementById("chat-send-btn")?.addEventListener("click", send);
  inputEl()?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      send();
    }
  });

  // User picked a room by hand: switch to it, remember the choice, and clear the
  // log so the two rooms' lines do not mix.
  selectEl()?.addEventListener("change", (e) => {
    const picked = e.target.value;
    if (picked === activeRoom) return;
    activeRoom = picked;
    userPicked = picked !== "global";
    resetToPlaceholder();
    inputEl()?.focus();
  });

  // Tune in / broadcasting state moved, so the reachable rooms did too.
  document.addEventListener("radioRoomChange", reconcile);

  // The broadcasts list carries account names, so relabel the options when it
  // refreshes in case the broadcaster of a room we can see just logged in.
  document.addEventListener("broadcastersUpdated", () => rebuildSelector());

  rebuildSelector();
})();
