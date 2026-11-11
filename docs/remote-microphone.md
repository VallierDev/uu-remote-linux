# Remote microphone / 控制端麦克风（实验性）

Opt-in controller-to-host Opus audio uses the existing authenticated desktop WebRTC connection and exposes a dedicated virtual microphone to Linux applications.

可选功能：通过原有、已认证的桌面 WebRTC 连接接收控制端 Opus 音轨，为 Linux 应用提供独立虚拟麦克风。

## Enable / 启用

Requires Linux, running PipeWire-pulse or PulseAudio 13+, the `libopus.so.0` / `libpulse.so.0` runtimes, and `pactl`. Debian/Ubuntu: install `libopus0 pulseaudio-utils`; Arch: install `opus libpulse`. No administrator privileges or additional network listener are needed at runtime. Both libraries are loaded only when requested; ordinary builds need no PulseAudio or Opus development package or extra link-time dependency.

需要 Linux、已运行的 PipeWire-pulse 或 PulseAudio 13+、`libopus.so.0` / `libpulse.so.0` 和 `pactl`。Debian/Ubuntu 对应 `libopus0 pulseaudio-utils`；Arch 对应 `opus libpulse`。运行时无需管理员权限，也不新增监听端口；普通构建无需安装 PulseAudio、Opus 开发包或增加链接依赖。

```ini
# ~/.config/systemd/user/uuremote-native.service.d/microphone.conf
[Service]
Environment=UU_MICROPHONE=1
```

```sh
systemctl --user daemon-reload
systemctl --user restart uuremote-native.service
```

The host creates the input device **UU-Remote-Microphone** (`uu-remote-microphone`) and a private backing sink (`uu-remote-mic-input`). In the application that should receive your voice, select UU-Remote-Microphone as its input. This patch does not change the system default input or output device. The backing sink is an audio destination for the decoder; it is not the device to select as an application microphone.

被控端会出现 **UU-远程麦克风** 输入设备（英文界面显示 **UU-Remote-Microphone**）。在需要使用麦克风的应用中选择这个输入。设备与音频流的显示名称跟随被控端语言，技术名称 `uu-remote-microphone`、`uu-remote-mic-input` 不变。补丁不修改系统默认输入、输出；底层的 `uu-remote-mic-input` 是解码器内部输出目标，不是应用应选择的麦克风。

Device and stream display labels follow the host language; the technical sink/source identifiers stay unchanged.

Enable the controller's microphone in the official UU client. The host completes VirtualAudioDriverPolicyRequest (RpcRequest field 21) using RpcResponse field 17 with a correctly encoded two-byte tag. It commits the reception gate and queues its reply/notification as one ordered transition before any network send can block. CONTROL and TEXT callbacks share the same transition lock, so a stalled open reply cannot overwrite a later close. Reception starts closed; connecting alone does not request capture. Opus is decoded at 48 kHz stereo and written through libpulse with PA_STREAM_DONT_MOVE, preventing rescue to a physical output on either supported audio server. A disconnected or superseded desktop drops queued audio and closes playback. Tool-only peers cannot inject microphone audio. A new incoming track replaces the previous track of the same peer.

在官方控制端中开启麦克风。被控端通过正确编码的 RPC 响应确认虚拟声卡策略；接收开关和回复、通知的排队操作使用统一的状态锁，均在网络发送之前完成，避免拥塞时旧开麦请求覆盖后到的关麦。接收初始为关闭，单纯建立远控连接不会主动要求采集。Opus 解码为 48 kHz 双声道，通过 libpulse 的禁止迁移标志仅写入虚拟麦克风，两个支持的音频后端均不会在目标丢失后转向物理输出。断开或桌面会话被替换时清空待播放内容并停止播放；文件、终端等非桌面连接不接收麦克风。

If an external tool removes the virtual endpoints, or the audio server restarts, reception stops. Restart the host to recreate its endpoints.

如果外部工具删除了虚拟设备，或音频服务重启，接收会停止。重启被控端服务可重新创建设备。

No microphone audio is logged, saved or played through physical speakers by this receiver. Applications consuming the virtual input retain their own behavior. A 40 ms initial jitter window and a maximum 12-packet queue limit backlog. RTP timestamps determine missing audio duration for Opus loss concealment, including variable-duration packets and sequence/timestamp rollover. Each track claims a unique generation before its worker is spawned; a superseded worker checks that generation before processing audio.

接收器不记录、保存麦克风音频，也不将它播放到物理扬声器。读取虚拟输入的应用仍按各自设置工作。初始抖动缓冲为 40 毫秒，队列最多 12 包；根据 RTP 时间戳推导缺失音频时长后进行 Opus 补偿，支持可变包长与序号、时间戳回绕。新音轨在启动接收任务前取得唯一归属编号，旧任务处理音频前会核验该编号。

The endpoint lock lives in a private per-user directory under `/tmp`, independently of `UU_CONFIG`, `XDG_RUNTIME_DIR` and `TMPDIR`. Recovery requires an exact match of module kind and arguments, including the remap source's master. Cleanup checks the module id and arguments again before unloading, and the route is released on ordinary error returns as well as shutdown.

设备锁位于 `/tmp` 下的用户私有目录，不随配置目录、`XDG_RUNTIME_DIR` 或 `TMPDIR` 变化。恢复设备时严格匹配模块类型及全部参数，包括虚拟输入的 master；清理前再次核验模块编号及参数。常规错误返回与正常关闭都会释放设备。

Native PulseAudio uses its `norewinds` null-sink option to avoid the default two-second idle buffer on first playback. This Pulse-only option is not passed to PipeWire. Unsupported older PulseAudio servers leave the opt-in microphone disabled.

原生 PulseAudio 使用空设备的 `norewinds` 选项，避免首次播放时的默认两秒空闲缓冲；不向 PipeWire 传递这个专用参数。较旧、不受支持的 PulseAudio 不会启用麦克风。

## Validation / 验证

Opus-dependent unit tests print a skip diagnostic when `libopus.so.0` is unavailable, so default tests still pass without this optional runtime. CI's `inproc-av` jobs install libopus and exercise these tests. The explicitly requested audio-server smoke tests require the runtime and fail if it is missing.

缺少可选的 `libopus.so.0` 时，依赖 Opus 的单元测试会输出跳过说明，默认测试仍能通过。CI 的 `inproc-av` 任务安装 libopus 并实际运行这些测试；显式启用的音频服务真机测试需要该运行库，缺少时会失败。

```sh
cargo test --locked --bin uuremote-server --features wayland,portal,engine-plugins microphone::tests
# Real audio-server smoke test: independent test endpoints, generated audio only, in memory.
UU_MICROPHONE=1 \
  cargo test --locked --bin uuremote-server --features wayland,portal,engine-plugins \
  microphone::tests::pipewire_virtual_microphone_receives_generated_tone -- --ignored --exact
```

The smoke test creates process-specific sink/source names, plays and captures only through those endpoints, waits for its playback task to end before removing them, and never reuses the host's microphone. A separate config file alone does not isolate audio resources. Unit tests cover cross-process locking, module ownership, concurrent track claims, 10/20/40/60 ms loss, mixed-duration gaps, clock rollover, WebRTC track replacement, and CONTROL/TEXT ordering under send congestion. Earlier controller acceptance passed with Windows 4.44.0.3919 on CachyOS using the previous receiver backend. The native playback backend is checked separately with generated audio; Windows acceptance has not been repeated after that backend change.

真机测试使用进程专属的虚拟输入、输出名称，仅在测试设备之间播放和采集，等播放任务结束后再删除设备，不复用正式服务的麦克风。单独更换配置文件不能隔离音频资源。回归测试覆盖跨进程锁、模块归属、并发音轨、10/20/40/60 毫秒包的丢包、混合时长缺口、时钟回绕、WebRTC 音轨替换及拥塞时的 CONTROL/TEXT 顺序。之前的接收后端已用 Windows 4.44.0.3919 在 CachyOS 实机通过控制端验收；原生播放后端另用生成音验证，更换后尚未重复 Windows 实机验收。

An optional native PulseAudio check starts a private server with no hardware modules. It first confirms sustained generated audio, then verifies that removing its playback target closes the stream and leaves the default null sink silent. Use `--smoke` to check audio delivery alone. The wrapper verifies that the named test exists before running it, so an old `--test-binary` cannot silently pass with zero tests. It needs a PulseAudio executable; `--daemon` and `--module-dir` also support unpacked packages. It does not start or modify a system service.

可选的原生 PulseAudio 检查会启动不加载硬件模块的独立服务，先确认持续生成音正常，再验证播放目标消失后音频流关闭、默认空设备保持静音；`--smoke` 仅验证生成音传输。脚本会检查目标测试存在，旧 `--test-binary` 不会以零项测试误报成功。需要 PulseAudio 可执行文件，`--daemon`、`--module-dir` 也支持解压后的程序包，不启动或修改系统服务。

```sh
python3 contrib/check-microphone-pulse.py
python3 contrib/check-microphone-pulse.py --smoke
```

To disable, remove `microphone.conf`, reload the user manager and restart the host. The virtual endpoints are removed on normal shutdown. After a crash, the next enabled run reuses only the exact tagged modules created by this receiver.

停用时删除 `microphone.conf`，重新加载并重启用户服务。正常退出会删除虚拟设备；异常退出后，下次启用仅复用本接收器创建并标记的同名模块。
