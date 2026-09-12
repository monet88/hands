document.addEventListener("DOMContentLoaded", async () => {
  const statusSpan = document.getElementById("statusSpan");
  const installHint = document.getElementById("installHint");
  const workspaceMsg = document.getElementById("workspaceMsg");
  const workspaceList = document.getElementById("workspaceList");

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
      const path = document.createElement("div");
      path.className = "workspace-path";
      path.textContent = target.canonical_path || "";
      main.append(name, path);

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
        installHint.style.display = "block";
        renderWorkspaces([]);
        return;
      }
      statusSpan.textContent = "Connected";
      statusSpan.className = "status ok";
      installHint.style.display = "none";
      renderWorkspaces(response.targets || []);
    } catch (err) {
      statusSpan.textContent = "Not installed / unavailable";
      statusSpan.className = "status error";
      installHint.style.display = "block";
      setMessage(err.message || String(err), true);
      renderWorkspaces([]);
    }
  }
  await refresh();
});
