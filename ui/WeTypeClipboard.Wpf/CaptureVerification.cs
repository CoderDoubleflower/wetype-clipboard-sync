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
        var light = Save(window, themes, directory, "light.png", "light");
        checks.Add("light theme resources and render");

        themes.SetCaptureTheme(true);
        await RenderReady(window);
        Require(window.ThemeLabel.Text.EndsWith("深色") && themes.BackgroundColor == "#FF181A1E", "Dark theme did not apply.");
        var dark = Save(window, themes, directory, "dark.png", "dark");
        checks.Add("dark theme resources and render");

        themes.SetCaptureTheme(null);
        File.WriteAllText(Path.Combine(directory, "capture-result.json"), JsonSerializer.Serialize(new
        {
            passed = true,
            version,
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

    private static object Save(Window window, ThemeService themes, string directory, string filename, string theme)
    {
        var width = (int)Math.Ceiling(window.ActualWidth);
        var height = (int)Math.Ceiling(window.ActualHeight);
        Require(width > 0 && height > 0, "Window must have a measured visual tree.");
        var bitmap = new RenderTargetBitmap(width, height, 96, 96, PixelFormats.Pbgra32);
        bitmap.Render(window);
        var encoder = new PngBitmapEncoder();
        encoder.Frames.Add(BitmapFrame.Create(bitmap));
        using (var stream = File.Create(Path.Combine(directory, filename)))
            encoder.Save(stream);
        Require(new FileInfo(Path.Combine(directory, filename)).Length > 4096, "Window capture is unexpectedly empty.");
        return new { theme, background = themes.BackgroundColor, width, height, file = filename };
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
        },
    };
}
