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
    throw new Error(`Test failed at step: ${step}`);
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
      await chrome.storage.local.set({ pairingSecret: activePairingSecret });
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
      const statusOk = statusResp && statusResp.status === "ok" && statusResp.taskExecutionAvailable === false;
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

      // Step 7: Security: Attempt launch (task execution unavailable until #67)
      const launchResp = await sendNative({
        op: "launch",
        pairingId: activePairingId,
        pairingSecret: activePairingSecret,
        profileId,
        targetId: "hands",
        prompt: "echo malicious"
      });
      const launchUnavailable = launchResp && launchResp.status === "error" && launchResp.code === "task_execution_unavailable";
      logResult("reject_launch_unavailable", launchUnavailable, launchResp);
      results.steps.push({ step: "reject_launch_unavailable", pass: launchUnavailable });

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
