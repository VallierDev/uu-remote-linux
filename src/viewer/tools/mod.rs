//! Tool transports use only this peer's ordered channels; payloads are never logged.
pub mod terminal;
pub mod files;
pub mod portmap;
pub mod cli;
pub mod terminal_ipc;
use std::{sync::{Arc,Mutex},time::Duration};
use tokio::sync::mpsc;
use crate::protocol::{decode_wire,WireValue};
use super::wire;
pub const CONTROL:usize=0;
pub const FILE:usize=3;
pub const BINARY:usize=4;
pub type Packet=(usize,Vec<u8>);
pub type Inbox=Arc<Mutex<Option<mpsc::Sender<Packet>>>>;
pub const TIMEOUT:Duration=Duration::from_secs(30);
#[async_trait::async_trait]
pub trait Send:std::marker::Send+Sync {async fn send(&self,channel:usize,data:Vec<u8>)->Result<(),String>;}
#[derive(Clone)]
pub struct Transport(pub Arc<dyn Send>);
impl Transport {
    pub async fn send(&self,channel:usize,data:Vec<u8>)->Result<(),String> {
        tokio::time::timeout(TIMEOUT,self.0.send(channel,data)).await.map_err(|_|"Tool send timeout")?
    }
}
struct Rtc(Vec<Arc<webrtc::data_channel::RTCDataChannel>>);
#[async_trait::async_trait]
impl Send for Rtc {
    async fn send(&self,channel:usize,data:Vec<u8>)->Result<(),String> {
        let dc=self.0.get(channel).ok_or("Unknown tool channel")?;
        while dc.buffered_amount().await>1024*1024 {tokio::time::sleep(Duration::from_millis(5)).await;}
        dc.send(&bytes::Bytes::from(data)).await.map_err(|_|"Tool data channel send failed")?;Ok(())
    }
}
pub fn attach(peer:&super::rtc::Peer)->Result<(Transport,mpsc::Receiver<Packet>),String> {
    let(tx,rx)=mpsc::channel(256);
    let mut slot=peer.tools.lock().unwrap();
    if slot.is_some(){return Err("Tool transport already attached".into())}
    *slot=Some(tx);Ok((Transport(Arc::new(Rtc(peer.channels.clone()))),rx))
}
pub fn uint(data:&[u8],n:u32)->u64 {decode_wire(data).ok().and_then(|f|f.into_iter().find_map(|f|match(f.number,f.value){(k,WireValue::Varint(v))if k==n=>Some(v),_=>None})).unwrap_or(0)}
pub fn blob(data:&[u8],n:u32)->Option<&[u8]> {decode_wire(data).ok()?.into_iter().find_map(|f|match(f.number,f.value){(k,WireValue::Bytes(v))if k==n=>Some(v),_=>None})}
pub fn text(data:&[u8],n:u32)->Result<String,String> {String::from_utf8(blob(data,n).unwrap_or_default().to_vec()).map_err(|_|"Invalid UTF-8 field".into())}
pub fn ints(fields:&[(u32,u64)])->Vec<u8>{let mut b=Vec::new();for &(n,v) in fields{wire::int(n,v,&mut b)}b}
pub fn nested(n:u32,b:&[u8])->Vec<u8>{let mut out=Vec::new();wire::bytes(n,b,&mut out);out}
pub fn rpc(id:u64,response:bool,case:u32,body:&[u8])->Vec<u8>{
    let mut b=nested(1,&ints(&[(1,id)]));wire::bytes(if response{4}else{8},&nested(case,body),&mut b);
    wire::envelope(id,if response{22}else{21},&b)
}
pub async fn receive(rx:&mut mpsc::Receiver<Packet>)->Result<Packet,String>{tokio::time::timeout(TIMEOUT,rx.recv()).await.map_err(|_|"Tool response timeout")?.ok_or("Tool transport closed".into())}
#[cfg(test)]
pub mod tests;

pub struct Abort<T>(pub tokio::task::JoinHandle<T>);
impl<T> Drop for Abort<T>{fn drop(&mut self){self.0.abort();}}

pub struct Cancel(pub tokio::task::AbortHandle);
impl Drop for Cancel{fn drop(&mut self){self.0.abort();}}
