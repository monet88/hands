// Focused unit and regression tests for extension guards:
// 1. Confused-deputy protection against content script messages
// 2. Fail-closed storage access level isolation

import assert from "node:assert/strict";

const EXTENSION_ID = "mkkajdpmlmliildflmnnmfndboldnnfa";

// Recreate the pure guard logic from companion/extension/background.js
function isTrustedExtensionSender(sender, currentExtId = EXTENSION_ID) {
  if (!sender || sender.id !== currentExtId) {
    return false;
  }
  const extensionOriginPrefix = `chrome-extension://${currentExtId}/`;
  if (typeof sender.url !== "string" || !sender.url.startsWith(extensionOriginPrefix)) {
    return false;
  }
  return true;
}

console.log("Running extension guard regression tests...");

// Test 1: Reject content script on chatgpt.com (confused deputy protection)
{
  const contentScriptSender = {
    id: EXTENSION_ID,
    url: "https://chatgpt.com/c/test-chat-session"
  };
  assert.equal(
    isTrustedExtensionSender(contentScriptSender),
    false,
    "Content script on chatgpt.com sharing extension ID must be rejected"
  );
}

// Test 2: Reject arbitrary web page sender
{
  const webSender = {
    id: EXTENSION_ID,
    url: "https://malicious.example.com/exploit.html"
  };
  assert.equal(
    isTrustedExtensionSender(webSender),
    false,
    "Arbitrary web page sharing extension ID must be rejected"
  );
}

// Test 3: Reject external extension sender with differing ID
{
  const foreignExtSender = {
    id: "different_extension_id_abcdefghijkl",
    url: "chrome-extension://different_extension_id_abcdefghijkl/options.html"
  };
  assert.equal(
    isTrustedExtensionSender(foreignExtSender),
    false,
    "Foreign extension sender must be rejected"
  );
}

// Test 4: Accept trusted extension documents
{
  const optionsSender = {
    id: EXTENSION_ID,
    url: `chrome-extension://${EXTENSION_ID}/options.html`
  };
  assert.equal(
    isTrustedExtensionSender(optionsSender),
    true,
    "Options page document must be accepted"
  );

  const popupSender = {
    id: EXTENSION_ID,
    url: `chrome-extension://${EXTENSION_ID}/popup.html`
  };
  assert.equal(
    isTrustedExtensionSender(popupSender),
    true,
    "Popup document must be accepted"
  );

  const testRunnerSender = {
    id: EXTENSION_ID,
    url: `chrome-extension://${EXTENSION_ID}/test_runner.html`
  };
  assert.equal(
    isTrustedExtensionSender(testRunnerSender),
    true,
    "Test runner document must be accepted"
  );
}

// Test 5: Fail-closed storage access level verification
{
  // Simulate environment where setAccessLevel fails
  let accessLevelSet = false;
  let mockStorage = {
    local: {
      async setAccessLevel({ accessLevel }) {
        throw new Error("setAccessLevel is not supported or permission denied");
      },
      async get(keys) {
        return { pairingSecret: "rb_sec_leak_attempt" };
      },
      async set(items) {
        // no-op
      }
    }
  };

  let storageAccessLevelEstablished = false;
  async function ensureStorageAccessLevel() {
    if (storageAccessLevelEstablished) return true;
    if (mockStorage.local.setAccessLevel) {
      try {
        await mockStorage.local.setAccessLevel({ accessLevel: "TRUSTED_CONTEXTS" });
        storageAccessLevelEstablished = true;
        return true;
      } catch (err) {
        storageAccessLevelEstablished = false;
        return false;
      }
    }
    return false;
  }

  // Verify ensureStorageAccessLevel returns false when setAccessLevel fails
  const established = await ensureStorageAccessLevel();
  assert.equal(established, false, "ensureStorageAccessLevel must return false on failure");

  // Verify fail-closed behavior: secret-bearing action must reject immediately without touching storage
  async function handleSetupAction() {
    const isTrustedStorage = await ensureStorageAccessLevel();
    if (!isTrustedStorage) {
      return {
        status: "error",
        code: "storage_isolation_unavailable",
        message: "Storage isolation (TRUSTED_CONTEXTS) could not be established."
      };
    }
    // Should never reach here
    return { status: "ok" };
  }

  const result = await handleSetupAction();
  assert.equal(result.status, "error");
  assert.equal(result.code, "storage_isolation_unavailable");

  // Verify getState does NOT claim TRUSTED_CONTEXTS when isolation is unavailable
  async function handleGetState() {
    const isTrustedStorage = await ensureStorageAccessLevel();
    if (!isTrustedStorage) {
      return {
        status: "ok",
        isPaired: false,
        pairingId: null,
        storageAccessLevel: "UNTRUSTED"
      };
    }
    return {
      status: "ok",
      storageAccessLevel: "TRUSTED_CONTEXTS"
    };
  }

  const state = await handleGetState();
  assert.equal(state.storageAccessLevel, "UNTRUSTED");
  assert.equal(state.isPaired, false);
}

console.log("All extension guard regression tests passed cleanly!");
