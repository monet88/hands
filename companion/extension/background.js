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

          // Tab binding (Finding 1 & 6): explicit tabId selector is REQUIRED (no focused-tab fallback)
          const tabId = request.tabId;
          if (typeof tabId !== "number") {
            sendResponse({
              status: "error",
              code: "missing_tab_binding",
              message: "Explicit ChatGPT tab binding (tabId) is required; focused-tab fallback is disabled"
            });
            return;
          }

          let targetTab;
          try {
            targetTab = await chrome.tabs.get(tabId);
          } catch (tabErr) {
            sendResponse({
              status: "error",
              code: "tab_not_found",
              message: "Bound ChatGPT tab not found: " + tabErr.message
            });
            return;
          }

          const tabUrl = targetTab?.url || targetTab?.pendingUrl;
          if (!tabUrl || typeof tabUrl !== "string" || !tabUrl.startsWith("https://chatgpt.com/")) {
            sendResponse({
              status: "error",
              code: "invalid_tab_url",
              message: "Bound tab URL must be an exact ChatGPT page (https://chatgpt.com/*); was: " + tabUrl
            });
            return;
          }

          const originConversationId = parseCanonicalConversationId(tabUrl);
          if (!originConversationId) {
            sendResponse({
              status: "error",
              code: "invalid_conversation_boundary",
              message: "Bound tab is not on a canonical existing ChatGPT conversation (e.g. https://chatgpt.com/c/<id>)"
            });
            return;
          }
          const originConversationUrl = tabUrl.split("#")[0].split("?")[0];

          // Obtain evidence directly from content script on bound top-level tab
          let evidenceRes;
          try {
            evidenceRes = await new Promise((resolve, reject) => {
              chrome.tabs.sendMessage(tabId, { action: "collect_page_evidence" }, (res) => {
                if (chrome.runtime.lastError) {
                  return reject(new Error(chrome.runtime.lastError.message));
                }
                resolve(res);
              });
            });
          } catch (contentErr) {
            sendResponse({
              status: "error",
              code: "evidence_collection_failed",
              message: "Failed to communicate with content script on bound tab: " + contentErr.message
            });
            return;
          }

          if (!evidenceRes || !evidenceRes.ok) {
            sendResponse({
              status: "error",
              code: evidenceRes?.error || "evidence_collection_failed",
              message: evidenceRes?.message || "Content script failed to collect page evidence"
            });
            return;
          }

          if (evidenceRes.originConversationId !== originConversationId || evidenceRes.originConversationUrl !== originConversationUrl) {
            sendResponse({
              status: "error",
              code: "conversation_binding_mismatch",
              message: "Content script conversation ID or URL does not match bound top-level tab"
            });
            return;
          }

          const transcriptText = evidenceRes.transcriptText?.trim();
          const accountText = evidenceRes.accountText?.trim();
          if (!transcriptText) {
            sendResponse({
              status: "error",
              code: "missing_rendered_transcript",
              message: "Rendered transcript text is empty or unavailable"
            });
            return;
          }
          if (!accountText) {
            sendResponse({
              status: "error",
              code: "missing_account_context",
              message: "Account/workspace context evidence is empty or unavailable"
            });
            return;
          }

          // Compute evidence hashes inside trusted background
          const transcriptEvidenceHash = await sha256Hex(evidenceRes.transcriptText);
          const accountEvidenceHash = await sha256Hex(evidenceRes.accountText);

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
          if (promptText.length > 128 * 1024) {
            sendResponse({ status: "error", code: "prompt_too_large", message: "Prompt exceeds bounded size limit (128 KB)" });
            return;
          }

          const requestedPolicyRevision = request.requestedPolicyRevision?.trim() || stored.policyRevision || "v1";

          // Durable request identity (Finding 5):
          // Installation-local request identity matches exact immutable payload.
          const activeKey = `active_launch_${stored.pairingId}_${originConversationId}_${targetId}_${requestedPolicyRevision}`;
          const activeStored = await chrome.storage.local.get([activeKey]);
          const existingActiveReqId = activeStored[activeKey];

          let launchRequestId;
          if (request.launchRequestId && request.launchRequestId.trim()) {
            launchRequestId = request.launchRequestId.trim();
          } else if (existingActiveReqId) {
            const storedLaunch = await chrome.storage.local.get(["launch_" + existingActiveReqId]);
            const existingRecord = storedLaunch["launch_" + existingActiveReqId];
            const isUnresolved = existingRecord && ["pending_native", "attempting", "unknown", "native-response-uncertain"].includes(existingRecord.status);
            if (isUnresolved) {
              const isSamePayload =
                existingRecord.originConversationId === originConversationId &&
                existingRecord.originConversationUrl === originConversationUrl &&
                existingRecord.transcriptEvidenceHash === transcriptEvidenceHash &&
                existingRecord.accountEvidenceHash === accountEvidenceHash &&
                existingRecord.targetId === targetId &&
                existingRecord.requestedPolicyRevision === requestedPolicyRevision &&
                existingRecord.promptText === promptText;

              if (isSamePayload) {
                launchRequestId = existingActiveReqId;
              } else {
                sendResponse({
                  status: "error",
                  code: "active_launch_unresolved",
                  message: `An unresolved launch request (${existingActiveReqId}) with state '${existingRecord.status}' is active for this conversation/target. Resolve or recover it before initiating a new task.`,
                  launchRequestId: existingActiveReqId,
                  executionId: existingRecord.executionId,
                  state: existingRecord.status
                });
                return;
              }
            } else {
              // Prior task was resolved (started, failed, revoked). Allocate fresh deterministic ID.
              const digest = await sha256Hex(
                `${stored.pairingId}:${originConversationId}:${originConversationUrl}:${transcriptEvidenceHash}:${accountEvidenceHash}:${targetId}:${requestedPolicyRevision}:${promptText}`
              );
              launchRequestId = "req_" + digest.slice(0, 16);
            }
          } else {
            const digest = await sha256Hex(
              `${stored.pairingId}:${originConversationId}:${originConversationUrl}:${transcriptEvidenceHash}:${accountEvidenceHash}:${targetId}:${requestedPolicyRevision}:${promptText}`
            );
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
              existingRecord.requestedPolicyRevision !== requestedPolicyRevision ||
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

          // Invoke native host with explicit lost-native-response boundary handling (Finding 4)
          let response;
          try {
            response = await sendNative({
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
          } catch (nativeErr) {
            launchRecord.status = "native-response-uncertain";
            launchRecord.lastError = nativeErr.message;
            await chrome.storage.local.set({
              [pendingLaunchKey]: launchRecord
            });
            sendResponse({
              status: "error",
              code: "native_response_uncertain",
              launchRequestId,
              state: "native-response-uncertain",
              message: "Native messaging host response was lost or rejected: " + nativeErr.message,
              trustNotice: TRUST_NOTICE
            });
            return;
          }

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
          if (request.launchRequestId && request.launchRequestId.trim()) {
            payload.launchRequestId = request.launchRequestId.trim();
          }

          const response = await sendNative(payload);
          if (response && response.status === "ok") {
            const list = Array.isArray(response.summaries)
              ? response.summaries
              : (response.summary ? [response.summary] : []);

            for (const summary of list) {
              if (summary.launch_request_id) {
                const recordKey = "launch_" + summary.launch_request_id;
                const cur = (await chrome.storage.local.get([recordKey]))[recordKey];
                if (cur && cur.status === "native-response-uncertain") {
                  cur.status = summary.state || "unknown";
                  cur.executionId = summary.execution_id;
                  cur.terminalEvidence = summary.orca_terminal_handle ? { orcaTerminalHandle: summary.orca_terminal_handle } : null;
                  await chrome.storage.local.set({ [recordKey]: cur });
                }
              }
            }
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
