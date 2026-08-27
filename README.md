# grok-harness

Local **Grok Build tools** as an MCP server for **ChatGPT Web**. No local LLM. ChatGPT is the brain; this machine is the body.

```text
ChatGPT Web  →  Secure MCP Tunnel  →  grok-harness  →  your repo
```

## Install

Needs: `git`, `python3`, `rustup`, macOS or Linux.

```bash
git clone https://github.com/<you>/grok-harness.git
cd grok-harness
./install.sh
```

Puts `grok-harness` in `~/.local/bin`. First build compiles [xai-org/grok-build](https://github.com/xai-org/grok-build) (several minutes).

```bash
# optional
PREFIX=/usr/local ./install.sh
GROK_BUILD_REF=main ./install.sh
```

## Use any workspace

```bash
cd /path/to/your/repo
grok-harness use
grok-harness status
```

ChatGPT talks to that folder. Switch repo: `cd` elsewhere and `grok-harness use` again. No tunnel restart.

## ChatGPT Web

1. Runtime key (Restricted, Tunnels **Read** + **Use**):  
   https://platform.openai.com/settings/organization/api-keys
2. Tunnel id:  
   https://platform.openai.com/settings/organization/tunnels
3. Install [tunnel-client](https://github.com/openai/tunnel-client):

```bash
brew install openai/tools/tunnel-client
export CONTROL_PLANE_API_KEY="sk-..."   # runtime key, not admin key
export CONTROL_PLANE_TUNNEL_ID="tunnel_..."

tunnel-client init \
  --sample sample_mcp_stdio_local \
  --profile grok-harness \
  --tunnel-id "$CONTROL_PLANE_TUNNEL_ID" \
  --mcp-command "grok-harness" \
  --force

tunnel-client doctor --profile grok-harness --explain
tunnel-client run --profile grok-harness
```

4. [chatgpt.com/plugins](https://chatgpt.com/plugins) → Developer mode → Connection **Tunnel** → paste tunnel id → Scan tools.

Keep `tunnel-client run` up while you chat.

## Tools

| Tool | Role |
|---|---|
| `workspace_info` | current pin |
| `read_file` | read |
| `grep` | search contents |
| `list_dir` | tree |
| `glob` | find files by name |
| `search_replace` | edit existing |
| `write` | create / overwrite |
| `apply_patch` | multi-hunk patch |
| `todo_write` | task list |
| `run_terminal_cmd` | tests / git / shell (background ok) |
| `get_task_output` | poll background job |
| `kill_task` | stop background job |

No Grok/Codex model tokens. Debug: `grok-harness list`, `grok-harness call read_file '{"target_file":"README.md"}'`.

## License

Apache-2.0. Tool runtime comes from Grok Build (Apache-2.0). See `NOTICE`.
