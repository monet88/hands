# Can ChatCMD remove the manual "Done" step after Orca workers finish?

## Conclusion

**Partially, and the missing piece is small but real.** ChatCMD already proves that a local runtime can inject a follow-up message into the exact signed-in ChatGPT conversation through its browser extension, and its queued-message UI can automatically send that message once the conversation is ready. That is the right primitive for removing the user's manual `Done` message.

However, ChatCMD does **not** currently provide a native integration from an external Orca `worker_done` event into that ChatGPT queue. Orca 1.4.197 also does not expose an event-trigger automation/webhook surface for `worker_done`; its native automation surface is schedule-based. Therefore replacing Hands with ChatCMD does not, by itself, make `Orca worker_done -> resume ChatGPT` automatic.

For this user's workflow, the best fit is **keep Hands + Orca and add a narrow completion bridge** rather than replace the local execution stack. The bridge only needs to translate an Orca completion event into a queued follow-up message for the bound ChatGPT conversation.

## Findings

### Verified: ChatCMD can send into an existing ChatGPT conversation

- `web/src/chatgptBridge.ts` exposes `dispatchChatGptRequest`, which posts an extension command containing `requestId`, `submittedContent`, model, and optional `conversationUrl`.
- `chatgpt-extension/background.js` handles the `send` action, resolves/acquires the requested conversation tab, stores request context, and sends `chatcmd-chatgpt-run` into that exact tab.
- The bridge depends on an installed browser extension and a signed-in ChatGPT browser session.

Sources:
- https://github.com/int04/ChatCmd/blob/bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c/web/src/chatgptBridge.ts
- https://github.com/int04/ChatCmd/blob/bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c/chatgpt-extension/background.js

### Verified: ChatCMD already has automatic queued follow-up sending

- `ChatGptMessageQueuePanel` watches the queue and, when `canAutoSend` becomes true, sends the first queued message automatically and removes it after success.
- The UI describes queued mode as: the message will be sent automatically when the ChatGPT conversation is ready for the next message.
- `ChatGptTaskComposer` only enables auto-send when the task is not active/busy and the extension reports the exact ChatGPT conversation tab open and ready.
- Therefore this is an actual wake/resume primitive, not merely an operator notification.

Sources:
- https://github.com/int04/ChatCmd/blob/bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c/web/src/chatgpt/ChatGptMessageQueue.tsx
- https://github.com/int04/ChatCmd/blob/bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c/web/src/chatgpt/ChatGptConversation.tsx

### Verified: ChatCMD exposes a local queue API, but its UI owns the extension dispatch

- The local API has `POST /api/local/chatgpt/tasks/<taskId>/queue` and `POST /api/local/chatgpt/tasks/<taskId>/messages`.
- Local UI API calls use ChatCMD's encrypted local API session.
- The actual browser-extension dispatch is initiated by browser-side `dispatchChatGptRequest`; the inspected server path does not independently push a message into Chrome.
- Consequence: simply inserting a queue row is not sufficient unless the ChatCMD UI/bridge page is alive to observe the queue and call the extension.

Sources:
- https://github.com/int04/ChatCmd/blob/bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c/web/src/api.ts
- https://github.com/int04/ChatCmd/blob/bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c/web/src/apiCrypto.ts

### Verified: ChatCMD's own subagent lifecycle does not directly replace Orca/OMP

- `agent_subagent_start` uses model sampling supplied by the ChatGPT/MCP host; the docs explicitly say it must not start Codex or another local executor when sampling is unavailable.
- `agent_subagent_wait` is a parent-turn wait primitive. It is not an adapter for Orca `worker_done`.

Source:
- https://github.com/int04/ChatCmd/blob/bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c/docs/mcp_method.md

### Verified: current Orca knows completion durably but does not natively wake ChatGPT

Live runtime inspected on 2026-09-04:
- Orca app version: `1.4.197`
- Runtime state: ready / connected.
- Orchestration tracks `worker_done`, Tasks, Dispatches, Run Delivery, and supports blocking `check --wait`.
- Current coordinator guidance deliberately ends the ChatGPT turn after worker start and resumes when the user says `done`, `check`, or `continue` unless active waiting was explicitly requested.
- Orca's current automation surface is schedule-based (`hourly`, `daily`, `weekdays`, cron, RRULE). No native event-trigger/webhook automation for `worker_done` appeared in the live guide.

Sources:
- live `orca status --json`
- live `orca skills get orchestration`
- live `orca skills get orca-cli`

### Project observation: Hands is not the right owner for Orca lifecycle state

- Hands' repository contract says it should remain a thin CLI/MCP adapter around upstream ToolBridge capabilities and must not become a general orchestration layer.
- Hands does contain a small OS notification watcher for tunnel-down state, but no worker lifecycle/callback flow was found in the indexed graph.
- Therefore an Orca completion-to-ChatGPT bridge should not be implemented as a new general task/event subsystem inside Hands.

Sources:
- local `AGENTS.md`
- local `crate/src/watch.rs`
- local GitNexus query for `notification`

## Implications for this repo and workflow

### Current workflow

```text
ChatGPT -> Orca -> OMP worker
                  |
                  +-> worker_done -> Orca durable state

ChatGPT turn is already over.
User sends "Done" / "check".
ChatGPT starts a new turn and inspects Orca.
```

### Desired workflow

```text
ChatGPT -> Orca -> OMP worker
                  |
                  +-> worker_done
                       |
                       v
                completion bridge
                       |
                       v
              exact ChatGPT conversation
                       |
                       v
              automatic follow-up turn
                       |
                       v
              ChatGPT verifies/continues
```

### Smallest viable seam

The bridge should be owned at the boundary that already knows both identities:

1. Orca completion identity: `runId/taskId/dispatchId`.
2. ChatGPT conversation identity: conversation URL / ChatCMD task binding.
3. On accepted `worker_done`, enqueue one idempotent message such as:
   `Worker <taskId> completed. Inspect Orca delivery and continue coordination.`
4. Deliver it only when the exact ChatGPT conversation is idle/ready.
5. Deduplicate using the Dispatch/completion identity so retries cannot create multiple ChatGPT turns.

This is a narrow adapter. It does not require moving Hands execution, adding a second task database, or replacing Orca orchestration.

## Decision options

| Option | Removes manual `Done` | Fit for current workflow | Main cost |
| --- | --- | --- | --- |
| Keep current Hands + Orca | No | Stable but manual | User must wake ChatGPT |
| Replace Hands with ChatCMD only | No, not for Orca workers | Poor justification | Large stack change without solving event hookup |
| Move orchestration to ChatCMD subagents | Partially | Poor fit for OMP/OpenCode workflow | Loses Orca/local-worker model |
| Hands + Orca + narrow completion bridge using ChatCMD browser primitive | Yes | **Best technical fit** | Extension/UI dependency + small adapter |
| Add native Orca -> ChatGPT wake capability | Yes | **Best long-term fit** | Requires Orca/product integration rather than repo-only change |

## Risks / constraints

- ChatCMD's current bridge is browser-DOM based and therefore sensitive to ChatGPT UI changes.
- The exact ChatGPT conversation tab must currently be open and ready for `ChatGptTaskComposer` auto-send; if it is closed, the queue remains but the UI asks to reopen the conversation.
- The ChatCMD web UI owns the queue-to-extension dispatch. A fully unattended bridge should move that dispatch responsibility into a background-capable component or add a dedicated extension/local-runtime channel.
- Any completion bridge must be idempotent. A duplicated `worker_done` or transport retry must not produce duplicate ChatGPT turns.
- The completion event should only trigger after lifecycle state is terminal enough for the next coordinator action; a mere terminal becoming idle is insufficient.

## Recommendation

Do **not** switch from Hands to ChatCMD just to eliminate the manual `Done` step.

Use ChatCMD as proof-of-concept for the missing return path. The most valuable feature to extract/test is its exact-conversation browser bridge + queued auto-send behavior. The ideal production shape for this workflow is:

`Orca worker_done -> idempotent completion event -> ChatGPT conversation wake/resume`.

If that path is reliable in a small dogfood test, it is materially more valuable to this user than ChatCMD's broader PTY/task/UI feature set.

## Open questions

- Can current Orca expose or gain a native external completion-event subscription without polling `check --wait`?
- Can ChatCMD's extension be driven from a background-capable local channel without requiring the React task page to remain mounted?
- Does the ChatGPT UI/extension path reliably resume the same project/chat context across desktop/mobile synchronization for long-running worker completions?
