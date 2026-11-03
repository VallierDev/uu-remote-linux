# webrtc-util 0.8.1

Copied from the crates.io 0.8.1 release (MIT/Apache-2.0), corresponding to
webrtc-rs/webrtc commit a9383c8c59f68e58119f86334f7b55e25b64b70e, directory `util`.
The release archive SHA-256 is
`5e85154ef743d9a2a116d104faaaa82740a281b8b4bed5ee691a2df6c133d873`.

The only functional change is in the normalized `Cargo.toml`: keep the upstream
`nix = "0.26.2"` requirement (locked to 0.26.4), disable its default features,
and enable only `net` and `socket`. These cover `ifaces/ffi/unix/mod.rs` without
compiling unsupported OHOS AIO, POSIX message queue and filesystem APIs.
Rust sources and `Cargo.toml.orig` remain identical to the published crate;
`CHANGELOG.md` only has its trailing blank line normalized.

Only the generated OHOS Cargo project created by `scripts/build-harmony.py`
patches crates.io to this directory. The Linux root manifest and lockfile retain
the upstream registry dependency and features. OHOS has its own lockfile in
`harmony/Cargo.lock`; no dependency override is applied to normal Linux builds.
