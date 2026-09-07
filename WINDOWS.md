# Hands on Windows — Setup & Runtime Bundle Guide

> **Current status:** the repository supports the Windows Runtime Bundle build/package/verification seam hardened by Issue #62. The Windows Tray Launcher architecture is accepted, but its implementation/activation workflow is still pending. Existing machine-local `start-hands.*` or Startup entries are legacy dogfood state, not the repository's install contract.

Hands remains a thin MCP/CLI runtime. Windows lifecycle ownership belongs outside the MCP execution path.

---

## 1. Source and runtime baselines

- **Hands repository:** `https://github.com/monet88/hands.git`
- **Default development branch:** `dev`
- **Upstream baseline:** `https://github.com/nghyane/hands.git` commit `c059e0d` (`feat: native edit diffs and per-chat workspace`)
- **Pinned Grok Build revision:** `72a61251fcffb464bcc687aeb5a998e5a98ec0c9`

`c059e0d` is the upstream baseline, not a claim that this repository is still byte-for-byte identical to upstream. Hands carries its own integration code, tests, Windows packaging logic, and deterministic Grok Build patch set on top of that baseline.

The accepted Windows product vocabulary and architecture live in:

- [`CONTEXT.md`](CONTEXT.md)
- [`docs/adr/0001-windows-tray-launcher-owns-windows-lifecycle.md`](docs/adr/0001-windows-tray-launcher-owns-windows-lifecycle.md)
- [`docs/adr/0002-portable-runtime-stays-beside-launcher.md`](docs/adr/0002-portable-runtime-stays-beside-launcher.md)
- [`docs/adr/0003-hands-runtime-owns-runtime-configuration.md`](docs/adr/0003-hands-runtime-owns-runtime-configuration.md)

---

## 2. Build and verify the Windows Runtime Bundle

The commands below are maintainer/build steps. They stage a verified bundle; they do **not** replace or restart a live Hands runtime.

### 2.1 Inject Hands into pinned Grok Build

```powershell
python scripts/inject.py . "$env:LOCALAPPDATA\hands\cache\grok-build"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
```

`scripts/inject.py` verifies the expected Grok Build revision and applies the deterministic patch set through `scripts/patch_grok_build.py`.

### 2.2 Build `hands.exe` with static MSVC CRT

```powershell
$prevRustflags = $env:RUSTFLAGS
$env:RUSTFLAGS = "-C target-feature=+crt-static"
try {
    cargo build --release -p hands --manifest-path "$env:LOCALAPPDATA\hands\cache\grok-build\Cargo.toml"
} finally {
    if ($null -ne $prevRustflags) {
        $env:RUSTFLAGS = $prevRustflags
    } else {
        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    }
}
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
```

`package_windows_bundle.py` fails closed if `hands.exe` still imports the dynamic MSVC runtime instead of satisfying the static-CRT contract.

### 2.3 Download and verify pinned `rg.exe`

```powershell
$rgZip = "$env:TEMP\ripgrep-15.1.0-x86_64-pc-windows-msvc.zip"
$rgDir = "$env:TEMP\ripgrep-15.1.0-hands"

Invoke-WebRequest `
  -Uri "https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-x86_64-pc-windows-msvc.zip" `
  -OutFile $rgZip

Expand-Archive -Path $rgZip -DestinationPath $rgDir -Force
$rgBin = "$rgDir\ripgrep-15.1.0-x86_64-pc-windows-msvc\rg.exe"
$actualHash = (Get-FileHash -Algorithm SHA256 $rgBin).Hash.ToLower()
$expectedHash = "decdd4992f3f1b9a5ef9898f1b40ab16886d579d6516b4efd3d5eaa19364e408"

if ($actualHash -ne $expectedHash) {
    throw "rg.exe SHA-256 mismatch: expected $expectedHash, got $actualHash"
}
```

Do not substitute Scoop/winget/global-PATH `rg.exe` for the packaged runtime dependency.

### 2.4 Stage and verify the bundle

The `%LOCALAPPDATA%` path below is a **maintainer staging example**, not the final Portable App Root contract. The accepted launcher will materialize `runtime\<version>\` beside `Hands.exe`.

```powershell
$tunnelClientBin = (Get-Command tunnel-client -ErrorAction Stop).Source
$runtimeVersion = "0.1.0-$(git rev-parse --short HEAD)"
$bundle = Join-Path $env:LOCALAPPDATA "Programs\hands\runtime\$runtimeVersion"

python scripts/package_windows_bundle.py `
  --out-dir $bundle `
  --hands-bin "$env:LOCALAPPDATA\hands\cache\grok-build\target\release\hands.exe" `
  --rg-bin $rgBin `
  --tunnel-client-bin $tunnelClientBin `
  --version $runtimeVersion
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

python scripts/package_windows_bundle.py --out-dir $bundle --verify-only
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
```

Keep the bundle intact: `hands.exe`, `tunnel-client.exe`, `rg.exe`, `manifest.json`, and `SHA256SUMS.txt` belong together. Do not copy only `hands.exe` out of the bundle; `grep`/`glob` depend on the pinned sibling `rg.exe` on a clean machine.

### 2.5 Verify the tool surface

```powershell
& "$bundle\hands.exe" list
```

`hands list` is a local bridge/debug command. It exposes **12** bridge/debug tools: Hands-owned `run_command` plus the 11 ToolBridge tools (`read_file`, `grep`, `list_dir`, `glob`, `search_replace`, `write`, `apply_patch`, `todo_write`, `run_terminal_cmd`, `get_task_output`, `kill_task`).

ChatGPT MCP `tools/list` exposes **15** tools total by adding the three MCP-host tools `workspace_info`, `set_workspace`, and `list_terminal_tasks` to that 12-tool surface.

---

## 3. Current Windows lifecycle state

The current Rust `service.rs` only installs native login supervision for macOS and Linux. Windows `install_supervisor()`/`start_supervisor()` intentionally do not pretend that a native Windows supervisor already exists.

Therefore:

- `hands enable` / `hands start` are **not** the Windows installation mechanism today.
- Issue #62 proves the Runtime Bundle and public MCP/runtime seams; it does not silently activate a new bundle.
- Existing `start-hands.bat`, `start-hands.ps1`, `hands-autostart.cmd`, or `hands-autostart.vbs` files on a developer machine are legacy machine-local dogfood. They may remain in use until the tray launcher lands, but do not reproduce them as the public setup contract.
- Do not add a second general process manager inside `hands.exe` to fill this gap.

`hands use` is still useful on Windows because workspace pinning happens before any service-start attempt:

```powershell
hands use F:\path\to\your\project
hands status --json
```

If the machine uses an existing external/legacy supervisor, it remains responsible for keeping the active tunnel/runtime alive.

---

## 4. Credentials and tunnel profile contract

Machine credentials are user-scoped state and are **not** part of the portable distribution artifact:

- Control Plane API key (`sk-...`)
- Tunnel ID (`tunnel_...`)

For the accepted launcher path:

1. The Windows Tray Launcher owns settings UX and lifecycle, but **Hands Runtime remains the Config Authority**.
2. The launcher passes settings to a narrow machine-readable Hands configuration seam; it does not implement a second credential/profile format.
3. The Windows tunnel profile is command-based and points to the selected Runtime Bundle's canonical `hands.exe` path.
4. Launcher-managed children do not inherit `CONTROL_PLANE_API_KEY` or `CONTROL_PLANE_TUNNEL_ID` as redirectable overrides; canonical persisted settings are authoritative.
5. Moving or rolling back the Portable App Root requires configuration preparation to rebind the profile before the tunnel starts.

The existing Unix `service.rs::write_profile()` writes HTTP-over-UDS `server_urls` and must not be reused unchanged for the Windows launcher topology.

For development/backward compatibility, `hands config` can still serve the Hands config/MCP UI at `http://127.0.0.1:8787/`; `hands --open config` is the valid form that also opens the browser. The accepted Windows launcher does **not** keep a second long-lived `hands.exe --http :8787` daemon merely to host configuration.

---

## 5. Accepted Windows Tray Launcher topology — implementation pending

The target Phase 1 topology is:

```text
Hands.exe (tray launcher / Windows Supervisor)
  └─ tunnel-client.exe (owned long-lived root)
       └─ hands.exe (MCP child from command-based profile)
            └─ pinned rg.exe (sibling runtime dependency)
```

Key contracts:

- Distribution is one `Hands.exe` artifact, but runtime materialization may contain multiple child binaries.
- Runtime Bundle lives beside the launcher under `runtime\<version>\`; it is not silently redirected to `%LOCALAPPDATA%`.
- Launcher owns only the process tree it spawned. It never kills unrelated `hands.exe`/`tunnel-client.exe` processes by executable name.
- Canonical tunnel health/admin endpoint is `127.0.0.1:18780`.
- Port conflict is reported explicitly; the launcher does not kill the process occupying the port or silently switch to a random port.
- Ready requires both a live owned tunnel process and affirmative `/readyz` health.
- Login autostart and tray lifecycle belong to the launcher, not to an in-process watchdog inside Hands Runtime.

Until this launcher is implemented, treat the Runtime Bundle flow in section 2 as build/package evidence rather than a finished end-user installer.

---

## 6. Upgrade workflow

1. **Update the repository's development branch:**

   ```powershell
   git checkout dev
   git pull --ff-only origin dev
   ```

2. **Inject the updated Hands source into the pinned Grok Build checkout:**

   ```powershell
   python scripts/inject.py . "$env:LOCALAPPDATA\hands\cache\grok-build"
   if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
   ```

3. **Rebuild with static MSVC CRT** using section 2.2.
4. **Download/verify pinned `rg.exe`** using section 2.3.
5. **Stage and `--verify-only` the new Runtime Bundle** using section 2.4.
6. **Do not replace/restart the currently serving Hands runtime as part of build verification.** Activation/rollback belongs to the Windows launcher workflow once implemented; existing dogfood environments must use their explicit machine-local activation procedure.

A new bundle must remain versioned and intact so a previous verified bundle can still be retained for rollback.

---

## 7. Daily use

### Pin the repository you want ChatGPT to operate on

```powershell
hands use F:\path\to\your\project
hands status --json
```

Inside ChatGPT, `set_workspace` is session-scoped and is the preferred way to move a specific chat without stealing another chat's workspace.

### Check the active tunnel

The canonical tunnel diagnostics endpoints are:

- `http://127.0.0.1:18780/readyz`
- `http://127.0.0.1:18780/ui`

On current dogfood machines, the existing external supervisor/launcher remains responsible for keeping the tunnel alive. Do not assume `hands start` installs Windows supervision.

### Connect Hands in ChatGPT Web

1. Create the Restricted runtime key with Tunnels **Read** + **Use** and obtain the Tunnel ID.
2. Enable Developer mode in ChatGPT.
3. Create the custom MCP app from **Settings → Apps → Create** or **Workspace settings → Apps → Create**, depending on the workspace UI.
4. Choose the Secure MCP Tunnel/Tunnel connection, paste the Tunnel ID, then **Scan Tools**.
5. Enable Hands for the chat and configure the desired action-confirmation policy.

OpenAI's current developer-mode instructions are at `https://help.openai.com/en/articles/12584461`.

---

## 8. Maintainer safety

This repository may be serving the active ChatGPT connection on the same Windows machine.

- Do not run `cargo build --release`, reinstall/replace the active runtime, or restart the active supervisor/tunnel during ordinary verification.
- Build against the pinned staging checkout and package into a new versioned bundle.
- Treat a failed/timeout tool call as transport uncertainty; inspect process/repository state before retrying a mutation.
- Preserve unrelated worktrees, running tasks, terminals, and WIP.
