document.addEventListener("DOMContentLoaded", async () => {
  const statusBadge = document.getElementById("statusBadge");
  const offlineContent = document.getElementById("offlineContent");
  const connectedContent = document.getElementById("connectedContent");
  const workspaceList = document.getElementById("workspaceList");
  const convTargetSection = document.getElementById("convTargetSection");
  const convTargetSelect = document.getElementById("convTargetSelect");
  const pendingReceiptsVal = document.getElementById("pendingReceiptsVal");
  const btnOptions = document.getElementById("btnOptions");
  const cmdAdd = document.getElementById("cmdAdd");
  const cmdRemove = document.getElementById("cmdRemove");
  const cmdCopyNotice = document.getElementById("cmdCopyNotice");

  btnOptions?.addEventListener("click", () => chrome.runtime.openOptionsPage());

  function setupCommandCopy(el, text) {
    if (!el) return;
    const handleCopy = async () => {
      try {
        await navigator.clipboard.writeText(text);
        if (cmdCopyNotice) {
          cmdCopyNotice.textContent = "Copied to clipboard!";
          setTimeout(() => { if (cmdCopyNotice) cmdCopyNotice.textContent = ""; }, 2000);
        }
      } catch (err) {
        if (cmdCopyNotice) {
          cmdCopyNotice.textContent = "Copy failed: " + (err?.message || "clipboard denied");
          setTimeout(() => { if (cmdCopyNotice) cmdCopyNotice.textContent = ""; }, 3000);
        }
      }
    };
    el.addEventListener("click", handleCopy);
  }
  setupCommandCopy(cmdAdd, 'hands-return-bridge target add --target "<path>"');
  setupCommandCopy(cmdRemove, 'hands-return-bridge target remove --target-id <id>');
  function renderWorkspaces(targets) {
    workspaceList.innerHTML = "";
    if (!targets.length) {
      const empty = document.createElement("div");
      empty.className = "muted";
      empty.textContent = "No workspaces. Add one in Manage Workspaces.";
      workspaceList.appendChild(empty);
      return;
    }
    for (const target of targets) {
      const item = document.createElement("div");
      item.className = "workspace";
      const name = document.createElement("div");
      name.className = "workspace-name";
      name.textContent = target.name || target.target_id;
      const path = document.createElement("div");
      path.className = "workspace-path";
      path.textContent = target.canonical_path || "";
      item.append(name, path);
      workspaceList.appendChild(item);
    }
  }

  function conversationIdFromUrl(url) {
    if (!url || !url.startsWith("https://chatgpt.com/")) return null;
    if (url.includes("#") || url.includes("?")) return null;
    try {
      const segments = new URL(url).pathname.split("/").filter(Boolean);
      let id = null;
      if (segments.length === 2 && segments[0] === "c") id = segments[1];
      if (segments.length === 4 && segments[0] === "g" && segments[2] === "c") id = segments[3];
      if (!id || id === "new" || id === "chat" || id.includes("new_chat") || id.includes("provisional")) {
        return null;
      }
      return id;
    } catch {}
    return null;
  }

  async function setupConversationTargetSelector(targets) {
    const tabs = await chrome.tabs.query({ active: true, currentWindow: true });
    const convId = conversationIdFromUrl(tabs?.[0]?.url);
    if (!convId) {
      convTargetSection.style.display = "none";
      return;
    }

    convTargetSection.style.display = "block";
    convTargetSelect.innerHTML = "";
    const placeholder = document.createElement("option");
    placeholder.value = "";
    placeholder.textContent = targets.length === 1 ? `Default (${targets[0].name})` : "Select workspace...";
    convTargetSelect.appendChild(placeholder);
    for (const target of targets) {
      const option = document.createElement("option");
      option.value = target.target_id;
      option.textContent = target.name || target.target_id;
      convTargetSelect.appendChild(option);
    }
    convTargetSelect.disabled = true;
    const binding = await chrome.runtime.sendMessage({
      action: "getConversationTarget",
      conversationId: convId
    });
    if (binding?.targetId) convTargetSelect.value = binding.targetId;
    convTargetSelect.disabled = false;
    convTargetSelect.onchange = async () => {
      await chrome.runtime.sendMessage({
        action: "setConversationTarget",
        conversationId: convId,
        targetId: convTargetSelect.value || null
      });
    };
  }

  async function updatePendingReceipts() {
    try {
      const response = await chrome.runtime.sendMessage({ action: "getPendingReceipts" });
      pendingReceiptsVal.textContent = String(response?.pendingReceipts?.length || 0);
    } catch {
      pendingReceiptsVal.textContent = "-";
    }
  }

  try {
    const response = await chrome.runtime.sendMessage({ action: "ensureLocalMode" });
    if (!response || response.status !== "ok") {
      throw new Error(response?.message || response?.code || "Native host unavailable");
    }

    const targets = Array.isArray(response.targets) ? response.targets : [];
    statusBadge.className = "badge connected";
    statusBadge.textContent = "Connected";
    offlineContent.style.display = "none";
    connectedContent.style.display = "block";
    renderWorkspaces(targets);
    await setupConversationTargetSelector(targets);
    await updatePendingReceipts();
  } catch {
    statusBadge.className = "badge offline";
    statusBadge.textContent = "Host unavailable";
    connectedContent.style.display = "none";
    offlineContent.style.display = "block";
  }
});
