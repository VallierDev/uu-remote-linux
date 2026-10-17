//! SDL scancodes are USB HID usages; translate physical keys, not typed text.
use serde_json::{json, Value};
use std::collections::{BTreeMap,BTreeSet};
pub fn windows_vk(scan: i32) -> Option<i32> {
    Some(match scan {
        4..=29 => 0x41 + scan - 4, 30..=38 => 0x31 + scan - 30, 39 => 0x30,
        40=>0x0d,41=>0x1b,42=>8,43=>9,44=>0x20,
        45=>0xbd,46=>0xbb,47=>0xdb,48=>0xdd,49=>0xdc,51=>0xba,52=>0xde,53=>0xc0,54=>0xbc,55=>0xbe,56=>0xbf,
        57=>0x14,58..=69=>0x70+scan-58,70=>0x2c,71=>0x91,72=>0x13,
        73=>0x2d,74=>0x24,75=>0x21,76=>0x2e,77=>0x23,78=>0x22,
        79=>0x27,80=>0x25,81=>0x28,82=>0x26,83=>0x90,
        84=>0x6f,85=>0x6a,86=>0x6d,87=>0x6b,88=>0x0d,
        89..=97=>0x61+scan-89,98=>0x60,99=>0x6e,101=>0x5d,
        104..=115=>0x7c+scan-104,
        224=>0xa2,225=>0xa0,226=>0xa4,227=>0x5b,228=>0xa3,229=>0xa1,230=>0xa5,231=>0x5c,
        _=>return None,
    })
}
#[derive(Clone,Copy,Debug,PartialEq)]
pub struct Viewport { pub x:f64, pub y:f64, pub width:f64, pub height:f64 }
impl Viewport {
    pub fn fit(window:(u32,u32), frame:(u32,u32))->Self {
        let scale=(window.0 as f64 / frame.0.max(1) as f64).min(window.1 as f64 / frame.1.max(1) as f64);
        let (w,h)=(frame.0 as f64*scale,frame.1 as f64*scale);
        Self{x:(window.0 as f64-w)/2.,y:(window.1 as f64-h)/2.,width:w,height:h}
    }
    pub fn motion(self,x:f64,y:f64)->Option<Value> {
        if self.width<=0. || self.height<=0. || !x.is_finite() || !y.is_finite() {return None}
        Some(json!({"action":"mouse_move_absolute","abs_x":((x-self.x)/self.width).clamp(0.,1.),"abs_y":((y-self.y)/self.height).clamp(0.,1.)}))
    }
}
#[derive(Default)]
pub struct Input { view_only:bool, keys:BTreeMap<i32,i32>, buttons:BTreeSet<i32> }
pub fn view_only(argument:Option<&str>,environment:Option<&str>)->Result<bool,String> {
    match argument {None=>Ok(environment==Some("1")),Some("--view-only")=>Ok(true),_=>Err("Expected --view-only".into())}
}
impl Input {
    pub fn new(view_only:bool)->Self {Self{view_only,..Default::default()}}
    pub fn enabled(&self)->bool {!self.view_only}
    pub fn key(&mut self,scan:i32,down:bool)->Option<Value> {
        if self.view_only {return None}
        let vk=windows_vk(scan)?;
        let changed=if down {
            if self.keys.contains_key(&scan) {return None}
            let already=self.keys.values().any(|v|*v==vk);self.keys.insert(scan,vk);!already
        }else{
            if self.keys.remove(&scan).is_none(){return None}
            !self.keys.values().any(|v|*v==vk)
        };
        changed.then(||json!({"action":if down{"kbd_press"}else{"kbd_release"},"key":vk}))
    }
    pub fn button(&mut self,button:i32,down:bool)->Option<Value> {
        if self.view_only {return None}
        if !matches!(button,1|2|4) {return None}
        let changed=if down{self.buttons.insert(button)}else{self.buttons.remove(&button)};
        changed.then(||json!({"action":if down{"mouse_press"}else{"mouse_release"},"button":button}))
    }
    pub fn release_all(&mut self)->Vec<Value> {
        let mut out=Vec::new();
        for key in std::mem::take(&mut self.keys).into_values().collect::<BTreeSet<_>>() {out.push(json!({"action":"kbd_release","key":key}));}
        for button in std::mem::take(&mut self.buttons) {out.push(json!({"action":"mouse_release","button":button}));}
        out
    }
    pub fn heartbeat()->Value {json!({"heartbeat":"kbd_heartbeat"})}
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn watch_switch_is_explicit_and_validated(){
        assert!(!view_only(None,None).unwrap());assert!(!view_only(None,Some("0")).unwrap());
        assert!(view_only(None,Some("1")).unwrap());assert!(view_only(Some("--view-only"),Some("0")).unwrap());
        assert!(view_only(Some("invalid"),None).is_err());
    }
    #[test] fn mappings_encoding_and_focus() {
        for (scan,vk) in [(4,65),(29,90),(39,48),(58,112),(80,37),(224,162),(230,165),(52,222)] {assert_eq!(windows_vk(scan),Some(vk));}
        assert_eq!(windows_vk(100),None);
        let mut i=Input::default();assert_eq!(i.key(4,true).unwrap(),json!({"action":"kbd_press","key":65}));assert!(i.key(4,true).is_none());
        i.key(225,true);i.button(2,true);let release=i.release_all();assert_eq!(release.len(),3);
        assert!(release.iter().all(|v|v["action"].as_str().unwrap().ends_with("release")));
assert!(i.release_all().is_empty());
        assert_eq!(Input::heartbeat(),json!({"heartbeat":"kbd_heartbeat"}));
    }
    #[test] fn aliased_physical_keys_release_only_after_last_keyup() {
        let mut i=Input::default();assert!(i.key(40,true).is_some());assert!(i.key(88,true).is_none());
        assert!(i.key(40,false).is_none());assert_eq!(i.key(88,false).unwrap(),json!({"action":"kbd_release","key":13}));
        assert!(i.button(3,true).is_none());assert_eq!(i.button(4,true).unwrap()["button"],4);
    }
    #[test] fn letterbox() {
        let v=Viewport::fit((1000,1000),(1920,1080));
        assert_eq!(v.motion(500.,500.).unwrap()["abs_y"],0.5);
        assert_eq!(v.motion(-10.,0.).unwrap()["abs_y"],0.0);
        assert_eq!(v.motion(1010.,1000.).unwrap()["abs_x"],1.0);
        assert!(v.motion(f64::NAN,0.).is_none());
    }
}
