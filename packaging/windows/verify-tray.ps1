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
    public static extern bool PostMessageW(IntPtr window, uint message, UIntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "SendMessageTimeoutW")]
    private static extern IntPtr SendThemeMessage(IntPtr window, uint message, UIntPtr wParam,
        string value, uint flags, uint timeout, out UIntPtr result);
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

    public static void NotifyThemeChange() {
        UIntPtr result;
        SendThemeMessage((IntPtr)0xffff, 0x1a, UIntPtr.Zero, "ImmersiveColorSet", 2, 200, out result);
    }

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

    public static string LastTrayQuery { get; private set; } = "No icon query yet";

    public static bool HasTrayIcon(IntPtr window) {
        var icon = new NOTIFYICONIDENTIFIER {
            cbSize = (uint)Marshal.SizeOf(typeof(NOTIFYICONIDENTIFIER)),
            hWnd = window, uID = 1, guidItem = Guid.Empty
        };
        RECT rectangle;
        int result = Shell_NotifyIconGetRect(ref icon, out rectangle);
        LastTrayQuery = String.Format("HRESULT=0x{0:X8}; RECT=({1}, {2}, {3}, {4})",
            result, rectangle.Left, rectangle.Top, rectangle.Right, rectangle.Bottom);
        // Follow SUCCEEDED(hr), as Microsoft's NotificationIcon sample does.
        // A nonzero success status must still provide a real icon rectangle.
        return result >= 0
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

}
'@

$OwnedProcesses = [Collections.Generic.List[Diagnostics.Process]]::new()
function Start-OwnedProcess([string]$Executable, [string[]]$Arguments = @(), [bool]$Capture = $false) {
    $Info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $Info.UseShellExecute = $false
    $Info.CreateNoWindow = $true
    $Info.RedirectStandardOutput = $Capture
    $Info.RedirectStandardError = $Capture
    if ($Capture) {
        $Info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
        $Info.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    }
    foreach ($Argument in $Arguments) { $Info.ArgumentList.Add($Argument) }
    $Process = [Diagnostics.Process]::new()
    $Process.StartInfo = $Info
    $null = $Process.Start()
    $OwnedProcesses.Add($Process)
    return $Process
}

function Invoke-Program([string]$Executable, [string[]]$Arguments = @(), [int]$Timeout = 20000, [switch]$Capture) {
    # GUI installers/launchers may have helper processes. They do not need a
    # captured stdout pipe whose EOF could depend on an inherited child handle.
    $Process = Start-OwnedProcess $Executable $Arguments $Capture.IsPresent
    $Output = ''
    $ErrorOutput = ''
    if ($Capture) {
        $OutputTask = $Process.StandardOutput.ReadToEndAsync()
        $ErrorTask = $Process.StandardError.ReadToEndAsync()
    }
    if (-not $Process.WaitForExit($Timeout)) {
        $Process.Kill($true)
        throw "Program timed out: $Executable"
    }
    if ($Capture) {
        # Process exit does not guarantee EOF if a descendant inherited a pipe.
        # Keep the diagnostics path bounded independently of WaitForExit.
        $Readers = [Threading.Tasks.Task[]]@($OutputTask, $ErrorTask)
        if (-not [Threading.Tasks.Task]::WaitAll($Readers, 5000)) {
            $Process.StandardOutput.Dispose()
            $Process.StandardError.Dispose()
            throw "Program exited but output pipes did not close: $Executable"
        }
        $Output = $OutputTask.GetAwaiter().GetResult()
        $ErrorOutput = $ErrorTask.GetAwaiter().GetResult()
    }
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
        $Output = Invoke-Program $InstalledCli @('inspect') 8000 -Capture
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

function Read-WpfView {
    $Output = Invoke-Program $WindowsPowerShell @('-NoLogo', '-NoProfile', '-NonInteractive', '-STA', '-ExecutionPolicy', 'Bypass',
        '-File', $WpfInspection, '-UiProcessId', "$($WpfProcess.Id)") 10000 -Capture
    return ($Output | ConvertFrom-Json)
}

function Wait-WpfProcess {
    $Process = Wait-Value {
        $Candidates = @(Get-Process -Name wxclipd-ui -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $InstalledUi })
        if ($Candidates.Count -eq 1) { return $Candidates[0] }
    } 'WPF companion process' 25
    $null = $Process.Handle
    $OwnedProcesses.Add($Process)
    return $Process
}

function Invoke-WpfAction([string]$AutomationId) {
    $null = Wait-Value { (Read-WpfView).controls.$AutomationId.enabled } "enabled WPF action $AutomationId"
    $null = Invoke-Program $WindowsPowerShell @('-NoLogo', '-NoProfile', '-NonInteractive', '-STA', '-ExecutionPolicy', 'Bypass',
        '-File', $WpfInspection, '-UiProcessId', "$($WpfProcess.Id)", '-Action', 'Invoke', '-AutomationId', $AutomationId) 10000 -Capture
}

function Toggle-WpfDirection([string]$AutomationId) {
    $null = Wait-Value { (Read-WpfView).controls.$AutomationId.enabled } "enabled direction switch $AutomationId"
    $null = Invoke-Program $WindowsPowerShell @('-NoLogo', '-NoProfile', '-NonInteractive', '-STA', '-ExecutionPolicy', 'Bypass',
        '-File', $WpfInspection, '-UiProcessId', "$($WpfProcess.Id)", '-Action', 'Toggle', '-AutomationId', $AutomationId) 10000 -Capture
}

function Post-TrayCommand([int]$CommandId) {
    # Match an actual click: a previous launch/action can still own the worker
    # queue after the named pipe becomes available. Busy buttons reject clicks.
    $ButtonId = @{1002 = 'StartButton'; 1003 = 'StopButton'; 1004 = 'RestartButton'; 1005 = 'SyncButton'}[$CommandId]
    $null = Wait-Value { (Read-WpfView).controls.$ButtonId.enabled } "enabled tray command $CommandId"
    if (-not [WeTypeNativeSmoke]::PostMessageW($Controller, 0x111, [UIntPtr]$CommandId, [IntPtr]::Zero)) {
        throw 'Could not deliver the native tray menu command'
    }
}

function Save-StatusScreenshot([IntPtr]$Window, [string]$Theme) {
    Add-Type -AssemblyName System.Drawing
    $Rect = [WeTypeNativeSmoke+RECT]::new()
    if (-not [WeTypeNativeSmoke]::GetWindowRect($Window, [ref]$Rect)) { throw 'Cannot locate status window bounds' }
    $Bitmap = [Drawing.Bitmap]::new($Rect.Right - $Rect.Left, $Rect.Bottom - $Rect.Top)
    $Graphics = [Drawing.Graphics]::FromImage($Bitmap)
    try {
        $Dc = $Graphics.GetHdc()
        try {
            if (-not [WeTypeNativeSmoke]::PrintWindow($Window, $Dc, 2)) { throw 'Cannot render WPF status window' }
        } finally { $Graphics.ReleaseHdc($Dc) }
        $Bitmap.Save((Join-Path $Dist "windows-tray-status-$Theme.png"), [Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $Graphics.Dispose()
        $Bitmap.Dispose()
    }
}

$Installer = Join-Path $Dist "wetype-clipboard-sync-$Version-windows-x86-setup.exe"
Write-Host '[tray check] Verifying executable and installer icon/version resources.'
foreach ($Executable in @('wxclipd.exe', 'wxclipd-background.exe')) {
    $Path = Join-Path $Dist "package/$Executable"
    if (-not [WeTypeNativeSmoke]::HasApplicationIcon($Path)) { throw "Missing embedded icon 101: $Executable" }
    $FileVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path).FileVersion
    if ($FileVersion -ne $Version) { throw "Wrong executable version resource: $Executable" }
}
$WpfExecutable = Join-Path $Dist 'package/ui/wxclipd-ui.exe'
$WpfIconCount = [WeTypeNativeSmoke]::ExtractIconExW($WpfExecutable, -1, [IntPtr]::Zero, [IntPtr]::Zero, 0)
if ($WpfIconCount -eq 0 -or $WpfIconCount -eq [uint32]::MaxValue) { throw 'WPF executable has no application icon resource' }
$WpfVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($WpfExecutable).FileVersion
$ExpectedWpfVersion = [regex]::Match($Version, '^\d+\.\d+\.\d+').Value + '.0'
if ($WpfVersion -ne $ExpectedWpfVersion) { throw "Wrong WPF executable version resource: $WpfVersion" }
$IconCount = [WeTypeNativeSmoke]::ExtractIconExW($Installer, -1, [IntPtr]::Zero, [IntPtr]::Zero, 0)
if ($IconCount -eq 0 -or $IconCount -eq [uint32]::MaxValue) {
    throw 'Installer has no icon resource'
}

$InstallDirectory = Join-Path $env:RUNNER_TEMP 'wxclipd-install-test'
$InstallArguments = @('/SP-', '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=', "/DIR=$InstallDirectory")
$InstalledCli = Join-Path $InstallDirectory 'wxclipd.exe'
$InstalledTray = Join-Path $InstallDirectory 'wxclipd-background.exe'
$InstalledUi = Join-Path $InstallDirectory 'ui/wxclipd-ui.exe'
$WindowsPowerShell = Join-Path $env:WINDIR 'System32/WindowsPowerShell/v1.0/powershell.exe'
$WpfInspection = Join-Path $PSScriptRoot 'inspect-wpf-ui.ps1'
$Uninstaller = Join-Path $InstallDirectory 'unins000.exe'
$TestState = Join-Path $env:RUNNER_TEMP "wxclipd-install-state-$([Guid]::NewGuid().ToString('N'))"
$PreviousEnvironment = @{}
foreach ($Key in @('WXCLIPD_STATE', 'WXCLIPD_PROXY', 'WXCLIPD_NAME')) {
    $PreviousEnvironment[$Key] = [Environment]::GetEnvironmentVariable($Key, 'Process')
}
$ThemeKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize'
$PreviousTheme = (Get-ItemProperty -Path $ThemeKey -Name AppsUseLightTheme -ErrorAction SilentlyContinue).AppsUseLightTheme
$ThemeChanged = $false

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

    # Verify an upgrade from the previous real release with its resident tray.
    $OldInstaller = Join-Path $env:RUNNER_TEMP 'wetype-clipboard-sync-0.4.1-windows-x86-setup.exe'
    Write-Host '[tray check] Obtaining the pinned 0.4.1 installer (45 second download deadline).'
    if (-not (Test-Path $OldInstaller)) {
        $Http = [Net.Http.HttpClient]::new()
        $Http.Timeout = [TimeSpan]::FromSeconds(45)
        $Http.MaxResponseContentBufferSize = 16 * 1024 * 1024
        try {
            # GetByteArrayAsync buffers the complete response, so HttpClient's
            # timeout covers both connection and body, including redirects.
            $Download = $Http.GetByteArrayAsync('https://github.com/CoderDoubleflower/wetype-clipboard-sync/releases/download/v0.4.1/wetype-clipboard-sync-0.4.1-windows-x86-setup.exe')
            [IO.File]::WriteAllBytes($OldInstaller, $Download.GetAwaiter().GetResult())
        } finally { $Http.Dispose() }
    }
    if ((Get-FileHash $OldInstaller -Algorithm SHA256).Hash.ToLowerInvariant() -ne 'd725dfc4525ffdb446a673b0337ca5de5808f0460551800beedd735e6b096423') {
        throw 'Previous release checksum mismatch'
    }
    Write-Host '[tray check] Installing 0.4.1 and starting its resident tray and offline worker.'
    $null = Invoke-Program $OldInstaller $InstallArguments 60000
    $OldTray = Start-OwnedProcess $InstalledTray
    $OldDaemon = Wait-Value {
        $Candidates = @(Get-Process -Name wxclipd -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $InstalledCli })
        if ($Candidates.Count -eq 1 -and (Test-Path (Join-Path $TestState 'identity.json'))) { return $Candidates[0] }
    } '0.4.1 worker startup'
    $null = $OldDaemon.Handle
    $OwnedProcesses.Add($OldDaemon)
    $IdentityHash = (Get-FileHash (Join-Path $TestState 'identity.json') -Algorithm SHA256).Hash
    $ConfigHash = (Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash
    Write-Host '[tray check] Upgrading the running 0.4.1 installation and checking retained identity/configuration.'
    $null = Invoke-Program $Installer $InstallArguments 60000
    if (-not $OldTray.WaitForExit(10000) -or -not $OldDaemon.WaitForExit(10000)) { throw 'Upgrade left the 0.4.1 tray or worker running' }
    if ((Get-FileHash (Join-Path $TestState 'identity.json') -Algorithm SHA256).Hash -ne $IdentityHash) { throw 'Upgrade replaced the existing identity' }
    if ((Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash -ne $ConfigHash) { throw 'Upgrade changed the saved configuration' }
    $VersionOutput = Invoke-Program $InstalledCli @('--version') -Capture
    if ($VersionOutput -notmatch "wxclipd $([regex]::Escape($Version)) \(windows-x86\)") { throw 'Installed version mismatch' }

    # Hosted Windows runners may start without Explorer. A real shell is needed
    # to prove notification-area registration; do not replace it with a mock.
    Write-Host '[tray check] Ensuring the native Explorer notification area is available.'
    if ([WeTypeNativeSmoke]::FindWindowW('Shell_TrayWnd', $null) -eq [IntPtr]::Zero) {
        $null = Start-Process (Join-Path $env:WINDIR 'explorer.exe') -PassThru
        $null = Wait-Value { [WeTypeNativeSmoke]::FindWindowW('Shell_TrayWnd', $null) -ne [IntPtr]::Zero } 'Explorer notification area' 20
    }

    Write-Host '[tray check] Starting resident tray; checking notification icon and live offline configuration.'
    $Tray = Start-OwnedProcess $InstalledTray @('--show')
    $Controller = Wait-Value {
        if ($Tray.HasExited) { throw 'Resident tray exited during startup' }
        $Window = [WeTypeNativeSmoke]::FindOwnedWindow($Tray.Id, 'WeTypeClipboardSyncTray')
        if ($Window -ne [IntPtr]::Zero) { return $Window }
    } 'native tray controller'
    try {
        $null = Wait-Value { [WeTypeNativeSmoke]::HasTrayIcon($Controller) } 'visible or overflow notification-area icon'
    } catch {
        throw "Notification-area verification failed: $([WeTypeNativeSmoke]::LastTrayQuery). $_"
    }
    Write-Host "[tray check] Native icon query: $([WeTypeNativeSmoke]::LastTrayQuery)"
    $Status = Wait-Worker
    if ($Status.poll_interval_seconds -ne 17 -or $Status.paste_trigger -ne $false) { throw 'Inspect did not report the active configuration' }
    if ($Status.proxy -notmatch '127\.0\.0\.1:1' -or $Status.proxy -notmatch 'http') { throw 'Inspect did not report the effective environment proxy' }
    Assert-SecretFree ($Status | ConvertTo-Json -Depth 8)
    $Daemon = Get-Process -Id $Status.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)

    Write-Host '[tray check] Inspecting the actual WPF controls and checking credential redaction.'
    $null = [WeTypeNativeSmoke]::PostMessageW($Controller, 0x8004, [UIntPtr]::Zero, [IntPtr]::Zero)
    $WpfProcess = Wait-WpfProcess
    $View = Wait-Value {
        $Current = Read-WpfView
        if ($Current.ready -and $Current.title -match 'WeType' -and $Current.handle -ne 0) { return $Current }
    } 'WPF status/configuration window'
    $StatusWindow = [IntPtr]$View.handle
    $View = Wait-Value {
        $Current = Read-WpfView
        if ($Current.text -match 'http://127\.0\.0\.1:1' -and $Current.text -match 'socks5h://127\.0\.0\.1:1' -and
            $Current.controls.PhaseLabel.text -match '正在连接服务|正在重试') { return $Current }
    } 'rendered WPF status and effective/saved proxy configuration'
    Assert-SecretFree $View.text

    Write-Host '[tray check] Exercising both direction switches through UI Automation.'
    if ($Status.send_enabled -ne $true -or $Status.receive_enabled -ne $true) { throw 'Old configuration did not default to bidirectional sync' }
    foreach ($Mode in @(
        @{ control = 'SendSwitch'; send = $false; receive = $true },
        @{ control = 'ReceiveSwitch'; send = $false; receive = $false },
        @{ control = 'SendSwitch'; send = $true; receive = $false },
        @{ control = 'ReceiveSwitch'; send = $true; receive = $true }
    )) {
        Toggle-WpfDirection $Mode.control
        $Next = Wait-Value {
            $Current = Inspect-Worker -DuringTransition
            if ($null -ne $Current -and $Current.pid -gt 0 -and $Current.pid -ne $Daemon.Id -and
                $Current.send_enabled -eq $Mode.send -and $Current.receive_enabled -eq $Mode.receive) { return $Current }
        } 'direction change persisted and loaded into a new worker' 25
        if (-not $Daemon.WaitForExit(10000)) { throw 'Direction change left the old worker running' }
        $Daemon = Get-Process -Id $Next.pid
        $null = $Daemon.Handle
        $OwnedProcesses.Add($Daemon)
        $Saved = Get-Content (Join-Path $TestState 'config.json') -Raw | ConvertFrom-Json
        if ($Saved.send_enabled -ne $Mode.send -or $Saved.receive_enabled -ne $Mode.receive -or
            $Saved.proxy -ne 'socks5h://saved-user:saved-password@127.0.0.1:1' -or
            $Saved.poll_interval_seconds -ne 17 -or $Saved.paste_trigger -ne $false) { throw 'Direction update lost another setting' }
        $null = Wait-Value {
            $Current = Read-WpfView
            $Current.controls.SendSwitch.toggle_state -eq [int]$Mode.send -and
                $Current.controls.ReceiveSwitch.toggle_state -eq [int]$Mode.receive -and
                $Current.controls.SendSwitch.enabled
        } 'switch state reflects the saved configuration'
        if ($WpfProcess.HasExited -or $Tray.HasExited) { throw 'Direction change replaced the UI or tray' }
    }
    if ((Get-FileHash (Join-Path $TestState 'identity.json') -Algorithm SHA256).Hash -ne $IdentityHash) { throw 'Direction changes replaced pairing identity' }
    $ConfigHash = (Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash

    Write-Host '[tray check] Changing the Windows application theme while the same WPF window stays open.'
    if (-not (Test-Path $ThemeKey)) { New-Item -Path $ThemeKey -Force | Out-Null }
    foreach ($Theme in @(@{ value = 1; name = 'light'; label = '浅色' }, @{ value = 0; name = 'dark'; label = '深色' })) {
        $ThemeChanged = $true
        Set-ItemProperty -Path $ThemeKey -Name AppsUseLightTheme -Value $Theme.value -Type DWord
        [WeTypeNativeSmoke]::NotifyThemeChange()
        $null = Wait-Value {
            $Current = Read-WpfView
            $Current.controls.ThemeLabel.text -eq "跟随系统 · $($Theme.label)"
        } "live Windows $($Theme.name) theme adaptation"
        if ($WpfProcess.HasExited) { throw 'Theme switch restarted/exited the WPF process' }
        Save-StatusScreenshot $StatusWindow $Theme.name
    }

    Write-Host '[tray check] Hiding the WPF window while synchronization and the tray remain active.'
    Invoke-WpfAction 'HideButton'
    $null = Wait-Value { -not [WeTypeNativeSmoke]::IsWindowVisible($StatusWindow) } 'WPF hide action'
    if ($Tray.HasExited -or (Inspect-Worker).pid -ne $Daemon.Id) { throw 'Hiding the WPF window stopped the tray or worker' }

    # A second launch must activate the existing UI and must not create a
    # second worker or tray. Exercise the normal public executable entry point.
    Write-Host '[tray check] Checking duplicate launch activates the existing singleton.'
    $null = Invoke-Program $InstalledTray @('--show')
    $null = Wait-Value { [WeTypeNativeSmoke]::IsWindowVisible($StatusWindow) } 'existing status window activation'
    if ((Inspect-Worker).pid -ne $Daemon.Id) { throw 'Duplicate launch replaced/duplicated the running worker' }
    $Trays = @(Get-Process -Name wxclipd-background -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $InstalledTray })
    if ($Trays.Count -ne 1 -or $Trays[0].Id -ne $Tray.Id) { throw 'Duplicate launch created another tray resident' }
    $Windows = @(Get-Process -Name wxclipd-ui -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $InstalledUi })
    if ($Windows.Count -ne 1 -or $Windows[0].Id -ne $WpfProcess.Id) { throw 'Duplicate launch created another WPF process' }

    Write-Host '[tray check] Invoking WPF Stop and Start buttons, then the native tray Restart command.'
    Invoke-WpfAction 'StopButton'
    $null = Wait-Value { (Inspect-Worker -DuringTransition).phase -eq 'stopped' } 'tray Stop action'
    if (-not $Daemon.WaitForExit(10000)) { throw 'Stop menu action left worker running' }
    if ($Tray.HasExited -or -not [WeTypeNativeSmoke]::HasTrayIcon($Controller)) { throw 'Stopping synchronization removed the tray entry' }
    Toggle-WpfDirection 'SendSwitch'
    $null = Wait-Value {
        $Saved = Get-Content (Join-Path $TestState 'config.json') -Raw | ConvertFrom-Json
        $Saved.send_enabled -eq $false -and (Read-WpfView).controls.SendSwitch.enabled
    } 'direction saved while stopped'
    if ((Inspect-Worker).phase -ne 'stopped') { throw 'Changing direction unexpectedly started synchronization' }
    Toggle-WpfDirection 'SendSwitch'
    $null = Wait-Value {
        $Saved = Get-Content (Join-Path $TestState 'config.json') -Raw | ConvertFrom-Json
        $Saved.send_enabled -eq $true -and (Read-WpfView).controls.SendSwitch.enabled
    } 'stopped direction restored'
    $ConfigHash = (Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash
    Invoke-WpfAction 'StartButton'
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
    Write-Host '[tray check] Reinstalling while the new resident tray and worker are running.'
    $null = Invoke-Program $Installer $InstallArguments 60000
    if (-not $Tray.WaitForExit(10000) -or -not $WpfProcess.WaitForExit(10000)) { throw 'Reinstall left the tray or WPF window process running' }
    if ((Inspect-Worker).phase -ne 'stopped') { throw 'Reinstall left a worker running' }
    Write-Host '[tray check] Verifying the public --quit command stops tray, WPF and worker.'
    $Tray = Start-OwnedProcess $InstalledTray @('--show')
    $WpfProcess = Wait-WpfProcess
    $Status = Wait-Worker
    $Daemon = Get-Process -Id $Status.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)
    $null = Invoke-Program $InstalledTray @('--quit')
    if (-not $Tray.WaitForExit(10000) -or -not $Daemon.WaitForExit(10000) -or -not $WpfProcess.WaitForExit(10000)) { throw '--quit did not stop tray, WPF and worker' }

    Write-Host '[tray check] Uninstalling while tray, WPF and worker are running; checking identity/configuration retention.'
    $Tray = Start-OwnedProcess $InstalledTray @('--show')
    $WpfProcess = Wait-WpfProcess
    $Status = Wait-Worker
    $Daemon = Get-Process -Id $Status.pid
    $null = $Daemon.Handle
    $OwnedProcesses.Add($Daemon)
    $null = Invoke-Program $Uninstaller @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') 60000
    if (-not $Tray.WaitForExit(10000) -or -not $Daemon.WaitForExit(10000) -or -not $WpfProcess.WaitForExit(10000)) { throw 'Uninstall left tray, WPF or worker running' }
    foreach ($Path in @($InstalledCli, $InstalledTray, $InstalledUi)) {
        if (Test-Path $Path) { throw "Uninstall left an executable behind: $Path" }
    }
    if ((Get-FileHash (Join-Path $TestState 'identity.json') -Algorithm SHA256).Hash -ne $IdentityHash) { throw 'Uninstall removed/replaced the retained identity' }
    if ((Get-FileHash (Join-Path $TestState 'config.json') -Algorithm SHA256).Hash -ne $ConfigHash) { throw 'Uninstall removed/changed the retained configuration' }
    Write-Output 'Tray/WPF verification passed: icon resources, 0.4.1 upgrade, configuration redaction, live system theme changes, UI Automation commands and four direction modes, settings persistence, singleton, stop/start/restart, resident reinstall, quit and uninstall.'
} finally {
    Write-Host '[tray check] Cleaning up isolated test processes and restoring environment.'
    # Only processes created or identified within this isolated installation
    # are eligible for cleanup; preserve any other user's app/identity.
    foreach ($Process in $OwnedProcesses) {
        try {
            if (-not $Process.HasExited) {
                $Process.Kill($true)
                $null = $Process.WaitForExit(5000)
            }
        } catch { Write-Warning "Test process cleanup failed: $_" }
        finally { $Process.Dispose() }
    }
    if ($ThemeChanged) {
        if ($null -eq $PreviousTheme) {
            Remove-ItemProperty -Path $ThemeKey -Name AppsUseLightTheme -ErrorAction SilentlyContinue
        } else {
            Set-ItemProperty -Path $ThemeKey -Name AppsUseLightTheme -Value $PreviousTheme -Type DWord
        }
        [WeTypeNativeSmoke]::NotifyThemeChange()
    }
    foreach ($Key in $PreviousEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($Key, $PreviousEnvironment[$Key], 'Process')
    }
    if (Test-Path $TestState) { Remove-Item $TestState -Recurse -Force }
}
