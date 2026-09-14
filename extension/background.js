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
const PUSH_RECONNECT_DELAY_MS = 2000;
let nativeEventPort = null;
let nativeEventReconnectTimer = null;
let recoveryDrainPromise = null;
// Receipts whose dispatch is running in this service-worker instance. A restart clears
// this set, which is exactly what makes their durable uncertainty resolvable again.
const inFlightDispatches = new Set();
const TRANSCRIPT_ABSENCE_GRACE_MS = 60 * 1000;

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
ensureNativePushChannel();

chrome.runtime.onInstalled?.addListener(() => {
  ensureStorageAccessLevel();
  ensureNativePushChannel();
});

chrome.runtime.onStartup?.addListener(() => {
  ensureNativePushChannel();
});

function scheduleNativePushReconnect() {
  if (!chrome.runtime?.connectNative || nativeEventPort || nativeEventReconnectTimer) return;
  nativeEventReconnectTimer = setTimeout(() => {
    nativeEventReconnectTimer = null;
    ensureNativePushChannel();
  }, PUSH_RECONNECT_DELAY_MS);
}

function ensureNativePushChannel() {
  if (!chrome.runtime?.connectNative || nativeEventPort) return;
  try {
    const port = chrome.runtime.connectNative(NATIVE_HOST);
    nativeEventPort = port;
    let recoveredAfterSubscribe = false;

    port.onMessage.addListener((message) => {
      if (message?.status === "ok" && message?.subscribed === true) {
        if (!recoveredAfterSubscribe) {
          recoveredAfterSubscribe = true;
          void performRecoveryDrain();
        }
        return;
      }
      if (message?.event === "receipt_ready") {
        void performRecoveryDrain();
      }
    });

    port.onDisconnect.addListener(() => {
      // Reading lastError suppresses Chrome's unchecked runtime.lastError warning.
      void chrome.runtime.lastError?.message;
      if (nativeEventPort === port) nativeEventPort = null;
      scheduleNativePushReconnect();
    });

    port.postMessage({ op: "subscribe_events" });
  } catch (err) {
    nativeEventPort = null;
    console.warn("Return Bridge push channel unavailable:", err);
    scheduleNativePushReconnect();
  }
}

async function performRecoveryDrain() {
  if (recoveryDrainPromise) return recoveryDrainPromise;
  recoveryDrainPromise = (async () => {
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
        if (!k.startsWith("receipt_") || !v) continue;
        if (v.deliveryStatus === "dispatching/uncertain") {
          // Attempt never reported back: settle from transcript evidence instead of
          // holding the conversation slot forever or blindly re-sending.
          await resolveStuckDispatch(v, stored, profileId);
          continue;
        }
        if (v.deliveryStatus === "received" || v.deliveryStatus === "not-sent") {
          await dispatchSingleReceipt(v, stored, profileId);
        }
      }
    }
  })();
  try {
    return await recoveryDrainPromise;
  } catch (err) {
    console.warn("Recovery drain failed:", err);
  } finally {
    recoveryDrainPromise = null;
  }
}

function buildReceiptRecord(rcpt, fallbackOriginConversationUrl, fallbackTaskId) {
  return {
    receiptId: rcpt.receipt_id,
    executionId: rcpt.execution_id,
    taskId: rcpt.task_id || fallbackTaskId || undefined,
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
        const reconstructedRecord = buildReceiptRecord(rcpt, summary.origin_conversation_url, summary.launch_request_id);
        try {
          await chrome.storage.local.set({
            [receiptStorageKey]: reconstructedRecord,
            [executionReceiptKey]: rcpt.receipt_id
          });
        } catch (storageErr) {
          console.error("Failed to reconstruct receipt " + rcpt.receipt_id + " from summary:", storageErr);
        }
      } else {
        // Reconcile existing record with native durable fence. Conclusive native
        // states (not-sent/submitted-observed) must not be masked by stale local
        // dispatching/uncertain. Terminal local state is preserved unless native
        // reports a newer conclusive revision. No lease, no auto-retry of uncertainty.
        const NATIVE_CONCLUSIVE = new Set(["not-sent", "submitted-observed"]);
        const nativeRev = rcpt.delivery_revision || 0;
        const localRev = existing.deliveryRevision || 0;
        const nativeConclusive = rcpt.delivery_status && NATIVE_CONCLUSIVE.has(rcpt.delivery_status);
        const localStaleUncertain = existing.deliveryStatus === "dispatching/uncertain" || existing.deliveryStatus === "dispatching";
        let updated = false;
        if (rcpt.delivery_revision && localRev < nativeRev) {
          existing.deliveryRevision = rcpt.delivery_revision;
          updated = true;
        }
        if (rcpt.delivery_status && rcpt.delivery_status !== existing.deliveryStatus) {
          if (localRev < nativeRev || (nativeConclusive && localStaleUncertain && nativeRev >= localRev)) {
            existing.deliveryStatus = rcpt.delivery_status;
            updated = true;
          }
        }
        if ((rcpt.origin_conversation_url || summary.origin_conversation_url) && !existing.originConversationUrl) {
          existing.originConversationUrl = rcpt.origin_conversation_url || summary.origin_conversation_url;
          updated = true;
        }
        if ((rcpt.task_id || summary.launch_request_id) && !existing.taskId) {
          existing.taskId = rcpt.task_id || summary.launch_request_id;
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
    // Preserve existing terminal delivery state, but never let stale local
    // dispatching/uncertain mask a newer conclusive native state (not-sent/submitted-observed).
    const existing = (await chrome.storage.local.get([receiptStorageKey]))[receiptStorageKey];
    let receiptRecord = buildReceiptRecord(rcpt);
    if (existing && existing.deliveryStatus && existing.deliveryStatus !== "received") {
      const conclusiveNative = rcpt.delivery_status === "not-sent" || rcpt.delivery_status === "submitted-observed";
      const nativeRev = rcpt.delivery_revision || 0;
      const localRev = existing.deliveryRevision || 0;
      // A newer local dispatch attempt must win over an older conclusive native
      // revision; otherwise a racing drain response would roll the receipt (and
      // its revision) back and leave the native slot held.
      if (!(conclusiveNative && nativeRev >= localRev)) {
        receiptRecord = existing;
      }
    }

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
  // Canonicalize to the pathname (drop any query/fragment) so share/tracking params or
  // anchors do not hide a valid conversation route, while the exact /c/<id> or
  // /g/<gizmo>/c/<id> boundary is still enforced.
  const queryStart = urlStr.search(/[?#]/);
  const path = queryStart === -1
    ? urlStr.slice("https://chatgpt.com/".length)
    : urlStr.slice("https://chatgpt.com/".length, queryStart);
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
  const receiptMarker = `[hands-bridge:receipt=${rcpt.receiptId}]`;
  const taskId = rcpt.taskId || rcpt.task_id || rcpt.executionId;
  const executionId = rcpt.executionId || rcpt.execution_id;
  const terminalStatus = rcpt.state || "completed";

  let continuationText;
  if (terminalStatus === "failed") {
    const rawMsg = rcpt.assistantText || rcpt.assistant_text || "";
    const boundedMsg = typeof rawMsg === "string" && rawMsg.trim() ? rawMsg.trim().slice(0, 1024) : "";
    const failureDetail = boundedMsg ? `: ${boundedMsg}` : "";
    continuationText = `[Hands Return Bridge] Local agent execution failed (task: ${taskId}, execution: ${executionId}, receipt: ${rcpt.receiptId}, status: ${terminalStatus})${failureDetail}. Please inspect local agent/repository truth and continue. ${receiptMarker}`;
  } else {
    continuationText = `[Hands Return Bridge] Local agent execution completed (task: ${taskId}, execution: ${executionId}, receipt: ${rcpt.receiptId}, status: ${terminalStatus}). Please inspect local agent/repository truth and continue. ${receiptMarker}`;
  }
  return { receiptMarker, continuationText };
}

async function findExistingChatgptExecutionTab(conversationUrl, conversationId) {
  if (!chrome.tabs) return null;
  // Prefer the target conversation if it is already open, but request-native
  // dispatch only needs any logged-in chatgpt.com page as its execution context.
  const tabs = await chrome.tabs.query({ url: "https://chatgpt.com/*" });
  let fallbackTabId = null;
  for (const tab of tabs) {
    if (!fallbackTabId && tab.id) fallbackTabId = tab.id;
    if (tab.url) {
      const tabCleanUrl = tab.url.split("#")[0].split("?")[0];
      const tabConvId = parseCanonicalConversationId(tabCleanUrl);
      if (tabCleanUrl === conversationUrl || (tabConvId && tabConvId === conversationId)) {
        return { tabId: tab.id, exactConversation: true };
      }
    }
  }
  return fallbackTabId ? { tabId: fallbackTabId, exactConversation: false } : null;
}

async function dispatchReceiptViaChatgptRequest(receiptRecord, stored, profileId, tabId) {
  const { receiptId, executionId, originConversationId } = receiptRecord;
  const receiptStorageKey = "receipt_" + receiptId;
  const { receiptMarker, continuationText } = buildContinuationPayload(receiptRecord);
  const payloadDigest = await sha256Hex(continuationText);
  const attemptId = "att_" + crypto.randomUUID().replace(/-/g, "").slice(0, 16);
  const expectedDeliveryRevision = Math.max(0, Number(receiptRecord.deliveryRevision) || 0) + 1;
  // Native still uses documentId as the fence-owner key. Keep a non-DOM owner
  // until the request-native path is proven live, then remove that protocol debt.
  const documentId = "request_native_" + attemptId;
  const userMessageId = crypto.randomUUID();

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
    tabId: String(tabId),
    documentId
  });

  if (!fenceResponse || fenceResponse.status !== "ok" || !fenceResponse.grant || !fenceResponse.grant.granted) {
    const grant = fenceResponse?.grant;
    if (grant && grant.state === "submitted-observed" && grant.receipt_id === receiptId) {
      receiptRecord.deliveryStatus = "submitted-observed";
      if (grant.delivery_revision) receiptRecord.deliveryRevision = grant.delivery_revision;
      await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
    }
    return {
      status: "denied",
      reason: "slot_busy_or_competing_owner",
      grantState: grant?.state
    };
  }

  receiptRecord.deliveryStatus = "dispatching/uncertain";
  receiptRecord.deliveryRevision = expectedDeliveryRevision;
  receiptRecord.activeAttemptId = attemptId;
  receiptRecord.activeDocumentId = documentId;
  await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });

  let requestResult;
  try {
    const injected = await chrome.scripting.executeScript({
      target: { tabId },
      world: "MAIN",
      args: [originConversationId, continuationText, receiptMarker, userMessageId],
      func: async (conversationId, messageText, marker, messageId) => {
        const jsonFetch = async (url, options = {}) => {
          const response = await fetch(url, { credentials: "same-origin", ...options });
          let data = null;
          try {
            data = await response.json();
          } catch {
            // Some error responses are not JSON. Status remains enough for diagnostics.
          }
          return { response, data };
        };

        const authResult = await jsonFetch("/api/auth/session");
        const accessToken = authResult.data?.accessToken;
        if (!authResult.response.ok || !accessToken) {
          return { status: "not-sent", reason: "session_access_token_unavailable", httpStatus: authResult.response.status };
        }

        const authHeaders = { Authorization: `Bearer ${accessToken}` };
        const conversationResult = await jsonFetch(`/backend-api/conversation/${encodeURIComponent(conversationId)}`, {
          headers: authHeaders
        });
        const parentMessageId = conversationResult.data?.current_node;
        if (!conversationResult.response.ok || !parentMessageId) {
          return { status: "not-sent", reason: "conversation_head_unavailable", httpStatus: conversationResult.response.status };
        }
        // Project chats live under /g/<gizmo_id>/c/<conversation_id> and are rejected with 404 unless
        // the request carries their own gizmo conversation_mode.
        const pagePath = (globalThis.location && globalThis.location.pathname) || "";
        const conversationGizmoId = (typeof conversationResult.data?.gizmo_id === "string" && conversationResult.data.gizmo_id)
          || (pagePath.match(/\/g\/(g-[^/]+)\//) || [])[1]
          || "";

        // chatgpt2api's current Sentinel flow starts with a legacy `p` token,
        // then prepare + optional PoW + finalize.
        const pad2 = (value) => String(value).padStart(2, "0");
        const weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        const buildPowConfig = () => {
          const eastern = new Date(Date.now() - (5 * 60 * 60 * 1000));
          const legacyTime = `${weekdays[eastern.getUTCDay()]} ${months[eastern.getUTCMonth()]} ${pad2(eastern.getUTCDate())} ${eastern.getUTCFullYear()} ${pad2(eastern.getUTCHours())}:${pad2(eastern.getUTCMinutes())}:${pad2(eastern.getUTCSeconds())} GMT-0500 (Eastern Standard Time)`;
          const perfNow = performance.now();
          return [
            (screen?.width || 1920) + (screen?.height || 1080),
            legacyTime,
            4294705152,
            1,
            navigator.userAgent,
            "https://chatgpt.com/backend-api/sentinel/sdk.js",
            "",
            navigator.language || "en-US",
            Array.isArray(navigator.languages) ? navigator.languages.join(",") : "en-US",
            Math.random(),
            `webdriver−${Boolean(navigator.webdriver)}`,
            "location",
            "window",
            perfNow,
            crypto.randomUUID(),
            "",
            navigator.hardwareConcurrency || 8,
            Date.now() - perfNow,
            0, 0, 0, 0, 0, 0,
            0
          ];
        };
        const base64Utf8 = (value) => {
          const bytes = new TextEncoder().encode(value);
          let binary = "";
          for (const byte of bytes) binary += String.fromCharCode(byte);
          return btoa(binary);
        };
        const pToken = "gAAAAAC" + base64Utf8(JSON.stringify(buildPowConfig()));

        const prepareResult = await jsonFetch("/backend-api/sentinel/chat-requirements/prepare", {
          method: "POST",
          headers: { ...authHeaders, "Content-Type": "application/json" },
          body: JSON.stringify({ p: pToken })
        });
        if (!prepareResult.response.ok || !prepareResult.data?.prepare_token) {
          return { status: "not-sent", reason: "sentinel_prepare_failed", httpStatus: prepareResult.response.status };
        }
        // The turnstile challenge (`turnstile.dx`) is base64(xor(JSON.stringify(program), key)),
        // where `key` is the `p` token sent to prepare. `program` is bytecode for the register VM
        // the ChatGPT web app runs in this same page, so it must run in the MAIN world and stay
        // faithful to the SDK opcode semantics: instructions pass raw register addresses, register
        // 9 holds the instruction queue, 16 the xor key, 10 the page window (real browser APIs).
        const solveSentinelTurnstile = (challenge, key) => new Promise((resolve) => {
          const xorText = (text, xorKey) => {
            if (!xorKey) return text;
            let out = "";
            for (let index = 0; index < text.length; index++) {
              out += String.fromCharCode(text.charCodeAt(index) ^ xorKey.charCodeAt(index % xorKey.length));
            }
            return out;
          };
          const vm = new Map();
          let steps = 0;
          let settled = false;
          const finish = (token) => {
            if (settled) return;
            settled = true;
            resolve(token);
          };
          const get = (register) => vm.get(register);
          const call = (register, ...registers) => get(register)(...registers.map((entry) => get(entry)));
          const runQueue = async () => {
            while (get(9).length > 0) {
              if (settled || steps > 100000) throw new Error("turnstile_vm_stopped");
              const [opcode, ...args] = get(9).shift();
              const pending = get(opcode)(...args);
              if (pending && typeof pending.then === "function") await pending;
              steps += 1;
            }
          };
          const runSubroutine = (resultRegister, program) => {
            const previous = [...get(9)];
            vm.set(9, [...program]);
            return runQueue()
              .catch((error) => { vm.set(resultRegister, "" + error); })
              .then(() => { vm.set(9, previous); });
          };
          vm.set(0, (program) => {
            try {
              vm.set(9, JSON.parse(xorText(atob("" + program), "" + get(16))));
            } catch {
              return;
            }
            return runQueue().catch(() => {});
          });
          vm.set(1, (target, source) => { vm.set(target, xorText("" + get(target), "" + get(source))); });
          vm.set(2, (target, value) => { vm.set(target, value); });
          vm.set(3, (value) => { finish(btoa("" + value)); });
          vm.set(4, () => { finish(null); });
          vm.set(5, (target, source) => {
            const current = get(target);
            if (Array.isArray(current)) current.push(get(source));
            else vm.set(target, current + get(source));
          });
          vm.set(6, (target, object, keyRegister) => { vm.set(target, get(object)[get(keyRegister)]); });
          vm.set(7, (register, ...registers) => call(register, ...registers));
          vm.set(8, (target, source) => { vm.set(target, get(source)); });
          vm.set(10, window);
          vm.set(11, (target, pattern) => {
            vm.set(target, (Array.from(document.scripts || [])
              .map((script) => script?.src?.match(get(pattern)))
              .filter((match) => match?.length)[0] ?? [])[0] ?? null);
          });
          vm.set(12, (target) => { vm.set(target, vm); });
          vm.set(13, (target, register, ...args) => {
            try {
              get(register)(...args);
            } catch (error) {
              vm.set(target, "" + error);
            }
          });
          vm.set(14, (target, source) => { vm.set(target, JSON.parse("" + get(source))); });
          vm.set(15, (target, source) => { vm.set(target, JSON.stringify(get(source))); });
          vm.set(17, (target, register, ...registers) => {
            try {
              const value = call(register, ...registers);
              if (value && typeof value.then === "function") {
                return value.then(
                  (resolved) => { vm.set(target, resolved); },
                  (error) => { vm.set(target, "" + error); }
                );
              }
              vm.set(target, value);
            } catch (error) {
              vm.set(target, "" + error);
            }
          });
          vm.set(18, (target) => { vm.set(target, atob("" + get(target))); });
          vm.set(19, (target) => { vm.set(target, btoa("" + get(target))); });
          vm.set(20, (left, right, register, ...args) => (
            get(left) === get(right) ? get(register)(...args) : null
          ));
          vm.set(21, (left, right, threshold, register, ...args) => (
            Math.abs(get(left) - get(right)) > get(threshold) ? get(register)(...args) : null
          ));
          vm.set(22, (resultRegister, program) => runSubroutine(resultRegister, program));
          // Guarded calls must RETURN the callee's value: a subroutine reached through one of these
          // ops hands back a promise the queue loop has to await before it decides the queue is empty.
          vm.set(23, (register, target, ...args) => (
            get(register) !== undefined ? get(target)(...args) : null
          ));
          vm.set(24, (target, object, keyRegister) => { vm.set(target, get(object)[get(keyRegister)].bind(get(object))); });
          vm.set(25, () => {});
          vm.set(26, () => {});
          vm.set(27, (target, source) => {
            const current = get(target);
            if (Array.isArray(current)) current.splice(current.indexOf(get(source)), 1);
            else vm.set(target, current - get(source));
          });
          vm.set(28, () => {});
          vm.set(29, (target, left, right) => { vm.set(target, get(left) < get(right)); });
          vm.set(30, (target, resultRegister, parameters, program) => {
            const isSubroutine = Array.isArray(program);
            const parameterRegisters = isSubroutine ? parameters : [];
            const body = (isSubroutine ? program : parameters) || [];
            vm.set(target, (...args) => {
              if (settled) return;
              const previous = [...get(9)];
              if (isSubroutine) {
                for (let index = 0; index < parameterRegisters.length; index++) {
                  vm.set(parameterRegisters[index], args[index]);
                }
              }
              vm.set(9, [...body]);
              return runQueue()
                .then(() => get(resultRegister))
                .catch(() => "")
                .then((value) => { vm.set(9, previous); return value; });
            });
          });
          vm.set(33, (target, left, right) => { vm.set(target, Number(get(left)) * Number(get(right))); });
          vm.set(34, (target, source) => Promise.resolve(get(source)).then((value) => { vm.set(target, value); }));
          vm.set(35, (target, left, right) => {
            const divisor = Number(get(right));
            vm.set(target, divisor === 0 ? 0 : Number(get(left)) / divisor);
          });
          vm.set(16, key);
          try {
            vm.set(9, JSON.parse(xorText(atob(challenge), key)));
          } catch {
            finish(null);
            return;
          }
          runQueue().catch(() => {}).then(() => { finish(null); });
        });

        let turnstileToken = "";
        const turnstile = prepareResult.data?.turnstile;
        if (turnstile?.required) {
          if (typeof turnstile.dx !== "string" || !turnstile.dx) {
            return { status: "not-sent", reason: "sentinel_turnstile_challenge_missing" };
          }
          const solved = await Promise.race([
            solveSentinelTurnstile(turnstile.dx, pToken),
            new Promise((resolve) => setTimeout(() => resolve(null), 5000))
          ]);
          if (!solved) {
            return { status: "not-sent", reason: "sentinel_turnstile_failed" };
          }
          turnstileToken = solved;
        }

        let proofToken = "";
        if (prepareResult.data?.proofofwork?.required) {
          const proof = prepareResult.data.proofofwork;
          if (typeof proof.seed !== "string" || typeof proof.difficulty !== "string" || !proof.seed || !/^(?:[0-9a-fA-F]{2})+$/.test(proof.difficulty)) {
            return { status: "not-sent", reason: "sentinel_proof_invalid_challenge" };
          }

          // Chrome WebCrypto does not expose SHA3-512, so use the compact Keccak-f[1600]
          // implementation required by chatgpt2api's proof algorithm.
          const MASK64 = (1n << 64n) - 1n;
          const ROUND_CONSTANTS = [
            0x0000000000000001n, 0x0000000000008082n, 0x800000000000808an, 0x8000000080008000n,
            0x000000000000808bn, 0x0000000080000001n, 0x8000000080008081n, 0x8000000000008009n,
            0x000000000000008an, 0x0000000000000088n, 0x0000000080008009n, 0x000000008000000an,
            0x000000008000808bn, 0x800000000000008bn, 0x8000000000008089n, 0x8000000000008003n,
            0x8000000000008002n, 0x8000000000000080n, 0x000000000000800an, 0x800000008000000an,
            0x8000000080008081n, 0x8000000000008080n, 0x0000000080000001n, 0x8000000080008008n
          ];
          const ROTATIONS = [
            [0, 36, 3, 41, 18],
            [1, 44, 10, 45, 2],
            [62, 6, 43, 15, 61],
            [28, 55, 25, 21, 56],
            [27, 20, 39, 8, 14]
          ];
          const rotateLeft64 = (value, shift) => shift === 0
            ? value
            : ((value << BigInt(shift)) | (value >> BigInt(64 - shift))) & MASK64;
          const keccakPermutation = (state) => {
            const column = new Array(5);
            const delta = new Array(5);
            const rotated = new Array(25);
            for (const roundConstant of ROUND_CONSTANTS) {
              for (let x = 0; x < 5; x++) {
                column[x] = state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20];
              }
              for (let x = 0; x < 5; x++) {
                delta[x] = column[(x + 4) % 5] ^ rotateLeft64(column[(x + 1) % 5], 1);
              }
              for (let y = 0; y < 5; y++) {
                for (let x = 0; x < 5; x++) state[x + (5 * y)] ^= delta[x];
              }
              for (let y = 0; y < 5; y++) {
                for (let x = 0; x < 5; x++) {
                  rotated[y + (5 * ((2 * x + 3 * y) % 5))] = rotateLeft64(state[x + (5 * y)], ROTATIONS[x][y]);
                }
              }
              for (let y = 0; y < 5; y++) {
                for (let x = 0; x < 5; x++) {
                  state[x + (5 * y)] = rotated[x + (5 * y)] ^ ((~rotated[((x + 1) % 5) + (5 * y)]) & rotated[((x + 2) % 5) + (5 * y)]);
                }
              }
              state[0] ^= roundConstant;
            }
          };
          const sha3_512 = (input) => {
            const rate = 72;
            const state = new Array(25).fill(0n);
            let offset = 0;
            const absorb = (block) => {
              for (let laneIndex = 0; laneIndex < 9; laneIndex++) {
                let lane = 0n;
                for (let byteIndex = 0; byteIndex < 8; byteIndex++) {
                  lane |= BigInt(block[laneIndex * 8 + byteIndex]) << BigInt(byteIndex * 8);
                }
                state[laneIndex] ^= lane;
              }
              keccakPermutation(state);
            };
            while (offset + rate <= input.length) {
              absorb(input.subarray(offset, offset + rate));
              offset += rate;
            }
            const tail = new Uint8Array(rate);
            tail.set(input.subarray(offset));
            tail[input.length - offset] = 0x06;
            tail[rate - 1] |= 0x80;
            absorb(tail);
            const output = new Uint8Array(64);
            for (let laneIndex = 0; laneIndex < 8; laneIndex++) {
              for (let byteIndex = 0; byteIndex < 8; byteIndex++) {
                output[laneIndex * 8 + byteIndex] = Number((state[laneIndex] >> BigInt(byteIndex * 8)) & 0xffn);
              }
            }
            return output;
          };
          const target = new Uint8Array(proof.difficulty.length / 2);
          for (let index = 0; index < target.length; index++) {
            target[index] = Number.parseInt(proof.difficulty.slice(index * 2, index * 2 + 2), 16);
          }
          const seedBytes = new TextEncoder().encode(proof.seed);
          const proofConfig = buildPowConfig();
          for (let nonce = 0; nonce < 500000; nonce++) {
            proofConfig[3] = nonce;
            proofConfig[9] = nonce >> 1;
            const encoded = base64Utf8(JSON.stringify(proofConfig));
            const encodedBytes = new TextEncoder().encode(encoded);
            const hashInput = new Uint8Array(seedBytes.length + encodedBytes.length);
            hashInput.set(seedBytes);
            hashInput.set(encodedBytes, seedBytes.length);
            const digest = sha3_512(hashInput);
            let acceptable = true;
            for (let index = 0; index < target.length; index++) {
              if (digest[index] < target[index]) break;
              if (digest[index] > target[index]) {
                acceptable = false;
                break;
              }
            }
            if (acceptable) {
              proofToken = "gAAAAAB" + encoded;
              break;
            }
            if (nonce > 0 && nonce % 2048 === 0) {
              // Chrome clamps nested timers to >=1s in hidden tabs, which stretched the search past the
              // MV3 service-worker lifetime; a MessageChannel yield keeps the page responsive unclamped.
              await new Promise((resolve) => {
                const channel = new MessageChannel();
                channel.port1.onmessage = () => resolve();
                channel.port2.postMessage(0);
              });
            }
          }
          if (!proofToken) {
            return { status: "not-sent", reason: "sentinel_proof_failed" };
          }
        }

        const finalizeResult = await jsonFetch("/backend-api/sentinel/chat-requirements/finalize", {
          method: "POST",
          headers: { ...authHeaders, "Content-Type": "application/json" },
          body: JSON.stringify({
            prepare_token: prepareResult.data.prepare_token,
            proof_token: proofToken,
            turnstile_token: turnstileToken
          })
        });
        const requirementsToken = finalizeResult.data?.token;
        if (!finalizeResult.response.ok || !requirementsToken) {
          return { status: "not-sent", reason: "sentinel_finalize_failed", httpStatus: finalizeResult.response.status };
        }
        const soToken = typeof finalizeResult.data?.so_token === "string" ? finalizeResult.data.so_token : "";

        const body = {
          action: "next",
          conversation_id: conversationId,
          messages: [{
            id: messageId,
            author: { role: "user" },
            content: { content_type: "text", parts: [messageText] },
            metadata: {}
          }],
          model: conversationResult.data?.default_model_slug || "auto",
          parent_message_id: parentMessageId,
          conversation_mode: conversationGizmoId
            ? { kind: "gizmo_interaction", gizmo_id: conversationGizmoId }
            : { kind: "primary_assistant" },
          force_use_sse: true,
          timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC",
          timezone_offset_min: new Date().getTimezoneOffset(),
          websocket_request_id: crypto.randomUUID()
        };

        let postAccepted = false;
        try {
          const sendResponse = await fetch("/backend-api/conversation", {
            method: "POST",
            credentials: "same-origin",
            headers: {
              ...authHeaders,
              Accept: "text/event-stream",
              "Content-Type": "application/json",
              "OpenAI-Sentinel-Chat-Requirements-Token": requirementsToken,
              ...(proofToken ? { "OpenAI-Sentinel-Proof-Token": proofToken } : {}),
              ...(turnstileToken ? { "OpenAI-Sentinel-Turnstile-Token": turnstileToken } : {}),
              ...(soToken ? { "OpenAI-Sentinel-SO-Token": soToken } : {})
            },
            body: JSON.stringify(body)
          });
          if (!sendResponse.ok) {
            let rejectionBody = "";
            try {
              rejectionBody = (await sendResponse.text()).slice(0, 200);
            } catch {
              // server error body is optional triage detail
            }
            return {
              status: "not-sent",
              reason: "conversation_post_rejected",
              httpStatus: sendResponse.status,
              gizmoId: conversationGizmoId,
              message: rejectionBody
            };
          }
          postAccepted = true;
        } catch (error) {
          return {
            status: "uncertain",
            reason: "conversation_post_transport_error",
            message: String(error)
          };
        }

        if (postAccepted) {
          for (let attempt = 0; attempt < 4; attempt++) {
            if (attempt > 0) await new Promise((resolve) => setTimeout(resolve, 250));
            try {
              const verifyResult = await jsonFetch(`/backend-api/conversation/${encodeURIComponent(conversationId)}`, {
                headers: authHeaders
              });
              const mapping = verifyResult.data?.mapping;
              const node = mapping && (mapping[messageId] || Object.values(mapping).find((entry) => entry?.message?.id === messageId));
              const parts = node?.message?.content?.parts;
              if (verifyResult.response.ok && node && (!Array.isArray(parts) || parts.some((part) => typeof part === "string" && part.includes(marker)))) {
                return {
                  status: "submitted-observed",
                  outcome: "submitted-observed",
                  observedMessageId: messageId,
                  evidenceText: `${conversationId}:${messageId}:${marker}`
                };
              }
            } catch {
              // POST was already accepted. Verification failure is uncertainty, not no-send evidence.
            }
          }
        }

        return { status: "uncertain", reason: "server_persistence_not_observed" };
      }
    });
    requestResult = injected?.[0]?.result || { status: "uncertain", reason: "missing_request_result" };
  } catch (error) {
    requestResult = { status: "uncertain", reason: "request_native_execution_failed", message: String(error) };
  }

  const lastDispatchDiagnostic = {
    receiptId,
    status: requestResult.status || "unknown",
    reason: requestResult.reason || null,
    httpStatus: Number.isInteger(requestResult.httpStatus) ? requestResult.httpStatus : null,
    gizmoId: typeof requestResult.gizmoId === "string" && requestResult.gizmoId ? requestResult.gizmoId : null,
    message: typeof requestResult.message === "string" && requestResult.message ? requestResult.message.slice(0, 200) : null
  };
  await chrome.storage.local.set({ lastDispatchDiagnostic });

  const settle = async (outcome, extra = {}) => sendNative({
    op: "settle_fence",
    pairingId: stored.pairingId,
    pairingSecret: stored.pairingSecret,
    profileId,
    receiptId,
    executionId,
    attemptId,
    expectedDeliveryRevision,
    outcome,
    ...extra
  });

  if (requestResult.status === "submitted-observed" && requestResult.observedMessageId) {
    const settleResp = await settle("submitted-observed", {
      observedMessageId: requestResult.observedMessageId,
      transcriptEvidenceHash: await sha256Hex(requestResult.evidenceText || `${originConversationId}:${requestResult.observedMessageId}:${receiptMarker}`)
    });
    if (settleResp?.status === "ok" && settleResp.settlement?.settled) {
      receiptRecord.deliveryStatus = "submitted-observed";
      receiptRecord.observedMessageId = requestResult.observedMessageId;
      receiptRecord.settledAt = Date.now();
      await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
      return {
        status: "ok",
        outcome: "submitted-observed",
        receiptId,
        observedMessageId: requestResult.observedMessageId,
        slotReleased: settleResp.settlement.slot_released
      };
    }
    receiptRecord.deliveryStatus = "dispatching/uncertain";
    await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
    return { status: "uncertain", outcome: "uncertain", receiptId, reason: settleResp?.code || "native_settlement_failed" };
  }

  if (requestResult.status === "not-sent") {
    const settleResp = await settle("not-sent", { details: requestResult.reason || "request_rejected_before_send" });
    if (settleResp?.status === "ok" && settleResp.settlement?.settled) {
      receiptRecord.deliveryStatus = "not-sent";
      await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
      return { status: "not-sent", reason: requestResult.reason, httpStatus: requestResult.httpStatus };
    }
  } else {
    await settle("uncertain", { details: requestResult.reason || "request_native_uncertain" });
  }

  receiptRecord.deliveryStatus = "dispatching/uncertain";
  await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
  return {
    status: "uncertain",
    outcome: "uncertain",
    receiptId,
    reason: requestResult.reason || "native_settlement_failed",
    message: requestResult.message
  };
}

// Transcript probe for a receipt whose attempt never reported back (service-worker or
// tab death). The conversation itself is the authority on whether its message landed,
// so evidence settles the fence instead of the conversation slot staying held forever.
async function probeConversationTranscript(tabId, conversationId, receiptMarker) {
  try {
    const injected = await chrome.scripting.executeScript({
      target: { tabId },
      world: "MAIN",
      args: [conversationId, receiptMarker],
      func: async (conversationIdArg, marker) => {
        const sessionResponse = await fetch("/api/auth/session", { credentials: "same-origin" });
        const sessionData = await sessionResponse.json().catch(() => null);
        const accessToken = sessionData?.accessToken;
        if (!sessionResponse.ok || !accessToken) {
          return { ok: false, reason: "session_access_token_unavailable", httpStatus: sessionResponse.status };
        }
        const response = await fetch(`/backend-api/conversation/${encodeURIComponent(conversationIdArg)}`, {
          credentials: "same-origin",
          headers: { Authorization: `Bearer ${accessToken}` }
        });
        const data = await response.json().catch(() => null);
        if (!response.ok || !data || !data.mapping) {
          return { ok: false, reason: "conversation_unavailable", httpStatus: response.status };
        }
        for (const node of Object.values(data.mapping)) {
          const parts = node?.message?.content?.parts;
          if (Array.isArray(parts) && parts.some((part) => typeof part === "string" && part.includes(marker))) {
            return { ok: true, observedMessageId: node.message.id || null };
          }
        }
        return { ok: true, observedMessageId: null };
      }
    });
    return injected?.[0]?.result || { ok: false, reason: "probe_result_missing" };
  } catch (error) {
    return { ok: false, reason: "probe_execution_failed", message: String(error) };
  }
}

async function resolveStuckDispatch(receiptRecord, stored, profileId) {
  const { receiptId, executionId, originConversationId, activeAttemptId } = receiptRecord;
  if (!activeAttemptId) {
    return { status: "unresolved", reason: "attempt_unknown" };
  }
  const receiptStorageKey = "receipt_" + receiptId;
  const { receiptMarker } = buildContinuationPayload(receiptRecord);
  const originConversationUrl = receiptRecord.originConversationUrl || `https://chatgpt.com/c/${originConversationId}`;

  let tabInfo;
  try {
    tabInfo = await findExistingChatgptExecutionTab(originConversationUrl, originConversationId);
  } catch (err) {
    return { status: "unresolved", reason: "tab_lookup_failed" };
  }
  if (!tabInfo || !tabInfo.tabId) {
    return { status: "unresolved", reason: "tab_unavailable" };
  }

  const probe = await probeConversationTranscript(tabInfo.tabId, originConversationId, receiptMarker);
  if (!probe.ok) {
    // No transcript read means no evidence: the slot stays held.
    return { status: "unresolved", reason: probe.reason };
  }

  if (!probe.observedMessageId) {
    // Absence is only conclusive once a later probe confirms it: an interrupted attempt
    // may still be finishing its request inside the page.
    const firstMissAt = Number(receiptRecord.resolutionProbeAt) || 0;
    if (!firstMissAt) {
      receiptRecord.resolutionProbeAt = Date.now();
      await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
      return { status: "waiting", reason: "transcript_absence_unconfirmed" };
    }
    if (Date.now() - firstMissAt < TRANSCRIPT_ABSENCE_GRACE_MS) {
      return { status: "waiting", reason: "transcript_absence_grace" };
    }
  }

  const outcome = probe.observedMessageId ? "submitted-observed" : "not-sent";
  const settleResp = await sendNative({
    op: "settle_fence",
    pairingId: stored.pairingId,
    pairingSecret: stored.pairingSecret,
    profileId,
    receiptId,
    executionId,
    attemptId: activeAttemptId,
    expectedDeliveryRevision: Math.max(0, Number(receiptRecord.deliveryRevision) || 0),
    outcome,
    ...(probe.observedMessageId
      ? {
        observedMessageId: probe.observedMessageId,
        transcriptEvidenceHash: await sha256Hex(`${originConversationId}:${probe.observedMessageId}:${receiptMarker}`)
      }
      : { details: "recovery_transcript_absent" })
  });

  if (settleResp && settleResp.status === "ok" && settleResp.settlement && settleResp.settlement.settled) {
    receiptRecord.deliveryStatus = outcome;
    receiptRecord.resolutionProbeAt = null;
    if (probe.observedMessageId) {
      receiptRecord.observedMessageId = probe.observedMessageId;
      receiptRecord.settledAt = Date.now();
    }
    await chrome.storage.local.set({ [receiptStorageKey]: receiptRecord });
    return {
      status: "ok",
      outcome,
      receiptId,
      observedMessageId: probe.observedMessageId || null,
      slotReleased: settleResp.settlement.slot_released
    };
  }
  return { status: "unresolved", reason: settleResp?.code || "native_settlement_failed" };
}

async function dispatchSingleReceipt(receiptRecord, stored, profileId) {
  if (!receiptRecord || inFlightDispatches.has(receiptRecord.receiptId)) {
    return { status: "skipped", reason: "dispatch_in_flight" };
  }
  inFlightDispatches.add(receiptRecord.receiptId);
  try {
    return await dispatchSingleReceiptInner(receiptRecord, stored, profileId);
  } finally {
    inFlightDispatches.delete(receiptRecord.receiptId);
  }
}

async function dispatchSingleReceiptInner(receiptRecord, stored, profileId) {
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

  // 1. Reuse an existing ChatGPT tab as the request execution context. Never
  // open a tab automatically: conversation_id is the routing authority.
  let tabInfo;
  try {
    tabInfo = await findExistingChatgptExecutionTab(originConversationUrl, originConversationId);
  } catch (err) {
    return { status: "error", code: "tab_lookup_failed", message: String(err) };
  }
  if (!tabInfo || !tabInfo.tabId) {
    return { status: "waiting", code: "tab_unavailable", message: "Open any ChatGPT tab to enable request-native dispatch" };
  }

  const tabId = tabInfo.tabId;

  if (chrome.scripting?.executeScript) {
    return dispatchReceiptViaChatgptRequest(receiptRecord, stored, profileId, tabId);
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

async function resolveBoundConversationTab(tabId) {
  if (typeof tabId !== "number") {
    return {
      status: "error",
      code: "missing_tab_binding",
      message: "Explicit ChatGPT tab binding (tabId) is required; focused-tab fallback is disabled"
    };
  }

  let targetTab;
  try {
    targetTab = await chrome.tabs.get(tabId);
  } catch (tabErr) {
    return {
      status: "error",
      code: "tab_not_found",
      message: "Bound ChatGPT tab not found: " + tabErr.message
    };
  }

  const tabUrl = targetTab?.url || targetTab?.pendingUrl;
  if (!tabUrl || typeof tabUrl !== "string" || !tabUrl.startsWith("https://chatgpt.com/")) {
    return {
      status: "error",
      code: "invalid_tab_url",
      message: "Bound tab URL must be an exact ChatGPT page (https://chatgpt.com/*); was: " + tabUrl
    };
  }

  const originConversationId = parseCanonicalConversationId(tabUrl);
  if (!originConversationId) {
    return {
      status: "error",
      code: "invalid_conversation_boundary",
      message: "Bound tab is not on a canonical existing ChatGPT conversation (e.g. https://chatgpt.com/c/<id>)"
    };
  }
  const originConversationUrl = tabUrl.split("#")[0].split("?")[0];

  return { status: "ok", originConversationId, originConversationUrl };
}

async function collectConversationEvidence(tabId, originConversationId, originConversationUrl) {
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

  return {
    status: "ok",
    transcriptEvidenceHash: await sha256Hex(evidenceRes.transcriptText),
    accountEvidenceHash: await sha256Hex(evidenceRes.accountText)
  };
}

// Registers exact conversation identity only. No target/workspace, account, draft, or
// transcript state participates in direct-worker routing.
async function registerConversationForTab(tabId) {
  const stored = await chrome.storage.local.get(["pairingId", "pairingSecret", "isPaired"]);
  if (!stored.isPaired || !stored.pairingId || !stored.pairingSecret) {
    return { status: "error", code: "not_paired", message: "Extension is not paired" };
  }

  const conversation = await resolveBoundConversationTab(tabId);
  if (conversation.status !== "ok") return conversation;

  const profileId = await getOrCreateProfileId();

  let response;
  try {
    response = await sendNative({
      op: "register_conversation",
      pairingId: stored.pairingId,
      pairingSecret: stored.pairingSecret,
      profileId,
      originConversationId: conversation.originConversationId,
      originConversationUrl: conversation.originConversationUrl
    });
  } catch (nativeErr) {
    return {
      status: "error",
      code: "native_host_unavailable",
      message: nativeErr.message || String(nativeErr)
    };
  }

  return response || { status: "error", code: "native_host_unavailable" };
}


// SPA conversation changes do not reload the content script, so watch URL changes here. Initial
// document loads are registered by the pageReady signal below.
if (chrome.tabs && chrome.tabs.onUpdated && chrome.tabs.onUpdated.addListener) {
  chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
    if (!changeInfo?.url) return;
    registerConversationForTab(tabId).catch(() => {});
  });
}

// Extension internal message router
chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  // Readiness signal from a live conversation document. The announcement carries no routing
  // data; background re-reads the tab URL and derives the canonical conversation ID itself.
  if (request?.action === "pageReady" && sender?.id === chrome.runtime.id && typeof sender?.tab?.id === "number") {
    registerConversationForTab(sender.tab.id)
      .then(async (result) => {
        if (result?.status !== "ok") {
          console.warn("Return Bridge conversation registration failed:", result?.code || "registration_failed", result?.message || "");
        } else {
          await performRecoveryDrain();
        }
        sendResponse(result || { status: "error", code: "registration_failed" });
      })
      .catch((err) => sendResponse({ status: "error", code: "registration_failed", message: String(err) }));
    return true;
  }

  // Reject messages from web pages or content scripts that are not extension documents
  if (!isTrustedExtensionSender(sender)) {
    sendResponse({ status: "error", code: "unauthorized_sender", message: "Rejected non-extension document message" });
    return false;
  }

  (async () => {
    try {
      const profileId = await getOrCreateProfileId();

      // Gate: Secret-bearing actions must fail-closed if storage isolation is not established
      if (["setup", "status", "connect", "revoke", "launch", "recover", "drain", "dispatchReceipt", "ensureLocalMode"].includes(request.action)) {
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

          // Seed local-mode credentials only when browser storage holds NO pairing state at all.
          // Any pre-existing pairing material - a real bootstrap pairing, or a partial/legacy set -
          // is preserved verbatim: replacing it would rebind this profile to the local pairing and
          // strand the superseded pairing's pending receipts and unresolved fences with no undo.
          const storedPairing = await chrome.storage.local.get([
            "isPaired",
            "pairingId",
            "pairingSecret",
            "profileId",
            "policyRevision"
          ]);
          const hasPairingMaterial =
            storedPairing.isPaired === true ||
            (typeof storedPairing.pairingId === "string" && storedPairing.pairingId.length > 0) ||
            (typeof storedPairing.pairingSecret === "string" && storedPairing.pairingSecret.length > 0);
          if (hasPairingMaterial) {
            // Sync native targets/status only; pairing identity and receipt/fence records stay untouched.
            await chrome.storage.local.set({ targets });
            sendResponse({
              status: "ok",
              isPaired: storedPairing.isPaired === true,
              pairingStatus: response.pairingStatus || "active",
              taskExecutionAvailable: response.taskExecutionAvailable !== false,
              targetsCount: targets.length,
              targets
            });
            break;
          }

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

          const tabId = request.tabId;
          const conversation = await resolveBoundConversationTab(tabId);
          if (conversation.status !== "ok") {
            sendResponse(conversation);
            return;
          }
          const { originConversationId, originConversationUrl } = conversation;

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
                ? "No workspace targets registered. Add a target via hands-bridge target add."
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
          const evidence = await collectConversationEvidence(tabId, originConversationId, originConversationUrl);
          if (evidence.status !== "ok") return evidence;
          const { transcriptEvidenceHash, accountEvidenceHash } = evidence;
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
            if (k.startsWith("receipt_") && v && (v.deliveryStatus === "received" || v.deliveryStatus === "not-sent" || v.deliveryStatus === "dispatching/uncertain")) {
              pending.push(v);
            }
          }
          sendResponse({
            status: "ok",
            pendingReceipts: pending,
            lastDispatchDiagnostic: allData.lastDispatchDiagnostic || null
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
