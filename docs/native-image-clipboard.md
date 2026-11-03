# 原生图片粘贴 / Native image paste

复制 PNG 截图后，聚焦 Mac 远控窗口并按 **Ctrl+V**（或 Command/Win+V）。
Viewer 按需读取本地图片，经当前会话的原生剪贴板通道传输，确认所有数据块后
发送一次平衡的 Command+V。不需要图片开关、远端终端或文件工具，也不重连；不会发送 Enter。

Copy a PNG screenshot, focus the Mac session, and press **Ctrl+V** (or Command/Win+V).
The viewer reads PNG bytes on demand, transfers them over the existing native clipboard
channel, then sends one balanced Command+V after every block is acknowledged. No separate
image setting, remote terminal, file tools, temporary files or reconnect is involved.
It never sends Enter or submits a chat.

## 范围 / Scope

- Linux → official Mac host; one PNG, up to 32 MiB encoded and 100 million pixels.
  Files, image lists, JPEG-only selections and reverse image synchronization are not implemented.
- No PNG: retain the normal text/key path. Read/transfer failure: suppress paste and show an
  in-window message, so the previous remote image is not pasted accidentally.
- Focus loss, menu activation, a new clipboard selection or closing the window cancels the
  pending paste and releases held keys. View-only sessions and `UU_VIEWER_CLIPBOARD=0` do not read images.
- X11/XWayland: native Xlib selection read through an unmapped requestor on a separate connection;
  supports ICCCM INCR, with a five-second deadline and a total byte limit. No polling subprocess/window.
- SDL2 native Wayland: `wl-paste --type image/png --no-newline` is used only on explicit paste;
  install `wl-clipboard`. This path needs compositor-specific acceptance. In particular, a
  compositor/helper-induced focus loss cancels the paste. It does not reintroduce periodic
  `wl-paste` polling from issue #37. SDL3 MIME access would be a future backend improvement.

目前实机确认的是官方 Mac 接收与 X11/XWayland 控制端路径。SDL2 原生 Wayland
按需读取仍需各桌面验证，辅助进程引起失焦时会取消本次粘贴。

## 协议证据 / Protocol evidence

Local analysis used the installed official `UURemoteServer` with SHA-256
`37f45ebafe3e0d87a692715c9eeedf488e581ec8f66b7a62dc948d9845f1ca9d`.
No official binary or assets are included here. Its x86-64 paths enumerate pasteboard
item types (`0x10018a800`), read raw type data (`0x100188310`), and write received
multi-format data through `setData:forType:` (`0x1001896a0`). Wire acceptance was then
checked against the running official host with generated PNG fixtures.

The existing rich text message shapes are reused on `TEXT_DATA_CHANNEL`, with clipboard
feature version 3:

| Operation | Wire fields |
|---|---|
| Format list | Envelope 21 → RPC field 9 → clipboard case 1 → repeated format field 1; format id field 1 = 0, name field 2 = `public.png` |
| Format ACK | Envelope 22 → RPC field 5 → clipboard case 1; correlate the RPC header id |
| Data ask | Clipboard request case 2: format id field 1 = 0, block key field 2, name field 3 = `public.png` |
| Data confirmation | Clipboard response case 2: status field 1 = 1, matching key field 2, block count field 3 |
| Data block | Clipboard request case 3: key field 1, **one-based index** field 2, raw PNG bytes field 3 |
| Block ACK | Clipboard response case 3: matching key field 1, same index field 2, success field 3 = 1 |

PNG blocks are 32 KiB, keeping each message below the normal 64 KiB SCTP message size.
The writer has bounded buffering and backpressure; the receiver continues processing ACKs
while blocks are sent. Only matching, successful, unique ACKs for every block release
paste. The original text encoding and host-side text-only behavior are retained.

关键发现：零起始编号也会收到块回执，但本次 Mac 不会应用最终图片；图片必须从 1
开始编号。生成的 512×384 PNG（787,124 字节，25 块）经原生通道传输后，在 Mac
剪贴板上的 SHA-256 与源图一致，且 `NSImage` 验证可解码。检查只返回布尔值、大小
和格式元数据，不保存用户剪贴板内容到仓库或日志，不执行聊天发送。

A generated 512×384 PNG (787,124 bytes, 25 blocks) was received byte-for-byte on the
Mac pasteboard: SHA-256 matched and `NSImage` accepted it. The same payload with
zero-based blocks was ACKed but left the old pasteboard image in place. Thus transport
ACKs alone were insufficient evidence until the one-based sequence was validated.
Testing used synthetic data; the prior Mac pasteboard was backed up locally on the Mac
and restored after validation. No user clipboard contents enter source or logs.

## 本地验证 / Checks

```sh
cargo test --locked
cargo test --locked --features native-viewer,viewer-sdl --bin uuremote-viewer
cc -Wall -Wextra -Werror c_engine/viewer_clipboard.c c_engine/viewer_clipboard_test.c -lX11 -o /tmp/viewer-clipboard-test
xvfb-run -a /tmp/viewer-clipboard-test
```

The Xvfb test owns an isolated selection and checks normal, 700 KB INCR, oversize and
unsupported conversions. Rust checks cover PNG bounds, block ordering/ACK correlation,
text compatibility, delayed paste cancellation and balanced Mac modifier release.
