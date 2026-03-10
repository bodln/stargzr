document.getElementById("upload-btn").addEventListener("click", async () => {
  const input       = document.getElementById("upload-input");
  const status      = document.getElementById("upload-status");
  const progressDiv = document.getElementById("upload-progress");
  const bar         = document.getElementById("upload-bar");

  if (!input.files.length) {
    status.textContent = "No files selected.";
    status.style.color = "#495057";
    return;
  }

  const form = new FormData();
  for (const file of input.files) form.append("file", file);

  status.textContent        = "Uploading...";
  status.style.color        = "#495057";
  progressDiv.style.display = "block";
  bar.style.width           = "0%";

  await new Promise((resolve) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "/stargzr/player/upload");

    xhr.upload.addEventListener("progress", (e) => {
      if (e.lengthComputable) bar.style.width = (e.loaded / e.total) * 100 + "%";
    });

    xhr.addEventListener("load", async () => {
      if (xhr.status === 200) {
        status.textContent = "Upload successful, refreshing playlist...";
        status.style.color = "#28a745";
        bar.style.width    = "100%";
        input.value        = "";
        await window.playlistManager.loadPlaylist();
        status.textContent = "Upload complete.";
      } else {
        status.textContent = xhr.responseText || "Upload failed.";
        status.style.color = "#dc3545";
      }
      setTimeout(() => { progressDiv.style.display = "none"; bar.style.width = "0%"; }, 2000);
      resolve();
    });

    xhr.addEventListener("error", () => {
      status.textContent = "Network error during upload.";
      status.style.color = "#dc3545";
      resolve();
    });

    xhr.send(form);
  });
});
