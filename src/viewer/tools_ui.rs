//! GTK windows for file transfer, port mapping and terminal sessions. Paths, names and
//! ports are shown only in the local window; logs keep to the existing id/field diagnostics.
use super::tools::{files::{Entry,Files},portmap::PortMap,Packet,Transport};
use std::ffi::{CStr,CString};
use std::os::raw::{c_char,c_int,c_ulonglong};
use std::sync::{Arc,Mutex,atomic::{AtomicU64,AtomicBool,Ordering}};
use std::time::{Duration,Instant};
use tokio::sync::mpsc;

extern "C" {
    fn uu_tools_ui_start(kind:*const c_char,name:*const c_char)->c_int;
    fn uu_tools_ui_take(out:*mut c_char,len:usize)->c_int;
    fn uu_tools_ui_closed()->c_int;
    fn uu_tools_ui_hide();
    fn uu_tools_ui_present();
    fn uu_tools_ui_toast(text:*const c_char);
    fn uu_files_ui_set_remote(path:*const c_char,rows:*const c_char);
    fn uu_files_ui_task(id:c_int,name:*const c_char,status:*const c_char,transferred:c_ulonglong,total:c_ulonglong,from:*const c_char,to:*const c_char,done:c_int);
    fn uu_files_ui_refresh_local();
    fn uu_terminal_ui_set(rows:*const c_char,connected:c_int);
    fn uu_portmap_ui_set_rules(rows:*const c_char);
}
fn c(text:&str)->CString{CString::new(text.replace('\0',"")).unwrap_or_default()}
fn toast(text:&str){let t=c(text);unsafe{uu_tools_ui_toast(t.as_ptr())}}
fn take()->Option<String>{
    let mut buf=vec![0 as c_char;4096];
    (unsafe{uu_tools_ui_take(buf.as_mut_ptr(),buf.len())}!=0).then(||unsafe{CStr::from_ptr(buf.as_ptr())}.to_string_lossy().into_owned())
}
pub fn hide(){unsafe{uu_tools_ui_hide()}}
pub fn present(){unsafe{uu_tools_ui_present()}}
pub fn closed()->bool{(unsafe{uu_tools_ui_closed()})!=0}
fn start(kind:&str,name:&str)->Result<(),String>{
    let(k,n)=(c(kind),c(name));
    if unsafe{uu_tools_ui_start(k.as_ptr(),n.as_ptr())}!=0{return Err("Cannot open the window (GTK3 and a graphical session are required)".into())}Ok(())
}
/// Last path component for either separator; hosts may be Windows (`C:\x`) or Unix.
pub fn base_name(path:&str)->&str{path.trim_end_matches(['/','\\']).rsplit(['/','\\']).next().filter(|s|!s.is_empty()).unwrap_or(path)}
pub fn remote_join(dir:&str,name:&str)->String{
    let sep=if dir.contains('\\') && !dir.contains('/'){'\\'}else{'/'};
    if dir.ends_with(['/','\\']){format!("{dir}{name}")}else{format!("{dir}{sep}{name}")}
}
/// First free `name`, `name (1)`, ... in `dir` so a download never replaces a local file.
pub fn free_local(dir:&std::path::Path,name:&str)->std::path::PathBuf{
    let(stem,ext)=match name.rfind('.'){Some(i) if i>0=>(&name[..i],&name[i..]),_=>(name,"")};
    (0..).map(|n|dir.join(if n==0{name.to_owned()}else{format!("{stem} ({n}){ext}")})).find(|p|!p.exists()).unwrap()
}
pub fn entry_rows(entries:&[Entry])->String{
    let clean=|s:&str|s.replace(['\t','\n','\r']," ");
    entries.iter().map(|e|format!("{}\t{}\t{}\t{}\t{}\n",e.kind,clean(&e.name),e.size,e.modified,clean(&e.path))).collect()
}
enum Job {List(String),Mkdir(String,String),Transfer{id:i32,upload:bool,from:String,dir:String,generation:u64}}
struct Active {id:i32,name:String,from:String,to:String,last:Instant}
fn task(id:i32,name:&str,status:&str,done:u64,total:u64,from:&str,to:&str,state:i32){
    let(n,s,f,t)=(c(name),c(status),c(from),c(to));
    unsafe{uu_files_ui_task(id,n.as_ptr(),s.as_ptr(),done,total,f.as_ptr(),t.as_ptr(),state)}
}
fn reason(error:&str)->String{if error.contains("canceled"){crate::i18n::s("已取消","Canceled").into()}else{crate::tr!("失败：{error}","Failed: {error}")}}

pub async fn files(io:Transport,rx:mpsc::Receiver<Packet>,name:&str)->Result<(),String>{
    start("files",name)?;
    let(f,mut progress)=Files::new(io,rx);let f=Arc::new(f);
    let generation=Arc::new(AtomicU64::new(0));
    let active:Arc<Mutex<Option<Active>>>=Arc::new(Mutex::new(None));
    let shown=active.clone();
    let reporter=tokio::spawn(async move{
        while let Some(p)=progress.recv().await{
            let mut guard=shown.lock().unwrap();let Some(a)=guard.as_mut() else{continue};
            if a.last.elapsed()<Duration::from_millis(200) && p.transferred<p.total{continue}a.last=Instant::now();
            let percent=if p.total>0{p.transferred*100/p.total}else{0};
            task(a.id,&a.name,&crate::tr!("传输中 {percent}%","Transferring {percent}%"),p.transferred,p.total,&a.from,&a.to,0);
        }
    });
    let _reporter=super::tools::Cancel(reporter.abort_handle());
    let(jobs,mut queue)=mpsc::unbounded_channel::<Job>();
    let closing=Arc::new(AtomicBool::new(false));let worker_closing=closing.clone();
    let(worker_f,worker_gen,worker_active)=(f.clone(),generation.clone(),active.clone());
    let mut worker=tokio::spawn(async move{
        let mut remote=":/".to_owned();
        while let Some(job)=queue.recv().await{if worker_closing.load(Ordering::Acquire){break}match job{
            Job::List(path)=>match worker_f.list_dir(&path).await{
                Ok(entries)=>{remote=path.clone();let(p,r)=(c(&path),c(&entry_rows(&entries)));unsafe{uu_files_ui_set_remote(p.as_ptr(),r.as_ptr())}}
                Err(e)=>toast(&crate::tr!("无法读取远端目录：{e}","Cannot read remote folder: {e}")),
            },
            Job::Mkdir(path,parent)=>match worker_f.mkdir(&path).await{
                Ok(())=>if parent==remote{if let Ok(entries)=worker_f.list_dir(&remote).await{let(p,r)=(c(&remote),c(&entry_rows(&entries)));unsafe{uu_files_ui_set_remote(p.as_ptr(),r.as_ptr())}}},
                Err(e)=>toast(&crate::tr!("无法创建远端文件夹：{e}","Cannot create remote folder: {e}")),
            },
            Job::Transfer{id,upload,from,dir,generation}=>{
                let name=base_name(&from).to_owned();
                let to=if upload{remote_join(&dir,&name)}else{free_local(std::path::Path::new(&dir),&name).to_string_lossy().into_owned()};
                if generation!=worker_gen.load(Ordering::Acquire){task(id,&name,crate::i18n::s("已取消","Canceled"),0,0,&from,&to,2);continue}
                *worker_active.lock().unwrap()=Some(Active{id,name:name.clone(),from:from.clone(),to:to.clone(),last:Instant::now()});
                task(id,&name,crate::i18n::s("传输中","Transferring"),0,0,&from,&to,0);
                let result=if upload{worker_f.upload(&from,&to).await}else{worker_f.download(&from,&to).await};
                *worker_active.lock().unwrap()=None;
                match result{
                    Ok(())=>{
                        let size=std::fs::metadata(if upload{&from}else{&to}).map(|m|if m.is_file(){m.len()}else{0}).unwrap_or(0);
                        task(id,&name,crate::i18n::s("已完成","Done"),size,size,&from,&to,1);
                        if upload{if dir==remote{if let Ok(entries)=worker_f.list_dir(&remote).await{let(p,r)=(c(&remote),c(&entry_rows(&entries)));unsafe{uu_files_ui_set_remote(p.as_ptr(),r.as_ptr())}}}}
                        else{unsafe{uu_files_ui_refresh_local()}}
                    }
                    Err(e)=>task(id,&name,&reason(&e),0,0,&from,&to,2),
                }
            }
        }}
    });
    let _worker=super::tools::Cancel(worker.abort_handle());
    let _=jobs.send(Job::List(":/".into()));
    static NEXT_TASK:std::sync::atomic::AtomicI32=std::sync::atomic::AtomicI32::new(1);
    let result=loop{
        if closed(){break Ok(())}
        if worker.is_finished(){break Err("File transfer worker stopped".into())}
        while let Some(cmd)=take(){
            let parts:Vec<&str>=cmd.split('\t').collect();
            match parts.as_slice(){
                ["ls",path]=>{let _=jobs.send(Job::List((*path).into()));}
                ["mkdir",path]=>{let parent=path.rfind(['/','\\']).map(|i|path[..i.max(1)].to_owned()).unwrap_or_default();let _=jobs.send(Job::Mkdir((*path).into(),parent));}
                [kind@("put"|"get"),from,dir]=>{
                    let(id,upload)=(NEXT_TASK.fetch_add(1,Ordering::Relaxed),*kind=="put");
                    let name=base_name(from);task(id,name,crate::i18n::s("等待中","Waiting"),0,0,from,dir,0);
                    let _=jobs.send(Job::Transfer{id,upload,from:(*from).into(),dir:(*dir).into(),generation:generation.load(Ordering::Acquire)});
                }
                ["cancel"]=>{generation.fetch_add(1,Ordering::AcqRel);f.cancel();}
                _=>{}
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    closing.store(true,Ordering::Release);drop(jobs);f.cancel();
    // Allow the existing transfer cancellation handshake to release the host's window.
    if tokio::time::timeout(Duration::from_secs(5),&mut worker).await.is_err(){worker.abort();let _=worker.await;}
    reporter.abort();let _=reporter.await;result
}

pub async fn portmap(io:Transport,rx:mpsc::Receiver<Packet>,name:&str)->Result<(),String>{
    start("portmap",name)?;
    let p=PortMap::new(io,rx);
    let mut targets:std::collections::HashMap<u64,String>=Default::default();
    let mut last=String::from("-");
    let closed_transport=p.closed();tokio::pin!(closed_transport);
    let result=loop{
        if closed(){break Ok(())}
        while let Some(cmd)=take(){
            let parts:Vec<&str>=cmd.split('\t').collect();
            match parts.as_slice(){
                ["add",rule,local,host,port]=>match(local.parse::<u16>(),port.parse::<u16>()){
                    (Ok(local),Ok(port))=>match p.add_rule(*rule,local,*host,port).await{
                        Ok(id)=>{targets.insert(id,format!("{host}:{port}"));}
                        Err(e)=>toast(&crate::tr!("添加失败：{}","Add failed: {}",if e.contains("bind"){crate::i18n::s("本地端口已被占用","Local port is already in use").to_owned()}else{e})),
                    },
                    _=>toast(crate::i18n::s("端口无效","Invalid port")),
                },
                ["remove",id]=>if let Ok(id)=id.parse::<u64>(){
                    if let Err(e)=p.remove_rule(id).await{toast(&crate::tr!("删除失败：{e}","Remove failed: {e}"))}targets.remove(&id);
                },
                _=>{}
            }
        }
        let mut status=p.status();status.sort_by_key(|s|s.id);
        let rows:String=status.iter().map(|s|format!("{}\t{}\t127.0.0.1:{}\t{}\t{}\n",s.id,s.name.replace(['\t','\n']," "),s.local_port,
            targets.get(&s.id).map(String::as_str).unwrap_or("-"),
            if s.failures>0{crate::tr!("已启用 · {} 个连接 · {} 次失败","On · {} connections · {} failures",s.streams,s.failures)}else{crate::tr!("已启用 · {} 个连接","On · {} connections",s.streams)})).collect();
        if rows!=last{let r=c(&rows);unsafe{uu_portmap_ui_set_rules(r.as_ptr())};last=rows;}
        tokio::select!{_=&mut closed_transport=>{toast(crate::i18n::s("连接已断开","Disconnected"));tokio::time::sleep(Duration::from_secs(3)).await;break Err("Forwarding transport closed".into())},_=tokio::time::sleep(Duration::from_millis(200))=>{}}
    };
    let ids:Vec<u64>=p.status().iter().map(|s|s.id).collect();for id in ids{let _=p.remove_rule(id).await;}
    result
}

// The worker serializes LIST/KILL requests; GTK commands remain responsive during network waits.
pub async fn terminal(io:Transport,rx:mpsc::Receiver<Packet>,_device:&str,name:&str)->Result<(),String>{
    use super::tools::terminal::{Terminal,session_rows,local_terminal_command};
    let exe=std::env::current_exe().map_err(|_|"Cannot locate executable")?;
    start("terminal",name)?;
    let(t,mut events)=Terminal::new(io,rx);let t=Arc::new(t);
    let server=super::tools::terminal_ipc::Server::new()?;let path=server.path.clone();
    let(output,_)=tokio::sync::broadcast::channel(256);
    let ipc=super::tools::Abort(tokio::spawn(server.run(t.clone(),output.clone())));
    let relay=super::tools::Abort(tokio::spawn(async move{while let Some(e)=events.recv().await{let _=output.send(e);}}));
    let(jobs,mut queue)=mpsc::unbounded_channel::<u32>();
    let worker=tokio::spawn(async move{
        let operation=async{
            t.check_env().await?;
            let mut refresh=tokio::time::interval(Duration::from_secs(2));
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop{tokio::select!{
                _=refresh.tick()=>{
                    let rows=c(&session_rows(&t.list_sessions().await?));
                    unsafe{uu_terminal_ui_set(rows.as_ptr(),1)}
                },
                id=queue.recv()=>match id{
                    Some(id)=>{if t.kill(id).await.is_err(){toast(crate::i18n::s("结束会话失败，会话可能已退出","Could not end the session; it may have exited"))}
                        let rows=c(&session_rows(&t.list_sessions().await?));unsafe{uu_terminal_ui_set(rows.as_ptr(),1)}},
                    None=>return Ok::<(),String>(()),
                },
                _=tokio::time::sleep(Duration::from_millis(100)),if relay.0.is_finished()=>return Err("Terminal channel closed".into()),
            }}
        }.await;
        if operation.is_err(){unsafe{uu_terminal_ui_set(c("").as_ptr(),0)};toast(crate::i18n::s("终端连接已断开，请关闭窗口后重试","Terminal disconnected. Close the window and try again"))}
    });
    let _worker=super::tools::Cancel(worker.abort_handle());
    let mut children:Vec<std::process::Child>=Vec::new();
    while !closed(){
        if worker.is_finished(){ipc.0.abort();}
        children.retain_mut(|child|matches!(child.try_wait(),Ok(None)));
        while let Some(cmd)=take(){
            if worker.is_finished(){toast(crate::i18n::s("连接已断开","Disconnected"));continue}
            let parts:Vec<_>=cmd.split('\t').collect();
            let session=match parts.as_slice(){
                ["create"]=>None,
                ["open",id]=>match id.parse::<u32>().ok().filter(|n|*n!=0){Some(id)=>Some(id),None=>continue},
                ["kill",id]=>{if let Ok(id)=id.parse::<u32>(){let _=jobs.send(id);}continue},
                _=>continue,
            };
            match local_terminal_command(&exe,&path,session){
                Some(mut command)=>match command.spawn(){Ok(child)=>children.push(child),Err(_)=>toast(crate::i18n::s("无法启动本机终端程序","Cannot start a local terminal"))},
                None=>toast(crate::i18n::s("没有找到本机终端程序（可设置 TERMINAL 环境变量）","No local terminal found (set the TERMINAL environment variable)")),
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    worker.abort();let _=worker.await;
    drop(ipc);
    // Closing the list drops all local IPC clients; reap emulator launchers off-thread.
    std::thread::spawn(move||{for mut child in children{let _=child.wait();}});
    Ok(())
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn path_helpers_handle_both_separators() {
        assert_eq!(base_name("/home/u/a.txt"),"a.txt");assert_eq!(base_name("C:\\Users\\x\\"),"x");assert_eq!(base_name("/"),"/");
        assert_eq!(remote_join("/home/u","a"),"/home/u/a");assert_eq!(remote_join("/","a"),"/a");assert_eq!(remote_join("C:\\Users","a"),"C:\\Users\\a");
        let dir=std::env::temp_dir().join(format!("uu-free-{}",super::super::wire::uuid()));std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.txt"),b"").unwrap();
        assert_eq!(free_local(&dir,"a.txt"),dir.join("a (1).txt"));assert_eq!(free_local(&dir,"b"),dir.join("b"));
        std::fs::remove_file(dir.join("a.txt")).unwrap();std::fs::remove_dir(&dir).unwrap();
    }
}
