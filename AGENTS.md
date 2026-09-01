# Hands — agent install

Unofficial ChatGPT connector. Local coding tools. No LLM on this machine.

```bash
# from a clone
./install.sh

# after install
export CONTROL_PLANE_API_KEY="sk-..."          # Restricted: Tunnels Read + Use
export CONTROL_PLANE_TUNNEL_ID="tunnel_..."
hands setup                                    # TTY checklist; non-interactive if env keys are set
hands status --json
hands use /path/to/repo
```

MCP stdio (what tunnel-client launches): `hands` with no args.

Config UI: `hands config` → http://127.0.0.1:8787/

After Scan tools: ChatGPT **Settings → Apps → Hands → Never ask** (or **Always allow** on the first write) so coding does not stop on Confirm. Developer Mode remembers approve only for that conversation.

Do not commit API keys. Not an official OpenAI or xAI product.

## Agent skills

### Issue tracker

GitHub Issues via `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Canonical 5-role triage vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout (`CONTEXT.md` + `docs/adr/`). See `docs/agents/domain.md`.

## Architecture & Anti-Overengineering Guardrails

Hands should stay a thin CLI/MCP adapter around upstream capabilities. Prefer the smallest reliable change that preserves the direct execution path; do not add architecture for failures that have not been reproduced.

### 1. ToolBridge-first execution
- Keep MCP -> `xai_grok_tools::bridge::ToolBridge` as the default tool-execution path.
- Reuse existing repository and upstream behavior before adding adapters.
- Add a narrow tool-specific adapter only when a demonstrated requirement cannot be satisfied through the existing bridge/upstream surface. Do not introduce a second general execution engine, router, or orchestration layer.

### 2. External supervision, lean Hands process
- Long-lived supervision belongs to OS/external mechanisms such as `launchd`, `systemd`, Windows launcher/startup scripts, and `tunnel-client`, not to a watchdog loop inside the Hands process.
- Rust may configure or invoke those external integrations, but Hands must not become an in-process restart/health daemon unless an explicit, reproduced requirement cannot be met externally.
- Keep lifecycle and environment ownership at clear process boundaries; do not duplicate supervision state inside the MCP command path.

### 3. Standard process primitives first
- Prefer upstream behavior plus `std::process` / `tokio::process` for child execution and lifecycle management.
- Add platform-specific process control only when a red-capable regression proves direct-child handling is insufficient, and keep that remediation at the narrowest owning seam.
- Keep the MCP protocol path direct and responsive: do not repurpose MCP stdio for child I/O or add polling/coordinator middleware merely to emulate synchronous execution.

### 4. Ponytail ladder
- Choose implementations in this order: YAGNI -> existing repo/upstream -> stdlib/runtime -> native platform feature -> existing dependency -> minimum new code.
- Prefer local, explicit changes over speculative reusable layers. Extract an abstraction only after concrete duplication or independent variation demonstrates that it is needed.
- Keep platform/config parser quirks in their owning docs or regression tests instead of accumulating bug-specific prohibitions here.

<!-- gitnexus:start -->
# GitNexus - Code Intelligence

Use GitNexus as a navigation and blast-radius aid; current source and tests remain the source of truth. Do not embed symbol, relationship, or flow counts here because the index changes with the codebase.

Before graph-dependent work, check index freshness and refresh it if stale. Use the installed workflow Skill and current GitNexus CLI help as command authority rather than copying CLI syntax into this file.

## Route by task

| Task | Skill |
| --- | --- |
| Understand unfamiliar architecture or execution flow | `~/.agents/skills/gitnexus-exploring/SKILL.md` |
| Assess blast radius before a risky/shared-seam/public-interface change | `~/.agents/skills/gitnexus-impact-analysis/SKILL.md` |
| Trace a bug or regression | `~/.agents/skills/gitnexus-debugging/SKILL.md` |
| Rename, extract, split, or refactor symbols | `~/.agents/skills/gitnexus-refactoring/SKILL.md` |
| Tool/resource/schema reference | `~/.agents/skills/gitnexus-guide/SKILL.md` |
| Index, status, refresh, or CLI operations | `~/.agents/skills/gitnexus-cli/SKILL.md` |

## Required gates

- For risky shared seams, public interfaces, refactors, or behavior with multiple callers, run upstream impact analysis before editing and verify the relevant source directly.
- Surface HIGH/CRITICAL impact before proceeding. Treat `UNKNOWN` as unresolved: confirm with source/text search rather than reading an empty caller set as safe.
- Before commit/readiness for a non-trivial code change, run graph change analysis against the intended base. `partial` or `truncated` output is incomplete evidence, not a clean result.
- For trivial docs/config-only edits, graph impact/change analysis is optional unless the edit changes executable behavior or an agent/runtime contract.
- Prefer graph-aware rename/refactor tooling when supported; do not use blind find-and-replace for semantic symbol changes.
- If GitNexus and direct source evidence disagree, trust the source, report the index limitation, and refresh/re-query when useful.

<!-- gitnexus:end -->
