//! FTP RPC cases and 1-based file indexes follow src/file_trans.rs.
use super::*;
use std::{collections::{BTreeMap,HashMap},io::Read,path::{Path,PathBuf},sync::atomic::{AtomicBool,AtomicU64,Ordering}};
use tokio::{io::{AsyncReadExt,AsyncWriteExt},sync::{Mutex as AsyncMutex,Notify}};
#[derive(Debug,Clone)]
pub struct Entry {pub kind:u64,pub name:String,pub size:u64,pub modified:u64,pub path:String}
#[derive(Clone,Copy,Debug)]
pub struct Progress {pub task:u64,pub transferred:u64,pub total:u64}
// Keep request/task ids unique when a desktop file window is reopened.
static NEXT:AtomicU64=AtomicU64::new(100);
pub struct Files {io:Transport,rx:AsyncMutex<mpsc::Receiver<Packet>>,next:&'static AtomicU64,canceled:AtomicBool,poisoned:AtomicBool,wake:Notify,progress:mpsc::Sender<Progress>}
/// Last path component for either separator; hosts may be Windows (`C:\x`) or Unix.
pub fn base_name(path:&str)->&str{path.trim_end_matches(['/','\\']).rsplit(['/','\\']).next().filter(|s|!s.is_empty()).unwrap_or(path)}
pub fn remote_join(dir:&str,name:&str)->String{
    let sep=if dir.contains('\\') && !dir.contains('/'){'\\'}else{'/'};
    if dir.ends_with(['/','\\']){format!("{dir}{name}")}else{format!("{dir}{sep}{name}")}
}
fn check(code:u64)->Result<(),String>{if code==1{Ok(())}else{Err(format!("File transfer error code {code}"))}}
fn id(task:u64,index:u64)->Vec<u8>{nested(1,&ints(&[(1,task),(2,index)]))}
fn all(data:&[u8],n:u32)->Result<Vec<&[u8]>,String>{Ok(decode_wire(data).map_err(|_|"Invalid file protobuf")?.into_iter().filter_map(|f|match(f.number,f.value){(k,WireValue::Bytes(b))if k==n=>Some(b),_=>None}).collect())}
pub fn inflate(data:&[u8])->Result<Vec<u8>,String>{
    let mut out=Vec::new();flate2::read::ZlibDecoder::new(data).take(16*1024*1024+1).read_to_end(&mut out).map_err(|_|"Invalid directory compression")?;
    if out.len()>16*1024*1024{return Err("Directory expansion limit".into())}Ok(out)
}
/// Splits a remote target into (directory, file name). Windows hosts list paths with `\\` (C:\\Users\\...),
/// Linux hosts with `/`; the directory keeps the host's own separator style. No directory means home.
/// Download block limit. Our host sends 32 KiB blocks; the official Windows host sends larger ones
/// (a 64 KiB file arrived as one block), bounded by the 256 KiB SCTP message size.
const MAX_BLOCK:usize=256*1024;
fn split_remote(remote:&str)->Option<(&str,&str)>{
    let cut=remote.rfind(['/','\\']);
    let (dir,name)=match cut{Some(i)=>(&remote[..i],&remote[i+1..]),None=>("",remote)};
    if name.is_empty(){return None}
    // "/x" keeps the root; "C:\\x" keeps "C:\\" (a bare drive letter is not a directory).
    let dir=match cut{None=>"~/",Some(i) if dir.is_empty()||dir.ends_with(':')=>&remote[..i+1],_=>dir};
    Some((dir,name))
}
fn safe(path:&str)->Result<PathBuf,String>{
    let normalized=path.replace('\\',"/");let p=Path::new(&normalized);
    if p.as_os_str().is_empty() || p.components().any(|c|!matches!(c,std::path::Component::Normal(_))) || normalized.contains(':') {return Err("Unsafe remote relative path".into())}Ok(p.to_owned())
}
struct Stage(PathBuf);
impl Drop for Stage {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
/// Download block ids start at 1 on the official Windows host and at 0 (sent out of order within a
/// 16-block window) on ours. `next` stays unset until block 0 arrives, 16 blocks are buffered, or the
/// file completes; the lowest buffered id is then the base.
struct Download {file:tokio::fs::File,written:u64,next:Option<u64>,blocks:BTreeMap<u64,Vec<u8>>}
impl Files {
    pub fn new(io:Transport,rx:mpsc::Receiver<Packet>)->(Self,mpsc::Receiver<Progress>){let(tx,progress)=mpsc::channel(64);(Self{io,rx:AsyncMutex::new(rx),next:&NEXT,canceled:AtomicBool::new(false),poisoned:AtomicBool::new(false),wake:Notify::new(),progress:tx},progress)}
    /// Cancels the active operation; subsequent operations can reuse the session.
    pub fn cancel(&self){self.canceled.store(true,Ordering::Release);self.wake.notify_waiters();}
    async fn recv(&self,rx:&mut mpsc::Receiver<Packet>)->Result<Vec<u8>,String>{loop{
        let notified=self.wake.notified();tokio::pin!(notified);notified.as_mut().enable();
        if self.canceled.load(Ordering::Acquire){return Err("File transfer canceled".into())}
        let(c,b)=tokio::select!{r=receive(rx)=>r?,_=notified=>return Err("File transfer canceled".into())};if c==FILE{return Ok(b)}
    }}
    async fn request(&self,rx:&mut mpsc::Receiver<Packet>,case:u32,body:&[u8],expected:u32)->Result<Vec<u8>,String>{
        if self.poisoned.load(Ordering::Acquire){return Err("File session requires reconnect after failed cleanup".into())}
        let rid=self.next.fetch_add(1,Ordering::Relaxed);self.io.send(CONTROL,rpc(rid,false,case,body)).await?;
        loop{let b=self.recv(rx).await?;
            // Ids and field numbers only, to match official-host replies.
            if let Some(r)=blob(&b,22){log::info!("File RPC reply: want_id={rid} got_id={} case_fields={:?}",blob(r,1).map(|h|uint(h,1)).unwrap_or(0),blob(r,4).and_then(|f|crate::protocol::decode_wire(f).ok()).map(|f|f.iter().map(|x|x.number).collect::<Vec<_>>()).unwrap_or_default());}
            else {log::info!("File channel message: fields={:?}",crate::protocol::decode_wire(&b).map(|f|f.iter().map(|x|x.number).collect::<Vec<_>>()).unwrap_or_default());}
            if let Some(r)=blob(&b,22){if blob(r,1).is_some_and(|h|uint(h,1)==rid){return blob(r,4).and_then(|f|blob(f,expected)).map(Vec::from).ok_or("Unexpected file response case".into())}}}
    }
    pub async fn list_dir(&self,path:&str)->Result<Vec<Entry>,String>{
        let mut rx=self.rx.lock().await;self.canceled.store(false,Ordering::Release);
        let mut body=id(self.next.fetch_add(1,Ordering::Relaxed),0);wire::bytes(2,path.as_bytes(),&mut body);
        let b=self.request(&mut rx,10,&body,7).await?;check(uint(&b,4))?;
        let expanded=blob(&b,5).map(inflate).transpose()?;
        let rows=if let Some(ref d)=expanded {all(d,1)?}else{all(&b,3)?};
        rows.into_iter().map(|r|Ok(Entry{kind:uint(r,1),name:text(r,2)?,size:uint(r,3),modified:uint(r,4),path:text(r,5)?})).collect()
    }
    pub async fn mkdir(&self,path:&str)->Result<(),String>{let mut rx=self.rx.lock().await;self.canceled.store(false,Ordering::Release);let b=self.request(&mut rx,6,&nested(1,path.as_bytes()),5).await?;check(uint(&b,1))}
    fn report(&self,task:u64,transferred:u64,total:u64){let _=self.progress.try_send(Progress{task,transferred,total});}
    pub async fn upload(&self,local:impl AsRef<Path>,remote:&str)->Result<(),String>{
        let mut rx=self.rx.lock().await;self.canceled.store(false,Ordering::Release);
        let mut file=tokio::fs::File::open(local).await.map_err(|_|"Cannot open upload file")?;
        let meta=file.metadata().await.map_err(|_|"Cannot stat upload file")?;if !meta.is_file(){return Err("Upload requires a regular file".into())}let size=meta.len();
        let (parent,name)=split_remote(remote).ok_or("Invalid remote file name")?;safe(name)?;
        let task=self.next.fetch_add(1,Ordering::Relaxed);
        // GvPb_FileInfo {1 rel_path, 2 size, 3 modified_time (unix s)}; receive_request {.., 7 task_unique_id}.
        // The official Windows host rejects (code 2) a request without the modified time / unique id.
        let modified=meta.modified().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0,|d|d.as_secs());
        let mut info=nested(1,name.as_bytes());wire::int(2,size,&mut info);wire::int(3,modified,&mut info);
        let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let unique=format!("{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",now.as_secs() as u32,task&0xffff,(now.subsec_nanos()>>20)&0xfff,std::process::id()&0xfff,now.as_nanos()&0xffff_ffff_ffff);
        let mut body=id(task,0);wire::bytes(2,parent.as_bytes(),&mut body);wire::bytes(3,&info,&mut body);wire::int(4,1,&mut body);
        // Official viewers with the list-compress feature send FileList{1 files} zlib-compressed in list_data (6);
        // the official Windows host only registers files from there. Keep the inline copy for older hosts.
        let mut list=Vec::new();wire::bytes(1,&info,&mut list);
        let mut z=flate2::write::ZlibEncoder::new(Vec::new(),flate2::Compression::default());
        std::io::Write::write_all(&mut z,&list).map_err(|_|"Cannot compress file list")?;
        wire::bytes(6,&z.finish().map_err(|_|"Cannot compress file list")?,&mut body);
        wire::bytes(7,unique.as_bytes(),&mut body);
        let result=async {
            let r=self.request(&mut rx,1,&body,1).await?;check(uint(&r,3))?;
            let mut ask=id(task,1);wire::int(3,size,&mut ask);let r=self.request(&mut rx,3,&ask,3).await?;check(uint(&r,3))?;
            if uint(&r,2)!=0 || uint(&r,4)!=0{return Err("Unexpected upload skip/resume".into())}
            let mut done=0;let mut block=0;let mut buf=vec![0;32*1024];
            loop{let n=file.read(&mut buf).await.map_err(|_|"Upload read failed")?;if n==0{break}
                let mut b=id(task,1);wire::int(2,block,&mut b);wire::bytes(3,&buf[..n],&mut b);
                let r=self.request(&mut rx,4,&b,4).await?;check(uint(&r,3))?;
                if uint(&r,2)!=block || uint(&r,4)!=n as u64{return Err("Invalid upload block acknowledgement".into())}
                done+=n as u64;block+=1;self.report(task,done,size);
            }
            if done!=size{return Err("Upload size changed".into())}
            let mut b=id(task,1);wire::int(2,1,&mut b);let r=self.request(&mut rx,9,&b,6).await?;check(uint(&r,2))?;self.report(task,done,size);Ok(())
        }.await;
        if result.is_err(){let mut b=id(task,1);wire::int(2,7,&mut b);let _=self.io.send(CONTROL,rpc(self.next.fetch_add(1,Ordering::Relaxed),false,9,&b)).await;}
        result
    }
    pub async fn download(&self,remote:&str,local:impl AsRef<Path>)->Result<(),String>{
        let local=local.as_ref();let mut rx=self.rx.lock().await;self.canceled.store(false,Ordering::Release);
        let task=self.next.fetch_add(1,Ordering::Relaxed);let mut b=id(task,0);wire::bytes(2,remote.as_bytes(),&mut b);
        let mut accepted=false;let mut ended=false;
        let result=async {
        let r=self.request(&mut rx,2,&b,2).await?;check(uint(&r,2))?;accepted=true;
        let expanded=blob(&r,5).map(inflate).transpose()?;
        let list=if let Some(ref e)=expanded{all(e,1)?}else{all(&r,3)?};
        let files:Vec<(PathBuf,u64)>=list.iter().map(|f|Ok((safe(&text(f,1)?)?,uint(f,2)))).collect::<Result<_,String>>()?;
        let folder=!text(&r,4)?.is_empty();if !folder && files.len()!=1{return Err("Invalid single file list".into())}
        let stage=Stage(local.with_file_name(format!(".uu-download-{}",wire::uuid())));
        tokio::fs::create_dir(&stage.0).await.map_err(|_|"Cannot create download staging directory")?;
        let mut open:HashMap<u64,Download>=HashMap::new();let mut completed=std::collections::HashSet::new();let total=files.iter().try_fold(0u64,|sum,x|sum.checked_add(x.1)).ok_or("Download size overflow")?;let mut done=0;
        loop {
            let data=self.recv(&mut rx).await?;
            let Some(rpc_body)=blob(&data,21).filter(|r|blob(r,8).is_some()) else{
                // Shape only (field numbers, size): what the host sent that is not a file RPC request.
                log::info!("Download skipped message: bytes={} fields={:?} inner={:?}",data.len(),decode_wire(&data).map(|f|f.iter().map(|x|x.number).collect::<Vec<_>>()).unwrap_or_default(),
                    decode_wire(&data).ok().and_then(|f|f.into_iter().find_map(|x|match x.value{WireValue::Bytes(b) if x.number>2=>decode_wire(b).ok().map(|g|(x.number,g.iter().map(|y|y.number).collect::<Vec<_>>())),_=>None})));
                continue};
            let ftp=blob(rpc_body,8).unwrap();
            let rid=blob(rpc_body,1).map(|h|uint(h,1)).unwrap_or(0);
            let Some(f)=decode_wire(ftp).ok().and_then(|v|v.into_iter().find(|f|matches!(f.value,WireValue::Bytes(_))))else{return Err("Invalid download request".into())};
            let WireValue::Bytes(body)=f.value else{unreachable!()};let tid=blob(body,1).unwrap_or_default();if uint(tid,1)!=task{return Err("Unexpected download task".into())}let index=uint(tid,2);
            let mut reply=id(task,index);let mut code=1;
            let response=match f.number {
                3=>{
                    if let Some((path,size))=index.checked_sub(1).and_then(|i|files.get(i as usize)) {
                        if uint(body,3)!=*size || open.contains_key(&index)||completed.contains(&index){code=3}else{
                            let path=stage.0.join(path);if let Some(p)=path.parent(){tokio::fs::create_dir_all(p).await.map_err(|_|"Cannot create download directory")?;}
                            match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await {
                                Ok(file)=>{open.insert(index,Download{file,written:0,next:None,blocks:BTreeMap::new()});},Err(_)=>code=6,
                            }
                        }
                    }else{code=3}wire::int(3,code,&mut reply);3
                },
                4=>{
                    let block=uint(body,2);let chunk=blob(body,3).unwrap_or_default();
                    if let Some(d)=open.get_mut(&index){
                        let expected=files[index as usize-1].1;
                        if chunk.len()>MAX_BLOCK || d.next.is_some_and(|n|block<n) || d.blocks.contains_key(&block)||d.blocks.len()>=32 {code=3}else{
                            d.blocks.insert(block,chunk.to_vec());
                            if d.next.is_none() && (block==0 || d.blocks.len()>=16) {d.next=d.blocks.keys().next().copied();}
                            while let Some(chunk)=d.next.and_then(|n|d.blocks.remove(&n)){
                                if d.written+chunk.len() as u64>expected || d.file.write_all(&chunk).await.is_err(){code=6;break}
                                d.written+=chunk.len() as u64;done+=chunk.len() as u64;d.next=d.next.map(|n|n+1);
                            }
                        }
                    }else{code=3}
                    wire::int(2,block,&mut reply);wire::int(3,code,&mut reply);wire::int(4,chunk.len() as u64,&mut reply);self.report(task,done,total);4
                },
                9=>{
                    if index==u64::MAX {
                        ended=true;check(uint(body,2))?;
                        if completed.len()!=files.len() || !open.is_empty(){return Err("Incomplete download task".into())}
                        if folder {tokio::fs::rename(&stage.0,local).await.map_err(|_|"Cannot finalize download directory")?;}else{tokio::fs::rename(stage.0.join(&files[0].0),local).await.map_err(|_|"Cannot finalize download file")?;}
                        self.report(task,done,total);return Ok(())
                    }
                    if let Some(mut d)=open.remove(&index){
                        if d.next.is_none(){d.next=d.blocks.keys().next().copied();}
                        while let Some(chunk)=d.next.and_then(|n|d.blocks.remove(&n)){
                            if d.written+chunk.len() as u64>files[index as usize-1].1 || d.file.write_all(&chunk).await.is_err(){break}
                            d.written+=chunk.len() as u64;done+=chunk.len() as u64;d.next=d.next.map(|n|n+1);
                        }
                        if uint(body,2)!=1 || d.written!=files[index as usize-1].1 || !d.blocks.is_empty() || d.file.flush().await.is_err(){code=6}else{completed.insert(index);}
                    }else{code=3}wire::int(2,code,&mut reply);6
                },_=>return Err("Unexpected download request case".into()),
            };
            // Numbers only: request case, file index, expected/declared sizes, our result code.
            if code!=1 || f.number!=4 {log::info!("Download step: case={} index={} files={} declared={} expected={} status={} code={code}",f.number,index,files.len(),uint(body,3),index.checked_sub(1).and_then(|i|files.get(i as usize)).map_or(0,|x|x.1),uint(body,2));}
            self.io.send(CONTROL,rpc(rid,true,response,&reply)).await?;
            if code!=1{return Err(format!("Download rejected with code {code}"))}
        }
        }.await;
        if result.is_err() && !ended && (accepted || self.canceled.load(Ordering::Acquire)) {
            // Reject outstanding host requests so cancellation releases its transfer window.
            let cleanup=async {
                loop {
                    let Some((channel,data))=rx.recv().await else{return};if channel!=FILE{continue}
                    let Some(r)=blob(&data,21)else{continue};let Some(ftp)=blob(r,8)else{continue};
                    let rid=blob(r,1).map(|h|uint(h,1)).unwrap_or(0);
                    for (request,response,error_field) in [(3,3,3),(4,4,3),(9,6,2)] {
                        let Some(body)=blob(ftp,request)else{continue};let tid=blob(body,1).unwrap_or_default();
                        if uint(tid,1)!=task{continue}
                        if request==9 && uint(tid,2)==u64::MAX{return}
                        let mut reply=nested(1,tid);wire::int(error_field,7,&mut reply);
                        if request==4{wire::int(2,uint(body,2),&mut reply);}
                        let _=self.io.send(CONTROL,rpc(rid,true,response,&reply)).await;
                    }
                }
            };
            if tokio::time::timeout(Duration::from_secs(3),cleanup).await.is_err(){self.poisoned.store(true,Ordering::Release);}
        }
        result
    }
}
#[cfg(test)]
mod split_tests{
    #[test] fn remote_targets_keep_the_host_separator(){
        assert_eq!(super::split_remote(r"C:\Users\Public\Downloads\a.bin"),Some((r"C:\Users\Public\Downloads","a.bin")));
        assert_eq!(super::split_remote(r"C:\a.bin"),Some((r"C:\","a.bin")));
        assert_eq!(super::split_remote("/tmp/a.bin"),Some(("/tmp","a.bin")));
        assert_eq!(super::split_remote("/a.bin"),Some(("/","a.bin")));
        assert_eq!(super::split_remote("a.bin"),Some(("~/","a.bin")));
        assert_eq!(super::split_remote("/tmp/"),None);
    }
}
