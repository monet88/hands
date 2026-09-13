document.addEventListener("DOMContentLoaded", async () => {
  const statusSpan = document.getElementById("statusSpan");
  const installHint = document.getElementById("installHint");
  const unsupportedPlatformHint = document.getElementById("unsupportedPlatformHint");
  const workspaceMsg = document.getElementById("workspaceMsg");
  const workspaceList = document.getElementById("workspaceList");

  async function getPlatformOs() {
    try {
      if (typeof chrome !== "undefined" && chrome.runtime?.getPlatformInfo) {
        const info = await new Promise((resolve) => {
          try {
            chrome.runtime.getPlatformInfo((res) => resolve(res));
          } catch {
            resolve(null);
          }
        });
        if (info && info.os) return info.os;
      }
    } catch {}
    if (typeof navigator !== "undefined") {
      if (/windows|win32|win64/i.test(navigator.userAgent || "") || /win/i.test(navigator.platform || "")) {
        return "win";
      }
    }
    return "other";
  }

  async function showInstallHint() {
    const os = await getPlatformOs();
    if (os === "win") {
      if (installHint) installHint.style.display = "block";
      if (unsupportedPlatformHint) unsupportedPlatformHint.style.display = "none";
    } else {
      if (installHint) installHint.style.display = "none";
      if (unsupportedPlatformHint) unsupportedPlatformHint.style.display = "block";
    }
  }

  function hideInstallHints() {
    if (installHint) installHint.style.display = "none";
    if (unsupportedPlatformHint) unsupportedPlatformHint.style.display = "none";
  }

  function setMessage(text, isError = false) {
    workspaceMsg.textContent = text || "";
    workspaceMsg.className = isError ? "error" : "ok";
  }

  function renderWorkspaces(targets) {
    workspaceList.innerHTML = "";
    if (!Array.isArray(targets) || targets.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty";
      empty.textContent = "No workspaces yet.";
      workspaceList.appendChild(empty);
      return;
    }

    for (const target of targets) {
      const row = document.createElement("div");
      row.className = "workspace";

      const main = document.createElement("div");
      main.className = "workspace-main";
      const name = document.createElement("div");
      name.className = "workspace-name";
      name.textContent = target.name || target.target_id;
      const idEl = document.createElement("div");
      idEl.style.fontSize = "11px";
      idEl.style.color = "#666";
      idEl.textContent = "ID: " + target.target_id;
      const path = document.createElement("div");
      path.className = "workspace-path";
      path.textContent = target.canonical_path || "";
      main.append(name, idEl, path);

      row.append(main);
      workspaceList.appendChild(row);
    }
  }

  async function refresh() {
    try {
      const response = await chrome.runtime.sendMessage({ action: "ensureLocalMode" });
      if (!response || response.status !== "ok") {
        statusSpan.textContent = "Not installed / unavailable";
        statusSpan.className = "status error";
        await showInstallHint();
        renderWorkspaces([]);
        return;
      }
      statusSpan.textContent = "Connected";
      statusSpan.className = "status ok";
      hideInstallHints();
      renderWorkspaces(response.targets || []);
    } catch (err) {
      statusSpan.textContent = "Not installed / unavailable";
      statusSpan.className = "status error";
      await showInstallHint();
      setMessage(err.message || String(err), true);
      renderWorkspaces([]);
    }
  }
  await refresh();
});
