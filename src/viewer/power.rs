//! SimpleAction numbers are from src/system_actions.rs (REBOOT_SYSTEM / SHUTDOWN_SYSTEM).
use super::{session::Session,wire};
use std::{sync::atomic::Ordering,time::Duration};
#[derive(Clone,Copy,Debug,PartialEq)]
pub enum Action {Reboot,Shutdown}
impl Action {
    pub fn parse(value:&str)->Result<Self,String>{match value{"reboot"=>Ok(Self::Reboot),"shutdown"=>Ok(Self::Shutdown),_=>Err("Power action must be reboot or shutdown".into())}}
    pub fn message(self,seq:u64)->Vec<u8>{
        let mut action=Vec::new();wire::int(1,match self{Self::Reboot=>9,Self::Shutdown=>12},&mut action);
        wire::envelope(seq,3,&action)
    }
}
pub async fn run(session:&mut Session,action:Action)->Result<(),String>{
    let channel=session.peer.channels[0].clone();
    let ready=async{
        loop {
            if session.peer.observed.control_open.load(Ordering::Acquire) && session.peer.observed.echo_received.load(Ordering::Acquire){return Ok::<_,String>(())}
            tokio::select!{r=session.step()=>r?,_=tokio::time::sleep(Duration::from_millis(20))=>{}}
        }
    };
    let send=async move {
        tokio::time::timeout(Duration::from_secs(5),channel.send(&bytes::Bytes::from(action.message(2))))
            .await.map_err(|_|"Power command send timed out")?.map_err(|_|"Power command send failed")?;
        // Drain SCTP locally before closing; this is not a remote execution acknowledgement.
        let drain=async{while channel.buffered_amount().await>0 {tokio::time::sleep(Duration::from_millis(20)).await;}};
        tokio::time::timeout(Duration::from_secs(2),drain).await.map_err(|_|"Power command drain timed out")?;
        tokio::time::sleep(Duration::from_millis(200)).await;Ok(())
    };
    after_echo(ready,send,Duration::from_secs(30)).await
}
async fn after_echo(ready:impl std::future::Future<Output=Result<(),String>>,send:impl std::future::Future<Output=Result<(),String>>,timeout:Duration)->Result<(),String>{
    tokio::time::timeout(timeout,ready).await.map_err(|_|"CONTROL echo timed out")??;
    send.await
}

#[cfg(test)] mod tests {
    use super::*;
    use crate::protocol::{decode_wire,WireValue};
    #[test] fn only_host_evidenced_actions_are_encoded(){
        for (name,number) in [("reboot",9),("shutdown",12)]{
            let bytes=Action::parse(name).unwrap().message(2);let fields=decode_wire(&bytes).unwrap();
            let WireValue::Bytes(body)=fields.iter().find(|f|f.number==3).unwrap().value else{panic!()};
            assert_eq!(decode_wire(body).unwrap()[0].value,WireValue::Varint(number));
            let host=include_str!("../system_actions.rs");
            assert!(host.contains(&format!("pub const {}_SYSTEM: u64 = {number};",name.to_uppercase())));
        }
        for value in ["logout","wol","9",""]{assert!(Action::parse(value).is_err());}
    }
    #[tokio::test] async fn waits_for_echo_and_sends_once_without_retrying_failure(){
        use std::sync::{Arc,atomic::AtomicUsize};
        for fail in [false,true]{
            let calls=Arc::new(AtomicUsize::new(0));let recorded=calls.clone();
            let(tx,rx)=tokio::sync::oneshot::channel();
            let operation=tokio::spawn(after_echo(async{rx.await.map_err(|_|"closed".into())},async move{
                recorded.fetch_add(1,Ordering::Relaxed);if fail{Err("send failed".into())}else{Ok(())}
            },Duration::from_secs(1)));
            tokio::task::yield_now().await;assert_eq!(calls.load(Ordering::Relaxed),0);
            tx.send(()).unwrap();assert_eq!(operation.await.unwrap().is_err(),fail);
            assert_eq!(calls.load(Ordering::Relaxed),1);
        }
    }
    #[tokio::test] async fn failed_or_missing_echo_never_sends(){
        for failed in [false,true]{
            let called=std::sync::atomic::AtomicBool::new(false);
            let result=after_echo(async{if failed{return Err("connection closed".into())}std::future::pending().await},async{called.store(true,Ordering::Relaxed);Ok(())},Duration::from_millis(5)).await;
            assert!(result.is_err());assert!(!called.load(Ordering::Relaxed));
        }
    }

}
