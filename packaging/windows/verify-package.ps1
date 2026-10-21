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
$WpfUi = Join-Path $Dist 'package/ui/wxclipd-ui.exe'
$Installer = Join-Path $Dist "wetype-clipboard-sync-$Version-windows-x86-setup.exe"
Assert-X86 $Cli 3
Assert-X86 $Launcher 2
Assert-X86 $WpfUi 2
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

$UiDirectory = Split-Path $WpfUi
foreach ($File in @('coreclr.dll', 'hostfxr.dll', 'hostpolicy.dll', 'PresentationFramework.dll', 'PresentationCore.dll', 'WindowsBase.dll')) {
    if (-not (Test-Path (Join-Path $UiDirectory $File) -PathType Leaf)) { throw "Missing bundled desktop runtime file: $File" }
}
Assert-X86 (Join-Path $UiDirectory 'coreclr.dll')
Assert-X86 (Join-Path $UiDirectory 'hostfxr.dll')
$RuntimeConfig = Get-Content (Join-Path $UiDirectory 'wxclipd-ui.runtimeconfig.json') -Raw | ConvertFrom-Json
$Frameworks = @($RuntimeConfig.runtimeOptions.includedFrameworks)
if ($Frameworks.Count -ne 2 -or $Frameworks.name -notcontains 'Microsoft.NETCore.App' -or $Frameworks.name -notcontains 'Microsoft.WindowsDesktop.App') {
    throw 'WPF publication is not self-contained with the Windows desktop runtime'
}

# Exercise the exact packaged WPF application and its real XAML resources.
# The capture command returns only after rendering both themes and checking
# layout, rendered button text contrast and theme state; no installed desktop
# runtime is needed.
$CaptureDirectory = Join-Path $Dist 'wpf-verification'
$CaptureInfo = [Diagnostics.ProcessStartInfo]::new($WpfUi)
$CaptureInfo.UseShellExecute = $false
$CaptureInfo.ArgumentList.Add('--capture-dir')
$CaptureInfo.ArgumentList.Add($CaptureDirectory)
$CaptureInfo.Environment['DOTNET_ROOT'] = Join-Path $env:RUNNER_TEMP 'no-external-dotnet-runtime'
$CaptureInfo.Environment['DOTNET_MULTILEVEL_LOOKUP'] = '0'
$Capture = [Diagnostics.Process]::Start($CaptureInfo)
try {
    if (-not $Capture.WaitForExit(60000)) {
        $Capture.Kill($true)
        throw 'WPF light/dark rendering verification timed out'
    }
    if ($Capture.ExitCode -ne 0) { throw "WPF rendering verification failed: $($Capture.ExitCode)" }
} finally { $Capture.Dispose() }
$CaptureResult = Get-Content (Join-Path $CaptureDirectory 'capture-result.json') -Raw | ConvertFrom-Json
if ($CaptureResult.passed -ne $true) { throw 'WPF capture checks did not pass' }
if ($CaptureResult.light.background -eq $CaptureResult.dark.background) { throw 'WPF light/dark surfaces are identical' }
foreach ($Theme in @('light', 'dark')) {
    $Result = $CaptureResult.$Theme
    if ($Result.width -lt 600 -or $Result.height -lt 500) { throw "Invalid WPF $Theme layout dimensions" }
    $Image = Join-Path $CaptureDirectory "$Theme.png"
    if (-not (Test-Path $Image) -or (Get-Item $Image).Length -lt 1024) { throw "Missing WPF $Theme render" }
}

# Native UI/lifecycle checks include an upgrade from the published 0.4.1 package.
& (Join-Path $PSScriptRoot 'verify-tray.ps1') -Version $Version -Dist $Dist
if ($LASTEXITCODE -ne 0) { throw 'Native tray/package verification failed' }
Write-Output 'Verified x86 PE32 executables, bundled WPF runtime, real light/dark XAML renders, version, icon resources, tray/WPF controls, live system theme changes, configuration, upgrade, lifecycle and uninstall.'
