// Accounts, browser side. The login token lives in an HttpOnly cookie the
// server sets and clears, so there is nothing to stash here. The page is
// rendered already knowing who is signed in, this file just reads that, wires
// the form, and keeps the panel in step after a login or logout.
//
// Exposes window.auth for the chat and broadcasts list, and fires "authChange"
// on the document whenever the signed in state moves.

(function () {
  const $ = (id) => document.getElementById(id);

  // Server stamps the current account name here, empty when nobody is signed in.
  const startingName =
    document.querySelector(".auth-container")?.dataset.username || "";

  window.auth = {
    username: startingName || null,
    isLoggedIn() {
      return !!this.username;
    },
  };

  function announce() {
    document.dispatchEvent(new CustomEvent("authChange"));
  }

  function showError(msg) {
    const el = $("auth-error");
    if (el) el.textContent = msg || "";
  }

  function paint() {
    const on = window.auth.isLoggedIn();
    $("auth-logged-in")?.classList.toggle("hidden", !on);
    $("auth-logged-out")?.classList.toggle("hidden", on);
    const nameEl = $("auth-current-user");
    if (nameEl) nameEl.textContent = window.auth.username || "";
    if (on) showError("");
  }

  let busy = false;

  // One path for both buttons, they hit the same shaped endpoint and get back
  // the same { username }. The token comes as a Set-Cookie we never touch.
  async function submit(path) {
    if (busy) return;
    showError("");

    const username = ($("auth-username")?.value || "").trim();
    const password = $("auth-password")?.value || "";
    if (!username || !password) {
      showError("Enter a username and password");
      return;
    }

    busy = true;
    toggleButtons(true);
    let resp;
    try {
      resp = await fetch(`/stargzr/auth/${path}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username, password }),
      });
    } catch (_) {
      busy = false;
      toggleButtons(false);
      showError("Could not reach the server");
      return;
    }
    busy = false;
    toggleButtons(false);

    const data = await resp.json().catch(() => ({}));
    if (!resp.ok) {
      showError(data.error || "Something went wrong");
      return;
    }

    const pw = $("auth-password");
    if (pw) pw.value = "";
    window.auth.username = data.username;
    paint();
    announce();
    debugLog(`Signed in as ${data.username}`);
  }

  async function logout() {
    if (busy) return;
    busy = true;
    try {
      await fetch("/stargzr/auth/logout", { method: "POST" });
    } catch (_) {}
    busy = false;
    window.auth.username = null;
    paint();
    announce();
    debugLog("Signed out");
  }

  function toggleButtons(disabled) {
    for (const id of ["auth-login-btn", "auth-register-btn", "auth-logout-btn"]) {
      const b = $(id);
      if (b) b.disabled = disabled;
    }
  }

  // Submitting the form (Enter in a field, or the Log in button) logs in. The
  // real <form> is here so password managers offer to save and fill.
  $("auth-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    submit("login");
  });
  $("auth-register-btn")?.addEventListener("click", () => submit("register"));
  $("auth-logout-btn")?.addEventListener("click", logout);

  paint();
})();
