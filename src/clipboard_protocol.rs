//! Official GvPb_Message field 17 / ClipboardChange (UInt32, String, Bool).
//! Source-bound field/type evidence is retained in control/CLIPBOARD.md.
use crate::protocol::{decode_wire, WireValue};
use crate::clipboard::MAX_TEXT;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Assembly {
    pending: Option<(u32, String, Instant)>,
}
impl Assembly {
    pub fn accept(&mut self, message: &[u8]) -> Result<Option<String>, &'static str> {
        let result = self.decode(message);
        if result.is_err() { self.pending = None; }
        result
    }
    fn decode(&mut self, message: &[u8]) -> Result<Option<String>, &'static str> {
        if message.len() > MAX_TEXT + 256 { return Err("clipboard message exceeds limit"); }
        let fields = decode_wire(message).map_err(|_| "invalid clipboard wire")?;
        let mut id = 0;
        let mut text = "";
        let mut more = false;
        let mut seen = [false; 3];
        for field in fields {
            if !(1..=3).contains(&field.number) { continue; }
            let index = field.number as usize - 1;
            if seen[index] { return Err("duplicate clipboard field"); }
            seen[index] = true;
            match (field.number, field.value) {
                (1, WireValue::Varint(value)) => id = u32::try_from(value).map_err(|_| "invalid clipboard content id")?,
                (2, WireValue::Bytes(value)) => text = std::str::from_utf8(value).map_err(|_| "invalid clipboard UTF-8")?,
                (3, WireValue::Varint(value @ 0..=1)) => more = value != 0,
                _ => return Err("invalid clipboard field type"),
            }
        }
        let now = Instant::now();
        let mut combined = match self.pending.take() {
            Some((previous, text, started)) if previous == id && now.duration_since(started) < Duration::from_secs(5) => (text, started),
            _ => (String::new(), now),
        };
        if combined.0.len() + text.len() > MAX_TEXT { return Err("clipboard text exceeds limit"); }
        combined.0.push_str(text);
        if more { self.pending = Some((id, combined.0, combined.1)); Ok(None) }
        else { Ok(Some(combined.0)) }
    }
}

pub(crate) fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 { out.push(value as u8 & 127 | 128); value >>= 7; }
    out.push(value as u8);
}
pub(crate) fn nested(field: u32, payload: &[u8], out: &mut Vec<u8>) {
    varint(u64::from(field) << 3 | 2, out);
    varint(payload.len() as u64, out); out.extend_from_slice(payload);
}
pub fn text_request(request_id: u64, text: &str) -> Vec<u8> {
    text_chunk_request(request_id, text, false)
}
/// One ClipboardChange RPC (field 10). Official hosts send these next to the rich offer:
/// mobile viewers only act on this form and ignore the rich format list.
pub fn text_chunk_request(request_id: u64, part: &str, more: bool) -> Vec<u8> {
    let mut header=vec![8];varint(request_id,&mut header);
    // Official ClipboardMobileManager uses format_id=1 for text.
    let data=encode(1,part,more);
    let mut request=Vec::new();nested(1,&header,&mut request);nested(10,&data,&mut request);request
}
pub fn read_text_request(request: &[u8]) -> Result<Option<(Vec<u8>,String)>, &'static str> {
    if request.len()>MAX_TEXT+256 {return Err("clipboard RPC exceeds limit");}
    let fields=decode_wire(request).map_err(|_|"invalid clipboard RPC")?;
    let data=fields.iter().find(|f|f.number==10);
    let Some(data)=data else {return Ok(None);};
    let header=fields.iter().find(|f|f.number==1).ok_or("missing clipboard RPC header")?;
    let WireValue::Bytes(header)=header.value else {return Err("invalid clipboard RPC header");};
    if header.len()>32 {return Err("invalid clipboard RPC header length");}
    let header_fields=decode_wire(header).map_err(|_|"invalid clipboard RPC request id")?;
    if header_fields.iter().any(|f|f.number!=1 || !matches!(f.value,WireValue::Varint(_))) {return Err("invalid clipboard RPC request id");}
    let WireValue::Bytes(data)=data.value else {return Err("invalid clipboard RPC text");};
    let parts=decode_wire(data).map_err(|_|"invalid clipboard RPC text")?;
    if !parts.iter().any(|f|f.number==1 && f.value==WireValue::Varint(1)) || parts.iter().any(|f|f.number==1 && f.value!=WireValue::Varint(1)) {return Err("unsupported clipboard RPC format");}
    let text=Assembly::default().accept(data)?.ok_or("incomplete clipboard RPC text")?;
    Ok(Some((header.to_vec(),text)))
}
pub fn text_response(header: &[u8]) -> Vec<u8> {
    let mut response=Vec::new();nested(1,header,&mut response);
    nested(6,&[8,1],&mut response); // TextChangeResponse.err=CB_SUCCEEDED
    response
}
pub fn encode(id: u32, text: &str, more: bool) -> Vec<u8> {
    let mut out = vec![8]; varint(u64::from(id), &mut out);
    out.push(18); varint(text.len() as u64, &mut out); out.extend_from_slice(text.as_bytes());
    if more { out.extend_from_slice(&[24, 1]); }
    out
}

pub fn chunks(text: &str) -> Result<Vec<&str>, &'static str> {
    if text.len() > MAX_TEXT { return Err("clipboard text exceeds limit"); }
    if text.is_empty() { return Ok(vec![""]); }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let mut end = remaining.len().min(8192);
        while !remaining.is_char_boundary(end) { end -= 1; }
        chunks.push(&remaining[..end]); remaining = &remaining[end..];
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rpc_text_roundtrip_preserves_request_id_and_returns_official_success() {
        let text="UU-test-中文🙂\n";
        let request=text_request(0x12345678,text);
        let (header,received)=read_text_request(&request).unwrap().unwrap();
        assert_eq!(received,text);
        assert_eq!(header,vec![8,0xf8,0xac,0xd1,0x91,1]);
        let response=text_response(&header);
        assert_eq!(&response[response.len()-4..],&[50,2,8,1]);
        assert_eq!(&response[2..2+header.len()],header.as_slice());
        assert!(read_text_request(&[10,1,255,82,0]).is_err());
        assert!(read_text_request(&[10,0,82,2,8,2]).is_err());
        assert!(read_text_request(&[10,0,74,0]).unwrap().is_none());
    }
    #[test]
    fn chunked_text_rpc_assembles_by_content_id_and_single_chunk_reads_as_request() {
        let text="中文🙂".repeat(3000); // > 8192 bytes: at least two chunks
        let parts=chunks(&text).unwrap();
        assert!(parts.len()>1);
        let mut assembly=Assembly::default();
        for (index,part) in parts.iter().enumerate() {
            let rpc=text_chunk_request(0x42+index as u64,part,index+1<parts.len());
            let inner=decode_wire(&rpc).unwrap();
            let WireValue::Bytes(data)=inner.iter().find(|f|f.number==10).unwrap().value else {panic!("field 10 missing")};
            if index+1==parts.len(){assert_eq!(assembly.accept(data).unwrap().as_deref(),Some(text.as_str()));}
            else{assert!(assembly.accept(data).unwrap().is_none());}
        }
        let single=text_chunk_request(7,"short",false);
        assert_eq!(read_text_request(&single).unwrap().unwrap().1,"short");
    }
    #[test]
    fn unicode_chunk_roundtrip_and_empty_copy() {
        let text = "中文🙂\n".repeat(1600);
        let parts = chunks(&text).unwrap(); assert!(parts.len() > 1);
        let mut assembly = Assembly::default();
        for (i, part) in parts.iter().enumerate() {
            let result = assembly.accept(&encode(u32::MAX, part, i + 1 < parts.len())).unwrap();
            if i + 1 == parts.len() { assert_eq!(result.as_deref(), Some(text.as_str())); }
            else { assert!(result.is_none()); }
        }
        assert_eq!(assembly.accept(&encode(0,"",false)).unwrap(),Some(String::new()));
    }
    #[test]
    fn resets_for_new_id_and_rejects_malformed_and_oversized_text() {
        let mut assembly = Assembly::default();
        assembly.accept(&encode(1,"stale",true)).unwrap();
        assert_eq!(assembly.accept(&encode(2,"new",false)).unwrap(),Some("new".into()));
        assert!(assembly.accept(&[18,1,255]).is_err());
        assert!(assembly.accept(&[8,1,8,2]).is_err());
        assert!(assembly.accept(&[24,2]).is_err());
        assert!(chunks(&"a".repeat(MAX_TEXT+1)).is_err());
        assembly.accept(&encode(1,&"a".repeat(MAX_TEXT),true)).unwrap();
        assert!(assembly.accept(&encode(1,"b",false)).is_err());
        assert_eq!(assembly.accept(&encode(1,"fresh",false)).unwrap(),Some("fresh".into()));
    }
}
