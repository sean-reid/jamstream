# JamStream installer for Windows. This file lives in site/src/ so mdBook
# copies it verbatim into the built site, which serves it at /install.ps1:
#
#   powershell -ExecutionPolicy Bypass -c "irm https://sean-reid.github.io/jamstream/install.ps1 | iex"
#
# Downloads the jamstream CLI from the latest GitHub release, verifies its
# sha256 against the release's SHA256SUMS file, and installs it. A winget
# package is planned; until it exists, this script and the zips on the
# download page are the install paths.
#
# Parameters (when run as a saved script rather than piped to iex):
#   -WithApp      also install the desktop app zip
#   -WithServer   explain where the server binary comes from on Windows
#   -InstallDir   install here; defaults to JAMSTREAM_INSTALL_DIR if set,
#                 otherwise $env:LOCALAPPDATA\Programs\jamstream

param(
    [switch]$WithApp,
    [switch]$WithServer,
    [string]$InstallDir = $(if ($env:JAMSTREAM_INSTALL_DIR) { $env:JAMSTREAM_INSTALL_DIR }
                           else { Join-Path $env:LOCALAPPDATA 'Programs\jamstream' })
)

$ErrorActionPreference = 'Stop'
# PowerShell 5.1 downloads an order of magnitude slower with the progress
# bar, and its .NET default can predate the TLS that GitHub requires.
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repo = 'sean-reid/jamstream'
$base = "https://github.com/$repo/releases/latest/download"

if ([Environment]::OSVersion.Platform -ne 'Win32NT') {
    Write-Host 'This is the Windows installer; on macOS and Linux run:'
    Write-Host '  curl -fsSL https://sean-reid.github.io/jamstream/install.sh | sh'
    exit 1
}
# Is64BitOperatingSystem is true on ARM64 too, where only emulated x64 would run.
if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    Write-Host 'Releases cover x86_64 only; there is no Windows ARM64 build yet.'
    exit 1
}
if (-not [Environment]::Is64BitOperatingSystem) {
    Write-Host 'Releases cover 64-bit Windows on x86_64 only.'
    exit 1
}
# The same floor the winget manifest declares.
if ([Environment]::OSVersion.Version -lt [Version]'10.0.17763') {
    Write-Host 'JamStream needs Windows 10 version 1809 (build 17763) or newer.'
    exit 1
}

# Expand-Archive -Force cannot overwrite a running exe; it aborts half-extracted.
$running = Get-Process -Name jamstream, jamstream-app, jamstreamd -ErrorAction SilentlyContinue
if ($running) {
    $names = ($running | Select-Object -ExpandProperty ProcessName | Sort-Object -Unique) -join ', '
    Write-Host "close JamStream first: still running: $names"
    exit 1
}

# Returns $true on success, $false on HTTP 404, throws on anything else.
function Get-Asset([string]$Name, [string]$Dest) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$Name" -OutFile $Dest
        return $true
    } catch {
        $status = $null
        if ($_.Exception.Response) {
            $status = [int]$_.Exception.Response.StatusCode
        }
        if ($status -eq 404) { return $false }
        throw
    }
}

function Install-Archive([string]$Asset, [string]$Binary, [string]$SumsPath, [string]$Dir, [string]$Tmp) {
    $zip = Join-Path $Tmp $Asset
    if (-not (Get-Asset $Asset $zip)) {
        throw "The latest release has no asset named $Asset; if a release was just published, its uploads may still be running, so retry in a few minutes."
    }
    $line = Select-String -Path $SumsPath -Pattern ([regex]::Escape($Asset)) | Select-Object -First 1
    if (-not $line) { throw "SHA256SUMS has no entry for $Asset" }
    $expected = ($line.Line -split '\s+')[0]
    $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash
    if ($expected.ToLowerInvariant() -ne $actual.ToLowerInvariant()) {
        throw "Checksum mismatch for ${Asset}: expected $expected, got $actual. Delete the download and retry; if it repeats, report it."
    }
    Expand-Archive -Path $zip -DestinationPath $Dir -Force
    if (-not (Test-Path (Join-Path $Dir $Binary))) {
        throw "the archive $Asset did not contain a $Binary binary"
    }
    Write-Host "installed $Asset into $Dir (sha256 verified)"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("jamstream-install-" + [IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null

Write-Host 'JamStream installer'
Write-Host "install directory: $InstallDir"

try {
    $sums = Join-Path $tmp 'SHA256SUMS'
    if (-not (Get-Asset 'SHA256SUMS' $sums)) {
        Write-Host ''
        Write-Host 'The latest release has no SHA256SUMS; if a release was just published,'
        Write-Host 'its uploads may still be running, so retry in a few minutes.'
        Write-Host 'If no release has been published yet, the repository builds from source'
        Write-Host '(Rust toolchain required):'
        Write-Host "  git clone https://github.com/$repo; cd jamstream"
        Write-Host '  cargo install --path crates/cli'
        exit 1
    }

    Install-Archive 'jamstream-cli-windows-x86_64.zip' 'jamstream.exe' $sums $InstallDir $tmp
    if ($WithApp) {
        Install-Archive 'jamstream-app-windows-x86_64.zip' 'jamstream-app.exe' $sums $InstallDir $tmp
    }
    if ($WithServer) {
        Write-Host 'jamstreamd binaries are published for Linux (musl, x86_64 and aarch64) only.'
        Write-Host 'On Windows, local mode uses the jamstreamd that ships next to the'
        Write-Host 'desktop app, or a from-source build: cargo install --path crates/server'
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$normalized = $InstallDir.TrimEnd('\')
$onPath = (($env:Path -split ';') | ForEach-Object { $_.TrimEnd('\') }) -contains $normalized
if (-not $onPath) {
    try {
        # The USER scope needs no admin rights and survives the session.
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $entries = @()
        if ($userPath) { $entries = @($userPath -split ';' | Where-Object { $_ }) }
        if (-not (($entries | ForEach-Object { $_.TrimEnd('\') }) -contains $normalized)) {
            [Environment]::SetEnvironmentVariable('Path', (($entries + $InstallDir) -join ';'), 'User')
        }
        $env:Path = "$InstallDir;$env:Path"
        Write-Host ''
        Write-Host "added $InstallDir to your user Path; a new terminal picks it up."
    } catch {
        Write-Host ''
        Write-Host "note: could not add $InstallDir to your Path. Add it for the current session with:"
        Write-Host "  `$env:Path = `"$InstallDir;`" + `$env:Path"
        Write-Host 'and permanently in Settings, System, About, Advanced system settings,'
        Write-Host 'Environment Variables, by editing the user Path variable.'
    }
}

Write-Host ''
Write-Host 'Done. Check the install with: jamstream --version'
Write-Host 'The binaries are not code signed yet, so SmartScreen may warn on first run.'
Write-Host 'A winget package is planned.'
