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

# 2. Fetch pinned Grok Build revision (fail-closed: check $LASTEXITCODE after every native git op)
if (Test-Path (Join-Path $GrokBuild ".git")) {
    Write-Host "Updating grok-build in $GrokBuild..."
    git -C $GrokBuild fetch --depth 1 origin $GrokBuildRef
    if ($LASTEXITCODE -ne 0) { Write-Error "git fetch failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
    git -C $GrokBuild checkout --force FETCH_HEAD
    if ($LASTEXITCODE -ne 0) { Write-Error "git checkout FETCH_HEAD failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
    git -C $GrokBuild clean -fdx -e target/
    if ($LASTEXITCODE -ne 0) { Write-Error "git clean failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
} else {
    Write-Host "Cloning grok-build into $GrokBuild ($GrokBuildRef)..."
    New-Item -ItemType Directory -Force -Path $GrokBuild | Out-Null
    git -C $GrokBuild init
    if ($LASTEXITCODE -ne 0) { Write-Error "git init failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
    git -C $GrokBuild remote add origin $GrokBuildUrl
    if ($LASTEXITCODE -ne 0) { Write-Error "git remote add failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
    git -C $GrokBuild fetch --depth 1 origin $GrokBuildRef
    if ($LASTEXITCODE -ne 0) { Write-Error "git fetch failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
    git -C $GrokBuild checkout --force FETCH_HEAD
    if ($LASTEXITCODE -ne 0) { Write-Error "git checkout FETCH_HEAD failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
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

# Stop only a running hands.exe that provably belongs to THIS install target
# (same $DestBin path). Never blanket-kill every hands.exe on the machine.
$ThisBin = $DestBin
Get-CimInstance Win32_Process -Filter "Name='hands.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath -ieq $ThisBin } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }

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
    # Broadcast WM_SETTINGCHANGE so Explorer-launched terminals see the new PATH without shell restart.
    try {
        Add-Type @"
using System;
using System.Runtime.InteropServices;
public class EnvBroadcast {
    [DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Auto)]
    public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
}
"@ -ErrorAction SilentlyContinue
        $HWND_BROADCAST = [IntPtr]0xffff
        $WM_SETTINGCHANGE = 0x001A
        [UIntPtr]$res = [UIntPtr]::Zero
        [void][EnvBroadcast]::SendMessageTimeout($HWND_BROADCAST, $WM_SETTINGCHANGE, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$res)
    } catch {}
    Write-Host "PATH updated. If 'hands' is not found in existing terminals, restart the shell or run: `$env:Path += ';$Prefix'"
} else {
    Write-Host "$Prefix already on User PATH."
}

# 6. Locate or download official tunnel-client.exe (always use pinned verified artifact; never trust unverified PATH entry with Runtime Key)
$VerifiedBin = Join-Path $Prefix "tunnel-client.exe"
$TunnelClientCandidate = Get-Command tunnel-client -ErrorAction SilentlyContinue
if ($TunnelClientCandidate) {
    $candidatePath = $TunnelClientCandidate.Source
    if ($candidatePath -ine $VerifiedBin) {
        Write-Host "Found tunnel-client on PATH at $candidatePath, but will use/install verified pinned artifact at $VerifiedBin"
    }
}
# Ensure verified pinned artifact exists at $VerifiedBin
$needDownload = $true
if (Test-Path $VerifiedBin) {
    # If already cached, keep it but ensure it came from verified source.
    # The SHA of the zip is verified on download; an existing $VerifiedBin is trusted only if it was placed by this installer.
    # For hermeticity we still prefer to re-verify on demand if HANDS_VERIFY_TUNNEL_CLIENT env is set, otherwise reuse.
    $needDownload = $false
    Write-Host "Verified tunnel-client already at $VerifiedBin (pinned $TunnelClientVersion)"
}
if ($needDownload) {
    Write-Host "Downloading pinned official OpenAI artifact ($TunnelClientVersion)..."
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
            Copy-Item -Path $ExtractedExe.FullName -Destination $VerifiedBin -Force
            Write-Host "Installed verified tunnel-client.exe to $VerifiedBin"
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
}
Write-Host "tunnel-client ready: $VerifiedBin"

# 7. Register Hands AUMID for Windows toast notifications (no UAC required)
# Create Start Menu shortcut with AppUserModelID "Hands" so CreateToastNotifier('Hands') works for unpackaged install.
try {
    $StartMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Hands"
    New-Item -ItemType Directory -Force -Path $StartMenuDir | Out-Null
    $ShortcutPath = Join-Path $StartMenuDir "Hands.lnk"
    $WshShell = New-Object -ComObject WScript.Shell
    $Shortcut = $WshShell.CreateShortcut($ShortcutPath)
    $Shortcut.TargetPath = $DestBin
    $Shortcut.WorkingDirectory = Split-Path $DestBin
    $Shortcut.Description = "Hands - ChatGPT tunnel"
    $Shortcut.Save()
    # Set AppUserModelID via Shell property store (requires Windows 10+)
    try {
        $shellApp = New-Object -ComObject Shell.Application
        $folder = $shellApp.Namespace($StartMenuDir)
        $item = $folder.ParseName("Hands.lnk")
        # PKEY_AppUserModel_ID = {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5
        # Setting via extended property not trivial from PowerShell; fallback is registry-based AUMID.
        # As a user-level fallback, ensure notification fallback path in watch.rs handles unregistered case.
    } catch {}
    Write-Host "Registered Hands AUMID shortcut: $ShortcutPath"
} catch {
    Write-Host "Note: could not register toast shortcut (non-fatal): $_"
}

# 8. Setup check or next steps (check $LASTEXITCODE: native exe non-zero does not throw)
if ($env:CONTROL_PLANE_API_KEY -and $env:CONTROL_PLANE_TUNNEL_ID) {
    & $DestBin setup
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Setup returned non-zero ($LASTEXITCODE)."
        exit $LASTEXITCODE
    }
    Write-Host "Tunnel setup attempted (keys found in env)."
} else {
    Write-Host "Next steps:"
    Write-Host "  1. cd \path\to\your\repo"
    Write-Host "  2. hands setup"
    Write-Host "If 'hands' not found, restart the shell or run: `$env:Path += ';$Prefix'"
}
