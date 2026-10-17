//! Message27/SYN JSON follows src/port_mapping.rs; TCP half-close is unsupported.
use super::*;
use std::{collections::HashMap,sync::atomic::{AtomicU64,Ordering}};
use tokio::{net::{TcpListener,TcpStream},io::{AsyncReadExt,AsyncWriteExt},sync::watch};
#[derive(Clone,Debug)]
pub struct Status {pub id:u64,pub name:String,pub local_port:u16,pub streams:usize,pub failures:u64}
struct Rule {status:Arc<Mutex<Status>>,stop:watch::Sender<bool>,task:tokio::task::JoinHandle<()>}
type Routes=Arc<Mutex<HashMap<(u64,u64),mpsc::Sender<(u64,Vec<u8>)>>>>;
pub struct PortMap {io:Transport,session:u64,next:Arc<AtomicU64>,routes:Routes,rules:Mutex<HashMap<u64,Rule>>,alive:watch::Receiver<()>,worker:tokio::task::JoinHandle<()>}
pub fn frame(session:u64,rule:u64,stream:u64,kind:u64,payload:&[u8])->Vec<u8>{let mut b=ints(&[(1,session),(2,rule),(3,stream),(4,kind)]);wire::bytes(5,payload,&mut b);wire::envelope(0,27,&b)}
struct Route {routes:Routes,key:(u64,u64),status:Arc<Mutex<Status>>}
impl Drop for Route {fn drop(&mut self){self.routes.lock().unwrap().remove(&self.key);self.status.lock().unwrap().streams-=1;}}
impl Drop for PortMap {fn drop(&mut self){self.worker.abort();for(_,r)in self.rules.lock().unwrap().drain(){let _=r.stop.send(true);}self.routes.lock().unwrap().clear();}}
impl PortMap {
    pub fn new(io:Transport,mut rx:mpsc::Receiver<Packet>)->Self{
        let session=rand::random::<u64>();let routes:Routes=Arc::new(Mutex::new(HashMap::new()));let route=routes.clone();
        let(life,alive)=watch::channel(());
        let worker=tokio::spawn(async move{
            let _life=life;
            while let Some((channel,b))=rx.recv().await {
                if channel!=FILE{continue}let Some(f)=blob(&b,27)else{continue};if uint(f,1)!=session{continue}
                let key=(uint(f,2),uint(f,3));let sender=route.lock().unwrap().get(&key).cloned();
                if let Some(tx)=sender {if tx.try_send((uint(f,4),blob(f,5).unwrap_or_default().to_vec())).is_err(){route.lock().unwrap().remove(&key);}}
            }
            route.lock().unwrap().clear();
        });
        Self{io,session,next:Arc::new(AtomicU64::new(1)),routes,rules:Mutex::new(HashMap::new()),alive,worker}
    }
    pub async fn closed(&self){let mut alive=self.alive.clone();let _=alive.changed().await;}
    pub fn status(&self)->Vec<Status>{self.rules.lock().unwrap().values().map(|r|r.status.lock().unwrap().clone()).collect()}
    pub async fn add_rule(&self,name:impl Into<String>,local_port:u16,target_host:impl Into<String>,target_port:u16)->Result<u64,String>{
        if self.worker.is_finished(){return Err("Forwarding transport closed".into())}
        let host=target_host.into();if host.trim().is_empty() || target_port==0 {return Err("Invalid forwarding target".into())}
        let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,local_port)).await.map_err(|_|"Cannot bind local forwarding port")?;
        let port=listener.local_addr().map_err(|_|"Cannot inspect local forwarding port")?.port();
        let id=self.next.fetch_add(1,Ordering::Relaxed);let status=Arc::new(Mutex::new(Status{id,name:name.into(),local_port:port,streams:0,failures:0}));
        let(stopper,mut stop)=watch::channel(false);let st=status.clone();let io=self.io.clone();let routes=self.routes.clone();let next=self.next.clone();let session=self.session;
        let mut disconnected=self.alive.clone();
        let task=tokio::spawn(async move {
            let mut streams=tokio::task::JoinSet::new();
            loop {tokio::select!{
                _=stop.changed()=>break,
                _=disconnected.changed()=>break,
                _=streams.join_next(),if !streams.is_empty()=>{},
                accepted=listener.accept()=>{
                    let Ok((socket,_))=accepted else{break};
                    if st.lock().unwrap().streams>=128 {continue}
                    let stream=next.fetch_add(1,Ordering::Relaxed);let(tx,rx)=mpsc::channel(64);
                    routes.lock().unwrap().insert((id,stream),tx);st.lock().unwrap().streams+=1;
                    let guard=Route{routes:routes.clone(),key:(id,stream),status:st.clone()};
                    let io=io.clone();let host=host.clone();let mut cancel=stop.clone();let mut transport_closed=disconnected.clone();
                    streams.spawn(async move {
                        let result=tokio::select!{r=forward(&io,session,id,stream,socket,rx,&host,target_port)=>r,_=cancel.changed()=>Ok(()),_=transport_closed.changed()=>Err("Forwarding transport closed".into())};
                        if result.is_err(){guard.status.lock().unwrap().failures+=1;}
                        let _=io.send(FILE,frame(session,id,stream,1,&[])).await;drop(guard);
                    });
                }
            }}
            drop(listener);
            while streams.join_next().await.is_some(){}
        });
        self.rules.lock().unwrap().insert(id,Rule{status,stop:stopper,task});Ok(id)
    }
    pub async fn remove_rule(&self,id:u64)->Result<(),String>{
        let rule=self.rules.lock().unwrap().remove(&id).ok_or("Unknown forwarding rule")?;
        let _=rule.stop.send(true);let mut task=rule.task;
        if tokio::time::timeout(TIMEOUT+Duration::from_secs(1),&mut task).await.is_err(){task.abort();return Err("Forwarding shutdown timed out".into())}Ok(())
    }
}
async fn forward(io:&Transport,session:u64,rule:u64,stream:u64,mut socket:TcpStream,mut rx:mpsc::Receiver<(u64,Vec<u8>)>,host:&str,port:u16)->Result<(),String>{
    let syn=serde_json::json!({"version":1,"target_host":host,"target_port":port}).to_string();
    io.send(FILE,frame(session,rule,stream,0,syn.as_bytes())).await?;
    let ack=tokio::time::timeout(Duration::from_secs(12),rx.recv()).await.map_err(|_|"Forwarding SYN timeout")?.ok_or("Forwarding channel closed")?;
    if ack.0!=3 || serde_json::from_slice::<serde_json::Value>(&ack.1).ok().and_then(|v|v["ok"].as_bool())!=Some(true){return Err("Forwarding target rejected".into())}
    let mut buf=vec![0;16*1024];
    loop {tokio::select!{
        r=socket.read(&mut buf)=>{let n=r.map_err(|_|"Forwarding read failed")?;if n==0{break}io.send(FILE,frame(session,rule,stream,2,&buf[..n])).await?;},
        f=rx.recv()=>match f {Some((2,b))=>if socket.write_all(&b).await.is_err(){break}/* local client hung up: a normal close */,Some((1,_))|None=>break,Some((4,_))=>{},_=>return Err("Unexpected forwarding frame".into())},
    }}let _=socket.shutdown().await;Ok(())
}
