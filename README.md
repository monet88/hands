# Hands

Unofficial **ChatGPT plugin**: local coding tools over MCP. No local LLM required. ChatGPT is the brain; your local machine is the hands.

Not affiliated with OpenAI or xAI. Tool runtime is powered by [Grok Build](https://github.com/xai-org/grok-build) (Apache-2.0).

```text
ChatGPT Web  ──(Secure MCP Tunnel)──>  tunnel-client  ──>  hands  ──>  Your Local Repo
```

---

## 1. Prerequisites

You need `git`, `python3`, and `rustup` (`cargo`).

* **Windows:**
  ```powershell
  winget install Git.Git Python.Python.3.12 Rustlang.Rustup
  ```
* **macOS:**
  ```bash
  brew install git python3 rustup
  brew install openai/tools/tunnel-client
  ```
* **Linux (Ubuntu/Debian):**
  ```bash
  sudo apt install git python3 python3-pip cargo rustc
  ```

---

## 2. Installation

Clone the repository and run the installer:

### Windows (PowerShell)
```powershell
git clone https://github.com/nghyane/hands.git
cd hands
.\install.ps1
```
> `install.ps1` automatically builds `hands.exe`, adds it to your User `PATH`, and downloads/verifies the official OpenAI `tunnel-client.exe`.

### macOS / Linux
```bash
git clone https://github.com/nghyane/hands.git
cd hands
./install.sh
```

---

## 3. Configuration & Setup

### A. Get OpenAI Credentials
1. **Runtime API Key** (Restricted: Tunnels **Read** + **Use**):  
   [platform.openai.com/settings/organization/api-keys](https://platform.openai.com/settings/organization/api-keys)
2. **Tunnel ID**:  
   [platform.openai.com/settings/organization/tunnels](https://platform.openai.com/settings/organization/tunnels)

### B. Configure Hands
Run the interactive setup wizard:
```bash
hands setup
```
* On Windows, your key is stored securely in **Windows Credential Manager**.
* On macOS, it is stored in **Keychain**; on Linux, in `0600` permissions config.
* The setup will automatically copy your Tunnel ID to the clipboard and enable the background service.

*(Optional)* You can also use the Web UI:
```bash
hands config --open
```
Opens http://127.0.0.1:8787/ to view status and update API keys.

---

## 4. Connect with ChatGPT Web

1. Open [chatgpt.com/plugins](https://chatgpt.com/plugins).
2. Enable **Developer mode**.
3. Choose Connection **Tunnel**.
4. Paste your Tunnel ID (`tunnel_...`) and click **Scan tools**.
5. The plugin **Hands** will be discovered with 13 local coding tools.

---

## 5. Daily Workflow

### Pin your active workspace
Whenever you switch to another repository or folder:
```bash
cd /path/to/your/project
hands use

# Or directly specify path:
hands use F:\CodeBase\my-project
```
ChatGPT will automatically execute all tools in the pinned folder starting from the next prompt.

### Service Management
* **Check Status:** `hands status` (or `hands status --json`)
* **Local Diagnostics:** `hands doctor` (or `hands doctor --json`)
* **Auto-Start on Boot (Recommended):** `hands enable` (uses Task Scheduler on Windows, LaunchAgent on macOS, systemd on Linux)
* **Start / Stop manually:** `hands start` / `hands stop`
* **Windows Quick-Run (Foreground):** Double-click `start-hands.bat`

---

## 6. Available MCP Tools

| Tool | Role | Description |
|---|---|---|
| `workspace_info` | Inspection | Get currently pinned workspace path |
| `read_file` | Read | Read file contents |
| `grep` | Search | Search text across codebase |
| `list_dir` | Tree | List directories and files |
| `glob` | Find | Find files matching glob patterns |
| `search_replace` | Edit | Replace unique code chunks in files |
| `write` | Create | Write or overwrite files |
| `apply_patch` | Edit | Apply unified diffs to files |
| `todo_write` | Tasks | Manage task checklist |
| `run_terminal_cmd` | Execution | Run tests, build scripts, shell commands |
| `run_command` | Execution | Run a native CLI directly with an argv vector, no shell |
| `get_task_output` | Polling | Poll long-running background tasks |
| `kill_task` | Cleanup | Terminate background tasks |

---

## 7. CLI Reference

```text
Hands — unofficial ChatGPT plugin (local tools, no model)

Commands:
  hands setup                      First-run checklist (TTY, no browser)
  hands setup --ui                 First-run checklist and open config UI
  hands config                     Serve config UI at http://127.0.0.1:8787/
  hands config --open              Serve and open config UI in browser
  hands use [path]                 Pin working folder for ChatGPT
  hands status [--json]            Show current pin and tunnel readiness
  hands doctor [--json]            Run local host & configuration diagnostics
  hands enable | disable           Register / remove auto-start service
  hands start | stop               Start / stop tunnel supervisor
  hands list                       List available MCP tools
  hands call <tool> <json>         Directly invoke a tool for debugging
```

---

## License

Apache-2.0. See `NOTICE`.

