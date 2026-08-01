# JamStream uninstaller for Windows, the pair of install.ps1. Served by the
# site at /uninstall.ps1 the same way:
#
#   powershell -ExecutionPolicy Bypass -c "irm https://sean-reid.github.io/jamstream/uninstall.ps1 | iex"
#
# Removes what install.ps1 installed and nothing else: the JamStream
# binaries from the install directory, and the directory itself once it is
# empty. Session data and credentials are kept unless asked for, because
# they are what let a reinstall find a session that is still running
# somewhere.
#
# Parameters (when run as a saved script rather than piped to iex):
#   -Purge        also delete the JamStream data directory
#   -Yes          do not stop for a session that is still running
#   -InstallDir   look here; defaults to JAMSTREAM_INSTALL_DIR if set,
#                 otherwise $env:LOCALAPPDATA\Programs\jamstream

param(
    [switch]$Purge,
    [switch]$Yes,
    [string]$InstallDir = $(if ($env:JAMSTREAM_INSTALL_DIR) { $env:JAMSTREAM_INSTALL_DIR }
                           else { Join-Path $env:LOCALAPPDATA 'Programs\jamstream' })
)

$ErrorActionPreference = 'Stop'

if ([Environment]::OSVersion.Platform -ne 'Win32NT') {
    Write-Host 'This is the Windows uninstaller; on macOS and Linux run:'
    Write-Host '  curl -fsSL https://sean-reid.github.io/jamstream/uninstall.sh | sh'
    exit 1
}

# Exactly the files the two install archives contain. Anything else in the
# directory is not ours to delete. jamstream.exe goes last: it is the recovery
# tool the error guidance leans on, so a lock elsewhere must not orphan things
# with the CLI already gone.
$owned = @('jamstream-app.exe', 'jamstreamd.exe', 'jamstream.ico', 'jamstream.exe')

$cli = Join-Path $InstallDir 'jamstream.exe'
if (Test-Path $cli) {
    # A session that is still running keeps costing money or holding a port
    # after the binary that can end it is gone. The JSON is pretty-printed,
    # so the match tolerates whitespace.
    $status = & $cli status --json 2>$null
    if ($status -match '"status":\s*"running"') {
        Write-Host 'a session is still running (jamstream status):'
        & $cli status 2>$null
        if (-not $Yes) {
            Write-Host 'end it first (jamstream end --last), sweep strays (jamstream sweep),'
            Write-Host 'or rerun with -Yes to remove the binary anyway.'
            exit 1
        }
        Write-Host 'continuing anyway (-Yes); the session keeps running until its own timers end it'
    }
}

# A running exe cannot be deleted; Remove-Item would die partway through the loop.
$running = Get-Process -Name jamstream-app, jamstreamd -ErrorAction SilentlyContinue
if ($running) {
    $names = ($running | Select-Object -ExpandProperty ProcessName | Sort-Object -Unique) -join ', '
    Write-Host "close JamStream first: still running: $names"
    exit 1
}

$removed = 0
foreach ($name in $owned) {
    $file = Join-Path $InstallDir $name
    if (Test-Path $file) {
        Remove-Item -Force $file
        Write-Host "removed $file"
        $removed++
    }
}

if ($removed -eq 0) {
    Write-Host "nothing to remove: no JamStream binaries in $InstallDir"
} elseif ((Test-Path $InstallDir) -and -not (Get-ChildItem -Force $InstallDir)) {
    Remove-Item -Force $InstallDir
    Write-Host "removed the empty $InstallDir"
    Write-Host 'If you added it to your Path, remove it there too: Settings, System, About,'
    Write-Host 'Advanced system settings, Environment Variables, user Path.'
}

# Data: session records under the local app data directory. Credentials are
# in Windows Credential Manager, which this script does not reach into.
$dataDir = Join-Path $env:LOCALAPPDATA 'jamstream'
if ($Purge) {
    if (Test-Path $dataDir) {
        Remove-Item -Recurse -Force $dataDir
        Write-Host "removed $dataDir"
    } else {
        Write-Host "no data directory at $dataDir"
    }
} elseif (Test-Path $dataDir) {
    Write-Host "kept session data at $dataDir (rerun with -Purge to delete it)"
}

Write-Host 'Cloud credentials, if you saved any, are in Credential Manager: search for jamstream and remove the entries.'
