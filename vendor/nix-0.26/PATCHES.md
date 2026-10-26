# Local patch to nix 0.26.4

Source: crates.io nix 0.26.4 (MIT; upstream license retained).

LoongArch uses the generic Linux ioctl encoding, but nix 0.26 predates
`loongarch64` support and omits it from the `consts` cfg list. Add
`target_arch = "loongarch64"` to `src/sys/ioctl/linux.rs`; the constants and
encoding are the same as the other generic 64-bit architectures already listed.
This is the only source change.

This crate is vendored because webrtc-util 0.8.1 pins nix 0.26 and nix 0.27+
is not semver-compatible with that dependency.
