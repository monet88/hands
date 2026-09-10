document.addEventListener("DOMContentLoaded", async () => {
  const badge = document.getElementById("statusBadge");
  const pairedContent = document.getElementById("pairedContent");
  const unpairedContent = document.getElementById("unpairedContent");

  const profileIdVal = document.getElementById("profileIdVal");
  const pairingIdVal = document.getElementById("pairingIdVal");
  const policyRevVal = document.getElementById("policyRevVal");
  const targetsVal = document.getElementById("targetsVal");
  const unpairedProfileId = document.getElementById("unpairedProfileId");

  const btnOptions = document.getElementById("btnOptions");
  const btnPair = document.getElementById("btnPair");

  btnOptions?.addEventListener("click", () => {
    chrome.runtime.openOptionsPage();
  });

  btnPair?.addEventListener("click", () => {
    chrome.runtime.openOptionsPage();
  });

  let state = null;
  try {
    state = await chrome.runtime.sendMessage({ action: "getState" });
    if (state && state.isPaired) {
      // Check live connection with native host
      const status = await chrome.runtime.sendMessage({ action: "status" });
      if (status && status.status === "ok" && status.pairingStatus === "active") {
        badge.className = "badge badge-paired";
        badge.textContent = "Connected & Paired";
        pairedContent.style.display = "block";
        unpairedContent.style.display = "none";

        profileIdVal.textContent = state.profileId || "-";
        pairingIdVal.textContent = state.pairingId || "-";
        policyRevVal.textContent = state.policyRevision || "-";
        targetsVal.textContent = `${state.targets?.length || 0} target(s)`;
        return;
      }
      if (!status || status.code !== "pairing_retired") {
        badge.className = "badge badge-unpaired";
        badge.textContent = "Host Unavailable";
        pairedContent.style.display = "block";
        unpairedContent.style.display = "none";
        profileIdVal.textContent = state.profileId || "-";
        pairingIdVal.textContent = state.pairingId || "-";
        policyRevVal.textContent = state.policyRevision || "-";
        targetsVal.textContent = `${state.targets?.length || 0} target(s)`;
        return;
      }
    }

    // Unpaired state
    badge.className = "badge badge-unpaired";
    badge.textContent = "Unpaired";
    pairedContent.style.display = "none";
    unpairedContent.style.display = "block";
    unpairedProfileId.textContent = state?.profileId || "-";
  } catch (err) {
    badge.className = "badge badge-unpaired";
    badge.textContent = "Host Unavailable";
    pairedContent.style.display = state?.isPaired ? "block" : "none";
    unpairedContent.style.display = state?.isPaired ? "none" : "block";
  }
});
