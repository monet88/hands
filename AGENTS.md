# Hands — Agent & Operator Guide

Unofficial ChatGPT connector. Local coding tools over MCP. No local LLM required.

## 1. Fast Install

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

---

## 2. Core CLI Commands

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
| **List Tools** | `hands list` | Print all 12 MCP tool definitions |
| **Test Tool Call** | `hands call <tool> <json>` | Directly test a tool from CLI |
| **MCP stdio** | `hands` (no args) | Standard IO MCP server (launched by tunnel-client) |

---

## 3. Testing & Verification Checklist

### Step 1: Local Smoke Test (Offline)
```powershell
# 1. Verify binary version
hands --version

# 2. Check tool definitions
hands list

# 3. Test direct tool execution
hands call workspace_info '{}'
hands call read_file '{"target_file":"README.md"}'
hands call run_terminal_cmd '{"command":"cmd.exe /c echo TEST_OK"}'
```

### Step 2: Tunnel Service Test
```powershell
# 1. Start the service
hands start

# 2. Check health status
hands status
# Expected output: tunnel ready http://127.0.0.1:18780/ui

# 3. Test health probe endpoint
curl http://127.0.0.1:18780/readyz
# Expected: ready
```

### Step 3: ChatGPT Web End-to-End Test
1. Open [chatgpt.com/plugins](https://chatgpt.com/plugins).
2. Turn on **Developer mode**.
3. Select Connection: **Tunnel**.
4. Paste Tunnel ID (from `hands status --json`) → Click **Scan tools**.
5. In a new ChatGPT chat, test with sample prompts:
   - *"Call `workspace_info` and read the first 10 lines of `README.md`"*
   - *"Run `cmd.exe /c echo CHATGPT_HANDS_OK` and report output"*
   - *"List root files using `list_dir`"*

---

## 4. Operational Notes & Windows Rules

- **PowerShell Invocation**: When executing absolute paths with spaces, use call operator: `& 'C:\path\to\app.exe' args`.
- **No Unix Text Utilities**: Windows shell lacks `grep`, `sed`, `awk`, `tail`. Use dedicated MCP tools (`grep`, `read_file`).
- **Output Truncation**: Output > 40KB is saved to a temp log file; query narrower commands when possible.
- **Credential Storage**: Keys are stored in OS credential store (Windows Credential Manager / macOS Keychain). Do not commit API keys.

