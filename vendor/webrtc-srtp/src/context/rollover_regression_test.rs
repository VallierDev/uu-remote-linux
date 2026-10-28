use crate::{context::Context, protection_profile::ProtectionProfile};
fn packet(seq: u16) -> (Vec<u8>, rtp::header::Header) {
    let h = rtp::header::Header {
        version: 2,
        sequence_number: seq,
        ssrc: 1,
        ..Default::default()
    };
    let mut b = vec![0x80, 96];
    b.extend(seq.to_be_bytes());
    b.extend(0u32.to_be_bytes());
    b.extend(1u32.to_be_bytes());
    b.extend([1, 2, 3, 4]);
    (b, h)
}
fn run(profile: ProtectionProfile, retransmits: bool) -> usize {
    let mut tx = Context::new(
        &vec![0; 16],
        &vec![
            0;
            match profile {
                ProtectionProfile::Aes128CmHmacSha1_80 => 14,
                _ => 12,
            }
        ],
        profile,
        None,
        None,
    )
    .unwrap();
    let mut rx = Context::new(
        &vec![0; 16],
        &vec![
            0;
            match profile {
                ProtectionProfile::Aes128CmHmacSha1_80 => 14,
                _ => 12,
            }
        ],
        profile,
        None,
        None,
    )
    .unwrap();
    for seq in (65000u16..=65535).chain(0..=85) {
        let (b, h) = packet(seq);
        let e = tx.encrypt_rtp_with_header(&b, &h).unwrap();
        rx.decrypt_rtp(&e).unwrap();
    }
    if retransmits {
        for seq in [65000, 65530] {
            let (b, h) = packet(seq);
            let _ = tx.encrypt_rtp_with_header(&b, &h).unwrap();
        }
    }
    let mut failures = 0;
    for seq in 86..=1000 {
        let (b, h) = packet(seq);
        let e = tx.encrypt_rtp_with_header(&b, &h).unwrap();
        failures += usize::from(rx.decrypt_rtp(&e).is_err());
    }
    failures
}
#[test]
fn delayed_retransmissions_preserve_authentication() {
    for profile in [
        ProtectionProfile::Aes128CmHmacSha1_80,
        ProtectionProfile::AeadAes128Gcm,
    ] {
        assert_eq!(run(profile, false), 0);
        assert_eq!(run(profile, true), 0);
    }
}
