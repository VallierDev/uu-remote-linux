//! TERM types and JSON shapes follow src/terminal.rs (including bare LIST_RESP arrays).
use super::*;
use std::{collections::HashMap,sync::atomic::{AtomicU32,AtomicU64,Ordering}};
use tokio::sync::oneshot;
use serde_json::{json,Value};
pub const OPEN:u8=1;pub const STDIN:u8=2;pub const RESIZE:u8=3;pub const CLOSE:u8=4;
pub const STDOUT:u8=5;pub const EXIT:u8=6;pub const ERROR:u8=8;
pub const LIST_REQ:u8=9;pub const LIST_RESP:u8=10;pub const KILL_REQ:u8=11;pub const KILL_RESP:u8=12;pub const OPEN_RESP:u8=13;
pub struct Frame {pub kind:u8,pub session:u32,pub payload:Vec<u8>}
pub fn encode(kind:u8,session:u32,payload:&[u8])->Vec<u8>{
    let mut b=b"TERM\x01".to_vec();b.extend([kind,0,0]);b.extend(session.to_le_bytes());b.extend((payload.len() as u32).to_le_bytes());b.extend(payload);b
}
#[derive(Default)]
pub struct Decoder {buffer:Vec<u8>}
impl Decoder {
    pub fn push(&mut self,data:&[u8])->Result<Vec<Frame>,String>{
        if self.buffer.len()+data.len()>2*1024*1024 {return Err("Terminal buffer limit".into())}
        self.buffer.extend(data);let mut out=Vec::new();let mut at=0;
        while self.buffer.len()-at>=16 {
            let b=&self.buffer[at..];
            if &b[..5]!=b"TERM\x01" || !(1..=16).contains(&b[5]) {return Err("Invalid TERM header".into())}
            let len=u32::from_le_bytes(b[12..16].try_into().unwrap()) as usize;
            if len>1024*1024{return Err("TERM payload limit".into())}
            if b.len()<16+len {break}
            out.push(Frame{kind:b[5],session:u32::from_le_bytes(b[8..12].try_into().unwrap()),payload:b[16..16+len].to_vec()});at+=16+len;
        }
        self.buffer.drain(..at);Ok(out)
    }
}
#[derive(Clone)]
pub enum Event {Output{session:u32,data:Vec<u8>},Exit{session:u32,code:i64},Error{session:u32}}
type Pending=Arc<Mutex<HashMap<(u8,u64),oneshot::Sender<Result<Vec<u8>,String>>>>>;
struct Request {pending:Pending,key:(u8,u64)}
impl Drop for Request {fn drop(&mut self){self.pending.lock().unwrap().remove(&self.key);}}
pub struct Terminal {io:Transport,pending:Pending,next:AtomicU32,rpc:AtomicU64,worker:tokio::task::JoinHandle<()>}
impl Drop for Terminal {fn drop(&mut self){self.worker.abort();}}
impl Terminal {
    pub fn new(io:Transport,mut rx:mpsc::Receiver<Packet>)->(Self,mpsc::Receiver<Event>){
        let pending:Pending=Arc::new(Mutex::new(HashMap::new()));let p=pending.clone();let(tx,events)=mpsc::channel(256);
        let worker=tokio::spawn(async move {
            let mut decoder=Decoder::default();
            while let Some((channel,data))=rx.recv().await {
                // Terminal RPC answers (check_env) come on CONTROL or, from the official Mac host, on BINARY
                // next to TERM frames (as this project's host does).
                if channel==CONTROL || (channel==BINARY && !data.starts_with(b"TERM")) {
                    if let Some(r)=blob(&data,22) {if let (Some(h),Some(body))=(blob(r,1),blob(r,24)) {
                        if let Some(t)=p.lock().unwrap().remove(&(24,uint(h,1))) {let _=t.send(Ok(body.to_vec()));}
                    }}continue
                }
                if channel!=BINARY {continue}
                let Ok(frames)=decoder.push(&data) else {break};
                let mut overflow=false;
                for f in frames {
                    let event=match f.kind {
                        STDOUT=>Some(Event::Output{session:f.session,data:f.payload.clone()}),
                        EXIT=>Some(Event::Exit{session:f.session,code:serde_json::from_slice::<Value>(&f.payload).ok().and_then(|v|v["exit_code"].as_i64()).unwrap_or(-1)}),
                        ERROR=>Some(Event::Error{session:f.session}),_=>None,
                    };
                    if let Some(event)=event {if tx.try_send(event).is_err(){overflow=true;break}}
                    let key=if f.kind==OPEN_RESP {(OPEN_RESP,0)}else{(f.kind,f.session as u64)};
                    let payload=if f.kind==OPEN_RESP {
                        // Our host puts the new session in the frame header; the official Windows host answers with header 0,
                        // so only a nonzero header overrides the JSON body.
                        serde_json::from_slice::<Value>(&f.payload).ok().filter(Value::is_object).map(|mut v|{if f.session!=0{v["session_id"]=json!(f.session);}v.to_string().into_bytes()}).unwrap_or(f.payload)
                    }else{f.payload};
                    if let Some(t)=p.lock().unwrap().remove(&key){let _=t.send(Ok(payload));}
                }
                if overflow {break}
            }
            for (_,t) in p.lock().unwrap().drain(){let _=t.send(Err("Terminal channel closed or output overflow".into()));}
        });
        (Self{io,pending,next:AtomicU32::new(1),rpc:AtomicU64::new(10),worker},events)
    }
    async fn request(&self,key:(u8,u64),channel:usize,data:Vec<u8>)->Result<Vec<u8>,String>{
        if self.worker.is_finished(){return Err("Terminal channel closed".into())}
        let(tx,rx)=oneshot::channel();{
            let mut pending=self.pending.lock().unwrap();
            if pending.contains_key(&key){return Err("Terminal request already pending".into())}pending.insert(key,tx);
        }
        let _request=Request{pending:self.pending.clone(),key};
        let result=async {self.io.send(channel,data).await?;tokio::time::timeout(TIMEOUT,rx).await.map_err(|_|"Terminal request timeout")?.map_err(|_|"Terminal closed")?}.await;
        result
    }
    pub async fn check_env(&self)->Result<(),String>{
        let id=self.rpc.fetch_add(1,Ordering::Relaxed);let mut r=nested(1,&ints(&[(1,id)]));wire::bytes(28,&[],&mut r);
        // The official Windows host's rpc_main.proto has no check_env (requests end at 25): no answer means
        // "not implemented", not "unavailable"; go on and let OPEN report availability.
        match tokio::time::timeout(std::time::Duration::from_secs(3),self.request((24,id),CONTROL,wire::envelope(id,21,&r))).await{
            Err(_)=>Ok(()),
            Ok(b)=>{let b=b?;let code=uint(&b,1);
                if code!=0 {
                    let message=blob(&b,2).map(|m|String::from_utf8_lossy(m).chars().take(80).collect::<String>()).unwrap_or_default();
                    log::info!("Terminal check_env: code={code} message={message:?}");
                    return Err("Terminal environment unavailable".into())
                }Ok(())}
        }
    }
    pub async fn list_sessions(&self)->Result<Vec<Value>,String>{
        let id=self.next.fetch_add(1,Ordering::Relaxed);
        serde_json::from_slice(&self.request((LIST_RESP,id as u64),BINARY,encode(LIST_REQ,id,&[])).await?).map_err(|_|"Invalid terminal session list".into())
    }
    pub async fn open(&self,cols:u16,rows:u16)->Result<u32,String>{
        self.open_session(0,cols,rows).await
    }
    pub async fn attach(&self,id:u32,cols:u16,rows:u16)->Result<u32,String>{
        if id==0 || !self.list_sessions().await?.iter().any(|s|s["session_id"].as_u64()==Some(id as u64)){return Err("Terminal session no longer exists".into())}
        self.open_session(id,cols,rows).await
    }
    async fn open_session(&self,id:u32,cols:u16,rows:u16)->Result<u32,String>{
        // Host TerminalConn handles OPEN(nonzero) as reattachment. The official Windows host rejects an OPEN
        // without "option" ("terminal open: missing 'option' field", kError 1004); "none" is its plain mode.
        let b=self.request((OPEN_RESP,0),BINARY,encode(OPEN,id,json!({"cols":cols.max(1),"rows":rows.max(1),"option":"none"}).to_string().as_bytes())).await?;
        let v:Value=serde_json::from_slice(&b).map_err(|_|"Invalid terminal open response")?;
        if v["code"].as_i64()!=Some(0){return Err("Terminal open rejected".into())}
        let id=v["session_id"].as_u64().or_else(||v["session_id"].as_str().and_then(|s|s.parse().ok())).and_then(|n|u32::try_from(n).ok()).filter(|n|*n!=0);
        // The official Windows host answers {"code":0,"message"} with header 0 and keeps one terminal per
        // connection addressed as session 0 (it then prints "[terminal] resumed existing session").
        if id.is_none(){log::info!("Terminal open response without a session id: keys={:?}; using session 0",v.as_object().map(|o|o.keys().collect::<Vec<_>>()));}
        Ok(id.unwrap_or(0))
    }
    pub async fn write(&self,id:u32,data:&[u8])->Result<(),String>{for b in data.chunks(16*1024){self.io.send(BINARY,encode(STDIN,id,b)).await?}Ok(())}
    pub async fn resize(&self,id:u32,cols:u16,rows:u16)->Result<(),String>{self.io.send(BINARY,encode(RESIZE,id,json!({"cols":cols.max(1),"rows":rows.max(1)}).to_string().as_bytes())).await}
    pub async fn close(&self,id:u32)->Result<(),String>{self.io.send(BINARY,encode(CLOSE,id,&[])).await}
    pub async fn kill(&self,id:u32)->Result<(),String>{
        let b=self.request((KILL_RESP,id as u64),BINARY,encode(KILL_REQ,id,&[])).await?;
        if serde_json::from_slice::<Value>(&b).ok().and_then(|v|v["code"].as_u64())!=Some(0){return Err("Terminal kill rejected".into())}Ok(())
    }
}

/// The remote shell needs a real terminal emulator: `$TERMINAL`, else the first installed common one.
/// On failure the window stays open so the error is readable.
pub fn terminal_command(exe: &std::path::Path, id: &str, session: Option<u32>) -> Option<std::process::Command> {
    terminal_launcher(exe,"terminal",id,session)
}
pub fn local_terminal_command(exe:&std::path::Path,path:&std::path::Path,session:Option<u32>)->Option<std::process::Command>{
    terminal_launcher(exe,"terminal-local",path.to_str()?,session)
}
fn terminal_launcher(exe:&std::path::Path,mode:&str,id:&str,session:Option<u32>)->Option<std::process::Command>{
    let script = r#""$0" "$1" "$2" ${3:+"$3"} || { printf '\n终端连接失败，按回车关闭'; read _; }"#;
    let inner = ["sh", "-c", script, &exe.to_string_lossy(), mode, id].map(str::to_owned);
    let mut inner = inner.to_vec();
    if let Some(id) = session { inner.push(id.to_string()); }
    let found = |name: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()));
    let custom = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty() && found(t));
    let candidates = ["xdg-terminal-exec", "x-terminal-emulator", "kitty", "foot", "alacritty", "wezterm", "ghostty", "gnome-terminal", "konsole", "xfce4-terminal", "xterm"];
    let program = custom.or_else(|| candidates.iter().find(|c| found(c)).map(|c| c.to_string()))?;
    let prefix: &[&str] = match program.as_str() {
        "xdg-terminal-exec" | "kitty" | "foot" => &[],
        "wezterm" => &["start", "--"],
        "gnome-terminal" => &["--"],
        "xfce4-terminal" => &["-x"],
        _ => &["-e"],
    };
    let mut command = std::process::Command::new(program);
    command.args(prefix).args(inner).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    Some(command)
}

/// Only known metadata is allowed across the GTK boundary; never serialize the full host object.
pub fn session_rows(sessions:&[Value])->String{
    let mut rows:Vec<_>=sessions.iter().filter_map(|s|{
        let id=u32::try_from(s["session_id"].as_u64()?).ok().filter(|n|*n!=0)?;
        let shell=s["shell"].as_str().unwrap_or("--").chars().take(256).collect::<String>().replace(['\t','\n','\r','\0']," ");
        let state=match s["state"].as_str(){Some("running")=>crate::i18n::s("运行中","Running"),Some("detached")=>crate::i18n::s("已分离","Detached"),_=>"--"};
        Some((id,format!("{id}\t{shell}\t{}\t{state}\n",s["created_at_ms"].as_u64().unwrap_or(0))))
    }).collect();rows.sort_by_key(|r|r.0);rows.into_iter().map(|r|r.1).collect()
}
