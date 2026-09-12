import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import assert from "node:assert/strict";

const BACKGROUND_JS_PATH = path.resolve("extension/background.js");
const CONTENT_SCRIPT_JS_PATH = path.resolve("extension/content_script.js");
const OPTIONS_JS_PATH = path.resolve("extension/options.js");
const POPUP_JS_PATH = path.resolve("extension/popup.js");
const backgroundCode = fs.readFileSync(BACKGROUND_JS_PATH, "utf8");
const contentScriptCode = fs.readFileSync(CONTENT_SCRIPT_JS_PATH, "utf8");
const optionsCode = fs.readFileSync(OPTIONS_JS_PATH, "utf8");
const popupCode = fs.readFileSync(POPUP_JS_PATH, "utf8");

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
        const resp = typeof nativeResponse === "function" ? nativeResponse(msg) : nativeResponse;
        cb(resp);
      }
    },
    alarms: {
      _alarms: new Map(),
      _listeners: [],
      onAlarm: {
        addListener(fn) {
          mockChrome.alarms._listeners.push(fn);
        }
      },
      async get(name) {
        return mockChrome.alarms._alarms.get(name) || null;
      },
      async create(name, info) {
        mockChrome.alarms._alarms.set(name, { name, ...info });
      },
      async clear(name) {
        mockChrome.alarms._alarms.delete(name);
      }
    },
    tabs: {
      async get(tabId) {
        if (mockTabs && mockTabs[tabId]) {
          return mockTabs[tabId];
        }
        throw new Error("Tab not found: " + tabId);
      },
      async query(queryInfo) {
        const res = [];
        for (const tab of Object.values(mockTabs)) {
          if (queryInfo && queryInfo.url) {
            const prefix = queryInfo.url.replace(/\*$/, "");
            if (tab.url && tab.url.startsWith(prefix)) {
              res.push(tab);
            }
          } else {
            res.push(tab);
          }
        }
        return res;
      },
      async create(createInfo) {
        const newId = 900 + Math.floor(Math.random() * 100);
        const newTab = { id: newId, url: createInfo.url, active: createInfo.active ?? true };
        mockTabs[newId] = newTab;
        return newTab;
      },
      lastSendMessageOptions: null,
      sendMessage(tabId, message, optionsOrCb, maybeCb) {
        const options = typeof optionsOrCb === "object" ? optionsOrCb : null;
        const cb = typeof optionsOrCb === "function" ? optionsOrCb : maybeCb;
        mockChrome.tabs.lastSendMessageOptions = options;
        if (mockTabMessages && mockTabMessages[tabId]) {
          const resp = mockTabMessages[tabId](message, options);
          if (cb) cb(resp);
          mockChrome.runtime.lastError = null;
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
      turnContainerTag = "DIV",
      roleTurns = null,
      userMenu = null,
      accountCandidates = null,
      modernProfileMenu = null,
      modernProfileMenuTag = "BUTTON",
      modernProfileMenuText = ""
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
            const isDivQualified = selector.includes("main div");
            if (isDivQualified && (turnContainerTag || "").toUpperCase() !== "DIV") return [];
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
          if (selector.includes('aria-label*="profile menu"') && modernProfileMenu) {
            const isButtonQualified = selector.trim().toLowerCase().startsWith("button");
            if (isButtonQualified && (modernProfileMenuTag || "").toUpperCase() !== "BUTTON") return [];
            return [{ tagName: modernProfileMenuTag, innerText: modernProfileMenuText, getAttribute: (name) => name === "aria-label" ? modernProfileMenu : null }];
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

    // Case G: current ChatGPT exposes account/workspace identity through an accessible
    // profile-menu label and project link instead of the legacy data-testid/id selectors.
    const modernAccountRes = runContentScriptInVm({
      pathname: "/g/g-p-project/c/c_123",
      turns: ["Turn 1"],
      modernProfileMenu: "Example Business, open profile menu"
    });
    assert.equal(modernAccountRes.ok, true);
    assert.equal(modernAccountRes.accountText, "Example Business");

    // Case H: generic label without identity (only "Open profile menu") must fail closed
    const genericLabelRes = runContentScriptInVm({
      pathname: "/c/c_123",
      turns: ["Turn 1"],
      modernProfileMenu: "Open profile menu"
    });
    assert.equal(genericLabelRes.ok, false);
    assert.equal(genericLabelRes.error, "missing_account_context");

    // Case I: whitespace / case tolerant stripping of ", open profile menu"
    const caseTolerantRes = runContentScriptInVm({
      pathname: "/c/c_123",
      turns: ["Turn 1"],
      modernProfileMenu: "Acme Corp ,  OPEN PROFILE MENU  "
    });
    assert.equal(caseTolerantRes.ok, true);
    assert.equal(caseTolerantRes.accountText, "Acme Corp");
    // Case J: live ChatGPT 2026-09-12 exposes profile menu as DIV role=button
    // (aria "Monet Business, open profile menu", visible "Monet\nBusiness").
    const liveDivRes = runContentScriptInVm({
      pathname: "/g/g-p-6a8e8fcbb0248191af6a77a554c62e31/c/6aa19a1f-34a0-83ec-8453-739b915a288b",
      turns: ["Turn 1"],
      modernProfileMenu: "Monet Business, open profile menu",
      modernProfileMenuTag: "DIV",
      modernProfileMenuText: "Monet\nBusiness"
    });
    assert.equal(liveDivRes.ok, true);
    assert.equal(liveDivRes.accountText, "Monet\nBusiness");
    // Case K: live ChatGPT 2026-09-12 renders turn containers as SECTION,
    // not DIV. Tier-1 must stay tag-generic to prefer canonical turns.
    const liveSectionRes = runContentScriptInVm({
      pathname: "/g/g-p-6a8e8fcbb0248191af6a77a554c62e31/c/6aa19a1f-34a0-83ec-8453-739b915a288b",
      conversationTurns: ["Canonical live turn one", "Canonical live turn two"],
      turnContainerTag: "SECTION",
      turns: ["Wrapper duplicate one"],
      roleTurns: ["Nested duplicate one"],
      modernProfileMenu: "Monet Business, open profile menu",
      modernProfileMenuTag: "DIV",
      modernProfileMenuText: "Monet\nBusiness"
    });
    assert.equal(liveSectionRes.ok, true);
    assert.ok(liveSectionRes.transcriptText.includes("Canonical live turn one"));
    assert.equal(liveSectionRes.transcriptText.includes("Wrapper duplicate one"), false);
    assert.equal(liveSectionRes.transcriptText.includes("Nested duplicate one"), false);
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

  // Local mode UX must not expose pairing/bootstrap/policy ceremony or browser path registration.
  assert.ok(optionsCode.includes('action: "ensureLocalMode"'));
  assert.equal(optionsCode.includes('action: "addWorkspace"'), false);
  assert.equal(optionsCode.includes('action: "removeWorkspace"'), false);
  assert.equal(optionsCode.includes("bootstrapToken"), false);
  assert.equal(optionsCode.includes("pairingId"), false);
  assert.equal(optionsCode.includes("policyRevision"), false);
  assert.ok(popupCode.includes('action: "ensureLocalMode"'));
  console.log("  [PASS] Extension UI uses local mode without pairing ceremony or browser path registration");

  // Test 25: local mode seeds fixed internal credentials and current targets.
  {
    const harness = createTestHarness({
      nativeResponse: (msg) => {
        assert.equal(msg.op, "local_status");
        return {
          status: "ok",
          pairingId: "local",
          profileId: "local",
          pairingStatus: "active",
          policyRevision: "v1",
          targets: [{ target_id: "hands", canonical_path: "F:\\CodeBase\\hands", name: "hands" }]
        };
      }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/options.html` };
    const res = await harness.sendMessage({ action: "ensureLocalMode" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.isPaired, true);
    assert.equal(harness.storageStore.profileId, "local");
    assert.equal(harness.storageStore.pairingId, "local");
    assert.equal(harness.storageStore.pairingSecret, "local");
    assert.equal(harness.storageStore.policyRevision, "v1");
    assert.equal(harness.storageStore.targets[0].target_id, "hands");
    console.log("  [PASS] Local mode seeds internal credentials and targets automatically");
  }

  // Test 26: browser rejects addWorkspace/removeWorkspace actions (trust boundary preserved).
  {
    const harness = createTestHarness({
      initialStorage: {
        profileId: "local",
        isPaired: true,
        pairingId: "local",
        pairingSecret: "local",
        policyRevision: "v1",
        targets: [{ target_id: "old", canonical_path: "F:\\old", name: "old" }],
        conv_target_local_conv_old: "old"
      }
    });
    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/options.html` };
    const add = await harness.sendMessage({ action: "addWorkspace", targetPath: "F:\\CodeBase\\flowkit" }, trustedSender);
    assert.equal(add.status, "error");
    assert.equal(add.code, "unsupported_action");

    const remove = await harness.sendMessage({ action: "removeWorkspace", targetId: "old" }, trustedSender);
    assert.equal(remove.status, "error");
    assert.equal(remove.code, "unsupported_action");
    assert.equal(harness.nativeMessagesSent.length, 0, "No native messages sent for removed browser workspace actions");
    console.log("  [PASS] Browser addWorkspace/removeWorkspace actions rejected, preserving trust boundary");
  }

  // Test 27: concurrent identical launches coalesce to one native request.
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

  // Test 26: Drain drains completion receipts and persists browser record before sending transport ACK
  {
    let ackSent = false;
    let storageStateAtAck = null;
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_drain_test",
        pairingSecret: "rb_sec_drain_test"
      }
    });

    // Override sendNativeMessage to mock drain & ack
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "drain") {
        cb({
          status: "ok",
          summaries: [],
          receipts: [{
            receipt_id: "rcpt_drain_unit_1",
            execution_id: "exec_drain_unit_1",
            pairing_id: "pair_drain_test",
            return_token: "ret_drain_1",
            origin_conversation_id: "conv_1",
            turn_index: 0,
            stop_reason: "stop",
            assistant_message_id: "msg_1",
            assistant_text: "Turn done",
            content_digest: "digest_1",
            tool_call_count: 2,
            state: "completed"
          }]
        });
      } else if (msg.op === "ack") {
        ackSent = true;
        // Snapshot storage state at the moment ACK is sent!
        storageStateAtAck = { ...harness.storageStore };
        cb({
          status: "ok",
          acknowledged: true,
          receiptId: msg.receiptId,
          executionId: msg.executionId
        });
      } else {
        cb({ status: "ok" });
      }
    };

    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const res = await harness.sendMessage({ action: "drain" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(JSON.stringify(res.ackedReceiptIds), JSON.stringify(["rcpt_drain_unit_1"]));
    assert.ok(ackSent, "Transport ACK must be sent for drained receipt");
    assert.ok(storageStateAtAck["receipt_rcpt_drain_unit_1"], "Receipt MUST be in browser storage when ACK is sent");
    assert.equal(storageStateAtAck["receipt_rcpt_drain_unit_1"].deliveryStatus, "received");

    // Verify getPendingReceipts returns this recoverable receipt
    const pendingRes = await harness.sendMessage({ action: "getPendingReceipts" }, trustedSender);
    assert.equal(pendingRes.status, "ok");
    assert.equal(pendingRes.pendingReceipts.length, 1);
    assert.equal(pendingRes.pendingReceipts[0].receiptId, "rcpt_drain_unit_1");
    assert.equal(pendingRes.pendingReceipts[0].deliveryStatus, "received");
    console.log("  [PASS] Drain drains receipts, persists locally before native ACK, and reports pending receipts");
  }

  // Test 27: Storage failure prevents transport ACK (fail-closed S3)
  {
    let ackSentForFail = false;
    const harness = createTestHarness({
      failStorageSet: true,
      initialStorage: {
        profileId: "prof_storage_fail",
        isPaired: true,
        pairingId: "pair_storage_fail",
        pairingSecret: "rb_sec_storage_fail"
      }
    });
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "drain") {
        cb({
          status: "ok",
          summaries: [],
          receipts: [{
            receipt_id: "rcpt_fail_1",
            execution_id: "exec_fail_1",
            pairing_id: "pair_storage_fail",
            return_token: "ret_fail_1",
            origin_conversation_id: "conv_fail",
            turn_index: 0,
            stop_reason: "stop",
            assistant_text: "Should not be acked",
            content_digest: "digest_fail",
            tool_call_count: 0
          }]
        });
      } else if (msg.op === "ack") {
        ackSentForFail = true;
        cb({ status: "ok", acknowledged: true });
      } else {
        cb({ status: "ok" });
      }
    };

    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const res = await harness.sendMessage({ action: "drain" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(ackSentForFail, false, "Transport ACK must NOT be sent when browser storage fails");
    assert.equal(JSON.stringify(res.ackedReceiptIds), JSON.stringify([]), "No receipts should be reported acked on storage failure");
    console.log("  [PASS] Storage failure prevents transport ACK without false acknowledgement");
  }

  // Test 28: Alarm scheduling and wakeup drains receipts
  {
    let drainSentOnAlarm = false;
    const harness = createTestHarness({
      initialStorage: {
        profileId: "prof_alarm_test",
        isPaired: true,
        pairingId: "pair_alarm_test",
        pairingSecret: "rb_sec_alarm_test"
      }
    });
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "drain") {
        drainSentOnAlarm = true;
        cb({ status: "ok", summaries: [], receipts: [] });
      } else {
        cb({ status: "ok" });
      }
    };

    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    // 1. Ensure alarms action creates recovery alarm if missing
    await harness.sendMessage({ action: "ensureAlarms" }, trustedSender);
    const alarm = await harness.mockChrome.alarms.get("hands_return_bridge_recovery_drain");
    assert.ok(alarm, "Recovery alarm must be scheduled");
    assert.equal(alarm.periodInMinutes, 1);

    // 2. Trigger alarm listener
    for (const listener of harness.mockChrome.alarms._listeners) {
      await listener({ name: "hands_return_bridge_recovery_drain" });
    }
    assert.ok(drainSentOnAlarm, "Drain must be invoked when recovery alarm fires");
    console.log("  [PASS] Alarm scheduling repairs missing alarms and triggers drain on wakeups");
  }

  // Test 29: AC4 Recovery: Local receipt state loss is reconstructed from native durable summary on later drain
  {
    const harness = createTestHarness({
      initialStorage: {
        profileId: "prof_loss_test",
        isPaired: true,
        pairingId: "pair_loss_test",
        pairingSecret: "rb_sec_loss_test"
      }
    });

    let ackCount = 0;
    const testReceipt = {
      receipt_id: "rcpt_reconstruct_1",
      execution_id: "exec_reconstruct_1",
      pairing_id: "pair_loss_test",
      return_token: "ret_reconstruct_1",
      origin_conversation_id: "conv_loss_test",
      turn_index: 0,
      stop_reason: "stop",
      assistant_message_id: "msg_loss_1",
      assistant_text: "Task completed",
      content_digest: "digest_loss_1",
      tool_call_count: 3,
      state: "completed"
    };

    const launchSummaryWithReceipt = {
      launch_request_id: "req_loss_1",
      execution_id: "exec_reconstruct_1",
      origin_conversation_id: "conv_loss_test",
      origin_conversation_url: "https://chatgpt.com/c/conv_loss_test",
      target_id: "default",
      policy_revision: "default_v1",
      state: "completed",
      created_at: 1000,
      attempt_marked_at: 1001,
      orca_terminal_handle: "term_loss_1",
      completion_receipt: testReceipt
    };

    // Round 1: First drain returns unacknowledged receipt -> persists & ACKs
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "drain") {
        cb({
          status: "ok",
          summaries: [launchSummaryWithReceipt],
          receipts: [testReceipt]
        });
      } else if (msg.op === "ack") {
        ackCount++;
        cb({
          status: "ok",
          acknowledged: true,
          receiptId: msg.receiptId,
          executionId: msg.executionId
        });
      } else {
        cb({ status: "ok" });
      }
    };

    const trustedSender = { id: harness.extensionId, url: `chrome-extension://${harness.extensionId}/popup.html` };
    const firstDrain = await harness.sendMessage({ action: "drain" }, trustedSender);
    assert.equal(firstDrain.status, "ok");
    assert.equal(ackCount, 1, "First drain must send transport ACK");
    assert.ok(harness.storageStore["receipt_rcpt_reconstruct_1"]);
    assert.equal(harness.storageStore["receipt_rcpt_reconstruct_1"].deliveryStatus, "received");

    // Simulating catastrophic local storage loss: receipt keys deleted/corrupted
    delete harness.storageStore["receipt_rcpt_reconstruct_1"];
    delete harness.storageStore["rcpt_by_exec_exec_reconstruct_1"];
    assert.equal(harness.storageStore["receipt_rcpt_reconstruct_1"], undefined);

    // Verify getPendingReceipts currently shows 0 due to local storage loss
    const pendingLost = await harness.sendMessage({ action: "getPendingReceipts" }, trustedSender);
    assert.equal(pendingLost.pendingReceipts.length, 0, "Receipt is lost in local storage");

    // Round 2: Later drain after local storage loss.
    // Since receipt was already ACKed, native journal excludes it from receipts[], but it IS in summaries[].
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "drain") {
        cb({
          status: "ok",
          summaries: [launchSummaryWithReceipt],
          receipts: [] // Already acknowledged in native journal!
        });
      } else if (msg.op === "ack") {
        ackCount++;
        cb({ status: "ok", acknowledged: true });
      } else {
        cb({ status: "ok" });
      }
    };

    const secondDrain = await harness.sendMessage({ action: "drain" }, trustedSender);
    assert.equal(secondDrain.status, "ok");
    // Storage must be reconstructed from native summary authority!
    const reconstructed = harness.storageStore["receipt_rcpt_reconstruct_1"];
    assert.ok(reconstructed, "Receipt MUST be reconstructed in local storage from native summary");
    assert.equal(reconstructed.receiptId, "rcpt_reconstruct_1");
    assert.equal(reconstructed.executionId, "exec_reconstruct_1");
    assert.equal(reconstructed.deliveryStatus, "received", "Reconstructed status MUST be 'received' without Send permission");
    assert.equal(harness.storageStore["rcpt_by_exec_exec_reconstruct_1"], "rcpt_reconstruct_1");

    // Verify getPendingReceipts now discovers the reconstructed receipt
    const pendingRecovered = await harness.sendMessage({ action: "getPendingReceipts" }, trustedSender);
    assert.equal(pendingRecovered.pendingReceipts.length, 1);
    assert.equal(pendingRecovered.pendingReceipts[0].receiptId, "rcpt_reconstruct_1");
    assert.equal(pendingRecovered.pendingReceipts[0].deliveryStatus, "received");

    // No redundant ACK should be sent for already-acknowledged reconstructed receipt
    assert.equal(ackCount, 1, "No duplicate transport ACK should be sent during reconstruction");

    console.log("  [PASS] AC4 Recovery: Reconstructs deliveryStatus='received' from native durable authority after local storage loss");
  }

  // ---------------------------------------------------------------------------
  // Test 32: Dispatch Fence: One-time grant consumption and synchronous guard+click
  // ---------------------------------------------------------------------------
  {
    const testReceipt = {
      receiptId: "rcpt_dispatch_1",
      executionId: "exec_dispatch_1",
      pairingId: "pair_dispatch",
      returnToken: "ret_dispatch_1",
      originConversationId: "c_fence_123",
      originConversationUrl: "https://chatgpt.com/c/c_fence_123",
      deliveryStatus: "received"
    };

    let grantCalled = false;
    let settleCalled = false;
    let settledOutcome = null;
    let clickAttempted = 0;

    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_dispatch",
        pairingSecret: "rb_sec_dispatch",
        receipt_rcpt_dispatch_1: testReceipt
      },
      mockTabs: {
        201: { id: 201, url: "https://chatgpt.com/c/c_fence_123" }
      },
      mockTabMessages: {
        201: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            return {
              ok: true,
              documentId: "doc_test_live_1",
              readiness: {
                ready: true,
                transcriptText: "Previous conversation transcript text...",
                accountText: "Personal (monet@example.com)"
              }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            clickAttempted++;
            return {
              ok: true,
              clicked: true,
              documentId: "doc_test_live_1",
              attemptId: msg.attemptId,
              receiptMarker: msg.receiptMarker
            };
          }
          if (msg.action === "verify_submitted_message") {
            return {
              ok: true,
              observed: true,
              observedMessageId: "msg_chatgpt_live_999",
              transcriptText: `Previous text... [Hands Return Bridge] Local agent completed... ${msg.receiptMarker}`
            };
          }
          return { ok: false };
        }
      }
    });

    // Override sendNativeMessage to mock native dispatch_fence and settle_fence
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "dispatch_fence") {
        grantCalled = true;
        cb({
          status: "ok",
          grant: {
            granted: true,
            receipt_id: msg.receiptId,
            execution_id: msg.executionId,
            attempt_id: msg.attemptId,
            delivery_revision: msg.expectedDeliveryRevision,
            state: "dispatching/uncertain",
            owner_document_id: msg.documentId,
            receipt_marker: msg.receiptMarker,
            payload_digest: msg.payloadDigest
          }
        });
        return;
      }
      if (msg.op === "settle_fence") {
        settleCalled = true;
        settledOutcome = msg.outcome;
        cb({
          status: "ok",
          settlement: {
            settled: true,
            receipt_id: msg.receiptId,
            attempt_id: msg.attemptId,
            outcome: msg.outcome,
            slot_released: msg.outcome === "submitted-observed" || msg.outcome === "not-sent"
          }
        });
        return;
      }
      cb({ status: "ok" });
    };

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // Dispatch receipt 1
    const res = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_dispatch_1" }, trustedSender);
    assert.equal(res.status, "ok", JSON.stringify(res));
    assert.equal(res.dispatchResult?.status, "ok", JSON.stringify(res.dispatchResult));
    assert.equal(res.dispatchResult.outcome, "submitted-observed");
    assert.equal(clickAttempted, 1, "Click must occur exactly once");
    assert.ok(grantCalled, "Native dispatch_fence must be called before click");
    assert.ok(settleCalled, "Native settle_fence must be called after verification");
    assert.equal(settledOutcome, "submitted-observed");

    // Local record is now terminal submitted-observed
    const updated = harness.storageStore["receipt_rcpt_dispatch_1"];
    assert.equal(updated.deliveryStatus, "submitted-observed");
    assert.equal(updated.observedMessageId, "msg_chatgpt_live_999");

    // Attempting to dispatch again must be skipped (no second grant or click)
    const res2 = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_dispatch_1" }, trustedSender);
    assert.equal(res2.dispatchResult.status, "skipped", "Already submitted receipt must be skipped");
    assert.equal(clickAttempted, 1, "No second click allowed");

    console.log("  [PASS] Dispatch Fence: One-time grant consumption and synchronous guard+click");
  }

  // ---------------------------------------------------------------------------
  // Test 33: User draft preservation: Dispatch halts without overwriting draft
  // ---------------------------------------------------------------------------
  {
    const testReceipt = {
      receiptId: "rcpt_draft_1",
      executionId: "exec_draft_1",
      originConversationId: "c_draft_123",
      originConversationUrl: "https://chatgpt.com/c/c_draft_123",
      deliveryStatus: "received"
    };

    let clickAttempted = false;
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_dispatch",
        pairingSecret: "rb_sec_dispatch",
        receipt_rcpt_draft_1: testReceipt
      },
      mockTabs: { 202: { id: 202, url: "https://chatgpt.com/c/c_draft_123" } },
      mockTabMessages: {
        202: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            return {
              ok: false,
              readiness: {
                ready: false,
                reason: "unrelated_draft_present",
                message: "User draft present in composer; preserving draft without overwrite"
              }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            clickAttempted = true;
            return { ok: true, clicked: true };
          }
          return { ok: false };
        }
      }
    });

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_draft_1" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.dispatchResult.status, "waiting");
    assert.equal(res.dispatchResult.reason, "unrelated_draft_present");
    assert.equal(clickAttempted, false, "Must never overwrite draft or click");

    console.log("  [PASS] User draft preservation: Dispatch halts without overwriting draft");
  }

  // ---------------------------------------------------------------------------
  // Test 34: Navigation / route change invalidates delivery preparation
  // ---------------------------------------------------------------------------
  {
    const testReceipt = {
      receiptId: "rcpt_nav_1",
      executionId: "exec_nav_1",
      originConversationId: "c_nav_orig",
      originConversationUrl: "https://chatgpt.com/c/c_nav_orig",
      deliveryStatus: "received"
    };

    let clickAttempted = false;
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_dispatch",
        pairingSecret: "rb_sec_dispatch",
        receipt_rcpt_nav_1: testReceipt
      },
      mockTabs: { 203: { id: 203, url: "https://chatgpt.com/c/c_nav_orig" } },
      mockTabMessages: {
        203: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            // Page navigated to a different conversation
            return {
              ok: false,
              readiness: {
                ready: false,
                reason: "navigation_invalidated",
                message: "Document URL or route changed since script load"
              }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            clickAttempted = true;
            return { ok: true, clicked: true };
          }
          return { ok: false };
        }
      }
    });

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_nav_1" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.dispatchResult.status, "waiting");
    assert.equal(res.dispatchResult.reason, "navigation_invalidated");
    assert.equal(clickAttempted, false, "Navigation must invalidate delivery");

    console.log("  [PASS] Navigation / route change invalidates delivery preparation");
  }

  // ---------------------------------------------------------------------------
  // Test 35: Competing tab / slot busy receives denied status, NEVER permission
  // ---------------------------------------------------------------------------
  {
    const testReceipt = {
      receiptId: "rcpt_busy_1",
      executionId: "exec_busy_1",
      originConversationId: "c_busy_123",
      originConversationUrl: "https://chatgpt.com/c/c_busy_123",
      deliveryStatus: "received"
    };

    let clickAttempted = false;
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_dispatch",
        pairingSecret: "rb_sec_dispatch",
        receipt_rcpt_busy_1: testReceipt
      },
      mockTabs: { 204: { id: 204, url: "https://chatgpt.com/c/c_busy_123" } },
      mockTabMessages: {
        204: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            return {
              ok: true,
              documentId: "doc_busy_tab_2",
              readiness: { ready: true, transcriptText: "t", accountText: "a" }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            clickAttempted = true;
            return { ok: true, clicked: true };
          }
          return { ok: false };
        }
      }
    });

    // Mock native host returning granted=false (slot busy or competing owner)
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      if (msg.op === "dispatch_fence") {
        cb({
          status: "ok",
          grant: {
            granted: false, // DENIED
            receipt_id: msg.receiptId,
            execution_id: msg.executionId,
            attempt_id: "attempt_other_tab",
            delivery_revision: 1,
            state: "dispatching/uncertain",
            owner_document_id: "doc_first_tab"
          }
        });
        return;
      }
      cb({ status: "ok" });
    };

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_busy_1" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.dispatchResult.status, "denied");
    assert.equal(res.dispatchResult.reason, "slot_busy_or_competing_owner");
    assert.equal(clickAttempted, false, "Loser must NEVER click");

    console.log("  [PASS] Competing tab / slot busy receives denied status, NEVER permission");
  }

  // ---------------------------------------------------------------------------
  // Test 36: Inconclusive outcome after click leaves attempt UNCERTAIN (retains slot)
  // ---------------------------------------------------------------------------
  {
    const testReceipt = {
      receiptId: "rcpt_uncertain_1",
      executionId: "exec_uncertain_1",
      originConversationId: "c_uncertain_123",
      originConversationUrl: "https://chatgpt.com/c/c_uncertain_123",
      deliveryStatus: "received"
    };

    let settledOutcome = null;
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_dispatch",
        pairingSecret: "rb_sec_dispatch",
        receipt_rcpt_uncertain_1: testReceipt
      },
      mockTabs: { 205: { id: 205, url: "https://chatgpt.com/c/c_uncertain_123" } },
      mockTabMessages: {
        205: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            return {
              ok: true,
              documentId: "doc_uncertain_1",
              readiness: { ready: true, transcriptText: "t", accountText: "a" }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            return { ok: true, clicked: true, documentId: "doc_uncertain_1" };
          }
          if (msg.action === "verify_submitted_message") {
            // Inconclusive! Message not yet rendered/persisted
            return { ok: true, observed: false };
          }
          return { ok: false };
        }
      }
    });

    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      if (msg.op === "dispatch_fence") {
        cb({
          status: "ok",
          grant: {
            granted: true,
            receipt_id: msg.receiptId,
            execution_id: msg.executionId,
            attempt_id: msg.attemptId,
            delivery_revision: 1,
            state: "dispatching/uncertain",
            owner_document_id: msg.documentId
          }
        });
        return;
      }
      if (msg.op === "settle_fence") {
        settledOutcome = msg.outcome;
        cb({
          status: "ok",
          settlement: {
            settled: true,
            receipt_id: msg.receiptId,
            attempt_id: msg.attemptId,
            outcome: msg.outcome,
            slot_released: false // Uncertain retains slot!
          }
        });
        return;
      }
      cb({ status: "ok" });
    };

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_uncertain_1" }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.dispatchResult.status, "uncertain");
    assert.equal(res.dispatchResult.outcome, "uncertain");
    assert.equal(settledOutcome, "uncertain", "Must record outcome='uncertain'");

    const storedRec = harness.storageStore["receipt_rcpt_uncertain_1"];
    assert.equal(storedRec.deliveryStatus, "dispatching/uncertain", "Local status must stay dispatching/uncertain");

    console.log("  [PASS] Inconclusive outcome after click leaves attempt UNCERTAIN (retains slot)");
  }

  // ---------------------------------------------------------------------------
  // Test 37: N3 State Loss: Loss of extension local state after Dispatch Fence
  //          does not click Send again; native durable state keeps attempt uncertain
  // ---------------------------------------------------------------------------
  {
    const testReceipt = {
      receiptId: "rcpt_n3_1",
      executionId: "exec_n3_1",
      originConversationId: "c_n3_123",
      originConversationUrl: "https://chatgpt.com/c/c_n3_123",
      deliveryStatus: "received"
    };

    let clickCount = 0;
    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_dispatch",
        pairingSecret: "rb_sec_dispatch",
        receipt_rcpt_n3_1: testReceipt
      },
      mockTabs: { 206: { id: 206, url: "https://chatgpt.com/c/c_n3_123" } },
      mockTabMessages: {
        206: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            return {
              ok: true,
              documentId: "doc_n3_live",
              readiness: { ready: true, transcriptText: "t", accountText: "a" }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            clickCount++;
            return { ok: true, clicked: true, documentId: "doc_n3_live" };
          }
          if (msg.action === "verify_submitted_message") {
            return { ok: true, observed: false };
          }
          return { ok: false };
        }
      }
    });

    let nativeFenceState = "not_granted";
    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      if (msg.op === "dispatch_fence") {
        if (nativeFenceState === "not_granted") {
          nativeFenceState = "dispatching/uncertain";
          cb({
            status: "ok",
            grant: {
              granted: true,
              receipt_id: msg.receiptId,
              execution_id: msg.executionId,
              attempt_id: msg.attemptId,
              delivery_revision: 1,
              state: "dispatching/uncertain",
              owner_document_id: msg.documentId
            }
          });
          return;
        } else {
          // Slot already owned and uncertain: native host denies grant to fresh attempt!
          cb({
            status: "ok",
            grant: {
              granted: false,
              receipt_id: msg.receiptId,
              execution_id: msg.executionId,
              attempt_id: "attempt_prior_uncertain",
              delivery_revision: 1,
              state: "dispatching/uncertain",
              owner_document_id: "doc_prior"
            }
          });
          return;
        }
      }
      if (msg.op === "settle_fence") {
        cb({
          status: "ok",
          settlement: {
            settled: true,
            receipt_id: msg.receiptId,
            attempt_id: msg.attemptId,
            outcome: msg.outcome,
            slot_released: false
          }
        });
        return;
      }
      cb({ status: "ok" });
    };

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // 1. Initial attempt clicks once and outcome is uncertain
    const res1 = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_n3_1" }, trustedSender);
    assert.equal(res1.dispatchResult.status, "uncertain");
    assert.equal(clickCount, 1);

    // 2. N3 State Loss: Simulate complete loss of local browser state (e.g. extension storage wiped / reconstructed)
    delete harness.storageStore["receipt_rcpt_n3_1"];
    harness.storageStore["receipt_rcpt_n3_1"] = { ...testReceipt, deliveryStatus: "received" }; // Re-drained as received

    // 3. New wakeup / dispatch attempt on reconstructed receipt
    const res2 = await harness.sendMessage({ action: "dispatchReceipt", receiptId: "rcpt_n3_1" }, trustedSender);
    assert.equal(res2.dispatchResult.status, "denied", "Native durable state must deny grant after local state loss");
    assert.equal(res2.dispatchResult.reason, "slot_busy_or_competing_owner");
    assert.equal(clickCount, 1, "Must NOT perform second click after N3 state loss!");

    console.log("  [PASS] N3 State Loss: Loss of extension local state does not click Send again");
  }

  // ---------------------------------------------------------------------------
  // Test 38: Multi-workspace targets sync on status and per-conversation binding
  // ---------------------------------------------------------------------------
  {
    const targetsList = [
      { target_id: "target_alpha", canonical_path: "/path/to/alpha", name: "alpha" },
      { target_id: "target_beta", canonical_path: "/path/to/beta", name: "beta" }
    ];
    const sharedTabs = {
      201: { id: 201, url: "https://chatgpt.com/c/c_conv_alpha" },
      202: { id: 202, url: "https://chatgpt.com/c/c_conv_beta" }
    };
    const sharedTabMessages = {
      201: (msg) => ({
        ok: true,
        originConversationId: "c_conv_alpha",
        originConversationUrl: "https://chatgpt.com/c/c_conv_alpha",
        transcriptText: "Turn 1: Alpha",
        accountText: "Personal"
      }),
      202: (msg) => ({
        ok: true,
        originConversationId: "c_conv_beta",
        originConversationUrl: "https://chatgpt.com/c/c_conv_beta",
        transcriptText: "Turn 1: Beta",
        accountText: "Personal"
      })
    };

    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_multi",
        pairingSecret: "rb_sec_multi",
        targets: [{ target_id: "target_old", canonical_path: "/old", name: "old" }]
      },
      nativeResponse: (msg) => {
        if (msg.op === "status") {
          return {
            status: "ok",
            pairingId: "pair_multi",
            pairingStatus: "active",
            taskExecutionAvailable: true,
            targetsCount: 2,
            targets: targetsList,
            policyRevision: "v1"
          };
        }
        if (msg.op === "launch") {
          return {
            status: "ok",
            executionId: "exec_" + msg.targetId,
            returnToken: "ret_" + msg.targetId,
            state: "started"
          };
        }
        return { status: "ok" };
      },
      mockTabs: sharedTabs,
      mockTabMessages: sharedTabMessages
    });

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // 1. Status syncs targets to local storage
    const statusRes = await harness.sendMessage({ action: "status" }, trustedSender);
    assert.equal(statusRes.status, "ok");
    assert.equal(statusRes.targetsCount, 2);
    assert.equal(harness.storageStore.targets.length, 2);
    assert.equal(harness.storageStore.targets[0].target_id, "target_alpha");
    assert.equal(harness.storageStore.targets[1].target_id, "target_beta");

    // 2. Bind conversation alpha to target_alpha, and beta to target_beta
    const setAlpha = await harness.sendMessage({
      action: "setConversationTarget",
      conversationId: "c_conv_alpha",
      targetId: "target_alpha"
    }, trustedSender);
    assert.equal(setAlpha.status, "ok");

    const setBeta = await harness.sendMessage({
      action: "setConversationTarget",
      conversationId: "c_conv_beta",
      targetId: "target_beta"
    }, trustedSender);
    assert.equal(setBeta.status, "ok");

    // Verify getConversationTarget returns bound target
    const getAlpha = await harness.sendMessage({
      action: "getConversationTarget",
      conversationId: "c_conv_alpha"
    }, trustedSender);
    assert.equal(getAlpha.targetId, "target_alpha");

    const getBeta = await harness.sendMessage({
      action: "getConversationTarget",
      conversationId: "c_conv_beta"
    }, trustedSender);
    assert.equal(getBeta.targetId, "target_beta");

    // 3. Launch without explicit targetId uses per-conversation bound target
    // Conv alpha:
    harness.nativeMessagesSent.length = 0;
    const launchAlpha = await harness.sendMessage({
      action: "launch",
      tabId: 201,
      promptText: "Task in alpha"
    }, trustedSender);
    assert.equal(launchAlpha.status, "ok");
    assert.equal(harness.nativeMessagesSent.length, 1);
    assert.equal(harness.nativeMessagesSent[0].msg.targetId, "target_alpha");

    // Conv beta:
    harness.nativeMessagesSent.length = 0;
    const launchBeta = await harness.sendMessage({
      action: "launch",
      tabId: 202,
      promptText: "Task in beta"
    }, trustedSender);
    assert.equal(launchBeta.status, "ok");
    assert.equal(harness.nativeMessagesSent.length, 1);
    assert.equal(harness.nativeMessagesSent[0].msg.targetId, "target_beta");

    // Authoritative binding check: supplying a differing targetId must fail closed with conversation_target_mismatch
    harness.nativeMessagesSent.length = 0;
    const launchMismatch = await harness.sendMessage({
      action: "launch",
      tabId: 201, // bound to target_alpha
      targetId: "target_beta", // conflicting targetId
      promptText: "Conflicting target override attempt"
    }, trustedSender);
    assert.equal(launchMismatch.status, "error");
    assert.equal(launchMismatch.code, "conversation_target_mismatch");
    assert.equal(harness.nativeMessagesSent.length, 0, "Must not send native message on binding mismatch");

    // Matching targetId succeeds
    const launchMatch = await harness.sendMessage({
      action: "launch",
      tabId: 201,
      targetId: "target_alpha",
      promptText: "Matching explicit target"
    }, trustedSender);
    assert.equal(launchMatch.status, "ok");

    // Unbound conversation binds on first explicit registered targetId
    sharedTabs[203] = { id: 203, url: "https://chatgpt.com/c/c_conv_gamma" };
    sharedTabMessages[203] = () => ({
      ok: true,
      originConversationId: "c_conv_gamma",
      originConversationUrl: "https://chatgpt.com/c/c_conv_gamma",
      transcriptText: "Turn 1: Gamma",
      accountText: "Personal"
    });
    harness.nativeMessagesSent.length = 0;
    const launchGammaFirst = await harness.sendMessage({
      action: "launch",
      tabId: 203,
      targetId: "target_beta",
      promptText: "First launch establishing binding"
    }, trustedSender);
    assert.equal(launchGammaFirst.status, "ok");
    assert.equal(harness.storageStore["conv_target_pair_multi_c_conv_gamma"], "target_beta");

    // Subsequent launch on gamma without targetId uses newly established binding
    harness.nativeMessagesSent.length = 0;
    const launchGammaSecond = await harness.sendMessage({
      action: "launch",
      tabId: 203,
      promptText: "Second launch using established binding"
    }, trustedSender);
    assert.equal(launchGammaSecond.status, "ok");
    assert.equal(harness.nativeMessagesSent[0].msg.targetId, "target_beta");
    // 4. Stale/removed target binding fails closed and prunes binding
    // Simulate target_beta was removed from pairing
    harness.storageStore.targets = [targetsList[0]]; // Only target_alpha remains
    harness.nativeMessagesSent.length = 0;

    const launchStale = await harness.sendMessage({
      action: "launch",
      tabId: 202, // bound to target_beta
      promptText: "Task with stale target"
    }, trustedSender);
    assert.equal(launchStale.status, "error");
    assert.equal(launchStale.code, "target_not_found");
    assert.equal(harness.nativeMessagesSent.length, 0, "Must not send native message for removed target");

    // Binding was pruned
    const bindingKeyBeta = "conv_target_pair_multi_c_conv_beta";
    assert.equal(harness.storageStore[bindingKeyBeta], undefined);

    // Single target fallback: now that only target_alpha exists, a conversation without binding uses it
    harness.nativeMessagesSent.length = 0;
    const launchFallback = await harness.sendMessage({
      action: "launch",
      tabId: 202,
      promptText: "Task with single target fallback"
    }, trustedSender);
    assert.equal(launchFallback.status, "ok");
    assert.equal(harness.nativeMessagesSent[0].msg.targetId, "target_alpha");
    assert.equal(
      harness.storageStore["conv_target_pair_multi_c_conv_beta"],
      "target_alpha",
      "Single-target fallback must persist the conversation binding before more targets are added"
    );

    console.log("  [PASS] Multi-workspace targets sync on status and per-conversation binding");
  }

  // ---------------------------------------------------------------------------
  // Test 39: Removed targets are pruned from stale conversation bindings
  // ---------------------------------------------------------------------------
  {
    const emptyHarness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_empty",
        pairingSecret: "rb_sec_empty",
        targets: [],
        conv_target_pair_empty_c_conv_empty: "target_gone"
      },
      nativeResponse: (msg) => msg.op === "launch"
        ? { status: "ok", executionId: "exec_should_not_launch", returnToken: "ret_should_not_launch", state: "started" }
        : { status: "ok" },
      mockTabs: {
        301: { id: 301, url: "https://chatgpt.com/c/c_conv_empty" }
      },
      mockTabMessages: {
        301: () => ({
          ok: true,
          originConversationId: "c_conv_empty",
          originConversationUrl: "https://chatgpt.com/c/c_conv_empty",
          transcriptText: "Turn 1: Empty target registry",
          accountText: "Personal"
        })
      }
    });
    const emptySender = {
      id: emptyHarness.extensionId,
      url: `chrome-extension://${emptyHarness.extensionId}/popup.html`
    };

    const emptyLaunch = await emptyHarness.sendMessage({
      action: "launch",
      tabId: 301,
      promptText: "Do not launch against a removed target"
    }, emptySender);
    assert.equal(emptyLaunch.status, "error");
    assert.equal(emptyLaunch.code, "target_not_found");
    assert.equal(emptyHarness.nativeMessagesSent.length, 0, "Authoritative empty target registry must fail locally");
    assert.equal(emptyHarness.storageStore.conv_target_pair_empty_c_conv_empty, undefined);

    const nativeHarness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_native_stale",
        pairingSecret: "rb_sec_native_stale",
        targets: [{ target_id: "target_gone", canonical_path: "/gone", name: "gone" }],
        conv_target_pair_native_stale_c_conv_native_stale: "target_gone"
      },
      nativeResponse: (msg) => {
        if (msg.op === "launch") {
          return { status: "error", code: "target_not_found", message: "Target was removed locally" };
        }
        return { status: "ok" };
      },
      mockTabs: {
        302: { id: 302, url: "https://chatgpt.com/c/c_conv_native_stale" }
      },
      mockTabMessages: {
        302: () => ({
          ok: true,
          originConversationId: "c_conv_native_stale",
          originConversationUrl: "https://chatgpt.com/c/c_conv_native_stale",
          transcriptText: "Turn 1: Native stale target",
          accountText: "Personal"
        })
      }
    });
    const nativeSender = {
      id: nativeHarness.extensionId,
      url: `chrome-extension://${nativeHarness.extensionId}/popup.html`
    };

    const rejectedLaunch = await nativeHarness.sendMessage({
      action: "launch",
      tabId: 302,
      promptText: "Native should reject stale target"
    }, nativeSender);
    assert.equal(rejectedLaunch.status, "error");
    assert.equal(rejectedLaunch.code, "target_not_found");
    assert.equal(nativeHarness.nativeMessagesSent.length, 1);
    assert.equal(nativeHarness.storageStore.conv_target_pair_native_stale_c_conv_native_stale, undefined);
    assert.deepEqual(nativeHarness.storageStore.targets, []);

    const retryLaunch = await nativeHarness.sendMessage({
      action: "launch",
      tabId: 302,
      promptText: "Retry must fail locally after pruning"
    }, nativeSender);
    assert.equal(retryLaunch.status, "error");
    assert.equal(retryLaunch.code, "missing_target_id");
    assert.equal(nativeHarness.nativeMessagesSent.length, 1, "Pruned stale target must not be retried against native");

    console.log("  [PASS] Removed targets prune stale bindings on cached and native rejection paths");
  }

  // ---------------------------------------------------------------------------
  // Test 41: Findings 1 & 3: Recovery reconciles durable not-sent revision & Gizmo URL
  // ---------------------------------------------------------------------------
  {
    const convId = "conv_gizmo_41";
    const gizmoUrl = `https://chatgpt.com/g/g-4141-custom-gpt/c/${convId}`;
    const rcptId = "rcpt_gizmo_41";
    const execId = "exec_gizmo_41";

    let nativeDispatchRevision = null;
    let nativeDispatchUrl = null;

    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_g41",
        pairingSecret: "sec_g41",
        // Local storage has LOST receipt records!
      },
      mockTabs: {
        401: { id: 401, url: gizmoUrl }
      },
      mockTabMessages: {
        401: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            return {
              ok: true,
              documentId: "doc_g41",
              readiness: {
                ready: true,
                transcriptText: "Transcript in Gizmo chat",
                accountText: "Personal"
              }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            return { ok: true, clicked: true, documentId: "doc_g41" };
          }
          if (msg.action === "verify_submitted_message") {
            return { ok: true, observed: true, observedMessageId: "msg_g41_done" };
          }
          return { ok: false };
        }
      }
    });

    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "drain") {
        cb({
          status: "ok",
          summaries: [
            {
              launch_request_id: "launch_g41",
              execution_id: execId,
              origin_conversation_id: convId,
              origin_conversation_url: gizmoUrl,
              state: "completed",
              completion_receipt: {
                receipt_id: rcptId,
                execution_id: execId,
                pairing_id: "pair_g41",
                return_token: "ret_g41",
                origin_conversation_id: convId,
                origin_conversation_url: gizmoUrl,
                delivery_status: "not-sent",
                delivery_revision: 1,
                turn_index: 0,
                stop_reason: "stop",
                assistant_text: "assistant output",
                content_digest: "digest_g41",
                tool_call_count: 1,
                state: "completed"
              }
            }
          ],
          receipts: []
        });
        return;
      }
      if (msg.op === "dispatch_fence") {
        nativeDispatchRevision = msg.expectedDeliveryRevision;
        nativeDispatchUrl = msg.originConversationUrl;
        cb({
          status: "ok",
          grant: {
            granted: true,
            receipt_id: msg.receiptId,
            execution_id: msg.executionId,
            attempt_id: msg.attemptId,
            delivery_revision: msg.expectedDeliveryRevision,
            state: "dispatching/uncertain",
            owner_document_id: msg.documentId
          }
        });
        return;
      }
      if (msg.op === "settle_fence") {
        cb({ status: "ok", settlement: { settled: true, slot_released: true } });
        return;
      }
      cb({ status: "ok" });
    };

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // 1. Trigger scheduled drain recovery
    await harness.sendMessage({ action: "drain" }, trustedSender);

    // 2. Verify receipt was reconstructed with durable native truth (not-sent, rev 1, Gizmo URL)
    const storedRcpt = harness.storageStore["receipt_" + rcptId];
    assert.ok(storedRcpt, "Receipt must be reconstructed from durable summary");
    assert.equal(storedRcpt.deliveryStatus, "not-sent", "Durable deliveryStatus must be reconciled from native");
    assert.equal(storedRcpt.deliveryRevision, 1, "Durable deliveryRevision must be reconciled from native");
    assert.equal(storedRcpt.originConversationUrl, gizmoUrl, "Gizmo URL must be preserved, not converted to /c/<id>");

    // 3. Dispatch receipt: retry must request incremented revision (1 + 1 = 2) with durable Gizmo URL
    const dispatchRes = await harness.sendMessage({ action: "dispatchReceipt", receiptId: rcptId }, trustedSender);
    assert.equal(dispatchRes.status, "ok");
    assert.equal(nativeDispatchRevision, 2, "Retry after not-sent must request incremented revision 2");
    assert.equal(nativeDispatchUrl, gizmoUrl, "Dispatch must preserve durable Gizmo URL");

    console.log("  [PASS] Findings 1 & 3: Recovery reconciles durable not-sent revision & Gizmo URL");
  }

  // ---------------------------------------------------------------------------
  // Test 42: Finding 4: Response channel failure (lastError) settles uncertain and retains slot
  // ---------------------------------------------------------------------------
  {
    const rcptId = "rcpt_chan_err";
    const testReceipt = {
      receiptId: rcptId,
      executionId: "exec_chan_err",
      pairingId: "pair_chan",
      returnToken: "ret_chan",
      originConversationId: "c_chan_123",
      originConversationUrl: "https://chatgpt.com/c/c_chan_123",
      deliveryStatus: "received"
    };

    let settledOutcome = null;

    const harness = createTestHarness({
      initialStorage: {
        isPaired: true,
        pairingId: "pair_chan",
        pairingSecret: "sec_chan",
        ["receipt_" + rcptId]: testReceipt
      },
      mockTabs: {
        501: { id: 501, url: "https://chatgpt.com/c/c_chan_123" }
      },
      mockTabMessages: {
        501: (msg) => {
          if (msg.action === "check_delivery_readiness") {
            return {
              ok: true,
              documentId: "doc_chan_1",
              readiness: {
                ready: true,
                transcriptText: "Transcript",
                accountText: "Personal"
              }
            };
          }
          if (msg.action === "consume_grant_and_dispatch") {
            // Simulate channel failure / runtime.lastError: callback called with undefined and lastError set
            harness.mockChrome.runtime.lastError = {
              message: "Could not establish connection. Receiving end does not exist."
            };
            return undefined;
          }
          return { ok: false };
        }
      }
    });

    harness.mockChrome.runtime.sendNativeMessage = (host, msg, cb) => {
      harness.nativeMessagesSent.push({ host, msg });
      if (msg.op === "dispatch_fence") {
        cb({
          status: "ok",
          grant: {
            granted: true,
            receipt_id: msg.receiptId,
            execution_id: msg.executionId,
            attempt_id: msg.attemptId,
            delivery_revision: 1,
            state: "dispatching/uncertain",
            owner_document_id: msg.documentId
          }
        });
        return;
      }
      if (msg.op === "settle_fence") {
        settledOutcome = msg.outcome;
        cb({
          status: "ok",
          settlement: {
            settled: true,
            receipt_id: msg.receiptId,
            attempt_id: msg.attemptId,
            outcome: msg.outcome,
            slot_released: false
          }
        });
        return;
      }
      cb({ status: "ok" });
    };

    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({ action: "dispatchReceipt", receiptId: rcptId }, trustedSender);
    assert.equal(res.status, "ok");
    assert.equal(res.dispatchResult.status, "uncertain", "Channel error after dispatch must yield uncertain status");
    assert.equal(settledOutcome, "uncertain", "Channel error must settle uncertain, NEVER not-sent");
    assert.equal(harness.storageStore["receipt_" + rcptId].deliveryStatus, "dispatching/uncertain");

    console.log("  [PASS] Finding 4: Response channel failure (lastError) settles uncertain and retains slot");
  }

  // ---------------------------------------------------------------------------
  // Helper for Content Script Tests (Findings 6, 7, 8)
  // ---------------------------------------------------------------------------
  function setupContentScriptDispatchHarness({
    pathname = "/c/c_test_cs",
    buttonDisabledInitially = false,
    buttonAriaDisabledInitially = false,
    buttonEnablesAfterTicks = 0,
    buttonDetachesAfterTicks = 0,
    routeChangesAfterTicks = 0,
    stopButtonAppearsAfterTicks = 0,
    tamperComposerAfterTicks = 0,
    detachButtonOnPreClick = false,
    userDraft = "",
    turns = ["Turn 1: prior chat"],
    account = "Personal User"
  } = {}) {
    let capturedListener = null;
    let clickCount = 0;
    let buttonTicks = 0;
    let currentPathname = pathname;
    let stopButtonPresent = false;
    let buttonAttached = true;
    let buttonDisabled = buttonDisabledInitially;
    let buttonAriaDisabled = buttonAriaDisabledInitially;

    const mockChrome = {
      runtime: {
        onMessage: {
          addListener(fn) {
            capturedListener = fn;
          }
        }
      }
    };

    let preClickChecked = false;
    const composerObj = {
      tagName: "TEXTAREA",
      get value() {
        if (preClickChecked && detachButtonOnPreClick) {
          buttonAttached = false;
        }
        return userDraft;
      },
      set value(v) {
        userDraft = v;
        preClickChecked = true;
      },
      dispatchEvent(ev) {}
    };

    const sendBtnObj = {
      tagName: "BUTTON",
      get disabled() {
        return buttonDisabled;
      },
      getAttribute(name) {
        if (name === "aria-disabled") return buttonAriaDisabled ? "true" : null;
        if (name === "data-testid") return "send-button";
        return null;
      },
      click() {
        clickCount++;
      }
    };

    const mockDocument = {
      readyState: "complete",
      contains(node) {
        return node === sendBtnObj ? buttonAttached : true;
      },
      body: {
        contains(node) {
          return node === sendBtnObj ? buttonAttached : true;
        }
      },
      querySelector(selector) {
        if (selector.includes("stop-button") && stopButtonPresent) {
          return { tagName: "BUTTON" };
        }
        if (selector.includes("login-button") || selector.includes(".auth-error")) {
          return null;
        }
        if (selector.includes("prompt-textarea") || selector.includes('textarea[data-id="root"]')) {
          return composerObj;
        }
        if (selector.includes("send-button") || selector.includes("composer-submit-button")) {
          buttonTicks++;
          if (buttonEnablesAfterTicks && buttonTicks >= buttonEnablesAfterTicks) {
            buttonDisabled = false;
            buttonAriaDisabled = false;
          }
          if (buttonDetachesAfterTicks && buttonTicks >= buttonDetachesAfterTicks) {
            buttonAttached = false;
          }
          if (routeChangesAfterTicks && buttonTicks >= routeChangesAfterTicks) {
            currentPathname = "/c/c_other_route";
            mockWindow.location.pathname = currentPathname;
            mockWindow.location.href = "https://chatgpt.com" + currentPathname;
          }
          if (stopButtonAppearsAfterTicks && buttonTicks >= stopButtonAppearsAfterTicks) {
            stopButtonPresent = true;
          }
          if (tamperComposerAfterTicks && buttonTicks >= tamperComposerAfterTicks) {
            composerObj.value = "Tampered text by human";
          }
          return buttonAttached ? sendBtnObj : null;
        }
        return null;
      },
      querySelectorAll(selector) {
        if (selector.includes("conversation-turn") || selector === "article") {
          return turns.map(t => ({ innerText: t }));
        }
        if (selector.includes("profile menu") || selector.includes("user-menu")) {
          return [{
            tagName: "BUTTON",
            innerText: account,
            getAttribute: (n) => n === "aria-label" ? account + ", open profile menu" : null
          }];
        }
        return [];
      }
    };

    const mockWindow = {
      location: {
        href: "https://chatgpt.com" + currentPathname,
        pathname: currentPathname
      }
    };

    const ctx = vm.createContext({
      chrome: mockChrome,
      document: mockDocument,
      window: mockWindow,
      setTimeout,
      clearTimeout,
      Promise,
      Set,
      Event: globalThis.Event || class Event {},
      InputEvent: globalThis.InputEvent || class InputEvent {},
      console: { log() {}, error() {}, warn() {} }
    });
    vm.runInContext(contentScriptCode, ctx);

    return {
      listener: capturedListener,
      getClickCount: () => clickCount,
      composer: composerObj,
      sendBtn: sendBtnObj,
      window: mockWindow
    };
  }

  // ---------------------------------------------------------------------------
  // Test 43: Finding 7: consumedGrantAttemptIds rejects replay of same attemptId
  //          but permits distinct later attemptId in same live document
  // ---------------------------------------------------------------------------
  {
    const cs = setupContentScriptDispatchHarness({ pathname: "/c/c_test_cs" });

    // Step 1: Readiness check to obtain documentId
    let readinessResp = null;
    cs.listener({
      action: "check_delivery_readiness",
      expectedConversationId: "c_test_cs",
      expectedConversationUrl: "https://chatgpt.com/c/c_test_cs"
    }, {}, (r) => { readinessResp = r; });
    assert.equal(readinessResp.ok, true);
    const docId = readinessResp.documentId;

    // Step 2: First attempt ("att_alpha") consumes grant and clicks
    let res1 = null;
    await new Promise((resolve) => {
      cs.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_alpha",
        expectedDocumentId: docId,
        expectedConversationId: "c_test_cs",
        expectedConversationUrl: "https://chatgpt.com/c/c_test_cs",
        continuationText: "Continuation payload alpha",
        receiptMarker: "marker_alpha"
      }, {}, (r) => { res1 = r; resolve(); });
    });
    assert.equal(res1.ok, true);
    assert.equal(res1.clicked, true);
    assert.equal(cs.getClickCount(), 1);

    // Step 3: Replay of same attempt ("att_alpha") MUST be rejected with grant_already_consumed
    let resReplay = null;
    await new Promise((resolve) => {
      cs.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_alpha",
        expectedDocumentId: docId,
        expectedConversationId: "c_test_cs",
        expectedConversationUrl: "https://chatgpt.com/c/c_test_cs",
        continuationText: "Continuation payload alpha",
        receiptMarker: "marker_alpha"
      }, {}, (r) => { resReplay = r; resolve(); });
    });
    assert.equal(resReplay.ok, false);
    assert.equal(resReplay.clicked, false);
    assert.equal(resReplay.reason, "grant_already_consumed", "Replay of same attemptId must fail closed");
    assert.equal(cs.getClickCount(), 1, "Replay must not trigger click");

    // Step 4: Later distinct attempt ("att_beta") after slot release MUST be permitted in same document!
    cs.composer.value = ""; // Composer cleared after prior submission
    let resBeta = null;
    await new Promise((resolve) => {
      cs.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_beta",
        expectedDocumentId: docId,
        expectedConversationId: "c_test_cs",
        expectedConversationUrl: "https://chatgpt.com/c/c_test_cs",
        continuationText: "Continuation payload beta",
        receiptMarker: "marker_beta"
      }, {}, (r) => { resBeta = r; resolve(); });
    });
    assert.equal(resBeta.ok, true, "Distinct new attemptId must be allowed");
    assert.equal(resBeta.clicked, true);
    assert.equal(cs.getClickCount(), 2, "Second distinct attempt must be clicked");

    console.log("  [PASS] Finding 7: consumedGrantAttemptIds rejects replay but permits distinct attemptId");
  }

  // ---------------------------------------------------------------------------
  // Test 44: Finding 8: Content script waits for enabled send button, rejects disabled button
  // ---------------------------------------------------------------------------
  {
    // Case A: Button stays disabled -> fails closed with send_button_disabled
    const csDisabled = setupContentScriptDispatchHarness({
      pathname: "/c/c_test_cs",
      buttonDisabledInitially: true
    });
    let readRespA = null;
    csDisabled.listener({ action: "check_delivery_readiness", expectedConversationId: "c_test_cs" }, {}, (r) => { readRespA = r; });
    let resA = null;
    await new Promise((resolve) => {
      csDisabled.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_dis_1",
        expectedDocumentId: readRespA.documentId,
        expectedConversationId: "c_test_cs",
        continuationText: "Continuation text",
        receiptMarker: "marker_1"
      }, {}, (r) => { resA = r; resolve(); });
    });
    assert.equal(resA.ok, false);
    assert.equal(resA.clicked, false);
    assert.equal(resA.reason, "send_button_disabled");
    assert.equal(csDisabled.getClickCount(), 0, "Disabled button must never be clicked");

    // Case B: Button has aria-disabled="true" -> fails closed
    const csAriaDisabled = setupContentScriptDispatchHarness({
      pathname: "/c/c_test_cs",
      buttonAriaDisabledInitially: true
    });
    let readRespB = null;
    csAriaDisabled.listener({ action: "check_delivery_readiness", expectedConversationId: "c_test_cs" }, {}, (r) => { readRespB = r; });
    let resB = null;
    await new Promise((resolve) => {
      csAriaDisabled.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_aria_1",
        expectedDocumentId: readRespB.documentId,
        expectedConversationId: "c_test_cs",
        continuationText: "Continuation text",
        receiptMarker: "marker_1"
      }, {}, (r) => { resB = r; resolve(); });
    });
    assert.equal(resB.ok, false);
    assert.equal(resB.clicked, false);
    assert.equal(resB.reason, "send_button_disabled");
    assert.equal(csAriaDisabled.getClickCount(), 0, "aria-disabled button must never be clicked");

    // Case C: Button starts disabled, becomes enabled on 2nd poll -> succeeds and clicks
    const csEnables = setupContentScriptDispatchHarness({
      pathname: "/c/c_test_cs",
      buttonDisabledInitially: true,
      buttonEnablesAfterTicks: 3
    });
    let readRespC = null;
    csEnables.listener({ action: "check_delivery_readiness", expectedConversationId: "c_test_cs" }, {}, (r) => { readRespC = r; });
    let resC = null;
    await new Promise((resolve) => {
      csEnables.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_enables_1",
        expectedDocumentId: readRespC.documentId,
        expectedConversationId: "c_test_cs",
        continuationText: "Continuation text",
        receiptMarker: "marker_1"
      }, {}, (r) => { resC = r; resolve(); });
    });
    assert.equal(resC.ok, true);
    assert.equal(resC.clicked, true);
    assert.equal(csEnables.getClickCount(), 1, "Button enabled after wait must be clicked");

    console.log("  [PASS] Finding 8: Content script waits for enabled send button, rejects disabled button");
  }

  // ---------------------------------------------------------------------------
  // Test 45: Finding 6: Content script pre-click revalidations
  // ---------------------------------------------------------------------------
  {
    // Sub-case 1: SPA route change during button wait -> aborts before click
    const csRoute = setupContentScriptDispatchHarness({
      pathname: "/c/c_test_cs",
      buttonDisabledInitially: true,
      buttonEnablesAfterTicks: 4,
      routeChangesAfterTicks: 2
    });
    let readResp1 = null;
    csRoute.listener({ action: "check_delivery_readiness", expectedConversationId: "c_test_cs" }, {}, (r) => { readResp1 = r; });
    let res1 = null;
    await new Promise((resolve) => {
      csRoute.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_route_race",
        expectedDocumentId: readResp1.documentId,
        expectedConversationId: "c_test_cs",
        expectedConversationUrl: "https://chatgpt.com/c/c_test_cs",
        continuationText: "Continuation payload",
        receiptMarker: "marker_1"
      }, {}, (r) => { res1 = r; resolve(); });
    });
    assert.equal(res1.ok, false);
    assert.equal(res1.clicked, false);
    assert.ok(res1.reason === "navigation_invalidated" || res1.reason === "conversation_mismatch");
    assert.equal(csRoute.getClickCount(), 0, "Route change during wait must prevent click");

    // Sub-case 2: Active generation appears during wait -> aborts before click
    const csActiveGen = setupContentScriptDispatchHarness({
      pathname: "/c/c_test_cs",
      buttonDisabledInitially: true,
      buttonEnablesAfterTicks: 4,
      stopButtonAppearsAfterTicks: 2
    });
    let readResp2 = null;
    csActiveGen.listener({ action: "check_delivery_readiness", expectedConversationId: "c_test_cs" }, {}, (r) => { readResp2 = r; });
    let res2 = null;
    await new Promise((resolve) => {
      csActiveGen.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_gen_race",
        expectedDocumentId: readResp2.documentId,
        expectedConversationId: "c_test_cs",
        continuationText: "Continuation payload",
        receiptMarker: "marker_1"
      }, {}, (r) => { res2 = r; resolve(); });
    });
    assert.equal(res2.ok, false);
    assert.equal(res2.clicked, false);
    assert.equal(res2.reason, "active_generation");
    assert.equal(csActiveGen.getClickCount(), 0, "Active generation during wait must prevent click");

    // Sub-case 3: User tampers with composer content during wait -> aborts before click
    const csTamper = setupContentScriptDispatchHarness({
      pathname: "/c/c_test_cs",
      buttonDisabledInitially: true,
      buttonEnablesAfterTicks: 4,
      tamperComposerAfterTicks: 2
    });
    let readResp3 = null;
    csTamper.listener({ action: "check_delivery_readiness", expectedConversationId: "c_test_cs" }, {}, (r) => { readResp3 = r; });
    let res3 = null;
    await new Promise((resolve) => {
      csTamper.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_tamper_race",
        expectedDocumentId: readResp3.documentId,
        expectedConversationId: "c_test_cs",
        continuationText: "Continuation payload",
        receiptMarker: "marker_1"
      }, {}, (r) => { res3 = r; resolve(); });
    });
    assert.equal(res3.ok, false);
    assert.equal(res3.clicked, false);
    assert.equal(res3.reason, "composer_content_tampered");
    assert.equal(csTamper.getClickCount(), 0, "Tampered composer text must prevent click");

    // Sub-case 4: Button detached from DOM before click -> aborts before click
    const csDetach = setupContentScriptDispatchHarness({
      pathname: "/c/c_test_cs",
      detachButtonOnPreClick: true
    });
    let readResp4 = null;
    csDetach.listener({ action: "check_delivery_readiness", expectedConversationId: "c_test_cs" }, {}, (r) => { readResp4 = r; });
    let res4 = null;
    await new Promise((resolve) => {
      csDetach.listener({
        action: "consume_grant_and_dispatch",
        attemptId: "att_detach_race",
        expectedDocumentId: readResp4.documentId,
        expectedConversationId: "c_test_cs",
        continuationText: "Continuation payload",
        receiptMarker: "marker_1"
      }, {}, (r) => { res4 = r; resolve(); });
    });
    assert.equal(res4.ok, false);
    assert.equal(res4.clicked, false);
    assert.equal(res4.reason, "send_button_invalid");
    assert.equal(csDetach.getClickCount(), 0, "Detached button must prevent click");

    console.log("  [PASS] Finding 6: Content script pre-click revalidations reject route change, active gen, tampered composer, detached button");
  }

  console.log("ALL real background.js and content_script.js harness tests PASSED CLEANLY!");
}

runTests().catch((err) => {
  console.error("Test failed:", err);
  process.exit(1);
});
