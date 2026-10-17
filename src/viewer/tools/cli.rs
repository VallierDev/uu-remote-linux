//! Command-line adapters; terminal output is intentional user output, never a log.
use super::*;
use std::os::fd::AsRawFd;
use tokio::io::AsyncWriteExt;
struct Tty {fd:i32,saved:libc::termios}
impl Tty {
    fn raw()->Result<Self,String>{
        let fd=std::io::stdin().as_raw_fd();let mut saved=unsafe{std::mem::zeroed()};
        if unsafe{libc::tcgetattr(fd,&mut saved)}!=0{return Err("Terminal command requires a local TTY".into())}
        let mut raw=saved;unsafe{libc::cfmakeraw(&mut raw)};
        if unsafe{libc::tcsetattr(fd,libc::TCSANOW,&raw)}!=0{return Err("Cannot set terminal raw mode".into())}Ok(Self{fd,saved})
    }
}
impl Drop for Tty {fn drop(&mut self){unsafe{libc::tcsetattr(self.fd,libc::TCSANOW,&self.saved);}}}
pub fn size()->(u16,u16){let mut w:libc::winsize=unsafe{std::mem::zeroed()};if unsafe{libc::ioctl(0,libc::TIOCGWINSZ as _,&mut w)}==0{(w.ws_col.max(1),w.ws_row.max(1))}else{(80,24)}}
pub async fn terminal(io:Transport,rx:mpsc::Receiver<Packet>,session:Option<u32>)->Result<(),String>{
    let(t,output)=terminal::Terminal::new(io,rx);t.check_env().await?;
    let(c,r)=size();let id=match session{Some(id)=>t.attach(id,c,r).await?,None=>t.open(c,r).await?};
    let result=terminal_loop(&t,output,id).await;let _=t.close(id).await;result
}
#[async_trait::async_trait]
pub trait TerminalInput:Sync {
    async fn write(&self,id:u32,data:&[u8])->Result<(),String>;
    async fn resize(&self,id:u32,cols:u16,rows:u16)->Result<(),String>;
}
#[async_trait::async_trait]
impl TerminalInput for terminal::Terminal {
    async fn write(&self,id:u32,data:&[u8])->Result<(),String>{self.write(id,data).await}
    async fn resize(&self,id:u32,cols:u16,rows:u16)->Result<(),String>{self.resize(id,cols,rows).await}
}
pub async fn terminal_loop(t:&impl TerminalInput,mut output:mpsc::Receiver<terminal::Event>,id:u32)->Result<(),String>{
    async{
        let _tty=Tty::raw()?;
        use std::os::unix::fs::OpenOptionsExt;
        let input=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open("/dev/tty").map_err(|_|"Cannot open local TTY")?;
        let input=tokio::io::unix::AsyncFd::new(input).map_err(|_|"Cannot watch local TTY")?;let mut stdout=tokio::io::stdout();let mut buf=vec![0;16*1024];
        let mut resize=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).map_err(|_|"Cannot watch terminal size")?;
        let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|_|"Cannot watch termination")?;
        loop{tokio::select!{
            _=terminate.recv()=>break,
            n=async {
                loop {let mut ready=input.readable().await?;
                    match ready.try_io(|fd|std::io::Read::read(&mut fd.get_ref(),&mut buf)){Ok(n)=>break n,Err(_)=>continue}
                }
            }=>{let n=n.map_err(|_|"TTY read failed")?;if n==0{break}t.write(id,&buf[..n]).await?;},
            _=resize.recv()=>{let(c,r)=size();t.resize(id,c,r).await?;},
            e=output.recv()=>match e{
                Some(terminal::Event::Output{session,data})if session==id=>{stdout.write_all(&data).await.map_err(|_|"TTY write failed")?;stdout.flush().await.map_err(|_|"TTY flush failed")?;},
                Some(terminal::Event::Exit{session,code})if session==id=>{if code!=0{return Err(format!("Remote shell exited with code {code}"))}break},
                Some(terminal::Event::Error{..})|None=>return Err("Terminal session failed".into()),_=>{},
            }
        }}Ok(())
    }.await
}
pub async fn files(io:Transport,rx:mpsc::Receiver<Packet>,args:&[String])->Result<(),String>{
    let(f,mut progress)=files::Files::new(io,rx);
    let reporter=tokio::spawn(async move{while let Some(p)=progress.recv().await{eprintln!("{} / {} bytes",p.transferred,p.total);}});
    let operation=async {match args.first().map(String::as_str){
        Some("ls")if args.len()==2=>match f.list_dir(&args[1]).await{Ok(entries)=>{for e in entries{println!("{}",serde_json::json!({"kind":e.kind,"name":e.name,"size":e.size,"path":e.path}));}Ok(())},Err(e)=>Err(e)},
        Some("get")if args.len()==3=>f.download(&args[1],&args[2]).await,
        Some("put")if args.len()==3=>f.upload(&args[1],&args[2]).await,
        _=>Err("files DEVICE ls PATH | get REMOTE LOCAL | put LOCAL REMOTE".into()),
    }};
    tokio::pin!(operation);
    let result=tokio::select!{r=&mut operation=>r,_=tokio::signal::ctrl_c()=>{f.cancel();operation.await}};
    reporter.abort();result
}
pub async fn portmap(io:Transport,rx:mpsc::Receiver<Packet>,local:u16,host:&str,port:u16)->Result<(),String>{
    let p=portmap::PortMap::new(io,rx);let id=p.add_rule("CLI",local,host,port).await?;
    eprintln!("Listening on 127.0.0.1:{}",p.status()[0].local_port);
    let result=tokio::select!{r=tokio::signal::ctrl_c()=>r.map_err(|_|"Cannot watch interrupt".to_owned()),_=p.closed()=>Err("Forwarding transport closed".into())};
    p.remove_rule(id).await?;result
}
