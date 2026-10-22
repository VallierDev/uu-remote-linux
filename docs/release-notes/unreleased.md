未发布：编码器 / codec 可在运行中选择。/ Unreleased: runtime encoder and codec selection.

### 行为变化 / Changed
- **`auto`（未设置 `UU_VIDEO_ENCODER`，且设置文件没有显式编码器）会做一次真实编码探测。** 显示由 NVIDIA 驱动时仍优先 NVENC；否则顺序为 VAAPI →（有 NVIDIA 节点时）NVENC → CPU。以前「有 NVIDIA 驱动就用 NVENC」的机器，探测成功后可能改用 VAAPI。
- 恢复旧选择：`uuremote-server set-encoder nvenc` 或 `set-encoder cpu`。显式环境变量或设置文件与 v0.2.23 一致。
- `auto` probes one real frame. NVIDIA driving the display still prefers NVENC; otherwise VAAPI, then NVENC when a driver node exists, then CPU. A machine that used NVENC only because a driver was present may switch to VAAPI. `set-encoder nvenc` or `set-encoder cpu` restores that choice.

### 新增 / Added
- `uuremote-server set-encoder auto|cpu|vaapi|nvenc` 与 `set-codec auto|h264|h265` 写入 `host_settings.json`。host 在运行时收到 `SIGUSR2` 后切换进行中桌面流的编码器，不断开连接；codec 在下次桌面连接生效。
- `set-encoder` / `set-codec` write `host_settings.json` and signal a running host with `SIGUSR2`.
