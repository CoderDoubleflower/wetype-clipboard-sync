using System.IO;
using System.Reflection;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Threading;

namespace WeTypeClipboard.Wpf;

/// <summary>
/// Exercises the actual WPF visual tree on the Windows build runner. Fixtures
/// contain no user identity or clipboard data. Live OS switching is separately
/// checked by the package's UI Automation test against the installed app.
/// </summary>
internal static class CaptureVerification
{
    internal static async Task RunAsync(MainWindow window, StatusViewModel view, ThemeService themes, string directory)
    {
        Directory.CreateDirectory(directory);
        var version = Assembly.GetExecutingAssembly().GetName().Version?.ToString(3) ?? "0.0.0";
        var fixture = Fixture(version);
        view.Apply(fixture);
        window.Width = 960;
        window.Height = 960;
        window.UpdateLayout();
        var checks = new List<string>();

        foreach (var command in new[] { "send-on", "send-off", "receive-on", "receive-off" })
            Require(UiBridge.IsAllowedCommand(command), "Direction action must pass the WPF bridge allowlist.");
        foreach (var command in new[] { "toggle-send", "send-on true", "receive-off\nstart", "exit", "" })
            Require(!UiBridge.IsAllowedCommand(command), "Unknown or injected direction actions must be rejected.");
        checks.Add("explicit direction bridge command allowlist");

        Require(window.SyncButton.IsEnabled && window.StopButton.IsEnabled, "Running actions must be enabled.");
        Require(!window.StartButton.IsVisible, "Running view must not show Start.");
        checks.Add("running action state");

        view.Apply(new UiSnapshot { Phase = "stopped", PhaseLabel = "同步已停止", Ready = true });
        window.UpdateLayout();
        Require(window.StartButton.IsVisible && window.StartButton.IsEnabled && !window.SyncButton.IsVisible, "Stopped action state is invalid.");
        checks.Add("stopped action state");

        view.Apply(new UiSnapshot { Running = true, Busy = true, Phase = "running", PhaseLabel = "正在处理" });
        window.UpdateLayout();
        Require(!window.SyncButton.IsEnabled && !window.StopButton.IsEnabled, "Busy actions must be disabled.");
        checks.Add("busy action state");

        view.Apply(new UiSnapshot { Running = true, Phase = "running", Runtime = new RuntimeSnapshot { SendPaused = true } });
        Require(view.Tone == "warning" && view.Description.Contains("查看日志"), "Paused sends must not look healthy.");
        checks.Add("paused send warning");

        foreach (var send in new[] { false, true })
        foreach (var receive in new[] { false, true })
        {
            view.Apply(new UiSnapshot
            {
                Running = true, Ready = true, Phase = "running", SendEnabled = send, ReceiveEnabled = receive,
                Runtime = new RuntimeSnapshot { SendEnabled = send, ReceiveEnabled = receive },
            });
            window.UpdateLayout();
            Require(window.SendSwitch.IsChecked == send && window.ReceiveSwitch.IsChecked == receive, "Direction switches must reflect configuration.");
            Require(window.SyncButton.IsEnabled == (send || receive), "Sync-now must be disabled when both directions are off.");
        }
        checks.Add("all four direction modes and manual sync gating");
        view.Apply(new UiSnapshot { Running = true, Ready = true, Busy = true, SendEnabled = true, ReceiveEnabled = true });
        window.UpdateLayout();
        Require(!window.SendSwitch.IsEnabled && !window.ReceiveSwitch.IsEnabled, "Busy direction changes must be disabled.");
        checks.Add("busy direction switches disabled");

        window.Width = 740;
        window.UpdateLayout();
        await window.Dispatcher.InvokeAsync(() => { }, DispatcherPriority.Render);
        Require(Grid.GetRow(window.ConfigPanel) == 1, "Narrow layout must stack configuration.");
        checks.Add("narrow responsive layout");
        window.Width = 960;
        view.Apply(fixture);

        themes.SetCaptureTheme(false);
        await RenderReady(window);
        Require(window.ThemeLabel.Text.EndsWith("浅色") && themes.BackgroundColor == "#FFF5F6F8", "Light theme did not apply.");
        var lightContrast = VerifyButtonText(window.SyncButton);
        var light = Save(window, themes, directory, "light.png", "light");
        checks.Add($"light primary rendered text contrast {lightContrast:F2}:1");
        checks.Add("light theme resources and opaque client render");

        themes.SetCaptureTheme(true);
        await RenderReady(window);
        Require(window.ThemeLabel.Text.EndsWith("深色") && themes.BackgroundColor == "#FF181A1E", "Dark theme did not apply.");
        var darkContrast = VerifyButtonText(window.SyncButton);
        var dark = Save(window, themes, directory, "dark.png", "dark");
        checks.Add($"dark primary rendered text contrast {darkContrast:F2}:1");
        checks.Add("dark theme resources and opaque client render");

        // Start uses the same primary style but lives in a different state.
        // Verify its actual generated text after both visibility and theme
        // changes, including a return from dark to light on the same window.
        view.Apply(new UiSnapshot { Phase = "stopped", PhaseLabel = "同步已停止", Ready = true });
        await RenderReady(window);
        VerifyButtonText(window.StartButton);
        themes.SetCaptureTheme(false);
        await RenderReady(window);
        VerifyButtonText(window.StartButton);
        checks.Add("start button generated text after live dark-to-light change");

        themes.SetCaptureTheme(null);
        File.WriteAllText(Path.Combine(directory, "capture-result.json"), JsonSerializer.Serialize(new
        {
            passed = true,
            version,
            operating_system = Environment.OSVersion.VersionString,
            light,
            dark,
            checks,
        }, new JsonSerializerOptions { WriteIndented = true }));
    }

    private static async Task RenderReady(Window window)
    {
        window.UpdateLayout();
        await window.Dispatcher.InvokeAsync(() => { }, DispatcherPriority.Render);
        // Allow the native caption and WPF's theme resources to settle on the
        // CI desktop before capturing the client visual tree.
        await Task.Delay(150);
        window.UpdateLayout();
    }

    private static object Save(MainWindow window, ThemeService themes, string directory, string filename, string theme)
    {
        // Window.ActualHeight includes the native non-client caption, which
        // RenderTargetBitmap cannot paint. Capture the measured client root
        // itself, at its real DPI, instead of allocating an empty caption band.
        var root = window.ClientRoot;
        var dpi = VisualTreeHelper.GetDpi(root);
        var width = (int)Math.Round(root.RenderSize.Width * dpi.DpiScaleX);
        var height = (int)Math.Round(root.RenderSize.Height * dpi.DpiScaleY);
        Require(width > 0 && height > 0, "Client root must have a measured visual tree.");
        var bitmap = new RenderTargetBitmap(width, height, dpi.PixelsPerInchX, dpi.PixelsPerInchY, PixelFormats.Pbgra32);
        bitmap.Render(root);
        var lastPixel = new byte[4];
        bitmap.CopyPixels(new Int32Rect(width / 2, height - 1, 1, 1), lastPixel, 4, 0);
        Require(lastPixel[3] == 255, "Client capture has a transparent bottom band.");
        var encoder = new PngBitmapEncoder();
        encoder.Frames.Add(BitmapFrame.Create(bitmap));
        using (var stream = File.Create(Path.Combine(directory, filename)))
            encoder.Save(stream);
        Require(new FileInfo(Path.Combine(directory, filename)).Length > 4096, "Window capture is unexpectedly empty.");
        return new { theme, background = themes.BackgroundColor, width, height, file = filename, requested_dark_caption = themes.IsDark && !themes.IsHighContrast, title_bar_hresult = themes.LastTitleBarResult };
    }

    private static double VerifyButtonText(Button button)
    {
        Require(button.IsVisible && button.IsEnabled, "Contrast check requires a visible, enabled button.");
        button.ApplyTemplate();
        button.UpdateLayout();
        var border = button.Template.FindName("ButtonBorder", button) as Border;
        Require(border?.Background is SolidColorBrush, "The rendered button must have a solid background.");
        Require(button.Foreground is SolidColorBrush, "The button must declare a contrasting text foreground.");
        var background = ((SolidColorBrush)border!.Background).Color;
        var expectedForeground = ((SolidColorBrush)button.Foreground).Color;
        var labels = VisualDescendants<TextBlock>(button).Where(text => !string.IsNullOrEmpty(text.Text)).ToArray();
        Require(labels.Length > 0, "No actual rendered label was found inside the button template.");
        var minimumContrast = double.MaxValue;
        foreach (var label in labels)
        {
            Require(label.Foreground is SolidColorBrush, "The rendered label must have a solid foreground.");
            var foreground = ((SolidColorBrush)label.Foreground).Color;
            Require(foreground == expectedForeground, "Generated button text lost the control's foreground through styling.");
            var contrast = Contrast(foreground, background);
            Require(contrast >= 4.5, $"Actual button text contrast is only {contrast:F2}:1.");
            minimumContrast = Math.Min(minimumContrast, contrast);
        }
        return minimumContrast;
    }

    private static IEnumerable<T> VisualDescendants<T>(DependencyObject parent) where T : DependencyObject
    {
        for (var index = 0; index < VisualTreeHelper.GetChildrenCount(parent); index++)
        {
            var child = VisualTreeHelper.GetChild(parent, index);
            if (child is T item)
                yield return item;
            foreach (var descendant in VisualDescendants<T>(child))
                yield return descendant;
        }
    }

    private static double Contrast(Color first, Color second)
    {
        static double Linear(byte component)
        {
            var channel = component / 255.0;
            return channel <= 0.04045 ? channel / 12.92 : Math.Pow((channel + 0.055) / 1.055, 2.4);
        }
        static double Luminance(Color color) => 0.2126 * Linear(color.R) + 0.7152 * Linear(color.G) + 0.0722 * Linear(color.B);
        var one = Luminance(first);
        var two = Luminance(second);
        return (Math.Max(one, two) + 0.05) / (Math.Min(one, two) + 0.05);
    }

    private static void Require(bool condition, string message)
    {
        if (!condition)
            throw new InvalidOperationException(message);
    }

    private static UiSnapshot Fixture(string version) => new()
    {
        Version = version,
        Phase = "running",
        PhaseLabel = "同步运行中",
        Running = true,
        Ready = true,
        SendEnabled = true,
        ReceiveEnabled = true,
        StateDirectory = @"C:\Users\WeType\AppData\Local\wxclipd",
        SavedConfig = "代理：socks5h://127.0.0.1:1080\n远端轮询：10 秒；Ctrl / 右键触发：开启",
        Runtime = new RuntimeSnapshot
        {
            Phase = "running",
            Pid = 6248,
            DeviceName = "我的 Windows 电脑",
            StartedAt = 1792576800,
            LastSuccessAt = 1792585800,
            LastSendAt = 1792585775,
            LastReceiveAt = 1792585718,
            SentCount = 128,
            ReceivedCount = 96,
            Proxy = "socks5h://127.0.0.1:1080",
            ProxySource = "config",
            PollIntervalSeconds = 10,
            LocalPollIntervalMs = 400,
            PasteTrigger = true,
            SendEnabled = true,
            ReceiveEnabled = true,
        },
    };
}
