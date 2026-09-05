# Hands

Hands is a local bridge that lets ChatGPT operate on a user's machine while keeping model reasoning outside the local runtime. This glossary defines the product language used when separating the Windows desktop shell from the execution runtime.

## Language

**Hands Runtime**:
The local MCP/CLI execution bridge used by ChatGPT. It is distinct from the Windows desktop shell that starts and supervises it.
_Avoid_: Launcher, tray app

**Windows Tray Launcher**:
The Windows-facing desktop shell distributed as `Hands.exe`. It owns Windows lifecycle UX but is not part of MCP tool execution.
_Avoid_: Hands Runtime, MCP server

**Single Distribution Artifact**:
One downloadable or copyable Windows executable. It does not imply that only one executable or process exists after launch.
_Avoid_: Single-process app, single runtime binary

**Runtime Bundle**:
The versioned set of child binaries needed by the Windows Tray Launcher: the Hands Runtime and `tunnel-client`.
_Avoid_: Installer payload

**Portable App Root**:
The writable directory containing `Hands.exe` and its versioned `runtime\` children. Runtime binaries stay here so moving/copying the app remains explicit and inspectable.
_Avoid_: User data directory, credential store

**Owned Runtime Process Tree**:
The launcher-spawned Windows process tree assigned to the launcher's lifecycle ownership. Only this tree may be terminated automatically by the Windows Supervisor.
_Avoid_: All Hands processes, matching processes by executable name

**Windows Supervisor**:
The Windows lifecycle owner for the Runtime Bundle, including start, health, restart, stop, and login autostart.
_Avoid_: MCP orchestrator, task scheduler

**Port Conflict**:
A launcher state where the canonical local tunnel health/admin endpoint is already owned by another process before the Runtime Bundle can start. It is a preflight conflict, not a runtime crash.
_Avoid_: Runtime crash, restart failure

**Faulted**:
A launcher state where startup or automatic runtime recovery has stopped because configuration preparation could not persist or read a compatible configuration, or because the bounded restart budget is exhausted. Recovery requires an explicit user action or a new launcher session.
_Avoid_: Port Conflict, Needs setup

**Config Authority**:
The Hands Runtime boundary that owns the canonical runtime credential and tunnel-profile representation. The Windows Tray Launcher presents settings but does not define a second config format.
_Avoid_: Launcher config parser, duplicate credential store

**Machine Credentials**:
The Control Plane API key and Tunnel ID associated with the local Windows user or machine. They are not part of the portable distribution artifact.
_Avoid_: Portable credentials, bundled credentials

**Hands Config UI**:
The Hands-owned local configuration surface. It is distinct from tunnel diagnostics.
_Avoid_: Tunnel Admin UI

**Tunnel Admin UI**:
The `tunnel-client` status and diagnostics surface. It is distinct from Hands configuration.
_Avoid_: Hands Config UI
