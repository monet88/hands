document.addEventListener("DOMContentLoaded", async () => {
  const profileIdSpan = document.getElementById("profileIdSpan");
  const statusSpan = document.getElementById("statusSpan");
  const pairFormCard = document.getElementById("pairFormCard");
  const pairedInfoCard = document.getElementById("pairedInfoCard");

  const pairingIdSpan = document.getElementById("pairingIdSpan");
  const policyRevSpan = document.getElementById("policyRevSpan");
  const targetsListSpan = document.getElementById("targetsListSpan");

  const bootstrapTokenInput = document.getElementById("bootstrapTokenInput");
  const btnSubmitPair = document.getElementById("btnSubmitPair");
  const pairMsg = document.getElementById("pairMsg");

  const btnRevoke = document.getElementById("btnRevoke");
  const revokeMsg = document.getElementById("revokeMsg");

  async function refreshUI() {
    pairMsg.textContent = "";
    revokeMsg.textContent = "";

    try {
      const state = await chrome.runtime.sendMessage({ action: "getState" });
      profileIdSpan.textContent = state?.profileId || "Unknown";

      if (state && state.isPaired) {
        const status = await chrome.runtime.sendMessage({ action: "status" });
        if (status && status.status === "ok" && status.pairingStatus === "active") {
          statusSpan.textContent = "Paired & Active (Local Native Host Connected)";
          statusSpan.style.color = "#28a745";

          pairingIdSpan.textContent = state.pairingId || "-";
          policyRevSpan.textContent = state.policyRevision || "-";
          targetsListSpan.textContent = state.targets?.map(t => `${t.name} (${t.canonical_path})`).join(", ") || "None";

          pairFormCard.style.display = "none";
          pairedInfoCard.style.display = "block";
          return;
        }
      }

      statusSpan.textContent = "Not Paired";
      statusSpan.style.color = "#856404";
      pairFormCard.style.display = "block";
      pairedInfoCard.style.display = "none";
    } catch (err) {
      statusSpan.textContent = "Error communicating with extension: " + err.message;
      statusSpan.style.color = "#dc3545";
    }
  }

  btnSubmitPair?.addEventListener("click", async () => {
    const token = bootstrapTokenInput.value.trim();
    if (!token) {
      pairMsg.className = "msg msg-error";
      pairMsg.textContent = "Please enter a valid Bootstrap Token.";
      return;
    }

    btnSubmitPair.disabled = true;
    pairMsg.className = "msg";
    pairMsg.textContent = "Connecting to native host...";

    try {
      const resp = await chrome.runtime.sendMessage({
        action: "setup",
        bootstrapToken: token
      });

      if (resp && resp.status === "ok") {
        pairMsg.className = "msg msg-success";
        pairMsg.textContent = "Pairing successful!";
        bootstrapTokenInput.value = "";
        setTimeout(refreshUI, 600);
      } else {
        pairMsg.className = "msg msg-error";
        pairMsg.textContent = "Pairing failed: " + (resp?.message || resp?.code || "Unknown error");
      }
    } catch (err) {
      pairMsg.className = "msg msg-error";
      pairMsg.textContent = "Native Host error: " + err.message;
    } finally {
      btnSubmitPair.disabled = false;
    }
  });

  btnRevoke?.addEventListener("click", async () => {
    if (!confirm("Are you sure you want to revoke this Return Bridge pairing?")) {
      return;
    }

    btnRevoke.disabled = true;
    revokeMsg.className = "msg";
    revokeMsg.textContent = "Revoking pairing...";

    try {
      const resp = await chrome.runtime.sendMessage({ action: "revoke" });
      if (resp && resp.status === "ok") {
        revokeMsg.className = "msg msg-success";
        revokeMsg.textContent = "Pairing revoked.";
        setTimeout(refreshUI, 600);
      } else {
        revokeMsg.className = "msg msg-error";
        revokeMsg.textContent = "Revoke failed: " + (resp?.message || resp?.code || "Unknown error");
      }
    } catch (err) {
      revokeMsg.className = "msg msg-error";
      revokeMsg.textContent = "Error: " + err.message;
    } finally {
      btnRevoke.disabled = false;
    }
  });

  await refreshUI();
});
