#!/usr/bin/env pwsh
# No Windows exe may import the redistributable CRT.
#
# vcruntime, msvcp and msvcr ship with Visual C++, not with Windows, so an exe
# that imports one dies in the loader on a clean machine before main and before
# any of the first-launch logging can say why (#416). The static CRT link that
# keeps them out lives in .cargo/config.toml, and this is the measurement that
# proves it held: the import table, not an argument about linker semantics.
#
# release.yml ran this at tag time only, so a failure arrived after the tag was
# cut (#433). ci.yml's release-build job runs it too, on every push to main.
#
# Usage: check-windows-crt.ps1 <directory> <exe>... [-Dumpbin <path>]
#
# The whole dependency list is printed, because reading that print is how #416
# was found. Anything that stops the check from looking at a real import table
# is a failure and never a quiet pass.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Directory,

    [Parameter(Mandatory = $true, Position = 1, ValueFromRemainingArguments = $true)]
    [string[]]$Exe,

    # Only the gate's own exercise in ci.yml passes this; it runs on a machine
    # with no Visual Studio to find.
    [string]$Dumpbin
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Dumpbin) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere)) {
        throw "vswhere is not at $vswhere, so no exe had its imports checked"
    }
    $vs = & $vswhere -latest -property installationPath
    $found = Get-ChildItem -Recurse -Filter dumpbin.exe "$vs\VC\Tools\MSVC" |
        Where-Object { $_.FullName -match 'Hostx64\\x64' } | Select-Object -First 1
    if (-not $found) { throw 'dumpbin not found, so no exe had its imports checked' }
    $Dumpbin = $found.FullName
}
"dumpbin: $Dumpbin"

foreach ($name in $Exe) {
    $path = Join-Path $Directory $name
    if (-not (Test-Path -LiteralPath $path)) {
        throw "$path does not exist, so its imports were never checked"
    }
    $imports = & $Dumpbin /nologo /dependents $path
    if ($LASTEXITCODE -ne 0) { throw "dumpbin exited $LASTEXITCODE on $path" }
    $imports
    # A dump with no list in it would pass the assert below on nothing at all.
    if (-not ($imports | Select-String -SimpleMatch 'Image has the following dependencies')) {
        throw "dumpbin printed no dependency list for $path"
    }
    $redist = $imports | Select-String -Pattern '(vcruntime|msvcp|msvcr)\d+.*\.dll'
    if ($redist) {
        throw "$name imports a Visual C++ redistributable DLL, which does not ship with Windows: $(($redist -join '; ').Trim())"
    }
    "$name imports no redistributable CRT"
}
