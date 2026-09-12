const NATIVE_HOST = "com.hands.return_bridge";
const TRUST_NOTICE = "Notice: A paired extension may submit coding-agent tasks. Target, argv, and policy validation does not sandbox model-directed tool execution or contain a compromised paired extension.";
const LOCAL_PAIRING_ID = "local";
const LOCAL_PAIRING_SECRET = "local";
const LOCAL_PROFILE_ID = "local";
const LOCAL_POLICY_REVISION = "v1";

let storageAccessLevelEstablished = false;
const inFlightLaunches = new Map();
const UNRESOLVED_LAUNCH_STATES = new Set([
  "claimed",
  "pending_native",
  "attempting",
  "unknown",
  "native-response-uncertain"
]);
const PROVEN_LAUNCH_STATES = new Set(["started", "completed"]);
const RECOVERY_ALARM_NAME = "hands_return_bridge_recovery_drain";
const RECOVERY_PERIOD_MINUTES = 1;

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
  ensureRecoveryAlarm();
  performScheduledDrain();
});

chrome.runtime.onStartup?.addListener(() => {
  ensureRecoveryAlarm();
  performScheduledDrain();
});

chrome.alarms?.onAlarm?.addListener(async (alarm) => {
  if (alarm && alarm.name === RECOVERY_ALARM_NAME) {
    await performScheduledDrain();
  }
});

async function ensureRecoveryAlarm() {
  try {
    if (!chrome.alarms) return;
    const existing = await chrome.alarms.get(RECOVERY_ALARM_NAME);
    if (!existing) {
      await chrome.alarms.create(RECOVERY_ALARM_NAME, {
        periodInMinutes: RECOVERY_PERIOD_MINUTES
      });
    }
  } catch (err) {
    console.warn("Failed to ensure recovery alarm:", err);
  }
}

async function performScheduledDrain() {
  try {
    await ensureRecoveryAlarm();
    const isTrustedStorage = await ensureStorageAccessLevel();
    if (!isTrustedStorage) return;

    const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
    if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) return;

    const profileId = await getOrCreateProfileId();
    const drainResponse = await sendNative({
      op: "drain",
      pairingId: stored.pairingId,
      pairingSecret: stored.pairingSecret,
      profileId
    });

    if (drainResponse && drainResponse.status === "ok") {
      await processDrainResponse(drainResponse, stored, profileId);
      // Attempt delivery of pending receipts
      const allData = await chrome.storage.local.get(null);
      for (const [k, v] of Object.entries(allData)) {
        if (k.startsWith("receipt_") && v && (v.deliveryStatus === "received" || v.deliveryStatus === "not-sent")) {
          await dispatchSingleReceipt(v, stored, profileId);
        }
      }
    }
  } catch (err) {
    console.warn("Scheduled drain encounter:", err);
  }
}

function buildReceiptRecord(rcpt, fallbackOriginConversationUrl) {
  return {
    receiptId: rcpt.receipt_id,
    executionId: rcpt.execution_id,
    pairingId: rcpt.pairing_id,
    returnToken: rcpt.return_token,
    originConversationId: rcpt.origin_conversation_id,
    originConversationUrl: rcpt.origin_conversation_url || fallbackOriginConversationUrl || undefined,
    turnIndex: rcpt.turn_index,
    stopReason: rcpt.stop_reason,
    assistantMessageId: rcpt.assistant_message_id,
    assistantText: rcpt.assistant_text,
    contentDigest: rcpt.content_digest,
    toolCallCount: rcpt.tool_call_count,
    state: rcpt.state || "completed",
    receivedAt: Date.now(),
    deliveryStatus: rcpt.delivery_status || "received",
    deliveryRevision: rcpt.delivery_revision || 0,
    activeAttemptId: rcpt.active_attempt_id || undefined
  };
}

async function processDrainResponse(drainResponse, stored, profileId) {
  // 1. Reconcile launch summaries and reconstruct missing receipts from native durable authority
  const summaries = Array.isArray(drainResponse.summaries) ? drainResponse.summaries : [];
  for (const summary of summaries) {
    if (summary.launch_request_id) {
      const recordKey = "launch_" + summary.launch_request_id;
      const cur = (await chrome.storage.local.get([recordKey]))[recordKey];
      if (cur && UNRESOLVED_LAUNCH_STATES.has(cur.status)) {
        cur.status = summary.state || "unknown";
        cur.executionId = summary.execution_id;
        cur.terminalEvidence = summary.orca_terminal_handle ? { orcaTerminalHandle: summary.orca_terminal_handle } : null;
        await chrome.storage.local.set({ [recordKey]: cur });
      }
    }

    // AC4 Recovery: Reconstruct browser receipt from native durable summary if local receipt was deleted/lost
    if (summary.completion_receipt && summary.completion_receipt.receipt_id && summary.completion_receipt.execution_id) {
      const rcpt = summary.completion_receipt;
      const receiptStorageKey = "receipt_" + rcpt.receipt_id;
      const executionReceiptKey = "rcpt_by_exec_" + rcpt.execution_id;
      const existing = (await chrome.storage.local.get([receiptStorageKey]))[receiptStorageKey];
      if (!existing) {
        // Local storage was lost or missing: reconstruct from native durable authority
        // Retain durable deliveryStatus, deliveryRevision, and originConversationUrl from native (Findings 1, 3)
        const reconstructedRecord = buildReceiptRecord(rcpt, summary.origin_conversation_url);
        try {
          await chrome.storage.local.set({
            [receiptStorageKey]: reconstructedRecord,
            [executionReceiptKey]: rcpt.receipt_id
          });
        } catch (storageErr) {
          console.error("Failed to reconstruct receipt " + rcpt.receipt_id + " from summary:", storageErr);
        }
      } else {
        // Reconcile existing record with native durable fence if native has more recent revision, state, or URL
        let updated = false;
        if (rcpt.delivery_revision && (existing.deliveryRevision || 0) < rcpt.delivery_revision) {
          existing.deliveryRevision = rcpt.delivery_revision;
          updated = true;
        }
        if (rcpt.delivery_status && rcpt.delivery_status !== existing.deliveryStatus) {
          existing.deliveryStatus = rcpt.delivery_status;
          updated = true;
        }
        if ((rcpt.origin_conversation_url || summary.origin_conversation_url) && !existing.originConversationUrl) {
          existing.originConversationUrl = rcpt.origin_conversation_url || summary.origin_conversation_url;
          updated = true;
        }
        if (updated) {
          await chrome.storage.local.set({ [receiptStorageKey]: existing });
        }
      }
    }
  }
  // 2. Process real Completion Receipts: Persist browser receipt handling BEFORE sending transport ACK
  const receipts = Array.isArray(drainResponse.receipts) ? drainResponse.receipts : [];
  const ackedReceiptIds = [];
  for (const rcpt of receipts) {
    if (!rcpt.receipt_id || !rcpt.execution_id) continue;
    const receiptStorageKey = "receipt_" + rcpt.receipt_id;
    const executionReceiptKey = "rcpt_by_exec_" + rcpt.execution_id;

    // Prepare durable browser record: status "received" (separate from ChatGPT submission, no send permission)
    const receiptRecord = buildReceiptRecord(rcpt);

    // Hard gate: Storage write MUST succeed BEFORE reporting acknowledgement to native host
    try {
      await chrome.storage.local.set({
        [receiptStorageKey]: receiptRecord,
        [executionReceiptKey]: rcpt.receipt_id
      });
    } catch (storageErr) {
      // Storage failure reports no successful durable acknowledgement
      console.error("Browser storage failure for receipt " + rcpt.receipt_id + "; ACK aborted:", storageErr);
      continue;
    }

    // Only after browser storage succeeds, send transport ACK to native host
    try {
      const ackResponse = await sendNative({
        op: "ack",
        pairingId: stored.pairingId,
        pairingSecret: stored.pairingSecret,
        profileId,
        receiptId: rcpt.receipt_id,
        executionId: rcpt.execution_id,
        ackStatus: "received"
      });

      if (ackResponse && ackResponse.status === "ok" && ackResponse.acknowledged) {
        ackedReceiptIds.push(rcpt.receipt_id);
      }
    } catch (ackErr) {
      console.warn("Transport ACK failed for receipt " + rcpt.receipt_id + ":", ackErr);
    }
  }
  return ackedReceiptIds;
}

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

function buildContinuationPayload(rcpt) {
  const receiptMarker = `[hands-return-bridge:receipt=${rcpt.receiptId}]`;
  const continuationText = `[Hands Return Bridge] Local agent execution completed (receipt: ${rcpt.receiptId}, execution: ${rcpt.executionId}). Please inspect local agent/repository truth and continue. ${receiptMarker}`;
  return { receiptMarker, continuationText };
}

async function findOrReopenConversationTab(conversationUrl, conversationId) {
  if (!chrome.tabs) return null;
  // 1. Check existing tabs without stealing focus
  const tabs = await chrome.tabs.query({ url: "https://chatgpt.com/*" });
  for (const tab of tabs) {
    if (tab.url) {
      const tabCleanUrl = tab.url.split("#")[0].split("?")[0];
      const tabConvId = parseCanonicalConversationId(tabCleanUrl);
      if (tabCleanUrl === conversationUrl || (tabConvId && tabConvId === conversationId)) {
        return { tabId: tab.id, reopened: false };
      }
    }
  }
  // 2. Not found: reopen in background without focus stealing (active: false)
  const created = await chrome.tabs.create({
    url: conversationUrl,
    active: false
  });
  return { tabId: created.id, reopened: true };
}

async function dispatchSingleReceipt(receiptRecord, stored, profileId) {
  // Hard gate: Only "received" or "not-sent" can attempt dispatch
  if (!receiptRecord || (receiptRecord.deliveryStatus !== "received" && receiptRecord.deliveryStatus !== "not-sent")) {
    return { status: "skipped", reason: "not_eligible_for_dispatch" };
  }

  const { receiptId, executionId, originConversationId } = receiptRecord;
  const receiptStorageKey = "receipt_" + receiptId;
  let originConversationUrl = receiptRecord.originConversationUrl;
  if (!originConversationUrl && executionId) {
    const allData = await chrome.storage.local.get(null);
    for (const [k, v] of Object.entries(allData)) {
      if (k.startsWith("launch_") && v && v.executionId === executionId && v.originConversationUrl) {
        originConversationUrl = v.originConversationUrl;
        break;
      }
    }
  }
  if (!originConversationUrl) {
    originConversationUrl = `https://chatgpt.com/c/${originConversationId}`;
  }

  // 1. Locate or reopen the bound ChatGPT tab in the background without focus stealing
  let tabInfo;
  try {
    tabInfo = await findOrReopenConversationTab(originConversationUrl, originConversationId);
  } catch (err) {
    return { status: "error", code: "tab_lookup_failed", message: String(err) };
  }
  if (!tabInfo || !tabInfo.tabId) {
    return { status: "error", code: "tab_unavailable", message: "Failed to locate or open background tab" };
  }

  const tabId = tabInfo.tabId;

  // 2. Pre-grant check: query content script for delivery readiness and document identity
  let readinessResp;
  try {
    readinessResp = await new Promise((resolve) => {
      chrome.tabs.sendMessage(
        tabId,
        {
          action: "check_delivery_readiness",
          expectedConversationId: originConversationId,
          expectedConversationUrl: originConversationUrl
        },
        { frameId: 0 },
        (resp) => resolve(resp)
      );
    });
  } catch (err) {
    return { status: "waiting", reason: "tab_not_ready", message: String(err) };
  }

  if (!readinessResp || !readinessResp.ok || !readinessResp.documentId) {
    return {
      status: "waiting",
      reason: readinessResp?.readiness?.reason || "readiness_guards_not_passed",
      message: readinessResp?.readiness?.message || "Content script reports page not ready"
    };
  }

  const documentId = readinessResp.documentId;
  const transcriptText = readinessResp.readiness.transcriptText || "";
  const accountText = readinessResp.readiness.accountText || "";

  const transcriptEvidenceHash = await sha256Hex(transcriptText);
  const accountEvidenceHash = await sha256Hex(accountText);

  // Build bounded synthetic continuation
  const { receiptMarker, continuationText } = buildContinuationPayload(receiptRecord);
  const payloadDigest = await sha256Hex(continuationText);

  // Preallocate attempt ID and revision
  const attemptId = "att_" + crypto.randomUUID().replace(/-/g, "").slice(0, 16);
  const expectedDeliveryRevision = (receiptRecord.deliveryRevision || 0) + 1;

  // 3. Atomically acquire Dispatch Fence ownership from native SQLite journal
  const fenceResponse = await sendNative({
    op: "dispatch_fence",
    pairingId: stored.pairingId,
    pairingSecret: stored.pairingSecret,
    profileId,
    receiptId,
    executionId,
    attemptId,
    expectedDeliveryRevision,
    payloadDigest,
    receiptMarker,
    originConversationId,
    originConversationUrl,
    accountEvidenceHash,
    transcriptEvidenceHash,
    tabId: String(tabId),
    documentId
  });

  if (!fenceResponse || fenceResponse.status !== "ok" || !fenceResponse.grant || !fenceResponse.grant.granted) {
    const grant = fenceResponse?.grant;
    if (grant) {
      let updated = false;
      if (grant.delivery_revision && (receiptRecord.deliveryRevision || 0) < grant.delivery_revision) {
        receiptRecord.deliveryRevision = grant.delivery_revision;
        updated = true;
      }
      if (grant.state && receiptRecord.deliveryStatus !== grant.state) {
        receiptRecord.deliveryStatus = grant.state;
        updated = true;
      }
      if (updated) {
        await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
      }
    }
    // Loser or slot busy: receives status, NEVER permission!
    return {
      status: "denied",
      reason: "slot_busy_or_competing_owner",
      grantState: fenceResponse?.grant?.state
    };
  }

  // Mark local state as dispatching/uncertain
  receiptRecord.deliveryStatus = "dispatching/uncertain";
  receiptRecord.deliveryRevision = expectedDeliveryRevision;
  receiptRecord.activeAttemptId = attemptId;
  receiptRecord.activeDocumentId = documentId;
  await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });

  // 4. Send attempt-bound grant to live document for one-time consumption & synchronous guard+click
  let clickResp = null;
  let clickChannelError = null;
  try {
    clickResp = await new Promise((resolve) => {
      chrome.tabs.sendMessage(
        tabId,
        {
          action: "consume_grant_and_dispatch",
          attemptId,
          expectedDocumentId: documentId,
          expectedConversationId: originConversationId,
          expectedConversationUrl: originConversationUrl,
          continuationText,
          receiptMarker
        },
        { frameId: 0 },
        (resp) => {
          if (chrome.runtime.lastError) {
            clickChannelError = chrome.runtime.lastError.message || "runtime_channel_error";
            resolve(null);
          } else {
            resolve(resp);
          }
        }
      );
    });
  } catch (err) {
    clickChannelError = String(err);
  }

  if (clickChannelError || !clickResp) {
    // Finding 4: Response-channel failure or ambiguity after possible click: outcome is uncertain!
    // Never treat channel failure as not-sent, never release the slot!
    const channelReason = clickChannelError || "no_response_from_content_script";
    await sendNative({
      op: "settle_fence",
      pairingId: stored.pairingId,
      pairingSecret: stored.pairingSecret,
      profileId,
      receiptId,
      executionId,
      attemptId,
      expectedDeliveryRevision,
      outcome: "uncertain",
      details: channelReason
    });
    receiptRecord.deliveryStatus = "dispatching/uncertain";
    await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
    return { status: "uncertain", reason: "click_dispatch_communication_error", error: channelReason };
  }

  if (!clickResp.ok || !clickResp.clicked) {
    // Document definitively rejected grant or guards failed synchronously before click:
    // Settle conclusively as not-sent so slot can be released
    await sendNative({
      op: "settle_fence",
      pairingId: stored.pairingId,
      pairingSecret: stored.pairingSecret,
      profileId,
      receiptId,
      executionId,
      attemptId,
      expectedDeliveryRevision,
      outcome: "not-sent",
      details: clickResp.reason || "synchronous_guard_failed"
    });
    receiptRecord.deliveryStatus = "not-sent";
    await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
    return { status: "not-sent", reason: clickResp.reason };
  }

  // 5. Verify submitted user message in transcript
  // Wait briefly for optimistic/submitted render
  let verifyResp = null;
  for (let i = 0; i < 3; i++) {
    try {
      verifyResp = await new Promise((resolve) => {
        chrome.tabs.sendMessage(
          tabId,
          {
            action: "verify_submitted_message",
            receiptMarker,
            expectedConversationId: originConversationId
          },
          { frameId: 0 },
          (resp) => resolve(resp)
        );
      });
      if (verifyResp && verifyResp.ok && verifyResp.observed && verifyResp.observedMessageId) {
        break;
      }
    } catch {
      // Tab may be re-rendering
    }
    await new Promise((r) => setTimeout(r, 200));
  }

  if (verifyResp && verifyResp.ok && verifyResp.observed && verifyResp.observedMessageId) {
    // Conclusive submitted-observed!
    const settleResp = await sendNative({
      op: "settle_fence",
      pairingId: stored.pairingId,
      pairingSecret: stored.pairingSecret,
      profileId,
      receiptId,
      executionId,
      attemptId,
      expectedDeliveryRevision,
      outcome: "submitted-observed",
      observedMessageId: verifyResp.observedMessageId,
      transcriptEvidenceHash: await sha256Hex(verifyResp.transcriptText || "")
    });

    receiptRecord.deliveryStatus = "submitted-observed";
    receiptRecord.observedMessageId = verifyResp.observedMessageId;
    receiptRecord.settledAt = Date.now();
    await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });

    return {
      status: "ok",
      outcome: "submitted-observed",
      receiptId,
      observedMessageId: verifyResp.observedMessageId,
      slotReleased: settleResp?.settlement?.slot_released
    };
  } else {
    // Inconclusive outcome: retain uncertain! CAS settlement records uncertainty without releasing slot
    await sendNative({
      op: "settle_fence",
      pairingId: stored.pairingId,
      pairingSecret: stored.pairingSecret,
      profileId,
      receiptId,
      executionId,
      attemptId,
      expectedDeliveryRevision,
      outcome: "uncertain",
      details: "Message not yet observed in transcript after click"
    });
    receiptRecord.deliveryStatus = "dispatching/uncertain";
    await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
    return {
      status: "uncertain",
      outcome: "uncertain",
      receiptId
    };
  }
}

function sameLaunchPayload(record, payload) {
  return !!record &&
    record.originConversationId === payload.originConversationId &&
    record.originConversationUrl === payload.originConversationUrl &&
    record.transcriptEvidenceHash === payload.transcriptEvidenceHash &&
    record.accountEvidenceHash === payload.accountEvidenceHash &&
    record.targetId === payload.targetId &&
    record.requestedPolicyRevision === payload.requestedPolicyRevision &&
    record.promptText === payload.promptText;
}

function unresolvedLaunchResponse(launchRequestId, record, message) {
  return {
    status: "error",
    code: "active_launch_unresolved",
    message,
    launchRequestId: launchRequestId || null,
    executionId: record?.executionId,
    state: record?.status || "pending_native"
  };
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
      if (["setup", "status", "connect", "revoke", "launch", "recover", "drain", "dispatchReceipt", "getConversationTarget", "setConversationTarget", "ensureLocalMode"].includes(request.action)) {
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
        case "ensureLocalMode": {
          let response;
          try {
            response = await sendNative({ op: "local_status" });
          } catch (err) {
            sendResponse({
              status: "error",
              code: "native_host_unavailable",
              message: err.message || String(err)
            });
            break;
          }
          if (!response || response.status !== "ok") {
            sendResponse(response || { status: "error", code: "native_host_unavailable" });
            break;
          }
          const targets = Array.isArray(response.targets) ? response.targets : [];
          await chrome.storage.local.set({
            profileId: LOCAL_PROFILE_ID,
            isPaired: true,
            pairingId: LOCAL_PAIRING_ID,
            pairingSecret: LOCAL_PAIRING_SECRET,
            policyRevision: LOCAL_POLICY_REVISION,
            targets
          });
          sendResponse({
            status: "ok",
            isPaired: true,
            pairingStatus: response.pairingStatus || "active",
            taskExecutionAvailable: response.taskExecutionAvailable !== false,
            targetsCount: targets.length,
            targets
          });
          break;
        }


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
            if (Array.isArray(response.targets)) {
              await chrome.storage.local.set({ targets: response.targets });
            }
            sendResponse({
              status: "ok",
              isPaired: true,
              profileId,
              pairingStatus: response.pairingStatus,
              targetsCount: response.targetsCount,
              targets: response.targets || stored.targets || [],
              policyRevision: response.policyRevision,
              taskExecutionAvailable: response.taskExecutionAvailable,
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

          // Resolve target: saved binding is authoritative.
          // If request.targetId is also supplied and differs from an existing valid binding, fail closed.
          // If unbound, an explicit registered targetId may be used and establishes the binding.
          // Fallback to single target for unbound conversations.
          const hasAuthoritativeTargets = Array.isArray(stored.targets);
          const availableTargets = hasAuthoritativeTargets ? stored.targets : [];
          const convBindingKey = `conv_target_${stored.pairingId}_${originConversationId}`;
          const requestedTargetId = request.targetId?.trim() || null;
          const storedBinding = (await chrome.storage.local.get([convBindingKey]))[convBindingKey];
          const existingBinding = (typeof storedBinding === "string" && storedBinding.trim()) ? storedBinding.trim() : null;

          let targetId = null;
          if (existingBinding) {
            // Check if existing binding is valid
            if (hasAuthoritativeTargets && !availableTargets.some(t => t.target_id === existingBinding)) {
              // Stale binding to removed target: prune and fail closed
              await chrome.storage.local.remove([convBindingKey]);
              sendResponse({
                status: "error",
                code: "target_not_found",
                message: `Bound target '${existingBinding}' is not registered or was removed from this pairing.`
              });
              return;
            }

            // Binding is authoritative: if request.targetId is also supplied and differs, fail closed with mismatch
            if (requestedTargetId && requestedTargetId !== existingBinding) {
              sendResponse({
                status: "error",
                code: "conversation_target_mismatch",
                message: `Target mismatch: conversation is bound to target '${existingBinding}', but launch requested '${requestedTargetId}'.`
              });
              return;
            }
            targetId = existingBinding;
          } else {
            // Conversation is unbound
            if (requestedTargetId) {
              // Validate against available targets if targets list is known
              if (hasAuthoritativeTargets && !availableTargets.some(t => t.target_id === requestedTargetId)) {
                sendResponse({
                  status: "error",
                  code: "target_not_found",
                  message: `Target '${requestedTargetId}' is not registered.`
                });
                return;
              }
              // Establish binding for future launches
              try {
                await chrome.storage.local.set({ [convBindingKey]: requestedTargetId });
              } catch (storageErr) {
                sendResponse({
                  status: "error",
                  code: "browser_persistence_failure",
                  message: "Failed to persist conversation target binding in browser storage; launch aborted"
                });
                return;
              }
              targetId = requestedTargetId;
            } else if (availableTargets.length === 1) {
              // Fallback to the only registered target and make that choice durable for
              // this conversation before the pairing gains additional workspaces.
              targetId = availableTargets[0].target_id;
              try {
                await chrome.storage.local.set({ [convBindingKey]: targetId });
              } catch (storageErr) {
                sendResponse({
                  status: "error",
                  code: "browser_persistence_failure",
                  message: "Failed to persist conversation target binding in browser storage; launch aborted"
                });
                return;
              }
            }
          }

          if (!targetId) {
            sendResponse({
              status: "error",
              code: "missing_target_id",
              message: availableTargets.length === 0
                ? "No workspace targets registered. Add a target via hands-return-bridge target add."
                : "Target ID is required. Select a workspace target for this conversation."
            });
            return;
          }

          // Final sanity check when targets list is known
          if (hasAuthoritativeTargets && !availableTargets.some(t => t.target_id === targetId)) {
            await chrome.storage.local.remove([convBindingKey]);
            sendResponse({
              status: "error",
              code: "target_not_found",
              message: `Target '${targetId}' is not registered or was removed from this pairing.`
            });
            return;
          }
          const promptText = request.promptText;
          if (!promptText || typeof promptText !== "string" || !promptText.trim()) {
            sendResponse({ status: "error", code: "missing_prompt_text", message: "Prompt text is required" });
            return;
          }
          if (new TextEncoder().encode(promptText).byteLength > 128 * 1024) {
            sendResponse({ status: "error", code: "prompt_too_large", message: "Prompt exceeds bounded size limit (128 KB)" });
            return;
          }

          const requestedPolicyRevision = request.requestedPolicyRevision?.trim() || stored.policyRevision || "v1";
          const activeKey = `active_launch_${stored.pairingId}_${originConversationId}_${targetId}_${requestedPolicyRevision}`;
          const explicitLaunchRequestId = request.launchRequestId?.trim() || null;
          const requestSignature = JSON.stringify({
            tabId,
            originConversationId,
            originConversationUrl,
            targetId,
            requestedPolicyRevision,
            promptText,
            explicitLaunchRequestId
          });

          const inFlight = inFlightLaunches.get(activeKey);
          if (inFlight) {
            if (inFlight.signature !== requestSignature) {
              sendResponse(unresolvedLaunchResponse(
                inFlight.launchRequestId,
                null,
                "A launch is already in flight for this conversation/target with a different payload or request ID"
              ));
              return;
            }
            sendResponse(await inFlight.promise);
            return;
          }

          const inFlightEntry = { signature: requestSignature, launchRequestId: explicitLaunchRequestId, promise: null };
          const launchPromise = (async () => {

          // Obtain evidence directly from content script on bound top-level tab
          let evidenceRes;
          try {
            evidenceRes = await new Promise((resolve, reject) => {
              chrome.tabs.sendMessage(tabId, { action: "collect_page_evidence" }, { frameId: 0 }, (res) => {
                if (chrome.runtime.lastError) {
                  return reject(new Error(chrome.runtime.lastError.message));
                }
                resolve(res);
              });
            });
          } catch (contentErr) {
            return {
              status: "error",
              code: "evidence_collection_failed",
              message: "Failed to communicate with content script on bound tab: " + contentErr.message
            };
          }

          if (!evidenceRes || !evidenceRes.ok) {
            return {
              status: "error",
              code: evidenceRes?.error || "evidence_collection_failed",
              message: evidenceRes?.message || "Content script failed to collect page evidence"
            };
          }

          if (evidenceRes.originConversationId !== originConversationId || evidenceRes.originConversationUrl !== originConversationUrl) {
            return {
              status: "error",
              code: "conversation_binding_mismatch",
              message: "Content script conversation ID or URL does not match bound top-level tab"
            };
          }

          const transcriptText = evidenceRes.transcriptText?.trim();
          const accountText = evidenceRes.accountText?.trim();
          if (!transcriptText) {
            return {
              status: "error",
              code: "missing_rendered_transcript",
              message: "Rendered transcript text is empty or unavailable"
            };
          }
          if (!accountText) {
            return {
              status: "error",
              code: "missing_account_context",
              message: "Account/workspace context evidence is empty or unavailable"
            };
          }

          // Compute evidence hashes inside trusted background
          const transcriptEvidenceHash = await sha256Hex(evidenceRes.transcriptText);
          const accountEvidenceHash = await sha256Hex(evidenceRes.accountText);
          const launchPayload = {
            originConversationId,
            originConversationUrl,
            transcriptEvidenceHash,
            accountEvidenceHash,
            targetId,
            requestedPolicyRevision,
            promptText
          };
            // Installation-local request identity matches exact immutable payload.
            const activeStored = await chrome.storage.local.get([activeKey]);
            const existingActiveReqId = activeStored[activeKey];
            let launchRequestId;

            if (existingActiveReqId) {
              const storedActive = await chrome.storage.local.get(["launch_" + existingActiveReqId]);
              const existingActiveRecord = storedActive["launch_" + existingActiveReqId];
              const isUnresolved = existingActiveRecord && UNRESOLVED_LAUNCH_STATES.has(existingActiveRecord.status);
              if (isUnresolved) {
                if (explicitLaunchRequestId && explicitLaunchRequestId !== existingActiveReqId) {
                  return unresolvedLaunchResponse(
                    existingActiveReqId,
                    existingActiveRecord,
                    `An unresolved launch request (${existingActiveReqId}) is already active; the supplied launchRequestId does not match it.`
                  );
                }
                if (!sameLaunchPayload(existingActiveRecord, launchPayload)) {
                  return unresolvedLaunchResponse(
                    existingActiveReqId,
                    existingActiveRecord,
                    `An unresolved launch request (${existingActiveReqId}) with state '${existingActiveRecord.status}' is active for this conversation/target. Resolve or recover it before initiating a new task.`
                  );
                }
                launchRequestId = existingActiveReqId;
              } else {
                launchRequestId = explicitLaunchRequestId || ("req_" + Date.now() + "_" + Math.random().toString(36).slice(2, 10));
              }
            } else {
              launchRequestId = explicitLaunchRequestId || ("req_" + Date.now() + "_" + Math.random().toString(36).slice(2, 10));
            }
            inFlightEntry.launchRequestId = launchRequestId;

            const pendingLaunchKey = "launch_" + launchRequestId;
            const storedLaunch = await chrome.storage.local.get([pendingLaunchKey]);
            const existingRecord = storedLaunch[pendingLaunchKey];
            if (existingRecord && !sameLaunchPayload(existingRecord, launchPayload)) {
              return {
                status: "error",
                code: "payload_conflict",
                message: "A launch request with this ID already exists with a different payload"
              };
            }

            const launchRecord = {
              launchRequestId,
              pairingId: stored.pairingId,
              ...launchPayload,
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
              return {
                status: "error",
                code: "browser_persistence_failure",
                message: "Failed to persist launch request in browser storage; native request aborted"
              };
            }

            let response;
            try {
              response = await sendNative({
                op: "launch",
                pairingId: stored.pairingId,
                pairingSecret: stored.pairingSecret,
                profileId,
                launchRequestId,
                ...launchPayload
              });
            } catch (nativeErr) {
              launchRecord.status = "native-response-uncertain";
              launchRecord.lastError = nativeErr.message;
              await chrome.storage.local.set({ [pendingLaunchKey]: launchRecord });
              return {
                status: "error",
                code: "native_response_uncertain",
                launchRequestId,
                state: "native-response-uncertain",
                message: "Native messaging host response was lost or rejected: " + nativeErr.message,
                trustNotice: TRUST_NOTICE
              };
            }

            if (response && response.status === "ok") {
              const nativeState = response.state || "unknown";
              launchRecord.status = nativeState;
              launchRecord.executionId = response.executionId;
              launchRecord.returnToken = response.returnToken;
              launchRecord.terminalEvidence = response.terminalEvidence;
              const storageUpdate = { [pendingLaunchKey]: launchRecord };
              if (PROVEN_LAUNCH_STATES.has(nativeState)) {
                storageUpdate.activeExecutionId = response.executionId;
              }
              await chrome.storage.local.set(storageUpdate);
              if (!PROVEN_LAUNCH_STATES.has(nativeState)) {
                return {
                  ...response,
                  status: "error",
                  code: "launch_unresolved",
                  message: `Native launch is not proven started; durable state is '${nativeState}'. Recover before retrying.`
                };
              }
            } else if (response && response.code === "launch_uncertain") {
              launchRecord.status = "unknown";
              launchRecord.executionId = response.executionId;
              launchRecord.terminalEvidence = response.terminalEvidence;
              await chrome.storage.local.set({ [pendingLaunchKey]: launchRecord });
            } else if (response && response.status === "error") {
              launchRecord.status = "rejected";
              launchRecord.rejectCode = response.code;
              launchRecord.rejectMessage = response.message;
              await chrome.storage.local.set({ [pendingLaunchKey]: launchRecord });
              await chrome.storage.local.remove([activeKey]);
              if (response.code === "target_not_found") {
                const staleState = await chrome.storage.local.get(["targets", convBindingKey]);
                if (Array.isArray(staleState.targets)) {
                  await chrome.storage.local.set({
                    targets: staleState.targets.filter(t => t.target_id !== targetId)
                  });
                }
                const currentBinding = (await chrome.storage.local.get([convBindingKey]))[convBindingKey];
                if (currentBinding === targetId) {
                  await chrome.storage.local.remove([convBindingKey]);
                }
              }
            }
            return response;
          })();

          inFlightEntry.promise = launchPromise;
          inFlightLaunches.set(activeKey, inFlightEntry);
          try {
            sendResponse(await launchPromise);
          } finally {
            if (inFlightLaunches.get(activeKey)?.promise === launchPromise) {
              inFlightLaunches.delete(activeKey);
            }
          }
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
                if (cur && UNRESOLVED_LAUNCH_STATES.has(cur.status)) {
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
        case "drain": {
          const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
          if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
            sendResponse({ status: "error", code: "not_paired", message: "Extension is not paired" });
            return;
          }

          await ensureRecoveryAlarm();
          const payload = {
            op: "drain",
            pairingId: stored.pairingId,
            pairingSecret: stored.pairingSecret,
            profileId
          };
          if (typeof request.limit === "number") {
            payload.limit = request.limit;
          }

          const response = await sendNative(payload);
          if (response && response.status === "ok") {
            const ackedIds = await processDrainResponse(response, stored, profileId);
            sendResponse({
              status: "ok",
              summaries: response.summaries || [],
              receipts: response.receipts || [],
              ackedReceiptIds: ackedIds,
              trustNotice: TRUST_NOTICE
            });
          } else {
            sendResponse(response || { status: "error", code: "native_failed" });
          }
          break;
        }

        case "getPendingReceipts": {
          const allData = await chrome.storage.local.get(null);
          const pending = [];
          for (const [k, v] of Object.entries(allData)) {
            if (k.startsWith("receipt_") && v && v.deliveryStatus === "received") {
              pending.push(v);
            }
          }
          sendResponse({
            status: "ok",
            pendingReceipts: pending
          });
          break;
        }

        case "dispatchReceipt": {
          const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
          if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
            sendResponse({ status: "error", code: "not_paired", message: "Extension is not paired" });
            return;
          }
          const receiptId = request.receiptId;
          if (!receiptId) {
            sendResponse({ status: "error", code: "missing_receipt_id", message: "Missing 'receiptId'" });
            return;
          }
          const receiptKey = "receipt_" + receiptId;
          const receiptRecord = (await chrome.storage.local.get([receiptKey]))[receiptKey];
          if (!receiptRecord) {
            sendResponse({ status: "error", code: "receipt_not_found", message: "Receipt record not found" });
            return;
          }
          const dispatchResult = await dispatchSingleReceipt(receiptRecord, stored, profileId);
          sendResponse({
            status: "ok",
            dispatchResult,
            trustNotice: TRUST_NOTICE
          });
          break;
        }

        case "ensureAlarms": {
          await ensureRecoveryAlarm();
          const alarm = chrome.alarms ? await chrome.alarms.get(RECOVERY_ALARM_NAME) : null;
          sendResponse({
            status: "ok",
            alarmScheduled: !!alarm,
            alarmName: RECOVERY_ALARM_NAME
          });
          break;
        }

        case "getConversationTarget": {
          const convId = request.conversationId?.trim();
          if (!convId) {
            sendResponse({ status: "error", code: "missing_conversation_id" });
            return;
          }
          const stored = await chrome.storage.local.get(["pairingId", "targets"]);
          if (!stored.pairingId) {
            sendResponse({ status: "ok", targetId: null });
            return;
          }
          const bindingKey = `conv_target_${stored.pairingId}_${convId}`;
          const bindingVal = (await chrome.storage.local.get([bindingKey]))[bindingKey];
          const availableTargets = Array.isArray(stored.targets) ? stored.targets : [];
          let resolvedTargetId = null;
          if (bindingVal && availableTargets.some(t => t.target_id === bindingVal)) {
            resolvedTargetId = bindingVal;
          } else if (!bindingVal && availableTargets.length === 1) {
            resolvedTargetId = availableTargets[0].target_id;
          }
          sendResponse({
            status: "ok",
            conversationId: convId,
            targetId: resolvedTargetId,
            targets: availableTargets
          });
          break;
        }

        case "setConversationTarget": {
          const convId = request.conversationId?.trim();
          const targetId = request.targetId?.trim();
          if (!convId) {
            sendResponse({ status: "error", code: "missing_conversation_id" });
            return;
          }
          const stored = await chrome.storage.local.get(["pairingId", "targets"]);
          if (!stored.pairingId) {
            sendResponse({ status: "error", code: "not_paired" });
            return;
          }
          const bindingKey = `conv_target_${stored.pairingId}_${convId}`;
          if (!targetId) {
            await chrome.storage.local.remove([bindingKey]);
            sendResponse({ status: "ok", conversationId: convId, targetId: null });
            return;
          }
          const availableTargets = Array.isArray(stored.targets) ? stored.targets : [];
          if (!availableTargets.some(t => t.target_id === targetId)) {
            sendResponse({ status: "error", code: "target_not_found", message: `Target '${targetId}' is not registered.` });
            return;
          }
          await chrome.storage.local.set({ [bindingKey]: targetId });
          sendResponse({ status: "ok", conversationId: convId, targetId });
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
