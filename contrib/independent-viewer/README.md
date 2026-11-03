# 独立会话与 Mac 兼容设置 / Independent sessions and Mac compatibility

这是可选的独立会话组件。账号登录方式和网络协议保持原样，
辅助脚本按设备保存配置，复用已登录的 `uuremote-viewer`。

This optional independent-session companion reuses the
signed-in viewer, stores per-device settings, and does not modify its network protocol.

## 安装 / Install

Linux 图形用户会话需要 Python 3、systemd 用户服务；设置窗口需要 `python3-gi` 和 GTK3。
图片传送额外需要 `wl-clipboard`、`notify-send`，Mac 上需允许 UU 文件传输和终端。
按键适配器从本目录 C 源码编译，需要 `cc`；不包含预编译二进制。

Run as the desktop user, with Python 3 and a systemd user manager. The settings
window needs PyGObject/GTK3. Image transfer needs wl-clipboard, notify-send, and
UU file/terminal access to the Mac. A C compiler builds the optional input adapter.

```sh
./contrib/independent-viewer/install.sh
```

从源码编译并安装带新入口的主窗口 / Build and install the main window:

```sh
cargo build --release --locked --features native-viewer,viewer-sdl --bin uuremote-viewer
./contrib/independent-viewer/install.sh --viewer target/release/uuremote-viewer
```

`--viewer` 把本地构建装到 `~/.local/lib/uuremote-independent/`，更新本用户的应用菜单入口，
备份此前的桌面入口；系统包中的程序仍保留。下次从应用菜单打开控制端即可。

`--viewer` installs the locally built executable under `~/.local/lib/uuremote-independent/`
and updates the user's launcher, keeping a backup of the previous desktop entry.
It leaves the system package executable in place. Reopen the viewer from the app menu.

主窗口设备页提供 **独立远控**、**会话设置** 和 **传送截图**；常规设置里也可配置当前所选设备。
配置后，从主窗口或应用菜单打开该设备。独立会话有自己的桌面应用标识，
重复打开会恢复同一窗口，关闭主窗口不会结束独立会话；明确退出账号会关闭这些会话。
Dock 的固定功能仍由桌面环境管理。

The device page adds **Independent remote**, **Session settings**, and **Send screenshot**.
General settings can also configure the selected device. Each device has its own
desktop identity; reopening reuses its session, closing the launcher preserves it,
and explicit sign-out stops independent sessions. Use the desktop's normal pin action.

也可从命令行配置 / Command-line setup:

```sh
~/.local/bin/uuremote-independent configure DEVICE_ID 'Remote Mac' 4
~/.local/bin/uuremote-independent open DEVICE_ID 'Remote Mac' 4
```

平台值：Mac `4`，Windows / Linux `1`。设备 ID 不是密码；登录凭据仍只由原程序读取。
配置位于 `$XDG_CONFIG_HOME/uuremote/independent/`（默认 `~/.config/...`，目录 0700、文件 0600），
桌面入口位于 `$XDG_DATA_HOME/applications/`，不在源码中写入账号或设备信息。

Platform is `4` for Mac, `1` for Windows/Linux. The stock viewer alone reads the
account credentials. Config files are private (0600, in a 0700 directory); generated
desktop entries contain a hashed configuration key, not account credentials.

## 可选设置 / Options

- 接管已连接的设备：默认启用，使用原程序已有的 `force_join=true`，可接管另一台
  电脑或手机的远控连接。关闭后，已占用的设备仍由原控制端保持连接。
- 字体文件：供 SDL 窗口和控制中心显示中文。自动选择可用的文泉驿 / Noto CJK 字体，
  也可指定覆盖所需字符的字体。不会改动系统 fontconfig。
- 固定 1920×1080 解码画布：针对某些自动尺寸解码停留在加载画面的情况；需要时启用。
  不强制更改远端屏幕分辨率。
- 按键长按连发：字母、数字、空格、普通符号、小键盘字符及 Backspace 使用本机重复速度连续输入；
  保留 Shift 大写/符号，松键或失焦即停止。字符快捷键不连发，Enter/Tab/Escape/功能键不额外连发。
- 常用 Ctrl 快捷键对应 Command：A/C/V/X/Z/F/S/O/N/P/W/L/T/R，保留 Shift，
  保留 Ctrl+Alt 和其他未列出的组合。不按远端应用识别终端：启用后 Ctrl+C 是 Command+C，
  要发送真实 Control+C，请在设置中关闭该选项。
- 图片传送：启用后，在该设备的 Dock / 应用菜单入口右键选择“传送剪贴板图片到 Mac”。

Options select a CJK font, an optional fixed decoder canvas, held-key repeats,
PC-style editing shortcuts, and a manual image-transfer action. Mapped Ctrl chords
use Command, including in a terminal; disable the option to send literal Control+C.
Unlisted, Ctrl+Alt, and physical Win/Command chords keep their existing mapping.
Held-key repeat covers letters, digits, space, punctuation, printable keypad keys
and Backspace at the local repeat rate, including Shift. Release/focus loss stops
repeats. Character shortcuts and Enter/Tab/Escape/function keys do not gain repeats.
Existing `backspace_repeat` settings also control typing repeats for compatibility.
Taking over an occupied device is enabled by default using the viewer's existing
`force_join=true` argument. Disable it to keep another computer/phone's session connected.

Mac 入口默认核对每个按键的左右 Shift 状态，补齐输入法或切换焦点时漏掉的按下/松开，
使 `Shift+2`、大写字母等组合按正确顺序传输。`@` 对应 Mac 的 ABC / US 键盘布局；
其他远端布局仍按物理键解释。该恢复不依赖“本机快捷键习惯”或“Backspace 连续删除”开关。
独立窗口还单独设置 `XMODIFIERS=@im=none`，隔离本地 XIM；中文输入继续使用远端输入法。

Mac entries reconcile left/right Shift snapshots before each key, recovering a
missing press or release after an IME/focus change. `Shift+2` yields `@` with the
remote ABC/US layout; other layouts retain their physical-key behavior. Recovery
does not require the optional PC shortcuts or held-Backspace switches. Each independent
window sets `XMODIFIERS=@im=none` to bypass local XIM; use the remote IME for Chinese.

**按键适配仅支持 Debian 包版本 0.2.45，且只对已配置的 Mac 入口加载。**
其他版本保留独立窗口功能，按键适配不加载。原程序二进制和全局键盘设置不被修改。
v0.2.45 已有的本地输入法吞键修复是上游改动（PR #150），本组件不重复实现。

**The SDL2 input adapter is gated to Debian package 0.2.45 and configured
Mac entries.** Other package versions run independent sessions without the adapter.
The stock binary and global keyboard settings are not patched. Upstream PR #150's
IME fix is already included in v0.2.45 and is not reimplemented here.

## 图片流程和限制 / Image flow and limits

1. 在 Linux 上复制 PNG 截图或图片。
2. 在主窗口选择 Mac，点击 **传送截图**（或右键设备入口选择 **传送剪贴板图片到 Mac**）。
3. 等待通知；远控会短暂重连。然后在 Mac 聊天框中右键 **粘贴**，或启用本机快捷键映射后按 Ctrl+V。

直接 Ctrl+V 不会把 Linux 图片传到 Mac；它只粘贴 Mac 当前剪贴板。先在“会话设置”启用图片传送。
应用菜单显示原名 UU Remote Linux Viewer，中文主窗口保留“非官方”标记；安装器沿用独立桌面 ID，隐藏本用户的旧菜单项，保留系统原包。

Enable clipboard images in Session settings. Copy a PNG, select the Mac in the
viewer, click **Send screenshot**, wait for completion, then paste in
the Mac chat box. Ctrl+V alone only pastes the Mac's current clipboard. The installer
uses a distinct desktop entry and hides the user's old menu item while
preserving the system package executable. The app keeps the original UU Remote Linux Viewer name and unofficial window title.

The manual action transfers the current PNG through UU's file tool, then uses the
Mac terminal tool and AppKit's general pasteboard. It checks the image was accepted,
removes temporary files, and restores the independent desktop. It never sends
Enter or submits a chat. The native UU clipboard protocol remains text-only.

当前官方 Mac 被控端在独立工具关闭时会发出共享的 leave 通知，因此脚本会明确暂停桌面，
在工具完成后恢复。不是无缝双向图片同步。需要 Mac 已登录、终端可用；不用于锁屏密码框。
仅处理 PNG（最大 32 MiB），不处理文件、视频或图片列表。窗口唤回已验证 X11/XWayland；
纯 Wayland 的前台激活取决于桌面环境，未完成实机验证。

Official Mac tool shutdown was observed to release the concurrent desktop, so the
script explicitly pauses/restores that session. This is not seamless bidirectional
image synchronization. It needs a logged-in Mac and working terminal access, and is
not for password prompts. PNG only, up to 32 MiB. X11/XWayland window restoration
was verified; native Wayland activation remains unverified.

## 验证 / Validation

```sh
python3 -m unittest discover -s contrib/independent-viewer -p 'test_*.py' -v
cc -Wall -Wextra -Werror -shared -fPIC contrib/independent-viewer/sdl-input-compat.c -ldl -o /tmp/uu-input-test.so
SDL_VIDEODRIVER=dummy UU_VIEWER_BACKSPACE_REPEAT=1 UU_VIEWER_PC_SHORTCUTS=1 UU_VIEWER_SHIFT_SYNC=1 \
  LD_PRELOAD=/tmp/uu-input-test.so python3 contrib/independent-viewer/check-key-repeat.py
```

测试使用虚构设备和 SDL dummy 窗口，不连接账号、不读取真实剪贴板、不发送真实按键。
当前实机验证范围：GNOME Wayland/XWayland 控制官方 Mac，独立窗口关闭/重开、
中文字体、固定解码画布、图片写入及重连；用户确认修复后 `@` 和大写字母输入正常。
按键状态转换另有 SDL 队列检查。

Tests use fixture devices and SDL's dummy video backend, never a real clipboard
or keyboard. Live acceptance covered GNOME Wayland/XWayland to an official Mac,
independent window lifecycle, CJK font/canvas compatibility, and image pasteboard
write/reconnect. The user confirmed that `@` and uppercase letters work after the
fix was installed. Shortcut/modifier sequences are covered by real SDL queue tests.
Shift recovery covers both sides, `Shift+2`, A–Z, missing press/release, and focus
changes (113 scenarios). Takeover/target routing is checked with fixture devices;
an end-to-end two-controller takeover test remains pending.
