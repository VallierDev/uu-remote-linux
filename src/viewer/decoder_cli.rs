//! H.264 decoding through an external `ffmpeg` process, like the host's encoder pipeline.
//! Needs only the ffmpeg CLI (any recent version), not FFmpeg development libraries.
//! Output is scaled into a fixed letterboxed size so every frame has a known byte length.
use super::desktop::{Decoder,Frame};
use std::io::{Read,Write};
use std::process::{Child,ChildStdin,Command,Stdio};
use std::sync::mpsc::{Receiver,TryRecvError};

pub struct Cli { child:Child, input:ChildStdin, frames:Receiver<Vec<u8>>, width:u32, height:u32 }

impl Cli {
    /// `UU_VIEWER_SIZE=WxH` picks the decoded size (default 1920x1080); `UU_VIEWER_FFMPEG` the binary.
    pub fn new()->Result<Self,String> {
        let (width,height)=std::env::var("UU_VIEWER_SIZE").ok()
            .and_then(|v|{let (w,h)=v.split_once('x')?;Some((w.parse().ok()?,h.parse().ok()?))})
            .filter(|&(w,h):&(u32,u32)|(16..=8192).contains(&w)&&(16..=8192).contains(&h)&&w%2==0&&h%2==0)
            .unwrap_or((1920,1080));
        let ffmpeg=std::env::var("UU_VIEWER_FFMPEG").unwrap_or_else(|_|"ffmpeg".into());
        let filter=format!("scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2");
        let mut child=Command::new(ffmpeg)
            .args(["-hide_banner","-loglevel","error","-nostdin","-threads","1","-probesize","32","-analyzeduration","0",
                "-fflags","nobuffer","-flags","low_delay","-f","h264","-i","pipe:0","-an","-vf",&filter,
                "-pix_fmt","rgb24",
                // Raw H.264 carries no timestamps; constant-rate output dropped almost every frame of
                // the official Mac host's stream (270 → 2). Pass frames through unchanged.
                "-fps_mode","passthrough","-f","rawvideo","pipe:1"])
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
        Ok(Self{child,input,frames,width,height})
    }
}

impl Decoder for Cli {
    fn reset(&mut self) {}
    fn decode(&mut self,au:&[u8])->Result<Vec<Frame>,String> {
        self.input.write_all(au).and_then(|_|self.input.flush()).map_err(|_|"ffmpeg decoder exited")?;
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
