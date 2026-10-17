//! Bounded RFC 6184 packetization-mode 1 receiver. No media contents in logs.
use std::{collections::VecDeque, sync::{Arc, Mutex}};
use std::time::{Duration,Instant};
const MAX_AU: usize = 8 * 1024 * 1024;
#[derive(Clone)]
pub struct Packet { pub seq: u16, pub timestamp: u32, pub marker: bool, pub payload: Vec<u8> }
#[derive(Default)]
pub struct Output { pub units: Vec<Vec<u8>>, pub pli: bool }
pub struct Receiver {
    next: Option<u16>, pending: Vec<Packet>, timestamp: Option<u32>,
    au: Vec<u8>, fragment: Option<u8>, damaged: bool, gap_since:Option<Instant>, last_packet:Instant,
}
impl Default for Receiver {
    fn default() -> Self { Self { next: None, pending: Vec::new(), timestamp: None, au: Vec::new(), fragment: None, damaged: false, gap_since:None,last_packet:Instant::now() } }
}
impl Receiver {
    pub fn push(&mut self, p: Packet) -> Output {
        if p.payload.len()>65536 {return Output{units:Vec::new(),pli:true}}
        let next = *self.next.get_or_insert(p.seq);
        if p.seq.wrapping_sub(next) >= 32768 || self.pending.iter().any(|q| q.seq == p.seq) { return Output::default() }
        self.last_packet=Instant::now();
        self.pending.push(p);
        let mut out = Output::default();
        self.drain(&mut out);
        if self.pending.len() >= 32 { self.skip_gap(&mut out); }
        if self.pending.is_empty(){self.gap_since=None;}else{self.gap_since.get_or_insert(Instant::now());}
        out
    }
    // A timer bounds latency even if the missing packet is the last packet sent.
    pub fn expire(&mut self) -> Output {
        self.expire_at(Instant::now())
    }
    fn expire_at(&mut self,now:Instant)->Output {
        let mut out=Output::default();
        if self.gap_since.is_some_and(|since|now.duration_since(since)>=Duration::from_millis(40)) && !self.pending.is_empty() {
            self.skip_gap(&mut out);self.gap_since=if self.pending.is_empty(){None}else{Some(now)};
        }
        if !self.au.is_empty() && now.duration_since(self.last_packet)>=Duration::from_millis(200) {
            self.au.clear();self.fragment=None;self.damaged=true;out.pli=true;
        }
        out
    }
    fn skip_gap(&mut self, out: &mut Output) {
        let next = self.next.unwrap();
        self.next = self.pending.iter().min_by_key(|p| p.seq.wrapping_sub(next)).map(|p| p.seq);
        self.au.clear(); self.fragment = None; self.damaged = true; out.pli = true;
        self.drain(out);
    }
    fn drain(&mut self, out: &mut Output) {
        while let Some(i) = self.pending.iter().position(|p| Some(p.seq) == self.next) {
            let p = self.pending.remove(i); self.next = Some(p.seq.wrapping_add(1));
            if self.timestamp != Some(p.timestamp) {
                // A timestamp transition without marker is an incomplete AU.
                if !self.au.is_empty() || self.fragment.is_some() { out.pli = true; }
                self.au.clear(); self.fragment = None; self.damaged = false; self.timestamp = Some(p.timestamp);
            }
            if !self.damaged && self.nal(&p.payload).is_err() { self.damaged = true; self.au.clear(); self.fragment = None; out.pli = true; }
            if p.marker {
                if self.fragment.is_some() { out.pli = true; self.damaged = true; }
                if !self.damaged && !self.au.is_empty() { out.units.push(std::mem::take(&mut self.au)); }
                self.au.clear(); self.fragment = None;
            }
        }
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if self.au.len() + bytes.len() > MAX_AU { return Err(()) }
        self.au.extend_from_slice(bytes); Ok(())
    }
    fn nal(&mut self, p: &[u8]) -> Result<(), ()> {
        let h = *p.first().ok_or(())?;
        if h & 0x80 != 0 { return Err(()) }
        match h & 31 {
            1..=23 if self.fragment.is_none() => { self.append(&[0,0,0,1])?; self.append(p) }
            24 if self.fragment.is_none() => {
                let mut rest = &p[1..]; if rest.is_empty() { return Err(()) }
                while !rest.is_empty() {
                    if rest.len() < 2 { return Err(()) }
                    let n = u16::from_be_bytes([rest[0], rest[1]]) as usize; rest = &rest[2..];
                    if n == 0 || rest.len() < n || rest[0] & 0x80 != 0 || !(1..=23).contains(&(rest[0] & 31)) { return Err(()) }
                    self.append(&[0,0,0,1])?; self.append(&rest[..n])?; rest = &rest[n..];
                } Ok(())
            }
            28 if p.len() > 2 => {
                let f = p[1]; let kind = f & 31; let start = f & 128 != 0; let end = f & 64 != 0;
                if f & 32 != 0 || kind == 0 || kind > 23 || (start && end) { return Err(()) }
                let header = (h & 0xe0) | kind;
                if start {
                    if self.fragment.is_some() { return Err(()) }
                    self.append(&[0,0,0,1,header])?; self.fragment = Some(header);
                } else if self.fragment != Some(header) { return Err(()) }
                self.append(&p[2..])?;
                if end { self.fragment = None; } Ok(())
            }
            _ => Err(()),
        }
    }
}
/// Drop the oldest item on overflow; never block the RTP reader on a decoder/UI.
pub struct Latest<T>(Arc<Mutex<VecDeque<T>>>, usize);
impl<T> Clone for Latest<T> { fn clone(&self) -> Self { Self(self.0.clone(), self.1) } }
impl<T> Latest<T> {
    pub fn new(capacity: usize) -> Self { assert!(capacity > 0); Self(Arc::new(Mutex::new(VecDeque::new())), capacity) }
    pub fn push(&self, value: T) -> bool { let mut q = self.0.lock().unwrap(); let dropped = q.len() == self.1; if dropped { q.pop_front(); } q.push_back(value); dropped }
    pub fn pop(&self) -> Option<T> { self.0.lock().unwrap().pop_front() }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn p(seq:u16, ts:u32, marker:bool, payload:&[u8])->Packet { Packet{seq,timestamp:ts,marker,payload:payload.into()} }
    #[test] fn single_stap_and_boundary() {
        let mut r=Receiver::default();
        assert!(r.push(p(1,1,false,&[0x67,1])).units.is_empty());
        assert_eq!(r.push(p(2,1,true,&[24,0,2,0x68,2,0,2,0x65,3])).units[0],vec![0,0,0,1,0x67,1,0,0,0,1,0x68,2,0,0,0,1,0x65,3]);
        r.push(p(3,2,false,&[0x61,1]));
        let o=r.push(p(4,3,true,&[0x65,4])); assert!(o.pli); assert_eq!(o.units.len(),1);
    }
    #[test] fn fu_reorder_wrap_duplicate_and_loss() {
        let mut r=Receiver::default(); r.push(p(65535,1,false,&[0x7c,0x85,1]));
        assert!(r.push(p(1,1,true,&[0x7c,0x45,3])).units.is_empty());
        let o=r.push(p(0,1,false,&[0x7c,5,2])); assert_eq!(o.units[0],vec![0,0,0,1,0x65,1,2,3]);
        assert!(r.push(p(0,1,false,&[0x61])).units.is_empty());
        r.push(p(2,2,false,&[0x7c,0x85,1]));r.push(p(4,2,true,&[0x7c,0x45,3]));
        let o=r.expire_at(Instant::now()+Duration::from_millis(50));assert!(o.pli);assert!(o.units.is_empty());
        assert_eq!(r.push(p(5,3,true,&[0x65])).units.len(),1);
    }
    #[test] fn missing_tail_and_reorder_window_are_bounded() {
        let mut r=Receiver::default();r.push(p(0,1,false,&[0x7c,0x85,1]));
        assert!(!r.expire().pli);assert!(r.expire_at(Instant::now()+Duration::from_millis(201)).pli);
        let mut pli=false;for seq in 2..34{pli|=r.push(p(seq,seq as u32,true,&[0x65])).pli;}
        assert!(pli);assert!(r.pending.len()<32);
    }
    #[test] fn malformed_and_bounded() {
        for bytes in [vec![],vec![24,0,4,0x65],vec![0x7c,0xc5,1],vec![0xff]] {
            assert!(Receiver::default().push(p(1,1,true,&bytes)).pli);
        }
        let q=Latest::new(2);assert!(!q.push(1));q.push(2);assert!(q.push(3));assert_eq!(q.pop(),Some(2));assert_eq!(q.pop(),Some(3));
    }
}
