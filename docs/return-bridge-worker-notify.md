# Return Bridge worker notify — sending task completion into a ChatGPT conversation

> **Scope:** how a local worker (an Orca OMP session, a CLI run, or any process holding the prepared environment) tells a bound ChatGPT conversation that its task finished, what the bridge does with that notification, and how to verify or unblock it. Use this when the task is to deliver a worker-completion message into ChatGPT. Delivery is request-native: the bridge talks to the ChatGPT backend from the open tab, never through the composer, send button, or DOM.

The flow lives entirely in the companion `hands-bridge` binary (crate `bridge/native`), installed as `%LOCALAPPDATA%\Hands\return-bridge\hands-bridge.exe` and registered as the browser's native messaging host. The `hands` runtime neither embeds nor depends on it, so building or changing this flow never rebuilds `hands.exe`.

Build, install, and register it from the repository (Windows):

```powershell
powershell -File bridge\install-return-bridge.ps1 -ExtensionId <extension-id>
```

That script builds `bridge\native\target\debug\hands-bridge.exe`, copies it into `%LOCALAPPDATA%\Hands\return-bridge\`, adds the directory to the user `PATH`, runs `local-init` to write the native messaging manifest, and registers the workspace target. Names outside the worker contract keep their historical spelling on purpose, so no live pairing is disturbed by a binary rename: the messaging host stays `com.hands.return_bridge`, the environment variables stay `HANDS_RETURN_BRIDGE_*`, and the state directory (with its journal and delivery slots) stays `%LOCALAPPDATA%\Hands\return-bridge\`. Reload the extension once after replacing the binary.

## Prerequisites

Verify these before notifying:

- The **Hands Return Bridge** extension is loaded, paired, and in local mode (`hands-bridge local-init`, then pair from the extension).
- The native messaging host is registered for the browser profile that runs the extension (`com.hands.return_bridge`).
- A `chatgpt.com` tab for the **target conversation** is open and signed in. Conversation registration is automatic: the content script announces readiness on load, and the background worker registers the canonical conversation ID on tab updates. Routing is conversation-first — no target, workspace, or target-id is involved.
- The target conversation is not holding a delivery slot for an unresolved attempt (see [Terminal states and recovery](#terminal-states-and-recovery)).
- The state directory defaults to `%LOCALAPPDATA%\Hands\return-bridge` and can be overridden with `--state-dir`.

An Orca OMP worker launched by the extension already receives the three environment variables, so it needs no arguments at all.

## Send the notification

The worker contract is one command:

```bash
hands-bridge done --conversation <conversation_id>
# → Notification recorded (receipt: rcpt_f6ac3810e32ec9127e99682e4fb0d1c5, conversation: 6aa7f156-6ef4-83ec-9be6-ebd9260b79ac, status: completed).

hands-bridge failed --conversation <conversation_id> --message "cargo test failed"
```

`done`/`failed` run both internal steps:

```text
resolve the conversation
-> reuse the execution this worker was launched with, or claim a new one
-> record the completion receipt
-> exit 0
```

The worker never handles task IDs, execution IDs, state directories, or the return token. A worker that the extension launched inherits `HANDS_TASK_ID`, `HANDS_RETURN_BRIDGE_EXECUTION_ID`, and `HANDS_RETURN_BRIDGE_CONVERSATION_ID`, so it needs no arguments at all:

```bash
hands-bridge done
```

Contract rules:

- `--conversation` is required unless the worker environment already names the conversation; otherwise the command exits 2 without touching the journal.
- A worker execution bound to one conversation refuses to report for another conversation (`--conversation` mismatch) and exits 1.
- Each call records its own receipt: two `done` calls produce two messages in ChatGPT.

### Underlying steps (implementation detail)

Drive the steps separately only when you must inject identity into a process you launch yourself:

1. `hands-bridge prepare --conversation <conversation_id> --json` claims identity and prints it:

```json
{
  "task_id": "task_491b03b2a87fbb93e20eb1a884690166",
  "execution_id": "exec_311a794e27f18c821d7406df6e7bf7e2",
  "state": "claimed",
  "state_dir": "C:\\Users\\monet\\AppData\\Local\\Hands\\return-bridge",
  "policy_revision": "v1",
  "origin_conversation_id": "6aa7f156-6ef4-83ec-9be6-ebd9260b79ac",
  "env": { "HANDS_TASK_ID": "…", "HANDS_RETURN_BRIDGE_EXECUTION_ID": "…", "HANDS_RETURN_BRIDGE_STATE_DIR": "…" }
}
```

`prepare` never prints the `return_token`; it stays in the native journal.

2. `hands-bridge notify done` (or `notify failed --message "<reason>"`) with that environment, or with `--task`/`--execution-id`/`--state-dir`. The low-level `notify` requires at least one identity source and fails closed with `execution_mismatch` without one.

## What happens after `notify`

1. The native host commits a completion receipt and records the notification.
2. The extension's persistent native push channel receives `receipt_ready` and runs a recovery drain.
3. The extension persists the receipt locally, then acknowledges it to the native host (ACK means "durably stored in the browser", not "delivered to ChatGPT").
4. The background worker acquires the Dispatch Fence for the receipt; the fence is the delivery authority and holds a per-conversation delivery slot.
5. Inside the MAIN world of the conversation tab the worker: reads `/api/auth/session` for an access token, reads `current_node` and `gizmo_id` from `/backend-api/conversation/<id>`, runs Sentinel `prepare`, proof-of-work, and the turnstile VM, calls Sentinel `finalize`, then `POST`s `/backend-api/conversation`.
6. It re-reads the conversation and looks for the receipt marker in the transcript before settling the fence.

The message the conversation receives is deliberately short and self-identifying:

```text
[Hands Bridge] Agent execution completed. Check the work and continue! [hands-bridge:receipt=rcpt_…]
[Hands Bridge] Agent execution failed: <reason>. Check the work and continue! [hands-bridge:receipt=rcpt_…]
```

Task and execution identifiers stay out of the text — the journal maps `receipt -> task/execution` — but the trailing marker must stay. It is the only token that is unique per receipt, so transcript verification and the recovery probe can tell this delivery apart from every other message in the same conversation; a conversation id cannot serve that purpose.

Fence settlement outcomes:

- **`submitted-observed`** — the receipt marker was observed in the conversation transcript. The message is in ChatGPT; the delivery slot is released.
- **`not-sent`** — conclusive evidence that nothing was sent (for example the server rejected the request before acceptance). The slot is released and the receipt becomes dispatchable again.
- **`dispatching/uncertain`** — no conclusion. The slot stays held.

## Terminal states and recovery

An attempt can die between "fence acquired" and "fence settled": the MV3 service worker is terminated while the page request is still running, or the tab closes. The fence then stays `dispatching/uncertain` and, by design, there is no lease, no automatic retry, and no CLI settle command.

Recovery resolves that state from new evidence instead of guessing. On the next recovery pass the extension probes the conversation transcript through the tab:

- marker present → settle `submitted-observed` with the observed message ID;
- marker absent on two probes at least 60 seconds apart → settle `not-sent` (the route then delivers the receipt on the following pass);
- transcript not readable (no tab for that conversation, rejected API read) → stay `dispatching/uncertain` and keep holding the slot.

The probe is guarded by an in-flight set, so a dispatch still running in the current service worker is never resolved underneath. It reuses the stored attempt ID and delivery revision because the native settlement CAS checks both.

Operational consequence: while a receipt is unresolved, every other receipt for the same conversation is refused with `slot_busy`. Open a tab for that conversation and let the recovery pass run; that is the only way to unblock it.

## Verify

Journal (read-only; the live journal is also opened for writing by the native host):

```bash
python - <<'PY'
import os, sqlite3
p = os.path.join(os.environ['LOCALAPPDATA'], 'Hands', 'return-bridge', 'journal.sqlite')
con = sqlite3.connect(f'file:{p}?mode=ro', uri=True); cur = con.cursor()
cur.execute("select receipt_id, state, observed_message_id, settled_at from dispatch_fences order by rowid desc limit 5")
print(cur.fetchall())
cur.execute("select origin_conversation_id, receipt_id, state from conversation_delivery_slots")
print(cur.fetchall())
PY
```

An empty `conversation_delivery_slots` row set means no conversation is blocked. `receipt_acknowledgements` proves only that the browser stored the receipt.

Extension-side diagnostic: the popup shows the last dispatch result, and the same object is persisted under the `lastDispatchDiagnostic` key in the extension's local storage (`…\User Data\<profile>\Local Extension Settings\<extension id>\`). It carries `status` (`submitted-observed`, `not-sent`, `uncertain`), `reason`, `httpStatus`, `gizmoId`, and the server's error body when the conversation POST was rejected.

## Failure modes

| Symptom | Cause | Action |
| --- | --- | --- |
| `execution_mismatch` | Low-level `notify` called with no identity | Use `hands-bridge done --conversation <id>`, or run `prepare` first |
| Exit 2 with a `usage:` line | `done`/`failed` called with no conversation and no worker environment | Pass `--conversation`, or run it from the launched worker |
| Exit 1 `does not match the conversation bound to this worker execution` | `--conversation` disagrees with the execution's conversation | Drop the flag, or pass the conversation this worker was launched for |
| `no_bound_conversation` | Conversation never registered | Open a `chatgpt.com` tab for that conversation |
| `ambiguous_target_binding` | Several live bindings | Close stale conversation tabs |
| `slot_busy` | Another receipt holds the conversation slot | See [Terminal states and recovery](#terminal-states-and-recovery) |
| `httpStatus: 404`, `history_disabled_conversation_not_found` | Request carried `history_and_training_disabled` while continuing a stored conversation | Fixed in the extension; update the extension if seen |
| 404 on the conversation POST for a project chat | Conversation lives under `/g/<gizmo_id>/c/<id>` and needs `conversation_mode: {"kind":"gizmo_interaction","gizmo_id":"…"}` | Fixed in the extension; `lastDispatchDiagnostic.gizmoId` shows the derived id |
| Receipt never leaves `dispatching/uncertain` | Service worker died mid-dispatch (long proof-of-work in a hidden tab) | Keep dispatches short; the proof-of-work loop yields through `MessageChannel` so hidden tabs do not throttle it |
| Extension runs old code | `background.js` is re-read when the service worker restarts, but an active user keeps it alive | Reload the extension from `chrome://extensions`; `content_script.js` and `manifest.json` changes always need a reload |

## Non-goals

- No ChatGPT account import. The bridge uses the signed-in session of the open tab (`/api/auth/session`); there is no access-token form, CPA JSON, or sub2api connection.
- No DOM or composer automation on this path.
- One receipt per execution: repeating `notify` with the same `execution_id` returns the existing receipt; a new receipt requires a new `prepare`.
