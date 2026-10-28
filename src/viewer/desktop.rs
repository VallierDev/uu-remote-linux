use super::media::Latest;
use std::sync::{Arc, atomic::{AtomicBool,AtomicU32,Ordering}};
#[derive(Clone)]
pub struct Desktop { pub units:Latest<super::media::AccessUnit>, pub pli:Arc<AtomicBool>, pub reset:Arc<AtomicBool>, pub ssrc:Arc<AtomicU32> }
impl Default for Desktop {
    fn default()->Self {Self{units:Latest::new(3),pli:Arc::new(AtomicBool::new(false)),reset:Arc::new(AtomicBool::new(false)),ssrc:Arc::new(AtomicU32::new(0))}}
}
/// `content` is the source picture size inside the (possibly padded) RGB canvas; None = full frame.
pub struct Frame { pub width:u32, pub height:u32, pub rgb:Vec<u8>, pub content:Option<(u32,u32)> }
pub trait Decoder { fn set_codec(&mut self,codec:super::media::Codec)->Result<(),String>; fn decode(&mut self, au:&[u8])->Result<Vec<Frame>,String>; fn reset(&mut self); }
pub trait FrameSink { fn present(&mut self,frame:&Frame)->Result<(),String>; }
/// Counts only (access units, presented frames, decode errors) for the periodic stats line.
pub static STATS:[std::sync::atomic::AtomicU64;3]=[std::sync::atomic::AtomicU64::new(0),std::sync::atomic::AtomicU64::new(0),std::sync::atomic::AtomicU64::new(0)];
fn wants_key_frame(units:u64,presented:u64)->bool {presented==0 && units%30==0}
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
    use super::*;
    struct Mock(bool);impl Decoder for Mock {fn set_codec(&mut self,_:crate::viewer::media::Codec)->Result<(),String>{Ok(())} fn decode(&mut self,_:&[u8])->Result<Vec<Frame>,String>{if self.0{Err("synthetic".into())}else{Ok(vec![Frame{width:1,height:1,rgb:vec![0;3],content:None}])}} fn reset(&mut self){self.0=false;}}
    struct Sink(usize);impl FrameSink for Sink{fn present(&mut self,f:&Frame)->Result<(),String>{assert_eq!(f.rgb.len(),3);self.0+=1;Ok(())}}
    #[test] fn requests_key_frames_until_the_first_frame(){
        assert_eq!((1..=90).filter(|&n|wants_key_frame(n,0)).count(),3);
        assert!(!(1..=90).any(|n|wants_key_frame(n,1)));
    }
    #[test] fn decode_error_requests_pli_and_recovers(){let mut d=Mock(true);let mut s=Sink(0);let p=AtomicBool::new(false);display(&mut d,&mut s,&[],crate::viewer::media::Codec::H264,&p).unwrap();assert!(p.load(Ordering::Acquire));display(&mut d,&mut s,&[],crate::viewer::media::Codec::H264,&p).unwrap();assert_eq!(s.0,1);}
}
