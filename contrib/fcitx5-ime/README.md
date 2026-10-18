# fcitx5 IME bridge（手机输入法文字 → 当前输入框）

官方 Windows 被控端用 `SendInput(KEYEVENTF_UNICODE)` 把手机输入法提交的文字直接注入。Linux 上没有
等价手段：

- KWin 不实现 `zwp_virtual_keyboard_manager_v1`，无法上传 keymap 后用虚拟键盘按键；
- `zwp_input_method_v2` 由本机输入法（fcitx5）占用，第三方不能接管；
- 直接发 keycode 只能打 ASCII，而且依赖本机键盘布局。

所以被控端把文字交给本机正在运行的输入法：这个插件在 `$XDG_RUNTIME_DIR/uuremote-ime.sock` 上收文本，
交给 fcitx5 的事件分发器，再由当前输入上下文 `commitString`——等价于你自己用输入法打字，**不占剪贴板**，
终端里也不需要粘贴快捷键。

## 安装

依赖：fcitx5（含头文件 `/usr/include/Fcitx5`）、`libFcitx5Core`、支持 C++20 的编译器。

```sh
cd contrib/fcitx5-ime
./build.sh              # 编译 + 安装到 /usr/lib/fcitx5 + 重启并检查
```

要在 KDE Wayland 上生效，fcitx5 必须拿到 compositor 的输入法连接，这一步由 **KWin** 完成
（`kwinrc` 里的 `InputMethod=/usr/share/applications/fcitx5-wayland-launcher.desktop`）。因此：

- 装完要重启输入法：**系统设置 → 键盘 → 输入法：先选「无」再选回 Fcitx 5**，或注销重新登录；
- 只重启 fcitx5 进程不够：没有 KWin 的 launcher，插件能加载，但 fcitx5 看不到任何输入上下文，
  文本会无处可去（插件日志里是 `focused=0`）。

`build.sh` 会检查三件事并给出提示：插件是否在 fcitx5 的插件列表里、socket 是否就绪、KWin 的
wayland launcher 是否在运行。

## 验证

```sh
# 插件是否加载
busctl --user call org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1 GetAddons | grep uuremote_ime

# 手动投一段文本（先在桌面点进一个输入框），会看到插件日志
printf '测试' | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/uuremote-ime.sock"
journalctl --user --since "-1min" -o cat | grep 'uuremote_ime:'
# 期望：uuremote_ime: commit 6 bytes, focused=1
```

被控端日志会写明走的是哪条路：

```sh
journalctl --user -u uu-remote-native.service --since "-5min" -o cat | grep 'Desktop text input'
# committed by fcitx5     → 走插件，剪贴板不受影响
# pasted from clipboard   → 回退路径（插件或 fcitx5 不可用）
```

## 卸载

```sh
./build.sh --uninstall   # 之后同样按上面的方式重启输入法
```

被控端**不会自动安装也不会自动删除**这个插件：没装就是剪贴板回退路径；卸载被控端时插件会留在
`/usr/lib/fcitx5` 与 `/usr/share/fcitx5/addon`（只是没人再连它的 socket，无害），所以建议顺手
`--uninstall` 一次。反过来，重装被控端时插件仍在，不用再装一遍（除非 fcitx5 换了 ABI）。

## 状态

- **主机侧是完整的**：Wayland 上的 `text_input` 按「插件优先、剪贴板粘贴回退」工作，两条路径都实测过。
- **插件的安装/卸载流程只是够用，没有做完整**：安装要 sudo（脚本编译后写入 `/usr/lib/fcitx5`），没有接入
  发行版打包；KDE 下生效所需的 compositor 连接必须从桌面侧重启输入法，脚本只检查并提示，不替你做；
  卸载也不会被任何自动流程带走（要手动 `./build.sh --uninstall`）。
- **日常使用没有问题**：插件只在 fcitx5 进程内跑一个 socket 线程，没有输入时零占用；插件不可用（或
  fcitx5 换 ABI 后加载失败）时被控端自动回退到剪贴板粘贴，不会出现「完全打不了字」。

## 注意事项

- 插件与 fcitx5 的 ABI 绑定（`libFcitx5Core.so.<major>`）：fcitx5 大版本更新后重跑 `build.sh`。
  没重跑也不会坏——被控端会自动回退到「剪贴板 + `Shift+Insert`」粘贴（这条回退会占用剪贴板）。
- socket 权限 0600 且在 `$XDG_RUNTIME_DIR` 下，只有同一用户能连；被控端只写文本，不读任何东西。
- 插件日志只记录字节数，不记录输入内容。