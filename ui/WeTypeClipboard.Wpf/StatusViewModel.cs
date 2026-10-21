using System.ComponentModel;
using System.Globalization;
using System.Reflection;
using System.Text.Json;
using System.Windows;

namespace WeTypeClipboard.Wpf;

internal sealed class UiSnapshot
{
    public string Type { get; init; } = "snapshot";
    public string? Version { get; init; }
    public string? PhaseLabel { get; init; }
    public string? Phase { get; init; }
    public bool Busy { get; init; }
    public bool Running { get; init; }
    public bool Ready { get; init; }
    public string? Operation { get; init; }
    public string? Notice { get; init; }
    public string? StateDirectory { get; init; }
    public string? SavedConfig { get; init; }
    public RuntimeSnapshot? Runtime { get; init; }

    internal static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        MaxDepth = 16,
    };
}

internal sealed class RuntimeSnapshot
{
    public string? Phase { get; init; }
    public uint? Pid { get; init; }
    public string? DeviceName { get; init; }
    public ulong? StartedAt { get; init; }
    public ulong? LastSuccessAt { get; init; }
    public ulong? LastSendAt { get; init; }
    public ulong? LastReceiveAt { get; init; }
    public ulong? SentCount { get; init; }
    public ulong? ReceivedCount { get; init; }
    public bool PendingSend { get; init; }
    public bool SendPaused { get; init; }
    public string? LastError { get; init; }
    public string? Proxy { get; init; }
    public string? ProxySource { get; init; }
    public ulong? PollIntervalSeconds { get; init; }
    public ulong? LocalPollIntervalMs { get; init; }
    public bool? PasteTrigger { get; init; }
}

internal sealed class StatusViewModel : INotifyPropertyChanged
{
    private UiSnapshot _snapshot = new();
    private bool _commandPending;
    public event PropertyChangedEventHandler? PropertyChanged;

    public bool Busy => _snapshot.Busy || _commandPending;
    public bool CanStart => !Busy && !_snapshot.Running;
    public bool CanStop => !Busy && _snapshot.Running;
    public bool CanSync => !Busy && _snapshot.Running;
    public bool CanConfigure => !Busy;
    public Visibility StartVisibility => _snapshot.Running ? Visibility.Collapsed : Visibility.Visible;
    public Visibility StopVisibility => _snapshot.Running ? Visibility.Visible : Visibility.Collapsed;
    private RuntimeSnapshot? Runtime => _snapshot.Runtime;
    private string? Phase => _snapshot.Phase ?? Runtime?.Phase;
    public string PhaseLabel => _snapshot.PhaseLabel ?? "正在读取状态";
    public string BusyLabel => "正在处理，请稍候…";
    public string Description => Busy ? _snapshot.Operation ?? BusyLabel : Runtime?.SendPaused == true
        ? "上次发送结果不确定。查看日志后可点击「立即同步」重试；重试可能重复发送。"
        : Phase switch
        {
            "running" => "文本与图片正在此电脑和已配对设备之间自动同步。",
            "stopped" => "点击「开始同步」，继续在设备间接收和发送文本与图片。",
            "retrying" => "后台正在自动恢复连接。你可以继续使用电脑，稍后会重试同步。",
            "initializing" => "正在连接服务并读取设备信息，请稍候。",
            "stopping" => "正在完成当前操作并安全停止同步。",
            "legacy" => "旧版本后台仍在运行。请点击「重启同步」以载入当前版本。",
            "unavailable" => "后台仍在运行，暂时无法读取详细状态；可查看日志或重启同步。",
            _ => _snapshot.Ready ? "暂时无法读取本地状态，请稍后重试或查看日志。" : "正在读取本地配置与同步状态。",
        };
    public string Tone => Runtime?.SendPaused == true || Phase is "retrying" or "legacy" or "unavailable"
        ? "warning" : Phase == "running" ? "active" : "neutral";
    public string StatusGlyph => Tone == "warning" ? "\uE7BA" : Phase == "stopped" ? "\uE769" : "\uE895";
    public string? Notice => _snapshot.Notice;
    public bool HasNotice => !string.IsNullOrWhiteSpace(Notice);
    public string? LastError => Runtime?.LastError;
    public bool HasError => !string.IsNullOrWhiteSpace(LastError);
    public string DeviceName => Runtime?.DeviceName ?? "等待同步启动";
    public string ProcessDescription => Runtime?.Pid is uint pid ? $"后台进程：{pid}" : "后台尚未启动";
    public string StartedAt => LocalTime(Runtime?.StartedAt);
    public string LastSuccess => LocalTime(Runtime?.LastSuccessAt);
    public string LastSend => Runtime?.LastSendAt is ulong timestamp ? $"最近 {LocalTime(timestamp)}" : "暂无发送记录";
    public string LastReceive => Runtime?.LastReceiveAt is ulong timestamp ? $"最近 {LocalTime(timestamp)}" : "暂无接收记录";
    public string SentCount => Runtime?.SentCount?.ToString("N0", CultureInfo.CurrentCulture) ?? "—";
    public string ReceivedCount => Runtime?.ReceivedCount?.ToString("N0", CultureInfo.CurrentCulture) ?? "—";
    public string PendingSend => Runtime?.SendPaused == true ? "发送暂停，等待手动处理"
        : Runtime?.PendingSend == true ? "有最新内容等待发送" : "无待发送内容";
    public string Proxy => Runtime?.Proxy ?? "同步启动后显示";
    public string ProxySource => Runtime?.Proxy is null ? "当前没有运行中的配置" : $"代理来源：{Runtime.ProxySource switch
    {
        "command" => "命令行",
        "environment" => "环境变量 WXCLIPD_PROXY",
        "config" => "config.json",
        "direct" => "默认直连",
        _ => "后台配置",
    }}";
    public string RemoteInterval => Runtime?.PollIntervalSeconds is ulong seconds ? $"{seconds} 秒" : "—";
    public string LocalInterval => Runtime?.LocalPollIntervalMs is ulong milliseconds ? $"{milliseconds} 毫秒" : "—";
    public string PasteTrigger => Runtime?.PasteTrigger is bool enabled ? $"Ctrl / 右键触发：{(enabled ? "开启" : "关闭")}" : "Ctrl / 右键触发：同步启动后显示";
    public string SavedConfig => _snapshot.SavedConfig ?? "正在读取配置…";
    public string StateDirectory => _snapshot.StateDirectory ?? "正在读取…";
    public string VersionLabel => $"v{_snapshot.Version ?? Assembly.GetExecutingAssembly().GetName().Version?.ToString(3)} · Windows x86";

    internal void Apply(UiSnapshot snapshot)
    {
        _snapshot = snapshot;
        _commandPending = false;
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(string.Empty));
    }

    internal void MarkCommandPending()
    {
        _commandPending = true;
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(string.Empty));
    }

    private static string LocalTime(ulong? timestamp)
    {
        if (timestamp is null or > 253402300799)
            return "暂无";
        return DateTimeOffset.FromUnixTimeSeconds((long)timestamp.Value).ToLocalTime().ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.CurrentCulture);
    }
}
