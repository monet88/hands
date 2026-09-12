param(
    [string]$Workspace = (Split-Path $PSScriptRoot -Parent),
    [ValidateSet("chrome", "edge")]
    [string]$Browser = "chrome",
    [string]$ExtensionId
)

$ErrorActionPreference = "Stop"

if ($env:OS -ne "Windows_NT") {
    throw "Hands Return Bridge local installer currently supports Windows only."
}

$repoRoot = [IO.Path]::GetFullPath((Split-Path $PSScriptRoot -Parent))
$extensionPath = [IO.Path]::GetFullPath((Join-Path $repoRoot "extension")).TrimEnd('\')
$manifestPath = Join-Path $repoRoot "bridge\native\Cargo.toml"
$debugBinary = Join-Path $repoRoot "bridge\native\target\debug\hands-return-bridge.exe"
$installDir = Join-Path $env:LOCALAPPDATA "Hands\return-bridge"
$installedBinary = Join-Path $installDir "hands-return-bridge.exe"

function Resolve-ExtensionId {
    param(
        [string]$BrowserName,
        [string]$ExpectedPath
    )

    $userDataRoot = if ($BrowserName -eq "edge") {
        Join-Path $env:LOCALAPPDATA "Microsoft\Edge\User Data"
    } else {
        Join-Path $env:LOCALAPPDATA "Google\Chrome\User Data"
    }

    if (-not (Test-Path -LiteralPath $userDataRoot)) {
        throw "Browser user-data directory not found: $userDataRoot"
    }

    $expected = [IO.Path]::GetFullPath($ExpectedPath).TrimEnd('\')
    $ids = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $profiles = Get-ChildItem -LiteralPath $userDataRoot -Directory |
        Where-Object { $_.Name -eq "Default" -or $_.Name -like "Profile *" }

    foreach ($profile in $profiles) {
        foreach ($fileName in @("Secure Preferences", "Preferences")) {
            $preferencesPath = Join-Path $profile.FullName $fileName
            if (-not (Test-Path -LiteralPath $preferencesPath)) {
                continue
            }

            try {
                $json = Get-Content -LiteralPath $preferencesPath -Raw -Encoding utf8 |
                    ConvertFrom-Json
                $settings = $json.extensions.settings
                if (-not $settings) {
                    continue
                }
                foreach ($property in $settings.PSObject.Properties) {
                    $id = $property.Name
                    $entry = $property.Value
                    if (-not $entry.path) {
                        continue
                    }
                    try {
                        $candidate = [IO.Path]::GetFullPath([string]$entry.path).TrimEnd('\')
                    } catch {
                        continue
                    }
                    if ([string]::Equals($candidate, $expected, [StringComparison]::OrdinalIgnoreCase)) {
                        [void]$ids.Add([string]$id)
                    }
                }
            } catch {
                continue
            }
        }
    }

    if ($ids.Count -eq 0) {
        throw "Hands Return Bridge extension was not found in $BrowserName. Load unpacked from '$ExpectedPath' first, then rerun this script."
    }
    if ($ids.Count -gt 1) {
        throw "Multiple extension IDs were found for '$ExpectedPath'. Remove stale duplicate unpacked installs or rerun with -ExtensionId <id>."
    }
    return @($ids)[0]
}

if ([string]::IsNullOrWhiteSpace($ExtensionId)) {
    $ExtensionId = Resolve-ExtensionId -BrowserName $Browser -ExpectedPath $extensionPath
} else {
    $ExtensionId = $ExtensionId.Trim()
}

$Workspace = [IO.Path]::GetFullPath($Workspace)
if (-not (Test-Path -LiteralPath $Workspace -PathType Container)) {
    throw "Workspace directory does not exist: $Workspace"
}

Write-Host "Building Return Bridge companion..."
& cargo build --manifest-path $manifestPath
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}
if (-not (Test-Path -LiteralPath $debugBinary -PathType Leaf)) {
    throw "Built Return Bridge binary was not found: $debugBinary"
}

New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Copy-Item -LiteralPath $debugBinary -Destination $installedBinary -Force

try {
    $regKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $false)
    $rawUserPath = $null
    if ($regKey) {
        try { $rawUserPath = $regKey.GetValue("Path", $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) } finally { $regKey.Close() }
    }
} catch { $rawUserPath = $null }
if ([string]::IsNullOrEmpty($rawUserPath)) { $rawUserPath = "" }
    $pathEntries = @($rawUserPath -split ";" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne "" })
    $alreadyPresent = @($pathEntries | Where-Object { $_.TrimEnd("\").Equals($installDir.TrimEnd("\"), [System.StringComparison]::OrdinalIgnoreCase) }).Count -gt 0
if (-not $alreadyPresent) {
    $newRawPath = if ($rawUserPath -eq "") { $installDir } else { $rawUserPath.TrimEnd(';', ' ', "`t") + ";" + $installDir }
    try {
        $regWrite = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
        try { $regWrite.SetValue("Path", $newRawPath, [Microsoft.Win32.RegistryValueKind]::ExpandString) } finally { $regWrite.Close() }
    } catch { throw "Failed to update user PATH in registry: $($_.Exception.Message)" }
    $env:Path = "$env:Path;$installDir"
    Write-Host "Added to user PATH: $installDir"
    Write-Host "Undo: remove '$installDir' from HKCU:\Environment\Path (User variables)."
} else {
    Write-Host "User PATH already contains: $installDir (no change)."
}

Write-Host "Registering local native host..."
& $installedBinary local-init --browser $Browser --extension-id $ExtensionId
if ($LASTEXITCODE -ne 0) {
    throw "Return Bridge local-init failed with exit code $LASTEXITCODE"
}

Write-Host "Registering workspace..."
& $installedBinary target add --target $Workspace --pairing-id local --state-dir $installDir
if ($LASTEXITCODE -ne 0) {
    throw "Failed to register workspace '$Workspace' (exit $LASTEXITCODE)"
}

Write-Host ""
Write-Host "Hands Return Bridge installed."
Write-Host "Workspace: $Workspace"
$reloadUrl = if ($Browser -eq "edge") { "edge://extensions" } else { "chrome://extensions" }
Write-Host "Reload the Hands Return Bridge extension once in $reloadUrl, then use it."
