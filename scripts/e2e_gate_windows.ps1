param(
    [string]$ChatGPTEvidence,
    [switch]$LocalOnly
)

# Windows E2E acceptance gate for Issue #8.
#
# The deterministic portion runs one persistent exact-head MCP stdio session so
# background task IDs, get_task_output, and kill_task share the same ToolBridge.
# The ChatGPT Web -> Secure MCP Tunnel boundary cannot be driven by a local
# PowerShell process, so the gate also requires a small evidence JSON captured
# from the real ChatGPT Web session. Missing/invalid evidence is a hard failure.

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$HandsBin = Join-Path $RepoRoot ".grok-build\target\release\hands.exe"
if (-not (Test-Path $HandsBin)) {
    $HandsBin = Join-Path $RepoRoot ".grok-build\target\debug\hands.exe"
}
if (-not (Test-Path $HandsBin)) {
    Write-Error "hands.exe not found. Build the injected crate first."
    exit 1
}

$HeadSha = (& git -C $RepoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or -not $HeadSha) {
    Write-Error "Unable to resolve Hands repository HEAD."
    exit 1
}
$TrackedDirty = (& git -C $RepoRoot status --porcelain --untracked-files=all) | Out-String
if (-not $LocalOnly -and -not [string]::IsNullOrWhiteSpace($TrackedDirty)) {
    Write-Error "Issue #8 acceptance requires a clean worktree at exact HEAD."
    exit 1
}
$HandsSha256 = (Get-FileHash -Path $HandsBin -Algorithm SHA256).Hash.ToLower()
if (-not $ChatGPTEvidence) {
    $ChatGPTEvidence = Join-Path $RepoRoot ".grok-build\chatgpt_e2e_evidence.json"
}

Write-Host "========================================="
Write-Host "Hands Windows E2E Gate"
Write-Host "HEAD:    $HeadSha"
Write-Host "Binary:  $HandsBin"
Write-Host "SHA-256: $HandsSha256"
Write-Host "========================================="

# ---- snapshot before any mutation ------------------------------------------
$SavedConfigDir = $env:HANDS_CONFIG_DIR
$SavedWorkspaceEnv = $env:HANDS_WORKSPACE
$SavedLegacyWorkspaceEnv = $env:GROK_HARNESS_WORKSPACE

$script:McpProc = $null
$script:McpRequestId = 0
$script:TrackedTaskId = $null
$script:ControlProc = $null
$script:ControlDir = $null
$script:TestWs = $null
$script:TestConfigDir = $null

function Get-McpText($result) {
    if (-not $result -or -not $result.content) { return "" }
    return (@($result.content | Where-Object { $_.type -eq "text" } | ForEach-Object { [string]$_.text }) -join "`n")
}

function Invoke-McpRpc([string]$method, $params) {
    if (-not $script:McpProc -or $script:McpProc.HasExited) {
        throw "MCP stdio process is not running."
    }
    $script:McpRequestId++
    $request = @{
        jsonrpc = "2.0"
        id = $script:McpRequestId
        method = $method
        params = $params
    } | ConvertTo-Json -Depth 30 -Compress

    $script:McpProc.StandardInput.WriteLine($request)
    $script:McpProc.StandardInput.Flush()
    $read = $script:McpProc.StandardOutput.ReadLineAsync()
    if (-not $read.Wait(30000)) {
        throw "MCP response timeout for $method"
    }
    $line = $read.Result
    if ([string]::IsNullOrWhiteSpace($line)) {
        throw "MCP process returned empty output for $method"
    }
    try {
        $response = $line | ConvertFrom-Json
    } catch {
        throw "Invalid MCP JSON for $method : $line"
    }
    if ($response.error) {
        throw "MCP $method failed: $($response.error | ConvertTo-Json -Compress)"
    }
    return $response.result
}

function Start-McpSession {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $HandsBin
    $psi.WorkingDirectory = $RepoRoot
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    if ($env:HANDS_CONFIG_DIR) {
        $psi.EnvironmentVariables["HANDS_CONFIG_DIR"] = $env:HANDS_CONFIG_DIR
    }
    # Workspace env overrides outrank the isolated pin; remove them explicitly
    # from the child even if the parent shell carried them before this gate.
    $psi.EnvironmentVariables.Remove("HANDS_WORKSPACE")
    $psi.EnvironmentVariables.Remove("GROK_HARNESS_WORKSPACE")

    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $psi
    if (-not $proc.Start()) { throw "Failed to start exact-head Hands MCP process." }
    $script:McpProc = $proc

    $init = Invoke-McpRpc "initialize" @{
        protocolVersion = "2025-06-18"
        capabilities = @{}
        clientInfo = @{ name = "hands-windows-e2e"; version = "1" }
    }
    if ($init.serverInfo.name -ne "Hands") {
        throw "Unexpected MCP server: $($init.serverInfo | ConvertTo-Json -Compress)"
    }
}

function Invoke-McpTool([string]$name, [hashtable]$arguments) {
    $result = Invoke-McpRpc "tools/call" @{ name = $name; arguments = $arguments }
    $text = Get-McpText $result
    if ($result.isError -eq $true) {
        throw "MCP tool '$name' returned isError=true: $text"
    }
    return $text
}

function Assert-TerminalSuccess([string]$text, [string]$label) {
    if ($text -notmatch '(?mi)^exit:\s*0\b') {
        throw "$label did not report exit: 0 : $text"
    }
}

function Parse-EmbeddedJson([string]$raw, [string]$label) {
    # Native CLIs can write diagnostics before their JSON payload, and Hands
    # can append task metadata after it. Scan every balanced JSON candidate
    # and keep the LAST valid parse; earlier fragments are likely diagnostics
    # (e.g. Crashpad on Windows) rather than the intended data payload.
    $lastValid = $null
    for ($start = 0; $start -lt $raw.Length; $start++) {
        $open = $raw[$start]
        if ($open -ne '{' -and $open -ne '[') { continue }
        $close = if ($open -eq '{') { '}' } else { ']' }
        $depth = 0
        $inString = $false
        $escaped = $false
        for ($i = $start; $i -lt $raw.Length; $i++) {
            $ch = $raw[$i]
            if ($inString) {
                if ($escaped) { $escaped = $false; continue }
                if ($ch -eq '\') { $escaped = $true; continue }
                if ($ch -eq '"') { $inString = $false }
                continue
            }
            if ($ch -eq '"') { $inString = $true; continue }
            if ($ch -eq $open) { $depth++; continue }
            if ($ch -ne $close) { continue }
            $depth--
            if ($depth -ne 0) { continue }
            $jsonText = $raw.Substring($start, $i - $start + 1)
            try {
                $lastValid = $jsonText | ConvertFrom-Json
                $start = $i  # skip past this fragment on next outer iteration
            } catch { }
            break
        }
    }
    if ($null -ne $lastValid) { return $lastValid }
    throw "$label produced no valid embedded JSON payload: $raw"
}

function Validate-ChatGPTEvidence {
    if (-not (Test-Path $ChatGPTEvidence)) {
        $template = [ordered]@{
            source_git_sha = $HeadSha
            connected_via_chatgpt_web = $true
            scan_succeeded = $true
            tool_names = @("workspace_info", "read_file", "write", "run_terminal_cmd", "get_task_output", "kill_task")
            workspace_info_text = "workspace: <intended workspace>`nsource_git_sha: $HeadSha"
            read_file_ok = $true
            write_ok = $true
            foreground_ok = $true
            background_task_id = "<task id returned by ChatGPT Hands call>"
            background_output_ordered = $true
            kill_task_ok = $true
            orca_status_ready = $true
            orca_runtime_operation_ok = $true
            unrelated_tunnel_survived = $true
        }
        $templatePath = Join-Path $RepoRoot ".grok-build\chatgpt_e2e_evidence.template.json"
        $template | ConvertTo-Json -Depth 10 | Set-Content -Path $templatePath -Encoding utf8
        throw "Real ChatGPT Web evidence is required. Capture it with the exact-head connector and save $ChatGPTEvidence. Template: $templatePath"
    }

    try { $evidence = Get-Content -Path $ChatGPTEvidence -Raw | ConvertFrom-Json } catch {
        throw "ChatGPT evidence is not valid JSON: $ChatGPTEvidence"
    }
    if ($evidence.source_git_sha -ne $HeadSha) {
        throw "ChatGPT evidence source_git_sha '$($evidence.source_git_sha)' does not match exact HEAD '$HeadSha'."
    }
    if ($evidence.connected_via_chatgpt_web -ne $true -or $evidence.scan_succeeded -ne $true) {
        throw "ChatGPT Web connection/tool scan was not proven by the evidence file."
    }
    foreach ($required in @("workspace_info", "read_file", "write", "run_terminal_cmd", "get_task_output", "kill_task")) {
        if (@($evidence.tool_names) -notcontains $required) {
            throw "ChatGPT evidence tool scan is missing '$required'."
        }
    }
    if ([string]$evidence.workspace_info_text -notmatch [regex]::Escape("source_git_sha: $HeadSha")) {
        throw "ChatGPT workspace_info evidence does not prove exact-head source provenance."
    }
    foreach ($field in @(
        "read_file_ok",
        "write_ok",
        "foreground_ok",
        "background_output_ordered",
        "kill_task_ok",
        "orca_status_ready",
        "orca_runtime_operation_ok",
        "unrelated_tunnel_survived"
    )) {
        if ($evidence.$field -ne $true) { throw "ChatGPT evidence field '$field' is not true." }
    }
    if (-not [string]$evidence.background_task_id -or [string]$evidence.background_task_id -like "<*") {
        throw "ChatGPT evidence does not contain a real background task ID."
    }
    Write-Host "Real ChatGPT Web evidence matches exact HEAD $HeadSha."
}

function Restore-State {
    if ($script:McpProc -and -not $script:McpProc.HasExited -and $script:TrackedTaskId) {
        try { $null = Invoke-McpTool "kill_task" @{ task_id = $script:TrackedTaskId } } catch {}
    }
    if ($script:McpProc) {
        try { $script:McpProc.StandardInput.Close() } catch {}
        try {
            if (-not $script:McpProc.HasExited -and -not $script:McpProc.WaitForExit(1000)) {
                # The MCP process and every descendant were created by this
                # gate. Tree-kill by the exact owned PID prevents orphaned
                # background fixtures even if task-ID parsing failed.
                $null = & taskkill.exe /PID $script:McpProc.Id /T /F 2>$null
            }
        } catch {}
    }

    if ($script:ControlProc -and -not $script:ControlProc.HasExited) {
        try { Stop-Process -Id $script:ControlProc.Id -Force -ErrorAction SilentlyContinue } catch {}
    }
    if ($script:ControlDir -and (Test-Path $script:ControlDir)) {
        Remove-Item -Path $script:ControlDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($script:TestWs -and (Test-Path $script:TestWs)) {
        Remove-Item -Path $script:TestWs -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($script:TestConfigDir -and (Test-Path $script:TestConfigDir)) {
        Remove-Item -Path $script:TestConfigDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    if ($null -ne $SavedConfigDir) { $env:HANDS_CONFIG_DIR = $SavedConfigDir }
    else { Remove-Item Env:\HANDS_CONFIG_DIR -ErrorAction SilentlyContinue }
    if ($null -ne $SavedWorkspaceEnv) { $env:HANDS_WORKSPACE = $SavedWorkspaceEnv }
    else { Remove-Item Env:\HANDS_WORKSPACE -ErrorAction SilentlyContinue }
    if ($null -ne $SavedLegacyWorkspaceEnv) { $env:GROK_HARNESS_WORKSPACE = $SavedLegacyWorkspaceEnv }
    else { Remove-Item Env:\GROK_HARNESS_WORKSPACE -ErrorAction SilentlyContinue }
}

try {
    # Environment overrides outrank the pin; clear them only for the gate and
    # restore them in finally.
    Remove-Item Env:\HANDS_WORKSPACE -ErrorAction SilentlyContinue
    Remove-Item Env:\GROK_HARNESS_WORKSPACE -ErrorAction SilentlyContinue

    Write-Host "`n[1/9] Exact-head tunnel readiness..."
    $statusRaw = (& $HandsBin status --json) | Out-String
    if ($LASTEXITCODE -ne 0) { throw "hands status --json failed: $statusRaw" }
    $status = $statusRaw | ConvertFrom-Json
    $allowedSourceShas = if ($LocalOnly) { @($HeadSha, "$HeadSha-dirty") } else { @($HeadSha) }
    if ($allowedSourceShas -notcontains $status.source_git_sha) {
        throw "Binary provenance mismatch: status=$($status.source_git_sha) allowed=$($allowedSourceShas -join ',')"
    }
    if ($status.tunnel_ready -ne $true -and -not $LocalOnly) {
        throw "Tunnel is not ready on the exact-head binary; Issue #8 cannot pass."
    }
    if ($status.tunnel_ready -ne $true -and $LocalOnly) {
        Write-Host "LOCAL-ONLY: tunnel is not ready; continuing deterministic MCP checks only."
    }

    Write-Host "`n[2/9] Persistent MCP stdio tool scan..."
    # The local deterministic section uses an isolated Hands config root so it
    # never calls `hands use` (which intentionally auto-enables the real
    # supervisor). This keeps the user's production supervisor state untouched.
    $script:TestConfigDir = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_e2e_config_" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $script:TestConfigDir | Out-Null
    $env:HANDS_CONFIG_DIR = $script:TestConfigDir
    $script:TestWs = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_e2e_ws_" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $script:TestWs | Out-Null
    $fixtureRel = "hands_e2e_fixture.txt"
    $fixtureMarker = "HANDS_E2E_FIXTURE_" + [guid]::NewGuid().ToString("N")
    Set-Content -Path (Join-Path $script:TestWs $fixtureRel) -Value $fixtureMarker -Encoding utf8
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText(
        (Join-Path $script:TestConfigDir "workspace"),
        $script:TestWs,
        $utf8NoBom
    )

    Start-McpSession
    $list = Invoke-McpRpc "tools/list" @{}
    $toolNames = @($list.tools | ForEach-Object { $_.name })
    foreach ($required in @("workspace_info", "read_file", "write", "run_terminal_cmd", "get_task_output", "kill_task")) {
        if ($toolNames -notcontains $required) { throw "Persistent MCP tool list is missing '$required'." }
    }

    Write-Host "`n[3/9] workspace_info + read/write boundary..."
    $workspaceInfo = Invoke-McpTool "workspace_info" @{}
    if ($workspaceInfo -notmatch [regex]::Escape($script:TestWs)) { throw "workspace_info returned wrong workspace: $workspaceInfo" }
    $workspaceSourceOk = $false
    foreach ($allowed in $allowedSourceShas) {
        if ($workspaceInfo -match [regex]::Escape("source_git_sha: $allowed")) { $workspaceSourceOk = $true; break }
    }
    if (-not $workspaceSourceOk) { throw "workspace_info did not prove expected source SHA: $workspaceInfo" }

    $read = Invoke-McpTool "read_file" @{ target_file = $fixtureRel }
    if ($read -notmatch [regex]::Escape($fixtureMarker)) { throw "read_file did not return fixture marker: $read" }

    $mutationRel = "hands_e2e_mutation.txt"
    $null = Invoke-McpTool "write" @{ file_path = $mutationRel; content = "HANDS_E2E_MUTATION_OK" }
    $mutation = Get-Content -Path (Join-Path $script:TestWs $mutationRel) -Raw
    if ($mutation -notmatch "HANDS_E2E_MUTATION_OK") { throw "write mutation did not land in intended workspace." }

    Write-Host "`n[4/9] Foreground terminal..."
    $foreground = Invoke-McpTool "run_terminal_cmd" @{
        command = 'powershell.exe -NoProfile -NonInteractive -Command "Write-Output E2E_FG_1; Write-Output E2E_FG_2; exit 0"'
        description = "Windows E2E foreground"
    }
    Assert-TerminalSuccess $foreground "foreground terminal"
    $fg1 = $foreground.IndexOf("E2E_FG_1")
    $fg2 = $foreground.IndexOf("E2E_FG_2")
    if ($fg1 -lt 0 -or $fg2 -le $fg1) { throw "Foreground output order was not preserved: $foreground" }

    Write-Host "`n[5/9] Unrelated tunnel-client.exe fixture..."
    $script:ControlDir = Join-Path ([System.IO.Path]::GetTempPath()) ("hands_e2e_control_" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $script:ControlDir | Out-Null
    $fakeTunnel = Join-Path $script:ControlDir "tunnel-client.exe"
    Copy-Item -Path ((Get-Command powershell.exe -ErrorAction Stop).Source) -Destination $fakeTunnel
    $script:ControlProc = Start-Process -FilePath $fakeTunnel -ArgumentList @("-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 900") -PassThru
    Start-Sleep -Milliseconds 500
    if ($script:ControlProc.HasExited) { throw "Unrelated tunnel-client.exe fixture failed to start." }

    Write-Host "`n[6/9] Persistent background output + kill_task tree..."
    # Use Python for the process-tree fixture so no nested PowerShell variable
    # expansion can corrupt the child/root PID markers before execution.
    $backgroundCommand = 'python -c "import os,subprocess,sys,time; child=subprocess.Popen([sys.executable,''-c'',''import time; time.sleep(900)'']); print(f''BG_ROOT_PID={os.getpid()}''); print(f''BG_CHILD_PID={child.pid}''); print(''BG_LINE_1''); print(''BG_LINE_2''); sys.stdout.flush(); time.sleep(900)"'
    $background = Invoke-McpTool "run_terminal_cmd" @{
        command = $backgroundCommand.Trim()
        description = "Windows E2E persistent background task"
        is_background = $true
    }
    if ($background -match '<task-id>\s*([^<\s]+)\s*</task-id>') { $script:TrackedTaskId = $Matches[1] }
    elseif ($background -match '(?mi)Task ID:\s*(\S+)') { $script:TrackedTaskId = $Matches[1] }
    if (-not $script:TrackedTaskId) { throw "Background call returned no task ID: $background" }

    Start-Sleep -Seconds 2
    $taskOut = Invoke-McpTool "get_task_output" @{ task_id = $script:TrackedTaskId }
    if ($taskOut -match '(?mi)^Status:\s*failed\b' -or $taskOut -match '(?mi)^Exit Code:\s*[1-9]\d*\b') {
        throw "Background task failed before output/kill assertions: $taskOut"
    }
    $bg1 = $taskOut.IndexOf("BG_LINE_1")
    $bg2 = $taskOut.IndexOf("BG_LINE_2")
    if ($bg1 -lt 0 -or $bg2 -le $bg1) { throw "get_task_output did not preserve ordered output: $taskOut" }
    if ($taskOut -notmatch 'BG_ROOT_PID=(\d+)') { throw "Background root PID missing: $taskOut" }
    $rootPid = [int]$Matches[1]
    if ($taskOut -notmatch 'BG_CHILD_PID=(\d+)') { throw "Background child PID missing: $taskOut" }
    $childPid = [int]$Matches[1]

    $kill = Invoke-McpTool "kill_task" @{ task_id = $script:TrackedTaskId }
    Start-Sleep -Seconds 1
    foreach ($pidToCheck in @($rootPid, $childPid)) {
        if (Get-Process -Id $pidToCheck -ErrorAction SilentlyContinue) {
            throw "kill_task left owned process PID $pidToCheck alive. Response: $kill"
        }
    }
    $script:TrackedTaskId = $null
    if (-not (Get-Process -Id $script:ControlProc.Id -ErrorAction SilentlyContinue)) {
        throw "kill_task terminated unrelated tunnel-client.exe fixture PID $($script:ControlProc.Id)."
    }

    Write-Host "`n[7/9] Orca status through exact-head Hands MCP..."
    $orcaStatus = Invoke-McpTool "run_terminal_cmd" @{ command = "orca status --json"; description = "Orca status through Hands" }
    Assert-TerminalSuccess $orcaStatus "orca status"
    $orcaObj = Parse-EmbeddedJson $orcaStatus "orca status --json"
    $runtime = if ($orcaObj.result.runtime) { $orcaObj.result.runtime } else { $orcaObj.runtime }
    if (-not $runtime -or $runtime.state -ne "ready" -or $runtime.reachable -ne $true) {
        throw "Orca runtime is not ready/reachable: $orcaStatus"
    }

    Write-Host "`n[8/9] Orca folder/runtime operation through exact-head Hands MCP..."
    $orcaRepos = Invoke-McpTool "run_terminal_cmd" @{ command = "orca repo list --json"; description = "Orca repo list through Hands" }
    Assert-TerminalSuccess $orcaRepos "orca repo list"
    $null = Parse-EmbeddedJson $orcaRepos "orca repo list --json"

    Write-Host "`n[9/9] Real ChatGPT Web -> Secure MCP Tunnel evidence..."
    if ($LocalOnly) {
        Write-Host "LOCAL-ONLY: ChatGPT Web evidence intentionally not evaluated."
    } else {
        Validate-ChatGPTEvidence
    }

    Write-Host "`n========================================="
    if ($LocalOnly) {
        Write-Host "LOCAL-ONLY WINDOWS CHECK PASSED -- NOT ISSUE #8 ACCEPTANCE"
    } else {
        Write-Host "WINDOWS E2E GATE PASSED"
    }
    Write-Host "Exact source: $HeadSha"
    Write-Host "Persistent MCP background/get/kill: PASS"
    Write-Host "Orca status + repo operation: PASS"
    if ($LocalOnly) { Write-Host "Real ChatGPT Web evidence: NOT RUN" }
    else { Write-Host "Real ChatGPT Web evidence: PASS" }
    Write-Host "========================================="
} finally {
    Restore-State
}
