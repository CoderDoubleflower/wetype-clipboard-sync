#!/usr/bin/env bash
# 把 wxclipd 装到 ~/.local/bin 并登记 systemd user 服务。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="$root/wxclipd/target/release/wxclipd"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

if [[ ! -x "$binary" ]]; then
    echo "先构建：cd $root/wxclipd && cargo build --release -j 3" >&2
    exit 1
fi
if ! command -v wl-paste >/dev/null && ! command -v xclip >/dev/null; then
    echo "缺少剪贴板工具：Wayland 装 wl-clipboard，X11 装 xclip" >&2
    exit 1
fi

install -Dm755 "$binary" "$HOME/.local/bin/wxclipd"
install -Dm644 "$root/packaging/wxclipd.service" "$unit_dir/wxclipd.service"
systemctl --user daemon-reload

# 粘贴触发需要读 /dev/input/event*；用 udev 的 uaccess 授权给活跃会话。
if [[ "${WXCLIPD_SKIP_UDEV:-0}" != "1" ]]; then
    if sudo install -Dm644 "$root/packaging/99-wxclipd-input.rules" /etc/udev/rules.d/99-wxclipd-input.rules; then
        sudo udevadm control --reload-rules
        sudo udevadm trigger --subsystem-match=input
        echo "已安装输入设备 udev 规则（Ctrl/右键触发即时同步）"
    else
        echo "跳过 udev 规则；如需 Ctrl/右键触发，请手动安装 packaging/99-wxclipd-input.rules" >&2
    fi
fi

echo "已安装。配对：wxclipd code 或 wxclipd pair CODE；启动：systemctl --user enable --now wxclipd"
