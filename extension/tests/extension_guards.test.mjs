import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import assert from "node:assert/strict";

const BACKGROUND_JS_PATH = path.resolve("extension/background.js");
const CONTENT_SCRIPT_JS_PATH = path.resolve("extension/content_script.js");
const OPTIONS_JS_PATH = path.resolve("extension/options.js");
const backgroundCode = fs.readFileSync(BACKGROUND_JS_PATH, "utf8");
const contentScriptCode = fs.readFileSync(CONTENT_SCRIPT_JS_PATH, "utf8");
const optionsCode = fs.readFileSync(OPTIONS_JS_PATH, "utf8");

function createTestHarness({
  extensionId = "mkkajdpmlmliildflmnnmfndboldnnfa",
  failSetAccessLevel = false,
  missingSetAccessLevel = false,
  failStorageSet = false,
  nativeResponse = { status: "ok", pairingId: "pair_123", pairingSecret: "rb_sec_456" },
  nativeError = null,
  initialStorage = {},
  mockTabs = {},
  mockTabMessages = {}
} = {}) {
  const storageStore = { ...initialStorage };
  let capturedListener = null;
  const nativeMessagesSent = [];

  const mockChrome = {
    runtime: {
      id: extensionId,
      lastError: null,
      onInstalled: {
        addListener() {}
      },
      onMessage: {
        addListener(fn) {
          capturedListener = fn;
        }
      },
      sendNativeMessage(host, msg, cb) {
        nativeMessagesSent.push({ host, msg });
        if (nativeError) {
          mockChrome.runtime.lastError = new Error(nativeError);
          cb(null);
          mockChrome.runtime.lastError = null;
          return;
        }
        cb(nativeResponse);
      }
    },
    tabs: {
      async get(tabId) {
        if (mockTabs && mockTabs[tabId]) {
          return mockTabs[tabId];
        }
        throw new Error("Tab not found: " + tabId);
      },
      lastSendMessageOptions: null,
      sendMessage(tabId, message, optionsOrCb, maybeCb) {
        const options = typeof optionsOrCb === "object" ? optionsOrCb : null;
        const cb = typeof optionsOrCb === "function" ? optionsOrCb : maybeCb;
        mockChrome.tabs.lastSendMessageOptions = options;
        if (mockTabMessages && mockTabMessages[tabId]) {
          const resp = mockTabMessages[tabId](message, options);
          if (cb) cb(resp);
          return Promise.resolve(resp);
        }
        const resp = {
          ok: true,
          originConversationId: "c_test_123",
          originConversationUrl: "https://chatgpt.com/c/c_test_123",
          transcriptText: "Turn 1: hello world\nTurn 2: how are you",
          accountText: "Personal Workspace (user@example.com)"
        };
        if (cb) cb(resp);
        return Promise.resolve(resp);
      }
    },
    storage: {
      local: {
        async get(keys) {
          if (!keys) return { ...storageStore };
          if (Array.isArray(keys)) {
            const res = {};
            for (const k of keys) {
              if (k in storageStore) res[k] = storageStore[k];
            }
            return res;
          }
          return { [keys]: storageStore[keys] };
        },
        async set(items) {
          if (failStorageSet) {
            throw new Error("Simulated storage.set failure");
          }
          Object.assign(storageStore, items);
        },
        async remove(keys) {
          const arr = Array.isArray(keys) ? keys : [keys];
          for (const k of arr) {
            delete storageStore[k];
          }
        }
      }
    }
  };

  if (!missingSetAccessLevel) {
    mockChrome.storage.local.setAccessLevel = async () => {
      if (failSetAccessLevel) {
        throw new Error("Simulated storage setAccessLevel failure");
      }
    };
  }

  const context = vm.createContext({
    chrome: mockChrome,
    crypto: globalThis.crypto,
    console: {
      log() {},
      warn() {},
      error() {}
    },
    TextEncoder: globalThis.TextEncoder,
    Uint8Array: globalThis.Uint8Array,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    Promise: globalThis.Promise
  });

  // Execute the REAL background.js in VM
  vm.runInContext(backgroundCode, context);

  assert.ok(capturedListener, "background.js must register an onMessage listener");

  async function sendMessage(request, sender) {
    return new Promise((resolve) => {
      let resolved = false;
      capturedListener(request, sender, (response) => {
        resolved = true;
        resolve(response);
      });
      // Synchronous rejection branch
      queueMicrotask(() => {
        if (!resolved) {
          // If sendResponse was not called asynchronously
        }
      });
    });
  }

  return {
    extensionId,
    sendMessage,
    storageStore,
    nativeMessagesSent,
    mockChrome
  };
}

async function runTests() {
  console.log("Running real background.js harness tests...");

  // ---------------------------------------------------------------------------
  // Test 1: Untrusted web-page sender rejection (Confused-deputy protection)
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness();
    const untrustedSenders = [
      // Content script on chatgpt.com sharing the extension ID
      { id: harness.extensionId, url: "https://chatgpt.com/c/session1" },
      // Arbitrary web page sender sharing the extension ID
      { id: harness.extensionId, url: "https://malicious.example.com/exploit.html" },
      // External extension sender
      { id: "foreign_extension_id_abcdef", url: "chrome-extension://foreign_extension_id_abcdef/options.html" },
      // Missing or non-string URL
      { id: harness.extensionId, url: null },
      { id: harness.extensionId, url: undefined },
      // Empty or missing sender
      null,
      undefined
    ];

    for (const sender of untrustedSenders) {
      const res = await harness.sendMessage({ action: "getState" }, sender);
      assert.equal(res?.status, "error");
      assert.equal(res?.code, "unauthorized_sender");
    }

    assert.equal(
      harness.nativeMessagesSent.length,
      0,
      "No native messaging must occur for untrusted senders"
    );
    console.log("  [PASS] Untrusted web-page senders rejected (confused-deputy protection)");
  }

  // ---------------------------------------------------------------------------
  // Test 2: Trusted extension-document acceptance
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness();
    const optionsSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/options.html`
    };
    const testRunnerSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/test_runner.html`
    };

    const state1 = await harness.sendMessage({ action: "getState" }, optionsSender);
    assert.equal(state1.status, "ok");
    assert.equal(state1.storageAccessLevel, "TRUSTED_CONTEXTS");
    assert.ok(state1.profileId.startsWith("prof_"));

    const state2 = await harness.sendMessage({ action: "getState" }, testRunnerSender);
    assert.equal(state2.status, "ok");
    assert.equal(state2.storageAccessLevel, "TRUSTED_CONTEXTS");

    console.log("  [PASS] Trusted extension documents accepted");
  }

  // ---------------------------------------------------------------------------
  // Test 3: Storage-isolation failure blocks secret-bearing actions fail closed
  //         before credential storage or native messaging
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      failSetAccessLevel: true,
      initialStorage: {
        isPaired: true,
        pairingId: "pair_existing",
        pairingSecret: "rb_sec_existing"
      }
    });

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/options.html`
    };

    // A. Setup must reject without native messaging or saving credentials
    const setupRes = await harness.sendMessage(
      { action: "setup", bootstrapToken: "rb_boot_secret_token" },
      trustedSender
    );
    assert.equal(setupRes.status, "error");
    assert.equal(setupRes.code, "storage_isolation_unavailable");
    assert.equal(harness.nativeMessagesSent.length, 0, "Native messaging must NOT be invoked when storage isolation fails");

    // B. Status must reject without native messaging
    const statusRes = await harness.sendMessage({ action: "status" }, trustedSender);
    assert.equal(statusRes.status, "error");
    assert.equal(statusRes.code, "storage_isolation_unavailable");
    assert.equal(harness.nativeMessagesSent.length, 0);

    // C. Connect must reject without native messaging
    const connectRes = await harness.sendMessage({ action: "connect" }, trustedSender);
    assert.equal(connectRes.status, "error");
    assert.equal(connectRes.code, "storage_isolation_unavailable");
    assert.equal(harness.nativeMessagesSent.length, 0);

    // D. Revoke must reject without native messaging
    const revokeRes = await harness.sendMessage({ action: "revoke" }, trustedSender);
    assert.equal(revokeRes.status, "error");
    assert.equal(revokeRes.code, "storage_isolation_unavailable");
    assert.equal(harness.nativeMessagesSent.length, 0);

    console.log("  [PASS] Storage-isolation failure blocks setup/status/connect/revoke before credential storage/native messaging");
  }

  // ---------------------------------------------------------------------------
  // Test 4: getState reports untrusted and unpaired when isolation fails
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      failSetAccessLevel: true,
      initialStorage: {
        isPaired: true,
        pairingId: "pair_existing",
        pairingSecret: "rb_sec_existing"
      }
    });

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/options.html`
    };

    const state = await harness.sendMessage({ action: "getState" }, trustedSender);
    assert.equal(state.status, "ok");
    assert.equal(state.storageAccessLevel, "UNTRUSTED");
    assert.equal(state.isPaired, false, "Must report unpaired when storage isolation is untrusted");
    assert.equal(state.pairingId, null);

    console.log("  [PASS] getState reports untrusted/unpaired when storage isolation fails");
  }
  // ---------------------------------------------------------------------------
  // Test 5: Launch rejects untrusted senders (e.g. content script or web page)
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" }
    });
    const contentScriptSender = {
      id: harness.extensionId,
      url: "https://chatgpt.com/c/c_test_123"
    };
    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Do something"
    }, contentScriptSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "unauthorized_sender");
    assert.equal(harness.nativeMessagesSent.length, 0, "Content script MUST NOT trigger native launch");
    console.log("  [PASS] Launch rejects content-script/web-page senders (confused-deputy guard)");
  }

  // ---------------------------------------------------------------------------
  // Test 6: Launch requires explicit tabId; fails closed without focused-tab fallback
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // Missing tabId
    const resMissing = await harness.sendMessage({
      action: "launch",
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);

    assert.equal(resMissing.status, "error");
    assert.equal(resMissing.code, "missing_tab_binding");
    assert.equal(harness.nativeMessagesSent.length, 0);

    // Non-number tabId
    const resBadType = await harness.sendMessage({
      action: "launch",
      tabId: "active",
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resBadType.status, "error");
    assert.equal(resBadType.code, "missing_tab_binding");

    console.log("  [PASS] Launch requires explicit tabId (no focused-tab fallback)");
  }

  // ---------------------------------------------------------------------------
  // Test 7: Launch rejects non-ChatGPT tab URL and non-canonical conversation URL
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" },
      mockTabs: {
        101: { id: 101, url: "https://example.com/not-chatgpt" },
        102: { id: 102, url: "https://chatgpt.com/" },
        103: { id: 103, url: "https://chatgpt.com/chat" },
        104: { id: 104, url: "https://chatgpt.com/c/new_chat" },
      }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // Tab on non-ChatGPT URL
    const resNonChat = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resNonChat.status, "error");
    assert.equal(resNonChat.code, "invalid_tab_url");

    // Tab on root ChatGPT page (no canonical conversation)
    const resRoot = await harness.sendMessage({
      action: "launch",
      tabId: 102,
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resRoot.status, "error");
    assert.equal(resRoot.code, "invalid_conversation_boundary");

    // Tab on /chat
    const resChat = await harness.sendMessage({
      action: "launch",
      tabId: 103,
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resChat.status, "error");
    assert.equal(resChat.code, "invalid_conversation_boundary");

    assert.equal(harness.nativeMessagesSent.length, 0);
    console.log("  [PASS] Launch rejects non-ChatGPT URLs and non-canonical conversation boundaries");
  }

  // ---------------------------------------------------------------------------
  // Test 8: Content script evidence errors and conversation ID/URL mismatches
  // ---------------------------------------------------------------------------
  {
    const trustedSender = {
      id: "mkkajdpmlmliildflmnnmfndboldnnfa",
      url: "chrome-extension://mkkajdpmlmliildflmnnmfndboldnnfa/popup.html"
    };

    // Subtest A: Missing rendered transcript
    const harnessA = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } },
      mockTabMessages: {
        101: () => ({ ok: false, error: "missing_rendered_transcript", message: "No rendered conversation transcript turns found in page" })
      }
    });
    const resA = await harnessA.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resA.status, "error");
    assert.equal(resA.code, "missing_rendered_transcript");
    assert.equal(harnessA.nativeMessagesSent.length, 0);

    // Subtest B: Missing account context
    const harnessB = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } },
      mockTabMessages: {
        101: () => ({ ok: false, error: "missing_account_context", message: "No account or workspace context evidence found in page" })
      }
    });
    const resB = await harnessB.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resB.status, "error");
    assert.equal(resB.code, "missing_account_context");
    assert.equal(harnessB.nativeMessagesSent.length, 0);

    // Subtest C: Conversation ID / URL mismatch between tab and content script
    const harnessC = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } },
      mockTabMessages: {
        101: () => ({
          ok: true,
          originConversationId: "c_other_999",
          originConversationUrl: "https://chatgpt.com/c/c_other_999",
          transcriptText: "real transcript",
          accountText: "real user"
        })
      }
    });
    const resC = await harnessC.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resC.status, "error");
    assert.equal(resC.code, "conversation_binding_mismatch");
    assert.equal(harnessC.nativeMessagesSent.length, 0);

    console.log("  [PASS] Content script evidence errors and conversation binding mismatches fail closed");
  }

  // ---------------------------------------------------------------------------
  // Test 9: Browser storage failure aborts native messaging call
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      failStorageSet: true,
      initialStorage: { profileId: "prof_launch", isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "browser_persistence_failure");
    assert.equal(harness.nativeMessagesSent.length, 0, "Zero native message must be sent on storage failure");
    console.log("  [PASS] Browser storage failure prevents native messaging call");
  }

  // ---------------------------------------------------------------------------
  // Test 10: Full valid launch collects evidence, hashes inside background, and sends only hashes
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      nativeResponse: { status: "ok", executionId: "exec_full_1", returnToken: "ret_full_1", state: "started" },
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } },
      mockTabMessages: {
        101: () => ({
          ok: true,
          originConversationId: "c_test_123",
          originConversationUrl: "https://chatgpt.com/c/c_test_123",
          transcriptText: "Line 1: implement review remediation\nLine 2: done",
          accountText: "OpenCode Workspace (user@example.com)"
        })
      }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_hands",
      promptText: "Fix bug in parser"
    }, trustedSender);

    assert.equal(res.status, "ok");
    assert.equal(harness.nativeMessagesSent.length, 1);

    const sent = harness.nativeMessagesSent[0].msg;
    assert.equal(sent.op, "launch");
    assert.equal(sent.originConversationId, "c_test_123");
    assert.equal(sent.originConversationUrl, "https://chatgpt.com/c/c_test_123");
    // Must contain computed SHA-256 hashes (64 hex characters)
    assert.match(sent.transcriptEvidenceHash, /^[a-f0-9]{64}$/);
    assert.match(sent.accountEvidenceHash, /^[a-f0-9]{64}$/);
    assert.equal(sent.targetId, "target_hands");
    assert.equal(sent.promptText, "Fix bug in parser");

    // Stored launch record must be persisted with status started
    const storedRecord = harness.storageStore["launch_" + sent.launchRequestId];
    assert.ok(storedRecord);
    assert.equal(storedRecord.status, "started");
    assert.equal(storedRecord.executionId, "exec_full_1");
    assert.equal(storedRecord.transcriptEvidenceHash, sent.transcriptEvidenceHash);

    console.log("  [PASS] Full launch collects evidence from content script, hashes in background, and persists before native call");
  }

  // ---------------------------------------------------------------------------
  // Test 11: Lost native response boundary sets local native-response-uncertain
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      nativeError: "Native messaging host disconnected unexpectedly (exit code 1)",
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_hands",
      promptText: "Crash probe"
    }, trustedSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "native_response_uncertain");
    assert.equal(res.state, "native-response-uncertain");
    assert.ok(res.launchRequestId);

    // Storage record must be marked 'native-response-uncertain', NOT 'pending_native'
    const storedRecord = harness.storageStore["launch_" + res.launchRequestId];
    assert.ok(storedRecord);
    assert.equal(storedRecord.status, "native-response-uncertain");
    assert.ok(storedRecord.lastError.includes("Native messaging host disconnected"));

    console.log("  [PASS] Lost native response boundary caught explicitly as native-response-uncertain");
  }

  // ---------------------------------------------------------------------------
  // Test 12: Recovery updates native-response-uncertain record from native summary
  // ---------------------------------------------------------------------------
  {
    const launchReqId = "req_recovering_1";
    const harness = createTestHarness({
      nativeResponse: {
        status: "ok",
        summaries: [
          {
            launch_request_id: launchReqId,
            execution_id: "exec_rec_1",
            state: "started",
            orca_terminal_handle: "term_rec_1"
          }
        ]
      },
      initialStorage: {
        isPaired: true,
        pairingId: "pair_launch",
        pairingSecret: "rb_sec_launch",
        policyRevision: "v1",
        lastLaunchRequestId: launchReqId,
        ["launch_" + launchReqId]: {
          launchRequestId: launchReqId,
          status: "native-response-uncertain"
        }
      }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({ action: "recover" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.summaries.length, 1);

    // Stored record must now be updated from native-response-uncertain to started!
    const updated = harness.storageStore["launch_" + launchReqId];
    assert.equal(updated.status, "started");
    assert.equal(updated.executionId, "exec_rec_1");
    assert.equal(updated.terminalEvidence.orcaTerminalHandle, "term_rec_1");

    console.log("  [PASS] Recovery reconciles native-response-uncertain records against native summary");
  }

  // ---------------------------------------------------------------------------
  // Test 13: Stale activeKey handling: resolved prior task does not trap new task
  // ---------------------------------------------------------------------------
  {
    const priorReqId = "req_prior_resolved";
    const activeKey = "active_launch_pair_launch_c_test_123_target_hands_v1";
    const harness = createTestHarness({
      nativeResponse: { status: "ok", executionId: "exec_new_task", returnToken: "ret_new", state: "started" },
      initialStorage: {
        isPaired: true,
        pairingId: "pair_launch",
        pairingSecret: "rb_sec_launch",
        policyRevision: "v1",
        [activeKey]: priorReqId,
        ["launch_" + priorReqId]: {
          launchRequestId: priorReqId,
          originConversationId: "c_test_123",
          targetId: "target_hands",
          requestedPolicyRevision: "v1",
          promptText: "Old prior prompt",
          status: "started" // Resolved!
        }
      },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // Send new prompt for same conversation/target: must NOT be trapped by stale activeKey!
    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_hands",
      promptText: "Brand new second task"
    }, trustedSender);

    assert.equal(res.status, "ok");
    assert.equal(harness.nativeMessagesSent.length, 1);
    const newSent = harness.nativeMessagesSent[0].msg;
    assert.notEqual(newSent.launchRequestId, priorReqId, "Must allocate a fresh request ID for subsequent task");
    assert.equal(newSent.promptText, "Brand new second task");

    console.log("  [PASS] Stale activeKey from resolved task does not trap subsequent new task");
  }

  // ---------------------------------------------------------------------------
  // Test 14: Uncertain/unresolved task blocks new launch (does not silently mint ID)
  // ---------------------------------------------------------------------------
  {
    const uncertainReqId = "req_prior_uncertain";
    const activeKey = "active_launch_pair_launch_c_test_123_target_hands_v1";
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_launch",
        pairingSecret: "rb_sec_launch",
        policyRevision: "v1",
        [activeKey]: uncertainReqId,
        ["launch_" + uncertainReqId]: {
          launchRequestId: uncertainReqId,
          executionId: "exec_uncertain_1",
          originConversationId: "c_test_123",
          originConversationUrl: "https://chatgpt.com/c/c_test_123",
          transcriptEvidenceHash: "hash_t_old",
          accountEvidenceHash: "hash_a_old",
          targetId: "target_hands",
          requestedPolicyRevision: "v1",
          promptText: "Prior in-flight prompt",
          status: "unknown" // Unresolved!
        }
      },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // Attempt to start a different task while prior task is unresolved
    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_hands",
      promptText: "Different task while prior task is uncertain"
    }, trustedSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "active_launch_unresolved");
    assert.equal(res.launchRequestId, uncertainReqId);
    assert.equal(harness.nativeMessagesSent.length, 0, "Must not dispatch native call while prior task unresolved");

    console.log("  [PASS] Unresolved task blocks new task and prevents silent ID minting");
  }

  // ---------------------------------------------------------------------------
  // Test 15: Replaying same request ID with conflicting payload returns payload_conflict
  // ---------------------------------------------------------------------------
  {
    const explicitReqId = "req_explicit_1";
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_launch",
        pairingSecret: "rb_sec_launch",
        policyRevision: "v1",
        ["launch_" + explicitReqId]: {
          launchRequestId: explicitReqId,
          originConversationId: "c_test_123",
          originConversationUrl: "https://chatgpt.com/c/c_test_123",
          targetId: "target_hands",
          requestedPolicyRevision: "v1",
          promptText: "Original prompt",
          status: "started"
        }
      },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({
      action: "launch",
      launchRequestId: explicitReqId,
      tabId: 101,
      targetId: "target_hands",
      promptText: "Changed conflicting prompt"
    }, trustedSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "payload_conflict");
    assert.equal(harness.nativeMessagesSent.length, 0);

    console.log("  [PASS] Replay with changed payload fails closed with payload_conflict");
  }

  // ---------------------------------------------------------------------------
  // Test 16: Content script direct unit tests (Removal of fake fallbacks)
  // ---------------------------------------------------------------------------
  {
    function runContentScriptInVm({
      pathname = "/c/c_123",
      turns = [],
      conversationTurns = null,
      roleTurns = null,
      userMenu = null,
      accountCandidates = null
    } = {}) {
      let capturedListener = null;
      const mockChrome = {
        runtime: {
          onMessage: {
            addListener(fn) {
              capturedListener = fn;
            }
          }
        }
      };
      const mockDocument = {
        querySelectorAll(selector) {
          if (selector.includes('conversation-turn')) {
            return (conversationTurns || []).map(t => ({ innerText: t }));
          }
          if (selector === "article") {
            return turns.map(t => ({ innerText: t }));
          }
          if (selector === "[data-message-author-role]") {
            return (roleTurns || []).map(t => ({ innerText: t }));
          }
          if (selector.includes("user-profile") || selector.includes("workspace") || selector.includes("user-menu")) {
            const candidates = accountCandidates || (userMenu ? [userMenu] : []);
            return candidates.map(t => ({ innerText: t }));
          }
          return [];
        },
      };
      const mockWindow = {
        location: {
          href: "https://chatgpt.com" + pathname,
          pathname
        }
      };

      const ctx = vm.createContext({
        chrome: mockChrome,
        document: mockDocument,
        window: mockWindow,
        console: { log() {}, error() {} }
      });
      vm.runInContext(contentScriptCode, ctx);

      let response = null;
      capturedListener({ action: "collect_page_evidence" }, {}, (r) => { response = r; });
      return response;
    }

    // Case A: Full valid page
    const validRes = runContentScriptInVm({
      pathname: "/c/c_123",
      turns: ["Turn 1 text", "Turn 2 text"],
      userMenu: "Workspace Personal"
    });
    assert.equal(validRes.ok, true);
    assert.equal(validRes.originConversationId, "c_123");
    assert.equal(validRes.originConversationUrl, "https://chatgpt.com/c/c_123");
    assert.ok(validRes.transcriptText.includes("Turn 1 text"));
    assert.equal(validRes.accountText, "Workspace Personal");

    // Case B: No turns rendered -> MUST fail closed with missing_rendered_transcript (NO fake fallback!)
    const noTurnsRes = runContentScriptInVm({
      pathname: "/c/c_123",
      turns: [],
      userMenu: "Workspace Personal"
    });
    assert.equal(noTurnsRes.ok, false);
    assert.equal(noTurnsRes.error, "missing_rendered_transcript");

    // Case C: No account/workspace context -> MUST fail closed with missing_account_context (NO fake fallback!)
    const noAccountRes = runContentScriptInVm({
      pathname: "/c/c_123",
      turns: ["Turn 1 text"],
      userMenu: null
    });
    assert.equal(noAccountRes.ok, false);
    assert.equal(noAccountRes.error, "missing_account_context");

    // Case D: Non-canonical conversation pathname (/c/new_chat or /)
    const badPathRes = runContentScriptInVm({
      pathname: "/c/new_chat",
      turns: ["Turn 1"],
      userMenu: "User"
    });
    assert.equal(badPathRes.ok, false);
    assert.equal(badPathRes.error, "invalid_conversation_boundary");

    // Case E: prefer one turn-node tier so wrapper + nested author nodes are not duplicated.
    const noDuplicateRes = runContentScriptInVm({
      pathname: "/c/c_123",
      conversationTurns: ["Canonical turn one", "Canonical turn two"],
      turns: ["Wrapper duplicate one", "Wrapper duplicate two"],
      roleTurns: ["Nested duplicate one", "Nested duplicate two"],
      userMenu: "Workspace Personal"
    });
    assert.equal(noDuplicateRes.ok, true);
    assert.ok(noDuplicateRes.transcriptText.includes("Canonical turn one"));
    assert.equal(noDuplicateRes.transcriptText.includes("Wrapper duplicate one"), false);
    assert.equal(noDuplicateRes.transcriptText.includes("Nested duplicate one"), false);

    // Case F: ignore empty account candidates and use a later rendered value.
    const laterAccountRes = runContentScriptInVm({
      pathname: "/c/c_123",
      turns: ["Turn 1"],
      accountCandidates: ["   ", "Workspace Later Candidate"]
    });
    assert.equal(laterAccountRes.ok, true);
    assert.equal(laterAccountRes.accountText, "Workspace Later Candidate");

    console.log("  [PASS] Content script evidence avoids duplicate turns and scans account candidates fail-closed");
  }

  // Test 17: Verify frameId: 0 explicit targeting during evidence collection
  {
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_123",
        pairingSecret: "rb_sec_456",
        policyRevision: "v1"
      },
      mockTabs: {
        101: { id: 101, url: "https://chatgpt.com/c/c_test_123" }
      }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Do frame test",
      launchRequestId: "req_frame_test"
    }, trustedSender);
    assert.equal(harness.mockChrome.tabs.lastSendMessageOptions?.frameId, 0);
    console.log("  [PASS] Top-frame evidence collection targets frameId 0 explicitly");
  }

  // Test 18: Native definitive rejection transitions record to rejected and clears activeKey
  {
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_123",
        pairingSecret: "rb_sec_456",
        policyRevision: "v1"
      },
      mockTabs: {
        101: { id: 101, url: "https://chatgpt.com/c/c_test_123" }
      },
      nativeResponse: {
        status: "error",
        code: "target_not_found",
        message: "Target workspace not found"
      }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_nonexistent",
      promptText: "Test definitive rejection",
      launchRequestId: "req_rej_test"
    }, trustedSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "target_not_found");

    // Verify record in storage has status = "rejected"
    const stored = harness.storageStore["launch_req_rej_test"];
    assert.ok(stored, "Record should exist in storage for audit");
    assert.equal(stored.status, "rejected");
    assert.equal(stored.rejectCode, "target_not_found");
    assert.equal(stored.rejectMessage, "Target workspace not found");

    // Verify activeKey was removed
    const activeKey = "active_launch_pair_123_c_test_123_target_nonexistent_v1";
    assert.equal(harness.storageStore[activeKey], undefined, "activeKey must be cleared after definitive rejection");
    console.log("  [PASS] Native definitive rejection transitions record to rejected and clears activeKey");
  }

  // Test 19: Deliberate new task receives fresh launchRequestId even with identical payload after resolution
  {
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_123",
        pairingSecret: "rb_sec_456",
        policyRevision: "v1"
      },
      mockTabs: {
        101: { id: 101, url: "https://chatgpt.com/c/c_test_123" }
      },
      nativeResponse: {
        status: "ok",
        executionId: "exec_1",
        returnToken: "tok_1",
        state: "started"
      }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    // First task without explicit launchRequestId
    const res1 = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Identical prompt text"
    }, trustedSender);
    assert.equal(res1.status, "ok");
    const firstReqId = harness.nativeMessagesSent[0].msg.launchRequestId;
    assert.ok(firstReqId.startsWith("req_"));

    // Subsequent task without explicit launchRequestId with the SAME prompt text
    const res2 = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_test",
      promptText: "Identical prompt text"
    }, trustedSender);
    assert.equal(res2.status, "ok");
    const secondReqId = harness.nativeMessagesSent[1].msg.launchRequestId;
    assert.ok(secondReqId.startsWith("req_"));
    assert.notEqual(firstReqId, secondReqId, "Subsequent deliberate new task must receive fresh unique launchRequestId");
    console.log("  [PASS] Resolved prior task does not trap new task with identical payload");
  }

  // Test 20: status preserves native taskExecutionAvailable.
  {
    const harness = createTestHarness({
      nativeResponse: {
        status: "ok",
        pairingStatus: "active",
        targetsCount: 1,
        policyRevision: "v1",
        taskExecutionAvailable: true
      },
      initialStorage: {
        isPaired: true,
        pairingId: "pair_status",
        pairingSecret: "rb_sec_status"
      }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const res = await harness.sendMessage({ action: "status" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.taskExecutionAvailable, true);
    console.log("  [PASS] Status preserves native taskExecutionAvailable");
  }

  // Test 21: prompt bound is UTF-8 bytes, not UTF-16/JS character count.
  {
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_utf8",
        pairingSecret: "rb_sec_utf8",
        policyRevision: "v1"
      },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const multibytePrompt = "界".repeat(Math.floor((128 * 1024) / 3) + 1);
    assert.ok(multibytePrompt.length < 128 * 1024);
    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_utf8",
      promptText: multibytePrompt
    }, trustedSender);
    assert.equal(res.status, "error");
    assert.equal(res.code, "prompt_too_large");
    assert.equal(harness.nativeMessagesSent.length, 0);
    console.log("  [PASS] Prompt limit is enforced in UTF-8 bytes");
  }

  // Test 22: explicit request ID cannot bypass another unresolved active request.
  {
    const activeReqId = "req_active_unresolved";
    const activeKey = "active_launch_pair_explicit_c_test_123_target_hands_v1";
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_explicit",
        pairingSecret: "rb_sec_explicit",
        policyRevision: "v1",
        [activeKey]: activeReqId,
        ["launch_" + activeReqId]: {
          launchRequestId: activeReqId,
          originConversationId: "c_test_123",
          originConversationUrl: "https://chatgpt.com/c/c_test_123",
          transcriptEvidenceHash: "ignored-for-id-mismatch",
          accountEvidenceHash: "ignored-for-id-mismatch",
          targetId: "target_hands",
          requestedPolicyRevision: "v1",
          promptText: "Prior task",
          status: "unknown"
        }
      },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const res = await harness.sendMessage({
      action: "launch",
      launchRequestId: "req_different_explicit",
      tabId: 101,
      targetId: "target_hands",
      promptText: "New task"
    }, trustedSender);
    assert.equal(res.status, "error");
    assert.equal(res.code, "active_launch_unresolved");
    assert.equal(res.launchRequestId, activeReqId);
    assert.equal(harness.nativeMessagesSent.length, 0);
    console.log("  [PASS] Explicit launchRequestId cannot bypass unresolved active request");
  }

  // Test 23: native replay in claimed state is unresolved, never a successful launch.
  {
    const harness = createTestHarness({
      nativeResponse: {
        status: "ok",
        executionId: "exec_claimed",
        returnToken: "ret_claimed",
        state: "claimed",
        isReplayed: true
      },
      initialStorage: {
        isPaired: true,
        pairingId: "pair_claimed",
        pairingSecret: "rb_sec_claimed",
        policyRevision: "v1"
      },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const res = await harness.sendMessage({
      action: "launch",
      tabId: 101,
      targetId: "target_claimed",
      promptText: "Claimed replay"
    }, trustedSender);
    assert.equal(res.status, "error");
    assert.equal(res.code, "launch_unresolved");
    assert.equal(res.state, "claimed");
    assert.equal(harness.storageStore.activeExecutionId, undefined);
    console.log("  [PASS] Claimed replay cannot masquerade as successful started launch");
  }

  // Test 24: recover reconciles all browser-side uncertain states.
  for (const browserState of ["pending_native", "unknown", "native-response-uncertain"]) {
    const reqId = `req_recover_${browserState.replaceAll("-", "_")}`;
    const harness = createTestHarness({
      nativeResponse: {
        status: "ok",
        summaries: [{
          launch_request_id: reqId,
          execution_id: "exec_recovered",
          state: "started",
          orca_terminal_handle: "term_recovered"
        }]
      },
      initialStorage: {
        isPaired: true,
        pairingId: "pair_recover_states",
        pairingSecret: "rb_sec_recover_states",
        ["launch_" + reqId]: { launchRequestId: reqId, status: browserState }
      }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const res = await harness.sendMessage({ action: "recover" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(harness.storageStore["launch_" + reqId].status, "started");
    assert.equal(harness.storageStore["launch_" + reqId].executionId, "exec_recovered");
  }
  console.log("  [PASS] Recovery reconciles pending_native/unknown/native-response-uncertain states");

  // Setup guidance must quote the workspace placeholder so paths with spaces are safe when pasted.
  assert.ok(optionsCode.includes('--target "<path>"'));
  assert.equal(optionsCode.includes("--target <path>"), false);
  console.log("  [PASS] Options setup command quotes the target path placeholder");

  // Non-Windows setup guidance must skip Windows Registry registration explicitly.
  assert.ok(optionsCode.includes("chrome.runtime.getPlatformInfo()"));
  assert.ok(optionsCode.includes('platformInfo?.os === "win"'));
  assert.ok(optionsCode.includes('" --skip-registry"'));
  console.log("  [PASS] Options setup command is platform-aware for registry registration");

  // Test 25: concurrent identical launches coalesce to one native request.
  {
    const harness = createTestHarness({
      nativeResponse: { status: "ok", executionId: "exec_coalesced", returnToken: "ret_coalesced", state: "started" },
      initialStorage: {
        profileId: "prof_fixed",
        isPaired: true,
        pairingId: "pair_coalesced",
        pairingSecret: "rb_sec_coalesced",
        policyRevision: "v1"
      },
      mockTabs: { 101: { id: 101, url: "https://chatgpt.com/c/c_test_123" } }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const request = {
      action: "launch",
      tabId: 101,
      targetId: "target_coalesced",
      promptText: "Exactly one native launch"
    };
    const [first, second] = await Promise.all([
      harness.sendMessage({ ...request }, trustedSender),
      harness.sendMessage({ ...request }, trustedSender)
    ]);
    assert.equal(first.status, "ok");
    assert.equal(second.status, "ok");
    assert.equal(first.executionId, "exec_coalesced");
    assert.equal(second.executionId, "exec_coalesced");
    assert.equal(harness.nativeMessagesSent.filter(x => x.msg.op === "launch").length, 1);
    console.log("  [PASS] Concurrent identical browser launches coalesce to one native request");
  }

  console.log("ALL real background.js and content_script.js harness tests PASSED CLEANLY!");
}

runTests().catch((err) => {
  console.error("Test failed:", err);
  process.exit(1);
});
