use super::media::Latest;
use std::sync::{Arc, atomic::{AtomicBool,AtomicU32,Ordering}};
#[derive(Clone)]
pub struct Desktop { pub units:Latest<super::media::AccessUnit>, pub pli:Arc<AtomicBool>, pub reset:Arc<AtomicBool>, pub ssrc:Arc<AtomicU32> }
impl Default for Desktop {
    fn default()->Self {Self{units:Latest::new(3),pli:Arc::new(AtomicBool::new(false)),reset:Arc::new(AtomicBool::new(false)),ssrc:Arc::new(AtomicU32::new(0))}}
}
/// `content` is the source picture size inside the (possibly padded) RGB canvas; None = full frame.
pub struct Frame { pub width:u32, pub height:u32, pub rgb:Vec<u8>, pub content:Option<(u32,u32)> }
pub trait Decoder { fn set_codec(&mut self,codec:super::media::Codec)->Result<(),String>; fn decode(&mut self, au:&[u8])->Result<Vec<Frame>,String>; fn reset(&mut self);
    /// Frames finished since the last call, without new input; an asynchronous decoder
    /// returns a picture here instead of holding it until the next access unit arrives.
    fn poll(&mut self)->Vec<Frame> {Vec::new()} }
pub trait FrameSink { fn present(&mut self,frame:&Frame)->Result<(),String>; }
/// Counts only (access units, presented frames, decode errors) for the periodic stats line.
pub static STATS:[std::sync::atomic::AtomicU64;3]=[std::sync::atomic::AtomicU64::new(0),std::sync::atomic::AtomicU64::new(0),std::sync::atomic::AtomicU64::new(0)];
fn wants_key_frame(units:u64,presented:u64)->bool {presented==0 && units%30==0}
/// True when an Annex-B access unit carries a picture the decoder can restart from: an IDR/IRAP,
/// an H.264 recovery-point SEI or an H.264 I slice (some hosts answer PLI without an IDR, #136).
pub fn is_key_frame(codec:super::media::Codec,au:&[u8])->bool {
    au.windows(4).enumerate().filter(|(_,w)|w[..3]==[0,0,1]).any(|(i,w)|match codec {
        super::media::Codec::H264=>match w[3]&0x1f {5=>true,6=>au.get(i+4)==Some(&6),1=>h264_i_slice(&au[i+4..]),_=>false},
        super::media::Codec::H265=>(16..=21).contains(&((w[3]>>1)&0x3f)),
    })
}
/// Reads first_mb_in_slice and slice_type (both ue(v)) from a slice header; I = 2 or 7.
fn h264_i_slice(rbsp:&[u8])->bool {
    let mut bit=0usize;
    let mut ue=||->Option<u32>{
        let mut zeros=0;
        while (rbsp.get(bit/8)?>>(7-bit%8))&1==0 {zeros+=1;bit+=1;if zeros>31 {return None}}
        bit+=1;let mut v=0u32;
        for _ in 0..zeros {v=(v<<1)|u32::from((rbsp.get(bit/8)?>>(7-bit%8))&1);bit+=1;}
        Some((1u32<<zeros)-1+v)
    };
    ue().is_some() && matches!(ue(),Some(2|7))
}
/// After a local reset, holds back pictures until a restart point arrives, but never longer
/// than `limit`: a host that ignores PLI must not leave the window blank (#136).
pub struct KeyGate { since:Option<std::time::Instant>, limit:std::time::Duration }
impl KeyGate {
    pub fn new()->Self {Self{since:None,limit:std::time::Duration::from_millis(1500)}}
    pub fn arm(&mut self) {self.since.get_or_insert_with(std::time::Instant::now);}
    /// False = drop this unit and keep asking for a key frame.
    pub fn admit(&mut self,codec:super::media::Codec,au:&[u8])->bool {
        let Some(since)=self.since else {return true};
        let timed_out=since.elapsed()>=self.limit;
        if timed_out || is_key_frame(codec,au) {
            if timed_out {log::debug!("Viewer desktop: no key frame within {:?} after reset; decoding on",self.limit);}
            self.since=None;true
        } else {false}
    }
}
/// Shows the newest frame a decoder finished between access units.
pub fn drain(decoder:&mut dyn Decoder,sink:&mut dyn FrameSink)->Result<(),String> {
    if let Some(frame)=decoder.poll().pop() {STATS[1].fetch_add(1,Ordering::Relaxed);sink.present(&frame)?;}
    Ok(())
}
pub fn display(decoder:&mut dyn Decoder,sink:&mut dyn FrameSink,au:&[u8],codec:super::media::Codec,pli:&AtomicBool)->Result<(),String> {
    let units=STATS[0].fetch_add(1,Ordering::Relaxed)+1;
    // A later session can join a clean stream with no IDR in flight (#124): keep asking until a frame shows.
    if wants_key_frame(units,STATS[1].load(Ordering::Relaxed)) {
        pli.store(true,Ordering::Release);
        if units==300 { log::warn!("Viewer desktop: no decodable frame after {units} access units; requesting key frames (another session may hold the host)"); }
    }
    // Local diagnostic: UU_VIEWER_DUMP_H264=path appends the first 300 access units (owner's own stream).
    if codec==super::media::Codec::H264 && STATS[0].load(Ordering::Relaxed)<=300 { if let Some(path)=std::env::var_os("UU_VIEWER_DUMP_H264") {
        use std::io::Write; if let Ok(mut f)=std::fs::OpenOptions::new().create(true).append(true).open(path){let _=f.write_all(au);}
    }}
    match decoder.decode(au) {
        Ok(frames)=>{for frame in frames {
            sink.present(&frame)?;
            // Local acceptance aid: UU_VIEWER_DUMP_FRAME=path writes the 5th frame as PPM (owner's own screen).
            if STATS[1].fetch_add(1,Ordering::Relaxed)==std::env::var("UU_VIEWER_DUMP_FRAME_INDEX").ok().and_then(|v|v.parse().ok()).unwrap_or(4u64) { if let Some(path)=std::env::var_os("UU_VIEWER_DUMP_FRAME") {
                let mut ppm=format!("P6\n{} {}\n255\n",frame.width,frame.height).into_bytes();ppm.extend_from_slice(&frame.rgb);let _=std::fs::write(path,ppm);
            }}
        }Ok(())},
        Err(_)=>{STATS[2].fetch_add(1,Ordering::Relaxed);decoder.reset();pli.store(true,Ordering::Release);Ok(())},
    }
}
#[cfg(test)] mod tests {
    #[test] fn key_frames_by_nal_type(){
        use crate::viewer::media::Codec;
        assert!(super::is_key_frame(Codec::H264,&[0,0,0,1,0x67,1,0,0,0,1,0x68,2,0,0,0,1,0x65,3]));
        assert!(!super::is_key_frame(Codec::H264,&[0,0,0,1,0x61,1]));
        assert!(super::is_key_frame(Codec::H265,&[0,0,0,1,0x40,1,0,0,1,0x26,1,3]));
        assert!(!super::is_key_frame(Codec::H265,&[0,0,0,1,0x02,1]));
        // I slice (first_mb=0 '1', slice_type=7 '0001000') and P slice (slice_type=5 '00110').
        assert!(super::is_key_frame(Codec::H264,&[0,0,0,1,0x41,0b1000_1000,0]));
        assert!(!super::is_key_frame(Codec::H264,&[0,0,0,1,0x41,0b1001_1000]));
        // Recovery-point SEI (payload type 6).
        assert!(super::is_key_frame(Codec::H264,&[0,0,0,1,0x06,0x06,1,0x80]));
    }
    #[test] fn key_gate_waits_then_gives_up(){
        use crate::viewer::media::Codec;
        let mut g=super::KeyGate::new();let p=[0,0,0,1,0x41,0b1001_1000];
        assert!(g.admit(Codec::H264,&p));
        g.arm();assert!(!g.admit(Codec::H264,&p));
        assert!(g.admit(Codec::H264,&[0,0,0,1,0x65,0x88]));assert!(g.admit(Codec::H264,&p));
        g.limit=std::time::Duration::ZERO;g.arm();assert!(g.admit(Codec::H264,&p));
    }
    use super::*;
    struct Mock(bool);impl Decoder for Mock {fn set_codec(&mut self,_:crate::viewer::media::Codec)->Result<(),String>{Ok(())} fn decode(&mut self,_:&[u8])->Result<Vec<Frame>,String>{if self.0{Err("synthetic".into())}else{Ok(vec![Frame{width:1,height:1,rgb:vec![0;3],content:None}])}} fn reset(&mut self){self.0=false;}}
    struct Sink(usize);impl FrameSink for Sink{fn present(&mut self,f:&Frame)->Result<(),String>{assert_eq!(f.rgb.len(),3);self.0+=1;Ok(())}}
    #[test] fn requests_key_frames_until_the_first_frame(){
        assert_eq!((1..=90).filter(|&n|wants_key_frame(n,0)).count(),3);
        assert!(!(1..=90).any(|n|wants_key_frame(n,1)));
    }
    #[test] fn decode_error_requests_pli_and_recovers(){let mut d=Mock(true);let mut s=Sink(0);let p=AtomicBool::new(false);display(&mut d,&mut s,&[],crate::viewer::media::Codec::H264,&p).unwrap();assert!(p.load(Ordering::Acquire));display(&mut d,&mut s,&[],crate::viewer::media::Codec::H264,&p).unwrap();assert_eq!(s.0,1);}
}
