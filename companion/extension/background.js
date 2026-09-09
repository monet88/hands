const NATIVE_HOST = "com.hands.return_bridge";
const TRUST_NOTICE = "Notice: A paired extension may submit coding-agent tasks. Target, argv, and policy validation does not sandbox model-directed tool execution or contain a compromised paired extension.";

async function getOrCreateProfileId() {
  const data = await chrome.storage.local.get(["profileId"]);
  if (data.profileId) {
    return data.profileId;
  }
  const newProfileId = "prof_" + crypto.randomUUID().replace(/-/g, "").slice(0, 16);
  await chrome.storage.local.set({ profileId: newProfileId });
  return newProfileId;
}

function sendNative(msg) {
  return new Promise((resolve, reject) => {
    chrome.runtime.sendNativeMessage(NATIVE_HOST, msg, (response) => {
      if (chrome.runtime.lastError) {
        return reject(new Error(chrome.runtime.lastError.message));
      }
      resolve(response);
    });
  });
}

// Extension internal message router
chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  // Reject messages from web pages or content scripts that are not extension-internal
  if (sender.id !== chrome.runtime.id) {
    sendResponse({ status: "error", code: "unauthorized_origin" });
    return false;
  }

  (async () => {
    try {
      const profileId = await getOrCreateProfileId();

      switch (request.action) {
        case "getState": {
          const stored = await chrome.storage.local.get([
            "pairingId",
            "targets",
            "policyRevision",
            "isPaired"
          ]);
          sendResponse({
            status: "ok",
            profileId,
            isPaired: !!stored.isPaired,
            pairingId: stored.pairingId || null,
            targets: stored.targets || [],
            policyRevision: stored.policyRevision || null,
            trustNotice: TRUST_NOTICE
          });
          break;
        }

        case "setup": {
          const token = request.bootstrapToken?.trim();
          if (!token) {
            sendResponse({ status: "error", code: "missing_token", message: "Bootstrap token is required" });
            return;
          }

          const response = await sendNative({
            op: "setup",
            bootstrapToken: token,
            profileId
          });

          if (response && response.status === "ok") {
            await chrome.storage.local.set({
              isPaired: true,
              pairingId: response.pairingId,
              pairingSecret: response.pairingSecret,
              targets: response.targets || [],
              policyRevision: response.policyRevision || "v1"
            });
            sendResponse({
              status: "ok",
              pairingId: response.pairingId,
              profileId,
              targets: response.targets,
              policyRevision: response.policyRevision,
              trustNotice: TRUST_NOTICE
            });
          } else {
            sendResponse(response || { status: "error", code: "native_failed" });
          }
          break;
        }

        case "status": {
          const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
          if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
            sendResponse({ status: "ok", isPaired: false, profileId });
            return;
          }

          const response = await sendNative({
            op: "status",
            pairingId: stored.pairingId,
            pairingSecret: stored.pairingSecret,
            profileId
          });

          if (response && response.status === "ok") {
            sendResponse({
              status: "ok",
              isPaired: true,
              profileId,
              pairingStatus: response.pairingStatus,
              targetsCount: response.targetsCount,
              policyRevision: response.policyRevision,
              taskExecutionAvailable: false,
              trustNotice: TRUST_NOTICE
            });
          } else {
            if (response && response.code === "pairing_retired") {
              await chrome.storage.local.remove(["isPaired", "pairingId", "pairingSecret", "targets", "policyRevision"]);
            }
            sendResponse(response || { status: "error", code: "status_failed" });
          }
          break;
        }

        case "connect": {
          const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
          if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
            sendResponse({ status: "error", code: "not_paired", message: "Extension is not paired" });
            return;
          }

          const response = await sendNative({
            op: "connect",
            pairingId: stored.pairingId,
            pairingSecret: stored.pairingSecret,
            profileId
          });

          sendResponse(response);
          break;
        }

        case "revoke": {
          const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
          if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
            sendResponse({ status: "ok", message: "Already unpaired" });
            return;
          }

          const response = await sendNative({
            op: "revoke",
            pairingId: stored.pairingId,
            pairingSecret: stored.pairingSecret,
            profileId
          });

          if (response && (response.status === "ok" || response.code === "pairing_retired")) {
            await chrome.storage.local.remove(["isPaired", "pairingId", "pairingSecret", "targets", "policyRevision"]);
          }
          sendResponse(response);
          break;
        }

        default:
          sendResponse({ status: "error", code: "unsupported_action" });
          break;
      }
    } catch (err) {
      sendResponse({ status: "error", code: "internal_error", message: err.message });
    }
  })();

  return true; // Keep channel open for async response
});
