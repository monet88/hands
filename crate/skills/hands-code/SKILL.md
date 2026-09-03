---
name: hands-code
description: Read, edit, and run code on the user's local machine via Hands MCP tools. Use when the user wants to work in a repo, fix a bug, run tests, or switch workspaces on this computer.
---

You drive Hands: unofficial local coding tools. There is no local LLM.

Do not ask the user in chat to confirm each edit. Call the tool. ChatGPT already shows a host confirmation when it requires one.

## Workspace

1. The pinned Workspace is the default/implicit context for relative operations.
2. Explicit absolute paths and explicit `workdir` may target elsewhere without repinning the Workspace.
3. Call `workspace_info` first if the folder might be wrong.
4. If the user names a repo to switch to, call `set_workspace` with an absolute path, `~/…`, or the folder name under `~/Dev`.
5. Do not invent paths. If `set_workspace` fails, use `recent` from `workspace_info` or ask once.
6. Command results should be interpreted using `cwd` plus `default_workspace`.
7. File-operation results should use `target_path` plus `default_workspace` when available.
## Edit

- Existing file: `read_file`, then `search_replace`.
- New file: `write`.
- Several hunks or files: `apply_patch`.
- Plan: `todo_write`.
- After each edit, run the check that would catch the mistake (`run_terminal_cmd`).

## Commands

Use `is_background: true` for builds, tests, servers, or anything that may exceed ~15s. Foreground `run_terminal_cmd` auto-backgrounds when its foreground wait budget expires (~15s) without killing or restarting the process; this handoff is not a timeout. Poll `get_task_output` for background tasks, or stop with `kill_task`.
For `run_command`, `timeout_ms` specifies a total process runtime deadline after which the child process is terminated.
Prefer one `apply_patch` over many tiny `search_replace` calls.
