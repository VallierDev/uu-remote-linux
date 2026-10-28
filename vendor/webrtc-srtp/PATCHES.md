# Local patch to webrtc-srtp 0.12.0

Source: crates.io webrtc-srtp 0.12.0 (MIT / Apache-2.0; upstream licenses retained).

`SrtpSsrcState` estimates the rollover counter with the half-sequence-space rule from
RFC 3711 Appendix A and only advances to the highest packet index seen. Upstream used a
100-packet window and stored every sequence number, so a NACK retransmission of a
pre-wrap packet sent after the wrap (e.g. 65000 after 85) moved the state back and the
next new packet was protected with ROC+1; the receiver then failed authentication for
every following packet (frozen video, #83). Fix and regression tests contributed by
@AYssu. Cipher code, wire format and SRTCP are unchanged.

The upstream lifecycle test jumped from 5004 to 65535, outside the half-space window;
bridge packets keep it consecutive and the known-answer vectors are unchanged.
