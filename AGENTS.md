# AGENTS.md — guide for AI coding agents

This file is written for large language model agents (Claude Code, Codex, Cursor, ...) that are asked to
**build, install, debug or develop** uu-remote-linux. Read it fully before acting. Human documentation is in
[README.md](README.md) (Chinese) and [README.en.md](README.en.md).

## 1. What this project is

- An **unofficial Linux host** for NetEase UU Remote (网易 UU 远程). The official Mac / Windows / mobile
  clients connect to it and control the Linux desktop. An experimental Linux viewer/controller, `uuremote-viewer`
  (`src/viewer_main.rs`, `src/viewer/`, feature `native-viewer`), connects to other hosts with the same protocol.
- One host binary, `uuremote-server` (Cargo package `uu-remote-linux`), plus small C engines in `c_engine/`
  compiled by `build.rs`. No Wine, no NetEase binaries.
- The host registers with the UU cloud as a **Windows PC** (`x-param-plat=1`) so the official clients accept it.
- License GPL-3.0. Not affiliated with NetEase.

## 2. Ground rules (must follow)

1. **Privacy in logs.** Never log keystrokes, terminal text, clipboard contents, file contents, passwords,
   tokens, QR login URLs or TURN credentials. Log sizes, counts, field numbers and booleans only. Existing code
   follows this; keep it that way in every new log line.
2. **The QR login URL is a credential.** Show the QR only to the human who owns the account. Never paste the
   URL or a QR screenshot into chats, issues, logs or commits.
3. **Follow the official host behavior.** Protocol behavior is copied from the official hosts (Mac
   `UURemoteServer`, Windows `GameViewerServer.exe`). Do not invent message shapes; when unsure, log the field
   numbers of what the viewer sends and derive behavior from evidence.
4. **No NetEase assets.** Do not add NetEase logos, icons, screenshots of their apps or copies of their binaries.
5. **No personal data in source.** Nothing machine- or account-specific is compiled in; credentials come from
   the config file / environment. The only built-in default is the public client signing key in `src/auth.rs`.
6. **Don't break other tools.** The desktop, file-transfer, terminal and port-mapping tools each open their own
   peer connection; a new offer must never close other peers.
7. **UI text is bilingual.** Every user-facing string has Chinese and English: `L("中文", "English")` in C
   (`c_engine/i18n.h`), `crate::i18n::s(..)` / `crate::tr!(..)` in Rust (`src/i18n.rs`; `UU_LANG`, saved choice, locale).
   Logs stay English.
8. Keep changes small and tested; match the surrounding code style (dense, comment only the non-obvious).

## 3. Build and test

Dependencies (Ubuntu/Debian):

```sh
sudo apt install build-essential pkg-config \
    libx11-dev libxext-dev libxtst-dev libxfixes-dev libavcodec-dev libavformat-dev libavdevice-dev libswscale-dev libclang-dev libgtk-3-dev ffmpeg
# --features inproc-av:      X11 in-process capture + native NVENC/VAAPI. NVENC headers are vendored and
#                            libcuda/libnvidia-encode dlopen'ed at runtime (NVENC API 12.1, driver >= 530).
#                            The VAAPI/DRI3 engines compile against
#                            libva-dev libgbm-dev libx11-xcb-dev libxcb-dri3-dev libxcb-damage-dev;
#                            libva/libva-drm themselves are dlopen'ed at runtime (no hard libva dependency)
# --features viewer-ffmpeg:  libavcodec-dev libavformat-dev libavdevice-dev libswscale-dev libclang-dev
#                            (FFmpeg 6.x only. FFmpeg >= 7 fails in ffmpeg-sys-next with "avfft.h file not found")
# --features wayland:        libwayland-dev libxkbcommon-dev wl-clipboard
# --features wayland,portal: + libglib2.0-dev libpipewire-0.3-dev libei-dev (libei >= 1.1)
```

Rust stable via rustup. Commands:

```sh
cargo test                                   # unit tests (macOS builds use C stubs, also fine for development)
cargo build --release                        # X11 host
cargo build --release --features wayland     # + Hyprland/wlroots
cargo build --release --features wayland,portal   # + GNOME/KDE Wayland via xdg-desktop-portal
cargo build --release --features native-viewer,viewer-sdl --bin uuremote-viewer   # viewer (needs libsdl2-dev)
```

- `build.rs` links X11/Xext/Xtst/Xfixes and `dl` (PAM is loaded at runtime with `dlopen`), and builds the GTK3
  login window only when `pkg-config gtk+-3.0` succeeds (`UU_NO_GTK=1` forces it off).
- The portal feature panics in `build.rs` if GIO/PipeWire development packages are missing.
- `vendor/webrtc-dtls` is a patched copy of the upstream crate (MIT/Apache); keep it as a path dependency.
- CI (`.github/workflows/ci.yml`) runs `cargo test --locked` on ubuntu-22.04. Pushing a `v*` tag runs
  `.github/workflows/release.yml`, which builds and publishes the x86_64 tarball; release notes come from
  `docs/release-notes/<tag>.md`.

## 4. Install on a user's machine (step by step)

1. Check the session: `echo $XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP $DISPLAY $WAYLAND_DISPLAY`.
   X11 → default build. Hyprland → `--features wayland`. GNOME/KDE on Wayland → `--features wayland,portal`
   (or advise the user to pick the "on Xorg" session at the login screen, which is the best-supported path).
2. Install dependencies and build (section 3). Copy the binary: `install -Dm755 target/release/uuremote-server ~/.local/bin/uuremote-server`.
   From a release tarball run its `./install.sh` as the desktop user instead: it installs into `~/.local`, writes the user unit
   (keeps an existing one) and installs FFmpeg with libx264 via sudo apt/pacman when missing (Fedora/openSUSE: prints the command).
   The .deb depends on `ffmpeg`. Without FFmpeg the viewer waits on 正在传输画面; the host checks at startup and on each desktop offer, logs an error and tells the desktop user (freedesktop notification, else opens its window with a red line; `assist::show_problem`).
3. **Login (needs the human).** Run `uuremote-server login` inside the graphical session. A GTK window shows a QR
   code; the user scans it with the UU Remote mobile app and confirms. Without a display use
   `uuremote-server login --no-gui` (terminal QR). Credentials are written to `~/.config/uuremote/config.json`
   (mode 0600). Do not read the token out of that file or print it.
4. Create a systemd user unit (`~/.config/systemd/user/uuremote-native.service`):

   ```ini
   [Unit]
   Description=UU Remote Linux host (unofficial)
   After=graphical-session.target
   PartOf=graphical-session.target
   [Service]
   Environment=RUST_LOG=info
   ExecStart=%h/.local/bin/uuremote-server
   Restart=always
   RestartSec=5
   [Install]
   WantedBy=graphical-session.target
   ```

   Same as packaging/deb. GNOME/KDE import DISPLAY / WAYLAND_DISPLAY and start graphical-session.target; do not hard-code
   DISPLAY there (it overrides the session's real one, XWayland is often :1); under default.target the unit can
   start before WAYLAND_DISPLAY is imported and fall back to X11. Desktops without that target (Xfce, i3) and the Xvfb
   virtual desktop need `WantedBy=default.target` plus an explicit `Environment=DISPLAY=...`; headless
   (`UU_DESKTOP_BACKEND=none` + linger) needs `WantedBy=default.target` only (GNOME on Xorg outside the session: `XAUTHORITY=/run/user/<uid>/gdm/Xauthority`).
   Find the real display/Xauthority with `loginctl show-session <id> -p Display` / `ps -o args -C Xorg`.
5. `systemctl --user daemon-reload && systemctl --user enable --now uuremote-native.service`
6. Verify: `journalctl --user -u uuremote-native.service -f` must show
   `Device status synchronized: ... availability=Some("ready")`. If credentials are missing or expired the
   service opens the QR window on the desktop by itself (disable with `UU_LOGIN_WINDOW=0`).
7. Ask the user to connect from the official client and check desktop, input, sound, file transfer, terminal.

Updating an installed binary: copy to a temp name and `mv -f` over the old one (a running binary cannot be
overwritten in place: "text file busy"), then restart the service. Restarting kills open viewer windows; the
viewer does not reconnect them automatically.

## 5. Runtime configuration

| Item | Meaning |
|---|---|
| `~/.config/uuremote/config.json` | user_id, device_id, client_id, system_id, machine_guid, token, csrf; optional `sign_key`, `ffmpeg_root`. `$UU_CONFIG` overrides the path |
| `~/.config/uuremote/host_settings.json` | persisted 安全 settings: auto_lock, keep_mute, mute_permanent; optional `video_encoder` (`auto`/`cpu`/`vaapi`/`nvenc`) and `video_codec` (`auto`/`h264`/`h265`), written by `set-encoder` / `set-codec` (0600, merge write). Non-`auto` beats `UU_VIDEO_ENCODER` / `UU_VIDEO_CODEC`. `set-encoder` and `set-codec` both send `SIGUSR2` to the pid in `host.pid` (same binary). Live desktop streams restart the encoder (`EncoderTask::restart`, sequence kept, first frame IDR) without dropping the peer. Codec preference is logged and applies on the next desktop stream. `SIGUSR2` does not re-probe. Hand edits: `kill -USR2 $(cat ~/.config/uuremote/host.pid)` |
| `UU_DESKTOP_BACKEND` | `auto` (default) / `x11` / `wayland` / `portal` / `kms` (experimental feature) / `none` (headless: no display; desktop offers ignored; tool peers get a nominal 1920x1080 screen + Allow permissions, which the port-mapping viewer requires) |
| `UU_X11_MULTISCREEN=1`, `UU_X11_OUTPUT` | X11: advertise each RandR monitor (`xrandr --listmonitors`, read once at start; primary or `UU_X11_OUTPUT` is id 0) through the same screen-switch path as `UU_WAYLAND_MULTISCREEN` (`wayland::outputs`/`select`). x11grab reads `display+x,y` at the monitor size, XTest/uinput absolute input adds the monitor offset, the in-process engine and `desktop_mode` resize are skipped (分辨率 scales). Needs two or more monitors |
| `UU_DESKTOP_RESIZE=0` | X11: the viewer's 分辨率 choice scales the encoded stream instead of changing the display mode. Default: `src/desktop_mode.rs` switches the primary output via xrandr (adds a CVT-RB mode when missing, re-applies once if mutter resets it), restores the original mode for resolution_type 3 and 2 s after the last desktop stream ends |
| `UU_NVENC_RGB=0` / `=1` | NVENC 4:2:0 without scaling takes packed BGRA/BGR0 directly (no CPU swscale). Default: on, except when a connected DRM connector belongs to an nvidia-bound card (desktop composited on the encoding GPU; #16 measured 82.6 vs 98 fps at 4K). `0` forces CPU conversion, `1` forces the direct path |
| `UU_TOUCH_INPUT` | Android/iOS controllers (connect header `controller_platform` 2/3) send a finger as a left-button drag at absolute positions. Default on the portal backend (gesture): per press, moving past 12 px within 450 ms scrolls (`NotifyPointerAxis` / EIS scroll, content follows the finger, `finish` on release), holding still selects with button 1 down, a short press clicks; needs no touchscreen consent. `touch`: replay as one finger on the portal touchscreen (`NotifyTouch*` / EIS touch; requested where `AvailableDeviceTypes` offers it; consents saved before touch support restore keyboard+pointer only, so the first mobile drag asks once per run in the background via `PortalSession::upgrade_touch`, swapped in only with the touchscreen granted for the same monitor; the saved consent is never removed first). `0`: plain mouse drags |
| `UU_INPUT_BACKEND` | `auto` (uinput on Wayland/portal when `/dev/uinput` is accessible, udev `uaccess` rule in README) / `uinput` / `native`. X11 keeps XTest (held-key autorepeat handling) |
| Mac viewer Cmd | With 「使用 macOS 快捷键」 off the Mac viewer sends Cmd=VK_LWIN 0x5B, Option=0xA4, Ctrl=0xA2; connect header `controller_platform`=4 is macOS. For Mac controllers Cmd is held back: Cmd+C/V/A/F/X/Z → Ctrl+key (terminal: Cmd+C/V → Ctrl+Shift+C/V), other combos/alone → Super |
| `UU_TERMINAL_SHORTCUTS=0` | disable Ctrl+V → Ctrl+Shift+V in focused X11 terminals (Mac Cmd and Ctrl both arrive as VK_LCONTROL) |
| `contrib/virtual-desktop/` | Xvfb + session (`UU_VIRTUAL_SESSION`, `UU_VIRTUAL_BUS=user|private`) for monitorless hosts; serve it with `DISPLAY=:99` |
| `UU_VIDEO_ENCODER` | `cpu`: libx264; `vaapi`: explicit Intel/AMD; `nvenc`: force; unset/`auto`: background probe, one synthetic IDR per candidate, 3 s each. While #33 is open, auto is NVENC then CPU when `/proc/driver/nvidia/version` or `/dev/nvidia0` exists; with no NVIDIA the only candidate is CPU and the probe is skipped. A session that starts before the probe finishes uses the legacy rule (NVENC if NVIDIA is present, else CPU). On `--features inproc-av`, a process-only failure of NVENC when the native engine would try it first on X11 is `native_unprobed` and keeps that legacy NVENC choice (Wayland, portal, and KMS fall back to CPU). The skipped CPU-only probe is logged `not probed (only candidate)`. A successful probe and that CPU-only skip stay cached while NVIDIA presence (`/proc/driver/nvidia/version` or `/dev/nvidia0`) matches the report. A later desktop connection whose presence differs treats the report as stale and starts one background re-probe (`warm_auto_probe`, non-blocking, one at a time) and still uses the cached encoder. A hardware-candidate failure (NVENC failed, chosen CPU) is cached for 10 minutes (`PROBE_FAILURE_TTL`); the next desktop connection after that starts the same re-probe and still uses the cached encoder. Explicit `cpu` / `vaapi` / `nvenc` (setting file or `UU_VIDEO_ENCODER`) does not start the probe. A later success selects NVENC again; another failure restarts the 10 minutes. `native_unprobed` is not a failure cache. Ignored when `host_settings.json` `video_encoder` is not `auto`. Explicit `vaapi` with 真彩 encodes 4:2:0 and reports that in CaptureSettingSync (field 3). VAAPI is not an auto candidate until #33 |
| `UU_VIDEO_CODEC=h264` | force H.264 even when the viewer negotiated H.265 (default: H.265 when both sides support it and the encoder is hardware). Same precedence as the encoder: setting file, then this variable, then auto |
| `UU_FFMPEG_ROOT` | FFmpeg prefix (`bin/ffmpeg`, libs in `lib/x86_64-linux-gnu`) |
| `UU_AUDIO=0`, `UU_AUDIO_SOURCE` | disable sound / choose the Pulse source (default `@DEFAULT_MONITOR@`) |
| `UU_LOGIN_WINDOW=0` | don't auto-open the login window |
| `UU_ENGINE_DIR` | directory holding `libuu-wayland.so` / `libuu-portal.so` (`--features engine-plugins`, used by release packages: engines are dlopen'ed only on Wayland sessions; also searched next to the binary, `../lib/uu-remote-linux`, `~/.local/lib/uu-remote-linux`, `/usr/lib/uu-remote-linux`) |
| `UU_WOL_HELPER=1` | experimental: report LAN info (`api-ipv4…/api/v1/device/wake_on_lan/info` {mac, ip, subnet_mask}) after sign-in and answer `wake_on_lan` pushes (`target_device` {mac, ip, subnet_mask}) with a magic packet, like the official Windows host (field names from its log strings; not verified end to end). `wake_on_lan` push key names are always logged |
| `UU_USE_PROXY=1` | honor HTTP/HTTPS proxy env vars for REST and WebSocket signaling, and also gather TURN relays over TCP/TLS through that proxy (`vendor/webrtc-ice/PATCHES.md`; chain-verified, name mismatch tolerated for IP TURN URLs); clients bypass env proxies by default |
| `UU_REMOTE_ASSIST=0`, `UU_ASSIST_ALLOW=1`, `UU_ASSIST_UI=tty`, `UU_ASSIST_DEBUG=1` | disable assistance / allow at start / terminal UI for `assist` / include the server's short error message in assist errors |

Token has no expiry field; API code 1120 (HTTP 422) means invalid/expired → re-login. `config.json` is parsed once
(`config::file()`); only a `LoginExpired` room answer re-reads it from disk (`config::read_fresh()`) before waiting
for a new QR login.

## 6. Code map

| Path | Role |
|---|---|
| `src/main.rs` | entry, subcommands, room loop (one peer per `ice_id`), room renewal (deferred while connected) |
| `src/signaling.rs` | Engine.IO v4 / Socket.IO client; `be-controlled` binary attachment = ConnectOptions (field 1 capture_type) |
| `src/auth.rs`, `src/config.rs`, `src/login.rs`, `src/login_ui.rs` | signed REST client, config, device init + QR login, GTK window |
| `src/webrtc.rs` | peer connection, data channels, CONTROL dispatch, initialization messages |
| `src/video.rs`, `src/capture_setting.rs`, `src/video_track.rs`, `src/hevc.rs` | FFmpeg H.264/H.265 pipeline, one video track for both codecs (RFC 7798 HEVC payloader); viewer quality/fps/resolution/chroma settings |
| `src/locked_video.rs` | low-rate synthetic black video on the negotiated track while Portal is pending and locked; enables iPhone cold auto unlock (#29), requires FFmpeg libx264/libx265 for the selected codec |
| `src/audio.rs` | Pulse/PipeWire monitor → Opus audio track |
| `src/input.rs`, `src/input_mapping.rs`, `src/uinput.rs` | input injection (X11 XTest / Wayland / portal / uinput), Windows VK → keysym → evdev |
| `src/cursor.rs` | pointer shape mirroring (X11) |
| `src/clipboard*.rs` | text clipboard bridge and protocol |
| `src/file_trans.rs` | FileTransferFtp RPC |
| `src/terminal.rs` | TERM frames + PTY sessions |
| `src/session_lock.rs`, `c_engine/pam_check.c` | lock state reporting, auto unlock via PAM + logind |
| `src/system_actions.rs`, `src/system_metrics.rs` | SimpleAction system commands, mute, auto-lock, SystemMetrics |
| `src/initialization.rs`, `src/control_protocol.rs`, `src/protocol.rs` | protobuf encoders/decoders for CONTROL |
| `src/session_ice.rs`, `src/network_diagnostics.rs` | cloud ICE servers, address-free path diagnostics |
| `src/desktop_backend.rs`, `src/wayland.rs`, `src/portal*.rs` | backend selection, Hyprland, GNOME/KDE portal |
| `src/engine_plugin.rs` | `engine_ffi!`: engine C functions linked statically, or dlsym-resolved from runtime plugins with `engine-plugins` |
| `c_engine/portal_eis.inc` | scaled GNOME input through the existing Portal session's EIS connection; all input must use EIS once connected |
| `c_engine/x11_engine.c`, `c_engine/clipboard.c` | X11 capture/input/selection |
| `c_engine/wayland_engine.c`, `c_engine/portal_engine.c` | Wayland and portal engines |
| `src/assist.rs`, `c_engine/assist_window.c` | remote assistance (guest + account) and the unified GTK main window (assist + account/QR card) |
| `src/viewer_main.rs`, `src/viewer/` | experimental viewer (`--features native-viewer,viewer-sdl`): cloud join, WebRTC offer side, SDL desktop window (ffmpeg CLI decode), files/terminal/portmap/power/assist tools, device removal (`POST /api/v1/device/{id}/unbind`, verified live; shared devices use `/api/v1/device/share/{id}/unbind`, strings only), GTK launcher |
| `c_engine/viewer_window.c`, `c_engine/viewer_tools.c` | viewer GTK launcher and tool windows (files, port mapping, terminal list) |
| `src/viewer/hud.rs` | in-window chrome drawn on the SDL canvas: loading screen, control center (Ctrl+Alt+M; capture settings RPC 2 and SimpleActions 14/16/18/19 as the official viewer sends them), network-status panel (Ctrl+Alt+G); fontconfig font via ab_glyph |
| `src/viewer/thumbs.rs`, `src/viewer/host.rs` | launcher device previews (last picture, local 0600 cache); viewer sign-in keeps the local host user service running so the machine joins the account's device list |

Diagnostic subcommands: `doctor` (paste-safe report: session, selected backend, engine plugins, uinput, encoder and codec with source, sign-in/service state; booleans and names only), `set-encoder auto|cpu|vaapi|nvenc` and `set-codec auto|h264|h265` (both notify a running host with SIGUSR2; live encoder switch, negotiated codec unchanged, codec preference on the next desktop stream; illegal values exit 2), `login [--no-gui]`, `unlock-check` (password on stdin, prints only accepted/rejected),
`desktop-probe [new.png]`, `desktop-video-probe` (`UU_PROBE_FPS`, `UU_PROBE_FRAMES`, `UU_PROBE_QUALITY` 1-5 = viewer bitrate preset, 5 auto; `UU_PROBE_CHROMA=444` 真彩; `UU_PROBE_INPUT` fixed video/image sequence, `UU_PROBE_LOOP=1` repeats), `wayland-input-probe`, `clipboard-acceptance`,
`portal-acceptance`, `help`. Unknown arguments exit with code 2. Builds up to v0.1.0 did **not** reject them and
started a second full host server instead, so check that a subcommand exists in the binary you run.

## 7. Protocol facts (verified against the official clients)

- Connection types (ConnectOptions.capture_type): 1 desktop, 5 file transfer, 8 terminal, 9 port mapping.
  Video is sent only for desktop connections (capture_type ≤ 2).
- CONTROL messages are protobuf `Message {1 seq, 2 timestamp, <which> payload}`; input arrives as JSON
  (`kbd_press`, `mouse_move_absolute` with normalized 0..1 floats, ...).
- Echo: the viewer sends SimpleAction action 0 with its FeatureFlag; the host answers action 1 with args
  `{"seq":<request seq>}` and **its own** FeatureFlag. The viewer only uses features the host declares:
  `1 ff_capture_setting=4`, `2 ff_simple_action=2`, `3 ff_system_metrics=2`, `6 ff_file_transfer_ftp=2`,
  `8 ff_clipboard=3` (when the clipboard bridge is ready). Do not declare versions you don't implement.
- RPC = Message 21 `RpcRequest {1 header{1 request_id}, <case>}`, answered with Message 22
  `RpcResponse {1 header (same id), <case>}`. Cases used: 2 capture_setting, 3 fps, 4 video_quality,
  6 resolution, 8 file transfer, 9 clipboard, 15 send_video_track, 16–20 plugins, 21 virtual audio policy,
  28/29 terminal.
- CaptureSettingRequest: field 1 is an **fps option index** (1=30, 2=60, 3=90); the real frame rate is
  field 18 `fpsCount`. Field 2 quality: 1 Fast, 2 General (高清 2M), 3 HD (超清 8M), 4 Bluray (原画 20M),
  5 Auto, 6 Custom. Resolution: 16/17 pixel size or 5/6, 10 resolution_type (3 = follow remote).
  Field 7 `chorma_format` (ChromaFormat 1 = 4:2:0, 3 = 4:4:4) is sent when 真彩 is toggled; ff_capture_setting versions
  are cumulative (3 CHROMA_SAMPLING, 4 CUSTOM_BITRATE, 5 DPI_SCALE, 6 HDR). 4:4:4 = NVENC `high444p` / x264 `high444`
  with `yuv444p`; the in-process NVENC engine uses High 4:4:4 / HEVC FRExt (AQ off); VAAPI stays 4:2:0.
  Reply `RpcResponse{2 CaptureSettingResponse{}}` (no errors = applied) and send Message 25 CaptureSettingSync
  (field 3 = the chroma actually encoded).
- ConnectOptions field 4 `decoder_cap_list` = DecoderCap {1 fps, 2 codec_type (1 H264, 2 H265), 3 width, 4 height,
  5 chroma_format}. The Mac viewer lists H.264 and H.265, each 4:2:0 and 4:4:4, up to 3840x2160@120.
- Codec choice is the host's (official negotiateCodec): the Mac viewer offers H265 before H264 in SDP and
  CaptureSettingRequest field 13 only reports its current codec. The host sends CONTROL Message 18
  CodecNegotiation {1 EncoderCap {1 capture_mode=1, 2 fps, 3 codec_type, 4 width, 5 height}} at CONTROL open and
  selects H.265 when both m-line codecs are bound (`UU_VIDEO_CODEC=h264` forces H.264).
- SystemMetrics (Message 13): GeneralMetric `2 system_sound_support` is an enum, **2 = supported** (1 = not
  supported); SettingMetric 1 keep_mute, 2 mute_permanent, 3 auto_lock, 5 file_transfer_save_location,
  8/9/10 permission states (2 = allow).
- Security menu: 静音运行 on → SimpleAction 14 + 18, off → 16. 远程结束后自动锁屏 → SimpleAction 19 with
  `value` (field 5) **2 = on, 1 = off**. The viewer sends these only when toggled, so the host persists them.
- Auto unlock: host reports `SystemStateChange{1 SecureDesktopState{1 is_locked, 5 is_support_auto_logon}}`;
  the viewer (after the user enables 自动解锁被控端) sends SimpleAction 15 with args `{"key": password}`;
  failure → Message 12 `ReportError{2: -5}`.
- File transfer: the viewer sends RPCs on CONTROL but receives only on FILE_DATA_CHANNEL, so all replies go
  out on FILE. With `ff_file_transfer_ftp=2` directory listings are zlib-compressed protobuf. `remove_dir` is
  sent for every delete and must be answered with response case **6 `result{id, file_error}`**, not
  `operation_result`. Host root is shown as ":/" (此电脑); home is "~/".
- Terminal: BINARY channel, RPC 28 check_env → 24; frames `"TERM"`, version 1, type (enum+1), 2 flag bytes,
  session u32 LE, length u32 LE, payload; JSON `{cols,rows,shell,option}`.
- Port mapping (capture_type 9): Message 27 `PortMappingFrame {1 session_id, 2 rule_id, 3 stream_id, 4 type, 5 payload}`
  on FILE_DATA_CHANNEL only. Types SYN=0, FIN=1, DATA=2, SYN_ACK=3, DATA_ACK=4 (ignored by both sides). SYN payload
  JSON `{"version","target_host","target_port"}` → connect → SYN_ACK `{"version","ok","error"}`; DATA raw bytes both ways.
  Echo session/rule/stream ids with their original wire types. The viewer does not support TCP half-close.
- SCTP message size: webrtc-rs omits `a=max-message-size`, so the viewer assumes 64 KB and refuses larger messages; the
  answer sent to the viewer adds `a=max-message-size:262144` (only the sent copy — webrtc-rs rejects a modified local
  description) and `vendor/webrtc` raises the data-channel receive buffer from 65535 to 262144 bytes.
- Cursor shape = Message 15 SystemStateChange field 2. Audio: the viewer always offers an Opus m-line.
- Login API: `/api/v1/device/windows/init` → device_id; `/qrcode/gen/login`; long-poll
  `/qrcode/login/status` (1 wait, 2 scanned, 4 confirmed); `/login/by_qrcode` → user token. QR expires after
  ~5 min with HTTP 400 code 1190 (not fatal: generate a new one).

- Remote assistance (recipient side), live-verified with the official Mac viewer; protocol from the official
  Windows lite host (GameViewerLite: GameViewerServer + GameViewerLiteInner). Guest (not signed in) and account
  identities use parallel endpoints:
  | step | guest | signed in |
  |---|---|---|
  | identity | register the Windows device, then `POST /api/v1/guest/create` (422/1001 if unregistered) | account token |
  | room | `POST /api/v1/guest/room/create` **before** share info (else 400/2004) | the host's own room |
  | ID + code owner | `POST /api/v1/guest/share/info` → `connect_id` | `GET /api/v1/device/share/info` (POST = 405) |
  | withdraw | `POST /api/v1/guest/share/cancel` | `POST /api/v1/device/share/cancel` |
  Cloud pushes arrive as `bmsg_push`: `get_control_mode {control_id}` → `POST …/room/share/upload_control_mode`
  `{control_id, allow_control, control_mode: by_password}` (guest v1, account v2); the viewer only asks for the code
  after this. `remote_control {control_id, salt}` → `upload/sign` (guest v1) / `upload_sign` (account v2)
  `{control_id, can_remote_control, sign: sha256hex(salt+code), backup_sign}`. Temporary-only: sign=temp code,
  empty backup; both: backup=custom; custom-only: sign=custom. The share ID stays the same across sign-in/out.
  Same-account devices cannot assist each other (viewer: 无法对同账号的设备发起远程协助). On a signed-in host never gate
  the owner's offers on the assist switch; the cloud already gates assist peers.

## 8. Known pitfalls

- webrtc-rs `Sample.duration` sets the gap to the **next** frame. FFmpeg 6.1 rejects `-r` together with
  `-fps_mode vfr`.
- All-intra 60 fps saturates Wi-Fi; keep GOP ≈ 0.5 s. A bitrate cap alone does not change quality on a static
  desktop, so presets also change the CQ level.
- X11 capture on a GPU saturated by CUDA work starves; prefer the iGPU for the desktop when available.
- Remote keys repeat at high RTT because key-up arrives after the X autorepeat delay. XTEST repeats through the
  master keyboard; the engine disables global autorepeat only while a remote key is held.
- Interactive shells ignore SIGTERM: the server HUPs/KILLs PTY sessions on shutdown.
- An explicit `DISPLAY` that cannot be opened is an error; the X11 engine falls back to `:0`/`:1` only when DISPLAY is unset.
- systemd/D-Bus-activated apps (GNOME Terminal, portals) run with the user manager's environment, not the caller's:
  a private `dbus-run-session` bus does not bring them onto a virtual display.
- GTK windows stop repainting while GNOME is locked or the monitor is off; test UI on Xvfb or after unlock.
- logind `unlock-session` by the session owner clears the GNOME shield in ~3 s.
- GNOME closes the portal session on lock; capture/input must share one portal session and be rebuilt after
  unlock. Portal input/clipboard permissions are persisted per desktop (0600 file); a revoked permission asks again.

## 9. Debugging checklist

- Connection: `Offer for connection: capture_type=`, `PeerConnection State changed`, `ICE path: ... route=`.
- Video: `Desktop encoder: ... fps= cq= maxrate=`, `Desktop latest-frame stats`, `Video feedback totals`.
- Viewer requests: `CONTROL message: fields=[..] simple_action=..` (numbers only), `Capture setting request`,
  `SimpleAction N (...) received`.
- Features: `File transfer ...`, `Terminal frame`, `Native desktop clipboard text received: bytes=`,
  `Desktop audio started`, `Auto unlock result`, `Auto lock after remote session`.
- When the viewer "does nothing" after a menu click, first check whether any message arrived; usually the
  host did not declare the matching FeatureFlag/SystemMetrics capability.

## 10. Contributing

- Run `cargo test` (and the feature builds you touched) before committing; keep commits focused.
- New protocol behavior: cite the evidence (observed field numbers, official log strings) in a comment.
- Update README.md / README.en.md when user-visible features or limitations change, and add
  `docs/release-notes/<tag>.md` before tagging a release.

### VAAPI

`UU_VIDEO_ENCODER=vaapi` or `set-encoder vaapi` selects Intel/AMD H.264 VAAPI explicitly.
`UU_VAAPI_DEVICE` defaults to `/dev/dri/renderD128` (render-node permissions required).
Auto is NVENC then CPU when an NVIDIA driver is present, otherwise CPU. VAAPI is not an
auto candidate until #33.
The FFmpeg process pipeline uploads NV12 frames to a VAAPI frame pool. With `--features inproc-av` on X11, VAAPI
H.264/HEVC Main runs in process: a DRI3 DMA-BUF of the root (XShm upload without DRI3) goes through VPP to NV12
and VCN, with SPS/PPS(/VPS) and slice headers packed by the host as radeonsi (Mesa 26) needs; parameters follow
FFmpeg's hevc_vaapi/h264_vaapi on that driver. Accepted on AMD HawkPoint1 (Mesa 26.2.3) and Cezanne
(Mesa 26.2.5; XShm-upload and DRI3 zero-copy probes plus a full HEVC Mac-viewer session) with the official
Mac viewer (presets, fps, scaling); Intel iHD is not verified. Measured on Cezanne + RTX 3050 Ti
(1920x1200, desktop-video-probe, 300 frames): in-process VAAPI ~1.5 ms CPU/frame vs in-process NVENC
~7.1 ms vs libx264 process pipeline ~19 ms.
VBR uses the viewer bitrate preset and CQ-derived minimum QP, no B-frames, async depth 1,
AUD and the existing GOP/VFR timing. Unsupported device/driver/initialization falls back to libx264
with a warning. Accepted on Intel UHD 770 (iHD) with the default FFmpeg process pipeline and the official Mac
viewer (presets, fps, scaling, reconnect; no fallback, ~40% less CPU than libx264). The process pipeline on
AMD is not separately verified (AMD acceptance is the in-process engine above).

### Virtual audio / 虚拟声卡

The viewer's 静音运行 switch creates the null sink `uu-remote-sink`,
moves streams from the original default output and captures `uu-remote-sink.monitor`.
New output goes to this sink; streams explicitly pinned to other outputs are untouched.
The last desktop session restores the previous default output and unloads the module.
`mute_permanent` retains the physical mute policy, but still releases the virtual sink.
A private recovery record beside the config and a process lock recover crash leftovers on startup.
`pactl` is used when installed (PulseAudio / PipeWire-pulse); plain PipeWire systems without it (default
Ubuntu desktop) use `pw-cli`/`wpctl` for the same sink and WirePlumber moves playing streams. Accepted on
Ubuntu 24.04 with the official Mac viewer. When routing is unavailable, the existing mute/monitor path remains.
Changing the switch restarts only the scoped audio encoder; normal non-muted capture is unchanged.
`virtual-audio-probe` temporarily routes output for five seconds and restores it (local diagnostic).

### Virtual desktop user bus

The supervisor imports DISPLAY and XAUTHORITY into the linger user bus, waits for authenticated
X11 readiness, and keeps its cleanup trap alive. The sample drop-in uses `%t/uu-virtual-99.Xauthority`.
Use a dedicated headless account. See the [setup and official Mac acceptance checklist](contrib/virtual-desktop/ACCEPTANCE.md).

### Arch Linux / Omarchy

```sh
sudo pacman -S --needed base-devel rust pkgconf clang ffmpeg libx11 libxext libxtst libxfixes gtk3 \
  wayland libxkbcommon wl-clipboard libpipewire glib2 libpulse
cargo test --locked --features wayland
cargo build --release --features wayland
```

Omarchy uses Hyprland: run in its user session, select `UU_DESKTOP_BACKEND=wayland`,
and retain `WAYLAND_DISPLAY` / `XDG_RUNTIME_DIR`. This requires wlr-screencopy and the
virtual input protocols supported by Hyprland; other compositors may need `--features wayland,portal`.
For uinput, load `uinput` and apply the uaccess rule above; seat ACLs require an active login.
For unattended accounts, explicitly restrict `/dev/uinput` access to a dedicated group.
This permission can inject into the whole seat, including lock/greeter screens.
`UU_INPUT_BACKEND=uinput` requests kernel devices; verify startup actually reports uinput.
Optional VAAPI: `intel-media-driver` (modern Intel) or `libva-mesa-driver` (AMD), `libva-utils`
for `vainfo`. Optional KMS: `libdrm`. Virtual Xfce: `xorg-server-xvfb xorg-xauth xorg-xdpyinfo
xfce4-session xfce4-panel xfdesktop xfwm4 gnome-terminal`.
Rolling Arch FFmpeg may be newer than supported `viewer-ffmpeg` (FFmpeg 6.x); build it against a separate
FFmpeg 6 prefix. Do not downgrade the system FFmpeg. The host's `inproc-av` does not link FFmpeg.
User-bus headless setup and the official Mac checklist: [virtual desktop acceptance](contrib/virtual-desktop/ACCEPTANCE.md).

### Experimental KMS / 实验后端

`--features kms` plus explicit `UU_DESKTOP_BACKEND=kms` enables a limited linear-framebuffer
prototype; `UU_KMS_DEVICE=/dev/dri/card0`. Requires libdrm headers and deliberately granted
`cap_sys_admin+ep`, plus card-node access. It is never automatically selected or granted privileges.
This capability is broad: it applies to the whole network-facing process and can expose greeter/lock screens that portal consent would hide. Use it only on a disposable machine with a reviewed binary; inspect with `getcap`, revoke with `sudo setcap -r <path>`. A VM greeter capture was solid black, so lock/greeter screens are not supported.
The VM GDM greeter capture was solid black (failed visual acceptance); lock/greeter support and Intel/AMD hardware remain unverified. Use uinput for input; clipboard
is unavailable without a desktop session. No default release behavior changes.
No official "no desktop" error code was found for headless desktop offers, so the existing offer behavior is retained.
