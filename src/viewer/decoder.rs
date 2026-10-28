//! Uses the optional FFmpeg 6 libraries of the `viewer-ffmpeg` feature.
use super::desktop::{Decoder,Frame};
use ffmpeg_next as av;
use super::media::Codec;
pub struct Ffmpeg { decoder:av::decoder::Video, codec:Codec }
impl Ffmpeg {
    pub fn new()->Result<Self,String> {
        Self::with_codec(Codec::H264)
    }
    fn with_codec(kind:Codec)->Result<Self,String>{
        av::init().map_err(|_|"FFmpeg initialization failed")?;
        // Library diagnostics can contain bitstream data; suppress them globally in this viewer process.
        av::log::set_level(av::log::Level::Quiet);
        let codec=av::decoder::find(match kind {Codec::H264=>av::codec::Id::H264,Codec::H265=>av::codec::Id::HEVC}).ok_or("Video decoder unavailable")?;
        let decoder=av::codec::context::Context::new().decoder().open_as(codec).and_then(|d|d.video()).map_err(|_|"Video decoder initialization failed")?;
        Ok(Self{decoder,codec:kind})
    }
}
impl Decoder for Ffmpeg {
    fn set_codec(&mut self,codec:Codec)->Result<(),String>{
        if self.codec!=codec {*self=Self::with_codec(codec)?;}Ok(())
    }
    fn reset(&mut self){self.decoder.flush();}
    fn decode(&mut self,au:&[u8])->Result<Vec<Frame>,String>{
        self.decoder.send_packet(&av::Packet::copy(au)).map_err(|_|"Video packet rejected")?;
        let mut frames=Vec::new();let mut decoded=av::frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(())=>{},
                Err(av::Error::Other{errno}) if errno==libc::EAGAIN=>break,
                Err(_)=>return Err("Video decode failed".into()),
            }
            if decoded.is_corrupt(){return Err("Corrupt video frame".into())}
            let (w,h)=(decoded.width(),decoded.height());
            if w==0 || h==0 || w>8192 || h>8192 {return Err("Decoded dimensions exceed limit".into())}
            let mut scaler=av::software::scaling::context::Context::get(decoded.format(),w,h,av::format::Pixel::RGB24,w,h,av::software::scaling::flag::Flags::BILINEAR).map_err(|_|"Frame conversion unavailable")?;
            let mut rgb=av::frame::Video::empty();scaler.run(&decoded,&mut rgb).map_err(|_|"Frame conversion failed")?;
            let mut data=Vec::with_capacity(w as usize*h as usize*3);
            for row in 0..h as usize {let start=row*rgb.stride(0);data.extend_from_slice(&rgb.data(0)[start..start+w as usize*3]);}
            frames.push(Frame{width:w,height:h,rgb:data,content:None});
        } Ok(frames)
    }
}
