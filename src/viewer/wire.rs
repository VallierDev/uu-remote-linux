//! Shapes from viewer-connect-shape-20261016.md; values describe caller capabilities.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::protocol::{decode_wire, WireValue};

pub const CHANNELS: [&str; 5] = ["CONTROL_DATA_CHANNEL", "TEXT_DATA_CHANNEL", "STREAMER_DATA_CHANNEL", "FILE_DATA_CHANNEL", "BINARY_DATA_CHANNEL"];
pub fn uuid() -> String {
    let mut b: [u8;16] = rand::random(); b[6] = (b[6] & 15) | 64; b[8] = (b[8] & 63) | 128;
    let s = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &s[..8], &s[8..12], &s[12..16], &s[16..20], &s[20..])
}
pub fn varint(mut n: u64, out: &mut Vec<u8>) { while n >= 128 { out.push(n as u8 | 128); n >>= 7; } out.push(n as u8); }
pub fn int(field: u32, value: u64, out: &mut Vec<u8>) { varint(u64::from(field) << 3,out); varint(value,out); }
pub fn bytes(field: u32, value: &[u8], out: &mut Vec<u8>) { varint((u64::from(field)<<3)|2,out); varint(value.len() as u64,out); out.extend(value); }
pub fn envelope(seq:u64, kind:u32, payload:&[u8]) -> Vec<u8> {
    let mut out=Vec::new(); int(1,seq,&mut out); bytes(kind,payload,&mut out); out
}
#[derive(Clone, Deserialize)]
pub struct Decoder { pub fps:u32, pub codec:u32, pub width:u32, pub height:u32, pub chroma:u32 }
#[derive(Clone, Deserialize, Serialize)]
pub struct Display { pub id:u32, pub fps:u32, pub r#type:u32, pub hdr:u32 }
#[derive(Clone, Deserialize, Serialize)]
pub struct Codec { pub video_codec:u32, pub chroma_sampling:u32, pub codec_impl:u32, pub bit_depth:u32, pub width:u32, pub height:u32 }
#[derive(Clone, Deserialize)]
pub struct Shape {
    pub capture_type:u32,
    pub platform:u32,
    pub client_version:String,
    pub width:u32,
    pub height:u32,
    pub fps:u32,
    pub chroma:u32,
    pub decoders:Vec<Decoder>,
    pub display_modes:Vec<[u32;3]>,
    pub displays:Vec<Display>,
    pub codecs:Vec<Codec>,
    /// FeatureFlag {field: version} pairs (ConnectOptions 11). Empty by default; official hosts may
    /// require it (live: official Mac/Windows hosts did not answer an empty flag set).
    #[serde(default)]
    pub feature_flag:Vec<[u32;2]>,
}
impl Shape {
    pub fn validate(&self) -> Result<(),String> {
        if ![1,5,8,9].contains(&self.capture_type) || ![3,4].contains(&self.platform)
            || self.width==0 || self.height==0 || self.fps==0 || ![1,3].contains(&self.chroma) {
            return Err("Invalid explicit viewer shape".into());
        }
        if self.decoders.iter().any(|d| ![1,2].contains(&d.codec) || ![1,3].contains(&d.chroma) || d.width==0 || d.height==0 || d.fps==0) {
            return Err("Invalid decoder capability".into());
        }
        Ok(())
    }
    pub fn validate_session(&self)->Result<(),String> {
        self.validate()?;
        // H264/420 receive only. streamer_data video_codec_capability is sent by the official Mac
        // viewer (8 entries observed live), so H264/420 entries are allowed there too.
        if self.decoders.iter().any(|d|d.codec!=1 || d.chroma!=1) || self.codecs.iter().any(|c|c.video_codec!=1 || c.chroma_sampling!=1)
            || (self.capture_type==1 && (self.chroma!=1 || self.decoders.is_empty())) {
            return Err("Sessions require H264/420 decoder and codec capabilities".into());
        }
        Ok(())
    }
    pub fn connect_options(&self, device:&str, clipboard:bool) -> Result<Vec<u8>,String> {
        self.validate()?;
        let mut out=Vec::new(); int(1,self.capture_type.into(),&mut out);
        if [1,8].contains(&self.capture_type) { int(2,u64::MAX,&mut out); }
        if [1,5].contains(&self.capture_type) {
            let mut params=Vec::new(); int(1,2,&mut params); int(2,4,&mut params); int(4,2,&mut params);
            let mut size=Vec::new(); int(1,self.width.into(),&mut size); int(2,self.height.into(),&mut size);
            bytes(5,&size,&mut params); bytes(6,&[],&mut params);
            if self.capture_type==1 { int(7,self.chroma.into(),&mut params); int(9,1,&mut params); int(11,self.fps.into(),&mut params); }
            bytes(3,&params,&mut out);
            for d in &self.decoders {
                let mut cap=Vec::new();
                for (f,v) in [(1,d.fps),(2,d.codec),(3,d.width),(4,d.height)] { int(f,v.into(),&mut cap); }
                if self.capture_type==1 { int(5,d.chroma.into(),&mut cap); }
                bytes(4,&cap,&mut out);
            }
            for mode in &self.display_modes {
                let mut value=Vec::new(); for (f,v) in mode.iter().enumerate() { int(f as u32+1,(*v).into(),&mut value); }
                bytes(6,&value,&mut out);
            }
            bytes(7,&size,&mut out);
        }
        int(8,self.platform.into(),&mut out); bytes(9,device.as_bytes(),&mut out); int(10,1,&mut out);
        if self.capture_type==1 {
            let mut flags=Vec::new(); for [field,version] in &self.feature_flag { if *field!=8 {int((*field).into(),(*version).into(),&mut flags);} }
            if clipboard{int(8,3,&mut flags);}
            bytes(11,&flags,&mut out);
        }
        if self.capture_type!=5 { bytes(12,self.client_version.as_bytes(),&mut out); }
        Ok(out)
    }
    pub fn control(&self, client:&str, id:&str, ice:&str) -> Result<Value,String> {
        self.validate()?;
        let streamer=json!({"control_id":id,"device_capability":{"display_info":self.displays,"ice_id":"","video_codec_capability":self.codecs}});
        // Official Control::SerializeSioMsg (libstreamer 0x107bf8) sends only these three keys. A
        // viewer-chosen ice_id reaches the host via be-controlled while the ack assigns another, and the
        // official host drops the offer ("drop the soac event ... not in current ice connections").
        let _=(client,ice);
        Ok(json!({"app_control_id":id,"app_data":{"_placeholder":true,"num":0},"streamer_data":streamer.to_string()}))
    }
}

/// Existing viewer FeatureFlag pairs; clipboard uses the host text protocol version (3).
/// Session encoders omit clipboard when its local backend is disabled or unavailable.
pub const VIEWER_FEATURES:[[u32;2];8]=[[1,6],[2,2],[3,1],[4,2],[6,2],[7,2],[8,3],[9,1]];
pub fn feature_flag(flags:&[[u32;2]]) -> Vec<u8> { let mut out=Vec::new(); for [f,v] in flags { int(*f,(*v).into(),&mut out); } out }
pub fn echo_request(seq:u64) -> Vec<u8> {echo_request_with_clipboard(seq,false)}
fn session_features(clipboard:bool)->Vec<u8>{
    let mut flags:Vec<_>=VIEWER_FEATURES.iter().copied().filter(|p|p[0]!=8).collect();
    if clipboard{flags.push([8,3]);}feature_flag(&flags)
}
pub fn echo_request_with_clipboard(seq:u64,clipboard:bool)->Vec<u8>{
    let mut action=Vec::new();int(1,0,&mut action);bytes(4,&session_features(clipboard),&mut action);envelope(seq,3,&action)
}
pub fn echo_clipboard_supported(data:&[u8])->bool{
    let Ok(fields)=decode_wire(data) else{return false};
    fields.iter().any(|f|match (f.number,&f.value){
        (3,WireValue::Bytes(action))=>decode_wire(action).is_ok_and(|fields|fields.iter().any(|f|match (f.number,&f.value){
            (4,WireValue::Bytes(flags))=>decode_wire(flags).is_ok_and(|flags|flags.iter().any(|f|matches!((f.number,&f.value),(8,WireValue::Varint(3..))))),_=>false,
        })),_=>false,
    })
}
/// Official hosts also send an echo request (SimpleAction action 0) and repeat it until answered.
/// Returns the request's Message seq when `data` is such a request.
pub fn echo_request_seq(data:&[u8]) -> Option<u64> {
    let fields=decode_wire(data).ok()?;
    let seq=fields.iter().find_map(|f|match (f.number,&f.value){(1,WireValue::Varint(v))=>Some(*v),_=>None})?;
    fields.iter().find_map(|f|match (f.number,&f.value) {
        (3,WireValue::Bytes(action))=>decode_wire(action).ok()?.iter().any(|f|f.number==1 && f.value==WireValue::Varint(0)).then_some(seq),
        _=>None,
    })
}
/// Answer like the host side does: action 1, args {"seq": request seq}, our FeatureFlag.
pub fn echo_response(seq:u64,request_seq:u64)->Vec<u8>{echo_response_with_clipboard(seq,request_seq,false)}
pub fn echo_response_with_clipboard(seq:u64, request_seq:u64,clipboard:bool) -> Vec<u8> {
    let mut action=Vec::new(); int(1,1,&mut action);
    bytes(2,format!("{{\"seq\":{request_seq}}}").as_bytes(),&mut action);
    bytes(4,&session_features(clipboard),&mut action); envelope(seq,3,&action)
}
pub fn echo_matches(data:&[u8], seq:u64) -> bool {
    let Ok(fields)=decode_wire(data) else {return false};
    fields.iter().any(|f| match (f.number,&f.value) {
        (3,WireValue::Bytes(action))=> {
            let Ok(fields)=decode_wire(action) else {return false};
            let response=fields.iter().any(|f|f.number==1 && f.value==WireValue::Varint(1));
            response && fields.iter().any(|f|match (f.number,&f.value) {
                (2,WireValue::Bytes(args))=>serde_json::from_slice::<Value>(args).ok().and_then(|v|v.get("seq").and_then(Value::as_u64))==Some(seq), _=>false,
            })
        }, _=>false,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    pub fn shape()->Shape { serde_json::from_str(include_str!("../../tests/fixtures/viewer/shape.json")).unwrap() }
    #[test]
    fn session_shape_uses_observed_field_presence_and_nested_values() {
        let mut s=shape();
        for capture in [1,5,8,9] {
            s.capture_type=capture;
            let b=s.connect_options("synthetic-device",false).unwrap(); let f=decode_wire(&b).unwrap();
            let numbers:Vec<u32>=f.iter().map(|f|f.number).collect();
            let expected=match capture {1=>vec![1,2,3,4,4,4,4,6,6,7,8,9,10,11,12],5=>vec![1,3,4,4,4,4,6,6,7,8,9,10],8=>vec![1,2,8,9,10,12],_=>vec![1,8,9,10,12]};
            assert_eq!(numbers,expected);
            for f in f.iter().filter(|f|f.number==4) {
                let WireValue::Bytes(b)=f.value else {panic!()}; let cap=decode_wire(b).unwrap();
                assert_eq!(cap.len(),if capture==1 {5}else{4});
            }
            if [1,8].contains(&capture) { assert!(f.iter().any(|f| f.number==2 && f.value==WireValue::Varint(u64::MAX))); }
        }
    }
    #[test]
    fn streamer_identity_and_echo_correlation_are_separate_from_socket_ack() {
        let s=shape();let id=uuid(); let v=s.control("synthetic-client",&id,"synthetic-ice").unwrap();
        let streamer:Value=serde_json::from_str(v["streamer_data"].as_str().unwrap()).unwrap();
        assert_eq!(streamer["control_id"],v["app_control_id"]); assert_eq!(id.len(),36); assert_ne!(id,uuid()); assert_eq!(&id[14..15],"4");
        let mut action=Vec::new();int(1,1,&mut action);bytes(2,br#"{"seq":7}"#,&mut action);
        let reply=envelope(99,3,&action); assert!(echo_matches(&reply,7)); assert!(!echo_matches(&reply,99));
        assert!(!echo_matches(&echo_request(7),7)); assert!(!echo_matches(&[255],7));
    }
}
