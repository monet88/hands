# Integration Policy

## 1. Upstream Baseline and Direct Dispatch
- The integration baseline for this repository is upstream commit `c059e0d` with direct dispatch through `ToolBridge`.
- Hands operates as a lean CLI and MCP server bridging directly to `xai_grok_tools` without intermediate virtual execution engines, proxy routers, or multi-thousand-line execution abstractions.

## 2. Non-Restoration of Closed #31 Implementation Machinery
- Closed Issue #31 implementation machinery—specifically custom parameters such as `execution_mode`, `yield_after_ms`, and coordinator polling loops—is **not** restored and must not be reintroduced into tool schemas or dispatch handlers.
- Dispatch follows upstream command execution semantics: foreground commands execute to completion up to the tool timeout, while background tasks use explicit `is_background: true` execution semantics.

## 3. WebCodex Framing Contract
- MCP tool call results follow the WebCodex framing contract:
  - `structuredContent` is the authoritative machine-readable result payload containing typed fields and raw outputs.
  - `content[].text` is a concise human-readable fallback for user interfaces and models that do not parse structured content. Large command outputs are bounded/truncated in `content[].text` and must not duplicate large payloads verbatim.

## 4. Bounded Task Recovery
- Task recovery (`list_terminal_tasks`) runs directly on top of upstream's terminal backend (`bridge.list_background_tasks()`) without introducing a secondary in-process registry or background daemon supervisor.
- Recovered task snapshots expose only bounded, safe fields (`task_id`, `status`, `command`, `cwd`, `exit_code`, `output_file`, `duration_secs`, `completed`, `truncated`, `total_bytes`).
- Secrets, environment variables, and raw buffers are strictly excluded from listing snapshots.
- Listing queries return all known session tasks bounded without unrequested filter arguments.
