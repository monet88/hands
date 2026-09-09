# Windows dogfood update — current workstation

> **Scope:** this is the machine-local update and rollback procedure for the current Windows dogfood workstation. Use it when the task is to update the Hands instance already installed on this machine. Do **not** use it as the public Windows install contract, CI/release packaging flow, or Windows Sandbox acceptance flow; those remain documented in `WINDOWS.md`.

## Current machine assumptions

Verify these before mutating anything:

- Source repo: `F:\CodeBase\hands`
- Active development branch: `dev`
- Installed runtime directory: `%LOCALAPPDATA%\Programs\hands\bin`
- Existing lifecycle scripts: `hands-start` (`.cmd`/`.ps1`), `hands-stop` (`.cmd`/`.ps1`), and `hands-reset` (`.cmd`/`.ps1`) in `%LOCALAPPDATA%\Programs\hands\bin` (added to system PATH)
- Machine autostart: `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\hands-autostart.cmd` launching `hands-start.ps1` hidden on user logon
- Process topology: lean background execution via WMI (`ShowWindow = [uint16]0`); `tunnel-client.exe` spawns `hands.exe` stdio child; no long-lived `hands.exe --http :8787` daemon
- Hands config: `%APPDATA%\hands`
- Tunnel profile: `%APPDATA%\tunnel-client\hands.yaml`
- Canonical build staging checkout: `%LOCALAPPDATA%\hands\cache\grok-build`
- Hourly state cleanup: Scheduled Task `HandsStateCleanup` running `%LOCALAPPDATA%\Programs\hands\bin\clean-hands-state.py`
This workstation is intentionally allowed to use its existing machine-local tunnel/runtime dependencies. A routine dogfood update replaces only `hands.exe`; it does not rebuild the public Runtime Bundle or reinstall `tunnel-client`, ripgrep, profiles, autostart, or credentials.

## Safety and activation gate

The installed Hands runtime may be serving the ChatGPT connection that is performing the update.

- Build, preflight, and backup while the current runtime remains live.
- Stop/replace/start only when the user's task explicitly authorizes activating the new build.
- A lost Hands response during activation is transport uncertainty, not proof that stop/copy/start failed. After reconnect, inspect process, file, version, and `/readyz` state before retrying anything.
- Preserve unrelated processes, worktrees, terminals, and tasks.
- Backups contain machine-local scripts/config that may contain credentials. Keep backups outside the repository and never commit or upload them.

## 1. Inspect the current state

```powershell
Set-Location "F:\CodeBase\hands"

git status --short
git branch --show-current
git rev-parse HEAD

$bin = "$env:LOCALAPPDATA\Programs\hands\bin"
Get-ChildItem $bin -File
& "$bin\hands.exe" --version
```

**Done when:** the repo/worktree state is understood, the intended source revision is known, and the currently installed `hands.exe` version has been recorded.

## 2. Back up the current installation

Create a timestamped backup outside the repo before replacing the live binary:

```powershell
$bin = "$env:LOCALAPPDATA\Programs\hands\bin"
$backupRoot = "$env:LOCALAPPDATA\Programs\hands\backups"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$backup = Join-Path $backupRoot $stamp

New-Item -ItemType Directory -Force $backup | Out-Null

Copy-Item $bin (Join-Path $backup "bin") -Recurse -Force

if (Test-Path "$env:APPDATA\hands") {
    Copy-Item "$env:APPDATA\hands" `
        (Join-Path $backup "hands-config") `
        -Recurse -Force
}

if (Test-Path "$env:APPDATA\tunnel-client\hands.yaml") {
    New-Item -ItemType Directory `
        (Join-Path $backup "tunnel-client") -Force | Out-Null

    Copy-Item "$env:APPDATA\tunnel-client\hands.yaml" `
        (Join-Path $backup "tunnel-client\hands.yaml") -Force
}

Write-Host "BACKUP_PATH=$backup"
```

Keep the printed `BACKUP_PATH`; rollback must use an explicit known-good backup rather than guessing.

**Done when:** the backup contains `bin\hands.exe` plus the current lifecycle scripts, and the relevant config/profile copies exist when present on the machine.

## 3. Update source, inject, and build

Do not stop the current runtime for this step. If step 1 found local WIP, preserve it: do not stash, reset, clean, or switch branches automatically. Resolve that repo state separately before pulling the dogfood source revision.

### 3.1 Ensure canonical grok-build staging checkout

If `%LOCALAPPDATA%\hands\cache\grok-build` does not yet exist on the machine, initialize it from the pinned Grok Build revision (`72a61251fcffb464bcc687aeb5a998e5a98ec0c9`):

```powershell
$cacheDir = "$env:LOCALAPPDATA\hands\cache\grok-build"
if (-not (Test-Path $cacheDir)) {
    New-Item -ItemType Directory -Force (Split-Path $cacheDir) | Out-Null
    git clone "https://github.com/xai-org/grok-build.git" $cacheDir
    git -C $cacheDir reset --hard 72a61251fcffb464bcc687aeb5a998e5a98ec0c9
}
```

*(Note: If an existing local clone of grok-build at that pinned SHA exists under `F:\CodeBase`, cloning from that local path is also supported).*

### 3.2 Update, inject, and build

```powershell
Set-Location "F:\CodeBase\hands"

if (git status --porcelain) {
    throw "Hands repo has local WIP; preserve it and resolve the repo state before dogfood update"
}

git switch dev
git pull --ff-only origin dev

python scripts/inject.py . "$env:LOCALAPPDATA\hands\cache\grok-build"
if ($LASTEXITCODE -ne 0) { throw "inject failed" }

# On Windows, xai-proto-build requires a protoc wrapper to handle Unix-style /dev/stdout and /dev/null flags
$prevProtoc = $env:PROTOC
if (Test-Path "$env:LOCALAPPDATA\Temp\hands-protoc-wrap\bin\protoc.cmd") {
    $env:PROTOC = "$env:LOCALAPPDATA\Temp\hands-protoc-wrap\bin\protoc.cmd"
}

$prevRustflags = $env:RUSTFLAGS
$env:RUSTFLAGS = "-C target-feature=+crt-static"

try {
    cargo build --release -p hands `
        --manifest-path "$env:LOCALAPPDATA\hands\cache\grok-build\Cargo.toml"

    if ($LASTEXITCODE -ne 0) { throw "build failed" }
}
finally {
    if ($null -ne $prevRustflags) {
        $env:RUSTFLAGS = $prevRustflags
    } else {
        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    }
    if ($null -ne $prevProtoc) {
        $env:PROTOC = $prevProtoc
    } else {
        Remove-Item Env:PROTOC -ErrorAction SilentlyContinue
    }
}
```

The candidate binary is:

```text
%LOCALAPPDATA%\hands\cache\grok-build\target\release\hands.exe
```

**Done when:** inject and release build both exit successfully without touching the installed runtime.

## 4. Preflight the candidate before activation

```powershell
$newHands = "$env:LOCALAPPDATA\hands\cache\grok-build\target\release\hands.exe"

& $newHands --version
if ($LASTEXITCODE -ne 0) { throw "candidate --version failed" }

& $newHands list
if ($LASTEXITCODE -ne 0) { throw "candidate list failed" }
```

Do not activate a candidate that fails either command.

**Done when:** the candidate starts cleanly and exposes the expected local bridge tools.

## 5. Activate the new `hands.exe`

This is the only step expected to interrupt the current Hands connection.

```powershell
$bin = "$env:LOCALAPPDATA\Programs\hands\bin"
$newHands = "$env:LOCALAPPDATA\hands\cache\grok-build\target\release\hands.exe"

& "$bin\hands-stop.ps1"
Copy-Item $newHands "$bin\hands.exe" -Force
& "$bin\hands-start.ps1"
```

Routine dogfood activation intentionally leaves the rest of `$bin`, tunnel profile, credentials, and machine-local lifecycle scripts untouched.

**Done when:** the new binary has replaced only `$bin\hands.exe` and the existing machine-local start procedure has been invoked once.

## 6. Verify the activated runtime

After the connection is available again:

```powershell
$bin = "$env:LOCALAPPDATA\Programs\hands\bin"

curl.exe --fail http://127.0.0.1:18780/readyz
& "$bin\hands.exe" --version
& "$bin\hands.exe" status --json
& "$bin\hands.exe" list
```

Also confirm that the reported Git revision/version matches the candidate from step 4.

**Done when:** `/readyz` succeeds, the installed binary reports the expected new revision, `status --json` is healthy enough for the intended use, and `list` succeeds.

## 7. Roll back if activation fails

Use the explicit `BACKUP_PATH` captured in step 2. In most code-only updates, restoring the old `hands.exe` is sufficient:

```powershell
$bin = "$env:LOCALAPPDATA\Programs\hands\bin"
$backup = "<BACKUP_PATH_FROM_STEP_2>"

& "$bin\hands-stop.ps1"
Copy-Item "$backup\bin\hands.exe" "$bin\hands.exe" -Force
& "$bin\hands-start.ps1"

curl.exe --fail http://127.0.0.1:18780/readyz
& "$bin\hands.exe" --version
```

If the update intentionally changed machine-local scripts/config and those changes also need to be reverted, restore the full saved state instead:

```powershell
$bin = "$env:LOCALAPPDATA\Programs\hands\bin"
$backup = "<BACKUP_PATH_FROM_STEP_2>"

& "$bin\hands-stop.ps1"
Copy-Item "$backup\bin\*" $bin -Recurse -Force

if (Test-Path "$backup\hands-config") {
    New-Item -ItemType Directory -Force "$env:APPDATA\hands" | Out-Null
    Copy-Item "$backup\hands-config\*" "$env:APPDATA\hands" -Recurse -Force
}

if (Test-Path "$backup\tunnel-client\hands.yaml") {
    New-Item -ItemType Directory -Force "$env:APPDATA\tunnel-client" | Out-Null
    Copy-Item "$backup\tunnel-client\hands.yaml" `
        "$env:APPDATA\tunnel-client\hands.yaml" -Force
}

& "$bin\hands-start.ps1"

curl.exe --fail http://127.0.0.1:18780/readyz
& "$bin\hands.exe" --version
```

**Done when:** the previous known-good revision is running and `/readyz` succeeds again.

## 8. Hourly state cleanup (`resources_state.json`)

To prevent MCP stdio stalls (HANDS-001) caused by unbounded accumulation of `ReportedTaskCompletions` in `%LOCALAPPDATA%\Temp\hands\resources_state.json`:

- Cleanup helper: `%LOCALAPPDATA%\Programs\hands\bin\clean-hands-state.py`
- Windows Scheduled Task: `HandsStateCleanup` (configured to trigger hourly).
- Rule: if `reported` completions exceed 50 items, the list is trimmed to the 20 most recent entries.

To register or inspect on this workstation:

```powershell
# Inspect
Get-ScheduledTask -TaskName "HandsStateCleanup"
Get-ScheduledTaskInfo -TaskName "HandsStateCleanup"

# Re-register if missing
$action = New-ScheduledTaskAction -Execute "pythonw.exe" -Argument "`"$env:LOCALAPPDATA\Programs\hands\bin\clean-hands-state.py`""
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Hours 1)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable
Register-ScheduledTask -TaskName "HandsStateCleanup" -Action $action -Trigger $trigger -Settings $settings -Force
```

## 9. Lifecycle control and machine autostart

The dogfood runtime directory `%LOCALAPPDATA%\Programs\hands\bin` is registered in system `PATH`. Lifecycle commands can be executed directly from any terminal (CMD, PowerShell, Git Bash) or via Win+R:

- **`hands-stop`** (`hands-stop.cmd` / `hands-stop.ps1`): Stops `tunnel-client` and `hands` processes cleanly.
- **`hands-reset`** (`hands-reset.cmd` / `hands-reset.ps1`): Stops the active runtime, pauses 1 second, and launches a fresh clean background instance.
- **`hands-start`** (`hands-start.cmd` / `hands-start.ps1`): Starts `tunnel-client.exe` detached via WMI with `ShowWindow = [uint16]0` (completely hidden).

### Machine autostart

- Startup entry: `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\hands-autostart.cmd`
- Execution: Runs `powershell.exe -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File "%LOCALAPPDATA%\Programs\hands\bin\hands-start.ps1"`.
- WMI detachment ensures the process escapes terminal Job Objects and remains hidden in the background without spawning visible console or Windows Terminal windows.

## Routine update summary

For this workstation, the normal path is deliberately short:

```text
inspect
→ backup current installation
→ pull dev
→ inject
→ build static-CRT hands.exe
→ preflight candidate
→ stop legacy dogfood runtime
→ replace only hands.exe
→ start
→ verify /readyz + version + status + tools
```

Use the full versioned Runtime Bundle flow in `WINDOWS.md` only when the task is packaging/release portability, clean-machine/Sandbox acceptance, or Windows Tray Launcher work.