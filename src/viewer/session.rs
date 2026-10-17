use super::{api::JoinData,wire::{self,Shape},signal::Signal,rtc::{self,Peer,ReadyPolicy}};
use serde_json::Value;
use std::{sync::atomic::Ordering,time::Duration};
use tokio::sync::mpsc;
use webrtc::api::setting_engine::SettingEngine;

pub struct Options {pub timeout:Duration,pub legacy_prefix:bool,pub ready_policy:ReadyPolicy,pub clipboard:super::clipboard::Bridge}
pub struct Session {pub peer:Peer,signal:Signal,candidates:mpsc::Receiver<Value>,policy:ReadyPolicy}
impl Session {
    pub async fn open(room:&JoinData,shape:&Shape,device:&str,client:&str,options:Options,settings:SettingEngine)->Result<Self,String> {
        shape.validate_session()?;
        let app=wire::uuid();let ice=hex::encode(rand::random::<[u8;16]>());
        let timeout=if room.ws_connect_timeout_ms>0 {Duration::from_millis(room.ws_connect_timeout_ms as u64)}else{options.timeout};
        let mut signal=Signal::connect(&room.signaling_list,&room.token,timeout,options.legacy_prefix).await?;
        let result=async {
            let result=signal.control(shape.control(client,&app,&ice)?,shape.connect_options(device,options.clipboard.enabled)?,options.timeout).await?;
            // Key names per ICE entry and URL scheme only (no values) to see where TURN credentials live.
            if let Some(list)=result.get("iceServers").and_then(Value::as_array) {
                let summary:Vec<String>=list.iter().map(|s|{
                    let scheme=s.get("urls").and_then(|u|u.as_str().map(str::to_owned).or_else(||u.get(0).and_then(Value::as_str).map(str::to_owned))).unwrap_or_default();
                    let keys:Vec<&str>=s.as_object().map(|o|o.keys().map(String::as_str).collect()).unwrap_or_default();
                    format!("{}:{:?}",scheme.split(':').next().unwrap_or(""),keys)}).collect();
                log::info!("Control ack ICE entries: {:?}",summary);
            }
            // Structure and relations only; never the ids or ICE credentials.
            log::info!("Control ack shape={} app_control_id_matches={:?} ice_id_matches={:?}",crate::protocol::json_shape(&result,5),
                result.get("app_control_id").map(|v|v.as_str()==Some(app.as_str())),result.get("ice_id").map(|v|v.as_str()==Some(ice.as_str())));
            if result.get("app_control_id").is_some_and(|v|v.as_str()!=Some(app.as_str())) {return Err("Control acknowledgement belongs to another session".into())}
            // Observed live: the cloud assigns the session ice_id in this ack rather than echoing ours.
            let ice=result.get("ice_id").and_then(Value::as_str).filter(|s|!s.is_empty()&&s.len()<=128).map(str::to_owned).unwrap_or(ice);
            let remote=result.get("client_id").and_then(Value::as_str).filter(|s|!s.is_empty()).ok_or("Control acknowledgement missing peer route")?;
            let config=rtc::configuration(&result)?;
            let(tx,rx)=mpsc::channel(256);
            let peer=Peer::new(config,shape,&app,&ice,remote,tx,settings,options.clipboard).await?;
            // The official host drops an offer whose ice_id is not yet in its connection table
            // (libstreamer 0xc3080); give it time to process be-controlled first.
            let delay=std::env::var("UU_VIEWER_OFFER_DELAY_MS").ok().and_then(|v|v.parse().ok()).unwrap_or(0u64).min(10_000);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            match peer.offer().await {
                Ok(offer)=>match signal.soac(offer).await {Ok(())=>Ok((peer,rx)),Err(e)=>{peer.close().await;Err(e)}},
                Err(e)=>{peer.close().await;Err(e)},
            }
        }.await;
        match result {
            Ok((peer,candidates))=>Ok(Self{peer,signal,candidates,policy:options.ready_policy}),
            Err(e)=>{signal.close().await;Err(e)},
        }
    }
    pub async fn step(&mut self)->Result<(),String> {
        tokio::select! {
            candidate=self.candidates.recv()=>if let Some(candidate)=candidate {self.signal.soac(candidate).await?},
            event=self.signal.event()=> {
                let event=event?;
                // Event name and SOAC type only.
                log::info!("Viewer signaling event: name={} soac_type={}",event.get(0).and_then(Value::as_str).unwrap_or("-"),
                    event.get(1).and_then(|b|b.get("type").or_else(||b.get("data").and_then(|d|d.get("type")))).and_then(Value::as_str).unwrap_or("-"));
                match event.get(0).and_then(Value::as_str) {
                    Some("soac")=>if let Some(body)=event.get(1) {self.peer.receive(body).await?},
                    Some("released"|"left"|"leave")=> {
                        // An event for another peer must not tear down this session.
                        if self.peer.matches_route(event.get(1).unwrap_or(&Value::Null)) {return Err("Remote session released".into())}
                    }
                    _=>{},
                }
            }
        }
        Ok(())
    }
    pub async fn run(&mut self)->Result<(),String> {
        let mut interval=tokio::time::interval(Duration::from_secs(1));
        let mut reported=(false,false,false);
        let deadline=tokio::time::sleep(Duration::from_secs(30));tokio::pin!(deadline);
        loop {
            tokio::select! {
                _=tokio::signal::ctrl_c()=>return Ok(()),
                result=self.step()=>result?,
                _=&mut deadline,if !self.peer.observed.echo_received.load(Ordering::Acquire)=>return Err("CONTROL echo timed out".into()),
                _=interval.tick()=> {
                    if self.peer.observed.control_open.load(Ordering::Acquire) {self.peer.send_input(&super::input::Input::heartbeat()).await?;}
                    let state=(self.peer.observed.control_open.load(Ordering::Acquire),self.peer.observed.echo_received.load(Ordering::Acquire),self.peer.ready(self.policy,false));
                    if state!=reported {log::info!("Viewer session: control_open={} echo_received={} configured_ready={} control_messages={} media_packets={}",state.0,state.1,state.2,self.peer.observed.control_messages.load(Ordering::Relaxed),self.peer.observed.media_packets.load(Ordering::Relaxed));reported=state;}
                }
            }
        }
    }
    pub async fn close(&mut self) {self.peer.close().await;self.signal.close().await;}
}
