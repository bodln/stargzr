class PlaylistManager {
  constructor(sessionId) {
    this.sessionId = sessionId;
    this.medias = [];
    // Preserves the original server order so we can look up the correct
    // numeric index to broadcast. The server's /stream/{index} endpoints
    // use this order, not the user's custom order.
    this.originalMedias = [];
    this.otherFiles = [];
    this.currentMediaId = null;
    this.storageKey = `playlist_order_${sessionId}`;

    // "all" | "audio" | "video" | "other" — gates next/prev navigation only, never blocks direct play
    this.mediaFilter = "all";
  }

  async loadPlaylist() {
    try {
      debugLog("Fetching playlist from server...");
      const [mediaResp, otherResp] = await Promise.all([
        fetch("/stargzr/player/playlist"),
        fetch("/stargzr/player/other-files"),
      ]);
      const serverMedias = await mediaResp.json();
      this.otherFiles    = await otherResp.json();
      this.originalMedias = serverMedias;
      this.medias = [...serverMedias];
      debugLog(`Loaded ${this.medias.length} medias, ${this.otherFiles.length} other file(s)`);
      this.loadCustomOrder();
      // Sync currentMediaId from whatever is actually playing right now.
      // This handles page load where audio src is set server-side and the
      // playlist manager doesn't know about it yet.
      try { this.syncCurrentFromAudio(); } catch (e) { debugLog(`syncCurrentFromAudio: ${e.message}`); }
      this.renderFilterBar();
      this.render();
    } catch (err) {
      debugLog(`Failed to load playlist: ${err.message}`);
      const c = document.getElementById("playlist-display");
      if (c) c.innerHTML = '<div class="loading" style="color:#dc3545">Failed to load playlist. Check the debug log.</div>';
    }
  }

  // Returns true if the given media should use the video element
  isVideo(media) {
    return media?.media_type === "video";
  }

  // Sets currentMediaId by reading the active media element's current src.
  // Must be called after originalMedias is populated so index lookups work.
  syncCurrentFromAudio() {
    // Use the active media element so this works for both audio and video.
    // Falls back to audio-player on initial page load before _activeMedia is set.
    const el = window._activeMedia ?? document.getElementById("audio-player");
    if (!el) return;

    // audio.src returns "" when src is set via a <source> child element
    // rather than directly on the <audio> tag (as on initial page load).
    // Reading the <source> attribute directly is the reliable fallback.
    // el.src on an audio with a <source> child returns the document URL, so
    // we only use it if it actually contains a stream path.
    const rawSrc   = (el.currentSrc || el.src || "");
    const srcToCheck = rawSrc.includes("/stream/")
      ? rawSrc
      : (el.querySelector?.("source")?.getAttribute("src") || "");

    const idMatch    = srcToCheck.match(/\/stream\/id\/([^/?]+)/);
    const indexMatch = srcToCheck.match(/\/stream\/(\d+)(?:[^/]|$)/);

    if (idMatch) {
      this.currentMediaId = idMatch[1];
      debugLog(`Synced current media from audio src (id): ${this.currentMediaId}`);
    } else if (indexMatch) {
      const index = parseInt(indexMatch[1]);
      const media = this.originalMedias[index];
      if (media) {
        this.currentMediaId = media.id;
        debugLog(`Synced current media from audio src (index ${index}): ${media.filename}`);
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
      const validIds = orderIds.filter((id) => this.medias.some((s) => s.id === id));
      const orderedMedias = [];
      validIds.forEach((id) => {
        const media = this.medias.find((s) => s.id === id);
        if (media) orderedMedias.push(media);
      });
      this.medias.forEach((media) => {
        if (!validIds.includes(media.id)) orderedMedias.push(media);
      });
      this.medias = orderedMedias;
      debugLog("Loaded custom playlist order from localStorage");
    } catch (err) {
      debugLog(`Failed to parse saved order: ${err.message}`);
    }
  }

  saveCustomOrder() {
    const orderIds = this.medias.map((s) => s.id);
    localStorage.setItem(this.storageKey, JSON.stringify(orderIds));
    debugLog("Saved custom playlist order to localStorage");
  }

  resetToDefaultOrder() {
    localStorage.removeItem(this.storageKey);
    debugLog("Reset to default playlist order");
    this.loadPlaylist();
  }

  // Returns the server's numeric index for a given media ID.
  // Must match the original unshuffled server order, not the user's custom order.
  getServerIndexById(mediaId) {
    const index = this.originalMedias.findIndex((s) => s.id === mediaId);
    return index >= 0 ? index : 0;
  }

  getMediaIdByServerIndex(index) {
    return this.originalMedias[index]?.id ?? null;
  }

  // Changes the navigation filter and re-renders the playlist.
  // Does not affect direct play clicks, only next/prev traversal.
  setMediaFilter(filter) {
    this.mediaFilter = filter;
    this.renderFilterBar();
    this.render(true);
    debugLog(`Media filter set to: ${filter}`);
  }

  // Returns medias that pass the current navigation filter.
  // Used by next/prev only so direct play is never gated.
  _filteredMedias() {
    if (this.mediaFilter === "all") return this.medias;
    return this.medias.filter((m) =>
      this.mediaFilter === "video" ? this.isVideo(m) : !this.isVideo(m)
    );
  }

  // Renders the filter bar into #playlist-filter-bar which lives outside the scrollable list.
  // Called on load and whenever the active filter changes.
  renderFilterBar() {
    const bar = document.getElementById("playlist-filter-bar");
    if (!bar) return;
    const audioCount = this.medias.filter((m) => !this.isVideo(m)).length;
    const videoCount = this.medias.filter((m) =>  this.isVideo(m)).length;
    const otherCount = this.otherFiles.length;
    bar.innerHTML = `
      <div class="playlist-filter-bar">
        <span class="filter-label">Navigate:</span>
        <button class="filter-btn ${this.mediaFilter === "all"   ? "active" : ""}" onclick="window.playlistManager.setMediaFilter('all')">All (${this.medias.length})</button>
        <button class="filter-btn ${this.mediaFilter === "audio" ? "active" : ""}" onclick="window.playlistManager.setMediaFilter('audio')">&#127925; Audio (${audioCount})</button>
        <button class="filter-btn ${this.mediaFilter === "video" ? "active" : ""}" onclick="window.playlistManager.setMediaFilter('video')">&#127909; Video (${videoCount})</button>
        <button class="filter-btn ${this.mediaFilter === "other" ? "active" : ""}" onclick="window.playlistManager.setMediaFilter('other')">&#128196; Other (${otherCount})</button>
      </div>
    `;
  }

  render(skipScroll = false) {
    const container = document.getElementById("playlist-display");

    // "Other" tab: non-media files and folders, download only, no play or queue buttons.
    // Folders get a zip download via /player/download-folder/{name}.
    // Files get a direct download via /player/download/{name}.
    if (this.mediaFilter === "other") {
      if (this.otherFiles.length === 0) {
        container.innerHTML = '<div class="loading">No other files found</div>';
        return;
      }
      container.innerHTML = this.otherFiles
        .map((f) => {
          const sizeStr = f.is_dir
            ? "folder"
            : f.size > 1024 * 1024
              ? `${(f.size / 1024 / 1024).toFixed(1)} MB`
              : `${(f.size / 1024).toFixed(0)} KB`;

          const icon = f.is_dir ? "&#128193;" : "&#128196;";
          const badgeClass = f.is_dir ? "folder" : "other";

          if (f.is_dir) {
            // Folders must be zipped server-side first which can take a while for large
            // directories. Use a button that shows a status message while waiting instead
            // of a plain <a> that gives no feedback until the browser receives the first byte.
            return `
              <div class="playlist-item" id="folder-item-${CSS.escape(f.filename)}">
                <span class="media-badge ${badgeClass}">${icon}</span>
                <span class="media-name"><span class="media-text">${f.filename}</span></span>
                <span style="font-size:11px;color:#888;margin-left:4px;flex-shrink:0">${sizeStr}</span>
                <span id="folder-zip-status-${CSS.escape(f.filename)}" style="font-size:11px;color:#888;margin-left:8px;flex-shrink:0"></span>
                <button class="action-btn download-btn"
                        id="folder-dl-btn-${CSS.escape(f.filename)}"
                        onclick="downloadFolder('${f.filename.replace(/'/g, "\\'")}')"
                        title="Download ${f.filename} as zip">&#11015;</button>
              </div>
            `;
          }

          return `
            <div class="playlist-item">
              <span class="media-badge ${badgeClass}">${icon}</span>
              <span class="media-name"><span class="media-text">${f.filename}</span></span>
              <span style="font-size:11px;color:#888;margin-left:4px;flex-shrink:0">${sizeStr}</span>
              <a class="action-btn download-btn"
                 href="/stargzr/player/download/${encodeURIComponent(f.filename)}"
                 download="${f.filename}"
                 title="Download ${f.filename}">&#11015;</a>
            </div>
          `;
        })
        .join("");
      return;
    }

    if (this.medias.length === 0) {
      container.innerHTML = '<div class="loading">No medias found</div>';
      return;
    }

    const inRadio    = window.player?.isInRadioMode() ?? false;
    const audioCount = this.medias.filter((m) => !this.isVideo(m)).length;
    const videoCount = this.medias.filter((m) =>  this.isVideo(m)).length;

    const items = this.medias
      .map((media, index) => {
        const isPlaying = media.id === this.currentMediaId;
        const isVid     = this.isVideo(media);
        // Small badge so the user can tell audio and video apart at a glance
        const badge     = `<span class="media-badge ${isVid ? "video" : "audio"}">${isVid ? "&#127909;" : "&#127925;"}</span>`;
        // Items filtered out of the navigation mode are hidden from the list.
        // Direct play is still possible by switching back to All.
        const filtered  = this.mediaFilter !== "all" && (this.mediaFilter === "video") !== isVid;
        if (filtered) return "";
        return `
          <div class="playlist-item ${isPlaying ? "current-playing" : ""} ${isVid ? "is-video" : ""}"
               draggable="true"
               data-media-id="${media.id}"
               data-index="${index}">
            <span class="drag-handle">&#8942;&#8942;</span>
            <span class="media-number">${index + 1}.</span>
            ${badge}
            <span class="media-name"><span class="media-text">${media.filename}</span></span>
            <a class="action-btn download-btn"
               href="/stargzr/player/stream/id/${media.id}"
               download="${media.filename}"
               title="Download ${media.filename}">&#11015;</a>
            <button class="action-btn play-next-btn"
                    onclick="window.playlistManager.playNext_queue('${media.id}')"
                    title="Play after current media"
                    ${inRadio ? "disabled" : ""}>&#9193;</button>
            <button class="action-btn play-media-btn"
                    onclick="window.playlistManager.playMedia('${media.id}')"
                    ${inRadio ? "disabled" : ""}>
              ${isPlaying ? "&#9208;&#65039;" : "&#9654;&#65039;"}
            </button>
          </div>
        `;
      })
      .join("");

    const hiddenCount = this.mediaFilter === "all" ? 0
      : this.medias.filter((m) => (this.mediaFilter === "video") !== this.isVideo(m)).length;
    const hiddenNote = hiddenCount > 0
      ? `<div class="playlist-hidden-note">${hiddenCount} item${hiddenCount > 1 ? "s" : ""} hidden — switch to All to see them</div>`
      : "";

    container.innerHTML = items + hiddenNote;

    this.setupDragAndDrop();
    if (!skipScroll) this.scrollToCurrentMedia();
  }

  setupDragAndDrop() {
    document.querySelectorAll(".playlist-item").forEach((item) => {
      item.addEventListener("dragstart", this.handleDragStart.bind(this));
      item.addEventListener("dragover",  this.handleDragOver.bind(this));
      item.addEventListener("drop",      this.handleDrop.bind(this));
      item.addEventListener("dragend",   this.handleDragEnd.bind(this));
    });

    // Marquee: scroll overflowing media names on hover
    document.querySelectorAll(".media-name").forEach((el) => {
      const span = el.querySelector(".media-text");
      if (!span) return;
      el.addEventListener("mouseenter", () => {
        const overflow = span.scrollWidth - el.clientWidth;
        if (overflow <= 0) return;
        const duration = Math.max(2, overflow / 40);
        span.style.setProperty("--scroll-dist", `-${overflow}px`);
        span.style.animation = `media-marquee ${duration}s ease-in-out infinite`;
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
      const media = this.medias[from];
      this.medias.splice(from, 1);
      this.medias.splice(to, 0, media);
      this.saveCustomOrder();
      this.render(true);
      debugLog(`Moved media from position ${from + 1} to ${to + 1}`);
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
    if (cloneRect.top < cRect.top - scrollThreshold)             container.scrollTop -= scrollSpeed;
    else if (cloneRect.bottom > cRect.bottom + scrollThreshold)  container.scrollTop += scrollSpeed;

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

    const media = this.medias[this.touchDragCurrentIndex];
    this.medias.splice(this.touchDragCurrentIndex, 1);
    this.medias.splice(targetIndex, 0, media);
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
      debugLog(`Moved media from position ${this.touchDragStartIndex + 1} to ${this.touchDragCurrentIndex + 1}`);
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

  scrollToCurrentMedia() {
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

    const items = this.medias
      .map((media, index) => {
        const isPlaying = media.id === this.currentMediaId;
        const isVid     = this.isVideo(media);
        const badge     = `<span class="media-badge ${isVid ? "video" : "audio"}">${isVid ? "&#127909;" : "&#127925;"}</span>`;
        const filtered  = this.mediaFilter !== "all" && (this.mediaFilter === "video") !== isVid;
        if (filtered) return "";
        return `
          <div class="playlist-item ${isPlaying ? "current-playing" : ""} ${isVid ? "is-video" : ""} ${index === activeDragIndex ? "dragging" : ""}"
               draggable="true"
               data-media-id="${media.id}"
               data-index="${index}">
            <span class="drag-handle">&#8942;&#8942;</span>
            <span class="media-number">${index + 1}.</span>
            ${badge}
            <span class="media-name"><span class="media-text">${media.filename}</span></span>
            <a class="action-btn download-btn"
               href="/stargzr/player/stream/id/${media.id}"
               download="${media.filename}"
               title="Download ${media.filename}">&#11015;</a>
            <button class="action-btn play-next-btn"
                    onclick="window.playlistManager.playNext_queue('${media.id}')"
                    title="Play after current media"
                    ${inRadio ? "disabled" : ""}>&#9193;</button>
            <button class="action-btn play-media-btn"
                    onclick="window.playlistManager.playMedia('${media.id}')"
                    ${inRadio ? "disabled" : ""}>${isPlaying ? "&#9208;&#65039;" : "&#9654;&#65039;"}</button>
          </div>
        `;
      })
      .join("");

    container.innerHTML = items;
    this.setupDragAndDrop();
  }

  playMedia(mediaId) {
    if (window.player?.isInRadioMode()) {
      alert("Cannot change medias while in radio mode");
      return;
    }
    this.currentMediaId = mediaId;
    const media = this.medias.find((s) => s.id === mediaId);

    // Switch to the correct media element before setting src
    window.switchMediaElement?.(this.isVideo(media));

    const active = document.getElementById(this.isVideo(media) ? "video-player" : "audio-player");
    active.src = `/stargzr/player/stream/id/${mediaId}`;
    active.play();

    // Update subtitle track whenever a video is loaded.
    // The track src must be set after the video src so the browser loads them together.
    // 404 responses (no subtitles for this file) are silently ignored by the browser.
    if (this.isVideo(media)) {
      const track = document.getElementById("subtitle-track");
      if (track) track.src = `/stargzr/player/subtitles/${mediaId}`;
    }

    if (media) {
      const mediaIndex = this.medias.findIndex((s) => s.id === mediaId);
      document.querySelector("#player-controls .media-info strong").nextSibling.textContent = " " + media.filename;
      document.querySelector("#player-controls div:last-child").textContent = `Track ${mediaIndex + 1} of ${this.medias.length}`;
    }

    this.render();
    debugLog(`Playing: ${media ? media.filename : mediaId} (${this.isVideo(media) ? "video" : "audio"})`);
  }

  // Moves a media to the slot immediately after the currently playing media
  playNext_queue(mediaId) {
    if (window.player?.isInRadioMode()) {
      alert("Cannot change queue while in radio mode");
      return;
    }
    const fromIndex = this.medias.findIndex((s) => s.id === mediaId);
    if (fromIndex === -1) return;

    const currentIndex = this.getCurrentIndex();
    let insertAt = currentIndex + 1;
    if (fromIndex === insertAt) return;

    const media = this.medias[fromIndex];
    this.medias.splice(fromIndex, 1);
    if (fromIndex < insertAt) insertAt--;
    this.medias.splice(insertAt, 0, media);
    this.saveCustomOrder();
    this.render(true);
    debugLog(`Queued "${media.filename}" to play next`);
  }

  getMediaById(mediaId) { return this.medias.find((s) => s.id === mediaId); }
  getCurrentIndex()     { return this.currentMediaId ? this.medias.findIndex((s) => s.id === this.currentMediaId) : 0; }

  // next/prev respect the active navigation filter, wrapping within the filtered subset
  getNextMedia() {
    const pool = this._filteredMedias();
    if (pool.length === 0) return this.medias[(this.getCurrentIndex() + 1) % this.medias.length];
    const currentInPool = pool.findIndex((m) => m.id === this.currentMediaId);
    return pool[(currentInPool + 1) % pool.length];
  }

  getPrevMedia() {
    const pool = this._filteredMedias();
    if (pool.length === 0) {
      const i = this.getCurrentIndex();
      return this.medias[i === 0 ? this.medias.length - 1 : i - 1];
    }
    const currentInPool = pool.findIndex((m) => m.id === this.currentMediaId);
    const prevIndex = currentInPool <= 0 ? pool.length - 1 : currentInPool - 1;
    return pool[prevIndex];
  }

  playNext() { if (!window.player?.isInRadioMode()) { const s = this.getNextMedia(); if (s) this.playMedia(s.id); } }
  playPrev() { if (!window.player?.isInRadioMode()) { const s = this.getPrevMedia(); if (s) this.playMedia(s.id); } }

  updateCurrentFromAudioSrc() {
    // Read from whichever element is currently active
    const el         = window._activeMedia ?? document.getElementById("audio-player");
    const idMatch    = el.src.match(/\/stream\/id\/([^/?]+)/);
    const indexMatch = el.src.match(/\/stream\/(\d+)(?:\?|$)/);

    if (idMatch) {
      this.currentMediaId = idMatch[1];
      this.render();
    } else if (indexMatch) {
      const mediaId = this.getMediaIdByServerIndex(parseInt(indexMatch[1]));
      if (mediaId) { this.currentMediaId = mediaId; this.render(); }
    }
  }
}

function resetPlaylistOrder() {
  if (confirm("Reset playlist to default order?")) {
    window.playlistManager.resetToDefaultOrder();
  }
}

// Triggers a folder zip download with accurate status feedback.
// The server must finish zipping before it sends a single byte, so we use
// fetch() to wait for the complete response. The "Zipping..." label stays up
// for exactly as long as the server is actually working, then flips to
// "Downloading..." while the blob is being saved, then clears when done.
// A hidden <a> with a blob URL is the only way to trigger Save-As from JS
// without navigating away from the page.
async function downloadFolder(filename) {
  const escapedId = CSS.escape(filename);
  const btn       = document.getElementById(`folder-dl-btn-${escapedId}`);
  const status    = document.getElementById(`folder-zip-status-${escapedId}`);

  if (btn)    { btn.disabled = true; btn.textContent = "⏳"; }
  if (status) status.textContent = "Zipping...";
  debugLog(`Zipping folder "${filename}", waiting for server...`);

  try {
    const resp = await fetch(`/stargzr/player/download-folder/${encodeURIComponent(filename)}`);

    if (!resp.ok) {
      throw new Error(`Server returned ${resp.status}`);
    }

    // Server is done zipping — now we're just pulling bytes over the network
    if (status) status.textContent = "Downloading...";
    debugLog(`Folder "${filename}" zipped, downloading...`);

    const blob = await resp.blob();
    const url  = URL.createObjectURL(blob);
    const a    = document.createElement("a");
    a.href     = url;
    a.download = `${filename}.zip`;
    a.style.display = "none";
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);

    // Revoke the blob URL after a short delay so the browser can release the memory
    setTimeout(() => URL.revokeObjectURL(url), 10000);

    debugLog(`Folder "${filename}" download complete`);
  } catch (err) {
    debugLog(`Folder download failed: ${err.message}`);
    if (status) status.textContent = "Failed";
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = "⬇"; }
    setTimeout(() => { if (status) status.textContent = ""; }, 3000);
  }
}