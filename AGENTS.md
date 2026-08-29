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
