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
pub fn mac_keycode(scan:i32)->Option<i32>{
    // Inverted SDL Darwin table (release-2.32.10/src/events/scancodes_darwin.h).
    // Live Mac host evidence: Windows VK 49 becomes Space, and VK 8 becomes C.
    Some(match scan{
        4=>0,22=>1,7=>2,9=>3,11=>4,10=>5,29=>6,27=>7,6=>8,25=>9,100=>10,5=>11,
        20=>12,26=>13,8=>14,21=>15,28=>16,23=>17,30=>18,31=>19,32=>20,33=>21,
        35=>22,34=>23,46=>24,38=>25,36=>26,45=>27,37=>28,39=>29,48=>30,18=>31,
        24=>32,47=>33,12=>34,19=>35,40=>36,15=>37,13=>38,52=>39,14=>40,51=>41,
        49=>42,54=>43,56=>44,17=>45,16=>46,55=>47,43=>48,44=>49,53=>50,42=>51,
        41=>53,231=>54,227=>55,225=>56,57=>57,226=>58,224=>59,229=>60,230=>61,228=>62,
        108=>64,99=>65,85=>67,87=>69,83=>71,84=>75,88=>76,86=>78,109=>79,110=>80,
        103=>81,98=>82,89=>83,90=>84,91=>85,92=>86,93=>87,94=>88,95=>89,96=>91,97=>92,
        62=>96,63=>97,64=>98,60=>99,65=>100,66=>101,68=>103,104=>105,107=>106,105=>107,
        70=>105,71=>107,72=>113,
        67=>109,101=>110,69=>111,106=>113,73=>114,74=>115,75=>116,76=>117,61=>118,
        77=>119,59=>120,78=>121,58=>122,80=>123,79=>124,81=>125,82=>126,
        _=>return None,
    })
}
/// The content rectangle inside a decoder canvas padded to a fixed size (ffmpeg's scale+pad).
/// Mirrors `force_original_aspect_ratio=decrease,pad=…:(ow-iw)/2:(oh-ih)/2` so pointer input
/// ignores the padding instead of compressing the valid area.
pub fn content_rect(canvas:(u32,u32),content:(u32,u32))->Option<(f64,f64,f64,f64)> {
    if canvas.0==0||canvas.1==0||content.0==0||content.1==0 {return None}
    let scale=(canvas.0 as f64/content.0 as f64).min(canvas.1 as f64/content.1 as f64);
    let(w,h)=(content.0 as f64*scale,content.1 as f64*scale);
    Some(((canvas.0 as f64-w)/2.,(canvas.1 as f64-h)/2.,w,h))
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
pub struct Input { view_only:bool, mac:bool, keys:BTreeMap<i32,i32>, buttons:BTreeSet<i32> }
pub fn view_only(argument:Option<&str>,environment:Option<&str>)->Result<bool,String> {
    match argument {None=>Ok(environment==Some("1")),Some("--view-only")=>Ok(true),_=>Err("Expected --view-only".into())}
}
impl Input {
    pub fn new(view_only:bool)->Self {Self{view_only,..Default::default()}}
    pub fn set_mac(&mut self,mac:bool){self.mac=mac;}
    pub fn enabled(&self)->bool {!self.view_only}
    pub fn set_view_only(&mut self,view_only:bool){self.view_only=view_only;}
    pub fn key(&mut self,scan:i32,down:bool)->Option<Value> {
        if self.view_only {return None}
        let vk=if self.mac{mac_keycode(scan)}else{windows_vk(scan)}?;
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
    /// SDL wheel units are notches; the wire uses the Mac convention (one notch = 120).
    /// Positive y is wheel-up, matching the host's `mouse_scroll` (button 4).
    pub fn scroll(&mut self,x:f64,y:f64)->Option<Value> {
        if self.view_only {return None}
        let(dx,dy)=((x*120.).round() as i32,(y*120.).round() as i32);
        if dx==0&&dy==0 {return None}
        Some(json!({"action":"mouse_scroll","delta_x":dx,"delta_y":dy}))
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
    #[test] fn mac_target_uses_native_codes_and_releases_same_keys(){
        let mut input=Input::default();input.set_mac(true);
        for (scan,code) in [(30,18),(39,29),(42,51),(44,49),(6,8),(4,0),(40,36),(43,48),
            (80,123),(79,124),(81,125),(82,126),(224,59),(225,56),(226,58),(227,55)]{
            assert_eq!(input.key(scan,true).unwrap()["key"],code);
            assert_eq!(input.key(scan,false).unwrap()["key"],code);
        }
        input.set_mac(false);assert_eq!(input.key(30,true).unwrap()["key"],49);
    }
    #[test] fn mac_focus_release_keeps_zero_keycode_and_modifiers(){
        let mut input=Input::default();input.set_mac(true);
        assert!(input.key(4,true).is_some());assert!(input.key(4,true).is_none());
        input.key(225,true);input.key(227,true);
        let release=input.release_all();
        assert_eq!(release,vec![json!({"action":"kbd_release","key":0}),
            json!({"action":"kbd_release","key":55}),json!({"action":"kbd_release","key":56})]);
        assert!(input.release_all().is_empty());assert!(input.key(4,false).is_none());
        let mut watch=Input::new(true);watch.set_mac(true);assert!(watch.key(4,true).is_none());
    }
    #[test] fn mac_function_key_aliases_release_after_last_keyup(){
        for (pc,function,code) in [(70,104,105),(71,105,107),(72,106,113)]{
            let mut input=Input::default();input.set_mac(true);
            assert_eq!(input.key(pc,true).unwrap()["key"],code);
            assert!(input.key(function,true).is_none());assert!(input.key(pc,false).is_none());
            assert_eq!(input.key(function,false).unwrap()["key"],code);
        }
    }
    #[test] fn watch_switch_is_explicit_and_validated(){
        assert!(!view_only(None,None).unwrap());assert!(!view_only(None,Some("0")).unwrap());
        assert!(view_only(None,Some("1")).unwrap());assert!(view_only(Some("--view-only"),Some("0")).unwrap());
        assert!(view_only(Some("invalid"),None).is_err());
    }
    #[test] fn wheel_notches_use_mac_wire_units(){
        let mut i=Input::default();
        assert_eq!(i.scroll(0.,1.).unwrap(),json!({"action":"mouse_scroll","delta_x":0,"delta_y":120}));
        assert_eq!(i.scroll(0.,-1.).unwrap()["delta_y"],-120);
        assert_eq!(i.scroll(1.,-2.).unwrap(),json!({"action":"mouse_scroll","delta_x":120,"delta_y":-240}));
        assert_eq!(i.scroll(0.,0.1).unwrap()["delta_y"],12); // precise trackpad deltas
        assert!(i.scroll(0.,0.).is_none());
        assert!(Input::new(true).scroll(0.,1.).is_none());
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
    #[test] fn padded_canvas_maps_only_its_content_rectangle(){
        // 16:10 picture inside the default 16:9 canvas: 1728x1080 centred, 96 px pillarbox each side.
        assert_eq!(content_rect((1920,1080),(1280,800)),Some((96.,0.,1728.,1080.)));
        assert_eq!(content_rect((1920,1080),(1920,1080)),Some((0.,0.,1920.,1080.)));
        // 4:3 picture: 1440x1080 centred, 240 px each side.
        assert_eq!(content_rect((1920,1080),(640,480)),Some((240.,0.,1440.,1080.)));
        assert_eq!(content_rect((0,1080),(640,480)),None);
        // The content rectangle's edges are the screen edges, not the canvas edges.
        let (x,y,w,h)=content_rect((1920,1080),(1280,800)).unwrap();
        let v=Viewport::fit((1920,1080),(1280,800));
        assert_eq!((x,y,w,h),(v.x,v.y,v.width,v.height));
        let abs=|px:f64|v.motion(px,540.).unwrap();
        assert_eq!(abs(96.)["abs_x"],0.0);
        assert_eq!(abs(960.)["abs_x"],0.5);
        assert_eq!(abs(1824.)["abs_x"],1.0);
        assert_eq!(abs(0.)["abs_x"],0.0); // padding is clamped to the screen edge
    }
}
