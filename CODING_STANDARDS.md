# Hands — Coding Standards

Practical, repo-specific standards for the `hands` crate. Grounded in the existing
codebase; follow these unless a change is explicitly directed otherwise.

## 1. Layout & source of truth

- **Canonical source lives in `crate/`.** Never edit the injected copy directly.
- The **build/test target** is the injected checkout at
  `.grok-build/crates/codegen/hands/`. After editing `crate/`, sync:
  ```bash
  python scripts/inject.py F:/CodeBase/hands F:/CodeBase/hands/.grok-build
  ```
  Then run cargo with `cwd` at `.grok-build`:
  ```bash
  cargo check -p hands
  cargo test  -p hands
  cargo build -p hands
  ```
  The debug binary lands at `.grok-build\target\debug\hands.exe`.
- `crate/` is the single source of truth; `.grok-build/` is generated and must not
  be edited by hand. Verify with `git diff` after any edit — an edit in this repo
  has silently mangled unrelated functions (duplicated blocks, removed adjacent
  lines). Prefer full-file `write` or one-shot string replacement over the block
  rewrite mode.

## 2. Language, edition & dependencies

- **Rust**, workspace-managed edition and lints (`edition.workspace = true`,
  `[lints] workspace = true`).
- **Tokio** async; enabled features are explicit in `crate/Cargo.toml`:
  `macros`, `rt-multi-thread`, `io-std`, `net`, `sync`. Do not add tokio features
  casually.
- **No extra crates for stdio/HTTP/net.** The MCP transport is hand-rolled
  (`mcp.rs` comment: "No extra crates"). Reach for stdlib before adding a dep.
- Dependency set (workspace): `dirs`, `dunce`, `libc`, `serde_json`, `tokio`,
  `xai-grok-tools` (path dep). Dev-only: `parking_lot` (test env lock).
- Function signatures use `Result<_, String>` with `map_err(|e| format!("...: {e}"))`.
  No `anyhow`/`thiserror`.

## 3. Architecture invariants

These are load-bearing; do not break them.

- **One authoritative execution path.** `crate/src/tool_engine.rs::ToolEngine` is the
  single seam owning: tool allowlisting, virtual-tool injection (`workspace_info`),
  native tools (`run_command`), Workspace-aware bridge caching, and
  execution/result/error shaping.
- **`host.rs::build_bridge` is the only bridge construction unit.** No other path
  builds a `ToolBridge`. No duplicate bridge paths exist in production.
- **Transports are protocol-only adapters.** `main.rs` (CLI `run_call_cli` /
  `list_tools_cli`) and `mcp.rs` stdio + HTTP (`handle_rpc` / `handle_http`) all
  route through `ToolEngine`. Never add a second path that bypasses `ToolEngine`.
- **Bridge cache: no lock held across an await.** Use double-checked locking:
  check cache under lock → drop lock → `build_bridge(cwd).await` → re-lock and
  assign only if still absent.

### Module responsibilities

| Module | Owns |
|---|---|
| `main.rs` | CLI dispatch: argument parsing, `run_call_cli`, `list_tools_cli`, `resolve_json_argument` |
| `mcp.rs` | JSON-RPC over stdio (newline-delimited) and Streamable HTTP `POST /mcp`; `handle_rpc` / `handle_http` |
| `tool_engine.rs` | `ToolEngine` seam, `ToolCallResult`/`ToolContent`, allowlist, cache |
| `host.rs` | workspace resolution (`resolve_workspace`), `build_bridge`, host execution |
| `run_proc.rs` | Native process execution: spawn, drain, tree-kill, Windows Job Object |
| `service.rs` | Supervisor/service lifecycle |
| `doctor.rs` | `hands doctor` diagnostics + JSON output |
| `setup.rs`, `secrets.rs` | First-time setup, OS credential store |
| `testenv.rs` | `#[cfg(test)]`-only shared hermetic env isolation (`isolate_env`) |

## 4. Error handling & result shaping

- Errors are `String`-based, produced by `format!`; never swallow errors that
  prevent data loss. Report pipe read errors, don't silently drop them.
- JSON-RPC dispatch errors are `Result<T, (i64, String, Value)>` =
  `(code, message, data)`, rendered by `rpc_error` / `rpc_error_with_data`.
- **CLI error prefix contract.** The top-level CLI printer prepends `error: `.
  `run_call_cli` therefore **strips a leading `"error: "`** from the rendered
  message before returning `Err`, so stdout/stderr never double-prefixes. Keep
  this normalization in the shared CLI adapter — not per-caller.
- **Reject, don't drop.** Invalid input (e.g. non-string argv elements) must
  produce an error result (`isError`), never be silently skipped.
- Never suppress a symptom (warning/exception/edge case) to pass a check — fix the
  root cause at the lowest shared layer.

## 5. CLI vs MCP schema contract

This is an **intentional, documented adapter difference** — keep them separate:

- **MCP wire format** (`tools/list` schema) uses `inputSchema`.
- **CLI `hands list` output** exposes the schema under `parameters` (legacy CLI
  contract). Do not unify them to one key.

`list_tools_cli` maps the engine's MCP tool definitions to the `parameters` form
without diverging from `ToolEngine` as the source of truth. `test_list_tools_cli_parameters_contract`
locks that `parameters` (not `inputSchema`) is emitted on the CLI.

## 6. Concurrency

- Prefer `tokio::sync::Mutex` in async paths; `std::sync::Mutex` only in short,
  non-await-scoped sections.
- **Never hold a lock across `.await`.** See the bridge-cache invariant (§3).
- `AtomicU64` (e.g. `call_seq`) for counter state; avoid a mutex just to increment.
- Process-global env mutation is serialized by a test-only lock (see §7); never
  mutate process env in production code without a restore path.

## 7. Testing

- **Use the shared `testenv.rs::isolate_env(name)`** for any test touching
  `HANDS_CONFIG_DIR`, `HANDS_WORKSPACE`, `GROK_HARNESS_WORKSPACE`, or workspace
  resolution. It returns a `(MutexGuard, EnvGuard)`; the `EnvGuard` restores env
  vars on drop and removes the temp config root. Never re-copy `EnvGuard`.
- Tests that mutate process-global env are **serialized** by the global
  `TEST_LOCK` in `testenv.rs` — parallel env mutation causes flaky/solo-only
  failures. A full-suite env-mutation failure may be this precondition; rerun a
  single test before diagnosing.
- **No live remote connector required** in unit tests — hermetic and deterministic.
- **Parity is locked by tests** (keep all three green):
  - `test_cli_and_mcp_listing_tool_names_parity` — CLI vs MCP tool names/schema
  - `test_cli_call_parity_with_mcp_engine` — CLI call → same engine behavior
    (virtual + bridge + native + error case)
  - `test_mcp_http_dispatch_matches_handle_rpc_parity` — HTTP `POST /mcp` vs
    `handle_rpc` (stdio serves the same function per line)
- A regression test that does not reach the real production branch is worthless.
  Extract the dispatch logic into a testable function and drive it with a real
  child/subsystem, not a mock that can't fail.
- Test helpers that are test-only (`ToolContent::text`) are `#[cfg(test)]`.

## 8. Process execution invariants (`run_proc.rs`)

These are platform-critical; they have caused real hang/deadlock bugs.

- **Capture the child PID immediately after spawn.** `tokio::process::Child::id()`
  returns `None` after reap — the Unix `killpg` must use the pre-reap PID.
- **`stdin = Stdio::null()`** or the child inherits the MCP stdio transport.
- **Drain stdout/stderr up to the deadline before reaping the root.** Until reaped
  (running or zombie), the child PID/PGID stays allocated in the kernel table.
- **Kill the tree before reaping the root.** In `terminate_tree`, call
  `libc::killpg(pid, SIGKILL)` (Unix) / `JobObject::terminate()` (Windows)
  **before** `child.kill().await`/`child.wait().await`. Killing the root first
  reaps it and makes a subsequent `killpg(child_pid)` target a stale/reusable PGID.
- **Windows spawn→assign race is closed by spawning suspended.**
  `CREATE_SUSPENDED` (`0x4`, not `0x0800_0004`), create+assign the Job Object
  while suspended, then resume the initial thread via
  `OpenThread(THREAD_SUSPEND_RESUME, ...)` + `ResumeThread` — **not** `OpenProcess`
  (which opens a process by PID, not a thread by ID). Enumerate threads with
  `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD)` matching `th32OwnerProcessID`.
- **Windows API failure values:** `CreateToolhelp32Snapshot` returns
  `INVALID_HANDLE_VALUE` (`-1` as `HANDLE`) on failure, not `NULL`; `ResumeThread`
  returns `u32::MAX` on failure. If resume fails after `CREATE_SUSPENDED`,
  terminate and reap immediately with an error — don't block until timeout.
- **Bounded drain preserves partial output.** Stream into a shared capture so an
  abort on timeout still keeps bytes captured before the abort.
- A `JoinHandle` can panic "polled after completion" if polled a second time after
  its output was taken; consume it once via the correct arm.
- Non-Windows builds need a `terminate()` stub for the Job Object type to compile.

## 9. Verification gates

- **Windows code/script change** → baseline gate:
  ```powershell
  .\scripts\smoke_windows.ps1
  ```
  It re-injects the crate, runs `cargo check -p hands`, and executes
  `hands doctor --json` through the debug build. It does **not** clone/fetch
  Grok Build, run `install.ps1`, build `--release`, or start/stop/restart the
  supervisor/tunnel.
- **Behavior change** → the applicable focused/regression tests in addition to the
  smoke gate (the smoke gate does not replace them).
- **Documentation-only change** → `git diff --check` is the minimum.
- **Run tests from `.grok-build`**: `cargo test -p hands` (or `-p hands <filter>`).
- **GitNexus:** before editing a symbol, run `impact({target, direction:"upstream"})`
  on it (pass `repo: "hands"` — multiple repos are indexed, omitting it errors).
  Never edit before checking the blast radius; never rename with find-and-replace.
  Before committing, run `detect_changes()`. If the index is stale (MEDIUM/"not
  found" for new symbols), refresh with `node .gitnexus/run.cjs analyze` from the
  repo root.

## 10. Editing safety

- **Source of truth `crate/`; build target `.grok-build/`.** Edit `crate/`, inject,
  then verify. Never edit `.grok-build/`.
- The block/selection rewrite mode of the edit tool has **mangled files** in this
  repo — duplicated whole blocks, removed adjacent `use`/`const` lines, orphaned
  blocks. Reliable fallbacks:
  1. full-file rewrite via `write`;
  2. surgical single-line/region replace via a one-shot python heredoc;
  3. `sed -i` for single-line patterns.
- **Always `git diff` after any edit.** A mangled edit can silently destroy
  unrelated functions. Verify the diff is minimal before injecting.

## 11. Taste

- **Ponytail ladder:** does it need to exist (YAGNI) → reuse existing code →
  stdlib first → native platform feature → existing dep → one line → minimum
  working code.
- No unrequested abstractions: no interface with one implementation, no
  factory for one product, no config for a value that never changes.
- **Deletion over addition.** Boring over clever — clever is what someone decodes
  at 3am. Shortest working diff wins, once the problem is understood.
- Deliberate simplifications with a known ceiling get a `ponytail:` comment
  naming the ceiling and the upgrade path.
- Doc comments `///` on public items; module header `//!`; keep comments to *why*,
  not *what*.

## 12. Commit conventions

- Conventional Commits: `type(scope): subject` (e.g. `fix(cli): ...`,
  `feat: ...`, `refactor(test): ...`, `test(...)`).
- Subject ≤ 50 chars. Body only when the *why* isn't obvious from the diff.
- Never commit API keys/credentials (OS credential store, not the repo).
