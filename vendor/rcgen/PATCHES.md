# Local patch to rcgen 0.11.3

Source: crates.io rcgen 0.11.3 (MIT / Apache-2.0; upstream licenses retained).

Upstream pins `ring = "0.16"`, and ring 0.16.20 has no loongarch64 support: its P-256 and
curve25519 implementations only exist as x86/arm assembly, so linking a loongarch64 binary fails
with undefined `GFp_nistz256_*` / `GFp_x25519_*` symbols. ring 0.17.x builds on loongarch64
(pure Rust for those curves), and `vendor/webrtc-dtls` already requires ring 0.17.

Changes, kept to the minimum needed to compile against ring 0.17 on every architecture:

- `Cargo.toml`: `ring = "0.17"` (instead of `0.16`). The Cargo.toml here is the normalized one
  cargo writes into the crate tarball; `Cargo.toml.orig` keeps the upstream text.
- `src/error.rs`: `KeyRejected::description_()` no longer exists in ring 0.17;
  `RcgenError::RingKeyRejected` now takes a `&'static str`, so the message comes from
  `Box::leak(format!("{:?}", err).into_boxed_str())`.
- `src/key_pair.rs`: `EcdsaKeyPair::from_pkcs8` takes the `&SystemRandom` as a third argument.

No behavioural change: the same key types, the same certificate encoding. Builds for x86_64,
aarch64 and loongarch64 use the same code path; ring 0.17 still selects its assembly backends on
x86/arm, so the other architectures keep their optimized implementations.
