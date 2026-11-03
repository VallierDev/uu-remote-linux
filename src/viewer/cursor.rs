//! Session-scoped remote pointer shapes. Wire evidence is documented in src/cursor.rs.
use crate::protocol::{decode_wire,WireValue};
use std::sync::{Arc,Mutex};
const MAX_SIDE:u32=512;
const MAX_PNG:usize=1024*1024;
#[derive(Clone,Debug,PartialEq)]
pub struct Shape {pub width:u32,pub height:u32,pub hot_x:u32,pub hot_y:u32,pub scale_x:f64,pub scale_y:f64,pub png:Vec<u8>}
fn blob(data:&[u8],field:u32)->Option<&[u8]>{decode_wire(data).ok()?.into_iter().find_map(|f|match(f.number,f.value){(n,WireValue::Bytes(v))if n==field=>Some(v),_=>None})}
fn parse(data:&[u8])->Result<Option<Shape>,()> {
    if data.len()>MAX_PNG+512{return Err(())}
    let Some(change)=blob(data,15) else{return Ok(None)};
    let Some(shape)=blob(change,2) else{return Ok(None)};
    let fields=decode_wire(shape).map_err(|_|())?;
    let uint=|n|fields.iter().find_map(|f|match(f.number,&f.value){(i,WireValue::Varint(v))if i==n=>Some(*v),_=>None}).unwrap_or(0);
    let scale=|n|fields.iter().find_map(|f|match(f.number,&f.value){(i,WireValue::Fixed64(v))if i==n=>Some(f64::from_le_bytes(*v)),_=>None}).unwrap_or(1.);
    let png=blob(shape,5).unwrap_or_default();
    // Type-only/unsupported shapes restore the local arrow instead of leaving a stale resize cursor.
    if png.is_empty(){return Ok(Some(Shape{width:0,height:0,hot_x:0,hot_y:0,scale_x:1.,scale_y:1.,png:Vec::new()}))}
    let(width,height)=(u32::try_from(uint(3)).map_err(|_|())?,u32::try_from(uint(4)).map_err(|_|())?);
    let(hot_x,hot_y)=(u32::try_from(uint(1)).map_err(|_|())?,u32::try_from(uint(2)).map_err(|_|())?);
    let(scale_x,scale_y)=(scale(7),scale(8));
    if width==0||height==0||width>MAX_SIDE||height>MAX_SIDE||hot_x>=width||hot_y>=height
        || ![scale_x,scale_y].iter().all(|v|v.is_finite()&&*v>0.&&*v<=16.) {return Err(())}
    if png.len()<33||png.len()>MAX_PNG||!png.starts_with(b"\x89PNG\r\n\x1a\n")||&png[12..16]!=b"IHDR"{return Err(())}
    let pw=u32::from_be_bytes(png[16..20].try_into().unwrap());let ph=u32::from_be_bytes(png[20..24].try_into().unwrap());
    if pw==0||ph==0||pw>MAX_SIDE||ph>MAX_SIDE{return Err(())}
    Ok(Some(Shape{width,height,hot_x,hot_y,scale_x,scale_y,png:png.to_vec()}))
}
#[derive(Default)] struct Latest {last:Option<Arc<Shape>>,pending:Option<Arc<Shape>>,count:u64}
#[derive(Clone,Default)] pub struct State(Arc<Mutex<Latest>>);
impl State {
    pub fn receive(&self,data:&[u8]) {
        let Ok(Some(shape))=parse(data) else{return};let mut state=self.0.lock().unwrap();
        if state.last.as_deref()==Some(&shape){return}
        state.count+=1;
        if state.count<=3 || state.count%100==0 {log::info!("Viewer remote cursor: count={} size={}x{} png_bytes={}",state.count,shape.width,shape.height,shape.png.len());}
        let shape=Arc::new(shape);state.last=Some(shape.clone());state.pending=Some(shape);
    }
    pub fn take(&self)->Option<Arc<Shape>>{self.0.lock().unwrap().pending.take()}
}
#[cfg(feature="viewer-sdl")]
mod display {
    use super::*;
    use sdl2::{mouse::{Cursor,SystemCursor},surface::Surface,pixels::PixelFormatEnum};
    struct Image {shape:Arc<Shape>,width:u32,height:u32,rgba:Vec<u8>}
    fn decode(shape:Arc<Shape>)->Result<Image,()> {
        let mut decoder=png::Decoder::new_with_limits(std::io::Cursor::new(&shape.png),png::Limits{bytes:4*MAX_PNG});
        decoder.set_ignore_text_chunk(true);decoder.set_ignore_iccp_chunk(true);
        decoder.set_transformations(png::Transformations::EXPAND|png::Transformations::STRIP_16);
        let mut reader=decoder.read_info().map_err(|_|())?;
        let size=reader.output_buffer_size().filter(|v|*v<=MAX_SIDE as usize*MAX_SIDE as usize*4).ok_or(())?;
        let mut data=vec![0;size];let info=reader.next_frame(&mut data).map_err(|_|())?;
        if info.width==0||info.height==0||info.width>MAX_SIDE||info.height>MAX_SIDE{return Err(())}
        data.truncate(info.buffer_size());let mut rgba=Vec::with_capacity(info.width as usize*info.height as usize*4);
        match info.color_type {
            png::ColorType::Rgba=>rgba=data,
            png::ColorType::Rgb=>for p in data.chunks_exact(3){rgba.extend_from_slice(&[p[0],p[1],p[2],255]);},
            png::ColorType::Grayscale=>for p in data {rgba.extend_from_slice(&[p,p,p,255]);},
            png::ColorType::GrayscaleAlpha=>for p in data.chunks_exact(2){rgba.extend_from_slice(&[p[0],p[0],p[0],p[1]]);},
            _=>return Err(()),
        }
        if rgba.len()!=info.width as usize*info.height as usize*4{return Err(())}
        Ok(Image{shape,width:info.width,height:info.height,rgba})
    }
    fn geometry(s:&Shape)->(u32,u32,i32,i32) {
        // Cursor width/hotspot are the host's cursor units. Video coordinate
        // scales belong to pointer position mapping, not the local OS cursor.
        // Keep the cursor readable when a large remote desktop is fitted down.
        let w=s.width.clamp(1,256);let h=s.height.clamp(1,256);
        let x=(s.hot_x as f64*w as f64/s.width as f64).round().clamp(0.,(w-1) as f64) as i32;
        let y=(s.hot_y as f64*h as f64/s.height as f64).round().clamp(0.,(h-1) as f64) as i32;
        (w,h,x,y)
    }
    pub struct Display {arrow:Option<Cursor>,remote:Option<Cursor>,image:Option<Image>,geometry:Option<(u32,u32,i32,i32)>,active:bool,applied:u64}
    impl Display {
        pub fn new()->Self {Self{arrow:Cursor::from_system(SystemCursor::Arrow).ok(),remote:None,image:None,geometry:None,active:false,applied:0}}
        pub fn reset(&mut self){if let Some(cursor)=&self.arrow{cursor.set();}self.active=false;}
        pub fn update(&mut self,shape:Option<Arc<Shape>>,active:bool){
            if let Some(shape)=shape {
                self.reset();self.remote=None;self.geometry=None;
                self.image=if shape.png.is_empty(){None}else{match decode(shape){Ok(image)=>Some(image),Err(())=>{log::warn!("Viewer remote cursor PNG rejected");None}}};
            }
            if let Some(image)=self.image.as_mut(){
                let size=geometry(&image.shape);
                if self.geometry!=Some(size) {
                    let result=(||->Result<Cursor,String>{
                        let mut src=Surface::from_data(&mut image.rgba,image.width,image.height,image.width*4,PixelFormatEnum::RGBA32)?;
                        src.set_blend_mode(sdl2::render::BlendMode::None)?;
                        let mut target=Surface::new(size.0,size.1,PixelFormatEnum::RGBA32)?;
                        src.blit_scaled(None,&mut target,None)?;
                        Cursor::from_surface(&target,size.2,size.3)
                    })();
                    self.reset();self.remote=match result {Ok(cursor)=>{self.applied+=1;if self.applied<=3||self.applied%100==0{log::info!("Viewer cursor applied: count={} size={}x{}",self.applied,size.0,size.1);}Some(cursor)},Err(_)=>{log::warn!("Viewer remote cursor unavailable on this display");None}};self.geometry=Some(size);
                }
            }
            let active=active&&self.remote.is_some();
            if active!=self.active {
                if active {self.remote.as_ref().unwrap().set();self.active=true;}else{self.reset();}
            }
        }
    }
    impl Drop for Display {fn drop(&mut self){self.reset();}}
    #[cfg(test)] mod tests {
        use super::*;
        #[test] fn png_pixels_and_retina_hotspot_use_stable_cursor_units(){
            let mut png=Vec::new();{
                let mut encoder=png::Encoder::new(&mut png,4,4);encoder.set_color(png::ColorType::Rgba);encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header().unwrap().write_image_data(&[128;64]).unwrap();
            }
            let shape=Arc::new(Shape{width:2,height:2,hot_x:1,hot_y:1,scale_x:2.,scale_y:2.,png});
            let image=decode(shape.clone()).unwrap();assert_eq!(image.rgba,vec![128;64]);
            assert_eq!(geometry(&shape),(2,2,1,1));
            let mut changed=(*shape).clone();changed.scale_x=0.25;changed.scale_y=0.25;assert_eq!(geometry(&changed),(2,2,1,1));
            let mut bad=(*shape).clone();bad.png[40]^=255;assert!(decode(Arc::new(bad)).is_err());
        }
    }
}
#[cfg(feature="viewer-sdl")] pub use display::Display;
#[cfg(test)] mod tests {
    use super::*;
    fn packet(w:u64,hot:u64,scale:f64,png:&[u8])->Vec<u8>{
        let mut shape=Vec::new();super::super::wire::int(3,w,&mut shape);super::super::wire::int(4,w,&mut shape);super::super::wire::int(1,hot,&mut shape);
        super::super::wire::bytes(5,png,&mut shape);shape.push(7<<3|1);shape.extend_from_slice(&scale.to_le_bytes());
        let mut change=Vec::new();super::super::wire::bytes(2,&shape,&mut change);super::super::wire::envelope(1,15,&change)
    }
    #[test] fn shape_bounds_and_latest_only_updates(){
        let png=include_bytes!("../../tests/fixtures/clipboard-test.png");
        let state=State::default();let valid=packet(16,2,1.,png);state.receive(&valid);state.receive(&valid);
        assert_eq!(state.take().unwrap().width,16);assert!(state.take().is_none());
        for bad in [packet(0,0,1.,png),packet(513,0,1.,png),packet(16,16,1.,png),packet(16,0,f64::NAN,png),packet(16,0,0.,png)] {assert!(parse(&bad).is_err());state.receive(&bad);assert!(state.take().is_none());}
        state.receive(&packet(20,2,1.,png));state.receive(&packet(24,2,1.,png));assert_eq!(state.take().unwrap().width,24);
        state.receive(&packet(0,0,1.,&[]));assert!(state.take().unwrap().png.is_empty());
        assert!(parse(&vec![0;MAX_PNG+513]).is_err());
    }
}
