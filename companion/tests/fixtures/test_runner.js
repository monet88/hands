const NATIVE_HOST = "com.hands.return_bridge";

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

function logResult(step, pass, details) {
  const line = `[${pass ? "PASS" : "FAIL"}] ${step}: ${JSON.stringify(details)}`;
  console.log(line);
  const el = document.getElementById("results");
  if (el) {
    el.textContent += line + "\n";
  }
  if (!pass) {
    throw new Error(`Test failed at step: ${step} details: ${JSON.stringify(details)}`);
  }
}

window.startTest = async function(config = {}) {
  const mode = config.mode || "profile_alpha";
  const profileId = config.profileId || (mode === "profile_alpha" ? "profile_alpha" : "profile_beta");
  const bootstrapToken = config.bootstrapToken;
  const pairingId = config.pairingId;
  const pairingSecret = config.pairingSecret;

  const statusEl = document.getElementById("status");
  if (statusEl) {
    statusEl.textContent = `Running mode: ${mode}...`;
  }

  const results = { mode, profileId, steps: [] };

  try {
    if (mode === "profile_alpha") {
      // Step 1: Setup / activate pairing using bootstrap token
      const setupResp = await sendNative({
        op: "setup",
        bootstrapToken,
        profileId
      });
      const setupOk = setupResp && setupResp.status === "ok" && setupResp.pairingId && setupResp.pairingSecret;
      logResult("setup_bootstrap", setupOk, setupResp);
      results.steps.push({ step: "setup_bootstrap", pass: setupOk, resp: setupResp });

      const activePairingId = setupResp.pairingId;
      const activePairingSecret = setupResp.pairingSecret;
      // Pairing credential isolation: verify chrome.storage.local retains secret in trusted context
      await chrome.storage.local.set({ profileId, isPaired: true, pairingId: activePairingId, pairingSecret: activePairingSecret, targets: setupResp.targets || [], policyRevision: setupResp.policyRevision || "v1" });
      const stored = await chrome.storage.local.get(["pairingSecret"]);
      const storageOk = stored.pairingSecret === activePairingSecret;
      logResult("storage_credential_retention", storageOk, { retained: storageOk });
      results.steps.push({ step: "storage_credential_retention", pass: storageOk });


      // Step 2: Connect with valid credentials
      const connectResp = await sendNative({
        op: "connect",
        pairingId: activePairingId,
        pairingSecret: activePairingSecret,
        profileId
      });
      const connectOk = connectResp && connectResp.status === "ok" && connectResp.pairingStatus === "active";
      logResult("connect_active", connectOk, connectResp);
      results.steps.push({ step: "connect_active", pass: connectOk });

      // Step 3: Status check
      const statusResp = await sendNative({
        op: "status",
        pairingId: activePairingId,
        pairingSecret: activePairingSecret,
        profileId
      });
      const statusOk = statusResp && statusResp.status === "ok" && statusResp.taskExecutionAvailable === true;
      logResult("status_check", statusOk, statusResp);
      results.steps.push({ step: "status_check", pass: statusOk });

      // Step 4: Security A1: Attempt unauthorized target override
      const overrideTargetResp = await sendNative({
        op: "connect",
        pairingId: activePairingId,
        pairingSecret: activePairingSecret,
        profileId,
        targets: [{ target_id: "injected", canonical_path: "C:\\Windows" }]
      });
      const targetOverrideRejected = overrideTargetResp && overrideTargetResp.status === "error" && overrideTargetResp.code === "unauthorized_override";
      logResult("reject_target_override", targetOverrideRejected, overrideTargetResp);
      results.steps.push({ step: "reject_target_override", pass: targetOverrideRejected });

      // Step 5: Security A1: Attempt unauthorized policy override
      const overridePolicyResp = await sendNative({
        op: "connect",
        pairingId: activePairingId,
        pairingSecret: activePairingSecret,
        profileId,
        policy: { tool_policy: "bypass" }
      });
      const policyOverrideRejected = overridePolicyResp && overridePolicyResp.status === "error" && overridePolicyResp.code === "unauthorized_override";
      logResult("reject_policy_override", policyOverrideRejected, overridePolicyResp);
      results.steps.push({ step: "reject_policy_override", pass: policyOverrideRejected });

      // Step 6: Security A1: Attempt unauthorized executable override
      const overrideExecResp = await sendNative({
        op: "status",
        pairingId: activePairingId,
        pairingSecret: activePairingSecret,
        profileId,
        executable: "powershell.exe"
      });
      const execOverrideRejected = overrideExecResp && overrideExecResp.status === "error" && overrideExecResp.code === "unauthorized_override";
      logResult("reject_executable_override", execOverrideRejected, overrideExecResp);
      results.steps.push({ step: "reject_executable_override", pass: execOverrideRejected });

      // Step 7: Security A1: Attempt launch with unauthorized field (fails closed with unexpected_field)
      const launchBadResp = await sendNative({
        op: "launch",
        pairingId: activePairingId,
        pairingSecret: activePairingSecret,
        profileId,
        targetId: "hands",
        prompt: "echo malicious" // unauthorized field (expected promptText)
      });
      const launchBadRejected = launchBadResp && launchBadResp.status === "error" && launchBadResp.code === "unexpected_field";
      logResult("reject_launch_unauthorized_field", launchBadRejected, launchBadResp);
      results.steps.push({ step: "reject_launch_unauthorized_field", pass: launchBadRejected });

      // Step 7b: Valid launch request through extension internal messaging with exact bound ChatGPT tab
      const targetList = setupResp.targets || [];
      const validTargetId = targetList.length > 0 ? targetList[0].target_id : "hands";
      const launchReqId = "e2e_req_" + Date.now();

      // Find or create bound ChatGPT tab
      const existingTabs = await chrome.tabs.query({ url: "https://chatgpt.com/c/*" });
      let chatTab = existingTabs[0];
      if (!chatTab) {
        chatTab = await chrome.tabs.create({ url: "https://chatgpt.com/c/conv_e2e_123" });
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }

      // Helper to render conversation turns and user context in the ChatGPT tab
      async function ensureMockChatGptDom() {
        await chrome.scripting.executeScript({
          target: { tabId: chatTab.id },
          func: () => {
            document.body.innerHTML = '<main><article data-testid="conversation-turn-1">Turn 1: Fix bug in parser</article><article data-testid="conversation-turn-2">Turn 2: Done</article><button id="user-menu" data-testid="user-profile">Workspace Alpha User</button></main>';
          }
        });
        await chrome.scripting.executeScript({
          target: { tabId: chatTab.id },
          files: ["content_script.js"]
        });
      }

      await ensureMockChatGptDom();

      const runNonce = Date.now().toString(36);
      const testPrompt = `--flag @some_file "quotes" ; echo pipe | unicode: Đại Ca ${runNonce}\nsecond_line_preserved`;
      const launchPayload = {
        action: "launch",
        launchRequestId: launchReqId,
        tabId: chatTab.id,
        targetId: validTargetId,
        requestedPolicyRevision: "v1",
        promptText: testPrompt
      };

      const internalLaunchResp = await chrome.runtime.sendMessage(launchPayload);
      const launchOk = internalLaunchResp && internalLaunchResp.status === "ok" && internalLaunchResp.executionId && internalLaunchResp.state === "started";
      logResult("launch_owned_execution", launchOk, internalLaunchResp);
      results.steps.push({ step: "launch_owned_execution", pass: launchOk });
      results.executionId = internalLaunchResp ? internalLaunchResp.executionId : null;
      results.terminalHandle = internalLaunchResp && internalLaunchResp.terminalEvidence ? internalLaunchResp.terminalEvidence.orcaTerminalHandle : null;
      results.promptSent = launchPayload.promptText;
      // Step 7c: Idempotent replay with identical payload returns existing execution without re-launching
      await ensureMockChatGptDom();
      const replayResp = await chrome.runtime.sendMessage(launchPayload);
      const replayOk = replayResp && replayResp.status === "ok" && replayResp.isReplayed === true && replayResp.executionId === internalLaunchResp.executionId;
      logResult("launch_idempotent_replay", replayOk, replayResp);
      results.steps.push({ step: "launch_idempotent_replay", pass: replayOk });

      // Step 7d: Replay conflict: same launchRequestId with changed prompt fails closed with payload_conflict
      await ensureMockChatGptDom();
      const conflictPayload = Object.assign({}, launchPayload, { promptText: "Changed prompt text!" });
      const conflictResp = await chrome.runtime.sendMessage(conflictPayload);
      const conflictOk = conflictResp && conflictResp.status === "error" && conflictResp.code === "payload_conflict";
      logResult("launch_payload_conflict", conflictOk, conflictResp);
      results.steps.push({ step: "launch_payload_conflict", pass: conflictOk });

      // Step 7e: Recover summaries verification
      const recoverResp = await chrome.runtime.sendMessage({ action: "recover" });
      const recoverOk = recoverResp && recoverResp.status === "ok" && Array.isArray(recoverResp.summaries) && recoverResp.summaries.length >= 1;
      logResult("recover_launch_summaries", recoverOk, recoverResp);
      results.steps.push({ step: "recover_launch_summaries", pass: recoverOk });
      // Step 8: Security: Unsupported operations fail closed
      const unknownOpResp = await sendNative({
        op: "shell_exec",
        command: "whoami"
      });
      const unknownOpRejected = unknownOpResp && unknownOpResp.status === "error" && unknownOpResp.code === "unsupported_operation";
      logResult("reject_unsupported_operation", unknownOpRejected, unknownOpResp);
      results.steps.push({ step: "reject_unsupported_operation", pass: unknownOpRejected });

      results.pairingId = activePairingId;
      results.pairingSecret = activePairingSecret;

    } else if (mode === "profile_beta") {
      // Step 9: N4 Profile Isolation: Profile B attempts to connect using Profile A's pairing
      const crossProfileResp = await sendNative({
        op: "connect",
        pairingId,
        pairingSecret,
        profileId: "profile_beta"
      });
      const crossProfileRejected = crossProfileResp && crossProfileResp.status === "error" && crossProfileResp.code === "profile_mismatch";
      logResult("profile_isolation_rejection", crossProfileRejected, crossProfileResp);
      results.steps.push({ step: "profile_isolation_rejection", pass: crossProfileRejected });

      const crossStatusResp = await sendNative({
        op: "status",
        pairingId,
        pairingSecret,
        profileId: "profile_beta"
      });
      const crossStatusRejected = crossStatusResp && crossStatusResp.status === "error" && crossStatusResp.code === "profile_mismatch";
      logResult("profile_isolation_status", crossStatusRejected, crossStatusResp);
      results.steps.push({ step: "profile_isolation_status", pass: crossStatusRejected });

    } else if (mode === "revoke") {
      // Step 10: Revoke pairing from Profile Alpha
      const revokeResp = await sendNative({
        op: "revoke",
        pairingId,
        pairingSecret,
        profileId: "profile_alpha"
      });
      const revokeOk = revokeResp && revokeResp.status === "ok" && revokeResp.pairingStatus === "revoked";
      logResult("revoke_pairing", revokeOk, revokeResp);
      results.steps.push({ step: "revoke_pairing", pass: revokeOk });

      // Step 11: Retired pairing rejected on subsequent connect
      const connectAfterRevokeResp = await sendNative({
        op: "connect",
        pairingId,
        pairingSecret,
        profileId: "profile_alpha"
      });
      const retiredRejected = connectAfterRevokeResp && connectAfterRevokeResp.status === "error" && connectAfterRevokeResp.code === "pairing_retired";
      logResult("reject_retired_pairing", retiredRejected, connectAfterRevokeResp);
      results.steps.push({ step: "reject_retired_pairing", pass: retiredRejected });
    }

    if (statusEl) {
      statusEl.textContent = "SUCCESS";
    }
    document.title = "TESTS_FINISHED_SUCCESS";
    window.__TEST_RESULTS__ = { success: true, results };
    return window.__TEST_RESULTS__;
  } catch (err) {
    if (statusEl) {
      statusEl.textContent = "ERROR: " + err.message;
    }
    document.title = "TESTS_FINISHED_ERROR";
    window.__TEST_RESULTS__ = { success: false, error: err.message, results };
    return window.__TEST_RESULTS__;
  }
};
