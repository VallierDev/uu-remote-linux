# 实验性防窥 / Experimental privacy screen

仅支持 KDE Plasma Wayland（portal 后端）。开启后，采集和输入切换到 KWin 虚拟输出
（portal ScreenCast 的 VIRTUAL 源），再用 `kscreen-doctor` 禁用物理屏；远程操作不会点亮
本机屏幕。关闭、最后一个桌面连接断开或正常退出时，先重新启用物理屏，再结束虚拟输出、切回物理屏采集。
顺序参考 KRdp 虚拟显示器的 replace 策略和 Sunshine 的 KDE 方案：创建虚拟输出前记录物理屏，
从不禁用虚拟输出本身（KWin bug 527747）。GNOME、其他 Wayland 桌面和 X11 不支持。

壁纸模式：窗口随物理屏禁用迁到虚拟输出后，重新启用物理屏，把虚拟输出设为主屏、用 KWin 脚本把窗口
留在虚拟输出上，再由 `privacy-wallpaper` 子进程在每块物理屏上全屏置顶显示 Plasma 当前桌面壁纸和
“此设备正在被远程控制”。物理屏开着时 uinput 绝对坐标覆盖整个布局，主机按虚拟输出在布局中的位置换算。
需要 GTK3。

KDE Plasma Wayland only (portal backend). Enabling moves capture and input to a KWin virtual
output (portal ScreenCast VIRTUAL source) and disables the physical outputs with `kscreen-doctor`,
so remote input cannot wake them. Disabling, the last desktop disconnect or a normal exit re-enables
the physical outputs first, then ends the virtual share and returns to the monitor. Physical outputs
are recorded before the virtual one exists; the virtual output itself is never disabled (KWin bug
527747). GNOME, other Wayland desktops and X11 are unsupported.

Wallpaper mode: after the windows have moved to the virtual output, the physical outputs are turned
back on as secondary outputs, a KWin script keeps the windows on the virtual one, and the
`privacy-wallpaper` child process covers each physical output with Plasma's desktop wallpaper and a
notice (fullscreen, keep-above). uinput's absolute axes then span the whole layout, so pointer input
is aimed at the virtual output's region. Needs GTK3.

注意 / Notes:

- 虚拟屏分辨率由 KDE 决定（默认 1920x1080，首次可能在本机弹窗选择，之后沿用保存的授权
  `portal-permissions/kde-virtual`）。窗口会被 KWin 移到虚拟屏，恢复后位置可能变化。
  The virtual size is KDE's (1920x1080 by default; the first share may ask on the host, later ones
  reuse `portal-permissions/kde-virtual`). KWin moves windows to it; positions may change on restore.
- KDE 不处理虚拟输出共享上的 portal 输入，防窥期间指针、按键和文字改走 uinput（需要 `/dev/uinput`
  权限，见 README）；手机拖动手势在此期间按普通鼠标拖动处理。
  KDE ignores portal input on a virtual-output share, so pointer, keys and text use uinput while the
  privacy screen is on (`/dev/uinput` access required); mobile drag gestures act as plain mouse drags.
- 开启期间通过 KWin 的 InputDevice 接口禁用本地键盘、鼠标、触摸板和触摸屏（含手势守护进程、ydotoold
  等由本机驱动的虚拟设备），保留电源键、合盖开关和主机自己的 uinput 设备；`UU_PRIVACY_BLOCK_INPUT=0` 关闭此项。
  KWin 会保存设备开关，所以主机另起 `privacy-guard` 子进程：主机以任何方式结束（包括 SIGKILL）都会按恢复记录
  重新启用设备和物理屏；下次启动也会重试。防窥期间新插入的设备不会被禁用。
  While on, local keyboards, pointers, touchpads and touchscreens are disabled through KWin's
  InputDevice interface (including software devices fed by local hardware such as gesture daemons and
  ydotoold); the power button, lid switch and the host's own uinput devices stay. Set
  `UU_PRIVACY_BLOCK_INPUT=0` to keep local input. KWin saves the device flag, so a `privacy-guard`
  child restores devices and outputs from the record whenever the host ends, including SIGKILL; the
  next start retries too. Devices plugged in during privacy are not blocked.
- 自动开启：`UU_PRIVACY_AUTO=picture`（或 `2`）在第一个桌面控制端连上后自动启用隐私屏保，
  `UU_PRIVACY_AUTO=screen-off`（或 `3`）自动关屏；默认关闭。远程协助（访客）的桌面会话同样适用。
  控制端自己的请求优先：一次会话里只要客户端发过隐私请求（包括连接刚建立、会话计数还没开始的
  那一刻），待执行的自动开启就不再触发；断开后照常恢复。主机不支持或依赖缺失时只记日志，不影响连接。
  自动开启后主机会向控制端推送一次 SystemMetrics 和 SystemStateChange，控制端菜单随之更新。支持的主机上，
  每次桌面连接也会先推送一次当前状态（关闭时为关闭），这是新增的主动上报。
  Auto: `UU_PRIVACY_AUTO=picture` (or `2`) enables picture mode when the first desktop controller
  connects; `screen-off` (or `3`) turns the physical screens off. Off by default. Remote-assistance
  (guest) desktop sessions are covered too. The viewer's own request wins for that session, including
  one that arrives right after the connection opens and before the session counter starts: a pending
  auto-enable then does not run, and a disconnect restores as usual. Unsupported hosts or missing
  tools only log; the connection is unaffected. After an auto-enable the host pushes SystemMetrics
  and SystemStateChange once, so the viewer's menu follows; on supported hosts every desktop
  connection also pushes the current state first (off when it is off), which is new outbound traffic.
- 这是尽力而为的遮挡，不是保密边界。/ This is best-effort obscuring, not a confidentiality boundary.
- 恢复记录保存在配置文件旁的 `privacy-power.json`（0600）；崩溃后下次启动重新启用记录的物理屏。
  若主机无法启动，可在本机终端执行 `kscreen-doctor output.<name>.enable`。
  The recovery record `privacy-power.json` (0600) re-enables the recorded outputs at the next start
  after a crash; manually: `kscreen-doctor output.<name>.enable`.
- 需要 coreutils `timeout`、`gdbus` 和可用的 uinput（`/dev/uinput` 可写，且未设 `UU_INPUT_BACKEND=native`）；
  缺少时请求在改动屏幕和设备之前被拒绝。隐私模式的遮挡窗口建好后才算开启，失败即回滚。
  Requires coreutils `timeout`, `gdbus` and working uinput (`/dev/uinput` writable, not
  `UU_INPUT_BACKEND=native`); otherwise the request is refused before outputs or devices change.
  Picture mode counts as on only after the cover window reports ready; a failure rolls back.

## 协议 / Protocol

字段号来自官方 Mac 客户端的 SwiftProtobuf name map / Field numbers from the Mac UURemote name maps:

- RPC 7 `PrivateScreenSettingRequest {1 enable, 2 privacy_type, 3 setting_source}` → RpcResponse 3
  `{1 error_code, 2 req_privacy_type, 3 cur_privacy_type}`；PrivateScreenType 2 隐私/picture, 3 关屏/screen off。
  主机先应答再切换（客户端约数秒超时，报 9998）；切换失败发 ReportError -7。
  The host answers before switching (the viewer times out with 9998); a failed switch sends ReportError -7.
  成功不发送 ReportError；Mac 4.43.0 的 -9 是系统会话变化提示，不是防窥成功通知。
  Success does not send ReportError; -9 in Mac 4.43.0 reports a system session change, not privacy success.
- RPC 25 `update_screen_saver_req` → RpcResponse 21：开启隐私模式前发送，主机回空成功应答，画面仍用本机壁纸。
  Sent before picture mode; answered empty (success), the host keeps its own wallpaper.
- FeatureFlag 4 `ff_private_screen` = 2 (ENHANCED_PRIVACY)；SettingMetric 4 `private_screen`、7 `private_screen_type`
  上报当前实际模式（1 关闭、2 屏保、3 关屏），不使用 host_settings.json 中的历史选项；SystemStateChange 5
  `PrivateScreenState` 在状态变化时推送。
  SettingMetric 4/7 report the effective mode (1 off, 2 picture, 3 screen off), not the saved preference;
  SystemStateChange 5 is sent after a switch.

Mac 4.43.0 从 CONTROL 发送请求，但只在 TEXT 接收分支处理上述 RPC 应答与 SystemMetrics。
防窥与屏保应答走 TEXT；SystemMetrics 同步发送到 TEXT，并保留 CONTROL 上报以兼容其他客户端。
每个 peer 独立排队，TEXT 打开后才发送；待发 SystemMetrics 只保留最新一条，其他应答最多 64 条，
按入队顺序发送。满队列时丢弃新增应答并记录一次警告，不阻塞 CONTROL 请求处理。
SystemMetrics 优先读取非零的 field 7，仅在其为 0 时回退
到 field 4。SystemMetrics field 1 `metrics` 必须列出本次报告的类型（general=1、setting=2），
否则客户端直接忽略消息内容。已用原签名 Mac 4.43.0 客户端验证三种模式的安全浮层回显，
关屏后断开恢复物理屏与输入设备，再次连接回显“不启用”。
Mac 4.43.0 sends requests on CONTROL, but handles these RPC responses and SystemMetrics only on TEXT.
Privacy/screensaver replies use TEXT; SystemMetrics are also sent on TEXT, retaining CONTROL for other
viewers. Each peer queues messages until its TEXT channel opens, retaining only the latest pending
SystemMetrics and up to 64 other replies in FIFO order. Excess replies are dropped with one warning
per peer, without blocking CONTROL request handling. Nonzero field 7 takes precedence over
field 4. SystemMetrics field 1 `metrics` must list the reported sections (general=1, setting=2), or the
viewer ignores the payload. The original signed Mac 4.43.0 viewer was verified for all three labels,
physical-screen/input restoration on disconnect, and an off label after reconnecting.

官方主机侧依据：原签名 Mac `UURemoteServer` 4.43.0（626），arm64 UUID
`7E10CB24-F516-3728-8375-BA5EC124A2C5`。`SystemMetricsHandler.handleSystemMetrics`（`0x10000a96c`）
按请求填充各节，general 分支在 `0x10000abe8` 追加 `1`，setting helper（`0x10000a230`）
在 `0x10000a64c` 追加 `2`；`GvPb_SystemMetrics.traverse`（`0x100423840`）将列表按 packed enum
写入 field 1。发送链 `0x10000b11c → 0x10018e948 → ControlledSendTextData:data:` 使用 TEXT。
这是静态主机代码依据：请求 general 和 setting 时得到 `[1,2]`，不是抓包，也不表示每条响应都固定
包含两节或证明官方 CONTROL 报文。本实现总是报告两节，因此列出 `[1,2]`。
Official host evidence: the original signed Mac `UURemoteServer` 4.43.0 (626), arm64 UUID
`7E10CB24-F516-3728-8375-BA5EC124A2C5`. `SystemMetricsHandler.handleSystemMetrics` (`0x10000a96c`)
fills the requested sections, appending GENERAL=1 at `0x10000abe8` and SETTING=2 at `0x10000a64c`
in helper `0x10000a230`. `GvPb_SystemMetrics.traverse` (`0x100423840`) encodes that list as packed
field 1. The send chain `0x10000b11c → 0x10018e948 → ControlledSendTextData:data:` uses TEXT.
This is static host-code evidence: requesting both sections produces `[1,2]`; it is not a packet
capture or proof that every response includes both sections or that the official host sends it on
CONTROL. Our payload always reports both sections, so it lists `[1,2]`.

Android 4.43.0 已确认打开 TEXT；主机日志和手机端人工验收确认，关屏、隐私屏保、取消三种操作的
实际模式与菜单选中状态均正确。Windows/iOS 未在本轮验收。
Android 4.43.0 opens TEXT; host logs and manual phone-side acceptance confirmed that screen-off,
picture and off operations update both the effective mode and the selected menu item.
Windows/iOS were not tested in this round.

## 本机诊断 / Local diagnostic

禁用物理屏 5 秒后恢复，不登录、不启动第二个主机；先停止使用同一配置目录的主机。
Disables the physical outputs for five seconds, then restores them. Stop the host first.

```sh
cargo build --locked --features wayland,portal --bin uuremote-server
target/debug/uuremote-server privacy-screen-probe
UU_PRIVACY_PROBE_WALLPAPER=1 target/debug/uuremote-server privacy-screen-probe   # 壁纸模式 / wallpaper mode
```

## 代码结构 / Code layout

`src/privacy_screen/` 分两层：与桌面无关的控制器和共享部件，以及每种桌面一个后端。
The module has a desktop-independent controller with shared parts, and one backend per desktop.

| 文件 / File | 职责 / Responsibility |
|---|---|
| `mod.rs` | 控制器：桌面会话计数、模式切换（先恢复再进入）、失败回滚、崩溃恢复、probe / controller: sessions, mode switch (leave, then enter), rollback, crash recovery, probe |
| `mode.rs` | `Mode { Picture, ScreenOff }` ↔ 协议 `privacy_type` 1 关 / 2 隐私 / 3 关屏 |
| `protocol.rs` | RPC 7 请求解析与 RpcResponse 3 编码 / request parsing and response encoding |
| `backend.rs` | `Backend` trait 与按桌面选择后端 / the trait and backend selection |
| `record.rs` | 恢复记录 `{backend, mode, data}`、进程锁、`Journal` / recovery record, process lock, journal |
| `guard.rs` | `privacy-guard` 子进程：主机以任何方式结束都按记录恢复 / restores from the record however the host ends |
| `cover.rs` | `privacy-wallpaper` 子进程：GTK 全屏遮挡窗口 / GTK cover window process |
| `tool.rs` | 带超时的外部命令、gdbus、等待生效 / bounded tool calls, gdbus, wait-until |
| `kde/` | KDE 后端：`outputs.rs` kscreen-doctor 布局，`input.rs` KWin 输入设备，`kwin.rs` KWin 脚本，`mod.rs` 编排 |

新增桌面（X11、GNOME…）只需实现 `Backend` 并在 `backend::detect` / `backend::named` 登记：
`enter` 在每一步改动之前通过 `Journal::save` 写入足以撤销它的数据；`leave` 只依据这些数据撤销，
因为崩溃恢复时（守护进程或下次启动）没有桌面视频流。Picture 模式可复用 `cover::show`；
若物理屏保持开启而 uinput 坐标覆盖整个布局，返回 `Entered::pointer_region`。

To add a desktop (X11, GNOME, ...), implement `Backend` and register it in `backend::detect` and
`backend::named`. `enter` saves, through `Journal::save`, what undoes each step before making it;
`leave` must undo from that data alone, because crash recovery (guard or next start) has no
desktop stream. Picture mode can reuse `cover::show`; when physical outputs stay on and uinput's
axes span the whole layout, return `Entered::pointer_region`.
