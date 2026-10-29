use super::{desktop::{Frame,FrameSink},input::{Input,Viewport},session::Session};
use sdl2::{event::{Event,WindowEvent},mouse::MouseButton,pixels::PixelFormatEnum,rect::Rect};
#[derive(Debug)] enum Command { Files, Type }
pub struct Window { canvas:sdl2::render::Canvas<sdl2::video::Window>, events:sdl2::EventPump, _sdl:sdl2::Sdl, frame:(u32,u32), focused:bool, files:bool, mac:bool,
    clipboard:sdl2::clipboard::ClipboardUtil, clipboard_dirty:bool, clipboard_tx:tokio::sync::mpsc::Sender<super::clipboard::ClipboardRequest>, clipboard_requests:tokio::sync::mpsc::Receiver<super::clipboard::ClipboardRequest>,
    pub last_frame:Option<Frame>, chrome:Chrome }
/// In-window chrome state (src/viewer/hud.rs): menu, network panel, loading screen.
struct Chrome { font:Option<ab_glyph::FontVec>, scale:f32, pub state:hud::State, menu:hud::Menu, net:Option<hud::NetStats>, tab_hover:bool, tab_down:bool,
    commands:Vec<hud::Command>, started:std::time::Instant, last_loading:std::time::Instant, title:String, stage:String, labels:std::collections::HashMap<(String,u32,u32),hud::Layer>,
    menu_cache:Option<(hud::State,hud::Menu,f32,hud::Layer)> }
use super::hud;
impl Window {
    pub fn new()->Result<Self,String>{
        // SDL's default nearest-neighbour scaling makes remote text uneven whenever the window size
        // differs from the stream; filter instead. Set before the renderer/textures exist.
        sdl2::hint::set_with_priority("SDL_RENDER_SCALE_QUALITY","linear",&sdl2::hint::Hint::Default);
        #[cfg(uu_gtk)] cairo_decorations();
        let sdl=sdl2::init()?;let video=sdl.video()?;
        let name=super::device_label(std::env::var("UU_VIEWER_DEVICE_NAME").ok().as_deref()).to_owned();
        let window=video.window(&format!("{name} · {}",crate::i18n::s("UU 远程 Linux 控制端","UU Remote Linux viewer")),1280,720).position_centered().resizable().build().map_err(|_|"Window creation failed")?;
        let mut canvas=window.into_canvas().build().map_err(|_|"Renderer creation failed")?;
        // Wayland maps a toplevel only after its first buffer; without this the window stays
        // invisible until the first decoded frame, which a stalled stream never delivers.
        canvas.set_draw_color(LOADING_BG);canvas.clear();canvas.present();
        let scale=std::env::var("UU_VIEWER_UI_SCALE").ok().and_then(|v|v.parse::<f32>().ok()).filter(|v|(0.5..=4.).contains(v)).unwrap_or(1.);
        let mut state=hud::State::default();hud::load_prefs(&mut state);
        let now=std::time::Instant::now();
        let chrome=Chrome{font:hud::load_font(),scale,state,menu:hud::Menu::default(),net:None,tab_hover:false,tab_down:false,commands:Vec::new(),started:now,last_loading:now-std::time::Duration::from_secs(1),
            title:crate::tr!("正在连接 {name}","Connecting to {name}"),stage:crate::i18n::s("正在连接远端设备…","Contacting the remote device…").into(),labels:Default::default(),menu_cache:None};
        let events=sdl.event_pump()?;let clipboard=video.clipboard();
        let(clipboard_tx,clipboard_requests)=tokio::sync::mpsc::channel(16);
        Ok(Self{canvas,events,_sdl:sdl,frame:(1920,1080),focused:true,files:false,mac:false,clipboard,clipboard_dirty:true,clipboard_tx,clipboard_requests,last_frame:None,chrome})
    }
    pub fn clipboard_bridge(&self)->super::clipboard::Bridge{super::clipboard::Bridge::sdl(self.clipboard_tx.clone())}
    pub fn set_mac(&mut self,mac:bool){self.mac=mac;}
    fn viewport(&self)->Viewport {
        let window=self.canvas.window().size();
        if !self.chrome.state.one_to_one {return Viewport::fit(window,self.frame)}
        let (w,h)=(self.frame.0 as f64,self.frame.1 as f64);
        Viewport{x:((window.0 as f64-w)/2.).round(),y:((window.1 as f64-h)/2.).round(),width:w,height:h}
    }
    fn poll_clipboard(&mut self){
        // Use the existing SDL window; wl-paste creates a transient toplevel on GNOME every poll.
        while let Ok(request)=self.clipboard_requests.try_recv(){
            use super::clipboard::ClipboardRequest;
            match request {
                ClipboardRequest::Read(reply)=>{
                    if self.clipboard_dirty {log::debug!("Viewer clipboard read: focused={} has_text={}",self.focused,self.clipboard.has_clipboard_text());}
                    let result=if self.focused && std::mem::take(&mut self.clipboard_dirty) && self.clipboard.has_clipboard_text(){read_clipboard()}else{Ok(None)};
                    let _=reply.send(result);
                },
                ClipboardRequest::Write(text,reply)=>{
                    let result=if self.focused {
                        self.clipboard_dirty=true;
                        self.clipboard.set_clipboard_text(&text).map(|()|true).map_err(|_|())
                    }else{Ok(false)};
                    let _=reply.send(result);
                },
            }
        }
    }
    pub fn poll(&mut self,input:&mut Input)->(bool,Vec<serde_json::Value>){
        let viewport=self.viewport();
        let mut redraw=false;
        let events=poll_events(&mut self.events);
        let mut forward=Vec::with_capacity(events.len());
        for event in events {
            if matches!(event,Event::ClipboardUpdate{..}|Event::Window{win_event:WindowEvent::FocusGained,..}){self.clipboard_dirty=true;}
            redraw|=needs_redraw(&event);
            let (consumed,changed)=self.chrome_event(&event);redraw|=changed;
            if !consumed {forward.push(event);}
        }
        let result=translate_actions(forward,input,viewport,&mut self.focused,&mut self.files);
        // A compositor resize (tiling, maximize) leaves the surface without a buffer of the new size
        // until the next frame; a static remote desktop sends none, so repaint the last frame now.
        if redraw {if let Some(frame)=self.last_frame.take(){
            log::debug!("Viewer window resized: {:?}",self.canvas.window().size());
            if let Err(e)=self.draw(&frame){log::warn!("Viewer redraw failed: {e}");}
            self.last_frame=Some(frame);
        }else{self.loading_frame(true);}}
        result
    }
    /// Menu, hotkeys and the control tab. Returns (consumed, needs redraw).
    fn chrome_event(&mut self,event:&Event)->(bool,bool){
        use sdl2::keyboard::{Mod,Scancode};
        let size=self.canvas.window().size();let scale=self.chrome.scale;
        let tab=hud::tab_rect(self.chrome.font.as_ref(),size,scale);
        let tab_visible=self.tab_visible();
        if let Event::KeyDown{scancode:Some(code),keymod,repeat,..}=event {
            let chord=keymod.intersects(Mod::LCTRLMOD|Mod::RCTRLMOD)&&keymod.intersects(Mod::LALTMOD|Mod::RALTMOD);
            let command=match code {Scancode::M=>Some(None),Scancode::G=>Some(Some(hud::Command::Stats(!self.chrome.state.stats))),Scancode::Return=>Some(Some(hud::Command::Fullscreen)),_=>None};
            if chord { if let Some(command)=command {
                if !repeat { match command {None=>self.chrome.menu.open^=true,Some(c)=>self.command(c)} }
                return (true,true)
            }}
            if self.chrome.menu.open { if *code==Scancode::Escape {self.chrome.menu.open=false;} return (true,true) }
        }
        if !self.chrome.menu.open {
            return match *event {
                Event::MouseMotion{x,y,..}=>{let h=tab_visible&&tab.contains(x as f32,y as f32);let changed=h!=self.chrome.tab_hover;self.chrome.tab_hover=h;(false,changed)},
                Event::MouseButtonDown{x,y,mouse_btn:MouseButton::Left,..} if tab_visible&&tab.contains(x as f32,y as f32)=>{self.chrome.menu.open=true;self.chrome.tab_down=true;(true,true)},
                Event::MouseButtonUp{mouse_btn:MouseButton::Left,..} if self.chrome.tab_down=>{self.chrome.tab_down=false;(true,false)},
                _=>(false,false),
            }
        }
        let lay=hud::layout(self.chrome.font.as_ref(),&self.chrome.state,&self.chrome.menu,scale);let origin=self.menu_origin(&lay);
        let hit=|x:i32,y:i32|->Option<hud::Hit>{
            let (lx,ly)=(x as f32-origin.0,y as f32-origin.1);
            if tab.contains(x as f32,y as f32) {return Some(hud::Hit::Tab)}
            if !lay.panels.iter().any(|p|p.contains(lx,ly)) {return None}
            Some(lay.items.iter().find(|(r,_)|r.contains(lx,ly)).map_or(hud::Hit::Panel,|(_,h)|*h))
        };
        match *event {
            Event::MouseMotion{x,y,..}=>{
                if self.chrome.menu.dragging { if let Some((a,len))=slider(&lay) {self.command(hud::Command::Volume(hud::slider_value(x as f32-origin.0,a,len)));} return (true,true) }
                let h=hit(x,y);
                if let Some(hud::Hit::Category(i))=h { if hud::category_command(i).is_none() {self.chrome.menu.category=i;} }
                let changed=h!=self.chrome.menu.hover;self.chrome.menu.hover=h;(true,changed)
            },
            Event::MouseButtonDown{x,y,mouse_btn:MouseButton::Left,..}=>{
                match hit(x,y) {
                    None|Some(hud::Hit::Tab)=>{self.chrome.menu.open=false;self.chrome.tab_down=true},
                    Some(hud::Hit::Category(i))=>match hud::category_command(i) {Some(c)=>{self.chrome.menu.open=false;self.command(c)},None=>self.chrome.menu.category=i},
                    Some(hud::Hit::Option(c,i))=>if let Some(command)=hud::option_command(&self.chrome.state,c,i) {
                        if command==hud::Command::Files {self.chrome.menu.open=false;}
                        self.command(command);
                    },
                    Some(hud::Hit::Slider(a,len))=>{self.chrome.menu.dragging=true;self.command(hud::Command::Volume(hud::slider_value(x as f32-origin.0,a,len)))},
                    Some(hud::Hit::Panel)=>{},
                }
                (true,true)
            },
            Event::MouseButtonUp{..}=>{self.chrome.tab_down=false;if std::mem::take(&mut self.chrome.menu.dragging){hud::save_prefs(&self.chrome.state);}(true,true)},
            Event::MouseButtonDown{..}|Event::MouseWheel{..}|Event::KeyUp{..}|Event::TextInput{..}=>(true,false),
            _=>(false,false),
        }
    }
    /// Local effects happen here; remote ones are drained by `run` through `take_commands`.
    fn command(&mut self,command:hud::Command){
        let st=&mut self.chrome.state;
        match command {
            hud::Command::Quality(q)=>{st.quality=q;hud::save_prefs(st)}, hud::Command::Fps(f)=>{st.fps=f;hud::save_prefs(st)},
            hud::Command::Chroma(c)=>{st.chroma=c;hud::save_prefs(st)},
            hud::Command::FollowRemote(b)=>st.follow_remote=b, hud::Command::Volume(v)=>st.volume=v,
            hud::Command::AutoLock(b)=>st.auto_lock=b, hud::Command::HostMute(b)=>st.host_mute=b, hud::Command::Watch(b)=>st.watch=b,
            hud::Command::OneToOne(b)=>{st.one_to_one=b;hud::save_prefs(st)},
            hud::Command::Stats(b)=>{st.stats=b;hud::save_prefs(st)},
            hud::Command::HideTab(b)=>{st.hide_tab=b;hud::save_prefs(st)},
            hud::Command::GrabKeys(b)=>{st.grab_keys=b;self.canvas.window_mut().set_keyboard_grab(b)},
            hud::Command::Fullscreen=>{
                st.fullscreen^=true;let mode=if st.fullscreen{sdl2::video::FullscreenType::Desktop}else{sdl2::video::FullscreenType::Off};
                let _=self.canvas.window_mut().set_fullscreen(mode);
            },
            hud::Command::Files=>self.files=true,
            hud::Command::Disconnect=>{},
        }
        self.chrome.commands.push(command);
    }
    pub fn take_commands(&mut self)->Vec<hud::Command>{std::mem::take(&mut self.chrome.commands)}
    /// The host starts every session at the official defaults; repeat the saved choices that differ.
    pub fn restore_remote_settings(&mut self){
        let(st,d)=(&self.chrome.state,hud::State::default());let c=&mut self.chrome.commands;
        if st.quality!=d.quality {c.push(hud::Command::Quality(st.quality));}
        if st.fps!=d.fps {c.push(hud::Command::Fps(st.fps));}
        if st.chroma!=d.chroma {c.push(hud::Command::Chroma(st.chroma));}
    }
    pub fn state(&self)->&hud::State{&self.chrome.state}
    pub fn sync_chroma(&mut self,chroma:u32){if chroma!=0 {self.chrome.state.chroma=chroma;}}
    pub fn set_stage(&mut self,stage:&str){self.chrome.stage=stage.into();}
    pub fn window_size(&self)->(u32,u32){self.canvas.window().size()}
    pub fn has_picture(&self)->bool{self.last_frame.is_some()}
    pub fn set_net(&mut self,net:hud::NetStats){
        let show=self.chrome.state.stats;let changed=self.chrome.net.as_ref()!=Some(&net);
        self.chrome.net=Some(net);
        if show && changed {self.redraw();}
    }
    fn tab_visible(&self)->bool{!(self.chrome.state.fullscreen&&self.chrome.state.hide_tab)||self.chrome.menu.open}
    fn menu_layout(&self)->(hud::Layer,hud::Layout){hud::menu_layer(self.chrome.font.as_ref(),&self.chrome.state,&self.chrome.menu,self.chrome.scale)}
    fn menu_origin(&self,lay:&hud::Layout)->(f32,f32){
        let size=self.canvas.window().size();
        // The category list sits centered under the tab; the cascade card grows to its right.
        let shadow=14.*self.chrome.scale;let main=lay.panels.first().map_or(lay.w,|p|p.w);
        (((size.0 as f32-main)/2.-shadow).max(-shadow).min(size.0 as f32-lay.w+shadow).round(),(hud::TAB_H*self.chrome.scale+4.-shadow).round())
    }
    pub fn redraw(&mut self){
        if let Some(frame)=self.last_frame.take(){if let Err(e)=self.draw(&frame){log::warn!("Viewer redraw failed: {e}");}self.last_frame=Some(frame);}
        else {self.loading_frame(true);}
    }
    fn blit(&mut self,layer:&hud::Layer,x:i32,y:i32)->Result<(),String>{
        let creator=self.canvas.texture_creator();
        let mut t=creator.create_texture_streaming(PixelFormatEnum::RGBA32,layer.w,layer.h).map_err(|_|"Texture creation failed")?;
        t.set_blend_mode(sdl2::render::BlendMode::Blend);
        t.update(None,&layer.px,layer.w as usize*4).map_err(|_|"Texture update failed")?;
        self.canvas.copy(&t,None,Some(Rect::new(x,y,layer.w,layer.h)))
    }
    fn label(&mut self,text:&str,size:f32,color:hud::Rgba)->hud::Layer{
        let key=(text.to_owned(),size.to_bits(),u32::from_le_bytes(color));
        if self.chrome.labels.len()>64 {self.chrome.labels.clear();}
        let font=self.chrome.font.as_ref();
        let layer=self.chrome.labels.entry(key).or_insert_with(||hud::label_layer(font,text,size,color));
        hud::Layer{w:layer.w,h:layer.h,px:layer.px.clone()}
    }
    /// Network panel, control tab and menu over the picture.
    fn draw_chrome(&mut self)->Result<(),String>{
        let size=self.canvas.window().size();let scale=self.chrome.scale;
        if self.chrome.state.stats { if let Some(net)=self.chrome.net.clone() {
            let layer=hud::stats_layer(self.chrome.font.as_ref(),&hud::stats_lines(&net),scale);
            self.blit(&layer,size.0 as i32-layer.w as i32-(12.*scale) as i32,size.1 as i32-layer.h as i32-(12.*scale) as i32)?;
        }}
        if self.tab_visible() {
            let r=hud::tab_rect(self.chrome.font.as_ref(),size,scale);
            let layer=hud::tab_layer(self.chrome.font.as_ref(),scale,self.chrome.tab_hover||self.chrome.menu.open);
            self.blit(&layer,r.x.round() as i32,0)?;
        }
        if self.chrome.menu.open {
            // Rasterizing the menu is costly; reuse it until what it shows changes.
            let c=&self.chrome;
            let cached=matches!(&c.menu_cache,Some((st,m,sc,_)) if *st==c.state&&*m==c.menu&&*sc==c.scale);
            if !cached {let (layer,_)=self.menu_layout();self.chrome.menu_cache=Some((self.chrome.state.clone(),self.chrome.menu.clone(),scale,layer));}
            let lay=hud::layout(self.chrome.font.as_ref(),&self.chrome.state,&self.chrome.menu,scale);let o=self.menu_origin(&lay);
            let (st,m,sc,layer)=self.chrome.menu_cache.take().expect("menu cached above");
            let r=self.blit(&layer,o.0 as i32,o.1 as i32);
            self.chrome.menu_cache=Some((st,m,sc,layer));r?;
        }
        Ok(())
    }
    /// Animated "connecting" screen until the first remote picture; throttled to ~60 fps.
    pub fn loading_frame(&mut self,force:bool){
        let now=std::time::Instant::now();
        if !force && now.duration_since(self.chrome.last_loading)<std::time::Duration::from_millis(16) {return}
        self.chrome.last_loading=now;
        let size=self.canvas.window().size();let scale=self.chrome.scale;
        self.canvas.set_draw_color(LOADING_BG);self.canvas.clear();
        let t=now.duration_since(self.chrome.started).as_secs_f32();
        let spin=hud::spinner(t,(56.*scale) as u32);
        let cy=size.1 as i32/2-(40.*scale) as i32;
        let _=self.blit(&spin,(size.0 as i32-spin.w as i32)/2,cy-spin.h as i32/2);
        let (title,stage)=(self.chrome.title.clone(),self.chrome.stage.clone());
        let a=self.label(&title,20.*scale,[240,242,245,255]);
        let _=self.blit(&a,(size.0 as i32-a.w as i32)/2,cy+(48.*scale) as i32);
        // The stage line breathes gently so a long wait still looks alive.
        let alpha=(170.+60.*(t*2.2).sin()) as u8;
        let b=self.label(&stage,14.*scale,[160,166,176,255]);
        let mut b2=b;for p in b2.px.chunks_exact_mut(4){p[3]=(u16::from(p[3])*u16::from(alpha)/255) as u8;}
        let _=self.blit(&b2,(size.0 as i32-b2.w as i32)/2,cy+(84.*scale) as i32);
        if self.chrome.menu.open||self.tab_visible() {let _=self.draw_chrome();}
        self.canvas.present();
    }
    fn draw(&mut self,frame:&Frame)->Result<(),String>{
        // Pad bars belong to the decoded canvas, not the remote picture: viewport and input use the
        // content aspect, and only the content sub-rectangle of the texture is drawn.
        let content=frame.content.unwrap_or((frame.width,frame.height));
        self.frame=content;let v=self.viewport();let creator=self.canvas.texture_creator();
        let mut texture=creator.create_texture_streaming(PixelFormatEnum::RGB24,frame.width,frame.height).map_err(|_|"Texture creation failed")?;
        texture.update(None,&frame.rgb,frame.width as usize*3).map_err(|_|"Texture update failed")?;
        self.canvas.set_draw_color(sdl2::pixels::Color::BLACK);self.canvas.clear();
        let src=super::input::content_rect((frame.width,frame.height),content)
            .map(|(x,y,w,h)|Rect::new(x.round() as i32,y.round() as i32,w.round().max(1.) as u32,h.round().max(1.) as u32));
        self.canvas.copy(&texture,src,Some(Rect::new(v.x.round() as i32,v.y.round() as i32,v.width.round().max(1.) as u32,v.height.round().max(1.) as u32)))?;
        drop(texture);self.draw_chrome()?;self.canvas.present();Ok(())
    }
}
/// On Wayland SDL draws its title bar through libdecor; the GTK plugin calls GTK3 from SDL's thread
/// while the tool windows run gtk_main on theirs, and the two crash in GTK's style code (SIGSEGV on
/// closing the file window). Offer libdecor only its cairo plugin unless the user chose a directory.
#[cfg(uu_gtk)]
fn cairo_decorations(){
    if std::env::var_os("LIBDECOR_PLUGIN_DIR").is_some() {return}
    let Some(cairo)=["/usr/lib64/libdecor/plugins-1","/usr/lib/x86_64-linux-gnu/libdecor/plugins-1","/usr/lib/libdecor/plugins-1"]
        .iter().map(|d|std::path::Path::new(d).join("libdecor-cairo.so")).find(|p|p.exists()) else {return};
    let dir=std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir,std::path::PathBuf::from).join("uuremote-libdecor");
    let link=dir.join("libdecor-cairo.so");
    if std::fs::create_dir_all(&dir).is_ok() && (link.exists() || std::os::unix::fs::symlink(&cairo,&link).is_ok()) {
        std::env::set_var("LIBDECOR_PLUGIN_DIR",&dir);
    }
}
const LOADING_BG:sdl2::pixels::Color=sdl2::pixels::Color::RGB(17,19,24);
/// sdl2 0.37 transmutes every event type into its `SDL_EventType` enum, which is undefined
/// behaviour for a type SDL 2 does not define (an abort with debug checks). sdl2-compat on SDL3
/// delivers such types (e.g. 0x207), so drop them before the crate converts the event.
fn poll_events(_pump:&mut sdl2::EventPump)->Vec<Event>{
    let mut events=Vec::new();
    loop{
        let mut raw=std::mem::MaybeUninit::<sdl2::sys::SDL_Event>::uninit();
        // SAFETY: the pump proves SDL events are initialised on this thread; SDL_PollEvent fills the event when it returns 1.
        if unsafe{sdl2::sys::SDL_PollEvent(raw.as_mut_ptr())}!=1 {return events}
        let raw=unsafe{raw.assume_init()};
        if sdl2_event_type(unsafe{raw.type_}) {events.push(Event::from_ll(raw));}
    }
}
/// The values of sdl2-sys 0.37 `SDL_EventType`.
fn sdl2_event_type(t:u32)->bool{
    matches!(t,0|0x100..=0x107|0x150|0x200|0x201|0x300..=0x305|0x400..=0x403|0x600..=0x607|0x650..=0x659|0x700..=0x702
        |0x800..=0x802|0x900|0x1000..=0x1003|0x1100|0x1101|0x1200|0x2000|0x2001|0x7F00|0x8000|0xFFFF)
}
fn slider(lay:&hud::Layout)->Option<(f32,f32)>{lay.items.iter().find_map(|(_,h)|match h{hud::Hit::Slider(a,len)=>Some((*a,*len)),_=>None})}
/// Runs `fut` while the window shows the loading animation; closing the window cancels it.
pub async fn loading<T>(window:Option<&mut Window>,stage:&str,fut:impl std::future::Future<Output=T>)->Result<T,String>{
    let Some(window)=window else {return Ok(fut.await)};
    window.set_stage(stage);
    tokio::pin!(fut);
    let mut tick=tokio::time::interval(std::time::Duration::from_millis(16));
    loop {tokio::select!{
        value=&mut fut=>return Ok(value),
        _=tick.tick()=>{
            for event in poll_events(&mut window.events) {
                if matches!(event,Event::Quit{..}|Event::Window{win_event:WindowEvent::Close,..}) {return Err("Viewer window closed".into())}
                let _=window.chrome_event(&event);
            }
            window.loading_frame(false);
        },
    }}
}
fn clipboard_text(bytes:&[u8])->Option<String>{
    if bytes.len()>super::clipboard::MAX_TEXT{return None}
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}
fn read_clipboard()->Result<Option<String>,()>{
    // SDL2 0.37's String wrapper panics on invalid UTF-8; validate native bytes before copying.
    // SDL still acquires the selection synchronously; the bridge timeout cannot interrupt it.
    unsafe{
        let data=sdl2::sys::SDL_GetClipboardText();
        if data.is_null(){return Err(())}
        let length=libc::strnlen(data,super::clipboard::MAX_TEXT+1);
        let text=clipboard_text(std::slice::from_raw_parts(data.cast::<u8>(),length));
        sdl2::sys::SDL_free(data.cast());Ok(text)
    }
}
fn translate(events:impl IntoIterator<Item=Event>,input:&mut Input,viewport:Viewport,focused:&mut bool)->(bool,Vec<serde_json::Value>){
    translate_actions(events,input,viewport,focused,&mut false)
}
fn translate_actions(events:impl IntoIterator<Item=Event>,input:&mut Input,viewport:Viewport,focused:&mut bool,files:&mut bool)->(bool,Vec<serde_json::Value>){
        let mut messages=Vec::new();let mut quit=false;
        for event in events{
            let message=match event {
                Event::Quit{..}|Event::Window{win_event:WindowEvent::Close,..}=>{quit=true;messages.extend(input.release_all());None},
                Event::Window{win_event:WindowEvent::FocusLost,..}=>{*focused=false;messages.extend(input.release_all());None},
                Event::Window{win_event:WindowEvent::FocusGained,..}=>{*focused=true;None},
                Event::KeyDown{scancode:Some(sdl2::keyboard::Scancode::F),keymod,repeat,..}
                    if *focused && keymod.intersects(sdl2::keyboard::Mod::LCTRLMOD|sdl2::keyboard::Mod::RCTRLMOD)
                        && keymod.intersects(sdl2::keyboard::Mod::LALTMOD|sdl2::keyboard::Mod::RALTMOD)=>{
                            if !repeat{*files=true;messages.extend(input.release_all());}None
                        },
                _ if !*focused || !input.enabled()=>None,
                Event::KeyDown{scancode:Some(s),repeat:false,..}=>input.key(s as i32,true),
                Event::KeyUp{scancode:Some(s),..}=>input.key(s as i32,false),
                Event::MouseMotion{x,y,..}=>viewport.motion(x as f64,y as f64),
                Event::MouseButtonDown{mouse_btn,..}=>button(mouse_btn).and_then(|b|input.button(b,true)),
                Event::MouseButtonUp{mouse_btn,..}=>button(mouse_btn).and_then(|b|input.button(b,false)),
                Event::MouseWheel{x,y,precise_x,precise_y,direction,..}=>{
                    let(dx,dy)=if precise_x!=0.||precise_y!=0.{(f64::from(precise_x),f64::from(precise_y))}else{(f64::from(x),f64::from(y))};
                    let(dx,dy)=match direction{sdl2::mouse::MouseWheelDirection::Flipped=>(-dx,-dy),_=>(dx,dy)};
                    input.scroll(dx,dy)
                },
                _=>None,
            };if let Some(m)=message{
                // Several motions in one poll: only the newest position matters. Sending each one
                // queues a drag behind itself on a slow (relayed) path; clicks keep their order.
                let motion=|m:&serde_json::Value|m.get("action").and_then(serde_json::Value::as_str)==Some("mouse_move_absolute");
                if motion(&m) && messages.last().is_some_and(motion) {messages.pop();}
                messages.push(m);
            }
        } (quit,messages)
}

fn needs_redraw(event:&Event)->bool{
    matches!(event,Event::Window{win_event:WindowEvent::SizeChanged(..)|WindowEvent::Resized(..)|WindowEvent::Exposed|WindowEvent::Maximized|WindowEvent::Restored,..})
}
fn button(b:MouseButton)->Option<i32>{match b{MouseButton::Left=>Some(1),MouseButton::Right=>Some(2),MouseButton::Middle=>Some(4),_=>None}}
impl FrameSink for Window {
    fn present(&mut self,frame:&Frame)->Result<(),String>{
        self.last_frame=Some(Frame{width:frame.width,height:frame.height,rgb:frame.rgb.clone(),content:frame.content});
        self.draw(frame)
    }
}
pub async fn run(session:&mut Session,window:&mut Window,decoder:&mut dyn super::desktop::Decoder,view_only:bool,device:&str)->Result<(),String>{
    #[cfg(uu_gtk)]
    let mut files:Option<tokio::task::JoinHandle<Result<(),String>>>=None;
    let(tx,mut commands)=tokio::sync::mpsc::channel::<Command>(8);
    if std::env::var("UU_VIEWER_COMMANDS").as_deref()==Ok("1") {
        std::thread::spawn(move||{
            use std::io::Read;
            let mut input=std::io::stdin().lock();let mut line=Vec::new();let mut byte=[0];
            while input.read(&mut byte).ok()==Some(1){
                if byte[0]==b'\n'{
                    let cmd=match line.as_slice(){b"files"=>Some(Command::Files),b"type"=>Some(Command::Type),_=>None};
                    line.clear();
                    if let Some(cmd)=cmd { if tx.blocking_send(cmd).is_err(){break} }
                }
                else if line.len()<32{line.push(byte[0]);}else{break}
            }
        });
    }else{drop(tx);}
    let(type_tx,mut type_rx)=tokio::sync::mpsc::channel::<super::type_ipc::PendingType>(4);
    let type_ipc=match super::type_ipc::Server::bind(device).await {
        Ok(server)=>{
            log::info!("Type IPC ready for device key {}", &super::type_ipc::device_key(device)[..12]);
            Some(tokio::spawn(async move { server.run(type_tx).await; }))
        }
        Err(e)=>{ log::warn!("Type IPC not started: {e}"); None }
    };
    let mut command_open=true;let mut requested=false;let mut type_requested=false;let mut restored=false;
    let mut type_job:Option<TypeJob>=None;
    let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|_|"Cannot watch termination")?;
    let mut audio=super::audio::Playback::new(&window._sdl,session.peer.audio.clone());
    let mut input=Input::new(view_only);input.set_mac(window.mac);let mut tick=tokio::time::interval(std::time::Duration::from_millis(8));
    let mut heartbeat=tokio::time::interval(std::time::Duration::from_secs(1));
    let deadline=tokio::time::sleep(std::time::Duration::from_secs(30));tokio::pin!(deadline);
    window.set_stage(crate::i18n::s("正在传输画面…","Waiting for the remote picture…"));
    if let Some(playback)=audio.as_mut(){playback.volume=window.state().volume;}
    if window.state().grab_keys {window.canvas.window_mut().set_keyboard_grab(true);}
    let started=std::time::Instant::now();let mut seq=3_000_000u64;
    let mut last=(0u64,0u64,0u64,0u64,std::time::Instant::now());
    let mut codec_name="--";
    // After a local drop the decoder has no reference picture; P-frames would only smear blocks.
    let mut key_gate=super::desktop::KeyGate::new();
    let mut next_thumb=std::time::Instant::now()+std::time::Duration::from_secs(5);
    let result=async {
        loop {tokio::select!{
            command=commands.recv(),if command_open=>{
                match command {
                    Some(Command::Files)=>{requested=true},
                    Some(Command::Type)=>{type_requested=true},
                    None=>{command_open=false},
                }
            },
            pending=type_rx.recv()=>{
                if let Some(mut pending)=pending {
                    if type_job.is_some() {
                        let _=pending.reply.send(super::type_ipc::TypeResponse{ok:false,events:0,chars:0,snapshot:None,error:Some("Secure input busy".into())});
                    } else {
                        let mut snapshot_create_new=false;
                        if pending.request.snapshot.as_ref().map_or(true, |s| s.is_empty()) {
                            if let Ok(path)=super::type_ipc::default_snapshot_path(device) {
                                pending.request.snapshot=Some(path.display().to_string());
                                snapshot_create_new=true;
                            }
                        }
                        match start_type_job(pending, window.mac, input.enabled(), snapshot_create_new) {
                            Ok(job)=>type_job=Some(job),
                            Err(())=>{},
                        }
                    }
                }
            },
            _=terminate.recv()=>return Ok(()),
            _=tokio::signal::ctrl_c()=>return Ok(()),
            result=session.step()=>result?,
            _=&mut deadline,if !session.peer.observed.echo_received.load(std::sync::atomic::Ordering::Acquire)=>return Err("CONTROL echo timed out".into()),
            _=heartbeat.tick()=>{
                if input.enabled() && session.peer.ready(super::rtc::ReadyPolicy::EchoOnly,false){session.peer.send_input(&Input::heartbeat()).await?;}
                let o=&session.peer.observed;let s=&super::desktop::STATS;let r=std::sync::atomic::Ordering::Relaxed;
                log::info!("Viewer desktop stats: control_open={} echo={} media_packets={} access_units={} presented={} decode_errors={}",
                    o.control_open.load(r),o.echo_received.load(r),o.media_packets.load(r),s[0].load(r),s[1].load(r),s[2].load(r));
                // Network-status panel: per-second deltas of local counters plus the selected ICE pair.
                let now=(s[1].load(r),o.media_bytes.load(r),o.video_received.load(r),o.video_expected.load(r),std::time::Instant::now());
                let secs=now.4.duration_since(last.4).as_secs_f64().max(0.2);
                let expected=now.3.saturating_sub(last.3);let lost=expected.saturating_sub(now.2.saturating_sub(last.2));
                let (rtt_ms,route)=session.peer.route().await;
                window.sync_chroma(o.chroma.load(r));
                let size=window.last_frame.as_ref().map_or((0,0),|f|f.content.unwrap_or((f.width,f.height)));
                window.set_net(hud::NetStats{elapsed:started.elapsed().as_secs(),route,fps:((now.0-last.0) as f64/secs).round() as u32,
                    kbps:(now.1-last.1) as f64*8./1000./secs,rtt_ms,loss:if expected>0 {lost as f64*100./expected as f64} else {0.},size,
                    codec:codec_name,chroma:window.state().chroma});
                last=now;
                if std::time::Instant::now()>=next_thumb { if let Some(frame)=window.last_frame.as_ref() {
                    if let Err(e)=super::thumbs::save(device,frame){log::debug!("Viewer thumbnail not saved: {e}");}
                    next_thumb=std::time::Instant::now()+std::time::Duration::from_secs(30);
                }}
            },
            _=tick.tick()=>{
                if let Some(playback)=audio.as_mut(){if playback.pump().is_err(){log::warn!("Viewer audio playback stopped");audio=None;}}
                let (quit,mut messages)=window.poll(&mut input);
                if !window.has_picture() {window.loading_frame(false);}
                if !restored && session.peer.ready(super::rtc::ReadyPolicy::EchoOnly,false) {restored=true;window.restore_remote_settings();}
                for command in window.take_commands() {
                    seq+=1;
                    let control=match command {
                        hud::Command::Quality(q)=>Some(super::wire::capture_setting(seq,&[(2,q)])),
                        hud::Command::Fps(f)=>Some(super::wire::capture_setting(seq,&[(1,match f{30=>1,60=>2,90=>3,_=>4}),(18,u64::from(f))])),
                        hud::Command::Chroma(c)=>Some(super::wire::capture_setting(seq,&[(7,u64::from(c))])),
                        hud::Command::FollowRemote(true)=>Some(super::wire::capture_setting(seq,&[(10,3)])),
                        hud::Command::FollowRemote(false)=>{let (w,h)=window.window_size();let (w,h)=(u64::from(w&!1),u64::from(h&!1));
                            Some(super::wire::capture_setting(seq,&[(5,w),(6,h),(16,w),(17,h)]))},
                        // 远程结束后自动锁屏: SimpleAction 19, value 2 = on, 1 = off.
                        hud::Command::AutoLock(on)=>Some(super::wire::simple_action(seq,19,Some(if on {2} else {1}))),
                        // 静音运行: on = SimpleAction 14 then 18, off = 16 (src/system_actions.rs).
                        hud::Command::HostMute(true)=>{let _=session.peer.send_control(super::wire::simple_action(seq,14,None)).await;seq+=1;Some(super::wire::simple_action(seq,18,None))},
                        hud::Command::HostMute(false)=>Some(super::wire::simple_action(seq,16,None)),
                        hud::Command::Volume(v)=>{if let Some(playback)=audio.as_mut(){playback.volume=v;}None},
                        hud::Command::Watch(on)=>{messages.extend(input.release_all());input.set_view_only(on||view_only);None},
                        hud::Command::Disconnect=>return Ok(()),
                        _=>None,
                    };
                    if let Some(data)=control { if let Err(e)=session.peer.send_control(data).await {log::warn!("Viewer control command not sent: {e}");} }
                }
                window.poll_clipboard();
                requested|=std::mem::take(&mut window.files);
                #[cfg(uu_gtk)] {
                    if files.as_ref().is_some_and(|f|f.is_finished()){
                        if let Some(f)=files.take(){let _=f.await;}session.peer.tools.lock().unwrap().take();
                    }
                    if requested && !(files.is_some() && super::tools_ui::closed()) && session.peer.channels[super::tools::FILE].ready_state()==webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
                        if files.is_some(){super::tools_ui::present();}
                        else {let(io,rx)=super::tools::attach(&session.peer)?;
                            let name=super::device_label(std::env::var("UU_VIEWER_DEVICE_NAME").ok().as_deref()).to_owned();
                            files=Some(tokio::spawn(async move{super::tools_ui::files(io,rx,&name).await}));}
                        requested=false;
                    }
                }
                #[cfg(not(uu_gtk))] {if requested{log::warn!("File window requires GTK3");requested=false;}}
                                if type_requested {
                    type_requested=false;
                    if type_job.is_some() {
                        log::warn!("Secure type command ignored: busy");
                    } else if !input.enabled() {
                        log::warn!("Secure type command ignored: view-only session");
                    } else if std::env::var_os("UU_INPUT_ENV").is_none() {
                        log::warn!("Secure type command ignored: set UU_INPUT_ENV (stdin is the command channel)");
                    } else {
                        match super::secure_input::load_secret(std::io::Cursor::new([] as [u8;0])) {
                            Ok(mut secret)=>{
                                match enqueue_local_type(&mut type_job, &mut secret, window.mac, None, 0, std::time::Duration::ZERO, None) {
                                    Ok(())=>{},
                                    Err(e)=>log::warn!("Secure type failed: {e}"),
                                }
                            }
                            Err(e)=>log::warn!("Secure type load failed: {e}"),
                        }
                    }
                }
                if let Some(job)=type_job.as_mut() {
                    if let Err(e)=pump_type_job(job, session, window).await {
                        log::warn!("Secure type job failed: {e}");
                        type_job=None;
                    } else if job.done {
                        type_job=None;
                    }
                }
                if session.peer.ready(super::rtc::ReadyPolicy::EchoOnly,false){for m in messages{session.peer.send_input(&m).await?;}}
                else {input.release_all();}
                if quit{return Ok(())}
                // At most one AU per iteration keeps local event polling responsive.
                if let Some(au)=session.peer.desktop.units.pop(){
                    if session.peer.desktop.reset.swap(false,std::sync::atomic::Ordering::AcqRel){decoder.reset();key_gate.arm();}
                    if !key_gate.admit(au.codec,&au.data) {session.peer.desktop.pli.store(true,std::sync::atomic::Ordering::Release);super::desktop::drain(decoder,window)?;}
                    else {
                        decoder.set_codec(au.codec)?;
                        codec_name=if au.codec==super::media::Codec::H265 {"H.265"} else {"H.264"};
                        super::desktop::display(decoder,window,&au.data,au.codec,&session.peer.desktop.pli)?;
                    }}
                // A picture can finish after its access unit was written; show it without waiting
                // for the next one (a single keystroke may change the remote screen only once).
                else {super::desktop::drain(decoder,window)?;}
            }
        }}
    }.await;
    if let Some(frame)=window.last_frame.as_ref() {if let Err(e)=super::thumbs::save(device,frame){log::debug!("Viewer thumbnail not saved: {e}");}}
    #[cfg(uu_gtk)] if let Some(f)=files {f.abort();let _=f.await;super::tools_ui::hide();session.peer.tools.lock().unwrap().take();}
    if let Some(t)=type_ipc {t.abort();let _=t.await;}
    for m in input.release_all(){let _=session.peer.send_input(&m).await;}
    result
}

struct TypeJob {
    actions: super::secure_input::ActionList,
    index: usize,
    delay: std::time::Duration,
    next_at: tokio::time::Instant,
    chars: usize,
    snapshot_path: Option<std::path::PathBuf>,
    snapshot_create_new: bool,
    phase: TypePhase,
    reply: Option<tokio::sync::oneshot::Sender<super::type_ipc::TypeResponse>>,
    done: bool,
}
enum TypePhase { Sending, WaitSnap(tokio::time::Instant) }

fn start_type_job(mut pending: super::type_ipc::PendingType, mac: bool, enabled: bool, snapshot_create_new: bool) -> Result<TypeJob, ()> {
    let reply = pending.reply;
    let respond_err = |reply: tokio::sync::oneshot::Sender<super::type_ipc::TypeResponse>, msg: String| {
        let _ = reply.send(super::type_ipc::TypeResponse{ok:false,events:0,chars:0,snapshot:None,error:Some(msg)});
    };
    if !enabled {
        respond_err(reply, "view-only session".into());
        return Err(());
    }
    let mut secret = super::secure_input::Secret::new(std::mem::take(&mut pending.request.secret));
    if let Err(e) = secret.omit_last_chars(pending.request.omit_last) {
        respond_err(reply, e);
        return Err(());
    }
    let chars = secret.chars_count();
    let actions = match super::secure_input::expand(secret.as_str(), mac) {
        Ok(a) => a,
        Err(e) => { respond_err(reply, e); return Err(()); }
    };
    drop(secret);
    log::info!("Secure input prepared: chars={} events={} delay_ms={}", chars, actions.len(), pending.request.delay_ms);
    let snapshot_path = pending.request.snapshot.as_ref().filter(|s| !s.is_empty()).map(std::path::PathBuf::from);
    Ok(TypeJob {
        actions,
        index: 0,
        delay: std::time::Duration::from_millis(pending.request.delay_ms.min(5_000)),
        next_at: tokio::time::Instant::now(),
        chars,
        snapshot_path,
        snapshot_create_new,
        phase: TypePhase::Sending,
        reply: Some(reply),
        done: false,
    })
}

fn enqueue_local_type(
    slot: &mut Option<TypeJob>,
    secret: &mut super::secure_input::Secret,
    mac: bool,
    snapshot: Option<std::path::PathBuf>,
    omit_last: usize,
    delay: std::time::Duration,
    reply: Option<tokio::sync::oneshot::Sender<super::type_ipc::TypeResponse>>,
) -> Result<(), String> {
    secret.omit_last_chars(omit_last)?;
    let chars = secret.chars_count();
    let actions = super::secure_input::expand(secret.as_str(), mac)?;
    drop(std::mem::replace(secret, super::secure_input::Secret::new(String::new())));
    log::info!("Secure input prepared: chars={} events={}", chars, actions.len());
    *slot = Some(TypeJob {
        actions,
        index: 0,
        delay,
        next_at: tokio::time::Instant::now(),
        chars,
        snapshot_path: snapshot,
        snapshot_create_new: false,
        phase: TypePhase::Sending,
        reply,
        done: false,
    });
    Ok(())
}

async fn pump_type_job(job: &mut TypeJob, session: &mut Session, window: &mut Window) -> Result<(), String> {
    use super::type_ipc::TypeResponse;
    match job.phase {
        TypePhase::Sending => {
            if !session.peer.ready(super::rtc::ReadyPolicy::EchoOnly, false) {
                return Ok(());
            }
            if tokio::time::Instant::now() < job.next_at {
                return Ok(());
            }
            if job.index >= job.actions.len() {
                let wait = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
                job.phase = TypePhase::WaitSnap(wait);
                return Ok(());
            }
            let action = &job.actions[job.index];
            session.peer.send_input(&action.to_json()).await?;
            job.index += 1;
            if !job.delay.is_zero() {
                job.next_at = tokio::time::Instant::now() + job.delay;
            }
            Ok(())
        }
        TypePhase::WaitSnap(deadline) => {
            if tokio::time::Instant::now() < deadline {
                return Ok(());
            }
            let mut snap = None;
            if let Some(path) = job.snapshot_path.clone() {
                if let Some(frame) = window.last_frame.as_ref() {
                    match super::type_ipc::write_png_rgb(&path, frame.width, frame.height, &frame.rgb, job.snapshot_create_new) {
                        Ok(()) => {
                            log::info!("Secure input snapshot written: {}", path.display());
                            snap = Some(path.display().to_string());
                        }
                        Err(e) => log::warn!("Secure input snapshot failed: {e}"),
                    }
                } else {
                    log::warn!("Secure input snapshot skipped: no decoded frame yet");
                }
            }
            let events = job.actions.len();
            let chars = job.chars;
            if let Some(reply) = job.reply.take() {
                let _ = reply.send(TypeResponse {
                    ok: true,
                    events,
                    chars,
                    snapshot: snap,
                    error: None,
                });
            } else {
                log::info!("Secure input finished: chars={} events={}", chars, events);
            }
            job.done = true;
            Ok(())
        }
    }
}

#[cfg(test)] mod tests {
    use super::*;
    // macOS: SDL video must start on the main thread, which the test harness does not use; CI runs it on Linux.
    #[cfg_attr(target_os="macos",ignore)]
    #[tokio::test] async fn clipboard_requests_use_existing_window(){
        if std::env::var_os("UU_VIEWER_CLIPBOARD_TEST_CHILD").is_none(){
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","viewer::sdl::tests::clipboard_requests_use_existing_window","--test-threads=1"])
                .env("SDL_VIDEODRIVER","dummy").env("UU_VIEWER_CLIPBOARD_TEST_CHILD","1").output().unwrap();
            assert!(output.status.success(),"{}{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return
        }
        let mut window=Window::new().unwrap();
        let tx=window.clipboard_tx.clone();
        use super::super::clipboard::ClipboardRequest;
        for text in ["clipboard text\n",""]{
            let(reply,rx)=tokio::sync::oneshot::channel();
            tx.send(ClipboardRequest::Write(text.into(),reply)).await.unwrap();
            window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok(true));
            let(reply,rx)=tokio::sync::oneshot::channel();
            tx.send(ClipboardRequest::Read(reply)).await.unwrap();
            window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok((!text.is_empty()).then(||text.to_owned())));
            let(reply,rx)=tokio::sync::oneshot::channel();
            tx.send(ClipboardRequest::Read(reply)).await.unwrap();
            window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok(None));
        }
        window.focused=false;window.clipboard_dirty=true;
        let(reply,rx)=tokio::sync::oneshot::channel();
        tx.send(ClipboardRequest::Read(reply)).await.unwrap();
        window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok(None));assert!(window.clipboard_dirty);
        let(reply,rx)=tokio::sync::oneshot::channel();
        tx.send(ClipboardRequest::Write("background".into(),reply)).await.unwrap();
        window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok(false));
        window.focused=true;
        let(reply,rx)=tokio::sync::oneshot::channel();
        tx.send(ClipboardRequest::Read(reply)).await.unwrap();
        window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok(None));
        window.clipboard_dirty=false;
        window._sdl.event().unwrap().push_event(Event::Window{timestamp:0,window_id:window.canvas.window().id(),win_event:WindowEvent::FocusGained}).unwrap();
        window.poll(&mut Input::new(true));assert!(window.clipboard_dirty);
        let bridge=window.clipboard_bridge();assert!(bridge.enabled);
        drop(window);
        assert!(tx.is_closed());drop(bridge);
        let window=Window::new().unwrap();assert!(window.clipboard_bridge().enabled);
    }
    #[test] fn only_event_types_sdl2_defines_reach_the_crate(){
        use sdl2::sys::SDL_EventType as T;
        for t in [T::SDL_QUIT,T::SDL_LOCALECHANGED,T::SDL_DISPLAYEVENT,T::SDL_WINDOWEVENT,T::SDL_TEXTEDITING_EXT,T::SDL_MOUSEWHEEL,
            T::SDL_JOYBATTERYUPDATED,T::SDL_CONTROLLERSENSORUPDATE,T::SDL_FINGERMOTION,T::SDL_MULTIGESTURE,T::SDL_CLIPBOARDUPDATE,
            T::SDL_DROPCOMPLETE,T::SDL_AUDIODEVICEREMOVED,T::SDL_SENSORUPDATE,T::SDL_RENDER_DEVICE_RESET,T::SDL_POLLSENTINEL,T::SDL_USEREVENT]{
            assert!(sdl2_event_type(t as u32),"{:#x}",t as u32);
        }
        for t in [0x151,0x202,0x207,0x306,0x404,0x608,0x65A,0x703,0x901,0x1004,0x1102,0x1201,0x2002,0x8001]{assert!(!sdl2_event_type(t),"{t:#x}");}
    }
    #[test] fn clipboard_text_rejects_invalid_or_large_data_and_keeps_empty_text(){
        assert_eq!(clipboard_text(b""),Some(String::new()));
        assert_eq!(clipboard_text(b"text\n"),Some("text\n".into()));
        assert_eq!(clipboard_text(&[255]),None);
        assert_eq!(clipboard_text(&vec![b'a';super::super::clipboard::MAX_TEXT+1]),None);
    }
    #[test] fn file_shortcut_is_local_in_control_and_watch_modes(){
        use sdl2::keyboard::{Scancode as S,Mod as M};
        for watch in [false,true]{
            let mut input=Input::new(watch);let mut focused=true;let mut files=false;
            input.key(S::LCtrl as i32,true);input.key(S::LAlt as i32,true);
            let key=|repeat|Event::KeyDown{timestamp:0,window_id:1,keycode:None,scancode:Some(S::F),keymod:M::LCTRLMOD|M::LALTMOD,repeat};
            let (_,out)=translate_actions([key(false),key(true),Event::KeyUp{timestamp:0,window_id:1,keycode:None,scancode:Some(S::F),keymod:M::NOMOD,repeat:false}],
                &mut input,Viewport::fit((800,600),(1920,1080)),&mut focused,&mut files);
            assert!(files);assert!(out.iter().all(|v|v["action"]=="kbd_release"));
            assert!(input.release_all().is_empty());
        }
    }
    #[test] fn compositor_resize_and_expose_repaint_the_last_frame(){
        let w=|win_event|Event::Window{timestamp:0,window_id:1,win_event};
        for e in [WindowEvent::SizeChanged(1280,800),WindowEvent::Resized(1280,800),WindowEvent::Exposed,WindowEvent::Maximized,WindowEvent::Restored]{assert!(needs_redraw(&w(e)));}
        for e in [WindowEvent::FocusGained,WindowEvent::Moved(0,0)]{assert!(!needs_redraw(&w(e)));}
    }
    #[test] fn wheel_events_become_scroll_messages(){
        use sdl2::mouse::MouseWheelDirection;
        let mut input=Input::default();let mut focused=true;
        let v=Viewport::fit((800,600),(1920,1080));
        let wheel=|direction,x,y|Event::MouseWheel{timestamp:0,window_id:1,which:0,mouse_x:0,mouse_y:0,x,y,precise_x:0.,precise_y:0.,direction};
        let(_,out)=translate([wheel(MouseWheelDirection::Normal,0,1),wheel(MouseWheelDirection::Flipped,0,1),wheel(MouseWheelDirection::Normal,0,0)],&mut input,v,&mut focused);
        assert_eq!(out.len(),2);
        assert_eq!(out[0]["action"],"mouse_scroll");
        assert_eq!(out[0]["delta_y"],120);
        assert_eq!(out[1]["delta_y"],-120);
        // Trackpads report fractional precise deltas; a quarter notch is 30 wire units.
        let precise=Event::MouseWheel{timestamp:0,window_id:1,which:0,mouse_x:0,mouse_y:0,x:0,y:0,precise_x:0.,precise_y:-0.25,direction:MouseWheelDirection::Normal};
        let(_,out)=translate([precise],&mut input,v,&mut focused);
        assert_eq!(out[0]["delta_y"],-30);
        let mut watch=Input::new(true);let mut watching=true;
        let(_,out)=translate([wheel(MouseWheelDirection::Normal,0,1)],&mut watch,v,&mut watching);
        assert!(out.is_empty());
    }
    #[test] fn watch_events_never_emit_input_and_still_close(){
        let mut input=Input::new(true);let mut focused=true;
        assert!(input.key(4,true).is_none());assert!(input.button(1,true).is_none());assert!(!input.enabled());
        let events=[
            Event::KeyDown{timestamp:0,window_id:1,keycode:None,scancode:Some(sdl2::keyboard::Scancode::A),keymod:sdl2::keyboard::Mod::NOMOD,repeat:false},
            Event::MouseMotion{timestamp:0,window_id:1,which:0,mousestate:sdl2::mouse::MouseState::from_sdl_state(0),x:12,y:34,xrel:1,yrel:1},
            Event::MouseButtonDown{timestamp:0,window_id:1,which:0,mouse_btn:MouseButton::Left,clicks:1,x:12,y:34},
            Event::MouseButtonUp{timestamp:0,window_id:1,which:0,mouse_btn:MouseButton::Left,clicks:1,x:12,y:34},
            Event::KeyUp{timestamp:0,window_id:1,keycode:None,scancode:Some(sdl2::keyboard::Scancode::A),keymod:sdl2::keyboard::Mod::NOMOD,repeat:false},
            Event::Window{timestamp:0,window_id:1,win_event:WindowEvent::FocusLost},
            Event::Window{timestamp:0,window_id:1,win_event:WindowEvent::FocusGained},Event::Quit{timestamp:0}];
        let (quit,out)=translate(events,&mut input,Viewport::fit((800,600),(1920,1080)),&mut focused);
        assert!(quit);assert!(out.is_empty());assert!(input.release_all().is_empty());
    }
    #[test] fn motions_in_one_poll_send_only_the_newest_and_keep_click_order(){
        let mut input=Input::default();let mut focused=true;
        let motion=|x,y|Event::MouseMotion{timestamp:0,window_id:1,which:0,mousestate:sdl2::mouse::MouseState::from_sdl_state(0),x,y,xrel:1,yrel:1};
        let down=Event::MouseButtonDown{timestamp:0,window_id:1,which:0,mouse_btn:MouseButton::Left,clicks:1,x:0,y:0};
        let v=Viewport::fit((800,600),(800,600));
        let (_,out)=translate([motion(10,10),motion(20,20),down,motion(30,30),motion(40,40)],&mut input,v,&mut focused);
        let actions:Vec<&str>=out.iter().map(|m|m["action"].as_str().unwrap()).collect();
        assert_eq!(actions.len(),3,"{out:?}");
        assert_eq!(actions[0],"mouse_move_absolute");assert_eq!(actions[2],"mouse_move_absolute");assert_ne!(actions[1],"mouse_move_absolute");
        assert!((out[0]["abs_x"].as_f64().unwrap()-20./800.).abs()<0.01 && (out[2]["abs_x"].as_f64().unwrap()-40./800.).abs()<0.01,"{out:?}");
    }
    #[test] fn focus_event_releases_and_suppresses_input_until_focus_returns(){
        let mut input=Input::default();input.key(4,true);input.button(1,true);let mut focused=true;
        let focus=|win_event|Event::Window{timestamp:0,window_id:1,win_event};
        let key=Event::KeyDown{timestamp:0,window_id:1,keycode:None,scancode:Some(sdl2::keyboard::Scancode::A),keymod:sdl2::keyboard::Mod::NOMOD,repeat:false};
        let v=Viewport::fit((800,600),(1920,1080));
        let (quit,out)=translate([focus(WindowEvent::FocusLost),key.clone()],&mut input,v,&mut focused);
        assert!(!quit);assert_eq!(out.len(),2);assert!(input.release_all().is_empty());assert!(!focused);
        let (_,out)=translate([focus(WindowEvent::FocusGained),key,Event::Quit{timestamp:0}],&mut input,v,&mut focused);
        assert_eq!(out.len(),2);assert_eq!(out[0]["action"],"kbd_press");assert_eq!(out[1]["action"],"kbd_release");
    }
}
