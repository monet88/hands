# Orca / local-agent to ChatGPT completion bridge

## Conclusion

Orca 1.4.197 already contains almost the entire local-side half of the desired return path. Its agent integrations report semantic status through native hooks, the hook server normalizes those reports into `working`, `blocked`, `waiting`, and `done`, and the plugin system exposes live changes as the first-class `agent.status.changed` event. The Windows completion sound the user observes is downstream of this same agent-completion machinery: accepted `agent-task-complete` notifications are dispatched through Electron and then play the configured notification sound.

Therefore the most useful product is not a replacement for Hands or Orca orchestration. It is a small **Return Bridge** whose only responsibility is:

`local agent completion -> exact ChatGPT conversation -> synthetic continuation message`.

Orca can be used purely as an agent/event host; structured Orca Run/Task/Dispatch orchestration is not required for this feature.

A browser extension alone is not the clean local event receiver. The practical shape is an Orca event adapter plus a tiny local/native bridge plus a ChatGPT browser extension. For a first prototype, existing `agent.status.changed` is sufficient if the adapter only accepts `done` after observing a real `working` state for the same pane and deduplicates the transition. For a production-quality integration, Orca should expose a dedicated plugin event such as `agent.turn.completed` / `agent.task.completed` from its already-accepted completion seam, because the current plugin projection omits `sessionBoundary` even though raw agent status explicitly says session-boundary `done` must not be treated as a completed turn.

## Findings

### Orca has semantic agent-status hooks, including OMP

The shared status contract explicitly states that status comes from native agent hooks rather than terminal-title inference. Its canonical states are `working`, `blocked`, `waiting`, and `done`. The known provider set includes Codex, OpenCode, OMP, Pi and other agents.

The agent hook transport posts provider payloads to Orca's loopback hook server at `/hook/<provider>` with per-runtime hook coordinates and a token. OMP is included in the hook/provider routing surface.

Primary sources:
- `src/shared/agent-status-types.ts`
- `src/shared/agent-hook-relay.ts`
- `src/main/agent-hooks/hook-post-command.ts`
- `src/shared/agent-hook-listener/provider-event-routing.ts`

### Orca already has a multi-subscriber completion/status seam

`AgentHookServerListeners` exposes multi-subscriber APIs including `subscribeEnrichedStatus`, `subscribeStatusChanges`, provider-session subscriptions, and pane-status-clear subscriptions. This is an actual event source, not a UI polling heuristic.

Primary source:
- `src/main/agent-hooks/server/server-listeners.ts`

### Orca already publishes agent status to plugins

During main-process plugin initialization Orca subscribes to enriched hook status and emits `agent.status.changed` through `PluginService`. Restored/unconfirmed historical rows are filtered before the plugin event is emitted.

The v0 manifest event set contains:
- `worktree.created`
- `worktree.removed`
- `agent.status.changed`

The bundled example plugin subscribes to `agent.status.changed` with `orca.events.on(...)`. Manifest subscriptions are durable activation triggers; when an event arrives, Orca lazily starts an approved plugin worker and delivers the event.

Primary sources:
- `src/main/startup/main-process-plugins.ts`
- `src/shared/plugins/plugin-manifest.ts`
- `src/main/plugins/plugin-event-delivery.ts`
- `src/main/plugins/plugin-event-bus.ts`
- `examples/plugins/hello-orca/orca-plugin.json`
- `examples/plugins/hello-orca/main.mjs`

### Current plugin payload is intentionally narrow

The current `agent.status.changed` payload contains only:
- `worktreeId`
- `paneKey`
- `state`
- `receivedAt`

This is enough for an MVP state machine, but not enough for a perfect completion contract. In particular, the raw agent status model has a `sessionBoundary` flag and explicitly says session-boundary `done` must be ignored by completion consumers, while the plugin projection does not include that field.

Primary sources:
- `src/shared/plugins/plugin-events.ts`
- `src/shared/agent-status-types.ts`

### The Windows sound is downstream of a real completion event

The terminal notification dispatcher accepts the semantic source `agent-task-complete`. It sends the event to Electron's notification IPC; when delivery succeeds it invokes the desktop notification sound helper. The preload bridge loads the configured sound and plays it through an `Audio` instance. On non-macOS platforms, including Windows, the main process uses native Electron notification delivery.

The hook-completion notification coordinator routes accepted hook completion into `dispatchTerminalNotification(... source: 'agent-task-complete')`.

Primary sources:
- `src/renderer/src/hooks/agent-hook-completion-notifications.ts`
- `src/renderer/src/components/terminal-pane/use-notification-dispatch.ts`
- `src/renderer/src/lib/desktop-notification-sound.ts`
- `src/preload/api/notifications-bridge.ts`
- `src/main/ipc/notifications.ts`

### Plugin workers are already suitable as event adapters

Orca plugin workers are separate plain-Node child processes started lazily. They receive declared events and can call capability-gated host APIs. This is a good place for a small agent-completion adapter rather than modifying each agent or the terminal UI.

However plugin capability v0 does not yet include a first-class scoped network capability. The capability source explicitly notes that `net:fetch` is planned for a later phase. A production bridge should therefore use an official local/native handoff surface rather than depending on undocumented direct networking from plugin code.

Primary sources:
- `src/main/plugins/plugin-host-process.ts`
- `src/main/plugins/plugin-host-runtime.ts`
- `src/shared/plugins/plugin-capabilities.ts`

### Source version matches the installed Orca runtime

The inspected upstream repository reports Orca `1.4.197`, matching the user's connected runtime inspected in the same session. This avoids a version-skew assumption in the conclusions above.

Primary sources:
- `package.json`
- live `orca status --json`

## Recommended architecture

### Orca-backed path

```text
OMP / OpenCode / Codex / other supported agent
                  |
             native hook
                  v
          Orca hook server
                  |
       semantic status stream
                  v
     agent.status.changed plugin event
                  |
          Return Bridge adapter
                  |
       Native Messaging / local bridge
                  |
        Chrome / Edge extension
                  |
        exact chatgpt.com chat
                  |
   bounded synthetic continuation message
                  v
              ChatGPT
```

Orca orchestration is optional. Orca is useful here because it already normalizes many agent providers into one status contract.

### No-Orca path

Keep the browser/native Return Bridge unchanged and swap only the completion adapter:

```text
Codex hook -----\
OpenCode hook ---+--> normalized AgentCompletionEvent --> Return Bridge --> ChatGPT
OMP hook --------/
```

A process-exit watcher should only be a fallback. Interactive coding agents often finish a turn while the TUI process remains alive, so process exit is not a reliable turn-completion signal.

## Correlation contract

Detecting completion is not the hard part. Correctly returning to the originating ChatGPT conversation is.

The bridge should establish a binding when ChatGPT launches or assigns work:

```text
returnToken -> ChatGPT conversation/tab
returnToken -> Orca paneKey or local agent run id
```

Rules:

1. Never route based only on the currently focused ChatGPT tab.
2. Do not use worktree alone as identity; multiple agents can share one worktree.
3. Use an opaque return token or equivalent exact binding.
4. Deduplicate completion with a stable transition identity such as pane/run identity + accepted completion timestamp.
5. The injected message should be bounded and should not paste untrusted worker output. Prefer: `Local agent <id> completed. Inspect its result through the connected local tools and continue the task.`
6. `blocked`/`waiting` should not automatically resume as success; they may later become a separate question/attention channel.

## MVP

1. Build a development Orca plugin subscribing to `agent.status.changed`.
2. Maintain a tiny state machine per `paneKey`.
3. Accept completion only for `working -> done` in the same pane; ignore a lone initial `done`.
4. Deduplicate `paneKey + receivedAt`.
5. Send the accepted event to a local Return Bridge.
6. Browser extension lets the user bind the current ChatGPT conversation to the next agent/pane for the first test.
7. On completion, extension submits one narrow synthetic continuation message to that exact ChatGPT chat.
8. Validate with OMP first, then OpenCode/Codex.

This MVP proves the valuable behavior without changing Hands, Orca's orchestration model, or ChatCMD.

## Production hardening

The clean upstream Orca improvement is a dedicated accepted-completion plugin event, for example:

```text
agent.turn.completed
```

It should be emitted only after Orca's existing completion coordinator has rejected stale/session-boundary/duplicate transitions. A bounded payload should include an idempotent completion identity and enough attribution to bind the pane/run safely.

That is preferable to permanently teaching external consumers to reconstruct completion semantics from raw `agent.status.changed`.

## Implications for Hands

Hands does not need a task database, agent scheduler, or browser UI for this feature. It can remain a thin ChatGPT-to-local execution bridge. The Return Bridge is a separate reverse-path component:

```text
Hands:   ChatGPT -> local
Return:  local completion -> ChatGPT
```

They compose without turning Hands into an orchestration platform.

## Open questions

- What is the narrowest supported mechanism for an Orca plugin to hand an event to a local Native Messaging host under the current plugin capability model?
- Should the first prototype use an external sidecar, while a small Orca PR adds `agent.turn.completed` for the stable version?
- What exact identifier can the ChatGPT-side extension expose to the MCP/launch path so an Orca `paneKey` is bound to the correct conversation without relying on UI focus?
- How robustly can the ChatGPT extension submit a continuation message across current ChatGPT DOM changes and signed-in browser state?

## Sources

- https://github.com/stablyai/orca/blob/main/src/shared/agent-status-types.ts
- https://github.com/stablyai/orca/blob/main/src/main/agent-hooks/hook-post-command.ts
- https://github.com/stablyai/orca/blob/main/src/main/agent-hooks/server/server-listeners.ts
- https://github.com/stablyai/orca/blob/main/src/main/startup/main-process-plugins.ts
- https://github.com/stablyai/orca/blob/main/src/shared/plugins/plugin-manifest.ts
- https://github.com/stablyai/orca/blob/main/src/shared/plugins/plugin-events.ts
- https://github.com/stablyai/orca/blob/main/src/main/plugins/plugin-event-delivery.ts
- https://github.com/stablyai/orca/blob/main/examples/plugins/hello-orca/main.mjs
- https://github.com/stablyai/orca/blob/main/src/renderer/src/hooks/agent-hook-completion-notifications.ts
- https://github.com/stablyai/orca/blob/main/src/renderer/src/components/terminal-pane/use-notification-dispatch.ts
- https://github.com/stablyai/orca/blob/main/src/preload/api/notifications-bridge.ts
- https://github.com/stablyai/orca/blob/main/src/main/ipc/notifications.ts
- https://github.com/stablyai/orca/blob/main/src/shared/plugins/plugin-capabilities.ts
