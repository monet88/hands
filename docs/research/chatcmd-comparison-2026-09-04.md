# ChatCMD vs Hands — architecture and product fit

## Conclusion

ChatCMD is broader and more productized, but it is not categorically better than Hands for Hands' current goal.

- **ChatCMD wins as a self-contained local-agent platform:** persistent PTY terminals, 46 MCP methods, per-profile tool permissions, local approval state, SQLite task/event persistence, process/Git primitives, a React supervision UI, optional ChatGPT browser bridge, and packaged desktop releases.
- **Hands wins on leverage and architectural economy for the stated product contract:** ChatGPT remains the brain; Hands is a thin CLI/MCP adapter that reuses `xai_grok_tools::ToolBridge`, keeps the tool surface small, and delegates public connectivity to the OpenAI tunnel client instead of implementing its own local-agent control plane.
- Replacing Hands with ChatCMD's architecture would violate Hands' current anti-overengineering contract and would duplicate state, supervision, permissions, persistence, orchestration, and UI that Hands deliberately does not own.

The best direction is to keep Hands thin and selectively borrow only contracts that improve safety or distribution without creating a second orchestration platform.

## Findings

### ChatCMD — verified facts

- Current inspected upstream snapshot: `int04/ChatCmd` main at `bf74767b1a72c9bb9b2fdc8d410839dcaf303c1c` (2026-09-03 latest inspected commit).
- Repository created 2026-08-26; latest release inspected is `v.26.09.03`, with packaged Windows x64/x86 and macOS Apple Silicon/Intel artifacts plus SHA-256 sums.
- The documented architecture is a multi-crate application: core domain contracts, SQLite storage, local runtime, MCP layer, top-level API/orchestration, React UI, and optional Chromium extension.
- The MCP catalog documents **46 methods** covering device, persistent PTY shell, filesystem/workspaces, Git, processes, skills, task state/artifacts, and agent lifecycle/orchestration.
- MCP access uses tokenized Streamable HTTP endpoints with per-profile tool allowlists. The runtime has a fail-closed policy engine supporting allow/deny/approval decisions.
- SQLite is the source of truth for task/session/turn state, approvals, terminal metadata/output, timeline events, artifacts, workspace projects, ChatGPT bridge state, and access-profile data.
- The PTY runtime keeps persistent sessions, replay buffers, idempotency/in-flight request state, concurrency limits, process metadata, and a per-session reaper.
- The optional ChatGPT browser extension drives an already signed-in ChatGPT DOM and is explicitly documented as brittle against UI/selector changes.

Primary sources:
- https://github.com/int04/ChatCmd
- https://github.com/int04/ChatCmd/blob/main/README.md
- https://github.com/int04/ChatCmd/blob/main/docs/ARCHITECTURE.md
- https://github.com/int04/ChatCmd/blob/main/docs/mcp_method.md
- https://github.com/int04/ChatCmd/blob/main/crates/chatcmd-runtime/src/policy.rs
- https://github.com/int04/ChatCmd/blob/main/crates/chatcmd-runtime/src/shell.rs
- https://github.com/int04/ChatCmd/blob/main/SECURITY.md

### Hands — verified project observations

Current local snapshot inspected:

- Workspace: `F:\CodeBase\hands`
- Branch: `feat/issue-46-final-windows-chatgpt-soak`
- HEAD: `763c3db1ce4b570cddba8c65305db61e1b758ccc`
- Existing unrelated WIP before this research: modified `AGENTS.md`.

Architecture and behavior:

- Hands' explicit repository contract is to remain a **thin CLI/MCP adapter around upstream capabilities** and to keep MCP -> `xai_grok_tools::bridge::ToolBridge` as the default execution path.
- `host.rs` constructs a `ToolBridge` with the upstream filesystem/terminal tools and only narrow Hands-owned additions.
- The current branch exposes a compact surface: upstream read/search/edit/task/terminal tools plus Hands-owned `workspace_info`, `set_workspace`, `list_terminal_tasks`, and native argv `run_command`.
- `run_command` deliberately bypasses shell interpretation and bounds stdin, runtime, raw output, and summarized output.
- `list_terminal_tasks` projects a bounded recovery snapshot and excludes environment/secrets; regression tests verify recovery, kill, completed-task discovery, bounded command summaries, and preservation of the original execution identity when foreground execution auto-backgrounds.
- Public-transport tests exercise real process boundaries for MCP stdio and HTTP, including workspace switching, terminal execution, literal argv handling, explicit workdirs, and timeout semantics.
- Hands does not own a database-backed task graph, browser-extension conversation bridge, permission-profile database, persistent PTY UI, or general sub-agent orchestration layer.

Primary local sources:
- `AGENTS.md`
- `docs/integration-policy.md`
- `crate/src/host.rs`
- `crate/src/mcp.rs`
- `crate/src/run_command.rs`
- `crate/tests/task_recovery.rs`
- `crate/tests/public_transports.rs`

## Comparison

| Area | ChatCMD | Hands | Assessment for Hands' goal |
| --- | --- | --- | --- |
| Product breadth | Full local-agent platform | Thin local coding bridge | ChatCMD has more features; Hands is intentionally narrower |
| Runtime ownership | Reimplements filesystem, PTY, Git, process, policies | Reuses ToolBridge/upstream runtime | Hands has better leverage and less duplicated machinery |
| Terminal model | Persistent interactive PTY with replay/resize/signals | Foreground/background command tasks + recovery | ChatCMD is stronger for interactive terminal UX; Hands is simpler for coding-agent command execution |
| Permissions | Per-profile allowlists + local approval policy | ChatGPT tool annotations/confirmation plus configured tool allowlist | ChatCMD has stronger defense-in-depth if serving multiple clients |
| Persistence | SQLite task/event/artifact/session state | Lightweight workspace/config + upstream task state | ChatCMD is stronger for a standalone control plane; persistence would be scope expansion for Hands |
| Git/process tools | Dedicated structured methods | Native argv `run_command` / shell path | ChatCMD has richer schemas; Hands avoids duplicate tool families |
| UI/observability | React console, timeline, diffs, PTY, approvals | Small config/status UI | ChatCMD clearly wins as an operator console |
| ChatGPT integration | MCP plus optional DOM extension | Native ChatGPT MCP tunnel path | Hands is cleaner for ChatGPT-only use; ChatCMD extension adds capability and selector-maintenance risk |
| Connectivity | Self-hosted Streamable HTTP + user tunnel/reverse proxy | OpenAI secure tunnel-client path; local HTTP mainly diagnostics/tests | Hands owns less public-edge security machinery |
| Distribution | Desktop release artifacts + checksums | Source/install-oriented; no latest GitHub release found | ChatCMD currently wins packaging/productization |
| Code/architecture complexity | Multiple crates + DB + API + UI + extension + orchestration | Small adapter layer around upstream | Hands is substantially easier to reason about if its scope remains narrow |

## What Hands should borrow

1. **Release packaging discipline** — signed/checksummed artifacts, repeatable Windows/macOS release jobs, and a clear release procedure are valuable without changing runtime architecture.
2. **Security/threat-model documentation** — ChatCMD's explicit trust-boundary table, bearer-token discussion, path/approval threat list, and private vulnerability-reporting policy are strong documentation patterns.
3. **Permission profiles only if Hands becomes multi-client** — per-client least-privilege tool profiles are worthwhile if Hands intentionally supports several independent MCP clients. They are unnecessary state today if ChatGPT is the only consumer.
4. **Persistent PTY only after a reproduced workflow need** — interactive REPL/SSH/dev-server control would justify it. Do not add it merely to match ChatCMD; Hands' current background-task contract is simpler and already tested.
5. **Do not copy ChatCMD's task DB / agent lifecycle / browser extension / sub-agent orchestration into Hands** unless Hands' product goal changes. Those are the main sources of ChatCMD's power and also the reason it is a much larger system.

## Open questions

- Whether Hands should become a general MCP runtime for multiple web AIs or remain ChatGPT-first. This single product decision changes the value of permission profiles, self-hosted HTTP authentication, and persistent task state.
- Whether users have a concrete need for truly interactive persistent PTYs beyond the current foreground/background terminal-task model.
- Whether packaged desktop releases are a desired distribution channel for Hands; this is currently the clearest area where ChatCMD is more productized without forcing a runtime redesign.
