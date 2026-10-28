# Local patch to webrtc-ice 0.10.1

Source: crates.io webrtc-ice 0.10.1 (MIT / Apache-2.0; upstream licenses retained).

Upstream gathers relay candidates only for `turn:` over UDP and logs "Unable to handle URL" for
`turn:?transport=tcp` and `turns:`. NetEase sessions that force relaying also offer
`turns:<host>:<port>?transport=tcp`, which is the only media path on hosts whose UDP cannot reach
the TURN servers but whose TCP goes through an HTTP proxy.

`src/agent/turn_tcp.rs` opens TCP (directly, or through an HTTP CONNECT proxy set by the
application with `set_relay_transport`), adds TLS for `turns:`, and exposes the stream to the
unchanged `turn` client as datagrams: one STUN message or ChannelData frame (with its 4-byte TCP
padding) per read. `agent_gather.rs` uses it for `ProtoType::Tcp` URLs. UDP gathering is unchanged.

TLS trusts the system certificate store (`rustls-native-certs`, as OpenSSL does) plus the bundled
Mozilla roots. A name mismatch is accepted only when the TURN URL is an IP literal.
