# Local patch to nix 0.26.4

Source: crates.io nix 0.26.4 (MIT; upstream license retained).

Two changes. Only the crate tarball's contents are vendored (`Cargo.toml`, `Cargo.toml.orig`,
`src`, `test`); the registry's `.cargo-ok` marker and any editor backups are not.

## 1. loongarch64 ioctl table (`src/sys/ioctl/linux.rs`)

`webrtc-util 0.8.1` depends on `nix 0.26.2` (resolved to 0.26.4). In that release the ioctl
request-code tables in `src/sys/ioctl/linux.rs` are selected by explicit `target_arch` lists, and
`loongarch64` matches none of them: neither the "generic" table nor the mips/powerpc/sparc one, so
`pub use self::consts::*;` fails to resolve and the crate does not compile on loongarch64.

The change adds `target_arch = "loongarch64"` to the "generic" table, which is the correct one for
LoongArch (same 8/8/14/2 bit layout as x86, arm, s390x, riscv). Architectures that already matched
a table are untouched.

## 2. dead_code exemption (`src/lib.rs`)

Not LoongArch-specific. `src/lib.rs` carries `#![deny(unused)]`, which never fired while the crate
came from crates.io, because cargo compiles registry dependencies with `--cap-lints allow`. As a
path dependency that cap is gone and the dead_code half of the lint becomes an error for items this
project never reaches:

```
error: fields `1` and `2` are never read
error: struct `GetOsString` is never constructed
error: struct `SetOsString` is never constructed
```

Which items are dead also differs per target — the socket-option helpers in
`src/sys/socket/sockopt.rs` are used on Linux but not on macOS — so the failure shows up on some
hosts and not others. Add `#![allow(dead_code)]` and keep `deny(unused)` for the remaining unused
lints (imports, variables, ...) instead of downgrading the whole lint to `warn`.
