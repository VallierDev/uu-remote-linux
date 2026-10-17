//! Local TTY clients share the list process's Terminal; no cloud credentials cross IPC.
use super::{terminal::{Terminal,Event},cli};
use std::{path::{Path,PathBuf},sync::Arc};
use tokio::{io::{AsyncRead,AsyncReadExt,AsyncWrite,AsyncWriteExt},net::{UnixListener,UnixStream},sync::{broadcast,mpsc,Mutex},task::JoinSet};
use serde::{Serialize,Deserialize};
const MAX:usize=4*1024*1024+1024;
#[derive(Serialize,Deserialize)]
enum Message {Open{session:Option<u32>,cols:u16,rows:u16},Ready(u32),Input(Vec<u8>),Resize(u16,u16),Output(Vec<u8>),Exit(i64),Close,Error}
async fn read(r:&mut (impl AsyncRead+Unpin))->Result<Message,String>{
    let n=r.read_u32().await.map_err(|_|"Local terminal closed")? as usize;
    if n>MAX{return Err("Local terminal frame limit".into())}
    let mut b=vec![0;n];r.read_exact(&mut b).await.map_err(|_|"Local terminal closed")?;
    serde_json::from_slice(&b).map_err(|_|"Invalid local terminal frame".into())
}
async fn write(w:&mut (impl AsyncWrite+Unpin),m:&Message)->Result<(),String>{
    let b=serde_json::to_vec(m).map_err(|_|"Invalid local terminal frame")?;
    if b.len()>MAX{return Err("Local terminal frame limit".into())}
    w.write_u32(b.len() as u32).await.map_err(|_|"Local terminal closed")?;
    w.write_all(&b).await.map_err(|_|"Local terminal closed".into())
}
pub struct Server {dir:PathBuf,pub path:PathBuf,listener:UnixListener}
impl Drop for Server {fn drop(&mut self){let _=std::fs::remove_file(&self.path);let _=std::fs::remove_dir(&self.dir);}}
impl Server {
    pub fn new()->Result<Self,String>{
        use std::os::unix::fs::DirBuilderExt;
        let dir=std::env::temp_dir().join(format!("uu-tty-{}",super::super::wire::uuid()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|_|"Cannot create private terminal directory")?;
        let path=dir.join("tty");
        match UnixListener::bind(&path){Ok(listener)=>Ok(Self{dir,path,listener}),Err(_)=>{let _=std::fs::remove_dir(dir);Err("Cannot bind local terminal socket".into())}}
    }
    pub async fn run(self,t:Arc<Terminal>,events:broadcast::Sender<Event>){
        let opening=Arc::new(Mutex::new(()));let active=Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));let mut clients=JoinSet::new();
        loop{tokio::select!{
            accepted=self.listener.accept(),if clients.len()<16=>match accepted{
                Ok((socket,_))=>{let(t,opening,active,output)=(t.clone(),opening.clone(),active.clone(),events.subscribe());clients.spawn(async move{let _=serve(socket,t,opening,active,output).await;});},
                Err(_)=>break,
            },
            _=clients.join_next(),if !clients.is_empty()=>{},
        }}
    }
}
type Sessions=Arc<std::sync::Mutex<std::collections::HashSet<u32>>>;
struct Active {id:u32,sessions:Sessions}
impl Drop for Active {fn drop(&mut self){self.sessions.lock().unwrap().remove(&self.id);}}
async fn serve<S:AsyncRead+AsyncWrite+Unpin+Send+'static>(mut socket:S,t:Arc<Terminal>,opening:Arc<Mutex<()>>,active:Sessions,mut output:broadcast::Receiver<Event>)->Result<(),String>{
    let first=tokio::time::timeout(super::TIMEOUT,read(&mut socket)).await.map_err(|_|"Local terminal open timeout")??;
    let Message::Open{session,cols,rows}=first else{return Err("Expected local terminal open".into())};
    let id={let _guard=opening.lock().await;
        if session.is_some_and(|id|active.lock().unwrap().contains(&id)){return Err("Terminal session already attached locally".into())}
        let id=match session{Some(id)=>t.attach(id,cols,rows).await?,None=>t.open(cols,rows).await?};
        active.lock().unwrap().insert(id);id
    };
    let _active=Active{id,sessions:active};
    let result=async{
        write(&mut socket,&Message::Ready(id)).await?;
        let(mut r,mut w)=tokio::io::split(socket);let(tx,mut commands)=mpsc::channel(32);
        let _reader=super::Abort(tokio::spawn(async move{loop{
            let message=read(&mut r).await;let failed=message.is_err();
            if tx.send(message).await.is_err() || failed{break}
        }}));
        loop{tokio::select!{
            m=commands.recv()=>match m.ok_or("Local terminal closed")??{
                Message::Input(b)=>t.write(id,&b).await?,Message::Resize(c,r)=>t.resize(id,c,r).await?,Message::Close=>break,
                _=>return Err("Invalid local terminal command".into()),
            },
            e=output.recv()=>match e{
                Ok(Event::Output{session,data})if session==id=>write(&mut w,&Message::Output(data)).await?,
                Ok(Event::Exit{session,code})if session==id=>{write(&mut w,&Message::Exit(code)).await?;break},
                Ok(Event::Error{session})if session==id=>{write(&mut w,&Message::Error).await?;break},
                Err(_)=>return Err("Local terminal output closed or overflowed".into()),_=>{},
            },
        }}Ok(())
    }.await;
    let _=t.close(id).await;result
}
struct Input(Mutex<tokio::net::unix::OwnedWriteHalf>);
#[async_trait::async_trait]
impl cli::TerminalInput for Input {
    async fn write(&self,_:u32,b:&[u8])->Result<(),String>{write(&mut *self.0.lock().await,&Message::Input(b.to_vec())).await}
    async fn resize(&self,_:u32,c:u16,r:u16)->Result<(),String>{write(&mut *self.0.lock().await,&Message::Resize(c,r)).await}
}
pub async fn client(path:&Path,session:Option<u32>)->Result<(),String>{
    let mut socket=UnixStream::connect(path).await.map_err(|_|"Terminal list is no longer available")?;
    let(c,r)=cli::size();write(&mut socket,&Message::Open{session,cols:c,rows:r}).await?;
    let Message::Ready(id)=read(&mut socket).await? else{return Err("Local terminal open failed".into())};
    let(mut r,w)=socket.into_split();let input=Input(Mutex::new(w));let(tx,rx)=mpsc::channel(256);
    let worker=tokio::spawn(async move{loop{
        let event=match read(&mut r).await{Ok(Message::Output(data))=>Event::Output{session:id,data},Ok(Message::Exit(code))=>Event::Exit{session:id,code},_=>break};
        if tx.send(event).await.is_err(){break}
    }});
    let result=cli::terminal_loop(&input,rx,id).await;
    worker.abort();let _=worker.await;let _=write(&mut *input.0.lock().await,&Message::Close).await;result
}

#[cfg(test)] mod tests {
    use super::*;
    struct Mock(mpsc::Sender<super::super::Packet>);
    #[async_trait::async_trait]
    impl super::super::Send for Mock {
        async fn send(&self,c:usize,b:Vec<u8>)->Result<(),String>{self.0.send((c,b)).await.map_err(|_|"Mock closed".into())}
    }
    #[tokio::test] async fn two_local_ttys_share_one_terminal_and_close_independently(){
        tokio::time::timeout(std::time::Duration::from_secs(3),async{
            use super::super::{terminal as term,BINARY};
            let(tx,mut requests)=mpsc::channel(256);let(input,rx)=mpsc::channel(256);
            let(t,mut events)=Terminal::new(super::super::Transport(Arc::new(Mock(tx))),rx);let t=Arc::new(t);
            let(output,_)=broadcast::channel(256);let sender=output.clone();
            let _relay=super::super::Abort(tokio::spawn(async move{while let Some(e)=events.recv().await{let _=sender.send(e);}}));
            let(closed,mut closes)=mpsc::channel(8);
            let _host=super::super::Abort(tokio::spawn(async move{
                let mut next=40;
                while let Some((channel,b))=requests.recv().await{
                    assert_eq!(channel,BINARY);let f=term::Decoder::default().push(&b).unwrap().remove(0);
                    let reply=match f.kind{
                        term::OPEN=>{next+=1;term::encode(term::OPEN_RESP,next,br#"{"code":0}"#)},
                        term::STDIN=>term::encode(term::STDOUT,f.session,&f.payload),
                        term::CLOSE=>{closed.send(f.session).await.unwrap();continue},_=>panic!("Unexpected remote request"),
                    };input.send((BINARY,reply)).await.unwrap();
                }
            }));
            let opening=Arc::new(Mutex::new(()));let active:Sessions=Default::default();let mut clients=Vec::new();let mut tasks=Vec::new();
            for _ in 0..2{
                let(mut client,server)=tokio::io::duplex(4096);
                let(t,opening,active,events)=(t.clone(),opening.clone(),active.clone(),output.subscribe());
                tasks.push(tokio::spawn(serve(server,t,opening,active,events)));
                write(&mut client,&Message::Open{session:None,cols:80,rows:24}).await.unwrap();
                let Message::Ready(id)=read(&mut client).await.unwrap()else{panic!()};clients.push((client,id));
            }
            let(mut first,id)=clients.remove(0);write(&mut first,&Message::Close).await.unwrap();
            assert_eq!(closes.recv().await.unwrap(),id);tasks.remove(0).await.unwrap().unwrap();
            let(mut second,id)=clients.remove(0);
            // Split an incoming frame while remote output is ready: the reader must retain framing.
            let b=serde_json::to_vec(&Message::Input(vec![0,255,27])).unwrap();second.write_u32(b.len() as u32).await.unwrap();second.write_all(&b[..2]).await.unwrap();
            assert!(output.send(Event::Output{session:id,data:b"synthetic".to_vec()}).is_ok());
            assert!(matches!(read(&mut second).await,Ok(Message::Output(b))if b==b"synthetic"));
            second.write_all(&b[2..]).await.unwrap();
            assert!(matches!(read(&mut second).await,Ok(Message::Output(b))if b==[0,255,27]));
            drop(second);assert_eq!(closes.recv().await.unwrap(),id);tasks.remove(0).await.unwrap().unwrap_err();
            assert!(active.lock().unwrap().is_empty());
        }).await.unwrap();
    }
    #[tokio::test] async fn framing_rejects_oversize_before_payload(){
        let(mut a,mut b)=tokio::io::duplex(64);a.write_u32(MAX as u32+1).await.unwrap();
        assert!(read(&mut b).await.err().unwrap().contains("limit"));
    }
    #[tokio::test] async fn framing_roundtrips_binary_and_detects_truncation(){
        let(mut a,mut b)=tokio::io::duplex(256);
        write(&mut a,&Message::Input(vec![0,255,27])).await.unwrap();
        assert!(matches!(read(&mut b).await,Ok(Message::Input(v))if v==[0,255,27]));
        a.write_u32(30).await.unwrap();drop(a);assert!(read(&mut b).await.is_err());
    }
}
