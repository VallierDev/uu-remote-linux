use super::{desktop::{Frame,FrameSink},input::{Input,Viewport},session::Session};
use sdl2::{event::{Event,WindowEvent},mouse::MouseButton,pixels::PixelFormatEnum,rect::Rect};
pub struct Window { canvas:sdl2::render::Canvas<sdl2::video::Window>, events:sdl2::EventPump, _sdl:sdl2::Sdl, frame:(u32,u32), focused:bool, files:bool, mac:bool,
    clipboard:sdl2::clipboard::ClipboardUtil, clipboard_tx:tokio::sync::mpsc::Sender<super::clipboard::ClipboardRequest>, clipboard_requests:tokio::sync::mpsc::Receiver<super::clipboard::ClipboardRequest> }
impl Window {
    pub fn new()->Result<Self,String>{
        let sdl=sdl2::init()?;let video=sdl.video()?;
        let window=video.window("Native viewer (experimental)",1280,720).position_centered().resizable().build().map_err(|_|"Window creation failed")?;
        let canvas=window.into_canvas().build().map_err(|_|"Renderer creation failed")?;
        let events=sdl.event_pump()?;let clipboard=video.clipboard();
        let(clipboard_tx,clipboard_requests)=tokio::sync::mpsc::channel(16);
        Ok(Self{canvas,events,_sdl:sdl,frame:(1920,1080),focused:true,files:false,mac:false,clipboard,clipboard_tx,clipboard_requests})
    }
    pub fn clipboard_bridge(&self)->super::clipboard::Bridge{super::clipboard::Bridge::sdl(self.clipboard_tx.clone())}
    pub fn set_mac(&mut self,mac:bool){self.mac=mac;}
    fn viewport(&self)->Viewport {Viewport::fit(self.canvas.window().size(),self.frame)}
    fn poll_clipboard(&mut self){
        // Use the existing SDL window; wl-paste creates a transient toplevel on GNOME every poll.
        while let Ok(request)=self.clipboard_requests.try_recv(){
            use super::clipboard::{ClipboardRequest,MAX_TEXT};
            match request {
                ClipboardRequest::Read(reply)=>{
                    let result=if self.clipboard.has_clipboard_text(){
                        self.clipboard.clipboard_text().map(|text|(text.len()<=MAX_TEXT).then_some(text)).map_err(|_|())
                    }else{Ok(None)};
                    let _=reply.send(result);
                },
                ClipboardRequest::Write(text,reply)=>{let _=reply.send(self.clipboard.set_clipboard_text(&text).map_err(|_|()));},
            }
        }
    }
    pub fn poll(&mut self,input:&mut Input)->(bool,Vec<serde_json::Value>){
        let viewport=self.viewport();
        translate_actions(self.events.poll_iter(),input,viewport,&mut self.focused,&mut self.files)
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
                _=>None,
            };if let Some(m)=message{messages.push(m);}
        } (quit,messages)
}

fn button(b:MouseButton)->Option<i32>{match b{MouseButton::Left=>Some(1),MouseButton::Right=>Some(2),MouseButton::Middle=>Some(4),_=>None}}
impl FrameSink for Window {
    fn present(&mut self,frame:&Frame)->Result<(),String>{
        self.frame=(frame.width,frame.height);let v=self.viewport();let creator=self.canvas.texture_creator();
        let mut texture=creator.create_texture_streaming(PixelFormatEnum::RGB24,frame.width,frame.height).map_err(|_|"Texture creation failed")?;
        texture.update(None,&frame.rgb,frame.width as usize*3).map_err(|_|"Texture update failed")?;
        self.canvas.set_draw_color(sdl2::pixels::Color::BLACK);self.canvas.clear();
        self.canvas.copy(&texture,None,Some(Rect::new(v.x as i32,v.y as i32,v.width as u32,v.height as u32)))?;self.canvas.present();Ok(())
    }
}
pub async fn run(session:&mut Session,window:&mut Window,decoder:&mut dyn super::desktop::Decoder,view_only:bool)->Result<(),String>{
    #[cfg(uu_gtk)]
    let mut files:Option<tokio::task::JoinHandle<Result<(),String>>>=None;
    let(tx,mut commands)=tokio::sync::mpsc::channel(8);
    if std::env::var("UU_VIEWER_COMMANDS").as_deref()==Ok("1") {
        std::thread::spawn(move||{
            use std::io::Read;
            let mut input=std::io::stdin().lock();let mut line=Vec::new();let mut byte=[0];
            while input.read(&mut byte).ok()==Some(1){
                if byte[0]==b'\n'{if line==b"files" && tx.blocking_send(()).is_err(){break}line.clear();}
                else if line.len()<32{line.push(byte[0]);}else{break}
            }
        });
    }else{drop(tx);}
    let mut command_open=true;let mut requested=false;
    let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|_|"Cannot watch termination")?;
    let mut audio=super::audio::Playback::new(&window._sdl,session.peer.audio.clone());
    let mut input=Input::new(view_only);input.set_mac(window.mac);let mut tick=tokio::time::interval(std::time::Duration::from_millis(8));
    let mut heartbeat=tokio::time::interval(std::time::Duration::from_secs(1));
    let deadline=tokio::time::sleep(std::time::Duration::from_secs(30));tokio::pin!(deadline);
    let result=async {
        loop {tokio::select!{
            command=commands.recv(),if command_open=>{if command.is_some(){requested=true}else{command_open=false}},
            _=terminate.recv()=>return Ok(()),
            _=tokio::signal::ctrl_c()=>return Ok(()),
            result=session.step()=>result?,
            _=&mut deadline,if !session.peer.observed.echo_received.load(std::sync::atomic::Ordering::Acquire)=>return Err("CONTROL echo timed out".into()),
            _=heartbeat.tick()=>{
                if input.enabled() && session.peer.ready(super::rtc::ReadyPolicy::EchoOnly,false){session.peer.send_input(&Input::heartbeat()).await?;}
                let o=&session.peer.observed;let s=&super::desktop::STATS;let r=std::sync::atomic::Ordering::Relaxed;
                log::info!("Viewer desktop stats: control_open={} echo={} media_packets={} access_units={} presented={} decode_errors={}",
                    o.control_open.load(r),o.echo_received.load(r),o.media_packets.load(r),s[0].load(r),s[1].load(r),s[2].load(r));
            },
            _=tick.tick()=>{
                if let Some(playback)=audio.as_mut(){if playback.pump().is_err(){log::warn!("Viewer audio playback stopped");audio=None;}}
                let (quit,messages)=window.poll(&mut input);
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
                if session.peer.ready(super::rtc::ReadyPolicy::EchoOnly,false){for m in messages{session.peer.send_input(&m).await?;}}
                else {input.release_all();}
                if quit{return Ok(())}
                // At most one AU per iteration keeps local event polling responsive.
                if let Some(au)=session.peer.desktop.units.pop(){
                    if session.peer.desktop.reset.swap(false,std::sync::atomic::Ordering::AcqRel){decoder.reset();}
                    decoder.set_codec(au.codec)?;
                    super::desktop::display(decoder,window,&au.data,au.codec,&session.peer.desktop.pli)?;}
            }
        }}
    }.await;
    #[cfg(uu_gtk)] if let Some(f)=files {f.abort();let _=f.await;super::tools_ui::hide();session.peer.tools.lock().unwrap().take();}
    for m in input.release_all(){let _=session.peer.send_input(&m).await;}
    result
}

#[cfg(test)] mod tests {
    use super::*;
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
            window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok(()));
            let(reply,rx)=tokio::sync::oneshot::channel();
            tx.send(ClipboardRequest::Read(reply)).await.unwrap();
            window.poll_clipboard();assert_eq!(rx.await.unwrap(),Ok((!text.is_empty()).then(||text.to_owned())));
        }
        let bridge=window.clipboard_bridge();assert!(bridge.enabled);
        drop(window);
        assert!(tx.is_closed());drop(bridge);
        let window=Window::new().unwrap();assert!(window.clipboard_bridge().enabled);
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
