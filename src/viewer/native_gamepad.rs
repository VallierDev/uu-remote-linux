//! Opt-in official UU gamepad JSON, recovered from Windows 4.45.0.4318.
//! Evidence/RVAs and limits: docs/deckpad-native-gamepad.md. No port mapping or helper.
use serde_json::{json,Value};
use std::time::{Duration,Instant};

#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub struct State {pub buttons:u16,pub lx:i16,pub ly:i16,pub rx:i16,pub ry:i16,pub lt:u8,pub rt:u8}
impl State {
    pub fn json(self)->Value{json!({"action":"gpd_update","index":0,"buttons":self.buttons,
        "left_trigger":self.lt,"right_trigger":self.rt,"left_thumb_x":self.lx,
        "left_thumb_y":self.ly,"right_thumb_x":self.rx,"right_thumb_y":self.ry})}
    fn released(self)->bool{self.buttons==0 && self.lt<8 && self.rt<8 &&
        [self.lx,self.ly,self.rx,self.ry].iter().all(|v|i32::from(*v).abs()<5000)}
}
pub fn enabled()->bool{std::env::var("UU_VIEWER_NATIVE_GAMEPAD").as_deref()==Ok("1")}
pub fn connect()->Value{json!({"action":"gpd_connect","index":0})}
pub fn disconnect()->Value{json!({"action":"gpd_disconnect","index":0})}
fn y_axis(v:i16)->i16{(-i32::from(v)).clamp(-32768,32767) as i16}
fn trigger(v:i16)->u8{((i32::from(v).max(0)*255+16383)/32767) as u8}

/// Local state machine: focus loss sends neutral then detach; re-entry requires release.
/// The caller stops the session on any send failure rather than queueing old states.
#[derive(Default)]
struct Gate {connected:bool,armed:bool,last:Option<(State,Instant)>}
impl Gate {
    fn close(&mut self)->Vec<Value>{
        let out=if self.connected{vec![State::default().json(),disconnect()]}else{vec![]};
        *self=Self::default();out
    }
    fn step(&mut self,state:Option<State>,active:bool,now:Instant)->Vec<Value>{
        let Some(state)=state.filter(|_|active) else{return self.close()};
        let mut out=Vec::new();
        if !self.connected{out.push(connect());out.push(State::default().json());self.connected=true;}
        if !self.armed {if state.released(){self.armed=true;}else{return out}}
        if self.last.is_none_or(|(s,t)|s!=state||now.duration_since(t)>=Duration::from_millis(100)){
            out.push(state.json());self.last=Some((state,now));
        }
        out
    }
}

#[cfg(feature="viewer-sdl")]
pub struct NativeGamepad {sub:sdl2::GameControllerSubsystem,pad:Option<sdl2::controller::GameController>,gate:Gate,next_scan:Instant,paused:bool,chord:bool,
    feedback:super::native_rumble::Feedback,rumble:super::native_rumble::Playback,rumble_enabled:bool,rumble_failed:bool,received_once:bool,applied_once:bool}
#[cfg(feature="viewer-sdl")]
impl NativeGamepad {
    pub fn new(sdl:&sdl2::Sdl,feedback:super::native_rumble::Feedback)->Result<Self,String>{
        let sub=sdl.game_controller()?;
        eprintln!("DECKPAD_NATIVE_ENABLED");
        let rumble_enabled=std::env::var("UU_VIEWER_NATIVE_RUMBLE").as_deref()!=Ok("0");
        if rumble_enabled {eprintln!("DECKPAD_RUMBLE_ENABLED");}
        Ok(Self{sub,pad:None,gate:Gate::default(),next_scan:Instant::now(),paused:false,chord:false,
            feedback,rumble:Default::default(),rumble_enabled,rumble_failed:false,received_once:false,applied_once:false})
    }
    pub fn poll(&mut self,active:bool)->Vec<Value>{
        use sdl2::controller::{Axis as A,Button as B};
        let now=Instant::now();
        if self.pad.as_ref().is_some_and(|p|!p.attached()) {
            self.stop_feedback();
            self.pad=None;self.chord=false;
            // Detach before discovery, even if a replacement is already available.
            return self.gate.close();
        }
        if self.pad.is_none() && now>=self.next_scan {
            self.next_scan=now+Duration::from_secs(1);
            if let Ok(n)=self.sub.num_joysticks(){
                let ids:Vec<_>=(0..n).filter(|&i|self.sub.is_game_controller(i)).collect();
                if ids.len()==1 {self.pad=self.sub.open(ids[0]).ok();if let Some(pad)=self.pad.as_ref(){
                    self.rumble_failed=false;self.received_once=false;self.applied_once=false;
                    eprintln!("DECKPAD_NATIVE_CONTROLLER_OPEN");
                    if self.rumble_enabled {eprintln!("{}",if pad.has_rumble(){"DECKPAD_RUMBLE_SUPPORTED"}else{"DECKPAD_RUMBLE_UNAVAILABLE"});}
                }}
            }
        }
        let state=self.pad.as_ref().map(|p|{
            let mut buttons=0;
            for (b,mask) in [(B::A,0x1000),(B::B,0x2000),(B::X,0x4000),(B::Y,0x8000),
                (B::Back,0x20),(B::Start,0x10),(B::LeftStick,0x40),(B::RightStick,0x80),
                (B::LeftShoulder,0x100),(B::RightShoulder,0x200),(B::DPadUp,1),(B::DPadDown,2),(B::DPadLeft,4),(B::DPadRight,8)] {
                if p.button(b){buttons|=mask;}
            }
            State{buttons,lx:p.axis(A::LeftX),ly:y_axis(p.axis(A::LeftY)),rx:p.axis(A::RightX),ry:y_axis(p.axis(A::RightY)),lt:trigger(p.axis(A::TriggerLeft)),rt:trigger(p.axis(A::TriggerRight))}
        });
        let chord=state.is_some_and(|s|s.buttons&0x30==0x30);
        if active && chord && !self.chord {self.paused=!self.paused;eprintln!("{}",if self.paused{"DECKPAD_NATIVE_PAUSED"}else{"DECKPAD_NATIVE_RESUMING"});}
        self.chord=chord;
        let connected=self.gate.connected;
        let out=self.gate.step(state,active&&!self.paused&&!chord,now);
        if self.gate.connected&&!connected {eprintln!("DECKPAD_NATIVE_CONNECT_QUEUED");}
        if !self.gate.connected&&connected {eprintln!("DECKPAD_NATIVE_RELEASE_QUEUED");}
        let feedback_active=self.gate.connected&&self.gate.armed&&self.rumble_enabled&&!self.rumble_failed;
        self.feedback.set_active(feedback_active);
        let incoming=self.feedback.take();
        if incoming.is_some()&&!self.received_once {self.received_once=true;eprintln!("DECKPAD_RUMBLE_RECEIVED");}
        if let Some(m)=self.rumble.step(incoming,feedback_active,now){
            if let Some(pad)=self.pad.as_mut(){
                if pad.set_rumble(m.left,m.right,super::native_rumble::EFFECT_MS).is_err(){
                    self.rumble_failed=true;self.feedback.set_active(false);self.rumble.stop();
                    let _=pad.set_rumble(0,0,0);eprintln!("DECKPAD_RUMBLE_OUTPUT_FAILED");
                }else if m!=super::native_rumble::Motors::default()&&!self.applied_once{
                    self.applied_once=true;eprintln!("DECKPAD_RUMBLE_APPLIED");
                }
            }
        }
        out
    }
    fn stop_feedback(&mut self){
        self.feedback.set_active(false);self.rumble.stop();
        if let Some(pad)=self.pad.as_mut(){let _=pad.set_rumble(0,0,0);}
    }
    pub fn close(&mut self)->Vec<Value>{self.stop_feedback();self.gate.close()}
}
#[cfg(feature="viewer-sdl")]
impl Drop for NativeGamepad {fn drop(&mut self){self.stop_feedback();}}

#[cfg(test)] mod tests{
    use super::*;
    #[test] fn official_shape_and_axis_boundaries(){
        assert_eq!(connect(),json!({"action":"gpd_connect","index":0}));
        let s=State{buttons:0x1000,lx:-32768,ly:32767,rx:100,ry:-100,lt:128,rt:255}.json();
        assert_eq!(s,json!({"action":"gpd_update","index":0,"buttons":4096,"left_trigger":128,"right_trigger":255,"left_thumb_x":-32768,"left_thumb_y":32767,"right_thumb_x":100,"right_thumb_y":-100}));
        assert_eq!((y_axis(-32768),y_axis(32767)),(32767,-32767));
        assert_eq!([trigger(-32768),trigger(0),trigger(16384),trigger(32767)],[0,0,128,255]);
    }
    #[test] fn held_on_entry_waits_for_release_then_focus_loss_detaches(){
        let mut g=Gate::default();let t=Instant::now();let held=State{buttons:0x1000,..Default::default()};
        assert_eq!(g.step(Some(held),true,t),vec![connect(),State::default().json()]);
        assert!(g.step(Some(held),true,t).is_empty());
        assert_eq!(g.step(Some(State::default()),true,t),vec![State::default().json()]);
        assert_eq!(g.step(Some(held),true,t),vec![held.json()]);
        assert_eq!(g.step(Some(held),false,t),vec![State::default().json(),disconnect()]);
        assert!(g.close().is_empty());
        assert_eq!(g.step(Some(held),true,t),vec![connect(),State::default().json()]);
    }
    #[test] fn unplug_neutral_and_idle_refresh(){
        let mut g=Gate::default();let t=Instant::now();let z=State::default();g.step(Some(z),true,t);
        assert!(g.step(Some(z),true,t+Duration::from_millis(50)).is_empty());
        assert_eq!(g.step(Some(z),true,t+Duration::from_millis(100)),vec![z.json()]);
        assert_eq!(g.step(None,true,t+Duration::from_millis(101)),vec![z.json(),disconnect()]);
    }
}
