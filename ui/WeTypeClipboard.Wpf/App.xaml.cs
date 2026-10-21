using System.IO;
using System.Windows;

namespace WeTypeClipboard.Wpf;

public partial class App : Application
{
    private ThemeService? _themes;
    private UiBridge? _bridge;

    protected override void OnStartup(StartupEventArgs e)
    {
        base.OnStartup(e);
        var bridgeMode = e.Args is ["--bridge"];
        var captureMode = e.Args is ["--capture-dir", _];
        if (!bridgeMode && !captureMode)
        {
            MessageBox.Show("请通过 WeType 剪贴板同步的托盘或开始菜单打开此窗口。", "WeType 剪贴板同步", MessageBoxButton.OK, MessageBoxImage.Information);
            Shutdown(1);
            return;
        }

        _themes = new ThemeService(this);
        var view = new StatusViewModel();
        var window = new MainWindow(view, _themes);
        MainWindow = window;

        DispatcherUnhandledException += (_, args) =>
        {
            // Never serialize exception text or stack traces into the command
            // channel. The tray detects EOF and can reopen a fresh UI process.
            args.Handled = true;
            Shutdown(1);
        };

        if (captureMode)
        {
            window.Loaded += async (_, _) =>
            {
                try
                {
                    await CaptureVerification.RunAsync(window, view, _themes, Path.GetFullPath(e.Args[1]));
                    Shutdown(0);
                }
                catch (Exception exception)
                {
                    Directory.CreateDirectory(Path.GetFullPath(e.Args[1]));
                    File.WriteAllText(Path.Combine(Path.GetFullPath(e.Args[1]), "capture-error.txt"), exception.ToString());
                    Shutdown(1);
                }
            };
            window.Show();
        }
        else
        {
            _bridge = new UiBridge(window, view);
            window.CommandRequested += _bridge.SendCommand;
            _ = _bridge.RunAsync();
        }
    }

    protected override void OnExit(ExitEventArgs e)
    {
        _bridge?.Dispose();
        _themes?.Dispose();
        base.OnExit(e);
    }
}
