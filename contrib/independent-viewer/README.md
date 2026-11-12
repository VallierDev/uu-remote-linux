# 独立会话组件 / Independent-session companion

这是可选的 Linux 控制端组件：每台设备使用单独的 systemd 用户服务和桌面入口，
关闭主窗口后仍保持连接。重复打开同一设备会复用会话；明确注销账号会停止独立会话。
主窗口原有布局和“快速启动”保持原样，组件入口需要主动启用。

This optional Linux viewer companion gives each device a separate user service
and desktop entry. Closing the launcher preserves the session; reopening the same
entry reuses it, and explicit account sign-out stops independent sessions.
The existing launcher layout and Quick launch section stay intact.

## 安装和使用 / Install and use

在已登录 UU 的 Linux 图形用户会话运行。需要 Python 3、systemd 用户服务；
设置窗口需要 PyGObject/GTK3（Debian/Ubuntu 的 `python3-gi`），字体自动选择需要
fontconfig；X11/XWayland 的窗口恢复需要 `xprop` 和 libX11。

Run as the signed-in desktop user. Requirements: Python 3 and a systemd user
manager, PyGObject/GTK3 for settings (`python3-gi` on Debian/Ubuntu), fontconfig
for font discovery, and xprop/libX11 for X11/XWayland window restoration.

```sh
./contrib/independent-viewer/install.sh
cargo build --release --locked --features native-viewer,viewer-sdl --bin uuremote-viewer
UU_VIEWER_INDEPENDENT=1 target/release/uuremote-viewer ui
```

选择设备后，在“更多工具”使用“独立远控”或“独立会话设置”。安装器只安装辅助脚本，
不替换系统软件包或主窗口桌面入口。也可直接调用：

Select a device, then use **More tools → Independent remote / Independent session
settings**. The installer only installs the helper; it does not replace the system
package or the main desktop entry. CLI setup is also available:

```sh
UU_VIEWER_EXECUTABLE=/absolute/path/to/uuremote-viewer \
  ~/.local/bin/uuremote-independent configure DEVICE_ID 'Remote device' 4
UU_VIEWER_EXECUTABLE=/absolute/path/to/uuremote-viewer \
  ~/.local/bin/uuremote-independent open DEVICE_ID 'Remote device' 4
```

平台值为 Mac `4`、Windows/Linux `1`。运行时配置位于
`$XDG_CONFIG_HOME/uuremote/independent/`（默认 `~/.config/...`），目录 0700、文件 0600；
入口位于 `$XDG_DATA_HOME/applications/`。登录凭据只由原生 viewer 读取。
Dock 固定使用桌面环境本身的功能。

Platform values are `4` for Mac and `1` for Windows/Linux. Configuration lives in
`$XDG_CONFIG_HOME/uuremote/independent/` (default `~/.config/...`), with a private
0700 directory and 0600 files. Generated entries live in `$XDG_DATA_HOME/applications/`.
Only the native viewer reads account credentials. Use the desktop's normal pin action.

## 设置和限制 / Settings and limits

- 接管已连接设备默认关闭；需要时启用，使用已有的 `force_join` 参数。
- 字体与固定 1920×1080 解码画布按设备保存，重新连接后生效。
- 输入和剪贴板完全使用原生 viewer；本组件不加载输入适配库，也不通过文件/终端传送图片。
- X11/XWayland 恢复最小化窗口曾实机验证；原生 Wayland 的前台激活仍未验证。
- 两个控制端之间的接管仅验证了参数和目标路由，尚未完成端到端验收。
- 自定义 `UU_CONFIG`、XDG 配置路径和语言会传入用户服务；配置中保存的 viewer 路径必须继续存在。

Takeover is off by default and uses the existing `force_join` argument when enabled.
Per-device font and optional fixed 1920×1080 decoder-canvas settings apply after
reconnecting. Input and clipboard use the native viewer without adapters or
file/terminal image workarounds. Window restoration was live-tested on X11/XWayland;
native Wayland activation and two-controller takeover remain unverified (argument
and target routing are tested). Custom `UU_CONFIG`, XDG paths and language are
forwarded to the user service. The stored viewer executable path must remain valid.

## 验证 / Validation

```sh
python3 -m unittest discover -s contrib/independent-viewer -p 'test_*.py' -v
shellcheck contrib/independent-viewer/install.sh
```

测试使用虚构设备，不连接账号。GTK smoke tests 同时检查默认入口隐藏、主动启用后的
两种动作路由和已有主题/语言设置。测试不记录按键、设备凭据或剪贴板内容。

Tests use fixture devices without account connections. GTK smoke tests check that
actions are hidden by default, both opt-in actions route correctly, and existing
theme/language settings work. No keystrokes, credentials or clipboard contents are logged.
