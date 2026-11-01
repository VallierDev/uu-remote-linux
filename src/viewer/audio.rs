use super::media::Latest;

const PACKETS:usize=12;
const MAX_PACKET:usize=8192;
#[derive(Clone)]
pub struct Packet { seq:u16, data:Vec<u8> }
#[derive(Clone)]
pub struct Audio { packets:Latest<Packet>, enabled:bool }
impl Default for Audio {
    fn default()->Self {Self{packets:Latest::new(PACKETS),enabled:std::env::var("UU_VIEWER_AUDIO").map_or(true,|v|v!="0")}}
}
impl Audio {
    #[cfg(test)] pub fn pop(&self)->Option<Vec<u8>> {self.packets.pop().map(|p|p.data)}
    pub fn push(&self,seq:u16,data:&[u8]) {
        if self.enabled && data.len()<=MAX_PACKET && samples(data).is_some() {self.packets.push(Packet{seq,data:data.to_vec()});}
    }
}
fn samples(data:&[u8])->Option<u64> {
    let toc=*data.first()?;
    let frame=if toc&0x80!=0 {120u64<<(toc>>3&3)} else if toc&0x60==0x60 {if toc&8!=0 {960}else{480}} else {match toc>>3&3 {3=>2880,n=>480u64<<n}};
    let count=match toc&3 {0=>1,1|2=>2,_=>u64::from(*data.get(1)?&63)};
    let total=frame*count;(count>0 && total<=5760).then_some(total)
}

// One complete packet per page; zero pre-skip because RTP has no encoder pre-skip header.
#[derive(Default)]
struct Ogg { sequence:u32, granule:u64, last:Option<u16> }
impl Ogg {
    fn page(&mut self,payload:&[u8],flags:u8)->Vec<u8> {
        let mut page=b"OggS\0".to_vec();page.push(flags);page.extend(self.granule.to_le_bytes());
        page.extend(1u32.to_le_bytes());page.extend(self.sequence.to_le_bytes());self.sequence=self.sequence.wrapping_add(1);
        page.extend([0;4]);page.push((payload.len()/255+1) as u8);
        page.extend(std::iter::repeat(255).take(payload.len()/255));page.push((payload.len()%255) as u8);page.extend(payload);
        let mut crc=0u32;
        for byte in &page {crc^=u32::from(*byte)<<24;for _ in 0..8 {crc=if crc&0x80000000!=0 {(crc<<1)^0x04c11db7}else{crc<<1};}}
        page[22..26].copy_from_slice(&crc.to_le_bytes());page
    }
    fn headers(&mut self)->Vec<u8> {
        let mut head=b"OpusHead\x01\x02\0\0".to_vec();head.extend(48000u32.to_le_bytes());head.extend([0,0,0]);
        let mut out=self.page(&head,2);let mut tags=b"OpusTags".to_vec();tags.extend([0;8]);out.extend(self.page(&tags,0));out
    }
    fn packet(&mut self,packet:Packet)->Option<Vec<u8>> {
        if self.last.is_some_and(|last|{let delta=packet.seq.wrapping_sub(last);delta==0 || delta>=0x8000}) {return None}
        self.granule+=samples(&packet.data)?;self.last=Some(packet.seq);Some(self.page(&packet.data,0))
    }
}

#[cfg(feature="viewer-sdl")]
mod playback {
    use super::*;
    use tokio::{io::{AsyncReadExt,AsyncWriteExt},process::{Child,Command},task::JoinHandle};
    use std::{process::Stdio,time::Duration};
    const PCM_SAMPLES:usize=1920;
    const PCM_BYTES:usize=PCM_SAMPLES*2;
    const SDL_BYTES:u32=48000*2*2/10;
    struct Decoder { child:Option<Child>, tasks:Vec<JoinHandle<()>>, pcm:Latest<Vec<i16>> }
    impl Decoder {
        fn new(audio:Audio)->Result<Self,String> {
            let mut child=Command::new(std::env::var_os("UU_VIEWER_FFMPEG").unwrap_or_else(||"ffmpeg".into()))
                .args(["-hide_banner","-loglevel","error","-nostdin","-threads","1","-probesize","32","-analyzeduration","0",
                    "-f","ogg","-i","pipe:0","-vn","-ac","2","-ar","48000","-c:a","pcm_s16le","-f","s16le","-flush_packets","1","pipe:1"])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|_|"Audio decoder start failed")?;
            let mut input=child.stdin.take().ok_or("Audio decoder stdin missing")?;
            let mut output=child.stdout.take().ok_or("Audio decoder stdout missing")?;
            let pcm=Latest::new(PACKETS);let decoded=pcm.clone();
            let writer=tokio::spawn(async move {
                let mut ogg=Ogg::default();
                let result:std::io::Result<()>=async {
                    input.write_all(&ogg.headers()).await?;
                    let mut tick=tokio::time::interval(Duration::from_millis(4));
                    loop {
                        tick.tick().await;
                        while let Some(packet)=audio.packets.pop() {if let Some(page)=ogg.packet(packet) {
                            tokio::time::timeout(Duration::from_secs(1),input.write_all(&page)).await??;
                        }}
                    }
                }.await;
                if result.is_err() {log::warn!("Viewer audio decoder input stopped");}
            });
            let reader=tokio::spawn(async move {
                let mut bytes=[0u8;PCM_BYTES];
                while output.read_exact(&mut bytes).await.is_ok() {
                    decoded.push(bytes.chunks_exact(2).map(|b|i16::from_le_bytes([b[0],b[1]])).collect());
                }
            });
            Ok(Self{child:Some(child),tasks:vec![writer,reader],pcm})
        }
        fn alive(&mut self)->bool {self.child.as_mut().is_some_and(|child|matches!(child.try_wait(),Ok(None))) && self.tasks.iter().all(|t|!t.is_finished())}
    }
    impl Drop for Decoder {
        fn drop(&mut self) {
            for task in &self.tasks {task.abort();}
            if let Some(mut child)=self.child.take() {let _=child.start_kill();tokio::spawn(async move {let _=child.wait().await;});}
        }
    }
    pub struct Playback { queue:sdl2::audio::AudioQueue<i16>, decoder:Decoder, pub volume:u8 }
    impl Playback {
        pub fn new(sdl:&sdl2::Sdl,audio:Audio)->Option<Self> {
            if !audio.enabled {return None}
            let result=(|| {
                let subsystem=sdl.audio()?;
                let queue=subsystem.open_queue::<i16,_>(None,&sdl2::audio::AudioSpecDesired{freq:Some(48000),channels:Some(2),samples:Some(960)})?;
                let decoder=Decoder::new(audio)?;queue.resume();Ok::<_,String>(Self{queue,decoder,volume:100})
            })();
            match result {Ok(p)=>Some(p),Err(_)=>{log::warn!("Viewer audio playback unavailable");None}}
        }
        pub fn pump(&mut self)->Result<(),String> {
            if !self.decoder.alive() {return Err("Audio decoder stopped".into())}
            while let Some(mut samples)=self.decoder.pcm.pop() {
                if self.volume<100 {let v=i32::from(self.volume);for s in samples.iter_mut(){*s=(i32::from(*s)*v/100) as i16;}}
                if self.queue.size()+samples.len() as u32*2>SDL_BYTES {self.queue.clear();}
                self.queue.queue_audio(&samples).map_err(|_|"Audio queue failed")?;
            }Ok(())
        }
    }
    #[cfg(test)] mod tests {
        use super::*;
        #[tokio::test] async fn sdl_dummy_queue_is_bounded_and_audio_can_be_disabled() {
            if std::env::var_os("UU_VIEWER_AUDIO_TEST_CHILD").is_none() {
                let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","viewer::audio::playback::tests::sdl_dummy_queue_is_bounded_and_audio_can_be_disabled","--test-threads=1"])
                    .env("SDL_AUDIODRIVER","dummy").env("UU_VIEWER_AUDIO_TEST_CHILD","1").output().unwrap();
                assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));return
            }
            let sdl=sdl2::init().unwrap();
            assert!(Playback::new(&sdl,Audio{packets:Latest::new(PACKETS),enabled:false}).is_none());
            let mut playback=Playback::new(&sdl,Audio{packets:Latest::new(PACKETS),enabled:true}).unwrap();playback.queue.pause();
            for value in 0..100 {playback.decoder.pcm.push(vec![value;PCM_SAMPLES]);}
            playback.pump().unwrap();assert!(playback.queue.size()>0 && playback.queue.size()<=SDL_BYTES);
            assert!(playback.decoder.pcm.pop().is_none());
        }
        #[tokio::test] async fn ffmpeg_streams_pcm_before_eof_and_is_reaped() {
            let audio=Audio{packets:Latest::new(PACKETS),enabled:true};let mut decoder=Decoder::new(audio.clone()).unwrap();
            let id=decoder.child.as_ref().unwrap().id().unwrap();
            tokio::time::timeout(Duration::from_secs(5),async {
                for seq in 0..250 {audio.push(seq,&[0xf8,0xff,0xfe]);tokio::time::sleep(Duration::from_millis(8)).await;
                    if let Some(pcm)=decoder.pcm.pop() {assert_eq!(pcm.len(),PCM_SAMPLES);assert!(decoder.alive());return}
                }panic!("No PCM before EOF");
            }).await.unwrap();
            drop(decoder);
            tokio::time::timeout(Duration::from_secs(2),async {while unsafe{libc::kill(id as i32,0)}==0 {tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
        }
    }
}
#[cfg(feature="viewer-sdl")] pub use playback::Playback;

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn duration_bounds_and_latest_packets() {
        assert_eq!(samples(&[0xf8,0xff,0xfe]),Some(960));assert_eq!(samples(&[0x98]),Some(960));
        assert_eq!(samples(&[0x18]),Some(2880));assert_eq!(samples(&[0x83,48]),Some(5760));
        assert_eq!(samples(&[0x83,49]),None);assert_eq!(samples(&[3,0]),None);assert_eq!(samples(&[]),None);
        let audio=Audio{packets:Latest::new(PACKETS),enabled:true};
        for seq in 0..1000 {audio.push(seq,&[0xf8,0xff,0xfe]);}
        audio.push(1000,&vec![0;MAX_PACKET+1]);audio.push(1000,&[]);
        for seq in 988..1000 {assert_eq!(audio.packets.pop().unwrap().seq,seq);}assert!(audio.packets.pop().is_none());
        let disabled=Audio{enabled:false,..audio};disabled.push(0,&[0xf8,0xff,0xfe]);assert!(disabled.packets.pop().is_none());
    }
    #[test] fn ogg_headers_crc_lacing_and_sequence_wrap() {
        let mut ogg=Ogg::default();let headers=ogg.headers();assert_eq!(&headers[..4],b"OggS");assert_eq!(&headers[28..36],b"OpusHead");
        let page=ogg.page(&vec![0;255],0);assert_eq!(&page[27..29],&[255,0]);
        let mut stream=headers;stream.extend(&page);
        let (mut reader,head)=webrtc::media::io::ogg_reader::OggReader::new(std::io::Cursor::new(stream.clone()),true).unwrap();
        assert_eq!(head.channels,2);assert_eq!(head.sample_rate,48000);
        assert!(reader.parse_next_page().unwrap().0.starts_with(b"OpusTags"));
        assert_eq!(reader.parse_next_page().unwrap().0.len(),255);
        stream[28]^=1;assert!(webrtc::media::io::ogg_reader::OggReader::new(std::io::Cursor::new(stream),true).is_err());
        let packet=|seq|Packet{seq,data:vec![0xf8,0xff,0xfe]};
        assert!(ogg.packet(packet(65535)).is_some());assert!(ogg.packet(packet(0)).is_some());
        assert!(ogg.packet(packet(0)).is_none());assert!(ogg.packet(packet(65534)).is_none());assert_eq!(ogg.granule,1920);
    }
}
