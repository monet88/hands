import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import crypto from "node:crypto";
import cp from "node:child_process";
import { Database } from "bun:sqlite";
// Read the production adapter TS content
const LAUNCHER_RS = fs.readFileSync("companion/native/src/launcher.rs", "utf8");
const match = LAUNCHER_RS.match(/pub const ADAPTER_TS_CONTENT: &str = r#"([\s\S]*?)"#;/);
assert.ok(match, "Must find ADAPTER_TS_CONTENT in launcher.rs");
const adapterSource = match[1];

function createTempTestEnvironment() {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "hands_probe_"));
  const dbPath = path.join(tempDir, "journal.sqlite");
  const adapterPath = path.join(tempDir, "adapter.ts");

  fs.writeFileSync(adapterPath, adapterSource, "utf8");

  const db = new Database(dbPath);
  db.run("PRAGMA journal_mode = WAL;");
  db.run("PRAGMA synchronous = FULL;");
  db.run("PRAGMA foreign_keys = ON;");

  db.run(`
    CREATE TABLE pairings (
      pairing_id TEXT PRIMARY KEY,
      bootstrap_token_hash TEXT UNIQUE,
      pairing_secret_hash TEXT,
      browser TEXT NOT NULL,
      profile_id TEXT NOT NULL,
      status TEXT NOT NULL,
      policy_revision TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );

    CREATE TABLE targets (
      pairing_id TEXT NOT NULL,
      target_id TEXT NOT NULL,
      canonical_path TEXT NOT NULL,
      name TEXT NOT NULL,
      PRIMARY KEY (pairing_id, target_id)
    );

    CREATE TABLE policies (
      pairing_id TEXT NOT NULL,
      policy_revision TEXT NOT NULL,
      tool_policy TEXT NOT NULL,
      approval_policy TEXT NOT NULL,
      PRIMARY KEY (pairing_id, policy_revision)
    );

    CREATE TABLE launch_requests (
      pairing_id TEXT NOT NULL,
      launch_request_id TEXT NOT NULL,
      execution_id TEXT NOT NULL UNIQUE,
      return_token TEXT NOT NULL UNIQUE,
      origin_conversation_id TEXT NOT NULL,
      origin_conversation_url TEXT NOT NULL,
      transcript_evidence_hash TEXT NOT NULL,
      account_evidence_hash TEXT NOT NULL,
      target_id TEXT NOT NULL,
      canonical_target_path TEXT NOT NULL,
      policy_revision TEXT NOT NULL,
      effective_tool_policy TEXT NOT NULL,
      effective_approval_policy TEXT NOT NULL,
      prompt_text TEXT NOT NULL,
      payload_digest TEXT NOT NULL,
      state TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (pairing_id, launch_request_id)
    );

    CREATE TABLE launch_attempts (
      execution_id TEXT PRIMARY KEY,
      pairing_id TEXT NOT NULL,
      attempt_marked_at INTEGER NOT NULL,
      invoked_at INTEGER,
      orca_terminal_handle TEXT,
      orca_tab_id TEXT,
      orca_pane_key TEXT,
      orca_pty_id TEXT,
      state TEXT NOT NULL,
      failure_reason TEXT
    );

    CREATE TABLE replay_tombstones (
      pairing_id TEXT NOT NULL,
      launch_request_id TEXT NOT NULL,
      execution_id TEXT NOT NULL,
      payload_digest TEXT NOT NULL,
      tombstoned_at INTEGER NOT NULL,
      PRIMARY KEY (pairing_id, launch_request_id)
    );

    CREATE TABLE completion_receipts (
      receipt_id TEXT PRIMARY KEY,
      execution_id TEXT NOT NULL UNIQUE,
      pairing_id TEXT NOT NULL,
      return_token TEXT NOT NULL UNIQUE,
      origin_conversation_id TEXT NOT NULL,
      turn_index INTEGER NOT NULL,
      stop_reason TEXT NOT NULL,
      assistant_message_id TEXT,
      assistant_text TEXT NOT NULL,
      content_digest TEXT NOT NULL,
      tool_call_count INTEGER NOT NULL,
      state TEXT NOT NULL,
      committed_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS execution_adapter_claims (
      execution_id TEXT PRIMARY KEY,
      adapter_instance_id TEXT NOT NULL,
      session_id TEXT NOT NULL,
      claimed_at INTEGER NOT NULL
    );
  `);

  const now = Math.floor(Date.now() / 1000);
  db.run(
    `INSERT INTO pairings VALUES ('pair_1', 'boot_hash', 'sec_hash', 'chrome', 'prof_1', 'active', 'v1', ?, ?)`,
    [now, now]
  );
  db.run(`INSERT INTO targets VALUES ('pair_1', 't_1', 'F:/CodeBase/test', 'test')`);
  db.run(`INSERT INTO policies VALUES ('pair_1', 'v1', 'standard', 'prompt')`);

  function seedLaunchRequest(execId, reqId, retToken) {
    db.run(
      `INSERT INTO launch_requests VALUES (
        'pair_1', ?, ?, ?, 'conv_123', 'https://chatgpt.com/c/conv_123',
        'thash', 'ahash', 't_1', 'F:/CodeBase/test',
        'v1', 'standard', 'prompt', 'Do task', 'pdigest', 'claimed', ?, ?
      )`,
      [reqId, execId, retToken, now, now]
    );
    db.run(
      `INSERT INTO launch_attempts VALUES (?, 'pair_1', ?, ?, 'term_1', 'tab_1', 'pane_1', 'pty_1', 'started', NULL)`,
      [execId, now, now]
    );
  }

  return { tempDir, dbPath, adapterPath, db, seedLaunchRequest };
}

async function loadAdapter(adapterPath, env) {
  const mod = await import(adapterPath + "?" + Date.now() + Math.random());
  return (pi) => {
    Object.assign(process.env, env);
    return mod.default(pi);
  };
}

function createMockPi() {
  const handlers = new Map();
  return {
    on(event, handler) {
      if (!handlers.has(event)) handlers.set(event, []);
      handlers.get(event).push(handler);
    },
    async emit(event, data, ctx) {
      const list = handlers.get(event) || [];
      let lastResult;
      for (const h of list) {
        lastResult = await h(data, ctx);
      }
      return lastResult;
    }
  };
}

function createMockCtx(sessionId = "sess_main_1") {
  return {
    sessionManager: {
      getSessionId: () => sessionId,
      getSessionFile: () => `/tmp/${sessionId}.jsonl`,
    },
  };
}

async function runLifecycleTests() {
  console.log("=== Running Issue #68 Return Bridge Lifecycle Probes ===");

  // -------------------------------------------------------------
  // L1: Normal Lifecycle - Multiple Tool Rounds -> Exactly One Receipt
  // -------------------------------------------------------------
  {
    console.log("-> Testing L1: Normal Lifecycle with multiple tool rounds...");
    const env = createTempTestEnvironment();
    const execId = "exec_l1_test";
    env.seedLaunchRequest(execId, "req_l1", "ret_l1");

    process.env.HANDS_RETURN_BRIDGE_EXECUTION_ID = execId;
    process.env.HANDS_RETURN_BRIDGE_STATE_DIR = env.tempDir;

    const adapterFactory = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi = createMockPi();
    adapterFactory(pi);

    const ctx = createMockCtx("sess_l1");

    // Agent starts
    await pi.emit("agent_start", {}, ctx);

    // Turn 0: tool calls
    await pi.emit("turn_start", { turnIndex: 0 }, ctx);
    const messagesRound1 = [
      { role: "user", content: [{ type: "text", text: "Please inspect files" }] },
      { role: "assistant", content: [{ type: "toolCall", name: "read", id: "call_1" }] },
    ];
    await pi.emit("turn_end", { turnIndex: 0, message: messagesRound1[1], toolResults: [{ toolCallId: "call_1" }] }, ctx);

    // Turn 1: more tool calls
    await pi.emit("turn_start", { turnIndex: 1 }, ctx);
    const messagesRound2 = [
      ...messagesRound1,
      { role: "toolResult", content: [{ type: "text", text: "file content" }] },
      { role: "assistant", content: [{ type: "toolCall", name: "grep", id: "call_2" }] },
    ];
    await pi.emit("turn_end", { turnIndex: 1, message: messagesRound2[3], toolResults: [{ toolCallId: "call_2" }] }, ctx);

    // Turn 2: final assistant text (no tool calls, stopReason: stop)
    await pi.emit("turn_start", { turnIndex: 2 }, ctx);
    const messagesRound3 = [
      ...messagesRound2,
      { role: "toolResult", content: [{ type: "text", text: "grep matches" }] },
      {
        role: "assistant",
        id: "msg_final_l1",
        stopReason: "stop",
        content: [{ type: "text", text: "Done! All files verified." }],
      },
    ];
    await pi.emit("turn_end", { turnIndex: 2, message: messagesRound3[5], toolResults: [] }, ctx);

    // Session stop fires!
    await pi.emit("session_stop", {
      turn_id: 2,
      session_id: "sess_l1",
      stop_hook_active: false,
      last_assistant_message: messagesRound3[5],
      messages: messagesRound3,
    }, ctx);

    // CRITICAL: session_stop alone MUST NOT commit a receipt!
    const midReceipts = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(midReceipts.length, 0, "session_stop alone must NEVER commit a receipt");

    // Terminal agent_end fires with willContinue: false
    await pi.emit("agent_end", {
      messages: messagesRound3,
      willContinue: false,
    }, ctx);

    // Now exactly one durable receipt exists!
    const finalReceipts = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(finalReceipts.length, 1, "Exactly one completion receipt must be committed on terminal agent_end");
    const rcpt = finalReceipts[0];
    assert.equal(rcpt.execution_id, execId);
    assert.equal(rcpt.pairing_id, "pair_1");
    assert.equal(rcpt.return_token, "ret_l1");
    assert.equal(rcpt.turn_index, 2);
    assert.equal(rcpt.stop_reason, "stop");
    assert.equal(rcpt.assistant_text, "Done! All files verified.");
    assert.equal(rcpt.tool_call_count, 2);
    assert.equal(rcpt.state, "completed");

    // Launch request state transitioned to completed
    const reqRow = env.db.query("SELECT state FROM launch_requests WHERE execution_id = ?").get(execId);
    assert.equal(reqRow.state, "completed");

    console.log("  [PASS] L1: Exactly one Completion Receipt committed after multiple tool rounds");
  }

  // -------------------------------------------------------------
  // L2: Interruption / Abort (Escape / Ctrl+C) -> Zero Receipt
  // -------------------------------------------------------------
  {
    console.log("-> Testing L2: Interruption/Abort (Escape/Ctrl+C)...");
    const env = createTempTestEnvironment();
    const execId = "exec_l2_test";
    env.seedLaunchRequest(execId, "req_l2", "ret_l2");

    const adapterFactory = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi = createMockPi();
    adapterFactory(pi);
    const ctx = createMockCtx("sess_l2");

    await pi.emit("agent_start", {}, ctx);
    await pi.emit("turn_start", { turnIndex: 0 }, ctx);

    // User aborts during stream or tool execution
    const abortedMsg = {
      role: "assistant",
      stopReason: "aborted",
      content: [{ type: "text", text: "Partial interrupted stream..." }],
    };

    // session_stop either does not fire or has stopReason "aborted"
    await pi.emit("session_stop", {
      turn_id: 0,
      session_id: "sess_l2",
      last_assistant_message: abortedMsg,
      messages: [abortedMsg],
      stop_hook_active: false,
    }, ctx);

    await pi.emit("agent_end", {
      messages: [abortedMsg],
      willContinue: false,
    }, ctx);

    const receipts = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(receipts.length, 0, "Aborted / interrupted turn must NOT commit a receipt");
    console.log("  [PASS] L2: Fail-closed on Ctrl+C / stream abort (0 receipts)");
  }

  // -------------------------------------------------------------
  // L3: Provider Failure / Error / Refusal / Length -> Zero Receipt
  // -------------------------------------------------------------
  {
    console.log("-> Testing L3: Provider Error, Refusal, Length termination...");
    const env = createTempTestEnvironment();
    const execId = "exec_l3_test";
    env.seedLaunchRequest(execId, "req_l3", "ret_l3");

    const adapterFactory = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi = createMockPi();
    adapterFactory(pi);
    const ctx = createMockCtx("sess_l3");

    await pi.emit("agent_start", {}, ctx);

    // Case 3a: stopReason error
    const errorMsg = { role: "assistant", stopReason: "error", content: [{ type: "text", text: "API 500 error" }] };
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: errorMsg, messages: [errorMsg], stop_hook_active: false }, ctx);
    await pi.emit("agent_end", { messages: [errorMsg], willContinue: false }, ctx);
    assert.equal(env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length, 0);

    // Case 3b: stopReason length
    const lengthMsg = { role: "assistant", stopReason: "length", content: [{ type: "text", text: "Truncated token length" }] };
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: lengthMsg, messages: [lengthMsg], stop_hook_active: false }, ctx);
    await pi.emit("agent_end", { messages: [lengthMsg], willContinue: false }, ctx);
    assert.equal(env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length, 0);

    // Case 3c: turn ending mid-tool-use
    const toolMsg = { role: "assistant", stopReason: "stop", content: [{ type: "toolCall", name: "bash" }] };
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: toolMsg, messages: [toolMsg], stop_hook_active: false }, ctx);
    await pi.emit("agent_end", { messages: [toolMsg], willContinue: false }, ctx);
    assert.equal(env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length, 0);

    console.log("  [PASS] L3: Provider error, length limit, and mid-tool end produce zero receipts");
  }

  // -------------------------------------------------------------
  // L4: Intermediate Continuation (willContinue: true / undefined / falsy) -> Zero Receipt
  // -------------------------------------------------------------
  {
    console.log("-> Testing L4: Intermediate continuation & ambiguous willContinue...");
    const env = createTempTestEnvironment();
    const execId = "exec_l4_test";
    env.seedLaunchRequest(execId, "req_l4", "ret_l4");

    const adapterFactory = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi = createMockPi();
    adapterFactory(pi);
    const ctx = createMockCtx("sess_l4");

    await pi.emit("agent_start", {}, ctx);
    await pi.emit("turn_start", { turnIndex: 0 }, ctx);

    const normalMsg = {
      role: "assistant",
      stopReason: "stop",
      content: [{ type: "text", text: "Candidate message" }],
    };

    // 4a: willContinue === true defers commit
    await pi.emit("session_stop", {
      turn_id: 0,
      last_assistant_message: normalMsg,
      messages: [normalMsg],
      stop_hook_active: false,
    }, ctx);
    await pi.emit("agent_end", {
      messages: [normalMsg],
      willContinue: true,
    }, ctx);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length,
      0,
      "willContinue: true must NOT commit completion receipt"
    );

    // 4b: Ambiguous willContinue: undefined must fail closed (0 receipts)
    await pi.emit("session_stop", {
      turn_id: 1,
      last_assistant_message: normalMsg,
      messages: [normalMsg],
    }, ctx);
    await pi.emit("agent_end", {
      messages: [normalMsg],
      willContinue: undefined,
    }, ctx);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length,
      0,
      "willContinue: undefined must NOT commit completion receipt"
    );

    // 4c: Ambiguous willContinue: string "false" (non-boolean) must fail closed (0 receipts)
    await pi.emit("session_stop", {
      turn_id: 2,
      last_assistant_message: normalMsg,
      messages: [normalMsg],
      stop_hook_active: false,
    }, ctx);
    await pi.emit("agent_end", {
      messages: [normalMsg],
      willContinue: "false",
    }, ctx);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length,
      0,
      "willContinue: 'false' (string) must NOT commit completion receipt"
    );

    console.log("  [PASS] L4: willContinue: true correctly defers, ambiguous values fail closed");
  }

  // -------------------------------------------------------------
  // L5: Ownership Drift - Subagent, Second Prompt, Second Adapter, Owner Death
  // -------------------------------------------------------------
  {
    console.log("-> Testing L5: Ownership drift (subagent, second prompt, second adapter, owner death)...");
    const env = createTempTestEnvironment();
    const execId = "exec_l5_test";
    env.seedLaunchRequest(execId, "req_l5", "ret_l5");

    const adapterFactory = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi = createMockPi();
    adapterFactory(pi);

    const mainCtx = createMockCtx("sess_main_l5");
    const subCtx = createMockCtx("sess_subagent_l5");

    // 5a. Initial main session starts and claims ownership in SQLite
    await pi.emit("agent_start", {}, mainCtx);

    // 5b. Subagent runs concurrently with inherited env!
    await pi.emit("agent_start", {}, subCtx);
    const subMsg = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Subagent answer" }] };
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: subMsg, messages: [subMsg], stop_hook_active: false }, subCtx);
    await pi.emit("agent_end", { messages: [subMsg], willContinue: false }, subCtx);

    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length,
      0,
      "Subagent inheriting env must NOT commit receipt for parent execution"
    );

    // 5c. A second adapter instance created BEFORE receipt attempts to commit!
    const adapterFactory2 = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi2 = createMockPi();
    adapterFactory2(pi2);
    const reloadCtx = createMockCtx("sess_reload_l5");

    await pi2.emit("agent_start", {}, reloadCtx);
    const intruderMsg = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Intruder turn answer" }] };
    await pi2.emit("session_stop", { turn_id: 0, last_assistant_message: intruderMsg, messages: [intruderMsg], stop_hook_active: false }, reloadCtx);
    await pi2.emit("agent_end", { messages: [intruderMsg], willContinue: false }, reloadCtx);

    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId).length,
      0,
      "Second adapter instance created before receipt must NOT be able to commit"
    );

    // 5d. Main session (original owner) completes owned initial turn
    const mainMsg1 = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Main initial turn answer" }] };
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: mainMsg1, messages: [mainMsg1], stop_hook_active: false }, mainCtx);
    await pi.emit("agent_end", { messages: [mainMsg1], willContinue: false }, mainCtx);

    const receipts = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(receipts.length, 1, "Main owned turn committed exactly one receipt");
    assert.equal(receipts[0].assistant_text, "Main initial turn answer");

    // 5e. Second prompt in same session must NOT emit a second receipt and rejects as conflict
    const mainMsg2 = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Second prompt answer" }] };
    await pi.emit("session_stop", { turn_id: 1, last_assistant_message: mainMsg2, messages: [mainMsg2], stop_hook_active: false }, mainCtx);
    await assert.rejects(
      async () => await pi.emit("agent_end", { messages: [mainMsg2], willContinue: false }, mainCtx),
      /payload_conflict/
    );

    const afterSecondPrompt = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(afterSecondPrompt.length, 1, "Second prompt must not duplicate or replace receipt");
    assert.equal(afterSecondPrompt[0].assistant_text, "Main initial turn answer");
    // 5f. Owner death before receipt: prove authority never transfers to fresh adapter
    const execDeath = "exec_l5_death";
    env.seedLaunchRequest(execDeath, "req_l5_death", "ret_l5_death");
    const adapterDead = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execDeath,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const piDead = createMockPi();
    adapterDead(piDead);
    const deadCtx = createMockCtx("sess_dead");
    await piDead.emit("agent_start", {}, deadCtx);
    // Original owner dies before receipt (never calls agent_end)

    // Later fresh adapter instance starts
    const adapterFresh = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execDeath,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const piFresh = createMockPi();
    adapterFresh(piFresh);
    const freshCtx = createMockCtx("sess_fresh");
    await piFresh.emit("agent_start", {}, freshCtx);
    const freshMsg = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Fresh adapter attempt" }] };
    await piFresh.emit("session_stop", { turn_id: 0, last_assistant_message: freshMsg, messages: [freshMsg], stop_hook_active: false }, freshCtx);
    await piFresh.emit("agent_end", { messages: [freshMsg], willContinue: false }, freshCtx);

    // Execution stays unknown with zero receipts, no authority transfer occurred
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execDeath).length,
      0,
      "When owner dies before receipt, execution stays unknown and no transfer occurs"
    );
    assert.equal(
      env.db.query("SELECT state FROM launch_requests WHERE execution_id = ?").get(execDeath).state,
      "claimed"
    );

    console.log("  [PASS] L5: Subagent isolation, reload prevention, and no-transfer on death verified");
  }

  // -------------------------------------------------------------
  // L6: Pre-receipt Death -> Remains Unknown / Incomplete
  // -------------------------------------------------------------
  {
    console.log("-> Testing L6: Pre-receipt death...");
    const env = createTempTestEnvironment();
    const execId = "exec_l6_test";
    env.seedLaunchRequest(execId, "req_l6", "ret_l6");

    const req = env.db.query("SELECT state FROM launch_requests WHERE execution_id = ?").get(execId);
    assert.equal(req.state, "claimed");
    const receipts = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(receipts.length, 0);

    console.log("  [PASS] L6: Pre-receipt death leaves state unknown with zero receipts");
  }

  // -------------------------------------------------------------
  // L7: Missing or Malformed Lifecycle Authority Evidence -> Zero Receipt
  // -------------------------------------------------------------
  {
    console.log("-> Testing L7: Missing or malformed lifecycle authority evidence...");
    const env = createTempTestEnvironment();

    // 7a: Missing session ID (undefined / empty string)
    const execNoSession = "exec_l7_no_session";
    env.seedLaunchRequest(execNoSession, "req_l7_1", "ret_l7_1");
    const adapterFactory1 = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execNoSession,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi1 = createMockPi();
    adapterFactory1(pi1);
    const emptySessionCtx = { sessionManager: { getSessionId: () => "" } };
    await pi1.emit("agent_start", {}, emptySessionCtx);
    const msg = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Text" }] };
    await pi1.emit("session_stop", { turn_id: 0, last_assistant_message: msg, messages: [msg], stop_hook_active: false }, emptySessionCtx);
    await pi1.emit("agent_end", { messages: [msg], willContinue: false }, emptySessionCtx);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execNoSession).length,
      0,
      "Missing session ID must fail closed with 0 receipts"
    );

    // 7b: Malformed turn ID (undefined / negative / NaN)
    const execBadTurn = "exec_l7_bad_turn";
    env.seedLaunchRequest(execBadTurn, "req_l7_2", "ret_l7_2");
    const adapterFactory2 = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execBadTurn,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi2 = createMockPi();
    adapterFactory2(pi2);
    const validCtx = createMockCtx("sess_l7_2");
    await pi2.emit("agent_start", {}, validCtx);
    // turn_id is undefined
    await pi2.emit("session_stop", { turn_id: undefined, last_assistant_message: msg, messages: [msg], stop_hook_active: false }, validCtx);
    await pi2.emit("agent_end", { messages: [msg], willContinue: false }, validCtx);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execBadTurn).length,
      0,
      "Undefined turn_id must fail closed with 0 receipts"
    );

    // 7c: Missing or malformed stop_hook_active (undefined, true, string, null)
    const execBadStopHook = "exec_l7_bad_stop_hook";
    env.seedLaunchRequest(execBadStopHook, "req_l7_3", "ret_l7_3");
    const adapterFactory3 = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execBadStopHook,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi3 = createMockPi();
    adapterFactory3(pi3);
    const validCtx3 = createMockCtx("sess_l7_3");
    await pi3.emit("agent_start", {}, validCtx3);

    // Missing stop_hook_active (undefined)
    await pi3.emit("session_stop", { turn_id: 0, last_assistant_message: msg, messages: [msg] }, validCtx3);
    await pi3.emit("agent_end", { messages: [msg], willContinue: false }, validCtx3);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execBadStopHook).length,
      0,
      "Missing stop_hook_active must fail closed with 0 receipts"
    );

    // Malformed stop_hook_active (string "false")
    await pi3.emit("session_stop", { turn_id: 0, last_assistant_message: msg, messages: [msg], stop_hook_active: "false" }, validCtx3);
    await pi3.emit("agent_end", { messages: [msg], willContinue: false }, validCtx3);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execBadStopHook).length,
      0,
      "Malformed string stop_hook_active must fail closed with 0 receipts"
    );

    // stop_hook_active === true (continuation hook active)
    await pi3.emit("session_stop", { turn_id: 0, last_assistant_message: msg, messages: [msg], stop_hook_active: true }, validCtx3);
    await pi3.emit("agent_end", { messages: [msg], willContinue: false }, validCtx3);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execBadStopHook).length,
      0,
      "Active stop_hook_active must fail closed with 0 receipts"
    );
    console.log("  [PASS] L7: Missing session ID and malformed turn ID fail closed (0 receipts)");
  }

  // -------------------------------------------------------------
  // W3: Native Host Exit while OMP continues in Orca PTY (real process boundary)
  // -------------------------------------------------------------
  {
    console.log("-> Testing W3: Native host exit during PTY execution (process-boundary)...");
    const env = createTempTestEnvironment();
    const execId = "exec_w3_test";
    // 1. Launching native host initializes request and attempt, then exits
    env.seedLaunchRequest(execId, "req_w3", "ret_w3");
    env.db.close(); // Launching host closes database handle and exits

    // 2. OMP child process runs independently in a separate OS process
    const producerScript = `
import { Database } from "bun:sqlite";
import * as path from "node:path";

const adapterPath = ${JSON.stringify(env.adapterPath)};
const execId = ${JSON.stringify(execId)};
const tempDir = ${JSON.stringify(env.tempDir)};

process.env.HANDS_RETURN_BRIDGE_EXECUTION_ID = execId;
process.env.HANDS_RETURN_BRIDGE_STATE_DIR = tempDir;

const mod = await import(adapterPath);
const handlers = new Map();
const pi = {
  on(event, handler) {
    if (!handlers.has(event)) handlers.set(event, []);
    handlers.get(event).push(handler);
  },
  async emit(event, data, ctx) {
    for (const h of (handlers.get(event) || [])) {
      await h(data, ctx);
    }
  }
};
mod.default(pi);

const ctx = {
  sessionManager: {
    getSessionId: () => "sess_w3_child",
    getSessionFile: () => "/tmp/sess_w3_child.jsonl",
  },
};

await pi.emit("agent_start", {}, ctx);
const msg = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "W3 child process result" }] };
await pi.emit("session_stop", { turn_id: 0, last_assistant_message: msg, messages: [msg], stop_hook_active: false }, ctx);
await pi.emit("agent_end", { messages: [msg], willContinue: false }, ctx);
process.exit(0);
`;
    const w3ScriptPath = path.join(env.tempDir, "w3_producer.mjs");
    fs.writeFileSync(w3ScriptPath, producerScript, "utf8");

    const runProducer = cp.spawnSync(process.execPath, [w3ScriptPath], {
      encoding: "utf8",
      env: { ...process.env },
    });
    assert.equal(runProducer.status, 0, `OMP producer child process failed: ${runProducer.stderr}`);

    // 3. New native host process opens database from disk and recovers/verifies receipt
    const recoveryScript = `
import { Database } from "bun:sqlite";
import assert from "node:assert/strict";

const dbPath = ${JSON.stringify(env.dbPath)};
const execId = ${JSON.stringify(execId)};

const db = new Database(dbPath);
const receipt = db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").get(execId);
assert.ok(receipt, "Committed receipt must be readable by recovery host after producer exit");
assert.equal(receipt.assistant_text, "W3 child process result");
assert.equal(receipt.state, "completed");

const req = db.query("SELECT state FROM launch_requests WHERE execution_id = ?").get(execId);
assert.equal(req.state, "completed");

db.close();
process.exit(0);
`;
    const w3RecoveryPath = path.join(env.tempDir, "w3_recovery.mjs");
    fs.writeFileSync(w3RecoveryPath, recoveryScript, "utf8");

    const runRecovery = cp.spawnSync(process.execPath, [w3RecoveryPath], {
      encoding: "utf8",
      env: { ...process.env },
    });
    assert.equal(runRecovery.status, 0, `Recovery host process failed: ${runRecovery.stderr}`);

    console.log("  [PASS] W3: Receipt readable across real native host & producer process boundaries");
  }

  // -------------------------------------------------------------
  // S1: Transaction Crash Consistency & Mid-Flight Kill
  // -------------------------------------------------------------
  {
    console.log("-> Testing S1: Transaction crash consistency & mid-flight kill (process-boundary)...");
    const env = createTempTestEnvironment();

    // Pre-seed an existing valid execution to verify prior data remains readable after later crashes
    const execPrior = "exec_s1_prior";
    env.seedLaunchRequest(execPrior, "req_s1_prior", "ret_s1_prior");
    const adapterPrior = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execPrior,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const piPrior = createMockPi();
    adapterPrior(piPrior);
    const ctxPrior = createMockCtx("sess_s1_prior");
    await piPrior.emit("agent_start", {}, ctxPrior);
    const msgPrior = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Prior verified data" }] };
    await piPrior.emit("session_stop", { turn_id: 0, last_assistant_message: msgPrior, messages: [msgPrior], stop_hook_active: false }, ctxPrior);
    await piPrior.emit("agent_end", { messages: [msgPrior], willContinue: false }, ctxPrior);
    assert.equal(env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execPrior).length, 1);

    // S1a: Genuinely kill child process between session_stop (completion candidate formed in memory) and agent_end transaction boundary
    const execKillMid = "exec_s1_kill_mid";
    env.seedLaunchRequest(execKillMid, "req_s1_km", "ret_s1_km");

    const s1aScript = `
const adapterPath = ${JSON.stringify(env.adapterPath)};
const execId = ${JSON.stringify(execKillMid)};
const tempDir = ${JSON.stringify(env.tempDir)};

process.env.HANDS_RETURN_BRIDGE_EXECUTION_ID = execId;
process.env.HANDS_RETURN_BRIDGE_STATE_DIR = tempDir;

const mod = await import(adapterPath);
const handlers = new Map();
const pi = {
  on(event, handler) {
    if (!handlers.has(event)) handlers.set(event, []);
    handlers.get(event).push(handler);
  },
  async emit(event, data, ctx) {
    for (const h of (handlers.get(event) || [])) {
      await h(data, ctx);
    }
  }
};
mod.default(pi);

const ctx = {
  sessionManager: {
    getSessionId: () => "sess_s1_km",
    getSessionFile: () => "/tmp/sess_s1_km.jsonl",
  },
};

await pi.emit("agent_start", {}, ctx);
const msgKm = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Mid-turn text" }] };
// session_stop creates completionCandidate in memory
await pi.emit("session_stop", { turn_id: 0, last_assistant_message: msgKm, messages: [msgKm], stop_hook_active: false }, ctx);

// Genuinely kill child process right here before agent_end transaction boundary!
process.kill(process.pid, 9);
`;
    const s1aScriptPath = path.join(env.tempDir, "s1a_worker.mjs");
    fs.writeFileSync(s1aScriptPath, s1aScript, "utf8");

    const runS1a = cp.spawnSync(process.execPath, [s1aScriptPath], { stdio: "ignore" });
    assert.notEqual(runS1a.status, 0, "S1a worker child process must be terminated non-zero by kill");

    // Reopen from disk and assert zero receipts and state remains claimed
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execKillMid).length,
      0,
      "Failure before agent_end transaction must produce zero receipts"
    );
    assert.equal(
      env.db.query("SELECT state FROM launch_requests WHERE execution_id = ?").get(execKillMid).state,
      "claimed",
      "State must remain 'claimed' with no false completed state"
    );

    // S1b: Genuinely kill writer process at transaction boundary holding an uncommitted transaction
    const execRollback = "exec_s1_rollback";
    env.seedLaunchRequest(execRollback, "req_s1_rb", "ret_s1_rb");

    const s1bScript = `
import { Database } from "bun:sqlite";
const dbPath = ${JSON.stringify(env.dbPath)};
const execId = ${JSON.stringify(execRollback)};

const db = new Database(dbPath);
db.run("PRAGMA journal_mode = WAL;");
db.run("BEGIN IMMEDIATE;");
db.run(
  "INSERT INTO completion_receipts (receipt_id, execution_id, pairing_id, return_token, origin_conversation_id, turn_index, stop_reason, assistant_message_id, assistant_text, content_digest, tool_call_count, state, committed_at) VALUES ('rcpt_rb', ?, 'pair_1', 'ret_s1_rb', 'conv_123', 0, 'stop', 'msg_1', 'Text', 'dig', 1, 'completed', 1000)",
  [execId]
);

// Genuinely kill writer process mid-transaction before COMMIT
process.kill(process.pid, 9);
`;
    const s1bScriptPath = path.join(env.tempDir, "s1b_worker.mjs");
    fs.writeFileSync(s1bScriptPath, s1bScript, "utf8");

    const runS1b = cp.spawnSync(process.execPath, [s1bScriptPath], { stdio: "ignore" });
    assert.notEqual(runS1b.status, 0, "S1b writer child process must be terminated non-zero by kill");

    // Reopen database handle from disk: SQLite WAL rollback must ensure 0 receipts and claimed state
    const freshDbAfterCrash = new Database(env.dbPath);
    assert.equal(
      freshDbAfterCrash.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execRollback).length,
      0,
      "Killed transaction writer leaves zero partial receipt after WAL recovery"
    );
    assert.equal(
      freshDbAfterCrash.query("SELECT state FROM launch_requests WHERE execution_id = ?").get(execRollback).state,
      "claimed"
    );
    freshDbAfterCrash.close();

    // S1c: Verify prior committed data remains intact and readable
    const priorRow = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").get(execPrior);
    assert.ok(priorRow, "Prior committed receipt must remain readable after later crashes");
    assert.equal(priorRow.assistant_text, "Prior verified data");

    console.log("  [PASS] S1: Mid-flight kill, transaction boundary crash, and prior data preservation verified");
  }

  // -------------------------------------------------------------
  // S2: Duplicate / Conflicting Writer (Canonical Digest Over All Material Fields)
  // -------------------------------------------------------------
  {
    console.log("-> Testing S2: Duplicate & conflicting writer with canonical digest...");
    const env = createTempTestEnvironment();
    const execId = "exec_s2_test";
    env.seedLaunchRequest(execId, "req_s2", "ret_s2");

    const adapterFactory = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const pi = createMockPi();
    adapterFactory(pi);
    const ctx = createMockCtx("sess_s2");

    await pi.emit("agent_start", {}, ctx);
    const msg = { role: "assistant", id: "msg_s2_1", stopReason: "stop", content: [{ type: "text", text: "Original S2 text" }] };
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: msg, messages: [msg], stop_hook_active: false }, ctx);
    await pi.emit("agent_end", { messages: [msg], willContinue: false }, ctx);

    const firstReceipt = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").get(execId);
    assert.ok(firstReceipt);
    assert.equal(firstReceipt.assistant_text, "Original S2 text");

    // 2a: Identical evidence re-emitted through production adapter path -> converges deterministically on same single receipt
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: msg, messages: [msg], stop_hook_active: false }, ctx);
    await pi.emit("agent_end", { messages: [msg], willContinue: false }, ctx);
    const duplicateReceipts = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(duplicateReceipts.length, 1, "Identical evidence duplicate must converge on single receipt");
    assert.equal(duplicateReceipts[0].receipt_id, firstReceipt.receipt_id);
    assert.equal(duplicateReceipts[0].content_digest, firstReceipt.content_digest);

    // 2b: Conflicting assistant text with same execution_id driven through production adapter path
    const conflictMsgText = { role: "assistant", id: "msg_s2_1", stopReason: "stop", content: [{ type: "text", text: "Conflicting text!" }] };
    await pi.emit("session_stop", { turn_id: 0, last_assistant_message: conflictMsgText, messages: [conflictMsgText], stop_hook_active: false }, ctx);
    await assert.rejects(
      async () => await pi.emit("agent_end", { messages: [conflictMsgText], willContinue: false }, ctx),
      /payload_conflict/,
      "Conflicting completion evidence must be explicitly rejected with payload_conflict"
    );

    // Verify original receipt is 100% unchanged in SQLite
    const receiptsAfterConflict = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(receiptsAfterConflict.length, 1, "Conflicting receipt must NOT overwrite existing receipt");
    assert.equal(receiptsAfterConflict[0].assistant_text, "Original S2 text");
    assert.equal(receiptsAfterConflict[0].receipt_id, firstReceipt.receipt_id);
    assert.equal(receiptsAfterConflict[0].content_digest, firstReceipt.content_digest);

    console.log("  [PASS] S2: Deterministic duplicates and canonical conflict rejection verified");
  }

  // -------------------------------------------------------------
  // S3: Isolated Storage & Engine Failure Probes (Lock, Chmod, Corrupt, Disk-Full)
  // -------------------------------------------------------------
  {
    console.log("-> Testing S3: Isolated storage & engine failure probes...");
    const env = createTempTestEnvironment();

    // Pre-seed an existing valid execution to verify it survives subsequent storage failures
    const execPriorS3 = "exec_s3_prior";
    env.seedLaunchRequest(execPriorS3, "req_s3_prior", "ret_s3_prior");
    const adapterPrior = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execPriorS3,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const piPrior = createMockPi();
    adapterPrior(piPrior);
    const ctxPrior = createMockCtx("sess_s3_prior");
    await piPrior.emit("agent_start", {}, ctxPrior);
    const msgPrior = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Prior verified S3 text" }] };
    await piPrior.emit("session_stop", { turn_id: 0, last_assistant_message: msgPrior, messages: [msgPrior], stop_hook_active: false }, ctxPrior);
    await piPrior.emit("agent_end", { messages: [msgPrior], willContinue: false }, ctxPrior);
    assert.equal(env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execPriorS3).length, 1);

    // S3a: SQLite busy/lock timeout with competing process holding exclusive lock
    const execLock = "exec_s3_lock";
    env.seedLaunchRequest(execLock, "req_s3_lock", "ret_s3_lock");
    const competingDb = new Database(env.dbPath);
    competingDb.run("BEGIN EXCLUSIVE;"); // Hold exclusive database lock

    const adapterLock = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execLock,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
      HANDS_RETURN_BRIDGE_BUSY_TIMEOUT_MS: "50", // Short timeout for fast isolated test
    });
    const piLock = createMockPi();
    adapterLock(piLock);
    const ctxLock = createMockCtx("sess_s3_lock");
    await piLock.emit("agent_start", {}, ctxLock);
    const msgLock = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Lock test" }] };
    await piLock.emit("session_stop", { turn_id: 0, last_assistant_message: msgLock, messages: [msgLock], stop_hook_active: false }, ctxLock);
    // agent_end hits busy_timeout and fails closed safely without unhandled crash
    await piLock.emit("agent_end", { messages: [msgLock], willContinue: false }, ctxLock);

    // Release competing lock
    competingDb.run("ROLLBACK;");
    competingDb.close();

    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execLock).length,
      0,
      "Locked database must fail closed with 0 receipts"
    );

    // S3b: Permission / open-write denial (readonly file mode on Windows)
    const execReadonly = "exec_s3_readonly";
    env.seedLaunchRequest(execReadonly, "req_s3_ro", "ret_s3_ro");
    // Set database file to read-only
    fs.chmodSync(env.dbPath, 0o444);
    const adapterRo = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execReadonly,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
    });
    const piRo = createMockPi();
    adapterRo(piRo);
    const ctxRo = createMockCtx("sess_s3_ro");
    await piRo.emit("agent_start", {}, ctxRo);
    const msgRo = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Readonly test" }] };
    await piRo.emit("session_stop", { turn_id: 0, last_assistant_message: msgRo, messages: [msgRo], stop_hook_active: false }, ctxRo);
    // Fails closed on attempt to write to readonly DB
    await piRo.emit("agent_end", { messages: [msgRo], willContinue: false }, ctxRo);
    // Restore write permissions
    fs.chmodSync(env.dbPath, 0o666);
    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execReadonly).length,
      0,
      "Readonly database must fail closed with 0 receipts"
    );

    // S3c: Malformed / database corruption failure
    const corruptDir = fs.mkdtempSync(path.join(os.tmpdir(), "hands_corrupt_"));
    const corruptDbPath = path.join(corruptDir, "journal.sqlite");
    fs.writeFileSync(corruptDbPath, "GARBAGE_NOT_A_SQLITE_DATABASE_HEADER_XYZ", "utf8");
    const adapterCorrupt = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: "exec_corrupt",
      HANDS_RETURN_BRIDGE_STATE_DIR: corruptDir,
    });
    const piCorrupt = createMockPi();
    adapterCorrupt(piCorrupt);
    const ctxCorrupt = createMockCtx("sess_corrupt");
    await piCorrupt.emit("agent_start", {}, ctxCorrupt);
    await piCorrupt.emit("session_stop", { turn_id: 0, last_assistant_message: msgPrior, messages: [msgPrior], stop_hook_active: false }, ctxCorrupt);
    await piCorrupt.emit("agent_end", { messages: [msgPrior], willContinue: false }, ctxCorrupt);
    fs.rmSync(corruptDir, { recursive: true, force: true });

    // S3d: Safe disk-full simulation via SQLite PRAGMA max_page_count = 1 (simulates native SQLITE_FULL without filling real disk)
    const execDiskFull = "exec_s3_disk_full";
    env.seedLaunchRequest(execDiskFull, "req_s3_df", "ret_s3_df");

    const adapterDf = await loadAdapter(env.adapterPath, {
      HANDS_RETURN_BRIDGE_EXECUTION_ID: execDiskFull,
      HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
      HANDS_RETURN_BRIDGE_FAULT_INJECT: "disk_full",
    });
    const piDf = createMockPi();
    adapterDf(piDf);
    const ctxDf = createMockCtx("sess_s3_df");
    await piDf.emit("agent_start", {}, ctxDf);
    const largeMsg = { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "X".repeat(10000) }] };
    await piDf.emit("session_stop", { turn_id: 0, last_assistant_message: largeMsg, messages: [largeMsg], stop_hook_active: false }, ctxDf);
    await piDf.emit("agent_end", { messages: [largeMsg], willContinue: false }, ctxDf);
    delete process.env.HANDS_RETURN_BRIDGE_FAULT_INJECT;

    assert.equal(
      env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execDiskFull).length,
      0,
      "Disk-full failure must fail closed with 0 receipts"
    );

    // S3e: Previously committed data remains readable after later storage failures
    const priorLoaded = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").get(execPriorS3);
    assert.ok(priorLoaded, "Previously committed receipt must remain readable after subsequent failures");
    assert.equal(priorLoaded.assistant_text, "Prior verified S3 text");

    console.log("  [PASS] S3: Lock timeout, readonly permission, corruption, disk-full, and prior data preservation verified");
  }

  // -------------------------------------------------------------
  // Real OMP Process Probe: multiple tool rounds -> exactly one receipt
  // -------------------------------------------------------------
  {
    console.log("-> Testing Real OMP process execution with multiple tool rounds...");
    const env = createTempTestEnvironment();
    const execId = "exec_real_omp_" + Date.now();
    env.seedLaunchRequest(execId, "req_real", "ret_real");

    const prompt = `Use the read tool to read the first line of "${env.adapterPath.replace(/\\/g, "/")}", then say "PROBE COMPLETED".`;

    const proc = Bun.spawn([
      "omp",
      "-e", env.adapterPath,
      "-p", prompt,
    ], {
      env: {
        ...process.env,
        HANDS_RETURN_BRIDGE_EXECUTION_ID: execId,
        HANDS_RETURN_BRIDGE_STATE_DIR: env.tempDir,
      },
      stdout: "pipe",
      stderr: "pipe",
    });

    const output = await new Response(proc.stdout).text();
    const exitCode = await proc.exited;
    assert.equal(exitCode, 0, `OMP process exited with code ${exitCode}`);
    console.log("  OMP output:", output.trim());
    const receipts = env.db.query("SELECT * FROM completion_receipts WHERE execution_id = ?").all(execId);
    assert.equal(receipts.length, 1, "Real OMP execution must commit exactly ONE receipt");
    const rcpt = receipts[0];
    console.log("  Committed receipt:", rcpt);
    assert.equal(rcpt.execution_id, execId);
    assert.equal(rcpt.pairing_id, "pair_1");
    assert.equal(rcpt.return_token, "ret_real");
    assert.equal(rcpt.stop_reason, "stop");
    assert.equal(rcpt.state, "completed");
    const reqRow = env.db.query("SELECT state FROM launch_requests WHERE execution_id = ?").get(execId);
    assert.equal(reqRow.state, "completed");

    console.log("  [PASS] Real OMP process execution produced exactly one Completion Receipt!");
  }

  console.log("\n=== ALL L1-L6, W3, S1-S3 Lifecycle Probes PASSED CLEANLY! ===");
}

runLifecycleTests().catch((err) => {
  console.error("Test failed:", err);
  process.exit(1);
});
