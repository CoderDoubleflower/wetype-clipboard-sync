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
echo "已安装。配对：wxclipd code 或 wxclipd pair CODE；启动：systemctl --user enable --now wxclipd"
