using System.ComponentModel;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;

namespace WeTypeClipboard.Wpf;

public partial class MainWindow : Window
{
    private readonly StatusViewModel _view;
    private readonly ThemeService _themes;

    public event Action<string>? CommandRequested;

    internal MainWindow(StatusViewModel view, ThemeService themes)
    {
        _view = view;
        _themes = themes;
        InitializeComponent();
        DataContext = view;
        _themes.Changed += UpdateThemeLabel;
        UpdateThemeLabel();
        SizeChanged += (_, _) => UpdateResponsiveLayout();
        ContentRendered += (_, _) => _themes.ApplyTitleBar();
        SourceInitialized += (_, _) =>
        {
            _themes.AttachWindow(new WindowInteropHelper(this).Handle);
            // Keep the content scrollable on small screens and high DPI while
            // letting WPF perform layout in device-independent units.
            var area = SystemParameters.WorkArea;
            MinWidth = Math.Min(MinWidth, Math.Max(320, area.Width - 32));
            MinHeight = Math.Min(MinHeight, Math.Max(300, area.Height - 32));
            Width = Math.Min(Width, Math.Max(MinWidth, area.Width - 32));
            Height = Math.Min(Height, Math.Max(MinHeight, area.Height - 32));
        };
        Closed += (_, _) => _themes.Changed -= UpdateThemeLabel;
    }

    internal void Reveal()
    {
        if (!IsVisible)
            Show();
        if (WindowState == WindowState.Minimized)
            WindowState = WindowState.Normal;
        Activate();
        if (!IsKeyboardFocusWithin)
            (StartButton.IsVisible ? StartButton : SyncButton).Focus();
    }

    private void UpdateThemeLabel() => ThemeLabel.Text = _themes.Label;

    private void UpdateResponsiveLayout()
    {
        var stacked = ActualWidth < 820;
        Grid.SetColumn(ConfigPanel, stacked ? 0 : 2);
        Grid.SetRow(ConfigPanel, stacked ? 1 : 0);
        ConfigPanel.Margin = stacked ? new Thickness(0, 16, 0, 0) : new Thickness(0);
        DetailsGrid.ColumnDefinitions[1].Width = new GridLength(stacked ? 0 : 16);
        DetailsGrid.ColumnDefinitions[2].Width = stacked ? new GridLength(0) : new GridLength(1, GridUnitType.Star);
    }

    private void Action_Click(object sender, RoutedEventArgs e)
    {
        if (sender is Button { Tag: string action })
        {
            if (action is "start" or "stop" or "sync" or "restart" or "setup")
                _view.MarkCommandPending();
            CommandRequested?.Invoke(action);
        }
    }

    private void Hide_Click(object sender, RoutedEventArgs e) => Hide();

    private void Window_Closing(object? sender, CancelEventArgs e)
    {
        // Application.Shutdown bypasses this cancellation when the owning tray
        // exits. A user's close gesture leaves synchronization and the tray up.
        e.Cancel = true;
        Hide();
    }

    private void Window_PreviewKeyDown(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.Escape)
        {
            Hide();
            e.Handled = true;
        }
    }
}
