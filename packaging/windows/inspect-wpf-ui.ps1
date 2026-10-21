param(
    [Parameter(Mandatory = $true)][int]$UiProcessId,
    [ValidateSet('Read', 'Invoke')][string]$Action = 'Read',
    [string]$AutomationId
)
$ErrorActionPreference = 'Stop'
$OutputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $OutputEncoding

# Run with inbox Windows PowerShell 5.1: its UI Automation assemblies can inspect
# a .NET 10 WPF process without loading its desktop runtime into PowerShell 7.
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$Condition = [System.Windows.Automation.PropertyCondition]::new(
    [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $UiProcessId)
$Window = [System.Windows.Automation.AutomationElement]::RootElement.FindFirst(
    [System.Windows.Automation.TreeScope]::Children, $Condition)
if ($null -eq $Window) {
    if ($Action -eq 'Invoke') { throw 'WPF window is not available' }
    @{ ready = $false } | ConvertTo-Json -Compress
    exit 0
}

if ($Action -eq 'Invoke') {
    if (-not $AutomationId) { throw 'An AutomationId is required for Invoke' }
    $ButtonCondition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::AutomationIdProperty, $AutomationId)
    $Button = $Window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $ButtonCondition)
    if ($null -eq $Button -or -not $Button.Current.IsEnabled) { throw "WPF action is unavailable: $AutomationId" }
    $Pattern = $Button.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $Pattern.Invoke()
    @{ invoked = $AutomationId } | ConvertTo-Json -Compress
    exit 0
}

$Elements = $Window.FindAll([System.Windows.Automation.TreeScope]::Descendants,
    [System.Windows.Automation.Condition]::TrueCondition)
$Controls = @{}
$Texts = [Collections.Generic.List[string]]::new()
foreach ($Element in $Elements) {
    $Current = $Element.Current
    $Text = $Current.Name
    $Pattern = $null
    if ($Element.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$Pattern)) {
        $Text = $Pattern.Current.Value
    }
    if ($Current.AutomationId) {
        $Controls[$Current.AutomationId] = @{
            text = $Text
            enabled = $Current.IsEnabled
            offscreen = $Current.IsOffscreen
        }
    }
    if ($Text) { $Texts.Add($Text) }
}
@{
    ready = $true
    process_id = $UiProcessId
    handle = $Window.Current.NativeWindowHandle
    title = $Window.Current.Name
    text = $Texts -join "`n"
    controls = $Controls
} | ConvertTo-Json -Depth 5 -Compress
