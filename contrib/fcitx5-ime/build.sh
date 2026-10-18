#!/usr/bin/env bash
# 构建并安装 uuremote 的 fcitx5 插件：把移动端输入法提交的文本直接送进当前输入框，不经过剪贴板。
#
#   ./build.sh               编译并安装（写入 /usr/lib/fcitx5，需要 sudo）
#   ./build.sh --yes         同上，但非交互：sudo 用 -n（凭据没缓存就直接失败，不会卡在密码提示）
#   ./build.sh --uninstall   卸载
#
# 依赖 fcitx5（头文件在 /usr/include/Fcitx5）、libFcitx5Core、C++20 编译器。
# 插件与 fcitx5 的 ABI 绑定（libFcitx5Core.so.<major>）：fcitx5 大版本更新后重跑一次本脚本。
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
lib=libuuremote_ime.so
conf=uuremote_ime.conf
lib_dir=/usr/lib/fcitx5
conf_dir=/usr/share/fcitx5/addon
socket="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/uuremote-ime.sock"
assume_yes=0
for arg in "$@"; do
    case "$arg" in
        --yes|-y) assume_yes=1 ;;
        --uninstall) uninstall=1 ;;
        *) echo "未知参数: $arg" >&2; exit 2 ;;
    esac
done
uninstall="${uninstall:-0}"
# 交互运行时让 sudo 自己提示密码；无人值守（uu-remote-update）用 sudo -n，失败就退出而不是卡住。
sudo_run() {
    if (( assume_yes )) || [[ ! -t 0 ]]; then sudo -n "$@"; else sudo "$@"; fi
}

restart_hint() {
    cat >&2 <<'MSG'
  重启输入法（KDE 下 compositor 的输入法连接由 KWin 提供，必须让它重新拉起）：
    系统设置 → 键盘 → 输入法：先选「无」，再选回 Fcitx 5；或者注销后重新登录。
    只 kill/重启 fcitx5 进程是不够的：没有 KWin 的 launcher 时插件能加载，
    但文本进不了任何输入框（fcitx5 看不到输入上下文）。
MSG
}

if (( uninstall )); then
    sudo_run rm -f "$lib_dir/$lib" "$conf_dir/$conf"
    echo "已删除 $lib_dir/$lib 与 $conf_dir/$conf"
    restart_hint
    exit 0
fi

echo "== 编译插件 =="
c++ -std=c++20 -O2 -fPIC -shared -Wall -Wextra -pthread \
    -I/usr/include/Fcitx5/Core -I/usr/include/Fcitx5/Utils -I/usr/include/Fcitx5/Config \
    "$here/uuremote_ime.cpp" -lFcitx5Core -lFcitx5Utils -o "$here/$lib"

echo "== 安装到 $lib_dir =="
if (( assume_yes )) || [[ ! -t 0 ]]; then
    echo "  非交互模式：使用 sudo -n，凭据没有缓存时会直接失败（不会停在密码提示）"
fi
sudo_run install -Dm755 "$here/$lib" "$lib_dir/$lib"
sudo_run install -Dm644 "$here/$conf" "$conf_dir/$conf"

echo "== 重启 fcitx5 =="
# 先让当前实例退出，再按 KDE 自启动项 (/etc/xdg/autostart/org.fcitx.Fcitx5.desktop) 的方式拉起。
# KWin 保留的 launcher 会把 compositor 的输入法连接交给新实例。
fcitx5-remote -e 2>/dev/null || true
sleep 1
if ! pgrep -x fcitx5 >/dev/null; then
    setsid /usr/bin/fcitx5 >/dev/null 2>&1 </dev/null &
    sleep 4
fi

echo "== 检查 =="
missing=0
if busctl --user call org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1 GetAddons 2>/dev/null | grep -q uuremote_ime; then
    echo "✅ 插件已加载"
else
    echo "⚠️  插件未加载"; missing=1
fi
if [[ -S "$socket" ]]; then
    echo "✅ socket 就绪：$socket"
else
    echo "⚠️  没有 socket：$socket"; missing=1
fi
if [[ "${XDG_CURRENT_DESKTOP:-}" == *KDE* ]]; then
    if pgrep -f 'fcitx5-wayland-launcher' >/dev/null 2>&1; then
        echo "✅ KWin 的 wayland launcher 在运行（compositor 连接正常）"
    else
        echo "⚠️  KWin 的 wayland launcher 没在运行：插件会加载，但文本进不了输入框。"
        missing=1
    fi
fi
(( missing )) && restart_hint || true
exit 0