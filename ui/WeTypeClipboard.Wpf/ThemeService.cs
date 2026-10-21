using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using Microsoft.Win32;

namespace WeTypeClipboard.Wpf;

/// <summary>
/// WPF's built-in Fluent ThemeMode follows Windows. Our branded surfaces use
/// the same preference, with dynamic resources and full high-contrast colors.
/// </summary>
internal sealed class ThemeService : IDisposable
{
    private readonly Application _application;
    private HwndSource? _source;
    private bool? _previewDark;
    private bool _disposed;
    public event Action? Changed;
    public bool IsDark { get; private set; }
    public bool IsHighContrast { get; private set; }
    public string Label => $"跟随系统 · {(IsHighContrast ? "高对比度" : IsDark ? "深色" : "浅色")}";

    internal ThemeService(Application application)
    {
        _application = application;
        Apply();
        SystemEvents.UserPreferenceChanged += OnPreferenceChanged;
        SystemParameters.StaticPropertyChanged += OnSystemParameterChanged;
    }

    internal void AttachWindow(IntPtr handle)
    {
        _source = HwndSource.FromHwnd(handle);
        _source?.AddHook(WindowMessages);
        ApplyTitleBar();
    }

    private static bool ReadDarkPreference()
    {
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
            return key?.GetValue("AppsUseLightTheme") is int value && value == 0;
        }
        catch (Exception error) when (error is System.Security.SecurityException or UnauthorizedAccessException or System.IO.IOException)
        {
            return false;
        }
    }

    private void OnPreferenceChanged(object sender, UserPreferenceChangedEventArgs e) => ScheduleApply();
    private void OnSystemParameterChanged(object? sender, PropertyChangedEventArgs e) => ScheduleApply();

    private IntPtr WindowMessages(IntPtr hwnd, int message, IntPtr wparam, IntPtr lparam, ref bool handled)
    {
        if (message is 0x001A or 0x031A or 0x0015) // WM_SETTINGCHANGE / WM_THEMECHANGED / WM_SYSCOLORCHANGE
            ScheduleApply();
        return IntPtr.Zero;
    }

    private void ScheduleApply()
    {
        if (!_disposed && !_application.Dispatcher.HasShutdownStarted)
            _application.Dispatcher.BeginInvoke(Apply);
    }

    private void Apply()
    {
        if (_disposed)
            return;
        IsHighContrast = _previewDark is null && SystemParameters.HighContrast;
        IsDark = _previewDark ?? ReadDarkPreference();
        var colors = IsHighContrast ? HighContrastColors() : IsDark ? DarkColors : LightColors;
        foreach (var (name, value) in colors)
        {
            var color = (Color)ColorConverter.ConvertFromString(value);
            var brush = new SolidColorBrush(color);
            brush.Freeze();
            _application.Resources[name] = brush;
        }
        ApplyTitleBar();
        Changed?.Invoke();
    }

    private void ApplyTitleBar()
    {
        if (_source is null)
            return;
        var dark = IsDark && !IsHighContrast ? 1 : 0;
        // Windows 10 versions predating this DWM attribute return an error;
        // their native caption still remains fully usable.
        if (DwmSetWindowAttribute(_source.Handle, 20, ref dark, sizeof(int)) != 0)
            _ = DwmSetWindowAttribute(_source.Handle, 19, ref dark, sizeof(int));
    }

    // Only the standalone render verifier forces a mode. Normal application
    // operation always retains ThemeMode="System" and observes OS changes.
    internal void SetCaptureTheme(bool? dark)
    {
        _previewDark = dark;
#pragma warning disable WPF0001 // Documented WPF API; isolated to screenshot verification.
        _application.ThemeMode = dark switch
        {
            true => ThemeMode.Dark,
            false => ThemeMode.Light,
            null => ThemeMode.System,
        };
#pragma warning restore WPF0001
        Apply();
    }

    internal string BackgroundColor => ((SolidColorBrush)_application.Resources["WindowBrush"]).Color.ToString();

    private static readonly IReadOnlyDictionary<string, string> LightColors = new Dictionary<string, string>
    {
        ["WindowBrush"] = "#F5F6F8", ["SurfaceBrush"] = "#FFFFFF", ["SubtleBrush"] = "#ECEFF3",
        ["BorderBrush"] = "#E0E5EB", ["TextBrush"] = "#1B2028", ["SecondaryTextBrush"] = "#626D79",
        ["HoverBrush"] = "#F0F3F6", ["PressedBrush"] = "#E5EAF0",
        ["AccentBrush"] = "#087C4B", ["AccentTextBrush"] = "#FFFFFF", ["AccentHoverBrush"] = "#08683F",
        ["AccentPressedBrush"] = "#075431", ["AccentSubtleBrush"] = "#E7F5ED",
        ["WarningBrush"] = "#84540C", ["WarningSubtleBrush"] = "#FFF3DC",
    };

    private static readonly IReadOnlyDictionary<string, string> DarkColors = new Dictionary<string, string>
    {
        ["WindowBrush"] = "#181A1E", ["SurfaceBrush"] = "#23262B", ["SubtleBrush"] = "#2C3037",
        ["BorderBrush"] = "#3A3F47", ["TextBrush"] = "#F2F4F7", ["SecondaryTextBrush"] = "#ADB6C3",
        ["HoverBrush"] = "#313640", ["PressedBrush"] = "#3B424D",
        ["AccentBrush"] = "#76DDA9", ["AccentTextBrush"] = "#092B1B", ["AccentHoverBrush"] = "#92E7BB",
        ["AccentPressedBrush"] = "#AFEED0", ["AccentSubtleBrush"] = "#203E31",
        ["WarningBrush"] = "#F0C47F", ["WarningSubtleBrush"] = "#423524",
    };

    private static IReadOnlyDictionary<string, string> HighContrastColors() => new Dictionary<string, string>
    {
        ["WindowBrush"] = SystemColors.WindowColor.ToString(), ["SurfaceBrush"] = SystemColors.WindowColor.ToString(),
        ["SubtleBrush"] = SystemColors.ControlColor.ToString(), ["BorderBrush"] = SystemColors.WindowTextColor.ToString(),
        ["TextBrush"] = SystemColors.WindowTextColor.ToString(), ["SecondaryTextBrush"] = SystemColors.WindowTextColor.ToString(),
        ["HoverBrush"] = SystemColors.ControlColor.ToString(), ["PressedBrush"] = SystemColors.ControlDarkColor.ToString(),
        ["AccentBrush"] = SystemColors.HighlightColor.ToString(), ["AccentTextBrush"] = SystemColors.HighlightTextColor.ToString(),
        ["AccentHoverBrush"] = SystemColors.HighlightColor.ToString(), ["AccentPressedBrush"] = SystemColors.HighlightColor.ToString(),
        ["AccentSubtleBrush"] = SystemColors.WindowColor.ToString(), ["WarningBrush"] = SystemColors.WindowTextColor.ToString(),
        ["WarningSubtleBrush"] = SystemColors.WindowColor.ToString(),
    };

    [DllImport("dwmapi.dll")]
    private static extern int DwmSetWindowAttribute(IntPtr window, int attribute, ref int value, int size);

    public void Dispose()
    {
        _disposed = true;
        SystemEvents.UserPreferenceChanged -= OnPreferenceChanged;
        SystemParameters.StaticPropertyChanged -= OnSystemParameterChanged;
        _source?.RemoveHook(WindowMessages);
    }
}
