# PowerShell install script for Windows.
# Run from repository root: .\install.ps1

$ErrorActionPreference = "Stop"

$RepoRoot = $PSScriptRoot
if (-not $RepoRoot) {
    $RepoRoot = (Get-Location).Path
}

# Prefix directory for binaries: %LOCALAPPDATA%\Programs\hands\bin or custom prefix.
# A custom prefix must be absolute; persisting a relative directory into PATH
# would make command resolution depend on each shell's current directory.
if ($env:HANDS_PREFIX) {
    if (-not [System.IO.Path]::IsPathRooted($env:HANDS_PREFIX)) {
        Write-Error "HANDS_PREFIX must be an absolute Windows path. Got: $($env:HANDS_PREFIX)"
        exit 1
    }
    $Prefix = [System.IO.Path]::GetFullPath($env:HANDS_PREFIX)
} else {
    $Prefix = Join-Path $env:LOCALAPPDATA "Programs\hands\bin"
}
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

# 5. Ensure the managed Hands prefix is first on User PATH. Before relying on
# User PATH, fail closed if Machine PATH can shadow this user-local install.
# Relative Machine PATH entries are inherently CWD-dependent, so no single
# install-time normalization can prove future shell resolution is safe.
$MachinePath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::Machine)
$PathExts = @($env:PATHEXT -split ';' | Where-Object { $_ })
if ($PathExts.Count -eq 0) { $PathExts = @('.COM', '.EXE', '.BAT', '.CMD') }
$MachineHandsCollisions = @()
foreach ($entry in @($MachinePath -split ';' | Where-Object { $_ })) {
    $expandedEntry = [Environment]::ExpandEnvironmentVariables($entry.Trim().Trim('"'))
    if (-not $expandedEntry) { continue }
    if (-not [System.IO.Path]::IsPathRooted($expandedEntry)) {
        Write-Error "Machine PATH contains relative entry '$entry'. Hands cannot guarantee that the user-local install will not be shadowed in future working directories. Make the Machine PATH entry absolute, then rerun install.ps1."
        exit 1
    }
    $expandedEntry = [System.IO.Path]::GetFullPath($expandedEntry)
    foreach ($ext in $PathExts) {
        $candidate = Join-Path $expandedEntry ("hands" + $ext)
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $resolved = [System.IO.Path]::GetFullPath($candidate)
            if ($resolved -ine [System.IO.Path]::GetFullPath($DestBin)) {
                $MachineHandsCollisions += $resolved
            }
        }
    }
}
$MachineHandsCollisions = @($MachineHandsCollisions | Sort-Object -Unique)
if ($MachineHandsCollisions.Count -gt 0) {
    Write-Error ("Machine PATH contains another 'hands' command that would shadow this user install: " + ($MachineHandsCollisions -join ', ') + ". Remove or rename the collision, or install Hands to the machine-managed location deliberately.")
    exit 1
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
$PathEntries = @($UserPath -split ';' | Where-Object { $_ -and $_ -ine $Prefix })
$NewUserPath = (@($Prefix) + $PathEntries) -join ';'
if ($UserPath -ne $NewUserPath) {
    Write-Host "Putting $Prefix first on User PATH..."
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, [EnvironmentVariableTarget]::User)
    $env:Path = "$Prefix;$env:Path"
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

# 6. Download and verify the pinned official tunnel-client.exe on every install.
# Never trust an existing PATH entry or cached executable with the Runtime Key.
$VerifiedBin = Join-Path $Prefix "tunnel-client.exe"
$TempZip = Join-Path ([System.IO.Path]::GetTempPath()) ("tunnel-client-" + [System.Guid]::NewGuid().ToString("N") + ".zip")
$ExtractDir = Join-Path ([System.IO.Path]::GetTempPath()) ("tunnel-client-extract-" + [System.Guid]::NewGuid().ToString("N"))
try {
    Write-Host "Downloading pinned official OpenAI artifact ($TunnelClientVersion)..."
    Invoke-WebRequest -Uri $TunnelClientZipUrl -OutFile $TempZip -UseBasicParsing
    $ActualHash = (Get-FileHash -Path $TempZip -Algorithm SHA256).Hash.ToLower()
    if ($ActualHash -ne $TunnelClientExpectedSha256.ToLower()) {
        Write-Error "SHA-256 verification failed for downloaded tunnel-client artifact. Expected: $TunnelClientExpectedSha256, got: $ActualHash"
        exit 1
    }
    Write-Host "Verified archive SHA-256: $ActualHash"

    Expand-Archive -Path $TempZip -DestinationPath $ExtractDir -Force
    $ExtractedExe = Get-ChildItem -Path $ExtractDir -Filter "tunnel-client.exe" -Recurse | Select-Object -First 1
    if (-not $ExtractedExe) {
        Write-Error "tunnel-client.exe not found in downloaded zip archive."
        exit 1
    }

    $ExpectedExeHash = (Get-FileHash -Path $ExtractedExe.FullName -Algorithm SHA256).Hash.ToLower()
    $ExistingExeHash = if (Test-Path $VerifiedBin) {
        (Get-FileHash -Path $VerifiedBin -Algorithm SHA256).Hash.ToLower()
    } else {
        $null
    }
    if ($ExistingExeHash -eq $ExpectedExeHash) {
        Write-Host "Existing tunnel-client.exe matches the verified pinned artifact."
    } else {
        # Windows cannot overwrite a running executable. Stop only processes
        # whose ExecutablePath is exactly the Hands-managed verified binary;
        # never kill unrelated tunnel-client.exe instances elsewhere.
        Get-CimInstance Win32_Process -Filter "Name='tunnel-client.exe'" -ErrorAction SilentlyContinue |
            Where-Object { $_.ExecutablePath -and $_.ExecutablePath -ieq $VerifiedBin } |
            ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
        Start-Sleep -Milliseconds 200
        $StillRunning = @(Get-CimInstance Win32_Process -Filter "Name='tunnel-client.exe'" -ErrorAction SilentlyContinue |
            Where-Object { $_.ExecutablePath -and $_.ExecutablePath -ieq $VerifiedBin })
        if ($StillRunning.Count -gt 0) {
            Write-Error "Hands-managed tunnel-client.exe is still running; refusing to overwrite it."
            exit 1
        }

        Copy-Item -Path $ExtractedExe.FullName -Destination $VerifiedBin -Force
        $InstalledHash = (Get-FileHash -Path $VerifiedBin -Algorithm SHA256).Hash.ToLower()
        if ($InstalledHash -ne $ExpectedExeHash) {
            Write-Error "Installed tunnel-client.exe hash mismatch after copy."
            exit 1
        }
        Write-Host "Installed verified tunnel-client.exe to $VerifiedBin"
    }
} finally {
    if (Test-Path $TempZip) {
        Remove-Item -Path $TempZip -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path $ExtractDir) {
        Remove-Item -Path $ExtractDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
Write-Host "tunnel-client ready: $VerifiedBin"

# 7. Setup check or next steps (check $LASTEXITCODE: native exe non-zero does not throw)
# Windows disconnect notifications use the user-level NotifyIcon fallback when
# an unpackaged WinRT AppUserModelID is unavailable, so installation does not
# claim or depend on an AUMID registration it cannot actually establish.
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
