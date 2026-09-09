const NATIVE_HOST = "com.hands.return_bridge";
const TRUST_NOTICE = "Notice: A paired extension may submit coding-agent tasks. Target, argv, and policy validation does not sandbox model-directed tool execution or contain a compromised paired extension.";

let storageAccessLevelEstablished = false;

async function ensureStorageAccessLevel() {
  if (storageAccessLevelEstablished) {
    return true;
  }
  if (chrome.storage?.local?.setAccessLevel) {
    try {
      await chrome.storage.local.setAccessLevel({ accessLevel: "TRUSTED_CONTEXTS" });
      storageAccessLevelEstablished = true;
      return true;
    } catch (err) {
      console.error("Failed to set chrome.storage.local accessLevel to TRUSTED_CONTEXTS:", err);
      storageAccessLevelEstablished = false;
      return false;
    }
  }
  return false;
}

// Initial attempt at background worker start
ensureStorageAccessLevel();

chrome.runtime.onInstalled?.addListener(() => {
  ensureStorageAccessLevel();
});

function isTrustedExtensionSender(sender) {
  if (!sender || sender.id !== chrome.runtime.id) {
    return false;
  }
  // Confused-deputy guard: content scripts share chrome.runtime.id but execute within web pages.
  // Privileged actions must only accept extension documents (options, popup, test_runner)
  // which have an exact chrome-extension://${chrome.runtime.id}/ URL prefix.
  const extensionOriginPrefix = `chrome-extension://${chrome.runtime.id}/`;
  if (typeof sender.url !== "string" || !sender.url.startsWith(extensionOriginPrefix)) {
    return false;
  }
  return true;
}

async function sha256Hex(str) {
  const encoder = new TextEncoder();
  const data = encoder.encode(str);
  const hashBuffer = await crypto.subtle.digest("SHA-256", data);
  const hashArray = Array.from(new Uint8Array(hashBuffer));
  return hashArray.map(b => b.toString(16).padStart(2, "0")).join("");
}

function parseCanonicalConversationId(urlStr) {
  if (!urlStr || typeof urlStr !== "string" || !urlStr.startsWith("https://chatgpt.com/")) return null;
  if (urlStr.includes("#") || urlStr.includes("?")) return null;
  const path = urlStr.slice("https://chatgpt.com/".length);
  const segments = path.split("/").filter(Boolean);
  let id = null;
  if (segments.length === 2 && segments[0] === "c") {
    id = segments[1];
  } else if (segments.length === 4 && segments[0] === "g" && segments[2] === "c" && segments[1]) {
    id = segments[3];
  }
  if (!id || id === "new" || id === "chat" || id.includes("new_chat") || id.includes("provisional")) {
    return null;
  }
  return id;
}

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
  // Reject messages from web pages or content scripts that are not extension documents
  if (!isTrustedExtensionSender(sender)) {
    sendResponse({ status: "error", code: "unauthorized_sender", message: "Rejected non-extension document message" });
    return false;
  }

  (async () => {
    try {
      const profileId = await getOrCreateProfileId();

      // Gate: Secret-bearing actions must fail-closed if storage isolation is not established
      if (["setup", "status", "connect", "revoke", "launch", "recover"].includes(request.action)) {
        const isTrustedStorage = await ensureStorageAccessLevel();
        if (!isTrustedStorage) {
          sendResponse({
            status: "error",
            code: "storage_isolation_unavailable",
            message: "Storage isolation (TRUSTED_CONTEXTS) could not be established. Secret-bearing storage is disabled fail-closed."
          });
          return;
        }
      }

      switch (request.action) {
        case "getState": {
          const isTrustedStorage = await ensureStorageAccessLevel();
          if (!isTrustedStorage) {
            sendResponse({
              status: "ok",
              profileId,
              extensionId: chrome.runtime.id,
              isPaired: false,
              pairingId: null,
              targets: [],
              policyRevision: null,
              trustNotice: TRUST_NOTICE,
              storageAccessLevel: "UNTRUSTED"
            });
            break;
          }

          const stored = await chrome.storage.local.get([
            "pairingId",
            "targets",
            "policyRevision",
            "isPaired"
          ]);
          sendResponse({
            status: "ok",
            profileId,
            extensionId: chrome.runtime.id,
            isPaired: !!stored.isPaired,
            pairingId: stored.pairingId || null,
            targets: stored.targets || [],
            policyRevision: stored.policyRevision || null,
            trustNotice: TRUST_NOTICE,
            storageAccessLevel: "TRUSTED_CONTEXTS"
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

        case "launch": {
          const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired", "policyRevision", "targets"]);
          if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
            sendResponse({ status: "error", code: "not_paired", message: "Extension is not paired" });
            return;
          }

          const transcriptEvidenceHash = request.transcriptEvidenceHash?.trim();
          const accountEvidenceHash = request.accountEvidenceHash?.trim();
          if (!transcriptEvidenceHash || transcriptEvidenceHash === "hash_transcript_empty") {
            sendResponse({ status: "error", code: "missing_evidence", message: "Real transcript evidence hash is required; empty placeholder rejected" });
            return;
          }
          if (!accountEvidenceHash || accountEvidenceHash === "hash_account_empty") {
            sendResponse({ status: "error", code: "missing_evidence", message: "Real account evidence hash is required; empty placeholder rejected" });
            return;
          }

          const originConversationId = request.originConversationId?.trim();
          const originConversationUrl = request.originConversationUrl?.trim();
          if (!originConversationId || !originConversationUrl) {
            sendResponse({ status: "error", code: "missing_conversation_binding", message: "Origin conversation ID and URL are required" });
            return;
          }

          const parsedConvId = parseCanonicalConversationId(originConversationUrl);
          if (!parsedConvId || parsedConvId !== originConversationId) {
            sendResponse({
              status: "error",
              code: "invalid_conversation_boundary",
              message: "Origin conversation URL must be a canonical existing ChatGPT conversation matching the conversation ID"
            });
            return;
          }

          const targetId = request.targetId?.trim();
          if (!targetId) {
            sendResponse({ status: "error", code: "missing_target_id", message: "Target ID is required" });
            return;
          }

          const promptText = request.promptText;
          if (!promptText || typeof promptText !== "string" || !promptText.trim()) {
            sendResponse({ status: "error", code: "missing_prompt_text", message: "Prompt text is required" });
            return;
          }

          const requestedPolicyRevision = request.requestedPolicyRevision?.trim() || stored.policyRevision || "v1";

          // Durable request identity: reuse unresolved matching request; reject duplicate with changed payload locally
          const activeKey = `active_launch_${originConversationId}_${targetId}`;
          const activeStored = await chrome.storage.local.get([activeKey]);
          const existingActiveReqId = activeStored[activeKey];

          let launchRequestId;
          if (request.launchRequestId && request.launchRequestId.trim()) {
            launchRequestId = request.launchRequestId.trim();
          } else if (existingActiveReqId) {
            launchRequestId = existingActiveReqId;
          } else {
            const digest = await sha256Hex(`${stored.pairingId}:${originConversationId}:${targetId}:${promptText}`);
            launchRequestId = "req_" + digest.slice(0, 16);
          }

          const pendingLaunchKey = "launch_" + launchRequestId;
          const storedLaunch = await chrome.storage.local.get([pendingLaunchKey]);
          const existingRecord = storedLaunch[pendingLaunchKey];

          if (existingRecord) {
            if (
              existingRecord.originConversationId !== originConversationId ||
              existingRecord.originConversationUrl !== originConversationUrl ||
              existingRecord.transcriptEvidenceHash !== transcriptEvidenceHash ||
              existingRecord.accountEvidenceHash !== accountEvidenceHash ||
              existingRecord.targetId !== targetId ||
              existingRecord.promptText !== promptText
            ) {
              sendResponse({
                status: "error",
                code: "payload_conflict",
                message: "A launch request with this ID already exists with a different payload"
              });
              return;
            }
          }

          const launchRecord = {
            launchRequestId,
            pairingId: stored.pairingId,
            originConversationId,
            originConversationUrl,
            transcriptEvidenceHash,
            accountEvidenceHash,
            targetId,
            requestedPolicyRevision,
            promptText,
            createdAt: existingRecord ? existingRecord.createdAt : Date.now(),
            status: existingRecord ? existingRecord.status : "pending_native"
          };

          try {
            await chrome.storage.local.set({
              [pendingLaunchKey]: launchRecord,
              [activeKey]: launchRequestId,
              lastLaunchRequestId: launchRequestId
            });
          } catch (storageErr) {
            sendResponse({
              status: "error",
              code: "browser_persistence_failure",
              message: "Failed to persist launch request in browser storage; native request aborted"
            });
            return;
          }

          // Invoke native host
          const response = await sendNative({
            op: "launch",
            pairingId: stored.pairingId,
            pairingSecret: stored.pairingSecret,
            profileId,
            launchRequestId,
            originConversationId,
            originConversationUrl,
            transcriptEvidenceHash,
            accountEvidenceHash,
            targetId,
            requestedPolicyRevision,
            promptText
          });

          if (response && response.status === "ok") {
            launchRecord.status = response.state || "started";
            launchRecord.executionId = response.executionId;
            launchRecord.returnToken = response.returnToken;
            launchRecord.terminalEvidence = response.terminalEvidence;
            await chrome.storage.local.set({
              [pendingLaunchKey]: launchRecord,
              activeExecutionId: response.executionId
            });
          } else if (response && response.code === "launch_uncertain") {
            launchRecord.status = "unknown";
            launchRecord.executionId = response.executionId;
            launchRecord.terminalEvidence = response.terminalEvidence;
            await chrome.storage.local.set({
              [pendingLaunchKey]: launchRecord
            });
          }

          sendResponse(response);
          break;
        }

        case "recover": {
          const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
          if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
            sendResponse({ status: "error", code: "not_paired", message: "Extension is not paired" });
            return;
          }

          const payload = {
            op: "recover",
            pairingId: stored.pairingId,
            pairingSecret: stored.pairingSecret,
            profileId
          };
          if (request.launchRequestId) {
            payload.launchRequestId = request.launchRequestId;
          }

          const response = await sendNative(payload);
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
