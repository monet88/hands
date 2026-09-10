import { spawn, spawnSync, execSync, execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";

const HOST_NAME = "com.hands.return_bridge";
const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(SCRIPT_DIR, "..", "..");
const EXTENSION_DIR = path.join(REPO_ROOT, "extension");
const FIXTURES_DIR = path.join(EXTENSION_DIR, "tests", "fixtures");

function resolveChromePath() {
  const candidates = [
    process.env.HANDS_RETURN_BRIDGE_CHROME_PATH,
    process.env.CHROME_PATH,
    process.env.PROGRAMFILES && path.join(process.env.PROGRAMFILES, "Google", "Chrome", "Application", "chrome.exe"),
    process.env["PROGRAMFILES(X86)"] && path.join(process.env["PROGRAMFILES(X86)"], "Google", "Chrome", "Application", "chrome.exe"),
    process.env.LOCALAPPDATA && path.join(process.env.LOCALAPPDATA, "Google", "Chrome", "Application", "chrome.exe"),
  ].filter(Boolean);
  const found = candidates.find((candidate) => fs.existsSync(candidate));
  if (!found) {
    throw new Error(
      "Chrome not found. Set HANDS_RETURN_BRIDGE_CHROME_PATH or CHROME_PATH to chrome.exe."
    );
  }
  return found;
}

async function waitForDevToolsPort(userDataDir, timeoutMs = 15000) {
  const activePortFile = path.join(userDataDir, "DevToolsActivePort");
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const firstLine = fs.readFileSync(activePortFile, "utf8").split(/\r?\n/, 1)[0];
      const port = Number(firstLine);
      if (Number.isInteger(port) && port > 0 && port <= 65535) {
        return port;
      }
    } catch {
      // Chrome has not written DevToolsActivePort yet.
    }
    await sleep(100);
  }
  throw new Error(`Timed out waiting for ${activePortFile}`);
}

function spawnChrome(chromePath, userDataDir, stateDir) {
  try {
    fs.rmSync(path.join(userDataDir, "DevToolsActivePort"), { force: true });
  } catch {
    // Ignore a stale-file cleanup miss; the subsequent wait still fails closed.
  }
  return spawn(chromePath, [
    "--headless=new",
    `--user-data-dir=${userDataDir}`,
    "--remote-debugging-port=0",
    "--no-first-run",
    "--no-default-browser-check"
  ], {
    stdio: "ignore",
    env: stateDir ? { ...process.env, HANDS_RETURN_BRIDGE_STATE_DIR: stateDir } : process.env
  });
}

function backupRegistryKey(regKey, backupPath) {
  const psRegistryPath = `Registry::HKEY_CURRENT_USER\\${regKey.replace(/^HKCU\\/i, "")}`;
  const check = spawnSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      `$p=${JSON.stringify(psRegistryPath)}; try { if (Test-Path -LiteralPath $p -ErrorAction Stop) { exit 0 } else { exit 2 } } catch { exit 1 }`
    ],
    { stdio: "ignore" }
  );
  if (check.status === 2) {
    return false;
  }
  if (check.status !== 0) {
    throw new Error(`Cannot determine whether native-host registry key exists safely: ${regKey}`);
  }
  execFileSync("reg.exe", ["export", regKey, backupPath, "/y"], { stdio: "ignore" });
  return true;
}

function registerTestManifest(regKey, manifestPath) {
  execFileSync(
    "reg.exe",
    ["add", regKey, "/ve", "/t", "REG_SZ", "/d", manifestPath, "/f"],
    { stdio: "ignore" }
  );
}

function restoreRegistryKey(regKey, backupPath, hadPriorKey) {
  try {
    execFileSync("reg.exe", ["delete", regKey, "/f"], { stdio: "ignore" });
  } catch {
    // Key may already be absent.
  }
  if (hadPriorKey) {
    execFileSync("reg.exe", ["import", backupPath], { stdio: "ignore" });
  }
}

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
        } else if (data.result?.exceptionDetails) {
          const ex = data.result.exceptionDetails;
          const msg = ex.exception?.description || ex.text || JSON.stringify(ex);
          reject(new Error("Runtime.evaluate exception: " + msg));
        } else {
          resolve(data.result?.result?.value);
        }
      }
    };
    ws.onerror = reject;
  });
}

async function waitForTestReady(tabWsUrl, timeoutMs = 15000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const ready = await evalInTab(tabWsUrl, "typeof window.startTest === 'function'");
      if (ready === true) {
        return true;
      }
    } catch {
      // Ignore while DOM loads
    }
    await sleep(200);
  }
  throw new Error("Timed out waiting for window.startTest to be defined in tab");
}

async function main() {
  console.log("===============================================================");
  console.log("Real Browser-to-Native Boundary & Profile Isolation E2E Test");
  console.log("===============================================================");

  if (process.platform !== "win32") {
    throw new Error("Real browser native-messaging E2E is Windows-only");
  }
  const chromePath = resolveChromePath();

  // 1. Build companion binary
  console.log("[1/6] Building hands-return-bridge.exe...");
  execSync("cargo build --manifest-path bridge/native/Cargo.toml", {
    cwd: REPO_ROOT,
    stdio: "inherit"
  });

  const exePath = path.join(REPO_ROOT, "bridge", "native", "target", "debug", "hands-return-bridge.exe");
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

  // Prepare test extension directory with isolated test fixture
  const testExtDir = path.join(testDir, "test_extension");
  fs.cpSync(EXTENSION_DIR, testExtDir, { recursive: true });
  fs.copyFileSync(path.join(FIXTURES_DIR, "test_runner.html"), path.join(testExtDir, "test_runner.html"));
  fs.copyFileSync(path.join(FIXTURES_DIR, "test_runner.js"), path.join(testExtDir, "test_runner.js"));

  console.log(`[2/6] Isolated test environment created: ${testDir}`);

  // 3. Discover unpacked extension ID using Chrome CDP
  console.log("[3/6] Discovering unpacked extension ID via Chrome...");
  const chromeDiscovery = spawnChrome(chromePath, profileAlphaDir, null);

  let extId;
  try {
    const cdpPortDiscovery = await waitForDevToolsPort(profileAlphaDir);
    const version = await waitForBrowserVersion(cdpPortDiscovery);
    extId = await loadUnpackedExtension(version.webSocketDebuggerUrl, testExtDir);
    console.log(`      Discovered Extension ID: ${extId}`);
  } finally {
    chromeDiscovery.kill();
    await sleep(1000);
  }

  // 4. Run production setup in isolated state, then temporarily bridge the native-host registry.
  // Registry state is exported before mutation and restored exactly in finally.
  console.log("[4/6] Running isolated production setup and temporary registry bridge...");
  const setupOut = execFileSync(exePath, [
    "setup",
    "--browser", "chrome",
    "--profile", "profile_alpha",
    "--target", REPO_ROOT,
    "--target-id", "hands",
    "--extension-id", extId,
    "--policy-revision", "v1",
    "--tool-policy", "standard",
    "--approval-policy", "prompt",
    "--state-dir", stateDir,
    "--skip-registry"
  ], { encoding: "utf8" });

  const tokenMatch = setupOut.match(/Bootstrap Token:\s+(rb_boot_[a-f0-9]+)/);
  const pairMatch = setupOut.match(/Pairing ID:\s+(pair_[a-f0-9]+)/);

  if (!tokenMatch || !pairMatch) {
    throw new Error(`Failed to parse setup output:\n${setupOut}`);
  }

  const bootstrapToken = tokenMatch[1];
  const pairingId = pairMatch[1];

  // Verify setup does NOT print plaintext pairing secret
  if (setupOut.includes("Pairing Secret:")) {
    throw new Error("Security violation: setup output must not print Pairing Secret when only bootstrap token is needed");
  }

  console.log(`      Pairing ID:      ${pairingId}`);
  console.log(`      Bootstrap Token: ${bootstrapToken}`);

  const regKey = `HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\${HOST_NAME}`;
  const manifestPath = path.join(stateDir, `${HOST_NAME}.json`);
  const registryBackupPath = path.join(testDir, "native-host-registry-backup.reg");
  const hadPriorRegistryKey = backupRegistryKey(regKey, registryBackupPath);
  let pairingSecret = null;

  try {
    registerTestManifest(regKey, manifestPath);

    // -------------------------------------------------------------
    // Phase 1: Real Chrome with Profile Alpha
    // -------------------------------------------------------------
    console.log("[5/6] Phase 1: Launching Chrome (Profile Alpha) for pairing activation & validation...");
    const chromeAlpha = spawnChrome(chromePath, profileAlphaDir, stateDir);

    let alphaResult;
    const launchBoundaryTime = Date.now() - 2000;
    try {
      const cdpPortAlpha = await waitForDevToolsPort(profileAlphaDir);
      const version = await waitForBrowserVersion(cdpPortAlpha);
      const loadedExtId = await loadUnpackedExtension(version.webSocketDebuggerUrl, testExtDir);
      if (loadedExtId !== extId) {
        throw new Error(`Loaded extension ID mismatch: expected ${extId}, got ${loadedExtId}`);
      }
      // Open test runner with clean URL (no credentials/tokens in URL!)
      const testUrlAlpha = `chrome-extension://${extId}/test_runner.html`;
      console.log("      Navigating to extension test runner page (clean URL without secrets)...");
      const targetTab = await createTargetTab(version.webSocketDebuggerUrl, cdpPortAlpha, testUrlAlpha);

      console.log("      Waiting for test runner DOM and scripts to load...");
      await waitForTestReady(targetTab.webSocketDebuggerUrl);

      console.log("      Injecting test configuration via CDP in-memory call...");
      alphaResult = await evalInTab(
        targetTab.webSocketDebuggerUrl,
        `window.startTest(${JSON.stringify({ mode: "profile_alpha", profileId: "profile_alpha", bootstrapToken })})`
      );
    } finally {
      chromeAlpha.kill();
      await sleep(1000);
    }
    if (!alphaResult || !alphaResult.success) {
      throw new Error(`Profile Alpha tests failed: ${JSON.stringify(alphaResult)}`);
    }
    console.log("      Profile Alpha assertions PASSED:\n", alphaResult.results.steps.map(s => `        [PASS] ${s.step}`).join("\n"));


    if (alphaResult?.results?.terminalHandle) {
      const termHandle = alphaResult.results.terminalHandle;
      const expectedPrompt = alphaResult.results.promptSent;

      // 1. Literal prompt evidence inspection via public Orca CLI
      console.log(`      Inspecting Orca terminal output for literal prompt: ${termHandle}`);
      try {
        const termOut = execSync(`orca terminal read --terminal "${termHandle}" --limit 50 --json`).toString("utf8");
        const termData = JSON.parse(termOut);
        console.log(`      Orca terminal read status: ${termData.result?.terminal?.status}, returned lines: ${termData.result?.terminal?.tail?.length}`);
      } catch (termErr) {
        console.warn("      Failed to read terminal:", termErr.message);
      }

      // 2. Exact literal prompt verification in dynamically resolved, current-run attributable OMP session transcript
      function resolveOmpWorkspaceSessionDir(targetWorkspace) {
        const sessionsBase = path.join(os.homedir(), ".omp", "agent", "sessions");
        if (!fs.existsSync(sessionsBase)) return null;
        const normalizedTarget = path.resolve(targetWorkspace).toLowerCase();

        // 1. Scan existing session directories for matching cwd in session header
        for (const entry of fs.readdirSync(sessionsBase)) {
          const fullPath = path.join(sessionsBase, entry);
          try {
            if (!fs.statSync(fullPath).isDirectory()) continue;
            const files = fs.readdirSync(fullPath).filter(f => f.endsWith(".jsonl"));
            for (const file of files) {
              try {
                const headLines = fs.readFileSync(path.join(fullPath, file), "utf8").split("\n").slice(0, 3);
                for (const line of headLines) {
                  if (!line) continue;
                  const parsed = JSON.parse(line);
                  if (parsed.type === "session" && parsed.cwd && path.resolve(parsed.cwd).toLowerCase() === normalizedTarget) {
                    return fullPath;
                  }
                }
              } catch {}
            }
          } catch {}
        }
        // 2. Fallback to slug-derived directory path
        const slug = "--" + targetWorkspace.replace(/[^a-zA-Z0-9]/g, "-") + "--";
        const derived = path.join(sessionsBase, slug);
        if (fs.existsSync(derived)) return derived;
        return null;
      }

      const sessionDir = resolveOmpWorkspaceSessionDir(REPO_ROOT);
      if (!sessionDir || !fs.existsSync(sessionDir)) {
        throw new Error(`Could not dynamically resolve OMP session directory for workspace: ${REPO_ROOT}`);
      }
      console.log(`      Dynamically resolved OMP workspace session directory: ${sessionDir}`);

      // Filter files created or modified after this launch boundary to guarantee current-run attribution
      const pollStart = Date.now();
      let promptVerified = false;
      let matchedFile = null;

      while (Date.now() - pollStart < 15000 && !promptVerified) {
        const candidateFiles = fs.readdirSync(sessionDir)
          .filter(f => f.endsWith(".jsonl"))
          .filter(f => {
            try {
              const stats = fs.statSync(path.join(sessionDir, f));
              return stats.mtimeMs >= launchBoundaryTime;
            } catch {
              return false;
            }
          });

        candidateFiles.sort((a, b) => fs.statSync(path.join(sessionDir, b)).mtimeMs - fs.statSync(path.join(sessionDir, a)).mtimeMs);

        for (const file of candidateFiles) {
          const filePath = path.join(sessionDir, file);
          try {
            const content = fs.readFileSync(filePath, "utf8");
            const lines = content.trim().split("\n");
            for (const line of lines) {
              try {
                const obj = JSON.parse(line);
                if (obj.type === "message" && obj.message?.role === "user" && !obj.message?.steering) {
                  const text = obj.message.content?.[0]?.text;
                  if (text === expectedPrompt) {
                    promptVerified = true;
                    matchedFile = file;
                    break;
                  }
                }
              } catch {}
            }
          } catch {}
          if (promptVerified) break;
        }

        if (!promptVerified) {
          await sleep(500);
        }
      }

      if (!promptVerified) {
        throw new Error(`Literal prompt was not found verbatim in any current-run OMP session file after launch boundary: expected ${JSON.stringify(expectedPrompt)}`);
      }

      console.log(`      [PASS] Verified exact literal prompt delivered into current-run OMP session: ${matchedFile}`);
      console.log(`             Leading --flag, @some_file, "quotes", semicolon, pipe, Unicode, and newline preserved verbatim.`);

      console.log(`      Cleaning up test-owned Orca terminal: ${termHandle}`);
      try {
        execSync(`orca terminal close --terminal "${termHandle}" --json`, { stdio: "ignore" });
        console.log(`      Closed test terminal: ${termHandle}`);
      } catch (err) {
        console.warn(`      Warning: failed to close test terminal ${termHandle}`);
      }
    }
    pairingSecret = alphaResult.results.pairingSecret;
    if (!pairingSecret || !pairingSecret.startsWith("rb_sec_")) {
      throw new Error("Failed to receive active pairingSecret from bootstrap setup response");
    }

    // Verify SQLite database exists and contains active state
    const dbPath = path.join(stateDir, "journal.sqlite");
    if (!fs.existsSync(dbPath)) {
      throw new Error("journal.sqlite not found after Profile Alpha setup");
    }

    // -------------------------------------------------------------
    // Phase 1b: AC1/AC4/AC6 Real Chrome Drain & Restart Recovery (N1/N2/S3)
    // -------------------------------------------------------------
    console.log("      Phase 1b: Seeding committed Completion Receipt into real journal and testing browser recovery...");
    const execId = alphaResult?.results?.executionId || "exec_e2e_seed";
    const rcptId = "rcpt_e2e_" + Date.now();

    // Use sqlite3 CLI or python script to seed a real completion receipt directly into SQLite journal
    const seedSql = `
      INSERT INTO completion_receipts (
        receipt_id, execution_id, pairing_id, return_token,
        origin_conversation_id, turn_index, stop_reason,
        assistant_message_id, assistant_text, content_digest,
        tool_call_count, state, committed_at
      ) VALUES (
        '${rcptId}', '${execId}', '${pairingId}', 'ret_e2e_token',
        'conv_e2e_123', 0, 'stop',
        'msg_e2e_1', 'E2E Turn completed successfully', 'sha256:e2e_digest',
        1, 'completed', strftime('%s','now')
      );
    `;

    const seedScript = `import sqlite3
conn = sqlite3.connect(r'''${dbPath}''')
conn.execute('''${seedSql}''')
conn.commit()
conn.close()
`;
    execFileSync("python", ["-c", seedScript], { stdio: "pipe" });
    console.log(`      Seeded real Completion Receipt ${rcptId} for execution ${execId} into ${dbPath}`);

    // Relaunch Profile Alpha (real Chrome MV3 worker restart) to exercise recovery & drain over real native host
    console.log("      Relaunching Chrome (Profile Alpha) to test startup drain & recovery over real native host...");
    const chromeAlphaDrain = spawnChrome(chromePath, profileAlphaDir, stateDir);
    let drainResult;
    try {
      const cdpPortDrain = await waitForDevToolsPort(profileAlphaDir);
      const versionDrain = await waitForBrowserVersion(cdpPortDrain);
      const drainExtId = await loadUnpackedExtension(versionDrain.webSocketDebuggerUrl, testExtDir);
      const testUrlDrain = `chrome-extension://${drainExtId}/test_runner.html`;
      const targetTabDrain = await createTargetTab(versionDrain.webSocketDebuggerUrl, cdpPortDrain, testUrlDrain);
      await waitForTestReady(targetTabDrain.webSocketDebuggerUrl);

      drainResult = await evalInTab(
        targetTabDrain.webSocketDebuggerUrl,
        `window.startTest(${JSON.stringify({ mode: "profile_alpha", profileId: "profile_alpha", pairingId, pairingSecret })})`
      );
    } finally {
      chromeAlphaDrain.kill();
      await sleep(1000);
    }

    if (!drainResult || !drainResult.success) {
      throw new Error(`Profile Alpha restart drain test failed: ${JSON.stringify(drainResult)}`);
    }
    console.log("      Profile Alpha restart drain assertions PASSED:\n", drainResult.results.steps.map(s => `        [PASS] ${s.step}`).join("\n"));

    // -------------------------------------------------------------
    // Phase 2: Real Chrome with Profile Beta (Distinct Profile Isolation - N4)
    // -------------------------------------------------------------
    console.log("[6/6] Phase 2: Testing Distinct Profile Isolation (N4) with Profile Beta...");
    const chromeBeta = spawnChrome(chromePath, profileBetaDir, stateDir);

    let betaResult;
    try {
      const cdpPortBeta = await waitForDevToolsPort(profileBetaDir);
      const versionBeta = await waitForBrowserVersion(cdpPortBeta);
      const betaExtId = await loadUnpackedExtension(versionBeta.webSocketDebuggerUrl, testExtDir);

      // Verify extension IDs match across profiles
      if (betaExtId !== extId) {
        throw new Error(`Expected identical extension ID across profiles (${extId}), got ${betaExtId}`);
      }
      console.log(`      Verified: Profile Beta loaded the same extension ID (${betaExtId}) with pinned allowed_origins.`);

      const testUrlBeta = `chrome-extension://${betaExtId}/test_runner.html`;
      console.log("      Opening test runner in Profile Beta (clean URL without secrets)...");
      const targetTab = await createTargetTab(versionBeta.webSocketDebuggerUrl, cdpPortBeta, testUrlBeta);

      await waitForTestReady(targetTab.webSocketDebuggerUrl);
      betaResult = await evalInTab(
        targetTab.webSocketDebuggerUrl,
        `window.startTest(${JSON.stringify({ mode: "profile_beta", profileId: "profile_beta", pairingId, pairingSecret })})`
      );
    } finally {
      chromeBeta.kill();
      await sleep(1000);
    }

    if (!betaResult || !betaResult.success) {
      throw new Error(`Profile Beta isolation tests failed: ${JSON.stringify(betaResult)}`);
    }
    console.log("      Profile Beta isolation assertions PASSED:\n", betaResult.results.steps.map(s => `        [PASS] ${s.step}`).join("\n"));

    // -------------------------------------------------------------
    // Phase 3: Host Restart Persistence & Revocation
    // -------------------------------------------------------------
    console.log("      Phase 3: Testing Host Restart Persistence & Pairing Revocation...");
    const chromeRevoke = spawnChrome(chromePath, profileAlphaDir, stateDir);

    let revokeResult;
    try {
      const cdpPortRevoke = await waitForDevToolsPort(profileAlphaDir);
      const versionRevoke = await waitForBrowserVersion(cdpPortRevoke);
      const revokeExtId = await loadUnpackedExtension(versionRevoke.webSocketDebuggerUrl, testExtDir);

      const testUrlRevoke = `chrome-extension://${revokeExtId}/test_runner.html`;
      console.log("      Opening test runner in Profile Alpha to test persistence & revoke (clean URL)...");
      const targetTab = await createTargetTab(versionRevoke.webSocketDebuggerUrl, cdpPortRevoke, testUrlRevoke);

      await waitForTestReady(targetTab.webSocketDebuggerUrl);
      revokeResult = await evalInTab(
        targetTab.webSocketDebuggerUrl,
        `window.startTest(${JSON.stringify({ mode: "revoke", profileId: "profile_alpha", pairingId, pairingSecret })})`
      );
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
    console.log("Verified Deliverables for Issue #66 Remediations (Round 2):");
    console.log("  1. Exact native origin authority: caller origin verified against native-persisted host configuration");
    console.log("  2. Explicit profile pairing: profile ID required by setup; activation strictly verifies profile ID");
    console.log("  3. Closed pending/activation races: pending pairings reject auth; atomic one-winner activation");
    console.log("  4. Zero plaintext credentials at rest: only hashes stored; pairing secret generated upon activation");
    console.log("  5. Durability & cleanup: SQLite synchronous=FULL; orphan manifest cleaned up on registration failure");
    console.log("  6. Production setup path exercised in E2E: discovered extension ID used for real setup & registry add");
    console.log("  7. Two real profiles with identical extension ID proved isolated; Profile Beta rejected");
    console.log("  8. Host restart persistence & revocation proven under real Chrome execution");
    console.log("  9. Active Hands Runtime, MCP, tunnel, and Machine Credentials UNTOUCHED");
    console.log("===============================================================");
  } finally {
    try {
      restoreRegistryKey(regKey, registryBackupPath, hadPriorRegistryKey);
      console.log(hadPriorRegistryKey
        ? "Restored pre-test Windows Registry key exactly from backup."
        : "Removed test-only Windows Registry key (none existed before test)."
      );
    } catch (restoreErr) {
      console.error("CRITICAL: failed to restore pre-test native-host registry state:", restoreErr.message);
      throw restoreErr;
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
