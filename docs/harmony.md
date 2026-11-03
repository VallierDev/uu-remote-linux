# HarmonyOS 被控端

鸿蒙工程位于 `harmony/`，包名 `com.fixedcat.uuremote.harmony`，与 Linux 主机共用
UU 登录、REST 签名、信令、CONTROL 协议和 WebRTC 实现。当前工程针对 API 26 的
ARM64 PC/二合一设备，与本次开发使用的 MateBook Pro 环境一致。

## 构建

需要 Rust stable（含 `aarch64-unknown-linux-ohos` target）、CMake、Ninja，以及
DevEco Studio。macOS 默认 DevEco 安装位置为 `/Applications/DevEco-Studio.app`；
可用 `DEVECO_HOME` 覆盖。首次运行：

```sh
rustup target add aarch64-unknown-linux-ohos
python3 scripts/build-harmony.py
```

构建目录默认在 `~/Library/Caches/uu-remote-harmony/`，可用 `UU_HARMONY_BUILD` 覆盖。
Rust 静态库与 N-API 模块在该目录内构建，HAP 输出位于
`project/entry/build/default/outputs/default/`。Linux 的 X11、GTK、PAM、FFmpeg
和 PulseAudio 不会链接到鸿蒙模块。

未设置签名时生成 unsigned HAP。使用 DevEco Studio 为**这个包名**生成调试签名，
把 `build-profile.json5` 的本地 `app.signingConfigs` 和产品签名选择放入
`harmony/signing.local.json` 的 `app` 字段。该文件被 Git 忽略。不要提交证书、
密钥、签名口令、设备标识或账号数据；共享的 `build-profile.json5` 保持无签名。

## 使用和当前限制

1. 打开“UU 远程·鸿蒙”，用手机 UU 远程 App 扫码并确认登录。
2. 在鸿蒙端点击“开始共享屏幕”，由本机用户确认系统录屏弹窗。
   如果需要在控制端看到输入法候选栏，在该弹窗中取消勾选“隐藏敏感信息”。
   这个选择会让更多屏幕内容进入共享画面，请按本次共享需要选择。
3. 首次使用键鼠控制时，点击“允许远程键鼠控制”，由本机用户确认系统输入授权。
   后续先查询系统授权状态，已有授权或正在授权时不重复弹窗。
4. 同账号的官方客户端可在设备列表中连接 HarmonyOS 主机。
   如需其他账号连接，开始共享后在鸿蒙端的“远程协助”区域开启“允许他人通过设备 ID 连接”，
   显示验证码，再由对方在官方客户端“开始协助”中输入设备 ID 与验证码。
   可手动刷新验证码；默认每次远控结束后刷新。开关选择沿用上游设置持久化。
   官方客户端不支持对同账号设备发起 ID 协助，同账号仍使用设备列表。
5. 点击窗口关闭按钮，默认收起到鸿蒙状态栏托盘，保持当前共享和连接。
   点击托盘图标或“打开主窗口”恢复窗口；“停止共享”结束远控但保留应用。
   托盘菜单“退出”才会停止录屏、关闭连接、释放本服务按下的键鼠并退出应用。
   停止共享保留系统仍有效的输入授权，不主动撤销。
   如果系统或用户已撤销授权，界面会重新显示授权入口，不使用本地标记假定授权仍有效。

录屏采用 AVScreenCapture → AVCodec Surface → H.264 → WebRTC，避免 CPU 逐帧拷贝。
录屏画面不包含主机光标，由控制端显示本地光标，避免延迟造成双光标；鸿蒙本机的光标仍正常显示。
鼠标拖动的移动事件保留已按下的按钮，支持绝对和相对移动；松开或停止共享后释放按钮。
当前固定原屏幕像素尺寸、H.264 4:2:0、30 fps；画质请求调整码率。
客户端同步的是实际输出格式，不支持的分辨率/帧率选择保持原值。
暂未适配系统声音、剪贴板、免账号的访客协助、系统关机/重启/锁屏/自动解锁，
以及 Linux PTY 终端。终端环境检查返回不可用，声音与剪贴板不宣称支持。
文件访问仍受鸿蒙应用沙箱限制；尚未完成官方客户端文件工具验收。

凭据只保存在应用沙箱 `files/uuremote/config.json`，写入权限为 0600。
二维码属于登录凭据，不要写入日志或提交到仓库。正常日志使用 `UUHarmony` 标签，
不含二维码内容、按键、剪贴板、账号 token 或文件内容。

### 输入法候选栏未出现在控制端

当前流式采集模式下，系统录屏弹窗默认勾选“隐藏敏感信息”，会隐藏输入法浮动窗口。
停止后重新开始共享，在系统弹窗中取消该选项，再输入中文检查候选栏。
这属于系统录屏的画面选择，键鼠授权仍按系统现有状态保留。
应用没有公开接口替用户设置或记忆该选择；系统可能在每次开始采集时恢复默认值。
本次真机已由用户确认：取消该选项后，控制端的输入法候选栏恢复显示。
应用在共享按钮旁显示中英文输入法提示，说明该选项的位置及对共享内容的影响。

依据是 [OpenHarmony 7.0 屏幕采集服务源码](https://github.com/openharmony/multimedia_player_framework/blob/29ec878e7a1c46687e7c042b35a60b2882c85bdd/services/services/screen_capture/server/screen_capture_server.cpp#L3008)：
原始流启动时将 `checkBoxSelected_` 置为 `true`，授权弹窗读取用户选择，
`HandleOriginalStreamPrivacy` 按该选择设置系统窗口保护。
[同场景问题报告及作者的验证](https://developer.huawei.com/consumer/cn/forum/topic/0201223813276681052)
也确认取消该选项后候选栏恢复。

## 验证状态

已通过 ARM64 鸿蒙原生库交叉编译、ArkTS/HAP 编译、独立调试签名及真机安装/启动。
接入上游 v0.2.47 后，macOS 上原有 Rust 测试为 304 passed，5 ignored；
托盘关闭、恢复、停止、退出、异常清理及中英文标签另有 5 项测试。
`python3 scripts/test-harmony-native.py` 在 macOS 上使用真实适配代码
和 SDK 声明，验证拖动、松开、相对移动、注入失败和停止采集时的鼠标事件序列。
ID/验证码协助复用上游账号协助协议，跨账号实连仍需验收；
拖动修复已通过适配层事件序列测试，跨平台窗口拖动仍待实连验收。
真机视频连接已观察到持续传输，输入法候选栏已由用户确认恢复；
不能将构建成功视为其他功能的实连验收。
