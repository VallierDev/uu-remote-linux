//! Input delayed by one image-paste transaction. Physical modifier snapshots
//! are independent of the remote Input state, which an atomic Cmd+V can reset.
use sdl2::{event::Event,keyboard::{Mod,Scancode}};

pub(super) struct InputEvent {pub event:Event,pub modifiers:Option<Mod>}
impl From<Event> for InputEvent {fn from(event:Event)->Self{Self{event,modifiers:None}}}

pub(super) struct Modifiers(Mod);
impl Default for Modifiers {fn default()->Self{Self(Mod::NOMOD)}}
impl Modifiers {
    pub fn observe(&mut self,event:&Event)->Option<Mod> {
        // SDL wheels have no keymod field. Derive their state from the ordered
        // keyboard stream, never from SDL's final state after polling a batch.
        if let Event::KeyDown{keymod,..}|Event::KeyUp{keymod,..}=event{self.0=*keymod;}
        if matches!(event,Event::Window{win_event:sdl2::event::WindowEvent::FocusLost,..}){self.0=Mod::NOMOD;}
        matches!(event,Event::MouseWheel{..}).then_some(self.0)
    }
}
pub(super) struct Buffer {events:Vec<InputEvent>}
impl Buffer {
    pub fn new(paste:Event)->Self{Self{events:vec![paste.into()]}}
    pub fn push(&mut self,event:InputEvent)->Result<(),InputEvent> {
        if self.events.len()==256{return Err(event)}
        self.events.push(event);Ok(())
    }
    pub fn take(&mut self)->Vec<InputEvent>{std::mem::take(&mut self.events)}
    pub fn following_events(&mut self,held:&mut bool)->Vec<InputEvent>{
        *held=true;
        // PNG success emits one Cmd+V; failure emits none. Retries queued for
        // that same transaction are coalesced in both cases. Text fallback
        // instead uses take(), preserving the existing ordinary key path.
        self.take().into_iter().skip(1).filter(|e|!suppress_queued_paste(held,&e.event)).collect()
    }
}
#[cfg(test)] impl From<Vec<Event>> for Buffer {
    fn from(events:Vec<Event>)->Self {
        assert!(!events.is_empty()&&events.len()<=256);let mut modifiers=Modifiers::default();
        Self{events:events.into_iter().map(|event|InputEvent{modifiers:modifiers.observe(&event),event}).collect()}
    }
}
pub(super) fn paste_chord(event:&Event)->bool {
    matches!(event,Event::KeyDown{scancode:Some(Scancode::V),keymod,repeat:false,..}
        if keymod.intersects(Mod::LCTRLMOD|Mod::RCTRLMOD|Mod::LGUIMOD|Mod::RGUIMOD)
        && !keymod.intersects(Mod::LSHIFTMOD|Mod::RSHIFTMOD|Mod::LALTMOD|Mod::RALTMOD))
}
// Suppress a consumed V's repeats/release even when they arrive in a later poll.
// A fresh, unmodified V press still belongs to subsequent typing.
pub(super) fn suppress_paste_key(held:&mut bool,event:&Event)->bool {
    if !*held{return false}
    match event {
        Event::KeyUp{scancode:Some(Scancode::V),..}=>{*held=false;true},
        Event::KeyDown{scancode:Some(Scancode::V),repeat,..}=>{if *repeat{true}else{*held=false;false}},
        _=>false,
    }
}
pub(super) fn suppress_queued_paste(held:&mut bool,event:&Event)->bool {
    if suppress_paste_key(held,event){return true}
    if paste_chord(event){*held=true;return true}
    false
}
