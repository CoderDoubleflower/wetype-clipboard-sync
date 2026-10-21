using System.IO;
using System.Text;
using System.Text.Json;
using System.Threading.Channels;
using System.Windows;

namespace WeTypeClipboard.Wpf;

/// <summary>
/// Private inherited pipes connect this disposable window to its owning Rust
/// tray. No pipe name, local server, credential, or daemon lifecycle is added.
/// </summary>
internal sealed class UiBridge : IDisposable
{
    private const int MaximumFrameBytes = 128 * 1024;
    private readonly MainWindow _window;
    private readonly StatusViewModel _view;
    private readonly CancellationTokenSource _cancellation = new();
    private readonly Channel<string> _commands = Channel.CreateBounded<string>(new BoundedChannelOptions(8)
    {
        SingleReader = true,
        SingleWriter = true,
        FullMode = BoundedChannelFullMode.Wait,
    });

    internal UiBridge(MainWindow window, StatusViewModel view)
    {
        _window = window;
        _view = view;
    }

    internal void SendCommand(string command)
    {
        if (command is not ("start" or "stop" or "restart" or "sync" or "setup" or "log" or "folder"))
            return;
        if (!_commands.Writer.TryWrite(command))
            Application.Current.Shutdown(1);
    }

    internal async Task RunAsync()
    {
        // Anonymous Windows pipe I/O may use blocking OS reads internally.
        // Both directions stay off the WPF dispatcher, even on the first read.
        var reader = Task.Run(ReadSnapshotsAsync);
        var writer = Task.Run(WriteCommandsAsync);
        try
        {
            var completed = await Task.WhenAny(reader, writer);
            await completed;
            _cancellation.Cancel();
            Application.Current.Shutdown(0);
        }
        catch (Exception error) when (error is IOException or JsonException or DecoderFallbackException or OperationCanceledException or InvalidOperationException)
        {
            Application.Current.Shutdown(1);
        }
    }

    private async Task ReadSnapshotsAsync()
    {
        using var input = Console.OpenStandardInput();
        using var frame = new MemoryStream();
        var buffer = new byte[4096];
        var utf8 = new UTF8Encoding(false, true);
        while (!_cancellation.IsCancellationRequested)
        {
            var count = await input.ReadAsync(buffer, _cancellation.Token);
            if (count == 0)
                return; // Owner exited: never leave an orphaned UI process.
            for (var index = 0; index < count; index++)
            {
                if (buffer[index] == (byte)'\n')
                {
                    var line = utf8.GetString(frame.GetBuffer(), 0, checked((int)frame.Length));
                    frame.SetLength(0);
                    using var document = JsonDocument.Parse(line, new JsonDocumentOptions { MaxDepth = 16 });
                    var root = document.RootElement;
                    if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("type", out var type))
                        throw new JsonException("Invalid bridge frame.");
                    switch (type.GetString())
                    {
                        case "snapshot":
                            var snapshot = root.Deserialize<UiSnapshot>(UiSnapshot.JsonOptions) ?? throw new JsonException("Empty snapshot.");
                            await _window.Dispatcher.InvokeAsync(() => _view.Apply(snapshot));
                            break;
                        case "show":
                            await _window.Dispatcher.InvokeAsync(_window.Reveal);
                            break;
                        case "exit":
                            return;
                        default:
                            throw new JsonException("Unknown bridge frame.");
                    }
                }
                else
                {
                    if (frame.Length >= MaximumFrameBytes)
                        throw new JsonException("Bridge frame exceeds size limit.");
                    frame.WriteByte(buffer[index]);
                }
            }
        }
    }

    private async Task WriteCommandsAsync()
    {
        using var output = Console.OpenStandardOutput();
        await foreach (var command in _commands.Reader.ReadAllAsync(_cancellation.Token))
        {
            var bytes = Encoding.ASCII.GetBytes(command + "\n");
            await output.WriteAsync(bytes, _cancellation.Token);
            await output.FlushAsync(_cancellation.Token);
        }
    }

    public void Dispose()
    {
        _commands.Writer.TryComplete();
        _cancellation.Cancel();
        // Do not dispose a CancellationTokenSource while a pipe task may still
        // be observing it. Process shutdown releases the inherited handles.
    }
}
