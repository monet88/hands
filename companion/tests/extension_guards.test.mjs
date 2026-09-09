import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import assert from "node:assert/strict";

const BACKGROUND_JS_PATH = path.resolve("companion/extension/background.js");
const backgroundCode = fs.readFileSync(BACKGROUND_JS_PATH, "utf8");

function createTestHarness({
  extensionId = "mkkajdpmlmliildflmnnmfndboldnnfa",
  failSetAccessLevel = false,
  missingSetAccessLevel = false,
  failStorageSet = false,
  nativeResponse = { status: "ok", pairingId: "pair_123", pairingSecret: "rb_sec_456" },
  initialStorage = {}
} = {}) {
  const storageStore = { ...initialStorage };
  let capturedListener = null;
  const nativeMessagesSent = [];

  const mockChrome = {
    runtime: {
      id: extensionId,
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
        cb(nativeResponse);
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
    nativeMessagesSent
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
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_test_123",
      transcriptEvidenceHash: "hash_t_real",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "Do something"
    }, contentScriptSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "unauthorized_sender");
    assert.equal(harness.nativeMessagesSent.length, 0, "Content script MUST NOT trigger native launch");
    console.log("  [PASS] Launch rejects content-script/web-page senders (confused-deputy guard)");
  }

  // ---------------------------------------------------------------------------
  // Test 6: Launch rejects missing or placeholder evidence hashes
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // Placeholder transcript hash
    const resPlaceholder = await harness.sendMessage({
      action: "launch",
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_test_123",
      transcriptEvidenceHash: "hash_transcript_empty",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resPlaceholder.status, "error");
    assert.equal(resPlaceholder.code, "missing_evidence");
    assert.equal(harness.nativeMessagesSent.length, 0);

    // Missing account hash
    const resMissing = await harness.sendMessage({
      action: "launch",
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_test_123",
      transcriptEvidenceHash: "hash_t_real",
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resMissing.status, "error");
    assert.equal(resMissing.code, "missing_evidence");
    assert.equal(harness.nativeMessagesSent.length, 0);
    console.log("  [PASS] Launch rejects empty placeholder or missing evidence hashes");
  }

  // ---------------------------------------------------------------------------
  // Test 7: Launch rejects non-canonical conversation URL / ID mismatch
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // Home / non-canonical URL
    const resHome = await harness.sendMessage({
      action: "launch",
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/",
      transcriptEvidenceHash: "hash_t_real",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resHome.status, "error");
    assert.equal(resHome.code, "invalid_conversation_boundary");

    // Mismatched ID
    const resMismatch = await harness.sendMessage({
      action: "launch",
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_other_999",
      transcriptEvidenceHash: "hash_t_real",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);
    assert.equal(resMismatch.status, "error");
    assert.equal(resMismatch.code, "invalid_conversation_boundary");
    assert.equal(harness.nativeMessagesSent.length, 0);
    console.log("  [PASS] Launch rejects non-canonical URL and ID mismatches");
  }

  {
    const harness = createTestHarness({
      failStorageSet: true,
      initialStorage: { profileId: "prof_launch", isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    const res = await harness.sendMessage({
      action: "launch",
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_test_123",
      transcriptEvidenceHash: "hash_t_real",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "Do something"
    }, trustedSender);

    assert.equal(res.status, "error");
    assert.equal(res.code, "browser_persistence_failure");
    assert.equal(harness.nativeMessagesSent.length, 0, "Zero native message must be sent on storage failure");
    console.log("  [PASS] Browser storage failure prevents native messaging call");
  }

  // ---------------------------------------------------------------------------
  // Test 9: First-request durability, idempotency, and local conflict
  // ---------------------------------------------------------------------------
  {
    const harness = createTestHarness({
      nativeResponse: { status: "ok", executionId: "exec_123", returnToken: "ret_456", state: "started" },
      initialStorage: { isPaired: true, pairingId: "pair_launch", pairingSecret: "rb_sec_launch", policyRevision: "v1" }
    });
    const trustedSender = {
      id: harness.extensionId,
      url: `chrome-extension://${harness.extensionId}/popup.html`
    };

    // A. Initial launch without launchRequestId derives deterministic durable ID
    const res1 = await harness.sendMessage({
      action: "launch",
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_test_123",
      transcriptEvidenceHash: "hash_t_real",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "Fix bug in parser"
    }, trustedSender);

    assert.equal(res1.status, "ok");
    assert.equal(harness.nativeMessagesSent.length, 1);
    const firstSent = harness.nativeMessagesSent[0].msg;
    assert.ok(firstSent.launchRequestId.startsWith("req_"));

    // Check stored durable record
    const storedKey = "launch_" + firstSent.launchRequestId;
    assert.ok(harness.storageStore[storedKey]);
    assert.equal(harness.storageStore[storedKey].status, "started");
    assert.equal(harness.storageStore[storedKey].executionId, "exec_123");

    // B. Re-invoking launch for same conversation & target reuses identical durable request ID
    const res2 = await harness.sendMessage({
      action: "launch",
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_test_123",
      transcriptEvidenceHash: "hash_t_real",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "Fix bug in parser"
    }, trustedSender);

    assert.equal(res2.status, "ok");
    assert.equal(harness.nativeMessagesSent.length, 2);
    assert.equal(harness.nativeMessagesSent[1].msg.launchRequestId, firstSent.launchRequestId, "Must reuse same durable launchRequestId");

    // C. Re-invoking same request ID with CHANGED payload yields local conflict before native call
    const resConflict = await harness.sendMessage({
      action: "launch",
      launchRequestId: firstSent.launchRequestId,
      originConversationId: "c_test_123",
      originConversationUrl: "https://chatgpt.com/c/c_test_123",
      transcriptEvidenceHash: "hash_t_real",
      accountEvidenceHash: "hash_a_real",
      targetId: "target_test",
      promptText: "DIFFERENT PROMPT TEXT!"
    }, trustedSender);

    assert.equal(resConflict.status, "error");
    assert.equal(resConflict.code, "payload_conflict");
    assert.equal(harness.nativeMessagesSent.length, 2, "Conflicting payload must NOT generate new native message");
    console.log("  [PASS] First-request durability, idempotency, and local conflict prevention verified");
  }

  console.log("ALL real background.js harness tests PASSED CLEANLY!");
}

runTests().catch((err) => {
  console.error("Test failed:", err);
  process.exit(1);
});
