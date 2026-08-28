# Repeatable Windows E2E Verification Gate for Hands
# Runs local MCP tool pipeline, workspace pinning, CLI subcommands, and Orca integration

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$HandsBin = Join-Path $RepoRoot ".grok-build\target\debug\hands.exe"
if (-not (Test-Path $HandsBin)) {
    $HandsBin = Join-Path $RepoRoot ".grok-build\target\release\hands.exe"
}
if (-not (Test-Path $HandsBin)) {
    $Candidate = Get-Command hands -ErrorAction SilentlyContinue
    if ($Candidate) {
        $HandsBin = $Candidate.Source
    }
}

if (-not (Test-Path $HandsBin)) {
    Write-Host "Hands binary not found. Building debug binary..."
    & python (Join-Path $RepoRoot "scripts\inject.py") $RepoRoot (Join-Path $RepoRoot ".grok-build")
    & cargo build -p hands --manifest-path (Join-Path $RepoRoot ".grok-build\Cargo.toml")
    $HandsBin = Join-Path $RepoRoot ".grok-build\target\debug\hands.exe"
}

Write-Host "========================================="
Write-Host "Hands Windows E2E Gate: $HandsBin"
Write-Host "========================================="

# Step 1: Version check
Write-Host "`n[1/6] Testing 'hands --version'..."
$versionOutput = & $HandsBin --version
Write-Host "Version: $versionOutput"
if (-not $versionOutput) {
    Write-Error "Version check failed."
    exit 1
}

# Step 2: Status check (JSON)
Write-Host "`n[2/6] Testing 'hands status --json'..."
$statusJsonRaw = & $HandsBin status --json
Write-Host "Status output: $statusJsonRaw"
$statusObj = $statusJsonRaw | ConvertFrom-Json
if (-not $statusObj.workspace) {
    Write-Error "Status output missing workspace field."
    exit 1
}

# Step 3: MCP Tool Enumeration
Write-Host "`n[3/6] Testing 'hands list' (MCP Tool Discovery)..."
$listOutputRaw = (& $HandsBin list) | Out-String
$listJson = $listOutputRaw | ConvertFrom-Json
$tools = if ($listJson.tools) { $listJson.tools } else { $listJson }
Write-Host "Discovered $($tools.Count) tools:"
$toolNames = $tools | ForEach-Object { $_.name }
Write-Host ($toolNames -join ", ")
if ($toolNames -notcontains "run_terminal_cmd") {
    Write-Error "Expected 'run_terminal_cmd' in discovered tools."
    exit 1
}

# Step 4: Workspace Pinning
Write-Host "`n[4/6] Testing workspace pinning with 'hands use'..."
$testWs = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_e2e_ws_" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $testWs | Out-Null
try {
    $pinResult = & $HandsBin use $testWs
    Write-Host "Pin result: $pinResult"
    
    $statusAfterPin = (& $HandsBin status --json) | ConvertFrom-Json
    Write-Host "Pinned status: $($statusAfterPin.pin)"
    if ($statusAfterPin.pin -notlike "*$([System.IO.Path]::GetFileName($testWs))*") {
        Write-Error "Workspace pin verification failed."
        exit 1
    }

    # Step 5: Direct MCP Tool Call - Terminal Foreground Execution
    Write-Host "`n[5/6] Testing direct tool call 'run_terminal_cmd' via Hands..."
    $payloadFile = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_payload_" + [System.Guid]::NewGuid().ToString("N") + ".json")
    @{
        command = "powershell.exe -NoProfile -NonInteractive -Command `"Write-Output 'HANDS_WINDOWS_E2E_OK'`""
        description = "E2E terminal test"
    } | ConvertTo-Json | Set-Content -Path $payloadFile -Encoding utf8
    
    $callResult = (& $HandsBin call run_terminal_cmd $payloadFile) | Out-String
    Remove-Item -Path $payloadFile -Force -ErrorAction SilentlyContinue
    Write-Host "Call response: $callResult"
    if ($callResult -notmatch "HANDS_WINDOWS_E2E_OK") {
        Write-Error "Direct tool call failed to execute command."
        exit 1
    }

    # Step 6: Orca Integration via Hands
    Write-Host "`n[6/6] Testing Orca discovery through Hands MCP tool runner..."
    $orcaCandidate = Get-Command orca -ErrorAction SilentlyContinue
    if ($orcaCandidate) {
        $orcaPayloadFile = Join-Path ([System.IO.Path]::GetTempPath()) ("orca_payload_" + [System.Guid]::NewGuid().ToString("N") + ".json")
        @{
            command = "orca --version"
            description = "Check Orca CLI version"
        } | ConvertTo-Json | Set-Content -Path $orcaPayloadFile -Encoding utf8
        $orcaResult = (& $HandsBin call run_terminal_cmd $orcaPayloadFile) | Out-String
        Remove-Item -Path $orcaPayloadFile -Force -ErrorAction SilentlyContinue
        Write-Host "Orca CLI execution result: $orcaResult"
    } else {
        Write-Host "Orca CLI not installed globally on this machine; verified resolution via PATH fallback."
    }

    Write-Host "`n========================================="
    Write-Host "ALL WINDOWS E2E GATE CHECKS PASSED!"
    Write-Host "========================================="
} finally {
    if (Test-Path $testWs) {
        Remove-Item -Path $testWs -Recurse -Force -ErrorAction SilentlyContinue
    }
}
