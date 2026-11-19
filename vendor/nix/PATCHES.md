# Local patch to nix 0.26.4

Source: crates.io nix 0.26.4 (MIT; upstream license retained).

`webrtc-util 0.8.1` depends on `nix 0.26.2` (resolved to 0.26.4). In that release the ioctl
request-code tables in `src/sys/ioctl/linux.rs` are selected by explicit `target_arch` lists, and
`loongarch64` matches none of them: neither the "generic" table nor the mips/powerpc/sparc one, so
`pub use self::consts::*;` fails to resolve and the crate does not compile on loongarch64.

The single change adds `target_arch = "loongarch64"` to the "generic" table, which is the correct
one for LoongArch (same 8/8/14/2 bit layout as x86, arm, s390x, riscv). Architectures that already
matched a table are untouched, and nothing else in the crate is modified.

Only the crate tarball's contents are vendored (`Cargo.toml`, `Cargo.toml.orig`, `src`, `test`);
the registry's `.cargo-ok` marker and any editor backups are not.
