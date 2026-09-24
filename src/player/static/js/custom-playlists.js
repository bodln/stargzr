// Custom, per-account playlists. Rides the /stargzr/player/playlists REST
// endpoints (see playlists.rs), account-gated on window.auth from auth.js.
// The ⭐ button on each row in the main playlist (playlist.js) opens the
// small anchored menu at the bottom of this file to add that track to one.

class CustomPlaylistManager {
  constructor() {
    this.playlists = []; // [{id, name, item_count}]
    this.activeId = null;
    this.activeDetail = null; // {id, name, items: [MediaInfo]}
    this._outsideCloseHandler = null;
  }

  isSignedIn() {
    return !!window.auth?.isLoggedIn();
  }

  // Flips between the signed-in and signed-out panels and (re)loads the list
  // when signing in. Called once at parse time and again on every
  // "authChange" event auth.js fires.
  async refreshVisibility() {
    const signedIn = this.isSignedIn();
    document
      .getElementById("custom-playlists-signed-out")
      ?.classList.toggle("hidden", signedIn);
    document
      .getElementById("custom-playlists-signed-in")
      ?.classList.toggle("hidden", !signedIn);

    if (signedIn) {
      await this.loadPlaylists();
    } else {
      this.playlists = [];
      this.activeId = null;
      this.activeDetail = null;
      this.renderSelect();
      this.renderDetail();
    }
  }

  async loadPlaylists() {
    try {
      const resp = await fetch("/stargzr/player/playlists");
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      this.playlists = await resp.json();
      this.renderSelect();

      // Keep whatever was open, open — unless it got deleted elsewhere
      if (this.activeId && this.playlists.some((p) => p.id === this.activeId)) {
        await this.openPlaylist(this.activeId, /* silent */ true);
      } else {
        this.activeId = null;
        this.activeDetail = null;
        this.renderDetail();
      }
    } catch (err) {
      debugLog(`Failed to load custom playlists: ${err.message}`);
    }
  }

  renderSelect() {
    const select = document.getElementById("custom-playlist-select");
    if (!select) return;
    const current = this.activeId ?? "";
    select.innerHTML =
      '<option value="">Select a playlist…</option>' +
      this.playlists
        .map(
          (p) =>
            `<option value="${p.id}">${escapeHtml(p.name)} (${p.item_count})</option>`,
        )
        .join("");
    select.value = current;
  }

  async createPlaylist() {
    const name = prompt("Playlist name?");
    if (!name || !name.trim()) return;
    try {
      const resp = await fetch("/stargzr/player/playlists", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: name.trim() }),
      });
      const data = await resp.json().catch(() => ({}));
      if (!resp.ok) {
        alert(data.error || "Could not create playlist");
        return;
      }
      debugLog(`Created playlist "${data.name}"`);
      await this.loadPlaylists();
      await this.openPlaylist(data.id);
    } catch (err) {
      alert("Could not reach the server");
    }
  }

  async openPlaylist(id, silent = false) {
    this.activeId = id;
    const select = document.getElementById("custom-playlist-select");
    if (select) select.value = id;
    try {
      const resp = await fetch(`/stargzr/player/playlists/${id}`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      this.activeDetail = await resp.json();
      this.renderDetail();
    } catch (err) {
      if (!silent) alert("Could not load that playlist");
      debugLog(`Failed to open playlist ${id}: ${err.message}`);
    }
  }

  renderDetail() {
    const box = document.getElementById("custom-playlist-detail");
    const nameEl = document.getElementById("custom-playlist-detail-name");
    const itemsEl = document.getElementById("custom-playlist-items");
    if (!box || !nameEl || !itemsEl) return;

    if (!this.activeDetail) {
      box.classList.add("hidden");
      return;
    }
    box.classList.remove("hidden");
    nameEl.textContent = this.activeDetail.name;

    if (this.activeDetail.items.length === 0) {
      itemsEl.innerHTML =
        '<div class="loading">No tracks yet — use &#11088; on a track below to add it here</div>';
      return;
    }

    const inRadio = window.player?.isInRadioMode() ?? false;
    itemsEl.innerHTML = this.activeDetail.items
      .map((media, index) => {
        const isVid = media.media_type === "video";
        const badge = `<span class="media-badge ${isVid ? "video" : "audio"}">${isVid ? "&#127909;" : "&#127925;"}</span>`;
        return `
          <div class="playlist-item">
            <span class="media-number">${index + 1}.</span>
            ${badge}
            <span class="media-name"><span class="media-text">${escapeHtml(media.filename)}</span></span>
            <button class="action-btn"
                    onclick="window.customPlaylists.removeItem('${media.id}')"
                    title="Remove from this playlist">&#10005;</button>
            <button class="action-btn play-media-btn"
                    onclick="window.customPlaylists.playItem('${media.id}')"
                    ${inRadio ? "disabled" : ""}>&#9654;&#65039;</button>
          </div>
        `;
      })
      .join("");
  }

  // Hands off to the main playlist manager — a custom playlist is just a
  // named subset of the same media, it doesn't get its own audio pipeline.
  playItem(mediaId) {
    if (window.player?.isInRadioMode()) {
      alert("Cannot change medias while in radio mode");
      return;
    }
    window.playlistManager?.playMedia(mediaId);
  }

  async removeItem(mediaId) {
    if (!this.activeId) return;
    try {
      const resp = await fetch(
        `/stargzr/player/playlists/${this.activeId}/items/${encodeURIComponent(mediaId)}`,
        { method: "DELETE" },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      this.activeDetail = await resp.json();
      this.renderDetail();
      this._syncCountInSelect(this.activeId, this.activeDetail.items.length);
    } catch (err) {
      alert("Could not remove that track");
    }
  }

  async addItem(mediaId, playlistId) {
    const id = playlistId ?? this.activeId;
    if (!id) return;
    try {
      const resp = await fetch(`/stargzr/player/playlists/${id}/items`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ media_id: mediaId }),
      });
      const data = await resp.json().catch(() => ({}));
      if (!resp.ok) {
        alert(data.error || "Could not add to playlist");
        return;
      }
      if (id === this.activeId) {
        this.activeDetail = data;
        this.renderDetail();
      }
      this._syncCountInSelect(id, data.items.length);
      debugLog(`Added to playlist "${data.name}"`);
    } catch (err) {
      alert("Could not reach the server");
    }
  }

  _syncCountInSelect(id, count) {
    const p = this.playlists.find((p) => p.id === id);
    if (p) p.item_count = count;
    this.renderSelect();
  }

  async renamePlaylist() {
    if (!this.activeDetail) return;
    const name = prompt("Rename playlist to:", this.activeDetail.name);
    if (!name || !name.trim()) return;
    try {
      const resp = await fetch(`/stargzr/player/playlists/${this.activeId}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: name.trim() }),
      });
      if (!resp.ok) {
        const data = await resp.json().catch(() => ({}));
        alert(data.error || "Could not rename playlist");
        return;
      }
      await this.loadPlaylists();
    } catch (err) {
      alert("Could not reach the server");
    }
  }

  async deleteActivePlaylist() {
    if (!this.activeId || !this.activeDetail) return;
    if (!confirm(`Delete playlist "${this.activeDetail.name}"? This can't be undone.`))
      return;
    try {
      const resp = await fetch(`/stargzr/player/playlists/${this.activeId}`, {
        method: "DELETE",
      });
      if (!resp.ok && resp.status !== 204) {
        alert("Could not delete playlist");
        return;
      }
      this.activeId = null;
      this.activeDetail = null;
      await this.loadPlaylists();
    } catch (err) {
      alert("Could not reach the server");
    }
  }

  // Small anchored menu letting the user pick which playlist to drop a track
  // into (or create a new one on the spot), opened from the ⭐ on a row in
  // the main playlist.
  openAddMenu(mediaId, anchorEl) {
    if (!this.isSignedIn()) {
      alert("Log in to save tracks to a playlist");
      return;
    }
    this.closeAddMenu();

    const menu = document.createElement("div");
    menu.className = "add-to-playlist-menu";
    menu.id = "add-to-playlist-menu";

    const options = this.playlists
      .map(
        (p) =>
          `<button class="add-to-playlist-option" data-id="${p.id}">${escapeHtml(p.name)}</button>`,
      )
      .join("");
    menu.innerHTML = `
      ${options || '<div class="add-to-playlist-empty">No playlists yet</div>'}
      <button class="add-to-playlist-option add-to-playlist-new" data-new="1">＋ New playlist…</button>
    `;

    menu.querySelectorAll(".add-to-playlist-option").forEach((btn) => {
      btn.addEventListener("click", async (e) => {
        e.stopPropagation();
        this.closeAddMenu();
        if (btn.dataset.new) {
          const name = prompt("Playlist name?");
          if (!name || !name.trim()) return;
          const resp = await fetch("/stargzr/player/playlists", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name: name.trim() }),
          });
          const data = await resp.json().catch(() => ({}));
          if (!resp.ok) {
            alert(data.error || "Could not create playlist");
            return;
          }
          await this.loadPlaylists();
          await this.addItem(mediaId, data.id);
        } else {
          await this.addItem(mediaId, parseInt(btn.dataset.id, 10));
        }
      });
    });

    document.body.appendChild(menu);
    const rect = anchorEl.getBoundingClientRect();
    menu.style.position = "fixed";
    menu.style.top = `${rect.bottom + 4}px`;
    menu.style.left = `${Math.min(rect.left, window.innerWidth - 220)}px`;

    // Close on the next outside click — deferred a tick so this same click
    // doesn't immediately close the menu it just opened.
    setTimeout(() => {
      this._outsideCloseHandler = () => this.closeAddMenu();
      document.addEventListener("click", this._outsideCloseHandler, { once: true });
    }, 0);
  }

  closeAddMenu() {
    document.getElementById("add-to-playlist-menu")?.remove();
    if (this._outsideCloseHandler) {
      document.removeEventListener("click", this._outsideCloseHandler);
      this._outsideCloseHandler = null;
    }
  }
}

function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str ?? "";
  return div.innerHTML;
}

window.customPlaylists = new CustomPlaylistManager();

document
  .getElementById("custom-playlist-new-btn")
  ?.addEventListener("click", () => window.customPlaylists.createPlaylist());

document.getElementById("custom-playlist-select")?.addEventListener("change", (e) => {
  const id = e.target.value;
  if (id) {
    window.customPlaylists.openPlaylist(parseInt(id, 10));
  } else {
    window.customPlaylists.activeId = null;
    window.customPlaylists.activeDetail = null;
    window.customPlaylists.renderDetail();
  }
});

document
  .getElementById("custom-playlist-rename-btn")
  ?.addEventListener("click", () => window.customPlaylists.renamePlaylist());

document
  .getElementById("custom-playlist-delete-btn")
  ?.addEventListener("click", () => window.customPlaylists.deleteActivePlaylist());

// auth.js fires this on every login/logout so this section stays in step
// without a page reload.
document.addEventListener("authChange", () => window.customPlaylists.refreshVisibility());

// Initial load — covers the case where the page was rendered already signed in.
window.customPlaylists.refreshVisibility();
