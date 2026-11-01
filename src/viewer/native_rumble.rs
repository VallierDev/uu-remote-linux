//! Official Windows 4.45: gpd_vibration {index:u8,left_motor:u8,right_motor:u8}.
//! Server RVAs 0x5df5d0/0x5dfe80; viewer 0x10331a0. See protocol evidence doc.
use serde::Deserialize;
use std::{sync::{Arc,Mutex},time::{Duration,Instant}};

#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub struct Motors {pub left:u16,pub right:u16}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {action:String,index:u8,left_motor:u8,right_motor:u8}
pub fn decode(data:&[u8])->Option<Motors>{
    if data.len()>512 {return None}
    let m:Message=serde_json::from_slice(data).ok()?;
    if m.action!="gpd_vibration" || m.index!=0 {return None}
    Some(Motors{left:u16::from(m.left_motor)*257,right:u16::from(m.right_motor)*257})
}

#[derive(Default)]
struct Slot {active:bool,latest:Option<(Motors,Instant)>}
/// One slot per peer: bounded memory, latest update wins (including zero/stop).
/// Disabled while unfocused/unarmed; never replay feedback accumulated during pause.
#[derive(Clone,Default)]
pub struct Feedback(Arc<Mutex<Slot>>);
impl Feedback {
    pub fn set_active(&self,active:bool){
        let mut slot=self.0.lock().unwrap();
        if slot.active!=active {slot.latest=None;slot.active=active;}
    }
    pub fn receive(&self,data:&[u8]){
        let mut slot=self.0.lock().unwrap();
        if slot.active {if let Some(m)=decode(data){slot.latest=Some((m,Instant::now()));}}
    }
    pub fn take(&self)->Option<(Motors,Instant)>{self.0.lock().unwrap().latest.take()}
}

/// Renew finite SDL effects while active; if the process stalls/dies, the hardware
/// effect expires. UU sends state changes, so a quiet network is NOT a stop message.
pub const EFFECT_MS:u32=1000;
const REFRESH:Duration=Duration::from_millis(250);
#[derive(Default)]
pub struct Playback {desired:Motors,next:Option<Instant>}
impl Playback {
    pub fn step(&mut self,incoming:Option<(Motors,Instant)>,active:bool,now:Instant)->Option<Motors>{
        if !active {return self.stop()}
        let previous=self.desired;
        if let Some((m,received))=incoming {
            // Do not play a pulse left pending while decoding or scheduling stalled.
            if m==Motors::default() || now.saturating_duration_since(received)<=Duration::from_millis(500){self.desired=m;}
        }
        let running=self.desired!=Motors::default();
        if previous!=self.desired || (running&&self.next.is_none_or(|t|now>=t)) {
            self.next=running.then_some(now+REFRESH);return Some(self.desired)
        }
        None
    }
    pub fn stop(&mut self)->Option<Motors>{
        let running=self.desired!=Motors::default();*self=Self::default();
        running.then_some(Motors::default())
    }
}

#[cfg(test)] mod tests {
    use super::*;
    fn data(left:u8,right:u8)->Vec<u8>{format!(r#"{{"action":"gpd_vibration","index":0,"left_motor":{left},"right_motor":{right}}}"#).into_bytes()}
    #[test] fn official_bytes_scale_to_full_sdl_range(){
        assert_eq!(decode(&data(255,128)),Some(Motors{left:65535,right:32896}));
        assert_eq!(decode(&data(0,0)),Some(Motors::default()));
    }
    #[test] fn reject_other_slots_malformed_ranges_duplicates_and_other_actions(){
        let good=String::from_utf8(data(1,2)).unwrap();
        for bad in [good.replace("\"index\":0","\"index\":1"),good.replace("vibration","update"),
            good.replace("\"left_motor\":1","\"left_motor\":256"),good.replace("\"left_motor\":1","\"left_motor\":-1"),
            good.replace("\"left_motor\":1","\"left_motor\":1.5"),good.replace("\"index\":0","\"index\":0,\"index\":0"),
            good.replace(",\"right_motor\":2",""),"{}".into()," ".repeat(513)] {assert_eq!(decode(bad.as_bytes()),None);}
    }
    #[test] fn peer_isolation_pause_and_latest_stop_wins(){
        let a=Feedback::default();let b=Feedback::default();
        a.receive(&data(255,0));assert!(a.take().is_none());
        a.set_active(true);b.set_active(true);
        for _ in 0..1000 {a.receive(&data(255,255));}a.receive(&data(0,0));
        assert_eq!(a.take().unwrap().0,Motors::default());assert!(b.take().is_none());
        a.receive(&data(255,0));a.set_active(false);a.set_active(true);assert!(a.take().is_none());
    }
    #[test] fn sustained_effect_refresh_stop_focus_loss_and_stale_pulse(){
        let t=Instant::now();let mut p=Playback::default();let m=Motors{left:100,right:200};
        assert_eq!(p.step(Some((m,t)),true,t),Some(m));
        assert_eq!(p.step(None,true,t+Duration::from_millis(249)),None);
        assert_eq!(p.step(None,true,t+REFRESH),Some(m));
        assert_eq!(p.step(Some((Motors::default(),t)),true,t+REFRESH),Some(Motors::default()));
        assert_eq!(p.step(Some((m,t)),true,t),Some(m));
        assert_eq!(p.step(None,false,t),Some(Motors::default()));
        assert_eq!(p.step(None,true,t),None);
        assert_eq!(p.step(Some((m,t)),true,t+Duration::from_millis(501)),None);
        p.step(Some((m,t)),true,t);
        assert_eq!(p.step(Some((Motors::default(),t)),true,t+Duration::from_secs(5)),Some(Motors::default()));
    }
}
