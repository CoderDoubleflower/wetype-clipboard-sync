param([Parameter(Mandatory = $true)][string]$Version)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$Dist = Join-Path $Root 'dist'

function Assert-X86([string]$Path, [int]$Subsystem = 0) {
    $Bytes = [IO.File]::ReadAllBytes($Path)
    if ($Bytes[0] -ne 0x4D -or $Bytes[1] -ne 0x5A) { throw "Not a PE executable: $Path" }
    $Offset = [BitConverter]::ToInt32($Bytes, 0x3C)
    if ([BitConverter]::ToUInt32($Bytes, $Offset) -ne 0x00004550) { throw "Invalid PE signature: $Path" }
    if ([BitConverter]::ToUInt16($Bytes, $Offset + 4) -ne 0x014c) { throw "Executable is not x86: $Path" }
    if ([BitConverter]::ToUInt16($Bytes, $Offset + 24) -ne 0x010b) { throw "Executable is not PE32: $Path" }
    if ($Subsystem -ne 0 -and [BitConverter]::ToUInt16($Bytes, $Offset + 24 + 68) -ne $Subsystem) {
        throw "Wrong Windows subsystem: $Path"
    }
}

$Cli = Join-Path $Dist 'package/wxclipd.exe'
$Launcher = Join-Path $Dist 'package/wxclipd-background.exe'
$Installer = Join-Path $Dist "wetype-clipboard-sync-$Version-windows-x86-setup.exe"
Assert-X86 $Cli 3
Assert-X86 $Launcher 2
Assert-X86 $Installer
$Output = & $Cli --version
if ($LASTEXITCODE -ne 0 -or $Output -notmatch "wxclipd $([regex]::Escape($Version)) \(windows-x86\)") {
    throw "Unexpected version: $Output"
}
foreach ($Exe in @($Cli, $Launcher)) {
    $Dependencies = & dumpbin /dependents $Exe
    if ($LASTEXITCODE -ne 0) { throw 'dumpbin failed' }
    if ($Dependencies -match '(?i)(libcrypto|libssl|vcruntime|msvcp)\S*\.dll') {
        throw "Nonportable runtime dependency in $Exe"
    }
}

# Native UI/lifecycle checks include an upgrade from the published 0.3 package.
& (Join-Path $PSScriptRoot 'verify-tray.ps1') -Version $Version -Dist $Dist
if ($LASTEXITCODE -ne 0) { throw 'Native tray/package verification failed' }
Write-Output 'Verified x86 PE32 executables, static runtime dependencies, version, icon resources, native tray/status UI, offline configuration, upgrade, lifecycle and uninstall.'
