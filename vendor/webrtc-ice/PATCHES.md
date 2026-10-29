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

## Round-trip time

Upstream never measures it: `CandidatePairStats::current_round_trip_time` is always 0. A successful
binding response now records `now - request timestamp` on its candidate pair, and the stats report
the latest sample. Keepalive pings only an idle pair, which a streaming session never is, so the
selected pair is also pinged when its last sample is older than the keepalive interval (a binding
request, like a consent check from RFC 7675).

The host (`uuremote-server`) uses the same crate, so on the selected pair it also sends one
binding request per keepalive interval (2 s) while media flows. Checked on the host side with the
official Android client controlling a Linux host (GNOME, cloud ICE servers incl. TURN) over repeated
sessions: the client answers every request and sessions stay up. Official Windows/macOS controllers
have not been tried against the host yet.
