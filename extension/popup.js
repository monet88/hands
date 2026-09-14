document.addEventListener("DOMContentLoaded", async () => {
  const statusBadge = document.getElementById("statusBadge");
  const offlineContent = document.getElementById("offlineContent");
  const connectedContent = document.getElementById("connectedContent");
  const pendingReceiptsVal = document.getElementById("pendingReceiptsVal");
  const lastDispatchDiagnosticVal = document.getElementById("lastDispatchDiagnosticVal");

  async function updatePendingReceipts() {
    try {
      const response = await chrome.runtime.sendMessage({ action: "getPendingReceipts" });
      pendingReceiptsVal.textContent = String(response?.pendingReceipts?.length || 0);
      const diagnostic = response?.lastDispatchDiagnostic;
      lastDispatchDiagnosticVal.textContent = diagnostic
        ? [diagnostic.status, diagnostic.reason, Number.isInteger(diagnostic.httpStatus) ? `HTTP ${diagnostic.httpStatus}` : null].filter(Boolean).join(" · ")
        : "-";
    } catch {
      pendingReceiptsVal.textContent = "-";
      lastDispatchDiagnosticVal.textContent = "-";
    }
  }

  try {
    const response = await chrome.runtime.sendMessage({ action: "ensureLocalMode" });
    if (!response || response.status !== "ok") {
      throw new Error(response?.message || response?.code || "Native host unavailable");
    }

    statusBadge.className = "badge connected";
    statusBadge.textContent = "Connected";
    offlineContent.style.display = "none";
    connectedContent.style.display = "block";
    await updatePendingReceipts();
  } catch {
    statusBadge.className = "badge offline";
    statusBadge.textContent = "Host unavailable";
    connectedContent.style.display = "none";
    offlineContent.style.display = "block";
  }
});
