[CmdletBinding()]
param(
    [string]$GrokBuild
)

# Fast Windows developer gate.
# Intentionally does NOT install Hands, build --release, or mutate supervisor/tunnel state.

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

function Resolve-PythonCommand {
    foreach ($name in @("python", "python3", "py")) {
        if (Get-Command $name -ErrorAction SilentlyContinue) {
            return $name
        }
    }
    throw "Python is required to run scripts/inject.py."
}

function Resolve-GrokBuild([string]$ExplicitPath) {
    if ($ExplicitPath) {
        $resolved = (Resolve-Path $ExplicitPath -ErrorAction Stop).Path
        if (-not (Test-Path (Join-Path $resolved "Cargo.toml") -PathType Leaf)) {
            throw "Grok Build checkout is missing Cargo.toml: $resolved"
        }
        return $resolved
    }

    $candidates = New-Object System.Collections.Generic.List[string]
    $candidates.Add((Join-Path $RepoRoot ".grok-build"))
    if ($env:HANDS_CACHE) { $candidates.Add((Join-Path $env:HANDS_CACHE "grok-build")) }
    if ($env:GROK_HARNESS_CACHE) { $candidates.Add((Join-Path $env:GROK_HARNESS_CACHE "grok-build")) }
    if ($env:LOCALAPPDATA) { $candidates.Add((Join-Path $env:LOCALAPPDATA "hands\cache\grok-build")) }
    $candidates.Add((Join-Path (Split-Path $RepoRoot -Parent) "grok-build"))

    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path (Join-Path $candidate "Cargo.toml") -PathType Leaf)) {
            return (Resolve-Path $candidate).Path
        }
    }

    throw "No existing Grok Build checkout found. Pass -GrokBuild <path>. This smoke gate will not clone/fetch one automatically."
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "Cargo is required for the Windows smoke gate."
}

$Python = Resolve-PythonCommand
$GrokBuild = Resolve-GrokBuild $GrokBuild
$Manifest = Join-Path $GrokBuild "Cargo.toml"
$Inject = Join-Path $RepoRoot "scripts\inject.py"

Write-Host "Hands Windows smoke"
Write-Host "Repo:       $RepoRoot"
Write-Host "Grok Build: $GrokBuild"
Write-Host ""

Write-Host "[1/3] Inject current Hands crate..."
& $Python $Inject $RepoRoot $GrokBuild
if ($LASTEXITCODE -ne 0) {
    throw "scripts/inject.py failed with exit code $LASTEXITCODE."
}

Write-Host "`n[2/3] cargo check -p hands..."
& cargo check -p hands --manifest-path $Manifest
if ($LASTEXITCODE -ne 0) {
    throw "cargo check failed with exit code $LASTEXITCODE."
}

Write-Host "`n[3/3] hands doctor --json (debug build via cargo run)..."
$DoctorRaw = (& cargo run --quiet -p hands --manifest-path $Manifest -- doctor --json) | Out-String
if ($LASTEXITCODE -ne 0) {
    throw "hands doctor --json failed with exit code $LASTEXITCODE."
}

try {
    $Doctor = $DoctorRaw | ConvertFrom-Json
} catch {
    throw "hands doctor --json did not produce valid JSON. Raw output: $DoctorRaw"
}

if ($Doctor.name -ne "Hands" -or $null -eq $Doctor.checks) {
    throw "hands doctor --json returned an unexpected schema."
}

Write-Host ("doctor.ok: {0}" -f $Doctor.ok)
Write-Host ("summary:   {0}" -f $Doctor.summary)
if ($Doctor.ok -ne $true) {
    Write-Warning "Host diagnostics are not fully OK. The smoke gate only requires the diagnostic command/schema to work; inspect the reported checks before any runtime-sensitive task."
}

Write-Host "`nPASS: inject + cargo check + doctor JSON contract"
