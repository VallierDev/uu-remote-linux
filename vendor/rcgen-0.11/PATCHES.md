# Local patch to rcgen 0.11.3

Source: crates.io rcgen 0.11.3 (MIT / Apache-2.0; upstream licenses retained).

ring 0.16 has no loongarch64 backend; ring 0.17 is the first release that
does. Bump the ring requirement and adapt to the ring 0.17 API:

- `KeyRejected::description_()` was removed; use `to_string()` and store the
  message in `RingKeyRejected(String)` instead of `&'static str`.
- `EcdsaKeyPair::from_pkcs8` now requires a `&dyn SecureRandom`; pass
  `SystemRandom::new()` at the four call sites.

Certificate generation, serialization and wire behavior are unchanged.
