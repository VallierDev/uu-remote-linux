# webrtc-util 0.8.1

Copied from the crates.io release (MIT/Apache-2.0). The Unix `nix` dependency is
0.29 with only `net` and `socket` enabled, instead of 0.26 with all default
features. These are the only modules used by `ifaces/ffi/unix/mod.rs`. This avoids
unsupported AIO, POSIX message queues and legacy libc types when targeting
`aarch64-unknown-linux-ohos`. No WebRTC protocol behavior is changed.
