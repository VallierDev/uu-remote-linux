//! Framing/ack evidence: internal viewer-evidence-20261016.md section 3.
//! Raw Engine.IO4 attachments and legacy SDK 0x04 framing are explicit alternatives.
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use std::{collections::VecDeque,time::Duration};

pub struct Signal {
    ws: crate::ws_transport::Socket,
    next_ack:u64,
    pending:VecDeque<Value>,
    legacy_prefix:bool,
}
impl Signal {
    pub async fn connect(urls:&[String], token:&str, timeout:Duration, legacy_prefix:bool)->Result<Self,String> {
        for url in urls {
            if let Ok(Ok(signal))=tokio::time::timeout(timeout,Self::one(url,token,legacy_prefix,timeout)).await {return Ok(signal)}
        }
        Err("Signaling connection failed (details omitted)".into())
    }
    async fn one(url:&str,token:&str,legacy_prefix:bool,timeout:Duration)->Result<Self,String> {
        let mut request=format!("{}/socket.io/?transport=websocket&EIO=4",url.trim_end_matches('/')).into_client_request().map_err(|_|"Invalid signaling endpoint")?;
        request.headers_mut().insert("X-NRD-AUTH",token.parse().map_err(|_|"Invalid room authentication header")?);
        let ws=crate::ws_transport::connect(request,timeout).await?;
        let mut s=Self{ws,next_ack:10,pending:VecDeque::new(),legacy_prefix};
        let mut opened=false;
        loop {
            match s.ws.next().await {
                Some(Ok(Message::Text(t))) if t.starts_with('0') && !opened => {
                    serde_json::from_str::<Value>(&t[1..]).map_err(|_|"Invalid Engine.IO open")?;
                    opened=true;s.send(Message::Text("40".into())).await?;
                }
                Some(Ok(Message::Text(t))) if t.starts_with("40") && opened =>return Ok(s),
                Some(Ok(Message::Text(t))) if t.starts_with('2')=>s.send(Message::Text(format!("3{}",&t[1..]))).await?,
                Some(Ok(Message::Ping(b)))=>s.send(Message::Pong(b)).await?,
                _=>return Err("Signaling handshake failed".into()),
            }
        }
    }
    async fn send(&mut self,message:Message)->Result<(),String> {self.ws.send(message).await.map_err(|_|"Signaling send failed".into())}
    pub async fn control(&mut self, header:Value, app_data:Vec<u8>, timeout:Duration)->Result<Value,String> {
        self.next_ack+=1;let id=self.next_ack;
        self.send(Message::Text(format!("451-{id}{}",serde_json::json!(["control",header])))).await?;
        let mut attachment=Vec::new();if self.legacy_prefix {attachment.push(4)} attachment.extend(app_data);
        self.send(Message::Binary(attachment)).await?;
        tokio::time::timeout(timeout,async {
            loop {
                match self.packet().await? {
                    Packet::Ack(ack,value) if ack==id=> {
                        let a=value.as_array().ok_or("Invalid control acknowledgement")?;
                        if a.len()!=2 || a[0].as_str()!=Some("success") || !a[1].is_object() {return Err("Control refused or malformed acknowledgement".into())}
                        return Ok(a[1].clone());
                    }
                    Packet::Event(value)=> {
                        if self.pending.len()>=64 {return Err("Too many pre-control signaling events".into())}
                        self.pending.push_back(value);
                    }
                    _=>{},
                }
            }
        }).await.map_err(|_|"Control acknowledgement timed out")?
    }
    async fn packet(&mut self)->Result<Packet,String> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(t))) if t.starts_with('2')=>self.send(Message::Text(format!("3{}",&t[1..]))).await?,
                Some(Ok(Message::Ping(b)))=>self.send(Message::Pong(b)).await?,
                Some(Ok(Message::Text(t))) if t.starts_with("43")=> {
                    let begin=t.find('[').ok_or("Malformed signaling acknowledgement")?;
                    let id=t[2..begin].parse().map_err(|_|"Malformed signaling acknowledgement id")?;
                    let value=serde_json::from_str(&t[begin..]).map_err(|_|"Malformed signaling acknowledgement body")?;
                    return Ok(Packet::Ack(id,value))
                }
                Some(Ok(Message::Text(t))) if t.starts_with("42")=> {
                    let begin=t.find('[').ok_or("Malformed signaling event")?;
                    let value=serde_json::from_str(&t[begin..]).map_err(|_|"Malformed signaling event body")?;
                    return Ok(Packet::Event(value))
                }
                // Socket.IO binary event (45N-[...] / 46N-[...] ack) followed by N attachment frames.
                // The official host sends its answer this way (binary gzip_sdp); attachments are
                // substituted as base64 strings so the existing gzip_sdp decoder applies.
                Some(Ok(Message::Text(t))) if t.starts_with("45") || t.starts_with("46")=> {
                    let dash=t.find('-').ok_or("Malformed binary signaling packet")?;
                    let count:usize=t[2..dash].parse().map_err(|_|"Malformed binary attachment count")?;
                    if count>8 {return Err("Too many signaling attachments".into())}
                    let begin=t.find('[').ok_or("Malformed binary signaling packet")?;
                    let mut value:serde_json::Value=serde_json::from_str(&t[begin..]).map_err(|_|"Malformed binary signaling body")?;
                    let mut attachments=Vec::with_capacity(count);
                    while attachments.len()<count {
                        match self.ws.next().await {
                            Some(Ok(Message::Binary(mut b)))=>{
                                if b.len()>2 && b[0]==4 && b[1]==0x1f {b.remove(0);}
                                if b.len()>4*1024*1024 {return Err("Signaling attachment too large".into())}
                                attachments.push(base64::engine::general_purpose::STANDARD.encode(&b));
                            }
                            Some(Ok(Message::Text(t))) if t.starts_with('2')=>self.send(Message::Text(format!("3{}",&t[1..]))).await?,
                            Some(Ok(_))=>{},
                            _=>return Err("Signaling disconnected".into()),
                        }
                    }
                    fn fill(v:&mut serde_json::Value,a:&[String]) {
                        match v {
                            serde_json::Value::Object(o) if o.get("_placeholder")==Some(&serde_json::Value::Bool(true))=>{
                                if let Some(s)=o.get("num").and_then(|n|n.as_u64()).and_then(|n|a.get(n as usize)) {*v=serde_json::Value::String(s.clone());}
                            }
                            serde_json::Value::Object(o)=>for x in o.values_mut(){fill(x,a)},
                            serde_json::Value::Array(l)=>for x in l{fill(x,a)},
                            _=>{},
                        }
                    }
                    fill(&mut value,&attachments);
                    log::info!("Viewer binary signaling event: name={} attachments={}",value.get(0).and_then(|v|v.as_str()).unwrap_or("-"),count);
                    if t.starts_with("46") {
                        let id=t[dash+1..begin].parse().map_err(|_|"Malformed binary acknowledgement id")?;
                        return Ok(Packet::Ack(id,value))
                    }
                    return Ok(Packet::Event(value))
                }
                Some(Ok(Message::Text(t))) if t=="41" || t=="1"=>return Err("Signaling room closed".into()),
                Some(Ok(Message::Close(_)))|None|Some(Err(_))=>return Err("Signaling disconnected".into()),
                // Type, short prefix and length only, to see what else the room delivers.
                Some(Ok(Message::Text(t)))=>log::info!("Viewer signaling frame ignored: text prefix={} bytes={}",t.chars().take(3).collect::<String>(),t.len()),
                Some(Ok(Message::Binary(b)))=>log::info!("Viewer signaling frame ignored: binary bytes={}",b.len()),
                _=>{},
            }
        }
    }
    pub async fn event(&mut self)->Result<Value,String> {
        if let Some(v)=self.pending.pop_front(){return Ok(v)}
        loop {if let Packet::Event(v)=self.packet().await?{return Ok(v)}}
    }
    pub async fn soac(&mut self, value:Value)->Result<(),String> {self.send(Message::Text(format!("42{}",serde_json::json!(["soac",value])))).await}
    pub async fn close(&mut self) {let _=self.ws.close(None).await;}
}
enum Packet { Ack(u64,Value), Event(Value) }
