# PowerShell install script for Windows.
# Run from repository root: .\install.ps1

$ErrorActionPreference = "Stop"

$RepoRoot = $PSScriptRoot
if (-not $RepoRoot) {
    $RepoRoot = (Get-Location).Path
}

# Prefix directory for binaries: %LOCALAPPDATA%\Programs\hands\bin or custom prefix
$Prefix = if ($env:HANDS_PREFIX) { $env:HANDS_PREFIX } else { Join-Path $env:LOCALAPPDATA "Programs\hands\bin" }
$Cache = if ($env:HANDS_CACHE) { $env:HANDS_CACHE } elseif ($env:GROK_HARNESS_CACHE) { $env:GROK_HARNESS_CACHE } else { Join-Path $env:LOCALAPPDATA "hands\cache" }
$GrokBuildUrl = if ($env:GROK_BUILD_URL) { $env:GROK_BUILD_URL } else { "https://github.com/xai-org/grok-build.git" }
$GrokBuildRef = if ($env:GROK_BUILD_REF) { $env:GROK_BUILD_REF } else { "9684fa3cdbf2995e30ea8b9b637f1db008f144fc" }

# Pinned official tunnel-client release artifact for Windows
$TunnelClientVersion = if ($env:TUNNEL_CLIENT_VERSION) { $env:TUNNEL_CLIENT_VERSION } else { "v0.0.13" }
$TunnelClientZipUrl = if ($env:TUNNEL_CLIENT_URL) { $env:TUNNEL_CLIENT_URL } else { "https://github.com/openai/tunnel-client/releases/download/$TunnelClientVersion/tunnel-client-$TunnelClientVersion-windows-amd64.zip" }
$TunnelClientExpectedSha256 = if ($env:TUNNEL_CLIENT_SHA256) { $env:TUNNEL_CLIENT_SHA256 } else { "17113162b353906bbb884c3ed7620facba5cc72b5fdc94fd54fd7208c7166edb" }

# 1. Validate prerequisites
Write-Host "Validating prerequisites..."
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Error "Git is required. Install Git from https://git-scm.com or via 'winget install Git.Git'."
    exit 1
}

$Python = if (Get-Command python -ErrorAction SilentlyContinue) { "python" } elseif (Get-Command python3 -ErrorAction SilentlyContinue) { "python3" } elseif (Get-Command py -ErrorAction SilentlyContinue) { "py" } else { $null }
if (-not $Python) {
    Write-Error "Python is required. Install Python from https://python.org or via 'winget install Python.Python.3.12'."
    exit 1
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Rust / Cargo is required. Install Rust from https://rustup.rs."
    exit 1
}

New-Item -ItemType Directory -Force -Path $Prefix, $Cache | Out-Null
$GrokBuild = Join-Path $Cache "grok-build"

# 2. Fetch pinned Grok Build revision
if (Test-Path (Join-Path $GrokBuild ".git")) {
    Write-Host "Updating grok-build in $GrokBuild..."
    git -C $GrokBuild fetch --depth 1 origin $GrokBuildRef
    git -C $GrokBuild checkout --force FETCH_HEAD
    git -C $GrokBuild clean -fd -e target/
} else {
    Write-Host "Cloning grok-build into $GrokBuild ($GrokBuildRef)..."
    New-Item -ItemType Directory -Force -Path $GrokBuild | Out-Null
    git -C $GrokBuild init
    git -C $GrokBuild remote add origin $GrokBuildUrl
    git -C $GrokBuild fetch --depth 1 origin $GrokBuildRef
    git -C $GrokBuild checkout --force FETCH_HEAD
}

# 3. Inject Hands crate and patch for Windows
Write-Host "Injecting Hands crate into grok-build..."
& $Python (Join-Path $RepoRoot "scripts\inject.py") $RepoRoot $GrokBuild
if ($LASTEXITCODE -ne 0) {
    Write-Error "Failed to inject Hands crate into grok-build."
    exit $LASTEXITCODE
}

# 4. Build hands.exe in release mode
Write-Host "Building hands release binary..."
$CargoArgs = @("build", "--release", "-p", "hands", "--manifest-path", (Join-Path $GrokBuild "Cargo.toml"))
if ($env:JOBS) {
    $CargoArgs += @("-j", $env:JOBS)
}
& cargo @CargoArgs
if ($LASTEXITCODE -ne 0) {
    Write-Error "Cargo build failed."
    exit $LASTEXITCODE
}

$BuiltBin = Join-Path $GrokBuild "target\release\hands.exe"
$DestBin = Join-Path $Prefix "hands.exe"

# Stop any running hands instance if overwriting
Get-Process -Name hands -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

Copy-Item -Path $BuiltBin -Destination $DestBin -Force

Write-Host ""
Write-Host "Installed: $DestBin"
& $DestBin --version
Write-Host ""

# 5. Ensure $Prefix is on User PATH
$UserPath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
$PathEntries = $UserPath -split ';' | Where-Object { $_ -ne "" }
if ($PathEntries -notcontains $Prefix) {
    Write-Host "Adding $Prefix to User PATH..."
    $NewUserPath = "$UserPath;$Prefix"
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, [EnvironmentVariableTarget]::User)
    $env:Path = "$env:Path;$Prefix"
}

# 6. Locate or download official tunnel-client.exe
$TunnelClientCandidate = Get-Command tunnel-client -ErrorAction SilentlyContinue
if (-not $TunnelClientCandidate -and (Test-Path (Join-Path $Prefix "tunnel-client.exe"))) {
    $TunnelClientCandidate = Join-Path $Prefix "tunnel-client.exe"
}

if (-not $TunnelClientCandidate) {
    Write-Host "Locating tunnel-client.exe: downloading pinned official OpenAI artifact ($TunnelClientVersion)..."
    $TempZip = Join-Path ([System.IO.Path]::GetTempPath()) ("tunnel-client-" + [System.Guid]::NewGuid().ToString("N") + ".zip")
    try {
        Invoke-WebRequest -Uri $TunnelClientZipUrl -OutFile $TempZip -UseBasicParsing
        $ActualHash = (Get-FileHash -Path $TempZip -Algorithm SHA256).Hash.ToLower()
        if ($ActualHash -ne $TunnelClientExpectedSha256.ToLower()) {
            Remove-Item -Path $TempZip -Force -ErrorAction SilentlyContinue
            Write-Error "SHA-256 verification failed for downloaded tunnel-client artifact. Expected: $TunnelClientExpectedSha256, got: $ActualHash"
            exit 1
        }
        Write-Host "Verified SHA-256: $ActualHash"
        $ExtractDir = Join-Path ([System.IO.Path]::GetTempPath()) ("tunnel-client-extract-" + [System.Guid]::NewGuid().ToString("N"))
        Expand-Archive -Path $TempZip -DestinationPath $ExtractDir -Force
        $ExtractedExe = Get-ChildItem -Path $ExtractDir -Filter "tunnel-client.exe" -Recurse | Select-Object -First 1
        if ($ExtractedExe) {
            Copy-Item -Path $ExtractedExe.FullName -Destination (Join-Path $Prefix "tunnel-client.exe") -Force
            Write-Host "Installed tunnel-client.exe to $Prefix"
        } else {
            Write-Error "tunnel-client.exe not found in downloaded zip archive."
            exit 1
        }
        Remove-Item -Path $ExtractDir -Recurse -Force -ErrorAction SilentlyContinue
    } finally {
        if (Test-Path $TempZip) {
            Remove-Item -Path $TempZip -Force -ErrorAction SilentlyContinue
        }
    }
} else {
    Write-Host "tunnel-client located: $TunnelClientCandidate"
}

# 7. Setup check or next steps
if ($env:CONTROL_PLANE_API_KEY -and $env:CONTROL_PLANE_TUNNEL_ID) {
    try {
        & $DestBin setup
        Write-Host "Tunnel setup attempted (keys found in env)."
    } catch {
        Write-Host "Setup returned non-zero."
    }
} else {
    Write-Host "Next steps:"
    Write-Host "  1. cd \path\to\your\repo"
    Write-Host "  2. hands setup"
}
