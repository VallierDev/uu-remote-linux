use super::{desktop::{Frame,FrameSink},input::{Input,Viewport},session::Session};
use sdl2::{event::{Event,WindowEvent},mouse::MouseButton,pixels::PixelFormatEnum,rect::Rect};
#[derive(Debug)] enum Command { Files, Type }
pub struct Window { canvas:sdl2::render::Canvas<sdl2::video::Window>, events:sdl2::EventPump, _sdl:sdl2::Sdl, frame:(u32,u32), focused:bool, files:bool, mac:bool,
    clipboard:sdl2::clipboard::ClipboardUtil, clipboard_dirty:bool, clipboard_tx:tokio::sync::mpsc::Sender<super::clipboard::ClipboardRequest>, clipboard_requests:tokio::sync::mpsc::Receiver<super::clipboard::ClipboardRequest>,
    pub last_frame:Option<Frame> }
impl Window {
    pub fn new()->Result<Self,String>{
        let sdl=sdl2::init()?;let video=sdl.video()?;
        let window=video.window("Native viewer (experimental)",1280,720).position_centered().resizable().build().map_err(|_|"Window creation failed")?;
        let mut canvas=window.into_canvas().build().map_err(|_|"Renderer creation failed")?;
        // Wayland maps a toplevel only after its first buffer; without this the window stays
        // invisible until the first decoded frame, which a stalled stream never delivers.
        canvas.set_draw_color(sdl2::pixels::Color::BLACK);canvas.clear();canvas.present();
        let events=sdl.event_pump()?;let clipboard=video.clipboard();
        let(clipboard_tx,clipboard_requests)=tokio::sync::mpsc::channel(16);
        Ok(Self{canvas,events,_sdl:sdl,frame:(1920,1080),focused:true,files:false,mac:false,clipboard,clipboard_dirty:true,clipboard_tx,clipboard_requests,last_frame:None})
    }
    pub fn clipboard_bridge(&self)->super::clipboard::Bridge{super::clipboard::Bridge::sdl(self.clipboard_tx.clone())}
    pub fn set_mac(&mut self,mac:bool){self.mac=mac;}
    fn viewport(&self)->Viewport {Viewport::fit(self.canvas.window().size(),self.frame)}
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
        let dirty=&mut self.clipboard_dirty;let mut redraw=false;
        let events=self.events.poll_iter().inspect(|event|{
            if matches!(event,Event::ClipboardUpdate{..}|Event::Window{win_event:WindowEvent::FocusGained,..}){*dirty=true;}
            redraw|=needs_redraw(event);
        });
        let result=translate_actions(events,input,viewport,&mut self.focused,&mut self.files);
        // A compositor resize (tiling, maximize) leaves the surface without a buffer of the new size
        // until the next frame; a static remote desktop sends none, so repaint the last frame now.
        if redraw {if let Some(frame)=self.last_frame.take(){
            log::debug!("Viewer window resized: {:?}",self.canvas.window().size());
            if let Err(e)=self.draw(&frame){log::warn!("Viewer redraw failed: {e}");}
            self.last_frame=Some(frame);
        }else{self.canvas.set_draw_color(sdl2::pixels::Color::BLACK);self.canvas.clear();self.canvas.present();}}
        result
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
        self.canvas.copy(&texture,src,Some(Rect::new(v.x.round() as i32,v.y.round() as i32,v.width.round().max(1.) as u32,v.height.round().max(1.) as u32)))?;self.canvas.present();Ok(())
    }
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
            };if let Some(m)=message{messages.push(m);}
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
    let mut command_open=true;let mut requested=false;let mut type_requested=false;
    let mut type_job:Option<TypeJob>=None;
    let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|_|"Cannot watch termination")?;
    let mut audio=super::audio::Playback::new(&window._sdl,session.peer.audio.clone());
    let mut input=Input::new(view_only);input.set_mac(window.mac);let mut tick=tokio::time::interval(std::time::Duration::from_millis(8));
    let mut heartbeat=tokio::time::interval(std::time::Duration::from_secs(1));
    let deadline=tokio::time::sleep(std::time::Duration::from_secs(30));tokio::pin!(deadline);
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
                    if session.peer.desktop.reset.swap(false,std::sync::atomic::Ordering::AcqRel){decoder.reset();}
                    decoder.set_codec(au.codec)?;
                    super::desktop::display(decoder,window,&au.data,au.codec,&session.peer.desktop.pli)?;}
            }
        }}
    }.await;
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
