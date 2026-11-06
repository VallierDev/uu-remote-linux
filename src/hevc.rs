//! RFC 7798 §4.4.1 / §4.4.3, single-layer non-interleaved HEVC (no DONL).
//! Parameter sets travel in-band before each IDR; no aggregation is required.
use bytes::Bytes;

pub fn payload(mtu: usize, annex_b: &[u8]) -> Result<Vec<Bytes>, String> {
    if mtu < 4 { return Err("HEVC payload MTU too small".into()); }
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annex_b.len() {
        let prefix = if annex_b[i..].starts_with(&[0, 0, 0, 1]) { 4 }
            else if annex_b[i..].starts_with(&[0, 0, 1]) { 3 } else { i += 1; continue; };
        starts.push((i, i + prefix)); i += prefix;
    }
    if starts.is_empty() { return Err("HEVC access unit has no Annex-B start code".into()); }
    let mut out = Vec::new();
    for (index, &(_, start)) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).map_or(annex_b.len(), |s| s.0);
        while end > start && annex_b[end - 1] == 0 { end -= 1; } // trailing_zero_8bits
        let nal = &annex_b[start..end];
        if nal.len() < 2 || nal[0] & 0x80 != 0 || nal[1] & 7 == 0 {
            return Err("Invalid HEVC NAL header".into());
        }
        let kind = (nal[0] >> 1) & 63;
        if kind >= 48 { return Err("HEVC Annex-B contains RTP packet type".into()); }
        if nal.len() <= mtu { out.push(Bytes::copy_from_slice(nal)); continue; }
        let chunks = nal[2..].chunks(mtu - 3);
        let last = chunks.len() - 1;
        for (n, chunk) in chunks.enumerate() {
            let mut fu = Vec::with_capacity(3 + chunk.len());
            // Keep F, LayerId and TID, replacing only the NAL type with FU=49.
            fu.extend_from_slice(&[(nal[0] & 0x81) | (49 << 1), nal[1],
                kind | if n == 0 { 0x80 } else { 0 } | if n == last { 0x40 } else { 0 }]);
            fu.extend_from_slice(chunk); out.push(fu.into());
        }
    }
    Ok(out)
}

/// Payload HEVC Annex-B bitstream with zero-copy sub-slicing for single NAL units <= MTU.
pub fn payload_bytes(mtu: usize, annex_b: &Bytes) -> Result<Vec<Bytes>, String> {
    if mtu < 4 { return Err("HEVC payload MTU too small".into()); }
    let mut starts = Vec::new();
    let mut i = 0;
    let bytes = annex_b.as_ref();
    while i + 3 <= bytes.len() {
        let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) { 4 }
            else if bytes[i..].starts_with(&[0, 0, 1]) { 3 } else { i += 1; continue; };
        starts.push((i, i + prefix)); i += prefix;
    }
    if starts.is_empty() { return Err("HEVC access unit has no Annex-B start code".into()); }
    let mut out = Vec::new();
    for (index, &(_, start)) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).map_or(bytes.len(), |s| s.0);
        while end > start && bytes[end - 1] == 0 { end -= 1; } // trailing_zero_8bits
        let nal_slice = &bytes[start..end];
        if nal_slice.len() < 2 || nal_slice[0] & 0x80 != 0 || nal_slice[1] & 7 == 0 {
            return Err("Invalid HEVC NAL header".into());
        }
        let kind = (nal_slice[0] >> 1) & 63;
        if kind >= 48 { return Err("HEVC Annex-B contains RTP packet type".into()); }
        if nal_slice.len() <= mtu {
            // Zero-copy reference-counted sub-slice:
            out.push(annex_b.slice(start..end));
            continue;
        }
        let chunks = nal_slice[2..].chunks(mtu - 3);
        let last = chunks.len() - 1;
        for (n, chunk) in chunks.enumerate() {
            let mut fu = Vec::with_capacity(3 + chunk.len());
            // Keep F, LayerId and TID, replacing only the NAL type with FU=49.
            fu.extend_from_slice(&[(nal_slice[0] & 0x81) | (49 << 1), nal_slice[1],
                kind | if n == 0 { 0x80 } else { 0 } | if n == last { 0x40 } else { 0 }]);
            fu.extend_from_slice(chunk); out.push(fu.into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parameter_sets_and_small_nals_remain_in_order_without_start_codes() {
        assert_eq!(payload(1200, &[0,0,0,1,64,1,9,0,0,1,66,1,8,0,0,1,38,1,7]).unwrap(),
            vec![Bytes::from_static(&[64,1,9]), Bytes::from_static(&[66,1,8]), Bytes::from_static(&[38,1,7])]);
    }
    #[test]
    fn fu_round_trip_preserves_header_payload_and_mtu_boundaries() {
        for mtu in [4, 5, 7, 1190] {
            let header = [0x27, 0x2b]; // IDR type19, nonzero layer and temporal id
            let data: Vec<u8> = (0..2501).map(|i| (i % 251 + 1) as u8).collect();
            let mut unit = vec![0,0,0,1]; unit.extend(header); unit.extend(&data);
            let packets = payload(mtu, &unit).unwrap();
            assert!(packets.len() > 1);
            let mut rebuilt = vec![(packets[0][0] & 0x81) | ((packets[0][2] & 63) << 1), packets[0][1]];
            for (i, p) in packets.iter().enumerate() {
                assert!(p.len() <= mtu); assert_eq!((p[0] >> 1) & 63, 49);
                assert_eq!(p[2] & 0x80 != 0, i == 0);
                assert_eq!(p[2] & 0x40 != 0, i + 1 == packets.len());
                rebuilt.extend_from_slice(&p[3..]);
            }
            assert_eq!(&rebuilt[..2], &header); assert_eq!(&rebuilt[2..], &data);
        }
    }
    #[test]
    fn rejects_invalid_headers_and_too_small_mtu() {
        for bytes in [&[0,0,1,64][..], &[0,0,1,64,0][..], &[0,0,1,128,1][..], &[64,1][..]] {
            assert!(payload(1200, bytes).is_err());
        }
        assert!(payload(3, &[0,0,1,64,1]).is_err());
    }
}
