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

  console.log("ALL real background.js harness tests PASSED CLEANLY!");
}

runTests().catch((err) => {
  console.error("Test failed:", err);
  process.exit(1);
});
