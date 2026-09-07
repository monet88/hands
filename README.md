# Hands

Unofficial **ChatGPT plugin**: local coding tools over MCP. No local LLM. ChatGPT is the brain; this machine is the hands.

Not affiliated with OpenAI or xAI. Tool runtime is [Grok Build](https://github.com/xai-org/grok-build) (Apache-2.0).

```text
ChatGPT Web  →  Secure MCP Tunnel  →  hands  →  your repo
```

## Platform support

| Platform | Install/runtime path | Supervision |
|---|---|---|
| macOS | `install.sh` | LaunchAgent |
| Linux | `install.sh` | `systemd --user` |
| Windows | Portable Runtime Bundle; see [`WINDOWS.md`](WINDOWS.md) | Windows launcher/activation work is separate from the current runtime-bundle flow |

`install.sh` needs `git`, `python3`, and `rustup`. The first build compiles the pinned Grok Build runtime and can take several minutes.

## Install — macOS / Linux

1. Install `tunnel-client` and make sure it is on `PATH`.

   macOS:

   ```bash
   brew install openai/tools/tunnel-client
   ```

   Linux: install the OpenAI Secure MCP Tunnel `tunnel-client` for your environment, then verify `tunnel-client` is discoverable on `PATH`.

2. Clone this repository and build Hands:

   ```bash
   git clone https://github.com/monet88/hands.git
   cd hands
   ./install.sh
   ```

   The installer writes `hands` to `${PREFIX:-$HOME/.local}/bin`. If that directory is not on `PATH`, the installer prints the required `export PATH=...` line.

3. Run first-time setup from the repo you want Hands to use initially:

   ```bash
   cd /path/to/your/repo
   hands setup
   hands status
   ```

`hands setup` pins the workspace, stores the runtime key and tunnel id, enables the supervised local MCP/tunnel service, and starts it. On macOS the key is stored in Keychain plus a `0600` daemon file. On Linux Hands uses `secret-tool` when available and keeps the same `0600` file fallback.

For non-interactive installation, provide both credentials before running the installer:

```bash
export CONTROL_PLANE_API_KEY="sk-..."
export CONTROL_PLANE_TUNNEL_ID="tunnel_..."
./install.sh
```

The runtime key should be Restricted to Tunnels **Read** + **Use**. Do not commit either credential.

## Install — Windows

Windows does not use `install.sh`. Follow [`WINDOWS.md`](WINDOWS.md) for the current supported manual flow:

1. inject the Hands crate into the pinned Grok Build checkout;
2. build `hands.exe` with the static MSVC CRT;
3. download and SHA-256 verify the pinned `rg.exe`;
4. stage and verify the portable Runtime Bundle containing `hands.exe`, `tunnel-client.exe`, and sibling `rg.exe`.

Do not copy `hands.exe` out of that bundle by itself. The current Windows work deliberately keeps build/package verification separate from launcher activation so updating the repo cannot silently replace or restart a live Hands runtime.

## ChatGPT Web

1. Runtime key (Restricted, Tunnels **Read** + **Use**):  
   https://platform.openai.com/settings/organization/api-keys
2. Tunnel id:  
   https://platform.openai.com/settings/organization/tunnels
3. In ChatGPT web, enable Developer mode and create the custom MCP app from **Settings → Apps → Create** or **Workspace settings → Apps → Create** (the exact entrypoint depends on the workspace). Choose the Secure MCP Tunnel/Tunnel connection, paste the tunnel id, then **Scan Tools**. See OpenAI's current [Developer mode and MCP apps](https://help.openai.com/en/articles/12584461) guide if the UI has moved.

Plugin name in ChatGPT: **Hands**.

ChatGPT, not Hands, shows Confirm. MCP cannot turn that off.

- Reads auto-run (`readOnlyHint`).
- File edits are routine (`destructiveHint: false`) — auto under **Important actions**.
- Shell / kill still confirm unless you opt in.

**Unattended coding:** first write prompt → **Always allow**, or **Settings → Apps → Hands → Never ask**. New chats keep that app setting. Developer Mode “remember for this conversation” dies on a new chat.

## Daily use

Set or change the host default workspace from a terminal:

```bash
cd /path/to/your/repo
hands use
hands status
```

Or pin an explicit path:

```bash
hands use /path/to/your/repo
```

Inside ChatGPT, `set_workspace` is different: it pins a workspace for that ChatGPT session without changing other sessions.

### CLI reference

| Command | Purpose |
|---|---|
| `hands setup` | first-run TTY checklist; on macOS/Linux save credentials and enable/start supervision |
| `hands use [dir]` | pin the host default workspace; starts/enables the service when possible |
| `hands status` | show workspace pin, tunnel readiness, and service state |
| `hands status --json` | machine-readable status |
| `hands enable` | install/enable login supervision on macOS/Linux and start Hands |
| `hands disable` | remove login supervision on macOS/Linux |
| `hands start` | start the supervised service |
| `hands stop` | stop it now; if still enabled it can start again at next login |
| `hands config` | serve the local config/MCP UI at `http://127.0.0.1:8787/` |
| `hands watch` | run the tunnel-drop watcher |
| `hands list` | print the tool definitions exposed by the local bridge |
| `hands call <tool> <json>` | invoke one tool directly for debugging |
| `hands --http [--port N]` | serve MCP over local HTTP instead of stdio |
| `hands` | serve MCP over stdio; this is the direct MCP entrypoint |

`enable`, `start`, and automatic login supervision are implemented for macOS and Linux. On Windows, `hands use` still pins the workspace, but lifecycle supervision remains external until the accepted tray-launcher contract is implemented; see `WINDOWS.md`.

The supervised macOS/Linux service uses:

- Tunnel health/admin: `http://127.0.0.1:18780/` (`/readyz`, `/ui`)
- Local MCP/config server: `http://127.0.0.1:8787/`

The accepted Windows launcher topology keeps `127.0.0.1:18780` as the canonical tunnel health/admin endpoint but does not keep a second long-lived `hands.exe --http :8787` daemon just for configuration.

## Tools

| Tool | Role |
|---|---|
| `workspace_info` | current pin + recent |
| `set_workspace` | pin this ChatGPT chat only (other chats keep their folder) |
| `list_terminal_tasks` | list running/completed terminal tasks owned by this ChatGPT session |
| `run_command` | native argv command execution with bounded output and explicit total timeout |
| `read_file` | read |
| `grep` | search contents |
| `list_dir` | tree |
| `glob` | find files by name |
| `search_replace` | edit existing; ChatGPT shows a diff card |
| `write` | create / overwrite; ChatGPT shows a diff card |
| `apply_patch` | multi-hunk patch; ChatGPT shows a diff card |
| `todo_write` | task list |
| `run_terminal_cmd` | tests / git / shell; long FG auto-backgrounds |
| `get_task_output` | poll background job |
| `kill_task` | stop background job |

## Troubleshooting

Start with:

```bash
hands status --json
hands list
hands call read_file '{"target_file":"README.md"}'
```

On macOS/Linux, if `hands setup` reports `tunnel-client` missing, install it first and make sure the binary is on `PATH`. If the tunnel is enabled but not ready, open `http://127.0.0.1:18780/ui` and inspect the Hands logs/config for the active profile. On Windows, use the runtime-bundle and launcher status guidance in `WINDOWS.md` instead of assuming `hands enable`/`hands start` installs a native Windows supervisor.

On AC the Mac stays awake for the long-poll; on battery, closing the lid may sleep.

Agents working on this repository should also read [`AGENTS.md`](AGENTS.md).

## License

Apache-2.0. See `NOTICE`.
