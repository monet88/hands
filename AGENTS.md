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

### 5. `.grok-build` is read-only upstream state
- Treat `.grok-build/` as a disposable checkout/injection sandbox sourced from `xai-org/grok-build`. Never edit files under `.grok-build/` directly, even for quick debugging or temporary workarounds.
- Use `.grok-build/` only for upstream source inspection and as the generated target for injection, build, and test workflows. Automated injection/bootstrap may wipe or rewrite `.grok-build/crates/codegen/hands/` at any time.
- Make required changes in Hands-owned source, tests, or injection/build logic. If a fix genuinely belongs upstream, change the upstream repository separately and update the tracking revision cleanly.
- Direct edits under `.grok-build/` invalidate review/test evidence because they are non-authoritative and disposable; restore or regenerate the sandbox before taking evidence.
<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **hands** (398 symbols, 1204 relationships, 20 execution flows).

> Index stale? Run `node .gitnexus/run.cjs analyze --index-only` from the project root — it auto-selects an available runner. No `.gitnexus/run.cjs` yet? Bootstrap with `npx`, `bunx`, or `pnpm dlx` — e.g. `bunx gitnexus@latest analyze` (npm 11 npx crash; #1939).

## Always Do

- **MUST run impact before editing.** Use `impact({target: "symbolName", direction: "upstream"})` or `node .gitnexus/run.cjs impact "symbolName" --direction upstream --repo .`; report callers, processes, and risk. Never substitute grep for graph analysis.
- **MUST analyze graph changes before committing.** Use `detect_changes({scope: "all"})` (MCP) or `node .gitnexus/run.cjs detect-changes --scope all --repo .` (CLI fallback). `partial: true` or `truncated: true` is not a clean check — a zero means unseen, not unaffected; re-run it. For regression review: `detect_changes({scope: "compare", base_ref: "main"})` or `node .gitnexus/run.cjs detect-changes --scope compare --base-ref "main" --repo .`.
- MUST warn on HIGH/CRITICAL `risk` pre-edit; never use `riskSharedAxes` to waive a HIGH/CRITICAL `risk` warning. Compare File/symbol: MCP File omits axes; Graph-RAG expands File.
- **MUST treat `risk: UNKNOWN` as unresolved, not as low.** An empty caller set is not evidence the symbol is unused — it can also mean the callers are not resolvable by the index (plain-object property access, dynamic dispatch, cross-language calls). `impact` pairs `UNKNOWN` with a `riskNote` saying so. Confirm with a text search before treating the symbol as safe to change or delete; do not proceed on the strength of a zero.
- **MUST use `query({search_query: "concept"})` for concepts/flows, `context({name: "symbolName"})` for a named symbol, or `impact` for blast radius, on read-only callers, dependencies, imports, or execution flow.** Graph first; text search only for empty/`UNKNOWN`/literals.
- For security review, `explain({target: "fileOrSymbol"})` lists taint findings (source→sink flows; needs `analyze --pdg`).

## Never Do

- NEVER edit a function, class, or method before MCP/CLI impact analysis.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis, and never read `UNKNOWN` as an all-clear — it means the walk could not answer, which is the one verdict that requires confirming by other means.
- NEVER rename symbols with find-and-replace — use `rename` which understands the call graph.
- NEVER commit before MCP/CLI graph change analysis.

## Resources

| Resource | Use for |
| --- | --- |
| `gitnexus://repo/hands/context` | Codebase overview, check index freshness |
| `gitnexus://repo/hands/clusters` | All functional areas |
| `gitnexus://repo/hands/processes` | All execution flows |
| `gitnexus://repo/hands/process/{name}` | Step-by-step execution trace |

<!-- gitnexus:end -->
