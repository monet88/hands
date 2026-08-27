# Hands origin Windows test checklist

Goal: verify the upstream/origin core path on Windows before implementing Issues #2-#9.

```text
ChatGPT Web -> Secure MCP Tunnel -> tunnel-client.exe -> Hands -> filesystem/terminal -> Orca CLI
```

## 0. Existing prerequisites

- [x] Repo: `F:\CodeBase\hands`
- [x] Rust/cargo installed
- [x] Python installed
- [x] Git installed
- [x] Runtime Key created
- [x] Tunnel ID created
- [x] tunnel-client downloaded and SHA-256 verified
- [x] tunnel-client path: `F:\CodeBase\hands\.tools\tunnel-client\bin\tunnel-client.exe`

Do not use `hands setup`, `hands enable`, `hands start`, `hands stop`, or `hands disable` for this origin test.

## 1. Build Hands

```powershell
cd F:\CodeBase\hands
git clone --depth 1 https://github.com/xai-org/grok-build.git .grok-build
python .\scripts\inject.py "F:\CodeBase\hands" "F:\CodeBase\hands\.grok-build"
cargo build --release -p hands --manifest-path .\.grok-build\Cargo.toml
$Hands = "F:\CodeBase\hands\.grok-build\target\release\hands.exe"
& $Hands --version
& $Hands --help
```

- [x] Build PASS
- [x] `--version` PASS
- [x] `--help` PASS

## 2. Local filesystem smoke

```powershell
& $Hands --cwd "F:\CodeBase\hands" list
& $Hands --cwd "F:\CodeBase\hands" call read_file '{"target_file":"README.md"}'
```

- [x] Tool list appears
- [x] `read_file README.md` PASS
- [x] No Windows path panic

## 3. Start Hands HTTP MCP

```powershell
& $Hands --cwd "F:\CodeBase\hands" --http --port 8787
```

Endpoint: `http://127.0.0.1:8787/mcp`

- [x] Port 8787 binds
- [x] Hands stays running

## 4. Start tunnel-client in another PowerShell

```powershell
$TunnelClient = "F:\CodeBase\hands\.tools\tunnel-client\bin\tunnel-client.exe"
$env:CONTROL_PLANE_API_KEY="sk-..."
$env:CONTROL_PLANE_TUNNEL_ID="tunnel_..."
$env:MCP_SERVER_URL="http://127.0.0.1:8787/mcp"
& $TunnelClient doctor --explain
& $TunnelClient run --log.level=info --log.format=struct-text
```

- [x] `doctor` PASS
- [x] No 401/403
- [x] Tunnel stays running and becomes ready

## 5. Connect ChatGPT

Add a Tunnel connection in ChatGPT using the same Tunnel ID, then scan tools.

- [x] ChatGPT connects
- [x] Hands tools are discovered

## 6. ChatGPT functional tests

- [x] `workspace_info` returns `F:\CodeBase\hands`
- [x] `read_file` reads `README.md`
- [x] Search for `tunnel-client` works
- [x] Foreground terminal: `cmd.exe /c echo HANDS_WINDOWS_OK` -> `HANDS_WINDOWS_OK` (explicit `cmd.exe` terminal invocation bypassing bare `cmd` PATH shadow)
- [x] Direct Windows executable works: `C:\Windows\System32\cmd.exe /c echo HANDS_WINDOWS_OK` -> `HANDS_WINDOWS_OK`
- [x] PowerShell command executes
- [x] Terminal CWD is the pinned Workspace (`F:\CodeBase\hands`)
- [x] Background task returns a task ID
- [x] `get_task_output` returns task state/output
- [x] `kill_task` stops the owned background task (`running` -> `cancelled`)
- [x] Unrelated processes remain alive and owned descendant processes terminate (verified via JobObject process-tree isolation test)

## 7. Orca gate

- [x] `orca --version` works through Hands (resolved via `compose_host_path` User registry propagation)
- [x] `orca status --json` works through Hands
- [x] At least one Orca folder-context/runtime command works through Hands (`orca repo list --json`)

## 8. Controlled mutation

Create `.scratch/hands-origin-test.txt` with `CHATGPT_HANDS_WINDOWS_OK`, then read it back.

- [x] Write PASS
- [x] Read-after-write PASS (`CHATGPT_HANDS_WINDOWS_OK`)

## 9. Observed Windows blockers / required fixes

### 9.1 `cmd` is shadowed by an npm package (RESOLVED)

Fixed:
- Explicit `cmd.exe` terminal invocation (`cmd.exe /c ...`) bypasses third-party bare `cmd` PATH shadowing by explicitly specifying `cmd.exe` rather than bare `cmd`.
- Hands-owned code paths that require native CMD semantics (currently including `ui::open_browser()`) invoke `crate::host::native_cmd_exe()` to deterministically resolve `%ComSpec%` or `%SystemRoot%\System32\cmd.exe` directly rather than relying on bare `cmd` name resolution.
Verified: Regression test `test_cmd_path_shadowing_regression` places shadowing `cmd.ps1` and `cmd.cmd` earlier on PATH and proves native CMD execution reaches the genuine Windows command processor; `test_native_cmd_exe_resolution` confirms deterministic resolution.
### 9.2 Orca is not visible in the Hands process environment (RESOLVED)

Authoritative Orca path on Windows host: `C:\Users\monet\AppData\Local\Programs\orca\resources\bin\orca.exe`.
Fixed: Hands initializes with `compose_host_path()` which queries `HKCU\Environment\Path` from Windows registry, expands environment strings, and composes host user tool directories into the process PATH dynamically without hard-coding machine-specific paths.
Verified: `test_orca_resolution_through_hands` and direct `hands call run_terminal_cmd` for `orca --version`, `orca status --json`, and `orca repo list --json` all pass with exit code 0.

### 9.3 Remaining process-tree isolation coverage (RESOLVED)

Verified: Integration test `test_process_tree_isolation_on_kill_task` starts a Hands-owned background task that spawns a descendant child process alongside an independent control process. Calling `kill_task` terminates the entire owned JobObject process tree (confirming the descendant process is terminated) while the unrelated control process remains alive and `get_task_output` reports the expected cancelled state.

## 10. Decision

Final result: **GO for the full origin Windows path**.

Every gate has passed on the real Windows host:
- Pinned workspace resolution & file read/write
- Explicit `cmd.exe` terminal invocation & `native_cmd_exe()` resolution without PATH shadowing
- Host tool PATH propagation & Orca CLI execution
- Background task execution, output retrieval, and JobObject process-tree isolation on kill
- Release binary builds cleanly and the full relevant Hands regression suite passes.

## 11. Cleanup

Stop tunnel-client and Hands with `Ctrl+C`, then run:

```powershell
Remove-Item Env:CONTROL_PLANE_API_KEY
Remove-Item Env:CONTROL_PLANE_TUNNEL_ID
Remove-Item Env:MCP_SERVER_URL
```
