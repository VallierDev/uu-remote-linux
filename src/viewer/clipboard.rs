//! Session-scoped text clipboard. Payloads never enter logs or process arguments.
use super::{rtc::Observed,wire};
use crate::{clipboard_protocol as text,clipboard_rich as rich,protocol::{decode_wire,WireValue}};
use std::{path::PathBuf,process::Stdio,sync::{Arc,atomic::{AtomicBool,Ordering}},time::Duration};
use tokio::{io::{AsyncReadExt,AsyncWriteExt},sync::mpsc};
use webrtc::data_channel::RTCDataChannel;

// Same limit as src/clipboard.rs; shared host codecs enforce it on both wire formats.
pub const MAX_TEXT:usize=30*1024;
pub const CHANNEL:usize=1; // TEXT_DATA_CHANNEL; native host clipboard state is scoped to this channel.
const POLL:Duration=Duration::from_millis(500);
static WARNED:AtomicBool=AtomicBool::new(false);
fn warn(){if !WARNED.swap(true,Ordering::Relaxed){log::warn!("Viewer text clipboard unavailable; synchronization disabled");}}
#[derive(Clone)]
struct CommandBackend {read:PathBuf,read_args:Vec<&'static str>,write:PathBuf,write_args:Vec<&'static str>}
#[derive(Clone)]
enum Backend {
    Command(CommandBackend),
    #[cfg(feature="viewer-sdl")]
    Sdl(mpsc::Sender<ClipboardRequest>),
}
#[cfg(feature="viewer-sdl")]
pub enum ClipboardRequest {
    Read(tokio::sync::oneshot::Sender<Result<Option<String>,()>>),
    Write(String,tokio::sync::oneshot::Sender<Result<(),()>>),
}
fn executable(name:&str)->Option<PathBuf>{
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").and_then(|p|std::env::split_paths(&p).map(|d|d.join(name))
        .find(|p|std::fs::metadata(p).is_ok_and(|m|m.is_file() && m.permissions().mode()&0o111!=0)))
}
impl Backend {
    fn detect()->Option<Self>{
        if std::env::var("UU_VIEWER_CLIPBOARD").as_deref()==Ok("0"){return None}
        // Unit sessions must never inspect or change the developer's real clipboard.
        if !cfg!(target_os="linux") || cfg!(test){return None}
        let selected=CommandBackend::select(std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v|!v.is_empty()),executable).map(Self::Command);
        if selected.is_none(){warn();}selected
    }
    async fn read(&self)->Result<Option<String>,()>{
        match self {
            Self::Command(backend)=>backend.read().await,
            #[cfg(feature="viewer-sdl")]
            Self::Sdl(tx)=>{
                let(reply,rx)=tokio::sync::oneshot::channel();
                tx.send(ClipboardRequest::Read(reply)).await.map_err(|_|())?;
                tokio::time::timeout(Duration::from_secs(2),rx).await.map_err(|_|())?.map_err(|_|())?
            }
        }
    }
    async fn write(&self,value:&str)->Result<(),()>{
        if value.len()>MAX_TEXT || value.contains('\0'){return Err(())}
        match self {
            Self::Command(backend)=>backend.write(value).await,
            #[cfg(feature="viewer-sdl")]
            Self::Sdl(tx)=>{
                let(reply,rx)=tokio::sync::oneshot::channel();
                tx.send(ClipboardRequest::Write(value.into(),reply)).await.map_err(|_|())?;
                tokio::time::timeout(Duration::from_secs(2),rx).await.map_err(|_|())?.map_err(|_|())?
            }
        }
    }
}
impl CommandBackend {
    fn select(wayland:bool,find:impl Fn(&str)->Option<PathBuf>)->Option<Self>{
        let selected=if wayland{
            find("wl-paste").zip(find("wl-copy")).map(|(read,write)|Self{read,write,read_args:vec!["--no-newline","--type","text"],write_args:vec!["--type","text/plain;charset=utf-8"]})
        }else{None};
        selected.or_else(||find("xclip").map(|p|Self{read:p.clone(),write:p,read_args:vec!["-selection","clipboard","-out"],write_args:vec!["-selection","clipboard","-in"]}))
            .or_else(||find("xsel").map(|p|Self{read:p.clone(),write:p,read_args:vec!["--clipboard","--output"],write_args:vec!["--clipboard","--input"]}))
    }
    async fn read(&self)->Result<Option<String>,()>{
        let mut child=tokio::process::Command::new(&self.read).args(&self.read_args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|_|())?;
        let operation=async{
            let mut data=Vec::new();child.stdout.take().ok_or(())?.take((MAX_TEXT+1) as u64).read_to_end(&mut data).await.map_err(|_|())?;
            if data.len()>MAX_TEXT {let _=child.kill().await;return Ok(None)}
            // No text selection (including an image-only selection) is not a fatal backend failure.
            if !child.wait().await.map_err(|_|())?.success(){return Ok(None)}
            Ok(String::from_utf8(data).ok())
        };
        tokio::time::timeout(Duration::from_secs(2),operation).await.map_err(|_|())?
    }
    async fn write(&self,value:&str)->Result<(),()>{
        let mut child=tokio::process::Command::new(&self.write).args(&self.write_args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|_|())?;
        let operation=async{
            let mut stdin=child.stdin.take().ok_or(())?;stdin.write_all(value.as_bytes()).await.map_err(|_|())?;drop(stdin);
            if child.wait().await.map_err(|_|())?.success(){Ok(())}else{Err(())}
        };
        tokio::time::timeout(Duration::from_secs(2),operation).await.map_err(|_|())?
    }
}
#[derive(Default)]
struct SyncState {last:Option<String>,assembly:text::Assembly,rich:rich::State,seq:u64}
impl SyncState {
    async fn apply(&mut self,backend:&Backend,value:String)->Result<(),()>{
        backend.write(&value).await?;
        log::info!("Viewer remote clipboard applied: bytes={}",value.len());
        self.last=Some(value);Ok(())
    }
    fn next(&mut self)->u64{self.seq=self.seq.wrapping_add(1).max(100);self.seq}
    fn local(&mut self,value:String)->Result<Option<Vec<u8>>,&'static str>{
        if self.last.as_ref()==Some(&value){return Ok(None)}
        let id=self.next();let payload=self.rich.offer(id,value.clone())?;
        self.last=Some(value);Ok(Some(wire::envelope(self.next(),21,&payload)))
    }
    fn receive(&mut self,data:&[u8])->Result<(Vec<Vec<u8>>,Option<String>),&'static str>{
        if data.len()>MAX_TEXT*2+1024{return Err("clipboard message exceeds limit")}
        let fields=decode_wire(data).map_err(|_|"invalid clipboard envelope")?;
        let mut replies=Vec::new();let mut value=None;
        for field in fields {
            let WireValue::Bytes(payload)=field.value else{continue};
            match field.number {
                17=>if let Some(text)=self.assembly.accept(payload)?{value=Some(text)},
                21=>{
                    let seq=&mut self.seq;
                    let result=self.rich.request(payload,&mut ||{*seq=seq.wrapping_add(1).max(100);*seq})?;
                    if result.text.is_some(){value=result.text;}
                    for (kind,body) in result.messages {replies.push(wire::envelope(self.next(),kind,&body));}
                    if let Some((header,text))=text::read_text_request(payload)?{
                        value=Some(text);replies.push(wire::envelope(self.next(),22,&text::text_response(&header)));
                    }
                },
                22=>if let Some(text)=self.rich.response(payload)?{value=Some(text)},
                _=>{},
            }
        }
        Ok((replies,value))
    }
}
async fn send(channel:&RTCDataChannel,message:Vec<u8>)->Result<(),()>{
    tokio::time::timeout(Duration::from_secs(2),channel.send(&bytes::Bytes::from(message))).await.map_err(|_|())?.map_err(|_|())?;Ok(())
}
pub struct Bridge {backend:Option<Backend>,pub enabled:bool,tx:Option<mpsc::Sender<Vec<u8>>>,task:Option<tokio::task::JoinHandle<()>>}
impl Bridge {
    pub fn new(desktop:bool)->Self{
        let backend=if desktop{Backend::detect()}else{None};let enabled=backend.is_some();
        Self{backend,enabled,tx:None,task:None}
    }
    #[cfg(feature="viewer-sdl")]
    pub fn sdl(tx:mpsc::Sender<ClipboardRequest>)->Self{
        let backend=(std::env::var("UU_VIEWER_CLIPBOARD").as_deref()!=Ok("0")).then_some(Backend::Sdl(tx));
        Self{enabled:backend.is_some(),backend,tx:None,task:None}
    }
    pub fn start(&mut self,channel:&Arc<RTCDataChannel>,control:&Arc<RTCDataChannel>,observed:Arc<Observed>){
        let Some(backend)=self.backend.take() else{return};
        let (tx,mut rx)=mpsc::channel::<Vec<u8>>(16);self.tx=Some(tx);
        let channel=Arc::downgrade(channel);let control=Arc::downgrade(control);
        self.task=Some(tokio::spawn(async move{
            let mut state=SyncState::default();let mut tick=tokio::time::interval(POLL);
            let mut retries=0;let mut last_probe=std::time::Instant::now();
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let Some(channel)=channel.upgrade() else{break};
                tokio::select!{
                    data=rx.recv()=>{
                        let Some(data)=data else{break};
                        if !observed.clipboard_supported.load(Ordering::Acquire){continue}
                        match state.receive(&data){
                            Ok((replies,value))=>{
                                if let Some(value)=value {
                                    // Update only after the write succeeds; the next poll cannot echo it.
                                    if state.apply(&backend,value).await.is_err(){warn();break}
                                }
                                let mut failed=false;for reply in replies{if send(&channel,reply).await.is_err(){failed=true;break}}
                                if failed{break}
                            },
                            Err(_)=>log::warn!("Viewer clipboard message rejected"),
                        }
                    },
                    _=tick.tick()=>{
                        if observed.echo_received.load(Ordering::Acquire) && !observed.control_open.load(Ordering::Acquire){break}
                        if !observed.echo_received.load(Ordering::Acquire){continue}
                        if !observed.clipboard_supported.load(Ordering::Acquire){
                            // Host TEXT-channel initialization may finish after the initial CONTROL echo.
                            if retries<5 && last_probe.elapsed()>=Duration::from_secs(1){
                                retries+=1;last_probe=std::time::Instant::now();
                                let Some(control)=control.upgrade() else{break};
                                if send(&control,wire::echo_request_with_clipboard(1,true)).await.is_err(){break}
                            }
                            continue
                        }
                        if channel.ready_state()!=webrtc::data_channel::data_channel_state::RTCDataChannelState::Open{continue}
                        match backend.read().await {
                            Ok(Some(value))=>{let length=value.len();match state.local(value){
                                Ok(Some(message))=>{if send(&channel,message).await.is_err(){break}log::info!("Viewer local clipboard offered: bytes={length}");},
                                Ok(None)=>{},Err(_)=>{},
                            }},
                            Ok(None)=>{},Err(())=>{warn();break},
                        }
                    }
                }
            }
        }));
    }
    pub fn sender(&self)->Option<mpsc::Sender<Vec<u8>>>{self.tx.clone()}
    pub fn stop(&self){if let Some(task)=&self.task{task.abort();}}
}
impl Drop for Bridge{fn drop(&mut self){self.stop();}}

#[cfg(test)] mod tests {
    use super::*;
    #[cfg(feature="viewer-sdl")]
    #[tokio::test] async fn sdl_backend_requests_and_invalid_text(){
        let(tx,mut rx)=mpsc::channel(8);let backend=Backend::Sdl(tx);
        let ui=tokio::spawn(async move{
            let Some(ClipboardRequest::Write(text,reply))=rx.recv().await else{panic!()};
            assert_eq!(text,"text\n");reply.send(Ok(())).unwrap();
            let Some(ClipboardRequest::Read(reply))=rx.recv().await else{panic!()};
            reply.send(Ok(Some("text\n".into()))).unwrap();
        });
        assert!(backend.write("nul\0text").await.is_err());
        assert!(backend.write(&"a".repeat(MAX_TEXT+1)).await.is_err());
        backend.write("text\n").await.unwrap();assert_eq!(backend.read().await.unwrap().as_deref(),Some("text\n"));
        ui.await.unwrap();assert!(backend.read().await.is_err());
    }
    #[test] fn backend_priority_requires_a_complete_wayland_pair_and_falls_back(){
        let select=|wayland,names:&[&str]|CommandBackend::select(wayland,|n|names.contains(&n).then(||PathBuf::from(n)));
        let all=["wl-copy","wl-paste","xclip","xsel"];
        assert_eq!(select(true,&all).unwrap().write,PathBuf::from("wl-copy"));
        assert_eq!(select(false,&all).unwrap().write,PathBuf::from("xclip"));
        assert_eq!(select(true,&["wl-copy","xclip","xsel"]).unwrap().write,PathBuf::from("xclip"));
        assert_eq!(select(true,&["xsel"]).unwrap().write,PathBuf::from("xsel"));
        assert!(select(true,&[]).is_none());
    }
    #[test] fn limits_match_host_and_unchanged_or_remote_text_is_not_reoffered(){
        assert!(include_str!("../clipboard.rs").contains("pub const MAX_TEXT: usize = 30 * 1024;"));
        assert!(POLL>=Duration::from_millis(500));
        let mut state=SyncState::default();
        assert!(state.local("中文🙂\n".into()).unwrap().is_some());
        assert!(state.local("中文🙂\n".into()).unwrap().is_none());
        state.last=Some("remote".into());
        assert!(state.local("remote".into()).unwrap().is_none());
        assert!(state.local(String::new()).unwrap().is_some());
        assert!(state.local(String::new()).unwrap().is_none());
        assert!(state.local("a".repeat(MAX_TEXT+1)).is_err());
    }
    #[test] fn message17_fragments_and_mobile_rpc_keep_unicode_and_request_id(){
        let mut state=SyncState::default();let value="中文🙂\n".repeat(2200);
        let parts=text::chunks(&value).unwrap();
        for (i,part) in parts.iter().enumerate(){
            let (replies,received)=state.receive(&wire::envelope(1,17,&text::encode(3,part,i+1<parts.len()))).unwrap();
            assert!(replies.is_empty());
            assert_eq!(received,if i+1==parts.len(){Some(value.clone())}else{None});
        }
        let (replies,received)=state.receive(&wire::envelope(2,21,&text::text_request(73,"mobile"))).unwrap();
        assert_eq!(received.as_deref(),Some("mobile"));assert_eq!(replies.len(),1);
        let fields=decode_wire(&replies[0]).unwrap();
        assert!(fields.iter().any(|f|f.number==22));
        let WireValue::Bytes(response)=fields.iter().find(|f|f.number==22).unwrap().value else{panic!()};
        let WireValue::Bytes(header)=decode_wire(response).unwrap().into_iter().find(|f|f.number==1).unwrap().value else{panic!()};
        assert_eq!(decode_wire(header).unwrap()[0].value,WireValue::Varint(73));
        assert!(state.receive(&wire::envelope(1,17,&[18,1,255])).is_err());
        assert!(state.receive(&wire::envelope(1,17,&text::encode(1,&"a".repeat(MAX_TEXT+1),false))).is_err());
    }
    // Match webrtc.rs: CONTROL and TEXT have separate rich States, and only TEXT has a desktop sink.
    fn native_host_exchange(outbound:usize,reverse:bool)->(Option<String>,bool){
        let mut viewer=SyncState::default();let mut host=[rich::State::default(),rich::State::default()];let mut id=1000;
        let value="clipboard-中文🙂\n".to_owned();
        let offer=if reverse{wire::envelope(1,21,&host[1].offer(1,value.clone()).unwrap())}else{viewer.local(value.clone()).unwrap().unwrap()};
        let mut queue=std::collections::VecDeque::from([(reverse,if reverse{1}else{outbound},offer)]);
        let mut received=None;let mut unavailable=false;let mut steps=0;
        while let Some((to_viewer,channel,packet))=queue.pop_front(){
            steps+=1;assert!(steps<20);
            if to_viewer {
                // Capture DataConfirm status without reading or logging text.
                for field in decode_wire(&packet).unwrap(){
                    if let (22,WireValue::Bytes(body))=(field.number,field.value){
                        if let Some(clip)=super::super::tools::blob(body,5){
                            if let Some(confirm)=super::super::tools::blob(clip,2){unavailable|=super::super::tools::uint(confirm,1)==2;}
                        }
                    }
                }
                let (replies,text)=viewer.receive(&packet).unwrap();if text.is_some(){received=text;}
                queue.extend(replies.into_iter().map(|p|(false,outbound,p)));
            }else{
                for field in decode_wire(&packet).unwrap(){let WireValue::Bytes(body)=field.value else{continue};
                    if field.number==21 {let result=host[channel].request(body,&mut ||{id+=1;id}).unwrap();
                        if channel==1 && result.text.is_some(){received=result.text;}
                        queue.extend(result.messages.into_iter().map(|(kind,body)|(true,channel,wire::envelope(1,kind,&body))));
                    }else if field.number==22 {let text=host[channel].response(body).unwrap();if channel==1 && text.is_some(){received=text;}}
                }
            }
        }
        if reverse && received.is_some(){viewer.last=Some(value.clone());assert!(viewer.local(value).unwrap().is_none());}
        (received,unavailable)
    }
    #[test] fn control_clipboard_reproduces_native_host_failures(){
        assert_eq!(native_host_exchange(0,false),(None,false)); // Complete transfer, no CONTROL desktop sink.
        assert_eq!(native_host_exchange(0,true),(None,true)); // TEXT offer, empty CONTROL provider.
    }
    #[test] fn text_clipboard_negotiates_both_directions_with_native_host(){
        assert_eq!(wire::CHANNELS[CHANNEL],"TEXT_DATA_CHANNEL");
        for reverse in [false,true]{
            let (received,unavailable)=native_host_exchange(CHANNEL,reverse);
            assert_eq!(received.as_deref(),Some("clipboard-中文🙂\n"));assert!(!unavailable);
        }
    }
    #[test] fn clipboard_capability_is_version3_and_can_be_disabled(){
        assert!(wire::echo_clipboard_supported(&wire::echo_request_with_clipboard(1,true)));
        assert!(!wire::echo_clipboard_supported(&wire::echo_request_with_clipboard(1,false)));
        assert!(wire::echo_matches(&wire::echo_response_with_clipboard(2,1,true),1));
        assert!(!wire::echo_matches(&wire::echo_response_with_clipboard(2,7,true),1));
        assert!(!Bridge::new(false).enabled);assert!(!Bridge::new(true).enabled);
        let mut shape:wire::Shape=serde_json::from_str(include_str!("../../tests/fixtures/viewer/shape.json")).unwrap();
        shape.feature_flag=vec![[8,99]];
        let options=shape.connect_options("synthetic-device",false).unwrap();
        let WireValue::Bytes(flags)=decode_wire(&options).unwrap().into_iter().find(|f|f.number==11).unwrap().value else{panic!()};
        assert!(!decode_wire(flags).unwrap().iter().any(|f|f.number==8));
        let options=shape.connect_options("synthetic-device",true).unwrap();
        let WireValue::Bytes(flags)=decode_wire(&options).unwrap().into_iter().find(|f|f.number==11).unwrap().value else{panic!()};
        assert!(decode_wire(flags).unwrap().iter().any(|f|f.number==8 && f.value==WireValue::Varint(3)));
    }
    #[tokio::test] async fn backend_uses_stdin_preserves_text_and_bounds_reads(){
        use std::os::unix::fs::PermissionsExt;
        let root=std::env::temp_dir().join(format!("uu-clipboard-mock-{}",wire::uuid()));std::fs::create_dir(&root).unwrap();
        let script=root.join("mock");
        std::fs::write(&script,b"#!/bin/sh\ncd -- \"${0%/*}\" || exit 1\ncase \"$1\" in read) cat data;; write) cat > data;; esac\n").unwrap();
        std::fs::set_permissions(&script,std::fs::Permissions::from_mode(0o700)).unwrap();
        let command=CommandBackend{read:script.clone(),write:script,read_args:vec!["read"],write_args:vec!["write"]};
        let backend=Backend::Command(command.clone());
        let mut state=SyncState::default();
        for value in ["中文🙂\n", "", "literal $(echo secret) `echo secret`"]{
            state.apply(&backend,value.into()).await.unwrap();
            let read=backend.read().await.unwrap().unwrap();assert_eq!(read,value);
            assert!(state.local(read).unwrap().is_none());
        }
        let broken=Backend::Command(CommandBackend{write:root.join("missing"),..command});
        let previous=state.last.clone();assert!(state.apply(&broken,"failed".into()).await.is_err());assert_eq!(state.last,previous);
        std::fs::write(root.join("data"),vec![b'a';MAX_TEXT+1]).unwrap();assert!(backend.read().await.unwrap().is_none());
        std::fs::write(root.join("data"),[255]).unwrap();assert!(backend.read().await.unwrap().is_none());
        assert!(backend.write(&"a".repeat(MAX_TEXT+1)).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
