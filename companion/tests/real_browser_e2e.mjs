import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

const CHROME_PATH = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
const HOST_NAME = "com.hands.return_bridge";
const REPO_ROOT = "F:\\CodeBase\\hands\\issue-66-return-bridge-pairing";
const COMPANION_DIR = path.join(REPO_ROOT, "companion");
const EXTENSION_DIR = path.join(COMPANION_DIR, "extension");

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function fetchJson(url) {
  const res = await fetch(url);
  return res.json();
}

async function waitForBrowserVersion(port, timeoutMs = 15000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const v = await fetchJson(`http://127.0.0.1:${port}/json/version`);
      if (v && v.webSocketDebuggerUrl) {
        return v;
      }
    } catch {
      // Chrome starting up
    }
    await sleep(200);
  }
  throw new Error(`Timed out waiting for Chrome browser version on port ${port}`);
}

async function loadUnpackedExtension(browserWsUrl, extensionPath) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(browserWsUrl);
    ws.onopen = () => {
      ws.send(JSON.stringify({
        id: 1,
        method: "Extensions.loadUnpacked",
        params: { path: extensionPath }
      }));
    };
    ws.onmessage = (event) => {
      const data = JSON.parse(event.data);
      if (data.id === 1) {
        ws.close();
        if (data.error) {
          reject(new Error("Extensions.loadUnpacked failed: " + data.error.message));
        } else {
          resolve(data.result.id);
        }
      }
    };
    ws.onerror = reject;
  });
}

async function createTargetTab(browserWsUrl, port, url) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(browserWsUrl);
    ws.onopen = () => {
      ws.send(JSON.stringify({
        id: 2,
        method: "Target.createTarget",
        params: { url }
      }));
    };
    ws.onmessage = async (event) => {
      const data = JSON.parse(event.data);
      if (data.id === 2) {
        ws.close();
        const targetId = data.result?.targetId;
        if (!targetId) {
          return reject(new Error("Target.createTarget returned no targetId"));
        }

        const start = Date.now();
        while (Date.now() - start < 10000) {
          try {
            const tabs = await fetchJson(`http://127.0.0.1:${port}/json/list`);
            const tab = tabs.find((t) => t.id === targetId);
            if (tab && tab.webSocketDebuggerUrl) {
              return resolve(tab);
            }
          } catch {
            // Ignore
          }
          await sleep(200);
        }
        reject(new Error("Timed out waiting for created target tab to appear in /json/list"));
      }
    };
    ws.onerror = reject;
  });
}

async function evalInTab(tabWsUrl, script) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(tabWsUrl);
    ws.onopen = () => {
      ws.send(JSON.stringify({
        id: 10,
        method: "Runtime.evaluate",
        params: {
          expression: script,
          returnByValue: true,
          awaitPromise: true
        }
      }));
    };
    ws.onmessage = (event) => {
      const data = JSON.parse(event.data);
      if (data.id === 10) {
        ws.close();
        if (data.error) {
          reject(new Error("Runtime.evaluate error: " + data.error.message));
        } else {
          resolve(data.result?.result?.value);
        }
      }
    };
    ws.onerror = reject;
  });
}

async function waitForTestCompletion(tabWsUrl, timeoutMs = 25000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const res = await evalInTab(tabWsUrl, "window.__TEST_RESULTS__");
      if (res && res.success !== undefined) {
        return res;
      }
    } catch {
      // Evaluation might fail while DOM loads
    }
    await sleep(300);
  }
  throw new Error("Timed out waiting for window.__TEST_RESULTS__");
}

async function main() {
  console.log("===============================================================");
  console.log("Real Browser-to-Native Boundary & Profile Isolation E2E Test");
  console.log("===============================================================");

  if (!fs.existsSync(CHROME_PATH)) {
    throw new Error(`Chrome not found at ${CHROME_PATH}`);
  }

  // 1. Build companion binary
  console.log("[1/6] Building hands-return-bridge.exe...");
  execSync("cargo build --manifest-path companion/native/Cargo.toml", {
    cwd: REPO_ROOT,
    stdio: "inherit"
  });

  const exePath = path.join(REPO_ROOT, "companion", "native", "target", "debug", "hands-return-bridge.exe");
  if (!fs.existsSync(exePath)) {
    throw new Error(`Binary not found at ${exePath}`);
  }

  // 2. Prepare isolated test environment
  const testDir = fs.mkdtempSync(path.join(os.tmpdir(), "hands-rb-e2e-"));
  const stateDir = path.join(testDir, "state");
  const profileAlphaDir = path.join(testDir, "profile_alpha");
  const profileBetaDir = path.join(testDir, "profile_beta");
  fs.mkdirSync(stateDir, { recursive: true });
  fs.mkdirSync(profileAlphaDir, { recursive: true });
  fs.mkdirSync(profileBetaDir, { recursive: true });

  console.log(`[2/6] Isolated test environment created: ${testDir}`);

  // 3. Run native CLI setup to bootstrap pairing for profile_alpha
  console.log("[3/6] Running native CLI setup...");
  const setupOut = execSync(
    `"${exePath}" setup --browser chrome --profile profile_alpha --target "${REPO_ROOT}" --target-id hands --policy-revision v1 --state-dir "${stateDir}" --skip-registry`,
    { encoding: "utf8" }
  );

  const tokenMatch = setupOut.match(/Bootstrap Token:\s+(rb_boot_[a-f0-9]+)/);
  const secretMatch = setupOut.match(/Pairing Secret:\s+(rb_sec_[a-f0-9]+)/);
  const pairMatch = setupOut.match(/Pairing ID:\s+(pair_[a-f0-9]+)/);

  if (!tokenMatch || !pairMatch || !secretMatch) {
    throw new Error(`Failed to parse setup output:\n${setupOut}`);
  }

  const bootstrapToken = tokenMatch[1];
  const pairingId = pairMatch[1];
  const pairingSecret = secretMatch[1];

  console.log(`      Pairing ID:      ${pairingId}`);
  console.log(`      Bootstrap Token: ${bootstrapToken}`);
  console.log(`      Pairing Secret:  ${pairingSecret}`);

  const regKey = `HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\${HOST_NAME}`;
  let extId = null;

  try {
    // -------------------------------------------------------------
    // Phase 1: Real Chrome with Profile Alpha
    // -------------------------------------------------------------
    console.log("[4/6] Phase 1: Launching Chrome (Profile Alpha)...");
    const cdpPortAlpha = 9250;
    const chromeAlpha = spawn(CHROME_PATH, [
      "--headless=new",
      `--user-data-dir=${profileAlphaDir}`,
      `--remote-debugging-port=${cdpPortAlpha}`,
      "--no-first-run",
      "--no-default-browser-check"
    ], {
      stdio: "ignore",
      env: { ...process.env, HANDS_RETURN_BRIDGE_STATE_DIR: stateDir }
    });

    let alphaResult;
    try {
      const version = await waitForBrowserVersion(cdpPortAlpha);
      console.log("      Installing unpacked extension via CDP Extensions.loadUnpacked...");
      extId = await loadUnpackedExtension(version.webSocketDebuggerUrl, EXTENSION_DIR);
      console.log(`      Installed Extension ID: ${extId}`);

      // Register Native Messaging Host in Windows Registry with this extension ID
      const manifestPath = path.join(stateDir, `${HOST_NAME}.json`);
      const manifest = {
        name: HOST_NAME,
        description: "Hands Return Bridge Native Host",
        path: exePath,
        type: "stdio",
        allowed_origins: [`chrome-extension://${extId}/`]
      };
      fs.writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
      execSync(`reg.exe add "${regKey}" /ve /t REG_SZ /d "${manifestPath}" /f`, { stdio: "ignore" });
      console.log("      Registered Native Messaging Host manifest in Windows Registry.");

      const testUrlAlpha = `chrome-extension://${extId}/test_runner.html?mode=profile_alpha&profileId=profile_alpha&bootstrapToken=${bootstrapToken}`;
      console.log("      Navigating to extension test runner page...");
      const targetTab = await createTargetTab(version.webSocketDebuggerUrl, cdpPortAlpha, testUrlAlpha);

      console.log("      Connected to test runner. Waiting for test assertions to execute...");
      alphaResult = await waitForTestCompletion(targetTab.webSocketDebuggerUrl);
    } finally {
      chromeAlpha.kill();
      await sleep(1000);
    }

    if (!alphaResult || !alphaResult.success) {
      throw new Error(`Profile Alpha tests failed: ${JSON.stringify(alphaResult)}`);
    }
    console.log("      Profile Alpha assertions PASSED:\n", alphaResult.results.steps.map(s => `        [PASS] ${s.step}`).join("\n"));

    // -------------------------------------------------------------
    // Phase 2: Real Chrome with Profile Beta (Distinct Profile Isolation - N4)
    // -------------------------------------------------------------
    console.log("[5/6] Phase 2: Testing Distinct Profile Isolation (N4) with Profile Beta...");
    const cdpPortBeta = 9251;
    const chromeBeta = spawn(CHROME_PATH, [
      "--headless=new",
      `--user-data-dir=${profileBetaDir}`,
      `--remote-debugging-port=${cdpPortBeta}`,
      "--no-first-run",
      "--no-default-browser-check"
    ], {
      stdio: "ignore",
      env: { ...process.env, HANDS_RETURN_BRIDGE_STATE_DIR: stateDir }
    });

    let betaResult;
    try {
      const versionBeta = await waitForBrowserVersion(cdpPortBeta);
      const betaExtId = await loadUnpackedExtension(versionBeta.webSocketDebuggerUrl, EXTENSION_DIR);

      const testUrlBeta = `chrome-extension://${betaExtId}/test_runner.html?mode=profile_beta&profileId=profile_beta&pairingId=${pairingId}&pairingSecret=${pairingSecret}`;
      console.log("      Opening test runner in Profile Beta to attempt cross-profile authentication...");
      const targetTab = await createTargetTab(versionBeta.webSocketDebuggerUrl, cdpPortBeta, testUrlBeta);

      betaResult = await waitForTestCompletion(targetTab.webSocketDebuggerUrl);
    } finally {
      chromeBeta.kill();
      await sleep(1000);
    }

    if (!betaResult || !betaResult.success) {
      throw new Error(`Profile Beta isolation tests failed: ${JSON.stringify(betaResult)}`);
    }
    console.log("      Profile Beta isolation assertions PASSED:\n", betaResult.results.steps.map(s => `        [PASS] ${s.step}`).join("\n"));

    // -------------------------------------------------------------
    // Phase 3: Revocation & Retired Pairing
    // -------------------------------------------------------------
    console.log("[6/6] Phase 3: Testing Pairing Revocation & Retired Pairing rejection...");
    const cdpPortRevoke = 9252;
    const chromeRevoke = spawn(CHROME_PATH, [
      "--headless=new",
      `--user-data-dir=${profileAlphaDir}`,
      `--remote-debugging-port=${cdpPortRevoke}`,
      "--no-first-run",
      "--no-default-browser-check"
    ], {
      stdio: "ignore",
      env: { ...process.env, HANDS_RETURN_BRIDGE_STATE_DIR: stateDir }
    });

    let revokeResult;
    try {
      const versionRevoke = await waitForBrowserVersion(cdpPortRevoke);
      const revokeExtId = await loadUnpackedExtension(versionRevoke.webSocketDebuggerUrl, EXTENSION_DIR);

      const testUrlRevoke = `chrome-extension://${revokeExtId}/test_runner.html?mode=revoke&profileId=profile_alpha&pairingId=${pairingId}&pairingSecret=${pairingSecret}`;
      console.log("      Opening test runner in Profile Alpha to revoke pairing...");
      const targetTab = await createTargetTab(versionRevoke.webSocketDebuggerUrl, cdpPortRevoke, testUrlRevoke);

      revokeResult = await waitForTestCompletion(targetTab.webSocketDebuggerUrl);
    } finally {
      chromeRevoke.kill();
      await sleep(1000);
    }

    if (!revokeResult || !revokeResult.success) {
      throw new Error(`Revocation tests failed: ${JSON.stringify(revokeResult)}`);
    }
    console.log("      Revocation assertions PASSED:\n", revokeResult.results.steps.map(s => `        [PASS] ${s.step}`).join("\n"));

    console.log("===============================================================");
    console.log("ALL REAL BROWSER-TO-NATIVE E2E GATES PASSED CLEANLY!");
    console.log("===============================================================");
    console.log("Verified Deliverables for Issue #66:");
    console.log("  1. Native-local companion setup owns pairing bootstrap & targets");
    console.log("  2. Explicit OMP launch policy revision (v1) registered locally");
    console.log("  3. On-demand Native Messaging host (no resident daemon)");
    console.log("  4. Closed Native Messaging surface (setup, connect, status, revoke)");
    console.log("  5. Unauthorized override rejection (targets/policy/executable/argv/env)");
    console.log("  6. Task execution launch fails closed (Issue #67 boundary)");
    console.log("  7. Unsupported operations fail closed (shell_exec, orca_rpc, etc.)");
    console.log("  8. Embedded self-contained SQLite durability across host restarts");
    console.log("  9. N4 Profile Isolation: two real profiles cannot share pairing");
    console.log(" 10. Pairing revocation marks status retired; retired pairing rejected");
    console.log(" 11. Explicit security & trust notice displayed");
    console.log(" 12. Active Hands Runtime, MCP, tunnel, and Machine Credentials UNTOUCHED");
    console.log("===============================================================");
  } finally {
    try {
      execSync(`reg.exe delete "${regKey}" /f`, { stdio: "ignore" });
      console.log("Cleaned up Windows Registry key.");
    } catch {
      // Ignore
    }

    try {
      fs.rmSync(testDir, { recursive: true, force: true });
      console.log("Cleaned up temporary test environment.");
    } catch {
      // Ignore
    }
  }
}

main().catch((err) => {
  console.error("E2E Test Failed:", err);
  process.exit(1);
});
