//! H.264/H.265 decoding through an external `ffmpeg` process, like the host's encoder pipeline.
//! Needs only the ffmpeg CLI, not FFmpeg development libraries.
//! Output is scaled into a fixed letterboxed canvas so every frame has a known byte length; the
//! source size from ffmpeg's stream line tells the window which sub-rectangle is picture content.
use super::desktop::{Decoder,Frame};
use super::media::Codec;
use std::io::{BufRead,Read,Write};
use std::process::{Child,ChildStdin,Command,Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64,Ordering};
use std::sync::mpsc::{Receiver,TryRecvError};

pub struct Cli { child:Child, input:Option<ChildStdin>, codec:Codec, frames:Receiver<Vec<u8>>, width:u32, height:u32, source:Arc<AtomicU64> }

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
        let passthrough=passthrough_args(&ffmpeg);
        let mut child=Command::new(&ffmpeg)
            .args(["-hide_banner","-loglevel","info","-nostats","-nostdin","-threads","1","-probesize","32","-analyzeduration","0",
                "-fflags","nobuffer","-flags","low_delay","-f",codec.demuxer(),"-i","pipe:0","-an","-vf",&filter,
                "-pix_fmt","rgb24",
                // Raw H.264/H.265 carries no timestamps; constant-rate output dropped almost every frame of
                // the official Mac host's stream (270 → 2). Pass frames through unchanged.
                passthrough.0,passthrough.1,"-f","rawvideo","pipe:1"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().map_err(|e|format!("Start ffmpeg decoder: {e}"))?;
        let input=child.stdin.take().ok_or("ffmpeg stdin missing")?;
        let mut output=child.stdout.take().ok_or("ffmpeg stdout missing")?;
        // Source picture size from the one-time stream line; 0 until parsed. Only the numbers are kept.
        let source=Arc::new(AtomicU64::new(0));
        if let Some(stderr)=child.stderr.take() {
            let source=source.clone();
            let _=std::thread::Builder::new().name("uu-viewer-ffmpeg-info".into()).spawn(move||{
                for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                    if source.load(Ordering::Relaxed)!=0 {continue}
                    if let Some((w,h))=parse_video_size(&line) {source.store((u64::from(w)<<32)|u64::from(h),Ordering::Relaxed);}
                }
            });
        }
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
        Ok(Self{child,input:Some(input),codec,frames,width,height,source})
    }
}

/// `30000/1001`-style or display strings are irrelevant here: the first WxH on a `Video:` line is the
/// coded picture size, which is the aspect the scale+pad filter letterboxes into the canvas.
fn parse_video_size(line:&str)->Option<(u32,u32)> {
    let rest=line.split_once("Video:")?.1;
    rest.split(|c:char|c==','||c==' '||c=='('||c==')'||c=='['||c==']').find_map(|token|{
        let token=token.trim_matches(|c:char|!c.is_ascii_digit()&&c!='x');
        let (w,h)=token.split_once('x')?;
        if w.is_empty()||h.is_empty()||!w.bytes().all(|b|b.is_ascii_digit())||!h.bytes().all(|b|b.is_ascii_digit()) {return None}
        let (w,h)=(w.parse::<u32>().ok()?,h.parse::<u32>().ok()?);
        ((16..=16384).contains(&w)&&(16..=16384).contains(&h)).then_some((w,h))
    })
}

/// FFmpeg ≥ 5.1 has `-fps_mode`; FFmpeg 8+ removed `-vsync`, and Ubuntu 22.04's 4.4 lacks `-fps_mode`.
/// Probe once per binary with a one-frame null run instead of parsing version strings.
fn passthrough_args(ffmpeg:&str)->(&'static str,&'static str) {
    static CACHE:std::sync::Mutex<Vec<(String,bool)>>=std::sync::Mutex::new(Vec::new());
    let mut cache=CACHE.lock().unwrap_or_else(|e|e.into_inner());
    let fps_mode=match cache.iter().find(|(name,_)|name==ffmpeg) {
        Some(&(_,known))=>known,
        None=>{
            let ok=Command::new(ffmpeg).args(["-hide_banner","-loglevel","quiet","-nostdin","-f","lavfi","-i","color=s=16x16:d=0.04",
                "-fps_mode","passthrough","-f","null","-"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                .status().is_ok_and(|s|s.success());
            cache.push((ffmpeg.into(),ok));ok
        }
    };
    if fps_mode {("-fps_mode","passthrough")} else {("-vsync","0")}
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
                Ok(rgb)=>{
                    let source=self.source.load(Ordering::Relaxed);
                    let content=(source!=0).then(||((source>>32) as u32,source as u32));
                    out.push(Frame{width:self.width,height:self.height,rgb,content})
                },
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
    #[cfg(unix)] #[test] fn passthrough_follows_ffmpeg_option_support(){
        use std::os::unix::fs::PermissionsExt;
        let dir=std::env::temp_dir().join(format!("uu-ffprobe-{}",std::process::id()));std::fs::create_dir_all(&dir).unwrap();
        for (name,script,expected) in [("new","exit 0",("-fps_mode","passthrough")),("old","case \"$*\" in *-fps_mode*) exit 1;; esac",("-vsync","0"))]{
            let path=dir.join(name);std::fs::write(&path,format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(super::passthrough_args(path.to_str().unwrap()),expected);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
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
                Ok(rgb)=>frames.push(Frame{width:32,height:32,rgb,content:None}),
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
    #[test] fn source_size_is_read_from_the_ffmpeg_stream_line(){
        assert_eq!(super::parse_video_size("  Stream #0:0: Video: h264 (High), yuv420p(progressive), 3024x1964, 25 fps"),Some((3024,1964)));
        assert_eq!(super::parse_video_size("Stream #0:0: Video: hevc, yuv420p, 1920x1080 [SAR 1:1 DAR 16:9]"),Some((1920,1080)));
        assert_eq!(super::parse_video_size("Stream #0:0: Audio: opus, 48000 Hz, stereo"),None);
        assert_eq!(super::parse_video_size("frame= 1 fps=0.0 q=0.0"),None);
        assert_eq!(super::parse_video_size("Stream #0:0: Video: h264, 1920xsynth"),None);
    }
}
