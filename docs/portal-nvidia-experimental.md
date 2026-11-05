# Experimental Portal → NVIDIA / 实验性 Portal → NVIDIA

默认关闭。此入口已串联到生产视频任务，并在单台测试主机 2560×1440 / H.264 配置完成实际客户端画面与操作验收；日志确认使用 Portal DMA-BUF → NVIDIA 原生编码路径。该结果不代表所有设备或配置均已验收。

Disabled by default. This entry is wired into production video tasks and passed deployed client picture/control acceptance on one tested host at 2560×1440 with H.264. Logs confirm Portal DMA-BUF to native NVIDIA encoding. This does not establish acceptance for other devices or configurations.

## 显式配置 / Explicit configuration

需要 Linux、`wayland,portal,inproc-av` 功能、PipeWire >= 1.2 的开发头文件及支持显式同步的生产者、NVIDIA CUDA/EGL/NVENC，以及现有 `nvenc` 编码器选择。`engine-plugins` 构建需要匹配的 Portal 插件。现有 `UU_VIDEO_PIPELINE=process`、`UU_CAPTURE=grab` 或自定义 FFmpeg root 会继续禁止原生编码入口。

Requires Linux, the `wayland,portal,inproc-av` features, PipeWire >= 1.2 headers and an explicit-sync producer, NVIDIA CUDA/EGL/NVENC, and the existing `nvenc` encoder selection. Plugin builds need a matching Portal plugin. Existing process-pipeline, grab-capture and custom-FFmpeg-root overrides still disable native selection.

| Variable | Required value / 必需值 |
|---|---|
| `UU_PORTAL_NVENC_PROFILE` | `mutter-release-point-v1` |
| `UU_PORTAL_NVENC_MODIFIER` | Explicit reviewed hexadecimal DMA-BUF modifier, optional `0x` prefix / 明确审查过的十六进制 modifier，可带 `0x` |
| `UU_PORTAL_NVENC_DEVICE` | Absolute DRM render-node path / DRM 渲染节点绝对路径 |

配置上述 profile 表示操作者已确认生产者满足已审查的同步契约：acquire 表示源写入完成，生产者在 release 点完成前不会复用缓冲；消费者独占取帧，没有外部信号者篡改时间线。不能仅凭 Mutter 版本、NVIDIA 型号或帧标志推断此契约。设备初始化会检查 CUDA 与 EGL 的 PCI 设备匹配，modifier 需要与实际协商的像素布局兼容。缺少或无效的配置不会启用此路径。程序不内置单台测试主机设备或 modifier。

Selecting this profile asserts the reviewed producer contract: acquire means source writes are complete, producer reuse waits for release, the consumer exclusively acquires frames, and no foreign signaler mutates the timelines. Compositor version, GPU vendor and frame flags alone do not establish this contract. Initialization checks CUDA/EGL PCI identity; the configured modifier must match the negotiated layout. Missing or invalid configuration does not enable this path. No machine-specific device or modifier is compiled in.

## 行为与限制 / Behavior and limits

- 当前导入为 BGRA → 同尺寸编码器自有 CUDA 显存；不提供缩放或补边。初始化不支持的请求进入新建 SHM 会话恢复。 / Import copies BGRA into an equally sized encoder-owned CUDA surface; scaling and padding are unsupported and trigger fresh-SHM recovery.
- 持续 worker 经现有视频队列发送，缺少新帧时可使用自有显存保活。 / The continuous worker publishes through the existing video queue and can keep alive from its own surface when no new frame arrives.
- 失败后仅在 GPU 已知完成、租约清理结束且新 SHM 会话安装成功后尝试进程编码。同协议和 modifier 的健康会话可用于设置重启，无需再次协商；真实 30→60 fps 配置重启已验证。失败恢复仍可能重新请求系统共享授权，不会复用失败源作 CPU 采集。 / Process fallback requires known completion, completed lease cleanup and a successfully installed fresh SHM session. Healthy sessions with identical protocol/modifier can be reused on settings restart; a real 30-to-60 fps configuration restart passed. Failure recovery may still request consent again; the failed source is not reused for CPU capture.
- 未知完成或清理失败保留资源，并在该进程内永久禁止后续 Portal 视频重试。该标记不会自动清除；它不是 GPU 完成证明。 / Unknown completion or failed cleanup retains resources and disables subsequent Portal video admission for the process lifetime. The latch never resets automatically and does not prove GPU completion.
- 生产 `run()` 入口和完整视频 reader 均已通过独立进程的真实 32 帧编码解码，reader 取消后的 worker 退出及旧帧失效也已验证；当前单台测试主机 H.264 客户端画面与操作已确认正常；任意尺寸或编解码器变更、多客户端仍需实测。现有真实测试覆盖独立 worker 连续编码、保活和 GPU 完成后受控失败的新 SHM 恢复；不覆盖真实 GPU 挂起。 / The production run() entry and full video reader passed real 32-frame encode/decode in isolated processes, including worker exit and stale-frame invalidation after reader abort; client picture/control at the current H.264 configuration is confirmed, while arbitrary resolution/codec changes and multiple clients still need live acceptance. Existing real tests cover standalone continuous workers, keepalive and fresh-SHM recovery after a controlled post-completion failure, not an actual GPU hang.

配置更改应在评审部署结果后应用；开发不会自动重启现有远程服务。
Apply configuration only as part of a reviewed deployment; development does not automatically restart the current remote service.

## 验证与性能 / Validation and performance

硬件验收使用一台 GTX 1660 SUPER / Ryzen 7 3700X / Mutter 51 主机。真实 Portal 测试覆盖单帧及连续编码解码、缓冲复用、静态显存保活、用户撤销、GPU 完成后的受控失败及新 SHM 会话恢复、正式客户端显示/输入，以及同会话 30→60 fps 重启。未覆盖真实 GPU 挂起、多客户端或所有尺寸/编解码器组合。

Hardware acceptance used one GTX 1660 SUPER / Ryzen 7 3700X / Mutter 51 host. Real Portal tests covered encode/decode, buffer reuse, cached-frame keepalive, user revocation, controlled post-completion failure with fresh-SHM recovery, official-client display/input, and same-session 30-to-60 fps restart. Actual GPU hangs, multiple clients, and all resolution/codec combinations remain unverified.

同一发布版与客户端、2560×1440 H.264、30 fps、CQ18、GOP2秒，按 B1/A1/A2/B2 顺序测量固定动画，每阶段预热10秒并采样60秒。下表为两轮均值，CPU100%代表一个逻辑核；GPU指标为整卡。新旧音频编码实现及AQ设置不同，因此数据代表整套路径收益，不能归因于单一复制操作。没有保存桌面像素、码流、身份信息或原始服务日志。

A fixed-animation comparison used the same release/client at 2560×1440, H.264, 30 fps, CQ18, and a two-second GOP, ordered B1/A1/A2/B2, with ten seconds warmup and sixty seconds sampling per phase. Means below combine two runs; CPU100% is one logical core, GPU metrics cover the whole card. Audio implementations and AQ settings differ, so this compares complete paths rather than isolating a copy operation. No desktop pixels, bitstreams, identities or raw service logs are included.

| Metric / 指标 | SHM + FFmpeg NVENC | DMA-BUF + native NVENC |
|---|---:|---:|
| Server delivery fps / 服务端交付帧率 | 30.02 | 30.02 |
| Service cgroup CPU / 服务CPU | 42.84% | 6.40% |
| Compositor CPU / 合成器CPU | 28.09% | 20.51% |
| GPU busy / GPU忙碌率 | 14.04% | 8.08% |
| NVENC busy / 编码器忙碌率 | 13.08% | 15.00% |
| GPU memory / 整卡显存 | 2510 MiB | 2737 MiB |
| GPU power / 整卡功耗 | 41.53 W | 41.72 W |

静态画面新路径约16.4 fps、旧路径30 fps，不能视为同编码量比较。两次重复不足以推断统计显著性；服务端交付帧率不等于客户端呈现帧率。测量工具为 `contrib/portal-ab-stimulus.py`、`portal-ab-round.py` 和 `portal-resource-sample.py`，不会自动切换服务。

Static delivery is about16.4 fps on the new path versus30 fps on the old path, so it is not an equal-encode-work comparison. Two repetitions do not establish statistical significance; server delivery is not client presentation. Measurement tools are `contrib/portal-ab-stimulus.py`, `portal-ab-round.py`, and `portal-resource-sample.py`; they do not switch services automatically.

停止修复：SIGTERM/SIGINT 统一关闭 peer 并让 runtime 取消和排空 worker，然后退出进程。此前直接从 runtime worker 调用 exit 会卡在 libnvcuvid 的退出回调。真实客户端连接下，修复版0.464秒正常退出（状态0），未触发90秒超时或SIGKILL。

Shutdown fix: SIGTERM/SIGINT close peers through the common path and let runtime teardown cancel/drain workers before process exit. Exiting directly from a runtime worker previously hung in a libnvcuvid exit handler. With a real client connected, the fix stopped normally in0.464 seconds (status0), without the previous90-second timeout or SIGKILL.

离线检查 / Offline checks:

```sh
cargo test --locked
cargo test --locked --features inproc-av,wayland,portal,engine-plugins
python3 tests/portal_nvidia_offline.py
python3 tests/portal_resource_sample.py
```

真实GPU及Portal测试在Rust测试中标记为ignored，需要明确选择、有兼容硬件且由用户授权共享；普通测试不会启动桌面共享。
Real GPU/Portal Rust tests are ignored by default and require explicit selection, compatible hardware and user consent to share. Ordinary tests do not open screen sharing.
