//! H.264/H.265 decoding through an external `ffmpeg` process, like the host's encoder pipeline.
//! Needs only the ffmpeg CLI, not FFmpeg development libraries.
//! Output is scaled into a fixed letterboxed size so every frame has a known byte length.
use super::desktop::{Decoder,Frame};
use super::media::Codec;
use std::io::{Read,Write};
use std::process::{Child,ChildStdin,Command,Stdio};
use std::sync::mpsc::{Receiver,TryRecvError};

pub struct Cli { child:Child, input:Option<ChildStdin>, codec:Codec, frames:Receiver<Vec<u8>>, width:u32, height:u32 }

impl Cli {
    /// `UU_VIEWER_SIZE=WxH` picks the decoded size (default 1920x1080); `UU_VIEWER_FFMPEG` the binary.
    pub fn new()->Result<Self,String> {
        let (width,height)=std::env::var("UU_VIEWER_SIZE").ok()
            .and_then(|v|{let (w,h)=v.split_once('x')?;Some((w.parse().ok()?,h.parse().ok()?))})
            .filter(|&(w,h):&(u32,u32)|(16..=8192).contains(&w)&&(16..=8192).contains(&h)&&w%2==0&&h%2==0)
            .unwrap_or((1920,1080));
        Self::with_codec(Codec::H264,width,height)
    }
    fn with_codec(codec:Codec,width:u32,height:u32)->Result<Self,String> {
        let ffmpeg=std::env::var("UU_VIEWER_FFMPEG").unwrap_or_else(|_|"ffmpeg".into());
        let filter=format!("scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2");
        let mut child=Command::new(ffmpeg)
            .args(["-hide_banner","-loglevel","error","-nostdin","-threads","1","-probesize","32","-analyzeduration","0",
                "-fflags","nobuffer","-flags","low_delay","-f",codec.demuxer(),"-i","pipe:0","-an","-vf",&filter,
                "-pix_fmt","rgb24",
                // Raw H.264/H.265 carries no timestamps; constant-rate output dropped almost every frame of
                // the official Mac host's stream (270 → 2). Pass frames through unchanged.
                // -vsync 0 also supports Ubuntu 22.04's FFmpeg 4.4, which lacks -fps_mode.
                "-vsync","0","-f","rawvideo","pipe:1"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().map_err(|e|format!("Start ffmpeg decoder: {e}"))?;
        let input=child.stdin.take().ok_or("ffmpeg stdin missing")?;
        let mut output=child.stdout.take().ok_or("ffmpeg stdout missing")?;
        let size=width as usize*height as usize*3;
        // Bounded: a slow window drops decoded frames instead of growing memory.
        let (tx,frames)=std::sync::mpsc::sync_channel(2);
        std::thread::Builder::new().name("uu-viewer-decode".into()).spawn(move||{
            loop {
                let mut frame=vec![0;size];
                if output.read_exact(&mut frame).is_err() {break;}
                match tx.try_send(frame) { Ok(())|Err(std::sync::mpsc::TrySendError::Full(_))=>{}, Err(_)=>break }
            }
        }).map_err(|e|format!("Decoder reader: {e}"))?;
        Ok(Self{child,input:Some(input),codec,frames,width,height})
    }
}

impl Decoder for Cli {
    fn set_codec(&mut self,codec:Codec)->Result<(),String>{
        if self.codec!=codec {*self=Self::with_codec(codec,self.width,self.height)?;}Ok(())
    }
    fn reset(&mut self) {}
    fn decode(&mut self,au:&[u8])->Result<Vec<Frame>,String> {
        let input=self.input.as_mut().ok_or("ffmpeg input closed")?;
        input.write_all(au).and_then(|_|input.flush()).map_err(|_|"ffmpeg decoder exited")?;
        let mut out=Vec::new();
        loop {
            match self.frames.try_recv() {
                Ok(rgb)=>out.push(Frame{width:self.width,height:self.height,rgb}),
                Err(TryRecvError::Empty)=>break,
                Err(TryRecvError::Disconnected)=>return Err("ffmpeg decoder exited".into()),
            }
        }
        Ok(out)
    }
}

impl Drop for Cli { fn drop(&mut self) { let _=self.child.kill(); let _=self.child.wait(); } }

#[cfg(test)] mod tests {
    use super::*;
    // Six synthetic red IDRs per fixture; no captured desktop or external encoder dependency.
    #[test] fn ffmpeg_decodes_h264_and_hevc_420_and_444_to_rgb24(){
        match Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status(){
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{eprintln!("skip: ffmpeg is not on PATH");return},
            result=>assert!(result.expect("ffmpeg availability").success()),
        }
        let mut cli=Cli::with_codec(Codec::H265,32,32).unwrap();
        for (codec,data) in [
            (Codec::H264,&include_bytes!("../../tests/fixtures/viewer/red-420.h264")[..]),
            (Codec::H265,&include_bytes!("../../tests/fixtures/viewer/red-420.hevc")[..]),
            (Codec::H264,&include_bytes!("../../tests/fixtures/viewer/red-444.h264")[..]),
            (Codec::H265,&include_bytes!("../../tests/fixtures/viewer/red-444.hevc")[..]),
        ] {
            cli.set_codec(codec).unwrap();
            // Exercise the same streaming process and RGB conversion as SDL, then drain at EOF.
            let mut frames=cli.decode(data).unwrap();drop(cli.input.take());
            loop {match cli.frames.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(rgb)=>frames.push(Frame{width:32,height:32,rgb}),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)=>break,
                Err(e)=>panic!("{codec:?} decoder did not finish: {e}"),
            }}
            let status=cli.child.wait().unwrap();
            assert!(status.success(),"{codec:?}: ffmpeg exited with {status}");
            assert!(!frames.is_empty(),"{codec:?}: no RGB frames");
            for frame in frames {
                assert_eq!(frame.rgb.len(),32*32*3);
                assert!(frame.rgb.chunks_exact(3).all(|p|p[0]>220 && p[1]<30 && p[2]<30),"{codec:?}: RGB conversion");
            }
        }
    }
}
