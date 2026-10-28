use super::*;
use tokio::io::{AsyncReadExt,AsyncWriteExt};
struct Mock(mpsc::Sender<Packet>);
#[async_trait::async_trait]
impl Send for Mock {async fn send(&self,c:usize,b:Vec<u8>)->Result<(),String>{self.0.send((c,b)).await.map_err(|_|"Mock closed".into())}}
fn pair()->(Transport,mpsc::Receiver<Packet>,mpsc::Sender<Packet>,mpsc::Receiver<Packet>){let(tx,out)=mpsc::channel(256);let(input,rx)=mpsc::channel(256);(Transport(Arc::new(Mock(tx))),rx,input,out)}
struct Temp(std::path::PathBuf);
impl Temp {fn new()->Self{let p=std::env::temp_dir().join(format!("uu-tools-test-{}",wire::uuid()));std::fs::create_dir(&p).unwrap();Self(p)}}
impl Drop for Temp {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
fn host_files()->(files::Files,mpsc::Receiver<files::Progress>,tokio::task::JoinHandle<()>,tokio::task::JoinHandle<()>){
    let(io,rx,input,mut out)=pair();let(tx,mut host)=mpsc::unbounded_channel();
    let session=crate::file_trans::FileSession::new(tx,Arc::new(std::sync::atomic::AtomicU64::new(1)));
    let server=tokio::spawn(async move{while let Some((c,b))=out.recv().await{assert_eq!(c,CONTROL);assert!(session.handle(&decode_wire(&b).unwrap()));}});
    let pump=tokio::spawn(async move{while let Some(b)=host.recv().await{if input.send((FILE,b)).await.is_err(){break}}});
    let(f,p)=files::Files::new(io,rx);(f,p,server,pump)
}
#[test]
fn terminal_frames_match_host_and_incremental_decoder(){
    let a=terminal::encode(terminal::OPEN,7,br#"{"cols":80,"rows":24}"#);
    assert_eq!(a,crate::terminal::encode(crate::terminal::OPEN,7,br#"{"cols":80,"rows":24}"#));
    let mut d=terminal::Decoder::default();let b=[a.clone(),terminal::encode(terminal::STDOUT,7,b"synthetic")].concat();let mut frames=Vec::new();
    for part in b.chunks(3){frames.extend(d.push(part).unwrap());}assert_eq!(frames.len(),2);assert_eq!(frames[1].payload,b"synthetic");
    assert!(terminal::Decoder::default().push(b"TERM\x02\x01\0\0\0\0\0\0\0\0\0\0").is_err());
    let mut bad=a;bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());assert!(terminal::Decoder::default().push(&bad).is_err());
}
#[tokio::test]
async fn terminal_rpc_list_open_io_and_errors(){
    let(io,rx,input,mut out)=pair();let(t,mut events)=terminal::Terminal::new(io,rx);
    let server=tokio::spawn(async move{
        let(tx,mut replies)=mpsc::unbounded_channel();let host=crate::terminal::TerminalConn::new(tx);
        while let Some((c,b))=out.recv().await{
            if c==CONTROL{let r=crate::terminal::rpc_response(&decode_wire(blob(&b,21).unwrap()).unwrap()).unwrap();input.send((CONTROL,wire::envelope(1,22,&r))).await.unwrap();continue}
            assert_eq!(c,BINARY);let f=crate::terminal::decode(&b).unwrap();
            match f.kind{
                terminal::LIST_REQ=>{host.handle(&b);input.send((BINARY,replies.recv().await.unwrap())).await.unwrap();},
                terminal::OPEN=>{let v:serde_json::Value=serde_json::from_slice(&f.payload).unwrap();assert_eq!(v["cols"],91);input.send((BINARY,terminal::encode(terminal::OPEN_RESP,37,br#"{"code":0}"#))).await.unwrap();},
                terminal::STDIN=>input.send((BINARY,terminal::encode(terminal::STDOUT,f.session,&f.payload))).await.unwrap(),
                terminal::RESIZE=>assert_eq!(serde_json::from_slice::<serde_json::Value>(&f.payload).unwrap()["rows"],40),
                terminal::KILL_REQ=>input.send((BINARY,terminal::encode(terminal::KILL_RESP,f.session,br#"{"code":1}"#))).await.unwrap(),
                terminal::CLOSE=>break,_=>panic!(),
            }
        }
    });
    t.check_env().await.unwrap();let _sessions=t.list_sessions().await.unwrap();let id=t.open(91,30).await.unwrap();
    let data=vec![42;50_000];t.write(id,&data).await.unwrap();let mut received=Vec::new();
    while received.len()<data.len(){match tokio::time::timeout(TIMEOUT,events.recv()).await.unwrap().unwrap(){terminal::Event::Output{data,..}=>{assert!(data.len()<=16*1024);received.extend(data)},_=>panic!()}}
    assert_eq!(received,data);t.resize(id,100,40).await.unwrap();assert!(t.kill(id).await.is_err());t.close(id).await.unwrap();server.await.unwrap();
}
#[tokio::test]
async fn file_host_roundtrip_multiblock_empty_mkdir_and_errors(){
    let tmp=Temp::new();let(f,mut progress,server,pump)=host_files();
    let remote=tmp.0.join("remote");f.mkdir(remote.to_str().unwrap()).await.unwrap();
    let source=tmp.0.join("source");let data:Vec<u8>=(0..130_111).map(|i|(i%251)as u8).collect();std::fs::write(&source,&data).unwrap();
    let dest=remote.join("uploaded");f.upload(&source,dest.to_str().unwrap()).await.unwrap();assert_eq!(std::fs::read(&dest).unwrap(),data);
    let entries=f.list_dir(remote.to_str().unwrap()).await.unwrap();assert_eq!(entries.len(),1);assert_eq!(entries[0].size,data.len()as u64);
    let downloaded=tmp.0.join("downloaded");f.download(dest.to_str().unwrap(),&downloaded).await.unwrap();assert_eq!(std::fs::read(downloaded).unwrap(),data);
    let empty=tmp.0.join("empty");std::fs::write(&empty,[]).unwrap();f.upload(&empty,remote.join("empty").to_str().unwrap()).await.unwrap();
    let copy=tmp.0.join("empty-copy");f.download(empty.to_str().unwrap(),&copy).await.unwrap();assert_eq!(std::fs::metadata(copy).unwrap().len(),0);
    let folder_copy=tmp.0.join("folder-copy");f.download(remote.to_str().unwrap(),&folder_copy).await.unwrap();assert_eq!(std::fs::read(folder_copy.join("uploaded")).unwrap(),data);
    assert!(f.list_dir(tmp.0.join("missing").to_str().unwrap()).await.unwrap_err().contains("9"));
    assert!(f.download(tmp.0.join("missing").to_str().unwrap(),tmp.0.join("absent")).await.unwrap_err().contains("9"));
    assert!(progress.try_recv().is_ok());server.abort();pump.abort();
}
#[test]
fn directory_expansion_and_invalid_compression(){
    use std::io::Write;let mut z=flate2::write::ZlibEncoder::new(Vec::new(),flate2::Compression::fast());z.write_all(&nested(1,b"synthetic")).unwrap();let data=z.finish().unwrap();assert_eq!(files::inflate(&data).unwrap(),nested(1,b"synthetic"));assert!(files::inflate(b"invalid").is_err());
}
#[tokio::test]
async fn files_cancel_wakes_pending_request(){
    let(io,rx,_input,mut out)=pair();let(f,_)=files::Files::new(io,rx);
    let op=f.list_dir("synthetic");tokio::pin!(op);
    tokio::select!{r=&mut op=>panic!("unexpected {r:?}"),_=out.recv()=>{}};
    f.cancel();assert!(tokio::time::timeout(Duration::from_secs(1),op).await.unwrap().unwrap_err().contains("canceled"));
}
#[tokio::test]
async fn forwarding_mock_loopback_echo_fragmentation_remove_and_reject(){
    let target=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let port=target.local_addr().unwrap().port();
    let echo=tokio::spawn(async move{let(mut s,_)=target.accept().await.unwrap();let mut b=vec![0;4096];loop{let n=s.read(&mut b).await.unwrap();if n==0{break}s.write_all(&b[..n]).await.unwrap();}});
    let(io,rx,input,mut out)=pair();let p=portmap::PortMap::new(io,rx);
    let host=tokio::spawn(async move{
        let mut tcp:Option<tokio::net::TcpStream>=None;
        while let Some((c,b))=out.recv().await{
            assert_eq!(c,FILE);let f=blob(&b,27).unwrap();let(s,r,t)=(uint(f,1),uint(f,2),uint(f,3));
            match uint(f,4){
                0=>{let v:serde_json::Value=serde_json::from_slice(blob(f,5).unwrap()).unwrap();assert_eq!(v["version"],1);let target=v["target_port"].as_u64().unwrap()as u16;
                    let connected=if target==port {tokio::net::TcpStream::connect(("127.0.0.1",target)).await.ok()}else{None};let ok=connected.is_some();tcp=connected;
                    input.send((FILE,portmap::frame(s,r,t,3,if ok{br#"{"version":1,"ok":true}"#}else{br#"{"version":1,"ok":false}"#}))).await.unwrap();},
                2=>{let payload=blob(f,5).unwrap();assert!(payload.len()<=16*1024);let socket=tcp.as_mut().unwrap();socket.write_all(payload).await.unwrap();let mut b=vec![0;payload.len()];socket.read_exact(&mut b).await.unwrap();input.send((FILE,portmap::frame(s,r,t,2,&b))).await.unwrap();},
                1=>{tcp.take();},_=>panic!(),
            }
        }
    });
    let id=p.add_rule("echo",0,"127.0.0.1",port).await.unwrap();let local=p.status()[0].local_port;
    let mut socket=tokio::net::TcpStream::connect(("127.0.0.1",local)).await.unwrap();let bytes=vec![91;100_000];socket.write_all(&bytes).await.unwrap();let mut received=vec![0;bytes.len()];tokio::time::timeout(TIMEOUT,socket.read_exact(&mut received)).await.unwrap().unwrap();assert_eq!(received,bytes);
    p.remove_rule(id).await.unwrap();assert!(p.status().is_empty());assert!(tokio::net::TcpStream::connect(("127.0.0.1",local)).await.is_err());
    let id=p.add_rule("reject",0,"127.0.0.1",1).await.unwrap();let mut socket=tokio::net::TcpStream::connect(("127.0.0.1",p.status()[0].local_port)).await.unwrap();assert_eq!(tokio::time::timeout(TIMEOUT,socket.read(&mut [0])).await.unwrap().unwrap(),0);
    assert_eq!(p.status()[0].failures,1);p.remove_rule(id).await.unwrap();host.abort();echo.await.unwrap();
}
#[tokio::test]
async fn file_download_reorders_blocks_and_rejects_traversal(){
    for (unsafe_name,one_based) in [(false,true),(false,false),(true,false)]{
        let tmp=Temp::new();let(io,rx,input,mut out)=pair();let(f,_)=files::Files::new(io,rx);
        let host=tokio::spawn(async move{
            let(_,b)=out.recv().await.unwrap();let r=blob(&b,21).unwrap();let rid=uint(blob(r,1).unwrap(),1);let req=blob(blob(r,8).unwrap(),2).unwrap();let task=uint(blob(req,1).unwrap(),1);
            let mut info=nested(1,if unsafe_name{b"../escape"}else{b"file"});wire::int(2,6,&mut info);
            let mut response=nested(1,&ints(&[(1,task)]));wire::int(2,1,&mut response);wire::bytes(3,&info,&mut response);
            input.send((FILE,rpc(rid,true,2,&response))).await.unwrap();
            if unsafe_name{
                // Let the client reject an ask, then close the host task normally.
                let mut b=nested(1,&ints(&[(1,task),(2,1)]));wire::int(3,6,&mut b);input.send((FILE,rpc(900,false,3,&b))).await.unwrap();
                let(_,reply)=out.recv().await.unwrap();assert_eq!(uint(blob(blob(blob(&reply,22).unwrap(),4).unwrap(),3).unwrap(),3),7);
            }else{
                let mut b=nested(1,&ints(&[(1,task),(2,1)]));wire::int(3,6,&mut b);input.send((FILE,rpc(900,false,3,&b))).await.unwrap();out.recv().await.unwrap();
                // Official Windows host numbering starts at 1 (base fixed at completion); our host sends 0-based out of order.
                let blocks:&[(u64,&[u8;2])]=if one_based {&[(1,b"ab"),(3,b"ef"),(2,b"cd")]} else {&[(2,b"ef"),(0,b"ab"),(1,b"cd")]};
                for &(block,data) in blocks {let mut b=nested(1,&ints(&[(1,task),(2,1)]));wire::int(2,block,&mut b);wire::bytes(3,data,&mut b);input.send((FILE,rpc(901+block,false,4,&b))).await.unwrap();out.recv().await.unwrap();}
                let mut b=nested(1,&ints(&[(1,task),(2,1)]));wire::int(2,1,&mut b);input.send((FILE,rpc(903,false,9,&b))).await.unwrap();out.recv().await.unwrap();
            }
            let mut b=nested(1,&ints(&[(1,task),(2,u64::MAX)]));wire::int(2,1,&mut b);input.send((FILE,rpc(904,false,9,&b))).await.unwrap();
        });
        let target=tmp.0.join("received");let result=f.download("synthetic",&target).await;
        if unsafe_name{assert!(result.unwrap_err().contains("Unsafe"));assert!(!tmp.0.join("escape").exists());assert!(!target.exists());}else{result.unwrap();assert_eq!(std::fs::read(target).unwrap(),b"abcdef");}
        host.await.unwrap();
    }
}
#[tokio::test]
async fn cancel_download_rejects_host_and_removes_staging(){
    let tmp=Temp::new();let(io,rx,input,mut out)=pair();let(f,_)=files::Files::new(io,rx);let(asked,ready)=tokio::sync::oneshot::channel();
    let host=tokio::spawn(async move{
        let(_,b)=out.recv().await.unwrap();let r=blob(&b,21).unwrap();let rid=uint(blob(r,1).unwrap(),1);let req=blob(blob(r,8).unwrap(),2).unwrap();let task=uint(blob(req,1).unwrap(),1);
        let mut info=nested(1,b"file");wire::int(2,10,&mut info);let mut b=nested(1,&ints(&[(1,task)]));wire::int(2,1,&mut b);wire::bytes(3,&info,&mut b);
        input.send((FILE,rpc(rid,true,2,&b))).await.unwrap();let _=asked.send(());
        let mut b=nested(1,&ints(&[(1,task),(2,1)]));wire::int(3,10,&mut b);input.send((FILE,rpc(900,false,3,&b))).await.unwrap();
        let(_,reply)=out.recv().await.unwrap();let code=uint(blob(blob(blob(&reply,22).unwrap(),4).unwrap(),3).unwrap(),3);
        if code==1{
            let mut b=nested(1,&ints(&[(1,task),(2,1)]));wire::int(2,0,&mut b);wire::bytes(3,b"synthetic!",&mut b);input.send((FILE,rpc(901,false,4,&b))).await.unwrap();let(_,r)=out.recv().await.unwrap();assert_eq!(uint(blob(blob(blob(&r,22).unwrap(),4).unwrap(),4).unwrap(),3),7);
        }else{assert_eq!(code,7);}
        let mut b=nested(1,&ints(&[(1,task),(2,u64::MAX)]));wire::int(2,7,&mut b);input.send((FILE,rpc(902,false,9,&b))).await.unwrap();
    });
    let target=tmp.0.join("received");let op=f.download("synthetic",&target);tokio::pin!(op);
    tokio::select!{r=&mut op=>panic!("unexpected {r:?}"),_=ready=>{}}f.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(4),op).await.unwrap().unwrap_err().contains("canceled"));host.await.unwrap();assert!(!target.exists());assert_eq!(std::fs::read_dir(&tmp.0).unwrap().count(),0);
}
#[tokio::test]
async fn forwarding_transport_close_stops_listener(){
    let(io,rx,input,_out)=pair();let p=portmap::PortMap::new(io,rx);let id=p.add_rule("closed",0,"127.0.0.1",1).await.unwrap();let port=p.status()[0].local_port;
    drop(input);tokio::time::timeout(Duration::from_secs(1),p.closed()).await.unwrap();
    p.remove_rule(id).await.unwrap();assert!(tokio::net::TcpStream::connect(("127.0.0.1",port)).await.is_err());
    assert!(p.add_rule("unavailable",0,"127.0.0.1",1).await.is_err());
}

#[test]
fn terminal_metadata_is_allowlisted_and_sorted(){
    let rows=terminal::session_rows(&[
        serde_json::json!({"session_id":9,"shell":"/bin/sh\tX\nY\0","state":"detached","created_at_ms":123,"output":"PRIVATE","name":"PRIVATE"}),
        serde_json::json!({"session_id":2,"shell":"/bin/bash","state":"running"}),
        serde_json::json!({"session_id":0}),serde_json::json!({"session_id":4294967296u64}),
    ]);
    assert_eq!(rows,format!("2\t/bin/bash\t0\t{}\n9\t/bin/sh X Y \t123\t{}\n",crate::i18n::s("运行中","Running"),crate::i18n::s("已分离","Detached")));
    assert!(!rows.contains("PRIVATE"));assert!(terminal::session_rows(&[]).is_empty());
}
#[tokio::test]
async fn terminal_attach_uses_existing_id_and_rejects_absent_session(){
    let(io,rx,input,mut out)=pair();let(t,_events)=terminal::Terminal::new(io,rx);
    let host=tokio::spawn(async move{
        for id in [37,99]{
            let(c,b)=out.recv().await.unwrap();assert_eq!(c,BINARY);
            let f=crate::terminal::decode(&b).unwrap();assert_eq!(f.kind,terminal::LIST_REQ);
            input.send((BINARY,terminal::encode(terminal::LIST_RESP,f.session,br#"[{"session_id":37}]"#))).await.unwrap();
            if id==37{
                let(_,b)=out.recv().await.unwrap();let f=crate::terminal::decode(&b).unwrap();
                assert_eq!(f.kind,terminal::OPEN);assert_eq!(f.session,37);
                let v:serde_json::Value=serde_json::from_slice(&f.payload).unwrap();assert_eq!(v["cols"],90);assert_eq!(v["rows"],30);
                input.send((BINARY,terminal::encode(terminal::OPEN_RESP,37,br#"{"code":0}"#))).await.unwrap();
            }
        }
        assert!(out.try_recv().is_err());
    });
    assert_eq!(t.attach(37,90,30).await.unwrap(),37);
    assert!(t.attach(99,90,30).await.is_err());assert!(t.attach(0,90,30).await.is_err());host.await.unwrap();
}

#[tokio::test]
async fn terminal_kill_acknowledgement_and_empty_refresh(){
    let(io,rx,input,mut out)=pair();let(t,_events)=terminal::Terminal::new(io,rx);
    let host=tokio::spawn(async move{
        let(c,b)=out.recv().await.unwrap();assert_eq!(c,BINARY);
        let f=crate::terminal::decode(&b).unwrap();assert_eq!(f.kind,terminal::KILL_REQ);assert_eq!(f.session,42);
        input.send((BINARY,terminal::encode(terminal::KILL_RESP,42,br#"{"code":0}"#))).await.unwrap();
        let(_,b)=out.recv().await.unwrap();let f=crate::terminal::decode(&b).unwrap();assert_eq!(f.kind,terminal::LIST_REQ);
        input.send((BINARY,terminal::encode(terminal::LIST_RESP,f.session,b"[]"))).await.unwrap();
    });
    t.kill(42).await.unwrap();assert!(t.list_sessions().await.unwrap().is_empty());host.await.unwrap();
}
