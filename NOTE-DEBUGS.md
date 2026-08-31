# Hands Debug Documentation (NOTE-DEBUGS.md)

This notebook records diagnosed, fixed, and verified bugs encountered during local development and operation of Hands on Windows with ChatGPT Web MCP.

---

## 1. HANDS-BUG-001 — `tunnel-client not found. brew install openai/tools/tunnel-client` on Windows

- **Date:** 2026-08-31
- **Severity:** High (Service failed to start on Windows)
- **Symptom:**
  Running `hands start` in PowerShell returned:
  ```text
  error: tunnel-client not found. brew install openai/tools/tunnel-client
  ```
- **Root Cause:**
  The installed `hands.exe` binary in `%LOCALAPPDATA%\Programs\hands\bin\hands.exe` was an outdated build (dated 2026-08-27) prior to the Windows platform refactoring. The old `which()` function used `:` (Unix colon separator) to parse `$env:PATH`, causing Windows `C:\...` paths to be corrupted, and lacked `.exe` extension lookup via `PATHEXT`.
- **Fix / Resolution:**
  Ran `.\install.ps1` to compile the latest release binary (`0.1.0` at `70af7ec`). The updated `service::profile::which` and `tunnel_client_bin()` properly inspect:
  1. Sibling directory of `hands.exe` (`parent.join("tunnel-client.exe")`).
  2. Managed prefix (`%LOCALAPPDATA%\Programs\hands\bin\tunnel-client.exe`).
  3. `PATH` using Windows semicolon `;` delimiter and `PATHEXT` (`.exe`, `.cmd`, `.bat`).
- **Verification:**
  `hands doctor` returns `[ok] binary C:\Users\monet\AppData\Local\Programs\hands\bin\tunnel-client.exe`.

---

## 2. HANDS-BUG-002 — Port 18780 Collision / Infinite Retry Loop on Parallel V2 Execution

- **Date:** 2026-08-31
- **Severity:** Medium (Prevented isolated parallel testing of Issue #37)
- **Symptom:**
  Starting `hands37` while production Hands main was running caused `tunnel-client` to log:
  ```json
  {"level":"ERROR","msg":"OnStart hook failed","error":"listen tcp 127.0.0.1:18780: bind: Only one usage of each socket address (protocol/network address/port) is normally permitted."}
  ```
  Followed by a repeated 5-second restart loop.
- **Root Cause:**
  A temporary PowerShell session function `hands37` was invoking `hands.exe start` (or `hands.exe run-tunnel`), which hardcodes loading the default profile `~/.config/tunnel-client/hands.yaml` configured for port `18780`. This bypassed the dedicated isolation wrapper `hands37.cmd` / `hands37.ps1` configured for port `18781`.
- **Fix / Resolution:**
  1. Removed the conflicting in-memory session function `Remove-Item Function:\hands37`.
  2. Executed through the isolated wrapper `hands37.cmd` / `hands37.ps1` which sets `HEALTH_LISTEN_ADDR=127.0.0.1:18781` and loads `hands37.yaml`.
- **Verification:**
  `tunnel-client doctor --profile-file F:\CodeBase\hands-issue-37\.grok-build\hands37\hands37.yaml` confirmed `health_listener: will bind http://127.0.0.1:18781` and `mcp_target: .../debug/hands.exe`.

---

## 3. HANDS-BUG-003 — MCP 502 Upstream Error Due to Escaped Double Quotes in YAML Command Path

- **Date:** 2026-08-31
- **Severity:** Critical (ChatGPT Web could not execute any MCP tools; returned `502 Upstream or external service errors`)
- **Symptom:**
  ChatGPT connected to the tunnel successfully, but any tool call (`workspace_info`, `initialize`, `tools/call`) failed with HTTP 502. `tunnel-client` logged:
  ```json
  {"level":"WARN","msg":"dispatcher received MCP upstream error; posted error response to control plane","rpc_method":"initialize","status_code":502,"failure_source":"client_internal","upstream_response_received":false}
  ```
- **Root Cause:**
  In `~/.config/tunnel-client/hands.yaml`, the MCP command was generated with redundant escaped inner quotation marks:
  ```yaml
  mcp:
    commands:
      - channel: main
        command: "\"C:/Users/monet/AppData/Local/Programs/hands/bin/hands.exe\""
  ```
  When Go's `yaml.Unmarshal` parsed this field, it preserved the literal double quotes as part of the binary path string (`"C:/Users/..."`). When `exec.Command` passed this to Windows `CreateProcess`, Windows failed to locate the file, causing stdio pipe creation to fail immediately and returning `upstream_response_received: false`.
- **Fix / Resolution:**
  Cleaned up `command:` in `hands.yaml` to specify the clean unescaped path:
  ```yaml
  mcp:
    commands:
      - channel: main
        command: "C:/Users/monet/AppData/Local/Programs/hands/bin/hands.exe"
  ```
  Restarted `tunnel-client` to reload the sanitized configuration.
- **Verification:**
  1. `tunnel-client.exe` (PID 67912) successfully spawned child process `hands.exe` (PID 68756) over stdio.
  2. `hands.exe` responded to MCP `initialize` and `workspace_info` JSON-RPC calls with 0ms latency.
  3. ChatGPT Web restored full live execution without 502 errors.
