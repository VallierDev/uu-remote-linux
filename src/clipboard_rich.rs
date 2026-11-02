//! Text-only desktop clipboard format negotiation. No file formats are offered.
use crate::clipboard::MAX_TEXT;
use crate::clipboard_protocol::{nested,varint};
use crate::protocol::{decode_wire,WireValue};
use std::collections::BTreeMap;

const UNICODE_TEXT:u64=13;
/// The official Mac host offers its pasteboard text as format id 0 named `public.utf8-plain-text` (observed live).
const MAC_TEXT:&str="public.utf8-plain-text";
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
enum TextFormat {Windows,Mac}
impl TextFormat {
    fn of(format:&[u8])->Option<Self>{
        if number(format,1)==Ok(UNICODE_TEXT){return Some(Self::Windows)}
        let named=|n|text_field(format,n).is_ok_and(|s|s==MAC_TEXT);
        (named(2)||named(3)).then_some(Self::Mac)
    }
    fn id(self)->u64{match self{Self::Windows=>UNICODE_TEXT,Self::Mac=>0}}
    fn name(self)->&'static str{match self{Self::Windows=>"CF_UNICODETEXT",Self::Mac=>MAC_TEXT}}
    /// Name in a format-list offer. Official Windows peers name predefined formats `predefined_<id>`; a plain
    /// `CF_UNICODETEXT` makes the Windows viewer register a custom format instead of id 13 (issue #151).
    fn offer_name(self)->&'static str{match self{Self::Windows=>WINDOWS_OFFER_NAME,Self::Mac=>MAC_TEXT}}
    fn encode(self,text:&str)->Vec<u8>{match self{Self::Windows=>encode_windows(text),Self::Mac=>text.as_bytes().to_vec()}}
    fn decode(self,data:&[u8])->Result<String,&'static str>{
        match self{
            Self::Windows=>decode_windows(data),
            Self::Mac=>{
                let data=data.strip_suffix(&[0]).unwrap_or(data);
                if data.len()>MAX_TEXT{return Err("clipboard text exceeds limit");}
                let text=std::str::from_utf8(data).map_err(|_|"invalid UTF-8 clipboard text")?;
                if text.contains('\0'){return Err("embedded NUL in clipboard text");}
                Ok(text.to_owned())
            }
        }
    }
}
const WINDOWS_OFFER_NAME:&str="predefined_13";
const BLOCK_BYTES:usize=0x20000;
const MAX_BYTES:usize=MAX_TEXT*2+2;
pub struct ResultMessages {pub messages:Vec<(u32,Vec<u8>)>,pub text:Option<String>}
impl Default for ResultMessages {fn default()->Self{Self{messages:Vec::new(),text:None}}}
struct Pending {key:String, format:TextFormat, count:Option<usize>, blocks:BTreeMap<usize,Vec<u8>>}
#[derive(Default)]
pub struct State {local:Option<String>, pending:Option<Pending>}
pub fn format_ids(rpc:&[u8])->Vec<u64>{
    let Ok(Some(clip))=bytes(rpc,9) else{return Vec::new();};
    let Ok(Some(list))=bytes(clip,1) else{return Vec::new();};
    let Ok(fields)=decode_wire(list) else{return Vec::new();};
    fields.iter().filter(|f|f.number==1).filter_map(|f|match f.value{WireValue::Bytes(format)=>number(format,1).ok(),_=>None}).take(32).collect()
}

fn int(field:u32,value:u64,out:&mut Vec<u8>){varint(u64::from(field)<<3,out);varint(value,out);}
fn string(field:u32,value:&str,out:&mut Vec<u8>){nested(field,value.as_bytes(),out);}
fn number(data:&[u8],field:u32)->Result<u64,&'static str>{
    let fields=decode_wire(data).map_err(|_|"invalid clipboard message")?;
    match fields.iter().find(|f|f.number==field).map(|f|&f.value){
        Some(WireValue::Varint(v))=>Ok(*v),None=>Ok(0),_=>Err("invalid clipboard integer"),
    }
}
fn bytes(data:&[u8],field:u32)->Result<Option<&[u8]>,&'static str>{
    let fields=decode_wire(data).map_err(|_|"invalid clipboard message")?;
    match fields.iter().find(|f|f.number==field).map(|f|&f.value){
        Some(WireValue::Bytes(v))=>Ok(Some(*v)),None=>Ok(None),_=>Err("invalid clipboard bytes"),
    }
}
fn text_field(data:&[u8],field:u32)->Result<&str,&'static str>{
    std::str::from_utf8(bytes(data,field)?.unwrap_or(&[])).map_err(|_|"invalid clipboard string")
}
fn header(data:&[u8])->Result<&[u8],&'static str>{
    let h=bytes(data,1)?.ok_or("missing clipboard request header")?;
    if h.len()>32{return Err("clipboard header exceeds limit");}
    number(h,1)?;Ok(h)
}
fn request(id:u64,kind:u32,body:&[u8])->Vec<u8>{
    let mut h=Vec::new();int(1,id,&mut h);
    let mut clip=Vec::new();nested(kind,body,&mut clip);
    let mut rpc=Vec::new();nested(1,&h,&mut rpc);nested(9,&clip,&mut rpc);rpc
}
fn response(h:&[u8],kind:u32,body:&[u8])->Vec<u8>{
    let mut clip=Vec::new();nested(kind,body,&mut clip);
    let mut rpc=Vec::new();nested(1,h,&mut rpc);nested(5,&clip,&mut rpc);rpc
}
fn encode_windows(text:&str)->Vec<u8>{
    text.replace("\r\n","\n").replace('\n',"\r\n").encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}
fn decode_windows(data:&[u8])->Result<String,&'static str>{
    if data.len()>MAX_BYTES||data.len()%2!=0{return Err("invalid Unicode clipboard length");}
    let mut chars:Vec<u16>=data.chunks_exact(2).map(|v|u16::from_le_bytes([v[0],v[1]])).collect();
    if chars.last()==Some(&0){chars.pop();}
    if chars.contains(&0){return Err("embedded NUL in clipboard text");}
    let text=String::from_utf16(&chars).map_err(|_|"invalid Unicode clipboard text")?.replace("\r\n","\n");
    if text.len()>MAX_TEXT{return Err("clipboard text exceeds limit");}Ok(text)
}
impl State{
    pub fn offer(&mut self,id:u64,text:String)->Result<Vec<u8>,&'static str>{self.offer_formats(id,text,&[TextFormat::Windows])}
    /// Viewer offers: Windows hosts pick CF_UNICODETEXT, the official Mac host its UTF-8 type.
    pub fn offer_any_host(&mut self,id:u64,text:String)->Result<Vec<u8>,&'static str>{self.offer_formats(id,text,&[TextFormat::Windows,TextFormat::Mac])}
    fn offer_formats(&mut self,id:u64,text:String,formats:&[TextFormat])->Result<Vec<u8>,&'static str>{
        if text.len()>MAX_TEXT{return Err("clipboard text exceeds limit");}
        if text.contains('\0')||encode_windows(&text).len()>MAX_BYTES{return Err("clipboard encoded text exceeds limit");}
        self.local=Some(text);
        let mut list=Vec::new();
        for kind in formats {let mut format=Vec::new();int(1,kind.id(),&mut format);string(2,kind.offer_name(),&mut format);nested(1,&format,&mut list);}
        Ok(request(id,1,&list))
    }
    pub fn request(&mut self,rpc:&[u8],next:&mut impl FnMut()->u64)->Result<ResultMessages,&'static str>{
        if rpc.len()>MAX_BYTES+512{return Err("clipboard RPC exceeds limit");}
        let Some(clip)=bytes(rpc,9)? else{return Ok(ResultMessages::default());};
        let h=header(rpc)?;
        let mut out=ResultMessages::default();
        if let Some(list)=bytes(clip,1)?{
            out.messages.push((22,response(h,1,&[])));
            let fields=decode_wire(list).map_err(|_|"invalid clipboard format list")?;
            let offered:Vec<TextFormat>=fields.iter().filter(|f|f.number==1).filter_map(|f|match f.value{WireValue::Bytes(format)=>TextFormat::of(format),_=>None}).collect();
            let chosen=if offered.contains(&TextFormat::Windows){Some(TextFormat::Windows)}else{offered.first().copied()};
            let supports_text=chosen.is_some();
            self.pending=None;
            if !supports_text{
                // Format ids/names only (never data), to learn non-Windows peers' text formats.
                let formats:Vec<(u64,String)>=fields.iter().filter(|f|f.number==1).filter_map(|f|match f.value{WireValue::Bytes(format)=>Some(format),_=>None})
                    .take(8).map(|format|(number(format,1).unwrap_or(u64::MAX),decode_wire(format).ok().and_then(|x|x.into_iter().find_map(|y|match (y.number,y.value){(n,WireValue::Bytes(s)) if n!=1=>std::str::from_utf8(s).ok().map(|s|s.chars().take(48).collect()),_=>None})).unwrap_or_default())).collect();
                log::info!("Clipboard offer without a supported text format: formats={formats:?}");
            }
            if let Some(format)=chosen{
                let id=next();
                let epoch=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as u32;
                let key=format!("{epoch:08x}-0000-4000-8000-{:012x}",id & 0xffffffffffff);
                let mut ask=Vec::new();int(1,format.id(),&mut ask);string(2,&key,&mut ask);string(3,format.name(),&mut ask);
                self.pending=Some(Pending{key,format,count:None,blocks:BTreeMap::new()});
                out.messages.push((21,request(id,2,&ask)));
            }
        }else if let Some(ask)=bytes(clip,2)?{
            let key=text_field(ask,2)?;
            if key.len()>256||key.is_empty(){return Err("invalid clipboard block key");}
            let format=if number(ask,1)?==UNICODE_TEXT{Some(TextFormat::Windows)}else if text_field(ask,3).is_ok_and(|s|s==MAC_TEXT){Some(TextFormat::Mac)}else{None};
            let data=format.zip(self.local.as_ref()).map(|(format,text)|format.encode(text));
            let mut confirm=Vec::new();int(1,if data.is_some(){1}else{2},&mut confirm);string(2,key,&mut confirm);
            if let Some(data)=&data{int(3,data.chunks(BLOCK_BYTES).len() as u64,&mut confirm);}
            out.messages.push((22,response(h,2,&confirm)));
            if let Some(data)=data{
                for (index,part) in data.chunks(BLOCK_BYTES).enumerate(){
                    let mut block=Vec::new();string(1,key,&mut block);int(2,index as u64,&mut block);nested(3,part,&mut block);
                    out.messages.push((21,request(next(),3,&block)));
                }
            }
        }else if let Some(block)=bytes(clip,3)?{
            let key=text_field(block,1)?;
            let index=usize::try_from(number(block,2)?).map_err(|_|"invalid clipboard block index")?;
            let data=bytes(block,3)?.unwrap_or(&[]);
            log::info!("Clipboard incoming block metadata: index={} bytes={}",index,data.len());
            if index>MAX_BYTES/BLOCK_BYTES+1||data.len()>MAX_BYTES{return Err("clipboard block exceeds limit");}
            let pending=self.pending.as_mut().ok_or("unsolicited clipboard block")?;
            if key!=pending.key{return Err("unexpected clipboard block key");}
            if let Some(existing)=pending.blocks.get(&index){
                if existing!=data{return Err("conflicting clipboard block");}
            }else{
                if pending.blocks.values().map(Vec::len).sum::<usize>()+data.len()>MAX_BYTES{return Err("clipboard transfer exceeds limit");}
                pending.blocks.insert(index,data.to_vec());
            }
            let mut confirm=Vec::new();string(1,key,&mut confirm);int(2,index as u64,&mut confirm);int(3,1,&mut confirm);
            out.messages.push((22,response(h,3,&confirm)));
            out.text=self.complete()?;
        }
        Ok(out)
    }
    pub fn response(&mut self,rpc:&[u8])->Result<Option<String>,&'static str>{
        let Some(clip)=bytes(rpc,5)? else{return Ok(None);};
        let Some(confirm)=bytes(clip,2)? else{return Ok(None);};
        let Some(pending)=self.pending.as_mut() else{return Ok(None);};
        if text_field(confirm,2)?!=pending.key{return Ok(None);}
        let status=number(confirm,1)?;
        log::info!("Clipboard data confirmation metadata: status={} block_count={}",status,number(confirm,3)?);
        if status!=1{self.pending=None;return Ok(None);}
        let count=usize::try_from(number(confirm,3)?).map_err(|_|"invalid clipboard block count")?;
        if count==0||count>MAX_BYTES/BLOCK_BYTES+2{self.pending=None;return Err("invalid clipboard block count");}
        pending.count=Some(count);self.complete()
    }
    fn complete(&mut self)->Result<Option<String>,&'static str>{
        let Some(pending)=&self.pending else{return Ok(None);};
        let Some(count)=pending.count else{return Ok(None);};
        // Native providers may number wire blocks from one while their internal
        // buffer slicing uses a zero-based index. Accept a complete contiguous
        // set in either convention; never complete from a missing middle block.
        let first=if (0..count).all(|i|pending.blocks.contains_key(&i)){0}
            else if (1..=count).all(|i|pending.blocks.contains_key(&i)){1}
            else{return Ok(None);};
        let data:Vec<u8>=(first..first+count).flat_map(|i|pending.blocks[&i].iter().copied()).collect();
        let format=pending.format;self.pending=None;format.decode(&data).map(Some)
    }
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test] fn mac_utf8_text_round_trips_both_directions(){
        // Mac host offer: format list {1 id 0, 2 "public.utf8-plain-text"} (observed live).
        let mut mac=Vec::new();int(1,0,&mut mac);string(2,MAC_TEXT,&mut mac);
        let mut list=Vec::new();nested(1,&mac,&mut list);
        let mut viewer=State::default();let mut id=200;let mut next=||{id+=1;id};
        let asked=viewer.request(&request(7,1,&list),&mut next).unwrap();
        assert_eq!(asked.messages.len(),2);
        let ask_rpc=&asked.messages[1].1;let clip=bytes(ask_rpc,9).unwrap().unwrap();let ask=bytes(clip,2).unwrap().unwrap();
        assert_eq!(number(ask,1),Ok(0));assert_eq!(text_field(ask,3),Ok(MAC_TEXT));
        let key=text_field(ask,2).unwrap().to_owned();
        let mut confirm=Vec::new();int(1,1,&mut confirm);string(2,&key,&mut confirm);int(3,1,&mut confirm);
        assert_eq!(viewer.response(&response(header(ask_rpc).unwrap(),2,&confirm)).unwrap(),None);
        let mut block=Vec::new();string(1,&key,&mut block);int(2,0,&mut block);nested(3,"中文 text\n".as_bytes(),&mut block);
        assert_eq!(viewer.request(&request(8,3,&block),&mut next).unwrap().text.as_deref(),Some("中文 text\n"));
        // Viewer offer lists both formats; a Mac-style ask gets UTF-8 bytes back.
        let offer=viewer.offer_any_host(9,"hi".into()).unwrap();
        assert_eq!(format_ids(&offer),vec![13,0]);
        let mut ask=Vec::new();int(1,0,&mut ask);string(2,"k",&mut ask);string(3,MAC_TEXT,&mut ask);
        let out=viewer.request(&request(10,2,&ask),&mut next).unwrap();
        let data=bytes(bytes(bytes(&out.messages[1].1,9).unwrap().unwrap(),3).unwrap().unwrap(),3).unwrap().unwrap();
        assert_eq!(data,b"hi");
        // Host-side offers are unchanged: CF_UNICODETEXT only.
        let host_offer=State::default().offer(1,"hi".into()).unwrap();
        assert_eq!(format_ids(&host_offer),vec![13]);
        let list=bytes(bytes(&host_offer,9).unwrap().unwrap(),1).unwrap().unwrap();
        let format=bytes(list,1).unwrap().unwrap();
        assert_eq!(text_field(format,2),Ok(WINDOWS_OFFER_NAME));
    }
    #[test]
    fn desktop_clipboard_negotiates_and_buffers_unicode_until_confirmation(){
        let text="UU-test-中文🙂\n".repeat(900);
        let mut provider=State::default();let mut receiver=State::default();let mut id=10;let mut next=||{id+=1;id};
        let offer=provider.offer(1,text.clone()).unwrap();
        let result=receiver.request(&offer,&mut next).unwrap();assert_eq!(result.messages.len(),2);
        let send=provider.request(&result.messages[1].1,&mut next).unwrap();assert_eq!(send.messages.len(),2);
        // Blocks may arrive before DataConfirm; completion waits for both.
        for (_,block) in send.messages.iter().skip(1).rev(){assert!(receiver.request(block,&mut next).unwrap().text.is_none());}
        assert_eq!(receiver.response(&send.messages[0].1).unwrap(),Some(text));
    }
    #[test]
    fn rejects_invalid_unicode_unsolicited_blocks_and_unknown_formats(){
        assert!(decode_windows(&[0,0xd8,0,0]).is_err());
        assert!(decode_windows(&[1]).is_err());
        let mut s=State::default();let mut block=Vec::new();string(1,"unknown",&mut block);nested(3,&[0,0],&mut block);
        assert!(s.request(&request(1,3,&block),&mut||2).is_err());
        let mut format=Vec::new();int(1,15,&mut format);let mut list=Vec::new();nested(1,&format,&mut list);
        let out=s.request(&request(1,1,&list),&mut||2).unwrap();assert_eq!(out.messages.len(),1);assert!(s.pending.is_none());
    }
    #[test]
    fn accepts_one_based_wire_blocks_without_breaking_zero_based_provider(){
        let mut source=State::default();let mut target=State::default();let mut id=10;let mut next=||{id+=1;id};
        let offer=source.offer(1,"中文🙂".into()).unwrap();
        let offered=target.request(&offer,&mut next).unwrap();
        let sent=source.request(&offered.messages[1].1,&mut next).unwrap();
        assert!(target.response(&sent.messages[0].1).unwrap().is_none());
        let clip=bytes(&sent.messages[1].1,9).unwrap().unwrap();
        let original=bytes(clip,3).unwrap().unwrap();
        let mut block=Vec::new();string(1,text_field(original,1).unwrap(),&mut block);int(2,1,&mut block);nested(3,bytes(original,3).unwrap().unwrap(),&mut block);
        assert_eq!(target.request(&request(99,3,&block),&mut next).unwrap().text,Some("中文🙂".into()));
    }
}
