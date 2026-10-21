param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$Dist
)
$ErrorActionPreference = 'Stop'

# These checks run on the Windows CI desktop using an unpaired, isolated state
# and a closed loopback proxy. No phone/account or production network is used.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class WeTypeNativeSmoke {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)]
    private struct NOTIFYICONIDENTIFIER {
        public uint cbSize;
        public IntPtr hWnd;
        public uint uID;
        public Guid guidItem;
    }
    private delegate bool EnumWindowProc(IntPtr window, IntPtr parameter);
    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowProc callback, IntPtr parameter);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetClassNameW(IntPtr window, StringBuilder name, int count);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindWindowW(string className, string title);
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")]
    private static extern bool IsWindowEnabled(IntPtr window);
    [DllImport("user32.dll")]
    public static extern bool PostMessageW(IntPtr window, uint message, UIntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")]
    private static extern IntPtr GetDlgItem(IntPtr window, int id);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr SendMessageTimeoutW(IntPtr window, uint message, UIntPtr wParam,
        StringBuilder text, uint flags, uint timeout, out UIntPtr result);
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr window, out RECT rectangle);
    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
    [DllImport("shell32.dll")]
    private static extern int Shell_NotifyIconGetRect(ref NOTIFYICONIDENTIFIER icon, out RECT rectangle);
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    public static extern uint ExtractIconExW(string path, int index, IntPtr large, IntPtr small, uint count);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr LoadLibraryExW(string path, IntPtr file, uint flags);
    [DllImport("kernel32.dll")]
    private static extern bool FreeLibrary(IntPtr module);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr LoadImageW(IntPtr module, IntPtr name, uint type, int width, int height, uint flags);
    [DllImport("user32.dll")]
    private static extern bool DestroyIcon(IntPtr icon);

    public static IntPtr FindOwnedWindow(int processId, string expectedClass) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((window, parameter) => {
            uint owner;
            GetWindowThreadProcessId(window, out owner);
            if (owner != processId) return true;
            var name = new StringBuilder(256);
            GetClassNameW(window, name, name.Capacity);
            if (name.ToString() != expectedClass) return true;
            found = window;
            return false;
        }, IntPtr.Zero);
        return found;
    }

    public static bool HasTrayIcon(IntPtr window) {
        var icon = new NOTIFYICONIDENTIFIER {
            cbSize = (uint)Marshal.SizeOf(typeof(NOTIFYICONIDENTIFIER)),
            hWnd = window, uID = 1, guidItem = Guid.Empty
        };
        RECT rectangle;
        return Shell_NotifyIconGetRect(ref icon, out rectangle) == 0
            && rectangle.Right > rectangle.Left && rectangle.Bottom > rectangle.Top;
    }

    public static bool HasApplicationIcon(string path) {
        IntPtr module = LoadLibraryExW(path, IntPtr.Zero, 0x22); // datafile + image resource
        if (module == IntPtr.Zero) return false;
        try {
            IntPtr icon = LoadImageW(module, (IntPtr)101, 1, 32, 32, 0);
            if (icon == IntPtr.Zero) return false;
            DestroyIcon(icon);
            return true;
        } finally { FreeLibrary(module); }
    }

    public static string ControlText(IntPtr window, int id) {
        IntPtr control = GetDlgItem(window, id);
        if (control == IntPtr.Zero) throw new Exception("Missing status/configuration control");
        var text = new StringBuilder(16384);
        UIntPtr result;
        if (SendMessageTimeoutW(control, 0xD, (UIntPtr)text.Capacity, text, 2, 2000, out result) == IntPtr.Zero)
            throw new Exception("Status/configuration window is not responding");
        return text.ToString();
    }

    public static bool ControlEnabled(IntPtr window, int id) {
        IntPtr control = GetDlgItem(window, id);
        if (control == IntPtr.Zero) throw new Exception("Missing tray command button");
        return IsWindowEnabled(control);
    }
}
'@

$OwnedProcesses = [Collections.Generic.List[Diagnostics.Process]]::new()
function Start-OwnedProcess([string]$Executable, [string[]]$Arguments = @(), [bool]$Capture = $false) {
    $Info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $Info.UseShellExecute = $false
    $Info.CreateNoWindow = $true
    $Info.RedirectStandardOutput = $Capture
    $Info.RedirectStandardError = $Capture
    foreach ($Argument in $Arguments) { $Info.ArgumentList.Add($Argument) }
    $Process = [Diagnostics.Process]::new()
    $Process.StartInfo = $Info
    $null = $Process.Start()
    $OwnedProcesses.Add($Process)
    return $Process
}

function Invoke-Program([string]$Executable, [string[]]$Arguments = @(), [int]$Timeout = 20000) {
    $Process = Start-OwnedProcess $Executable $Arguments $true
    $OutputTask = $Process.StandardOutput.ReadToEndAsync()
    $ErrorTask = $Process.StandardError.ReadToEndAsync()
    if (-not $Process.WaitForExit($Timeout)) {
        $Process.Kill()
        throw "Program timed out: $Executable"
    }
    $Output = $OutputTask.GetAwaiter().GetResult()
    $ErrorOutput = $ErrorTask.GetAwaiter().GetResult()
    if ($Process.ExitCode -ne 0) {
        $Failure = [InvalidOperationException]::new("Program failed ($($Process.ExitCode)): $Executable $ErrorOutput")
        $Failure.Data['NativeExitCode'] = $Process.ExitCode
        throw $Failure
    }
    return $Output
}

function Wait-Value([scriptblock]$Probe, [string]$Description, [int]$Seconds = 15) {
    $Deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    $script:InspectionTransitionError = $null
    do {
        $Value = & $Probe
        if ($Value) { return $Value }
        Start-Sleep -Milliseconds 150
    } while ([DateTime]::UtcNow -lt $Deadline)
    if ($script:InspectionTransitionError) {
        throw "Timed out: $Description. Last inspection error: $script:InspectionTransitionError"
    }
    throw "Timed out: $Description"
}

function Inspect-Worker([switch]$DuringTransition) {
    try {
        $Output = Invoke-Program $InstalledCli @('inspect') 8000
    } catch {
        # A pipe can close after connection while the observed daemon exits.
        # Retry normal CLI errors only during an expected lifecycle transition.
        # Missing binaries, hung processes, panics and malformed JSON still fail.
        if ($DuringTransition -and $_.Exception.Data['NativeExitCode'] -eq 1) {
            $script:InspectionTransitionError = $_.Exception.Message
            return $null
        }
        throw
    }
    $script:InspectionTransitionError = $null
    return ($Output | ConvertFrom-Json)
}

function Wait-Worker {
    return (Wait-Value {
        $State = Inspect-Worker -DuringTransition
        if ($null -ne $State -and $State.phase -in @('initializing', 'retrying') -and $State.pid -gt 0) { return $State }
    } 'offline worker inspection')
}

function Assert-SecretFree([string]$Text) {
    foreach ($Secret in @('probe-user', 'probe-password', 'saved-user', 'saved-password')) {
        if ($Text.Contains($Secret)) { throw 'Runtime/UI inspection exposed proxy credentials' }
    }
}

function Post-TrayCommand([int]$CommandId) {
    # Match an actual click: a previous launch/action can still own the worker
    # queue after the named pipe becomes available. Busy buttons reject clicks.
    $null = Wait-Value { [WeTypeNativeSmoke]::ControlEnabled($StatusWindow, $CommandId) } "enabled tray command $CommandId"
    if (-not [WeTypeNativeSmoke]::PostMessageW($Controller, 0x111, [UIntPtr]$CommandId, [IntPtr]::Zero)) {
        throw 'Could not deliver the native tray menu command'
    }
}

function Save-StatusScreenshot([IntPtr]$Window) {
    Add-Type -AssemblyName System.Drawing
    $Rect = [WeTypeNativeSmoke+RECT]::new()
    if (-not [WeTypeNativeSmoke]::GetWindowRect($Window, [ref]$Rect)) { throw 'Cannot locate status window bounds' }
    $Bitmap = [Drawing.Bitmap]::new($Rect.Right - $Rect.Left, $Rect.Bottom - $Rect.Top)
    $Graphics = [Drawing.Graphics]::FromImage($Bitmap)
    try {
        $Dc = $Graphics.GetHdc()
        try {
            if (-not [WeTypeNativeSmoke]::PrintWindow($Window, $Dc, 0)) { throw 'Cannot render native status window' }
        } finally { $Graphics.ReleaseHdc($Dc) }
        $Bitmap.Save((Join-Path $Dist 'windows-tray-status.png'), [Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $Graphics.Dispose()
        $Bitmap.Dispose()
    }
}

$Installer = Join-Path $Dist "wetype-clipboard-sync-$Version-windows-x86-setup.exe"
foreach ($Executable in @('wxclipd.exe', 'wxclipd-background.exe')) {
    $Path = Join-Path $Dist "package/$Executable"
    if (-not [WeTypeNativeSmoke]::HasApplicationIcon($Path)) { throw "Missing embedded icon 101: $Executable" }
    $FileVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path).FileVersion
    if ($FileVersion -ne $Version) { throw "Wrong executable version resource: $Executable" }
}
$IconCount = [WeTypeNativeSmoke]::ExtractIconExW($Installer, -1, [IntPtr]::Zero, [IntPtr]::Zero, 0)
if ($IconCount -eq 0 -or $IconCount -eq [uint32]::MaxValue) {
    throw 'Installer has no icon resource'
}

$InstallDirectory = Join-Path $env:RUNNER_TEMP 'wxclipd-install-test'
$InstallArguments = @('/SP-', '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=', "/DIR=$InstallDirectory")
$InstalledCli = Join-Path $InstallDirectory 'wxclipd.exe'
$InstalledTray = Join-Path $InstallDirectory 'wxclipd-background.exe'
$Uninstaller = Join-Path $InstallDirectory 'unins000.exe'
$TestState = Join-Path $env:RUNNER_TEMP "wxclipd-install-state-$([Guid]::NewGuid().ToString('N'))"
$PreviousEnvironment = @{}
foreach ($Key in @('WXCLIPD_STATE', 'WXCLIPD_PROXY', 'WXCLIPD_NAME')) {
    $PreviousEnvironment[$Key] = [Environment]::GetEnvironmentVariable($Key, 'Process')
}

try {
    $env:WXCLIPD_STATE = $TestState
    $env:WXCLIPD_PROXY = 'http://probe-user:probe-password@127.0.0.1:1'
    $env:WXCLIPD_NAME = 'wxclipd-package-test'
    New-Item -ItemType Directory -Force $TestState | Out-Null
    @{
        proxy = 'socks5h://saved-user:saved-password@127.0.0.1:1'
        poll_interval_seconds = 17
        paste_trigger = $false
    } | ConvertTo-Json | Set-Content (Join-Path $TestState 'config.json') -Encoding utf8

    # Verify the previous real release, including its lack of --quit support.
    $OldInstaller = Join-Path $env:RUNNER_TEMP 'wetype-clipboard-sync-0.3.0-windows-x86-setup.exe'
    if (-not (Test-Path $OldInstaller)) {
        Invoke-WebRequest 'https://github.com/CoderDoubleflower/wetype-clipboard-sync/releases/download/v0.3.0/wetype-clipboard-sync-0.3.0-windows-x86-setup.exe' -OutFile $OldInstaller
    }
    if ((Get-FileHash $OldInstaller -Algorithm SHA256).Hash.ToLowerInvariant() -ne 'fde703a92f4a95a9604b13289bade5121807cde800eab546149d577c3796a8a5') {
        throw 'Previous release checksum mismatch'
    }
    $null = Invoke-Program $OldInstaller $InstallArguments 60000
    $null = Invoke-Program $InstalledTray
    $OldDaemon = Wait-Value {
        $Candidates = @(Get-Process -Name wxclipd -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $InstalledCli })
        if ($Candidates.Count -eq 1 -and (Test-Path (Join-Path $TestState 'identity.json'))) { return $Candidates[0] }
    } '0.3 worker startup'
    $null = $OldDaemon.Handle
    $OwnedProcesses.Add($OldDaemon)
    $IdentityHash = (Get-FileHash (Join-Path $TestState 'identity.json') -Algorithm SHA256).Hash
    $ConfigHash = (Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash
    $null = Invoke-Program $Installer $InstallArguments 60000
    if (-not $OldDaemon.WaitForExit(10000)) { throw 'Upgrade left the 0.3 worker running' }
    if ((Get-FileHash (Join-Path $TestState 'identity.json') -Algorithm SHA256).Hash -ne $IdentityHash) { throw 'Upgrade replaced the existing identity' }
    if ((Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash -ne $ConfigHash) { throw 'Upgrade changed the saved configuration' }
    $VersionOutput = Invoke-Program $InstalledCli @('--version')
    if ($VersionOutput -notmatch "wxclipd $([regex]::Escape($Version)) \(windows-x86\)") { throw 'Installed version mismatch' }

    # Hosted Windows runners may start without Explorer. A real shell is needed
    # to prove notification-area registration; do not replace it with a mock.
    if ([WeTypeNativeSmoke]::FindWindowW('Shell_TrayWnd', $null) -eq [IntPtr]::Zero) {
        $null = Start-Process (Join-Path $env:WINDIR 'explorer.exe') -PassThru
        $null = Wait-Value { [WeTypeNativeSmoke]::FindWindowW('Shell_TrayWnd', $null) -ne [IntPtr]::Zero } 'Explorer notification area' 20
    }

    $Tray = Start-OwnedProcess $InstalledTray @('--show')
    $Controller = Wait-Value {
        if ($Tray.HasExited) { throw 'Resident tray exited during startup' }
        $Window = [WeTypeNativeSmoke]::FindOwnedWindow($Tray.Id, 'WeTypeClipboardSyncTray')
        if ($Window -ne [IntPtr]::Zero) { return $Window }
    } 'native tray controller'
    $null = Wait-Value { [WeTypeNativeSmoke]::HasTrayIcon($Controller) } 'visible or overflow notification-area icon'
    $Status = Wait-Worker
    if ($Status.poll_interval_seconds -ne 17 -or $Status.paste_trigger -ne $false) { throw 'Inspect did not report the active configuration' }
    if ($Status.proxy -notmatch '127\.0\.0\.1:1' -or $Status.proxy -notmatch 'http') { throw 'Inspect did not report the effective environment proxy' }
    Assert-SecretFree ($Status | ConvertTo-Json -Depth 8)
    $Daemon = Get-Process -Id $Status.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)

    $null = [WeTypeNativeSmoke]::PostMessageW($Controller, 0x8004, [UIntPtr]::Zero, [IntPtr]::Zero)
    $StatusWindow = Wait-Value {
        $Window = [WeTypeNativeSmoke]::FindOwnedWindow($Tray.Id, 'WeTypeClipboardSyncStatus')
        if ($Window -ne [IntPtr]::Zero -and [WeTypeNativeSmoke]::IsWindowVisible($Window)) { return $Window }
    } 'native status/configuration window'
    $ConfigurationText = Wait-Value {
        $Text = [WeTypeNativeSmoke]::ControlText($StatusWindow, 1102)
        if ($Text -match '运行中的实际配置' -and $Text -match 'http://127\.0\.0\.1:1' -and $Text -match 'socks5h://127\.0\.0\.1:1') { return $Text }
    } 'rendered configuration'
    $StatusText = Wait-Value {
        $Text = [WeTypeNativeSmoke]::ControlText($StatusWindow, 1101)
        if ($Text -match '正在连接服务|正在重试') { return $Text }
    } 'rendered offline connection state'
    Assert-SecretFree "$StatusText`n$ConfigurationText"
    Save-StatusScreenshot $StatusWindow

    # A second launch must activate the existing UI and must not create a
    # second worker or tray. Exercise the normal public executable entry point.
    $null = Invoke-Program $InstalledTray @('--show')
    $null = Wait-Value { [WeTypeNativeSmoke]::IsWindowVisible($StatusWindow) } 'existing status window activation'
    if ((Inspect-Worker).pid -ne $Daemon.Id) { throw 'Duplicate launch replaced/duplicated the running worker' }
    $Trays = @(Get-Process -Name wxclipd-background -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $InstalledTray })
    if ($Trays.Count -ne 1 -or $Trays[0].Id -ne $Tray.Id) { throw 'Duplicate launch created another tray resident' }

    Post-TrayCommand 1003
    $null = Wait-Value { (Inspect-Worker -DuringTransition).phase -eq 'stopped' } 'tray Stop action'
    if (-not $Daemon.WaitForExit(10000)) { throw 'Stop menu action left worker running' }
    if ($Tray.HasExited -or -not [WeTypeNativeSmoke]::HasTrayIcon($Controller)) { throw 'Stopping synchronization removed the tray entry' }
    Post-TrayCommand 1002
    $Restarted = Wait-Worker
    if ($Restarted.pid -eq $Daemon.Id) { throw 'Start menu did not start a fresh worker' }
    $Daemon = Get-Process -Id $Restarted.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)
    Post-TrayCommand 1004
    $Restarted = Wait-Value {
        $Next = Inspect-Worker -DuringTransition
        if ($null -ne $Next -and $Next.pid -ne $Daemon.Id -and $Next.phase -in @('initializing', 'retrying')) { return $Next }
    } 'tray Restart action' 25
    if (-not $Daemon.WaitForExit(10000)) { throw 'Restart left previous worker running' }
    $Daemon = Get-Process -Id $Restarted.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)

    # Reinstall the new release while both UI and worker are active. Then test
    # the public quit command and an uninstall with active processes separately.
    $null = Invoke-Program $Installer $InstallArguments 60000
    if (-not $Tray.WaitForExit(10000)) { throw 'Reinstall left the resident tray running' }
    if ((Inspect-Worker).phase -ne 'stopped') { throw 'Reinstall left a worker running' }
    $Tray = Start-OwnedProcess $InstalledTray
    $Status = Wait-Worker
    $Daemon = Get-Process -Id $Status.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)
    $null = Invoke-Program $InstalledTray @('--quit')
    if (-not $Tray.WaitForExit(10000) -or -not $Daemon.WaitForExit(10000)) { throw '--quit did not stop tray and worker' }

    $Tray = Start-OwnedProcess $InstalledTray
    $Status = Wait-Worker
    $Daemon = Get-Process -Id $Status.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)
    $null = Invoke-Program $Uninstaller @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') 60000
    if (-not $Tray.WaitForExit(10000) -or -not $Daemon.WaitForExit(10000)) { throw 'Uninstall left tray or worker running' }
    foreach ($Path in @($InstalledCli, $InstalledTray)) {
        if (Test-Path $Path) { throw "Uninstall left an executable behind: $Path" }
    }
    if ((Get-FileHash (Join-Path $TestState 'identity.json') -Algorithm SHA256).Hash -ne $IdentityHash) { throw 'Uninstall removed/replaced the retained identity' }
    if ((Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash -ne $ConfigHash) { throw 'Uninstall removed/changed the retained configuration' }
    Write-Output 'Native tray verification passed: icon resources, 0.3 upgrade, configuration redaction, status window, singleton, stop/start/restart, resident reinstall, quit and uninstall.'
} finally {
    # Only processes created or identified within this isolated installation
    # are eligible for cleanup; preserve any other user's app/identity.
    foreach ($Process in $OwnedProcesses) {
        try {
            if (-not $Process.HasExited) {
                $Process.Kill()
                $null = $Process.WaitForExit(5000)
            }
        } catch { Write-Warning "Test process cleanup failed: $_" }
        finally { $Process.Dispose() }
    }
    foreach ($Key in $PreviousEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($Key, $PreviousEnvironment[$Key], 'Process')
    }
    if (Test-Path $TestState) { Remove-Item $TestState -Recurse -Force }
}
