const searchInput     = document.getElementById("playlist-search");
const searchResultsEl = document.getElementById("search-results");

searchInput.addEventListener("input", () => {
  const query = searchInput.value.trim().toLowerCase();
  if (!query) { searchResultsEl.classList.add("hidden"); return; }

  const matches = window.playlistManager.medias.filter((s) =>
    s.filename.toLowerCase().includes(query)
  );
  if (matches.length === 0) { searchResultsEl.classList.add("hidden"); return; }

  searchResultsEl.innerHTML = matches
    .map((s) => `<div class="search-result-item" data-media-id="${s.id}">${s.filename}</div>`)
    .join("");
  searchResultsEl.classList.remove("hidden");

  searchResultsEl.querySelectorAll(".search-result-item").forEach((item) => {
    item.addEventListener("click", () => {
      const mediaId = item.dataset.mediaId;
      searchResultsEl.classList.add("hidden");
      searchInput.value = "";

      const container = document.getElementById("playlist-display");
      const el        = container.querySelector(`[data-media-id="${mediaId}"]`);
      if (!el) return;

      const cRect           = container.getBoundingClientRect();
      const iRect           = el.getBoundingClientRect();
      const targetScrollTop = container.scrollTop + (iRect.top - cRect.top) - 16;
      container.scrollTo({ top: Math.max(0, targetScrollTop), behavior: "smooth" });

      el.classList.add("search-highlight");
      setTimeout(() => el.classList.remove("search-highlight"), 1500);

      debugLog(`Search scrolled to: ${item.textContent}`);
    });
  });
});

document.addEventListener("click", (e) => {
  if (!e.target.closest(".search-row")) searchResultsEl.classList.add("hidden");
});
