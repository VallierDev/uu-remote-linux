# Local patch to x509-parser 0.15.1

Source: crates.io x509-parser 0.15.1 (MIT / Apache-2.0; upstream licenses retained).

ring 0.16 has no loongarch64 backend; ring 0.17 is the first release that
does. Bump the optional ring requirement to 0.17 and allow
`unused_qualifications` at crate level, because ring 0.17's re-exports trigger
the existing deny lint. Parsing and verification behavior is unchanged.
