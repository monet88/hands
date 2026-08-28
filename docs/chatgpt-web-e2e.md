# ChatGPT Web / Tunnel E2E Verification

This is progressive-disclosure guidance for changes that cross the local Hands process boundary into the Secure MCP Tunnel or ChatGPT Web. Routine local code changes should use `scripts/smoke_windows.ps1` instead.

## When to run this flow

Use this flow when a change affects any of the following:

- MCP tool names/schemas or tool discovery;
- tunnel-client discovery/configuration;
- supervisor/service lifecycle or `/readyz` behavior;
- workspace propagation through a real ChatGPT connection;
- foreground/background terminal behavior visible through ChatGPT Web.

When the local `docs/hands-debug-notes.md` notebook is present, consult it for Windows runtime history and known edge cases before diagnosing a failure.

## 1. Local deterministic gate

Use the existing exact-head binary and isolated test config:

```powershell
.\scripts\e2e_gate_windows.ps1 -LocalOnly
```

The gate owns and cleans up its test MCP process/config. It must not be replaced with `hands start`, `hands stop`, `install.ps1`, or a blanket process kill during an active development connection.

## 2. Confirm tunnel readiness without mutating it

```powershell
hands status --json
hands doctor --json
```

If the active connection is already serving the required exact-head binary, keep it running. Do not restart the supervisor/tunnel merely to perform a read-only check.

## 3. ChatGPT Web verification

1. Open ChatGPT connection/plugin settings and enable Developer mode if required.
2. Select the **Tunnel** connection and use the Tunnel ID reported by `hands status --json`.
3. Scan tools.
4. In a new chat, verify representative behavior:
   - call `workspace_info` and confirm the intended workspace;
   - read a known repository file with `read_file`;
   - list the root with `list_dir`;
   - run a harmless foreground command such as `cmd.exe /c echo CHATGPT_HANDS_OK` through `run_terminal_cmd` when terminal behavior is in scope;
   - exercise background task/get-output/kill only when those surfaces changed.

Do not paste secrets into the chat or evidence file.

## 4. Evidence-backed full gate

`scripts/e2e_gate_windows.ps1` can validate real ChatGPT Web evidence against exact `HEAD`. If the evidence file is missing, the script creates a template under `.grok-build` and fails closed with the expected path.

```powershell
.\scripts\e2e_gate_windows.ps1 -ChatGPTEvidence .\.grok-build\chatgpt_e2e_evidence.json
```

Completion requires both the deterministic checks and evidence fields required by the script. Do not treat a tunnel timeout or connector error as proof of a repository regression until local/runtime state is checked independently.
