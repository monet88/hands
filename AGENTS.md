# Hands — Agent & Operator Guide

Unofficial ChatGPT connector. Local coding tools over MCP. No local LLM required.

## 1. Repository Map & Context Pointers

Load branch-specific context only when the task touches that area.

| Path | Use when |
|---|---|
| `crate/src/main.rs` | CLI parsing/dispatch or command-surface changes |
| `crate/src/doctor.rs` | `hands doctor`, diagnostics, health checks, or JSON diagnostic output |
| `crate/src/service.rs` | supervisor/service lifecycle, runtime readiness, or platform service behavior |
| `crate/src/host.rs` | workspace resolution, PATH/process environment, or host execution behavior |
| `scripts/inject.py` | build integration with the pinned Grok Build checkout, especially Windows patches |
| `scripts/smoke_windows.ps1` | routine Windows code/script verification without install, release build, or service restart |
| `scripts/e2e_gate_windows.ps1` | deterministic Windows MCP acceptance and exact-head ChatGPT evidence gate |
| `docs/hands-debug-notes.md` | local debug notebook when present; known Windows runtime, quoting, pipes, connector, and process edge cases |
| `docs/chatgpt-web-e2e.md` | manual ChatGPT Web/Tunnel verification; load only for connector/E2E work |

## 2. Fast Install

### Windows (PowerShell)

```powershell
.\install.ps1
```

### macOS / Linux (Bash)

```bash
./install.sh
```

### One-shot Non-interactive Setup

```bash
export CONTROL_PLANE_API_KEY="sk-..."          # Restricted: Tunnels Read + Use
export CONTROL_PLANE_TUNNEL_ID="tunnel_..."
hands setup
```

## 3. Core CLI Commands

| Action | Command | Description |
|---|---|---|
| **First-time Setup** | `hands setup` | Interactive TTY checklist; saves keys to OS Credential Manager |
| **Check Status** | `hands status` | Check active workspace, pin, and tunnel readiness |
| **Status (JSON)** | `hands status --json` | Structured output for scripting/agents |
| **Diagnostics** | `hands doctor` | Comprehensive local host & config diagnostics |
| **Diagnostics (JSON)** | `hands doctor --json` | Machine-readable local diagnostic object |
| **Pin Workspace** | `hands use [path]` | Pin folder (ChatGPT uses this on next tool call) |
| **Start Service** | `hands start` | Start tunnel supervisor background service |
| **Auto-boot Enable** | `hands enable` | Register Task Scheduler / LaunchAgent auto-start |
| **Stop Service** | `hands stop` | Stop tunnel supervisor |
| **Disable Service** | `hands disable` | Unregister auto-start service |
| **Web Dashboard** | `hands config --open` | Serve and open config UI at http://127.0.0.1:8787/ |
| **List Tools** | `hands list` | Print MCP tool definitions |
| **Test Tool Call** | `hands call <tool> <json>` | Directly test a tool from CLI |
| **MCP stdio** | `hands` (no args) | Standard IO MCP server (launched by tunnel-client) |

## 4. Testing & Verification

### Required Verification

Before finishing any change, run the applicable verification. For Windows code or script changes, the baseline gate is:

```powershell
.\scripts\smoke_windows.ps1
```

The smoke runner re-injects the crate into an existing Grok Build checkout, runs `cargo check -p hands`, and executes `hands doctor --json` through the debug build. It does **not** clone/fetch Grok Build, run `install.ps1`, build `--release`, or start/stop/restart the supervisor/tunnel.

Behavior changes additionally require the relevant focused/regression tests; the smoke gate does not replace them. For documentation-only changes, `git diff --check` is the minimum required verification.

### Direct CLI Tool Tests on PowerShell

Prefer the MCP tools directly. If a task specifically needs `hands call`, do not hand-inline JSON through nested PowerShell/CMD layers. Put the payload in a temporary JSON file and use the argv-safe helper:

```powershell
$Payload = New-TemporaryFile
@{ target_file = "README.md" } | ConvertTo-Json -Compress | Set-Content -Path $Payload -Encoding utf8
python .\scripts\call_tool_json.py read_file $Payload
Remove-Item $Payload
```

### ChatGPT Web / Tunnel E2E

Only load `docs/chatgpt-web-e2e.md` when the change affects the connector, tunnel, MCP tool discovery, service lifecycle, workspace propagation, or real ChatGPT Web behavior. Use `scripts/e2e_gate_windows.ps1` for the deterministic/evidence-backed gate described there.

## 5. Operational Rules

- **PowerShell Invocation**: when executing absolute paths with spaces, use the call operator: `& 'C:\path\to\app.exe' args`.
- **Runtime Safety**: while a Hands connection is active, do not run `cargo build --release`, `install.ps1`, or stop/restart the supervisor/tunnel unless the task explicitly requires it and the operator has been warned first.
- **Output Truncation**: output > 40KB is saved to a temp log file; query narrower commands when possible.
- **Credential Storage**: keys are stored in the OS credential store (Windows Credential Manager / macOS Keychain). Do not commit API keys.
