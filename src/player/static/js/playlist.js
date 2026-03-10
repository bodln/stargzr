class PlaylistManager {
  constructor(sessionId) {
    this.sessionId = sessionId;
    this.songs = [];
    // Preserves the original server order so we can look up the correct
    // numeric index to broadcast. The server's /stream/{index} endpoints
    // use this order, not the user's custom order.
    this.originalSongs = [];
    this.currentSongId = null;
    this.storageKey = `playlist_order_${sessionId}`;
  }

  async loadPlaylist() {
    try {
      debugLog("Fetching playlist from server...");
      const response = await fetch("/stargzr/player/playlist");
      const serverSongs = await response.json();
      this.originalSongs = serverSongs;
      this.songs = [...serverSongs];
      debugLog(`Loaded ${this.songs.length} songs`);
      this.loadCustomOrder();
      // Sync currentSongId from whatever is actually playing right now.
      // This handles page load where audio src is set server-side and the
      // playlist manager doesn't know about it yet.
      this.syncCurrentFromAudio();
      this.render();
    } catch (err) {
      debugLog(`Failed to load playlist: ${err.message}`);
    }
  }

  // Returns true if the given song should use the video element
  isVideo(song) {
    return song?.media_type === "video";
  }

  // Sets currentSongId by reading the audio element's current src.
  // Must be called after originalSongs is populated so index lookups work.
  syncCurrentFromAudio() {
    // audio.src returns "" when src is set via a <source> child element
    // rather than directly on the <audio> tag (as on initial page load).
    // Reading the <source> attribute directly is the reliable fallback.
    const audio = document.getElementById("audio-player");
    const srcToCheck =
      audio.currentSrc ||
      audio.src ||
      audio.querySelector("source")?.getAttribute("src") ||
      "";

    const idMatch    = srcToCheck.match(/\/stream\/id\/([^/?]+)/);
    const indexMatch = srcToCheck.match(/\/stream\/(\d+)(?:[^/]|$)/);

    if (idMatch) {
      this.currentSongId = idMatch[1];
      debugLog(`Synced current song from audio src (id): ${this.currentSongId}`);
    } else if (indexMatch) {
      const index = parseInt(indexMatch[1]);
      const song = this.originalSongs[index];
      if (song) {
        this.currentSongId = song.id;
        debugLog(`Synced current song from audio src (index ${index}): ${song.filename}`);
      }
    }
  }

  loadCustomOrder() {
    const savedOrder = localStorage.getItem(this.storageKey);
    if (!savedOrder) {
      debugLog("No custom playlist order found, using default");
      return;
    }
    try {
      const orderIds = JSON.parse(savedOrder);
      const validIds = orderIds.filter((id) => this.songs.some((s) => s.id === id));
      const orderedSongs = [];
      validIds.forEach((id) => {
        const song = this.songs.find((s) => s.id === id);
        if (song) orderedSongs.push(song);
      });
      this.songs.forEach((song) => {
        if (!validIds.includes(song.id)) orderedSongs.push(song);
      });
      this.songs = orderedSongs;
      debugLog("Loaded custom playlist order from localStorage");
    } catch (err) {
      debugLog(`Failed to parse saved order: ${err.message}`);
    }
  }

  saveCustomOrder() {
    const orderIds = this.songs.map((s) => s.id);
    localStorage.setItem(this.storageKey, JSON.stringify(orderIds));
    debugLog("Saved custom playlist order to localStorage");
  }

  resetToDefaultOrder() {
    localStorage.removeItem(this.storageKey);
    debugLog("Reset to default playlist order");
    this.loadPlaylist();
  }

  // Returns the server's numeric index for a given song ID.
  // Must match the original unshuffled server order, not the user's custom order.
  getServerIndexById(songId) {
    const index = this.originalSongs.findIndex((s) => s.id === songId);
    return index >= 0 ? index : 0;
  }

  getSongIdByServerIndex(index) {
    return this.originalSongs[index]?.id ?? null;
  }

  render(skipScroll = false) {
    const container = document.getElementById("playlist-display");
    if (this.songs.length === 0) {
      container.innerHTML = '<div class="loading">No songs found</div>';
      return;
    }

    const inRadio = window.player?.isInRadioMode() ?? false;
    container.innerHTML = this.songs
      .map((song, index) => {
        const isPlaying = song.id === this.currentSongId;
        const isVid     = this.isVideo(song);
        // Small badge so the user can tell audio and video apart at a glance
        const badge     = `<span class="media-badge ${isVid ? "video" : "audio"}">${isVid ? "🎬" : "🎵"}</span>`;
        return `
          <div class="playlist-item ${isPlaying ? "current-playing" : ""} ${isVid ? "is-video" : ""}"
               draggable="true"
               data-song-id="${song.id}"
               data-index="${index}">
            <span class="drag-handle">⋮⋮</span>
            <span class="song-number">${index + 1}.</span>
            ${badge}
            <span class="song-name"><span class="song-text">${song.filename}</span></span>
            <button class="play-next-btn"
                    onclick="window.playlistManager.playNext_queue('${song.id}')"
                    title="Play after current song"
                    ${inRadio ? "disabled" : ""}>
              ⏩ After current
            </button>
            <button class="play-song-btn"
                    onclick="window.playlistManager.playSong('${song.id}')"
                    ${inRadio ? "disabled" : ""}>
              ${isPlaying ? "⏸️ Playing" : "▶️ Play"}
            </button>
          </div>
        `;
      })
      .join("");

    this.setupDragAndDrop();
    if (!skipScroll) this.scrollToCurrentSong();
  }

  setupDragAndDrop() {
    document.querySelectorAll(".playlist-item").forEach((item) => {
      item.addEventListener("dragstart", this.handleDragStart.bind(this));
      item.addEventListener("dragover",  this.handleDragOver.bind(this));
      item.addEventListener("drop",      this.handleDrop.bind(this));
      item.addEventListener("dragend",   this.handleDragEnd.bind(this));
    });

    // Marquee: scroll overflowing song names on hover
    document.querySelectorAll(".song-name").forEach((el) => {
      const span = el.querySelector(".song-text");
      if (!span) return;
      el.addEventListener("mouseenter", () => {
        const overflow = span.scrollWidth - el.clientWidth;
        if (overflow <= 0) return;
        const duration = Math.max(2, overflow / 40);
        span.style.setProperty("--scroll-dist", `-${overflow}px`);
        span.style.animation = `song-marquee ${duration}s ease-in-out infinite`;
      });
      el.addEventListener("mouseleave", () => {
        span.style.animation = "";
        span.style.transform = "";
      });
    });

    // Mobile: touch events only on the handle
    document.querySelectorAll(".drag-handle").forEach((handle) => {
      handle.addEventListener("touchstart",  this.handleTouchStart.bind(this),  { passive: false });
      handle.addEventListener("touchmove",   this.handleTouchMove.bind(this),   { passive: false });
      handle.addEventListener("touchend",    this.handleTouchEnd.bind(this),    { passive: true });
      handle.addEventListener("touchcancel", this.handleTouchCancel.bind(this), { passive: true });
    });
  }

  // --- Desktop drag handlers ---

  handleDragStart(e) {
    this.draggedElement = e.currentTarget;
    e.currentTarget.classList.add("dragging");
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/html", e.currentTarget.innerHTML);
  }

  handleDragOver(e) {
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    return false;
  }

  handleDrop(e) {
    e.stopPropagation();
    const from = parseInt(this.draggedElement.dataset.index);
    const to   = parseInt(e.currentTarget.dataset.index);
    if (from !== to) {
      const song = this.songs[from];
      this.songs.splice(from, 1);
      this.songs.splice(to, 0, song);
      this.saveCustomOrder();
      this.render(true);
      debugLog(`Moved song from position ${from + 1} to ${to + 1}`);
    }
    return false;
  }

  handleDragEnd(e) {
    e.currentTarget.classList.remove("dragging");
    this.draggedElement = null;
  }

  // --- Mobile touch handlers ---

  handleTouchStart(e) {
    const row = e.currentTarget.closest(".playlist-item");
    if (!row) return;
    e.preventDefault();

    this.touchDragElement = row;
    this.touchDragStartIndex   = parseInt(row.dataset.index);
    this.touchDragCurrentIndex = this.touchDragStartIndex;

    const touch = e.touches[0];
    this.touchLastY = touch.clientY;

    const rect = row.getBoundingClientRect();
    this.touchGrabOffsetY = touch.clientY - rect.top;

    this.touchClone = row.cloneNode(true);
    this.touchClone.style.cssText = `
      position: fixed; left: ${rect.left}px; top: ${touch.clientY - this.touchGrabOffsetY}px;
      width: ${rect.width}px; opacity: 0.85; pointer-events: none; z-index: 9999;
      box-shadow: 0 4px 16px rgba(0,0,0,0.25); border-radius: 4px;
      background: #e7f3ff; border: 1px solid #007bff;
    `;
    document.body.appendChild(this.touchClone);
    row.classList.add("dragging");
    this.touchDragActive = true;

    if (navigator.vibrate) navigator.vibrate(30);
  }

  handleTouchMove(e) {
    if (!this.touchDragActive) return;
    e.preventDefault();

    const touch = e.touches[0];
    this.touchLastY = touch.clientY;
    this.touchClone.style.top = `${touch.clientY - this.touchGrabOffsetY}px`;

    // Auto-scroll when clone is out of bounds
    const container = document.getElementById("playlist-display");
    const cRect = container.getBoundingClientRect();
    const cloneRect = this.touchClone.getBoundingClientRect();
    const scrollThreshold = 30;
    const scrollSpeed = 8;
    if (cloneRect.top < cRect.top - scrollThreshold)         container.scrollTop -= scrollSpeed;
    else if (cloneRect.bottom > cRect.bottom + scrollThreshold) container.scrollTop += scrollSpeed;

    this.touchClone.style.display = "none";
    const target = document.elementFromPoint(touch.clientX, touch.clientY);
    this.touchClone.style.display = "";

    const targetItem = target?.closest(".playlist-item");
    if (!targetItem) return;

    const targetIndex = parseInt(targetItem.dataset.index);
    if (isNaN(targetIndex) || targetIndex === this.touchDragCurrentIndex) return;

    // Only swap once the clone's leading edge crosses the midpoint of the target row
    const targetRect = targetItem.getBoundingClientRect();
    const targetMid  = targetRect.top + targetRect.height / 2;
    const movingDown = targetIndex > this.touchDragCurrentIndex;
    if (movingDown && touch.clientY < targetMid) return;
    if (!movingDown && touch.clientY > targetMid) return;

    const song = this.songs[this.touchDragCurrentIndex];
    this.songs.splice(this.touchDragCurrentIndex, 1);
    this.songs.splice(targetIndex, 0, song);
    this.touchDragCurrentIndex = targetIndex;

    if (navigator.vibrate) navigator.vibrate(15);
    this.renderForDrag(targetIndex);
  }

  handleTouchEnd(e) {
    if (!this.touchDragActive) return;
    this.touchDragActive = false;

    this.touchClone?.remove();
    this.touchClone = null;

    document.querySelectorAll(".playlist-item.dragging").forEach((el) => el.classList.remove("dragging"));

    if (this.touchDragCurrentIndex !== this.touchDragStartIndex) {
      this.saveCustomOrder();
      debugLog(`Moved song from position ${this.touchDragStartIndex + 1} to ${this.touchDragCurrentIndex + 1}`);
    }
    this.render(true);
  }

  handleTouchCancel(e) {
    if (!this.touchDragActive) return;
    this.touchDragActive = false;

    this.touchClone?.remove();
    this.touchClone = null;

    document.querySelectorAll(".playlist-item.dragging").forEach((el) => el.classList.remove("dragging"));
    this.render(true);
    debugLog("Touch drag cancelled by browser");
  }

  scrollToCurrentSong() {
    const container  = document.getElementById("playlist-display");
    const activeItem = container.querySelector(".current-playing");
    if (!activeItem) return;

    const cRect = container.getBoundingClientRect();
    const iRect = activeItem.getBoundingClientRect();
    if (iRect.top >= cRect.top && iRect.bottom <= cRect.bottom) return;

    const targetScrollTop = container.scrollTop + (iRect.top - cRect.top) - 16;
    container.scrollTo({ top: Math.max(0, targetScrollTop), behavior: "smooth" });
  }

  // Mid-drag re-render: rebuilds DOM order while preserving the dragging class
  renderForDrag(activeDragIndex) {
    const container = document.getElementById("playlist-display");
    const inRadio   = window.player?.isInRadioMode() ?? false;
    container.innerHTML = this.songs
      .map((song, index) => {
        const isPlaying = song.id === this.currentSongId;
        const isVid     = this.isVideo(song);
        const badge     = `<span class="media-badge ${isVid ? "video" : "audio"}">${isVid ? "🎬" : "🎵"}</span>`;
        return `
          <div class="playlist-item ${isPlaying ? "current-playing" : ""} ${isVid ? "is-video" : ""} ${index === activeDragIndex ? "dragging" : ""}"
               draggable="true"
               data-song-id="${song.id}"
               data-index="${index}">
            <span class="drag-handle">⋮⋮</span>
            <span class="song-number">${index + 1}.</span>
            ${badge}
            <span class="song-name"><span class="song-text">${song.filename}</span></span>
            <button class="play-next-btn"
                    onclick="window.playlistManager.playNext_queue('${song.id}')"
                    title="Play after current song"
                    ${inRadio ? "disabled" : ""}>
              ⏩ Next
            </button>
            <button class="play-song-btn"
                    onclick="window.playlistManager.playSong('${song.id}')"
                    ${inRadio ? "disabled" : ""}>
              ${isPlaying ? "⏸️ Playing" : "▶️ Play"}
            </button>
          </div>
        `;
      })
      .join("");
    this.setupDragAndDrop();
  }

  playSong(songId) {
    if (window.player?.isInRadioMode()) {
      alert("Cannot change songs while in radio mode");
      return;
    }
    this.currentSongId = songId;
    const song = this.songs.find((s) => s.id === songId);

    // Switch to the correct media element before setting src
    window.switchMediaElement?.(this.isVideo(song));

    const active = document.getElementById(this.isVideo(song) ? "video-player" : "audio-player");
    active.src = `/stargzr/player/stream/id/${songId}`;
    active.play();

    if (song) {
      const songIndex = this.songs.findIndex((s) => s.id === songId);
      document.querySelector("#player-controls .song-info strong").nextSibling.textContent = " " + song.filename;
      document.querySelector("#player-controls div:last-child").textContent = `Track ${songIndex + 1} of ${this.songs.length}`;
    }

    this.render();
    debugLog(`Playing: ${song ? song.filename : songId} (${this.isVideo(song) ? "video" : "audio"})`);
  }

  // Moves a song to the slot immediately after the currently playing song
  playNext_queue(songId) {
    if (window.player?.isInRadioMode()) {
      alert("Cannot change queue while in radio mode");
      return;
    }
    const fromIndex = this.songs.findIndex((s) => s.id === songId);
    if (fromIndex === -1) return;

    const currentIndex = this.getCurrentIndex();
    let insertAt = currentIndex + 1;
    if (fromIndex === insertAt) return;

    const song = this.songs[fromIndex];
    this.songs.splice(fromIndex, 1);
    if (fromIndex < insertAt) insertAt--;
    this.songs.splice(insertAt, 0, song);
    this.saveCustomOrder();
    this.render(true);
    debugLog(`Queued "${song.filename}" to play next`);
  }

  getSongById(songId)  { return this.songs.find((s) => s.id === songId); }
  getCurrentIndex()    { return this.currentSongId ? this.songs.findIndex((s) => s.id === this.currentSongId) : 0; }
  getNextSong()        { return this.songs[(this.getCurrentIndex() + 1) % this.songs.length]; }
  getPrevSong()        { const i = this.getCurrentIndex(); return this.songs[i === 0 ? this.songs.length - 1 : i - 1]; }

  playNext() { if (!window.player?.isInRadioMode()) { const s = this.getNextSong(); if (s) this.playSong(s.id); } }
  playPrev() { if (!window.player?.isInRadioMode()) { const s = this.getPrevSong(); if (s) this.playSong(s.id); } }

  updateCurrentFromAudioSrc() {
    // Read from whichever element is currently active
    const el         = window._activeMedia ?? document.getElementById("audio-player");
    const idMatch    = el.src.match(/\/stream\/id\/([^/?]+)/);
    const indexMatch = el.src.match(/\/stream\/(\d+)(?:\?|$)/);

    if (idMatch) {
      this.currentSongId = idMatch[1];
      this.render();
    } else if (indexMatch) {
      const songId = this.getSongIdByServerIndex(parseInt(indexMatch[1]));
      if (songId) { this.currentSongId = songId; this.render(); }
    }
  }
}

function resetPlaylistOrder() {
  if (confirm("Reset playlist to default order?")) {
    window.playlistManager.resetToDefaultOrder();
  }
}