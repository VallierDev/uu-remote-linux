use super::media::Latest;
use std::sync::{Arc, atomic::{AtomicBool,AtomicU32,Ordering}};
#[derive(Clone)]
pub struct Desktop { pub units:Latest<super::media::AccessUnit>, pub pli:Arc<AtomicBool>, pub reset:Arc<AtomicBool>, pub ssrc:Arc<AtomicU32> }
impl Default for Desktop {
    fn default()->Self {Self{units:Latest::new(8),pli:Arc::new(AtomicBool::new(false)),reset:Arc::new(AtomicBool::new(false)),ssrc:Arc::new(AtomicU32::new(0))}}
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
fn present(sink:&mut dyn FrameSink,frame:&Frame)->Result<(),String> {
    sink.present(frame)?;
    // Local acceptance aid: UU_VIEWER_DUMP_FRAME=path writes the 5th frame as PPM (owner's own screen).
    if STATS[1].fetch_add(1,Ordering::Relaxed)==std::env::var("UU_VIEWER_DUMP_FRAME_INDEX").ok().and_then(|v|v.parse().ok()).unwrap_or(4u64) { if let Some(path)=std::env::var_os("UU_VIEWER_DUMP_FRAME") {
        let mut ppm=format!("P6\n{} {}\n255\n",frame.width,frame.height).into_bytes();ppm.extend_from_slice(&frame.rgb);let _=std::fs::write(path,ppm);
    }}
    Ok(())
}
fn decode(decoder:&mut dyn Decoder,au:&[u8],codec:super::media::Codec,pli:&AtomicBool)->Vec<Frame> {
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
        Ok(frames)=>frames,
        Err(_)=>{STATS[2].fetch_add(1,Ordering::Relaxed);decoder.reset();pli.store(true,Ordering::Release);Vec::new()},
    }
}
/// Feed reference-dependent units in FIFO order, but present only the newest decoded picture.
pub fn pump(desktop:&Desktop,decoder:&mut dyn Decoder,sink:&mut dyn FrameSink,gate:&mut KeyGate)->Result<Option<super::media::Codec>,String> {
    let started=std::time::Instant::now();
    pump_with_elapsed(desktop,decoder,sink,gate,||started.elapsed())
}
fn pump_with_elapsed(desktop:&Desktop,decoder:&mut dyn Decoder,sink:&mut dyn FrameSink,gate:&mut KeyGate,mut elapsed:impl FnMut()->std::time::Duration)->Result<Option<super::media::Codec>,String> {
    let mut newest=None;let mut codec=None;
    for _ in 0..8 {
        let Some(au)=desktop.units.pop() else {break};
        if desktop.reset.swap(false,Ordering::AcqRel) {decoder.reset();gate.arm();}
        if !gate.admit(au.codec,&au.data) {desktop.pli.store(true,Ordering::Release);}
        else {
            decoder.set_codec(au.codec)?;codec=Some(au.codec);
            if let Some(frame)=decode(decoder,&au.data,au.codec,&desktop.pli).pop() {newest=Some(frame);}
        }
        // A soft budget between calls: an in-progress decoder call cannot be interrupted.
        if elapsed()>=std::time::Duration::from_millis(4) {break}
    }
    // Asynchronous decoders may finish a picture even when no new input has arrived.
    if let Some(frame)=decoder.poll().pop() {newest=Some(frame);}
    if let Some(frame)=newest {present(sink,&frame)?;}
    Ok(codec)
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
    #[test] fn decode_error_requests_pli_and_recovers(){
        let desktop=Desktop::default();let mut d=Mock(true);let mut s=Sink(0);let mut gate=KeyGate::new();
        enqueue(&desktop,unit(0,false));
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();assert!(desktop.pli.load(Ordering::Acquire));assert_eq!(s.0,0);
        enqueue(&desktop,unit(1,false));
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();assert_eq!(s.0,1);
    }

    use crate::viewer::media::{AccessUnit,Codec};
    use std::time::Duration;
    fn unit(id:u32,key:bool)->AccessUnit {
        let mut data=vec![0,0,0,1,if key {0x65}else{0x41},0b1001_1000];
        data.extend_from_slice(&id.to_le_bytes());AccessUnit{codec:Codec::H264,data}
    }
    fn picture(id:u32)->Frame {Frame{width:1,height:1,rgb:id.to_le_bytes()[..3].to_vec(),content:None}}
    #[derive(Default)]
    struct BatchDecoder {seen:Vec<u32>,resets:usize,pending:Vec<Frame>,extra_picture:bool}
    impl Decoder for BatchDecoder {
        fn set_codec(&mut self,_:Codec)->Result<(),String>{Ok(())}
        fn decode(&mut self,au:&[u8])->Result<Vec<Frame>,String>{
            let id=u32::from_le_bytes(au[6..].try_into().unwrap());self.seen.push(id);
            let mut frames=vec![picture(id)];if self.extra_picture {frames.push(picture(id+100));}Ok(frames)
        }
        fn reset(&mut self){self.resets+=1;self.pending.clear();}
        fn poll(&mut self)->Vec<Frame>{std::mem::take(&mut self.pending)}
    }
    #[derive(Default)]
    struct Pictures(Vec<u32>);
    impl FrameSink for Pictures {
        fn present(&mut self,frame:&Frame)->Result<(),String>{assert_eq!(frame.rgb.len(),3);self.0.push(u32::from_le_bytes([frame.rgb[0],frame.rgb[1],frame.rgb[2],0]));Ok(())}
    }
    // Match the RTP reader's overflow signal without a network or wall-clock sleeps.
    fn enqueue(desktop:&Desktop,au:AccessUnit)->bool {
        let dropped=desktop.units.push(au);
        if dropped {desktop.reset.store(true,Ordering::Release);desktop.pli.store(true,Ordering::Release);}
        dropped
    }
    #[test] fn one_unit_per_tick_overflows_even_with_a_larger_queue(){
        for capacity in [3,8] {
            let q=Latest::new(capacity);let mut arrived=0;let mut dropped=0;
            // 1,000 ticks at 125 Hz = 8 seconds; exactly 1,152 units arrive at 144 Hz.
            for tick in 1..=1000 {
                let due=tick*144/125;
                while arrived<due {dropped+=usize::from(q.push(arrived));arrived+=1;}
                assert!(q.pop().is_some());
            }
            assert_eq!(arrived,1152);assert_eq!(dropped,152-(capacity-1));
        }
    }
    #[test] fn batches_keep_up_with_144_units_per_second_in_fifo_order(){
        let desktop=Desktop::default();let mut d=BatchDecoder::default();let mut s=Pictures::default();let mut gate=KeyGate::new();let mut arrived=0;
        for tick in 1..=1000 {
            let due=tick*144/125;
            while arrived<due {assert!(!enqueue(&desktop,unit(arrived,false)));arrived+=1;}
            let before=s.0.len();
            assert_eq!(pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap(),Some(Codec::H264));
            assert_eq!(s.0.len(),before+1);assert_eq!(s.0.last(),Some(&(due-1)));
        }
        assert_eq!(arrived,1152);assert_eq!(d.seen,(0..1152).collect::<Vec<_>>());
        assert_eq!(d.resets,0);assert!(desktop.units.pop().is_none());assert!(!desktop.reset.load(Ordering::Acquire));
    }
    #[test] fn batch_presents_only_the_latest_picture_and_polls_when_idle(){
        let desktop=Desktop::default();let mut d=BatchDecoder{extra_picture:true,..Default::default()};let mut s=Pictures::default();let mut gate=KeyGate::new();
        for id in 0..3 {assert!(!enqueue(&desktop,unit(id,false)));}
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();
        assert_eq!(d.seen,vec![0,1,2]);assert_eq!(s.0,vec![102]);
        d.pending=vec![picture(103),picture(104)];
        assert_eq!(pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap(),None);
        assert_eq!(s.0,vec![102,104]);
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();assert_eq!(s.0,vec![102,104]);
        enqueue(&desktop,unit(105,false));d.pending=vec![picture(206)];
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();assert_eq!(s.0,vec![102,104,206]);
    }
    #[test] fn actual_queue_loss_still_resets_and_waits_for_a_key_frame(){
        let desktop=Desktop::default();let mut d=BatchDecoder::default();let mut s=Pictures::default();let mut gate=KeyGate::new();
        for id in 0..8 {assert!(!enqueue(&desktop,unit(id,false)));}
        assert!(enqueue(&desktop,unit(8,false)));
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();
        assert_eq!(d.resets,1);assert!(d.seen.is_empty());assert!(s.0.is_empty());assert!(desktop.pli.load(Ordering::Acquire));
        enqueue(&desktop,unit(9,false));enqueue(&desktop,unit(10,true));enqueue(&desktop,unit(11,false));
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();
        assert_eq!(d.resets,1);assert_eq!(d.seen,vec![10,11]);assert_eq!(s.0,vec![11]);
    }
    #[test] fn batch_count_limit_leaves_remaining_units_for_the_next_tick(){
        let desktop=Desktop{units:Latest::new(12),..Default::default()};let mut d=BatchDecoder::default();let mut s=Pictures::default();let mut gate=KeyGate::new();
        for id in 0..12 {assert!(!enqueue(&desktop,unit(id,false)));}
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();
        assert_eq!(d.seen,(0..8).collect::<Vec<_>>());assert_eq!(s.0,vec![7]);
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();
        assert_eq!(d.seen,(0..12).collect::<Vec<_>>());assert_eq!(s.0,vec![7,11]);
    }
    #[test] fn elapsed_budget_is_checked_between_calls_and_preserves_the_queue(){
        let desktop=Desktop::default();let mut d=BatchDecoder::default();let mut s=Pictures::default();let mut gate=KeyGate::new();
        for id in 0..4 {enqueue(&desktop,unit(id,false));}
        let mut times=[Duration::from_millis(3),Duration::from_millis(4)].into_iter();
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||times.next().unwrap()).unwrap();
        assert_eq!(d.seen,vec![0,1]);assert_eq!(s.0,vec![1]);
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::from_millis(20)).unwrap();
        assert_eq!(d.seen,vec![0,1,2]);assert_eq!(s.0,vec![1,2]);
        pump_with_elapsed(&desktop,&mut d,&mut s,&mut gate,||Duration::ZERO).unwrap();
        assert_eq!(d.seen,vec![0,1,2,3]);assert_eq!(s.0,vec![1,2,3]);
    }
}
