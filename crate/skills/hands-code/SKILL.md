---
name: hands-code
description: Read, edit, and run code on the user's local machine via Hands MCP tools. Use when the user wants to work in a repo, fix a bug, run tests, or switch workspaces on this computer.
---

You drive Hands: unofficial local coding tools. There is no local LLM.

Do not ask the user in chat to confirm each edit. Call the tool. ChatGPT already shows a host confirmation when it requires one.

## Workspace

Each ChatGPT conversation has its own folder. `set_workspace` in this chat does not change other chats.

1. Call `workspace_info` first if the folder might be wrong.
2. If the user names a repo, call `set_workspace` with an absolute path, `~/…`, or the folder name under `~/Dev`.
3. Do not invent paths. If `set_workspace` fails, use `recent` from `workspace_info` or ask once.
4. If `workspace_info` has `"session": null`, pass `workspace` on later tool calls (same path) so another chat cannot steal the pin.
5. The resolved per-chat Workspace is the default/implicit context for relative operations.
6. Explicit absolute paths and explicit `workdir` may target elsewhere without repinning the Workspace.
7. Command results should be interpreted using `cwd` plus `default_workspace` when available.
8. File-operation results should use `target_path` plus `default_workspace` when available.
## Edit

- Existing file: `read_file`, then `search_replace`.
- New file: `write`.
- Several hunks or files: `apply_patch`.
- Plan: `todo_write`.
- Edit results include a unified diff (ChatGPT shows it as an inline card). Use that; do not re-read the whole file unless the diff is truncated. Do not paste the diff back in chat.
- After each edit, run the check that would catch the mistake (`run_terminal_cmd`).

## Commands

Use `is_background: true` for builds, tests, servers, or anything that may exceed ~15s. Foreground `run_terminal_cmd` auto-backgrounds when its foreground wait budget expires (~15s) without killing or restarting the process; this handoff is not a timeout. Poll `get_task_output` for background tasks, or stop with `kill_task`.
For `run_command`, `timeout_ms` specifies a total process runtime deadline after which the child process is terminated.
Prefer one `apply_patch` over many tiny `search_replace` calls.
