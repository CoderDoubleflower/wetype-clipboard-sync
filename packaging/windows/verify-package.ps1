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

# Exercise uninstall while the installed worker is running. Its proxy points
# exclusively at loopback, and its temporary identity is never paired.
$InstallDirectory = Join-Path $env:RUNNER_TEMP 'wxclipd-install-test'
$Arguments = @('/SP-', '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=', "/DIR=`"$InstallDirectory`"")
$Process = Start-Process -FilePath $Installer -ArgumentList $Arguments -Wait -PassThru
if ($Process.ExitCode -ne 0) { throw "Installer exit code: $($Process.ExitCode)" }
$InstalledCli = Join-Path $InstallDirectory 'wxclipd.exe'
$InstalledLauncher = Join-Path $InstallDirectory 'wxclipd-background.exe'
Assert-X86 $InstalledCli 3
& $InstalledCli --version
if ($LASTEXITCODE -ne 0) { throw 'Installed CLI did not run' }
$Uninstaller = Join-Path $InstallDirectory 'unins000.exe'
$TestState = Join-Path $env:RUNNER_TEMP "wxclipd-install-state-$([Guid]::NewGuid().ToString('N'))"
$PreviousEnvironment = @{}
foreach ($Key in @('WXCLIPD_STATE', 'WXCLIPD_PROXY', 'WXCLIPD_NAME')) {
    $PreviousEnvironment[$Key] = [Environment]::GetEnvironmentVariable($Key, 'Process')
}
$Daemon = $null
$LauncherProcess = $null
try {
    # The uninstaller inherits this same state directory when it runs `stop`.
    $env:WXCLIPD_STATE = $TestState
    $env:WXCLIPD_PROXY = 'http://127.0.0.1:1'
    $env:WXCLIPD_NAME = 'wxclipd-package-test'
    $LauncherProcess = Start-Process -FilePath $InstalledLauncher -PassThru
    if (-not $LauncherProcess.WaitForExit(10000)) { throw 'Installed background launcher did not exit' }
    if ($LauncherProcess.ExitCode -ne 0) { throw 'Installed background launcher failed' }
    $Deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $Workers = @(Get-Process -Name wxclipd -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -eq $InstalledCli })
        if ($Workers.Count -gt 1) { throw 'Launcher started more than one installed worker' }
        if ($Workers.Count -eq 1) {
            $Daemon = $Workers[0]
            $null = $Daemon.Handle
            if (-not $Daemon.HasExited -and (Test-Path (Join-Path $TestState 'identity.json'))) { break }
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $Deadline)
    if ($null -eq $Daemon -or $Daemon.HasExited -or -not (Test-Path (Join-Path $TestState 'identity.json'))) {
        $Log = Join-Path $TestState 'wxclipd.log'
        if (Test-Path $Log) { Get-Content $Log -Tail 40 | Write-Output }
        throw 'Installed background worker did not initialize with the offline proxy'
    }

    $Process = Start-Process -FilePath $Uninstaller -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') -Wait -PassThru
    if ($Process.ExitCode -ne 0) { throw "Uninstaller exit code: $($Process.ExitCode)" }
    if (-not $Daemon.WaitForExit(10000)) { throw 'Uninstaller left the background worker running' }
    foreach ($Executable in @($InstalledCli, $InstalledLauncher)) {
        if (Test-Path $Executable) { throw "Uninstaller left an executable behind: $Executable" }
    }
} finally {
    # Only processes from this isolated installation are eligible for cleanup.
    foreach ($OwnedProcess in @($Daemon, $LauncherProcess)) {
        if ($null -ne $OwnedProcess) {
            try {
                if (-not $OwnedProcess.HasExited) {
                    $OwnedProcess.Kill()
                    $null = $OwnedProcess.WaitForExit(5000)
                }
            } catch {
                Write-Warning "Could not clean up test process: $_"
            } finally {
                $OwnedProcess.Dispose()
            }
        }
    }
    foreach ($Key in $PreviousEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($Key, $PreviousEnvironment[$Key], 'Process')
    }
    if (Test-Path $TestState) { Remove-Item $TestState -Recurse -Force }
}
Write-Output 'Verified x86 PE32 executables, static runtime dependencies, version, installation, offline background startup and uninstall of a running worker.'
