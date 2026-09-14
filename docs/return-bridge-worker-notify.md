# Return Bridge worker notify — sending task completion into a ChatGPT conversation

> **Scope:** how a local worker (an Orca OMP session, a CLI run, or any process holding the prepared environment) tells a bound ChatGPT conversation that its task finished, what the bridge does with that notification, and how to verify or unblock it. Use this when the task is to deliver a worker-completion message into ChatGPT. Delivery is request-native: the bridge talks to the ChatGPT backend from the open tab, never through the composer, send button, or DOM.

## Prerequisites

Verify these before notifying:

- The **Hands Return Bridge** extension is loaded, paired, and in local mode (`hands-return-bridge local-init`, then pair from the extension).
- The native messaging host is registered for the browser profile that runs the extension (`com.hands.return_bridge`).
- A `chatgpt.com` tab for the **target conversation** is open and signed in. Conversation registration is automatic: the content script announces readiness on load, and the background worker registers the canonical conversation ID on tab updates. Routing is conversation-first — no target, workspace, or target-id is involved.
- The target conversation is not holding a delivery slot for an unresolved attempt (see [Terminal states and recovery](#terminal-states-and-recovery)).
- The state directory defaults to `%LOCALAPPDATA%\Hands\return-bridge` and can be overridden with `--state-dir`.

An Orca OMP worker launched by the extension already receives the three environment variables and can skip step 1.

## Send the notification

1. Claim worker identity for the conversation:

```bash
hands-return-bridge prepare --conversation <conversation_id> --json
```

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

2. Record the terminal state:

```bash
HANDS_TASK_ID=task_… \
HANDS_RETURN_BRIDGE_EXECUTION_ID=exec_… \
HANDS_RETURN_BRIDGE_STATE_DIR='C:\Users\monet\AppData\Local\Hands\return-bridge' \
hands-return-bridge notify done
# → Notification recorded (receipt: rcpt_199309bccd80f89cb300051c0c987039, task: task_…, execution: exec_…, status: completed)
```

Use `notify failed --message "<reason>"` for a failed run. `--task`, `--execution-id`, and `--state-dir` are accepted flags if the environment variables are not set, but at least one identity source is required; a notification without identity fails closed with `execution_mismatch`.

## What happens after `notify`

1. The native host commits a completion receipt and records the notification.
2. The extension's persistent native push channel receives `receipt_ready` and runs a recovery drain.
3. The extension persists the receipt locally, then acknowledges it to the native host (ACK means "durably stored in the browser", not "delivered to ChatGPT").
4. The background worker acquires the Dispatch Fence for the receipt; the fence is the delivery authority and holds a per-conversation delivery slot.
5. Inside the MAIN world of the conversation tab the worker: reads `/api/auth/session` for an access token, reads `current_node` and `gizmo_id` from `/backend-api/conversation/<id>`, runs Sentinel `prepare`, proof-of-work, and the turnstile VM, calls Sentinel `finalize`, then `POST`s `/backend-api/conversation`.
6. It re-reads the conversation and looks for the receipt marker in the transcript before settling the fence.

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
| `execution_mismatch` | No identity supplied | Run `prepare` first, or pass `--task`/`--execution-id`/`--state-dir` |
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
