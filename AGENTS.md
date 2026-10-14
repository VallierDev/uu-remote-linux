# AGENTS.md — guide for AI coding agents

This file is written for large language model agents (Claude Code, Codex, Cursor, ...) that are asked to
**build, install, debug or develop** uu-remote-linux. Read it fully before acting. Human documentation is in
[README.md](README.md) (Chinese) and [README.en.md](README.en.md).

## 1. What this project is

- An **unofficial Linux host** for NetEase UU Remote (网易 UU 远程). The official Mac / Windows / mobile
  clients connect to it and control the Linux desktop. There is no viewer/controller side here.
- One Rust binary, `uuremote-server` (Cargo package `uu-remote-linux`), plus small C engines in `c_engine/`
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
7. Keep changes small and tested; match the surrounding code style (dense, comment only the non-obvious).

## 3. Build and test

Dependencies (Ubuntu/Debian):

```sh
sudo apt install build-essential pkg-config \
    libx11-dev libxext-dev libxtst-dev libxfixes-dev libavcodec-dev libavformat-dev libavdevice-dev libswscale-dev libclang-dev libgtk-3-dev ffmpeg
# --features wayland:        libwayland-dev libxkbcommon-dev wl-clipboard
# --features wayland,portal: + libglib2.0-dev libpipewire-0.3-dev
```

Rust stable via rustup. Commands:

```sh
cargo test                                   # unit tests (macOS builds use C stubs, also fine for development)
cargo build --release                        # X11 host
cargo build --release --features wayland     # + Hyprland/wlroots
cargo build --release --features wayland,portal   # + GNOME/KDE Wayland via xdg-desktop-portal
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
3. **Login (needs the human).** Run `uuremote-server login` inside the graphical session. A GTK window shows a QR
   code; the user scans it with the UU Remote mobile app and confirms. Without a display use
   `uuremote-server login --no-gui` (terminal QR). Credentials are written to `~/.config/uuremote/config.json`
   (mode 0600). Do not read the token out of that file or print it.
4. Create a systemd user unit (`~/.config/systemd/user/uuremote-native.service`):

   ```ini
   [Unit]
   Description=UU Remote Linux host (unofficial)
   After=graphical-session.target
   [Service]
   Environment=RUST_LOG=info
   Environment=DISPLAY=:0
   # GNOME on Xorg usually needs: Environment=XAUTHORITY=/run/user/<uid>/gdm/Xauthority
   ExecStart=%h/.local/bin/uuremote-server
   Restart=always
   RestartSec=5
   [Install]
   WantedBy=default.target
   ```

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
| `~/.config/uuremote/host_settings.json` | persisted 安全 settings: auto_lock, keep_mute, mute_permanent |
| `UU_DESKTOP_BACKEND` | `auto` (default) / `x11` / `wayland` / `portal` |
| `UU_VIDEO_ENCODER=cpu` | force libx264 instead of NVENC |
| `UU_FFMPEG_ROOT` | FFmpeg prefix (`bin/ffmpeg`, libs in `lib/x86_64-linux-gnu`) |
| `UU_AUDIO=0`, `UU_AUDIO_SOURCE` | disable sound / choose the Pulse source (default `@DEFAULT_MONITOR@`) |
| `UU_LOGIN_WINDOW=0` | don't auto-open the login window |

Token has no expiry field; API code 1120 (HTTP 422) means invalid/expired → re-login. The config is re-read
every 60 s.

## 6. Code map

| Path | Role |
|---|---|
| `src/main.rs` | entry, subcommands, room loop (one peer per `ice_id`), room renewal (deferred while connected) |
| `src/signaling.rs` | Engine.IO v4 / Socket.IO client; `be-controlled` binary attachment = ConnectOptions (field 1 capture_type) |
| `src/auth.rs`, `src/config.rs`, `src/login.rs`, `src/login_ui.rs` | signed REST client, config, device init + QR login, GTK window |
| `src/webrtc.rs` | peer connection, data channels, CONTROL dispatch, initialization messages |
| `src/video.rs`, `src/capture_setting.rs` | FFmpeg H.264 pipeline; viewer quality/fps/resolution settings |
| `src/audio.rs` | Pulse/PipeWire monitor → Opus audio track |
| `src/input.rs`, `src/input_mapping.rs` | input injection (X11 XTest / Wayland / portal), Windows VK → keysym |
| `src/cursor.rs` | pointer shape mirroring (X11) |
| `src/clipboard*.rs` | text clipboard bridge and protocol |
| `src/file_trans.rs` | FileTransferFtp RPC |
| `src/terminal.rs` | TERM frames + PTY sessions |
| `src/session_lock.rs`, `c_engine/pam_check.c` | lock state reporting, auto unlock via PAM + logind |
| `src/system_actions.rs`, `src/system_metrics.rs` | SimpleAction system commands, mute, auto-lock, SystemMetrics |
| `src/initialization.rs`, `src/control_protocol.rs`, `src/protocol.rs` | protobuf encoders/decoders for CONTROL |
| `src/session_ice.rs`, `src/network_diagnostics.rs` | cloud ICE servers, address-free path diagnostics |
| `src/desktop_backend.rs`, `src/wayland.rs`, `src/portal*.rs` | backend selection, Hyprland, GNOME/KDE portal |
| `c_engine/x11_engine.c`, `c_engine/clipboard.c` | X11 capture/input/selection |
| `c_engine/wayland_engine.c`, `c_engine/portal_engine.c` | Wayland and portal engines |
| `c_engine/login_window.c` | GTK3 login window |

Diagnostic subcommands: `login [--no-gui]`, `unlock-check` (password on stdin, prints only accepted/rejected),
`desktop-probe [new.png]`, `desktop-video-probe`, `wayland-input-probe`, `clipboard-acceptance`,
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
  Reply `RpcResponse{2 CaptureSettingResponse{}}` (no errors = applied) and send Message 25 CaptureSettingSync.
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
- Cursor shape = Message 15 SystemStateChange field 2. Audio: the viewer always offers an Opus m-line.
- Login API: `/api/v1/device/windows/init` → device_id; `/qrcode/gen/login`; long-poll
  `/qrcode/login/status` (1 wait, 2 scanned, 4 confirmed); `/login/by_qrcode` → user token. QR expires after
  ~5 min with HTTP 400 code 1190 (not fatal: generate a new one).

## 8. Known pitfalls

- webrtc-rs `Sample.duration` sets the gap to the **next** frame. FFmpeg 6.1 rejects `-r` together with
  `-fps_mode vfr`.
- All-intra 60 fps saturates Wi-Fi; keep GOP ≈ 0.5 s. A bitrate cap alone does not change quality on a static
  desktop, so presets also change the CQ level.
- X11 capture on a GPU saturated by CUDA work starves; prefer the iGPU for the desktop when available.
- Remote keys repeat at high RTT because key-up arrives after the X autorepeat delay. XTEST repeats through the
  master keyboard; the engine disables global autorepeat only while a remote key is held.
- Interactive shells ignore SIGTERM: the server HUPs/KILLs PTY sessions on shutdown.
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
