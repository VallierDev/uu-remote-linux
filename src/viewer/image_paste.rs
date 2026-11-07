//! On-demand PNG reads and the paste barrier. No temporary files or remote tools.
use std::{process::Stdio,time::Duration};
use tokio::{io::AsyncReadExt,sync::{mpsc,oneshot}};
use crate::clipboard_rich::{MAX_IMAGE,valid_png};

pub struct Request {pub data:Vec<u8>,pub selection:u64,pub reply:oneshot::Sender<Result<(),()>>}
pub type Sender=mpsc::Sender<Request>;
#[derive(Clone,Copy)]
pub enum Reader {X11,Wayland,Unavailable}
impl Reader {
    pub fn for_driver(driver:&str)->Self {match driver {"x11"=>Self::X11,"wayland"=>Self::Wayland,_=>Self::Unavailable}}
    async fn read(self)->Result<Option<Vec<u8>>,()> {
        match self {
            Self::X11=>tokio::task::spawn_blocking(read_x11).await.map_err(|_|())?,
            // SDL2 has no arbitrary-MIME API. On native Wayland request PNG once,
            // only for an explicit paste; never poll wl-paste (GNOME issue #37).
            Self::Wayland=>read_wayland(std::ffi::OsStr::new("wl-paste")).await,
            Self::Unavailable=>Ok(None),
        }
    }
}
#[cfg(all(target_os="linux",feature="viewer-sdl"))]
fn read_x11()->Result<Option<Vec<u8>>,()> {
    extern "C" {fn uu_viewer_clipboard_png(out:*mut *mut u8,length:*mut usize,limit:usize)->libc::c_int;}
    let mut ptr=std::ptr::null_mut();let mut length=0;
    // The worker owns its Display and an unmapped requestor; malloc output is bounded.
    let status=unsafe{uu_viewer_clipboard_png(&mut ptr,&mut length,MAX_IMAGE)};
    let result=match status {
        0=>Ok(None),
        1 if !ptr.is_null() && length<=MAX_IMAGE=>Ok(Some(unsafe{std::slice::from_raw_parts(ptr,length)}.to_vec())),
        _=>Err(()),
    };
    unsafe{libc::free(ptr.cast());}result
}
#[cfg(not(all(target_os="linux",feature="viewer-sdl")))]
fn read_x11()->Result<Option<Vec<u8>>,()> {Ok(None)}
async fn read_wayland(program:&std::ffi::OsStr)->Result<Option<Vec<u8>>,()> {
    let mut child=match tokio::process::Command::new(program).args(["--no-newline","--type","image/png"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn(){
            Ok(child)=>child,
            // An optional MIME reader must not disable SDL's existing text paste.
            Err(e) if matches!(e.kind(),std::io::ErrorKind::NotFound|std::io::ErrorKind::PermissionDenied)=>return Ok(None),
            Err(_)=>return Err(()),
        };
    tokio::time::timeout(Duration::from_secs(5),async{
        let mut data=Vec::new();child.stdout.take().ok_or(())?.take((MAX_IMAGE+1) as u64).read_to_end(&mut data).await.map_err(|_|())?;
        if data.len()>MAX_IMAGE {let _=child.kill().await;return Err(())}
        if !child.wait().await.map_err(|_|())?.success(){return Ok(None)}
        Ok(Some(data))
    }).await.map_err(|_|())?
}
/// false: no PNG; preserve the ordinary text/key path. An error suppresses paste,
/// so a failed image transfer can never paste the previous remote clipboard.
pub async fn prepare(reader:Reader,tx:Sender,selection:u64)->Result<bool,()> {
    let Some(data)=reader.read().await? else{return Ok(false)};
    prepare_data(data,tx,selection).await?;Ok(true)
}
async fn prepare_data(data:Vec<u8>,tx:Sender,selection:u64)->Result<(),()> {
    if !valid_png(&data){return Err(())}
    let(reply,rx)=oneshot::channel();
    tokio::time::timeout(Duration::from_secs(20),async{
        tx.send(Request{data,selection,reply}).await.map_err(|_|())?;
        rx.await.map_err(|_|())?
    }).await.map_err(|_|())?
}

#[cfg(test)] mod tests {
    use super::*;
    #[tokio::test] async fn absent_optional_wayland_reader_preserves_normal_paste(){
        let path=std::env::temp_dir().join(format!("uu-missing-png-reader-{}",super::super::wire::uuid()));
        assert_eq!(read_wayland(path.as_os_str()).await,Ok(None));
        let(tx,mut requests)=mpsc::channel(1);
        assert_eq!(prepare(Reader::Unavailable,tx,7).await,Ok(false));
        assert!(requests.try_recv().is_err());
    }
    #[tokio::test] async fn paste_waits_for_ack_and_rejects_corrupt_images(){
        let(tx,mut requests)=mpsc::channel(1);
        assert!(prepare_data(vec![0;40],tx.clone(),0).await.is_err());
        assert!(requests.try_recv().is_err());
        let task=tokio::spawn(prepare_data(include_bytes!("../../tests/fixtures/clipboard-test.png").to_vec(),tx,0));
        let request=requests.recv().await.unwrap();assert!(!task.is_finished());
        request.reply.send(Ok(())).unwrap();assert_eq!(task.await.unwrap(),Ok(()));
    }
    #[tokio::test] async fn rejection_and_closed_worker_do_not_release_paste(){
        let(tx,mut requests)=mpsc::channel(1);
        let task=tokio::spawn(prepare_data(include_bytes!("../../tests/fixtures/clipboard-test.png").to_vec(),tx.clone(),0));
        requests.recv().await.unwrap().reply.send(Err(())).unwrap();assert!(task.await.unwrap().is_err());
        drop(requests);assert!(prepare_data(include_bytes!("../../tests/fixtures/clipboard-test.png").to_vec(),tx,0).await.is_err());
    }
}
