# Use Pod provider-agent installer (Windows / PowerShell).
#
# Usage:
#   irm https://usepod.ai/install.ps1 | iex
#
# Optional environment overrides:
#   $env:USEPOD_VERSION   pin a specific release tag (default: latest)
#   $env:USEPOD_BASE_URL  base URL for the version pointer (default: https://usepod.ai)
#   $env:USEPOD_REPO      GitHub releases repo (default: Sortis-AI/usepod-agent)

#Requires -Version 5.1

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# --- Configuration --------------------------------------------------------

$FallbackVersion = 'v0.3.3'
$BaseUrl = if ($env:USEPOD_BASE_URL) { $env:USEPOD_BASE_URL } else { 'https://usepod.ai' }
$Repo    = if ($env:USEPOD_REPO)     { $env:USEPOD_REPO }     else { 'Sortis-AI/usepod-agent' }

# --- Architecture detection ----------------------------------------------

$Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($Arch) {
    'X64'   { $Asset = 'usepod-agent-windows-x64.exe' }
    'Arm64' { $Asset = 'usepod-agent-windows-arm64.exe' }
    default { throw "Unsupported Windows architecture: $Arch" }
}

# --- Resolve version ------------------------------------------------------

if ($env:USEPOD_VERSION) {
    $Version = $env:USEPOD_VERSION
    Write-Host "usepod-agent installer: using pinned version: $Version"
} else {
    Write-Host "usepod-agent installer: fetching latest version from $BaseUrl/agent-latest"
    try {
        $Version = (Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/agent-latest").Content.Trim()
    } catch {
        $Version = $null
    }
    if ([string]::IsNullOrWhiteSpace($Version)) {
        Write-Host "usepod-agent installer: could not resolve latest version; falling back to $FallbackVersion"
        $Version = $FallbackVersion
    }
    $ResolvedVersion = if ($Version.StartsWith('v')) { $Version.Substring(1) } else { $Version }
    $DefaultVersion = if ($FallbackVersion.StartsWith('v')) { $FallbackVersion.Substring(1) } else { $FallbackVersion }
    if ([version]$ResolvedVersion -lt [version]$DefaultVersion) {
        Write-Host "usepod-agent installer: latest version $Version is older than bundled default $FallbackVersion; using $FallbackVersion"
        $Version = $FallbackVersion
    }
}
if (-not $Version.StartsWith('v')) { $Version = "v$Version" }

$ReleaseUrl = "https://github.com/$Repo/releases/download/$Version"

# --- Download + verify ----------------------------------------------------

$WorkDir = Join-Path $env:TEMP ("usepod-agent-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $WorkDir | Out-Null

try {
    $BinPath = Join-Path $WorkDir $Asset
    $SumPath = "$BinPath.sha256"

    Write-Host "usepod-agent installer: downloading $Asset ($Version)"
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseUrl/$Asset"        -OutFile $BinPath
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseUrl/$Asset.sha256" -OutFile $SumPath

    Write-Host "usepod-agent installer: verifying SHA-256"
    $expectedLine = (Get-Content -LiteralPath $SumPath -First 1).Trim()
    if (-not $expectedLine) { throw "checksum file is empty" }
    $expected = ($expectedLine -split '\s+')[0].ToLowerInvariant()
    $actual   = (Get-FileHash -Algorithm SHA256 -LiteralPath $BinPath).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "checksum verification failed: expected $expected, got $actual"
    }

    # --- Install --------------------------------------------------------------

    $InstallDir = Join-Path $env:ProgramFiles 'usepod'
    $TargetPath = Join-Path $InstallDir 'usepod-agent.exe'

    if (-not (Test-Path -LiteralPath $InstallDir)) {
        try {
            New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        } catch {
            # Fall back to per-user install if ProgramFiles is not writable.
            $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\usepod'
            $TargetPath = Join-Path $InstallDir 'usepod-agent.exe'
            New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
            Write-Host "usepod-agent installer: ProgramFiles not writable; installing to $InstallDir"
        }
    }

    Copy-Item -LiteralPath $BinPath -Destination $TargetPath -Force

    # --- PATH (user scope; no admin elevation required) -------------------
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    $entries = $userPath -split ';' | Where-Object { $_ -ne '' }
    if ($entries -notcontains $InstallDir) {
        $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "usepod-agent installer: added $InstallDir to user PATH (open a new shell to pick it up)"
    }

    Write-Host "Installed usepod-agent $Version to $TargetPath"
    Write-Host 'Run: usepod-agent --help'
}
finally {
    Remove-Item -Recurse -Force -LiteralPath $WorkDir -ErrorAction SilentlyContinue
}
