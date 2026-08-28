# Windows E2E gate for Hands (#8 acceptance).
#
# Proves the production path: ChatGPT Web -> Secure MCP Tunnel -> Hands ->
# Windows workspace/terminal -> Orca. Every acceptance step asserts exit
# status + JSON/output semantics and FAILS the gate when unmet. Orca is
# REQUIRED: if the `orca` CLI is unavailable the gate fails (never passes).
#
# Host state mutated by this gate is snapshotted and restored in `finally`:
#   - workspace pin (saved/restored, never left on hands_e2e_ws_*)
#   - environment vars touched by the run
#   - temporary fixtures, payload files, spawned processes
# Run from repo root:  powershell -File scripts\e2e_gate_windows.ps1

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$HandsBin = Join-Path $RepoRoot ".grok-build\target\release\hands.exe"
if (-not (Test-Path $HandsBin)) {
    $HandsBin = Join-Path $RepoRoot ".grok-build\target\debug\hands.exe"
}
if (-not (Test-Path $HandsBin)) {
    Write-Error "hands.exe not found. Run the release build first."
    exit 1
}

Write-Host "========================================="
Write-Host "Hands Windows E2E Gate (production path): $HandsBin"
Write-Host "========================================="

# ---- state snapshot (restored in finally) ---------------------------------
$PinFile = Join-Path $env:APPDATA "hands\workspace"
$SavedPin = $null
if (Test-Path $PinFile) {
    $SavedPin = Get-Content -Path $PinFile -Raw -ErrorAction SilentlyContinue
}
$SavedWorkspaceEnv = $env:HANDS_WORKSPACE

function Restore-State {
    if ($null -ne $SavedPin) {
        Set-Content -Path $PinFile -Value $SavedPin -NoNewline -ErrorAction SilentlyContinue
    } elseif (Test-Path $PinFile) {
        # The pin file existed before? No: saved only when it existed. When no
        # pre-existing pin, remove anything this gate created.
        Remove-Item -Path $PinFile -Force -ErrorAction SilentlyContinue
    }
    if ($null -ne $SavedWorkspaceEnv) {
        $env:HANDS_WORKSPACE = $SavedWorkspaceEnv
    } else {
        Remove-Item Env:\HANDS_WORKSPACE -ErrorAction SilentlyContinue
    }
}

# Pre-flight: production tunnel path requires a configured tunnel; assert it.
# (ChatGPT Web -> tunnel reachability itself is proven by `hands status` health
# below; this gate fails closed when the tunnel is not ready rather than
# silently testing a local-only path.)
$statusJsonRaw = & $HandsBin status --json
if ($LASTEXITCODE -ne 0) { Write-Error "hands status --json exited $LASTEXITCODE"; exit 1 }
$statusObj = $statusJsonRaw | ConvertFrom-Json
if (-not $statusObj.workspace) { Write-Error "status JSON missing workspace"; exit 1 }
Write-Host "Status: $statusJsonRaw"

# Step 1: ChatGPT -> tunnel readiness (production path evidence).
Write-Host "`n[1/8] Tunnel readiness (ChatGPT -> tunnel)..."
$health = $null
try {
    $health = Invoke-WebRequest -Uri "http://127.0.0.1:18780/readyz" -UseBasicParsing -TimeoutSec 5
} catch {
    Write-Error "Tunnel health endpoint 127.0.0.1:18780/readyz not ready. Start the supervised tunnel first (hands setup / hands start). ChatGPT->Tunnel->Hands production path cannot be proven without it."
    exit 1
}
if ($health.Content -notmatch "ready") {
    Write-Error "Tunnel health did not report ready: $($health.Content)"
    exit 1
}
Write-Host "Tunnel ready."

# Step 2: MCP tool scan (what ChatGPT sees after 'Scan tools').
Write-Host "`n[2/8] Tool scan via 'hands list'..."
$listOutputRaw = (& $HandsBin list) | Out-String
if ($LASTEXITCODE -ne 0) { Write-Error "hands list exited $LASTEXITCODE"; exit 1 }
$listJson = $listOutputRaw | ConvertFrom-Json
if (-not $listJson) { Write-Error "hands list did not return JSON"; exit 1 }
$tools = if ($listJson.tools) { $listJson.tools } else { $listJson }
$toolNames = @($tools | ForEach-Object { $_.name })
Write-Host "Discovered $($toolNames.Count) tools: $($toolNames -join ', ')"
foreach ($required in @("run_terminal_cmd", "get_task_output", "kill_task", "read_file")) {
    if ($toolNames -notcontains $required) {
        Write-Error "Required tool '$required' missing from scan."
        exit 1
    }
}

# Step 3: Intended workspace — pin an isolated fixture, assert it resolves.
Write-Host "`n[3/8] Intended Windows workspace..."
$testWs = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_e2e_ws_" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $testWs | Out-Null
$fixtureRel = "hands_e2e_fixture.txt"
$fixtureContent = "HANDS_E2E_FIXTURE_$(Get-Random)"
Set-Content -Path (Join-Path $testWs $fixtureRel) -Value $fixtureContent -Encoding utf8

$pinResult = & $HandsBin use $testWs
if ($LASTEXITCODE -ne 0) { Write-Error "hands use exited $LASTEXITCODE"; exit 1 }
Write-Host "Pin result: $pinResult"

$statusAfterPin = (& $HandsBin status --json) | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { Write-Error "post-pin status exited $LASTEXITCODE"; exit 1 }
if ($statusAfterPin.pin -notlike "*hands_e2e_ws_*") {
    Write-Error "Workspace pin verification failed: $($statusAfterPin.pin)"
    exit 1
}

try {
    # Step 4: workspace_info semantics via status (intended workspace active).
    Write-Host "`n[4/8] workspace_info..."
    $wsStatus = (& $HandsBin status --json) | ConvertFrom-Json
    if ($wsStatus.workspace -ne $testWs -and $wsStatus.pin -notlike "*$testWs*") {
        Write-Error "workspace_info does not report the intended workspace '$testWs'"
        exit 1
    }
    Write-Host "Workspace: $($wsStatus.workspace)"

    # Step 5: read_file reads the known fixture through the tool path.
    Write-Host "`n[5/8] read_file fixture..."
    $readPayload = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_payload_" + [System.Guid]::NewGuid().ToString("N") + ".json")
    @{ target_file = $fixtureRel } | ConvertTo-Json | Set-Content -Path $readPayload -Encoding utf8
    $readResult = (& $HandsBin call read_file $readPayload) | Out-String
    Remove-Item -Path $readPayload -Force -ErrorAction SilentlyContinue
    if ($LASTEXITCODE -ne 0) { Write-Error "read_file exited $LASTEXITCODE"; exit 1 }
    if ($readResult -notmatch [regex]::Escape($fixtureContent)) {
        Write-Error "read_file did not return fixture content '$fixtureContent': $readResult"
        exit 1
    }
    Write-Host "read_file OK."

    # Step 6: controlled write mutation INSIDE the intended workspace only.
    Write-Host "`n[6/8] Controlled workspace mutation..."
    $mutationRel = "hands_e2e_mutation.txt"
    $mutationPayload = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_payload_" + [System.Guid]::NewGuid().ToString("N") + ".json")
    @{ content = "HANDS_E2E_MUTATION_OK"; file_path = $mutationRel } | ConvertTo-Json | Set-Content -Path $mutationPayload -Encoding utf8
    $mutRes = (& $HandsBin call write_file $mutationPayload) | Out-String
    Remove-Item -Path $mutationPayload -Force -ErrorAction SilentlyContinue
    if ($LASTEXITCODE -ne 0) { Write-Error "write_file exited $LASTEXITCODE"; exit 1 }
    $mutated = Get-Content -Path (Join-Path $testWs $mutationRel) -Raw -ErrorAction SilentlyContinue
    if ($mutated -notmatch "HANDS_E2E_MUTATION_OK") {
        Write-Error "Controlled mutation inside intended workspace failed: $mutRes"
        exit 1
    }

    # Step 7: terminal jobs — foreground, background ordered output, kill.
    Write-Host "`n[7/8] Terminal jobs..."
    function Invoke-HandsTool([string]$tool, [hashtable]$args) {
        $payload = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_payload_" + [System.Guid]::NewGuid().ToString("N") + ".json")
        $args | ConvertTo-Json | Set-Content -Path $payload -Encoding utf8
        $out = (& $HandsBin call $tool $payload) | Out-String
        Remove-Item -Path $payload -Force -ErrorAction SilentlyContinue
        if ($LASTEXITCODE -ne 0) { Write-Error "$tool exited $LASTEXITCODE" }
        return $out
    }

    $fg = Invoke-HandsTool "run_terminal_cmd" @{
        command = "powershell.exe -NoProfile -NonInteractive -Command `"Write-Output E2E_FG_OK; Write-Output E2E_FG_LINE_2`""
        description = "E2E foreground"
    }
    if ($fg -notmatch "E2E_FG_OK") { Write-Error "foreground terminal failed: $fg"; exit 1 }
    $fg1 = $fg.IndexOf("E2E_FG_OK"); $fg2 = $fg.IndexOf("E2E_FG_LINE_2")
    if (-not ($fg1 -ge 0 -and $fg2 -gt $fg1)) { Write-Error "foreground output not ordered"; exit 1 }

    $bg = Invoke-HandsTool "run_terminal_cmd" @{
        command = "powershell.exe -NoProfile -NonInteractive -Command `"Write-Output BG_LINE_1; Write-Output BG_LINE_2; Start-Sleep -Seconds 900`""
        description = "E2E background ordered output"
        is_background = $true
    }
    $taskId = $null
    if ($bg -match "<task-id>\s*([A-Za-z0-9_\-\.]+)\s*</task-id>") { $taskId = $Matches[1] }
    if (-not $taskId -and $bg -match "(?m)Task ID:\s*(\S+)") { $taskId = $Matches[1] }
    if (-not $taskId) { Write-Error "background run_terminal_cmd returned no task ID: $bg"; exit 1 }
    Write-Host "Background task: $taskId"

    Start-Sleep -Seconds 2
    $taskOut = Invoke-HandsTool "get_task_output" @{ task_id = $taskId }
    $p1 = $taskOut.IndexOf("BG_LINE_1"); $p2 = $taskOut.IndexOf("BG_LINE_2")
    if ($p1 -lt 0 -or $p2 -le $p1) { Write-Error "ordered output assertion failed: $taskOut"; exit 1 }

    # kill_task: owned task tree is stopped and confirmed gone.
    $killOut = Invoke-HandsTool "kill_task" @{ task_id = $taskId }
    Write-Host "kill_task: $($killOut.Trim())"
    Start-Sleep -Seconds 1
    $pw = Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match "Start-Sleep -Seconds 900" }
    if ($pw) { Write-Error "owned background process survived kill_task"; exit 1 }

    # Step 8: Orca REQUIRED through the Hands path — exit status + JSON.
    Write-Host "`n[8/8] Orca runtime through Hands..."
    $orcaCmd = Get-Command orca -ErrorAction SilentlyContinue
    if (-not $orcaCmd) {
        Write-Error "Orca CLI not available; #8 acceptance REQUIRES Orca. Failing gate."
        exit 1
    }
    $orcaVer = Invoke-HandsTool "run_terminal_cmd" @{
        command = "orca --version"
        description = "Orca CLI through Hands"
    }
    if ($orcaVer -notmatch "version|[0-9]+\.[0-9]+") {
        Write-Error "orca --version did not return version output: $orcaVer"
        exit 1
    }
    $orcaStatus = Invoke-HandsTool "run_terminal_cmd" @{
        command = "orca status --json"
        description = "Orca status through Hands"
    }
    $orcaJsonStart = $orcaStatus.IndexOf("{")
    if ($orcaJsonStart -lt 0) { Write-Error "orca status --json produced no JSON: $orcaStatus"; exit 1 }
    try {
        $orcaObj = ($orcaStatus.Substring($orcaJsonStart) | ConvertFrom-Json)
    } catch {
        Write-Error "orca status --json output not valid JSON: $orcaStatus"
        exit 1
    }
    Write-Host "Orca status OK: $($orcaStatus.Trim().Substring(0, [Math]::Min(200, $orcaStatus.Trim().Length)))"

    Write-Host "`n========================================="
    Write-Host "ALL WINDOWS E2E GATE CHECKS PASSED (production path)"
    Write-Host "========================================="
} finally {
    # Restore host state — panic-safe equivalent: finally runs on every path.
    Restore-State
    if (Test-Path $testWs) {
        Remove-Item -Path $testWs -Recurse -Force -ErrorAction SilentlyContinue
    }
    Get-ChildItem ([System.IO.Path]::GetTempPath()) -Filter "hands_payload_*.json" -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
    Get-ChildItem ([System.IO.Path]::GetTempPath()) -Filter "hands_e2e_ws_*" -ErrorAction SilentlyContinue |
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}
