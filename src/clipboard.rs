//! Session-scoped native X11 text selection worker. No content is logged.
use tokio::sync::watch;

// Keep worst-case UTF-16 plus protobuf framing below webrtc0.10's65535-byte reader.
pub const MAX_TEXT: usize = 30 * 1024;

pub struct ClipboardBridge {
    pub incoming: watch::Sender<Option<String>>,
    pub outgoing: watch::Receiver<Option<String>>,
    pub ready: tokio::sync::oneshot::Receiver<bool>,
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
extern "C" {
    fn clipboard_open(display: *const libc::c_char) -> *mut libc::c_void;
    fn clipboard_set(handle: *mut libc::c_void, text: *const u8, length: usize) -> libc::c_int;
    fn clipboard_poll(handle: *mut libc::c_void, out: *mut u8, capacity: usize) -> libc::c_int;
    fn clipboard_close(handle: *mut libc::c_void);
}

pub fn start(display: String) -> ClipboardBridge {
    if display == "wayland:native" { return start_wayland(); }
    if display == "wayland:portal" || display.starts_with("kms:") {
        let (incoming,_)=watch::channel(None);let (_,outgoing)=watch::channel(None);
        let (sender,ready)=tokio::sync::oneshot::channel();let _=sender.send(false);
        return ClipboardBridge {incoming,outgoing,ready};
    }
    let (incoming, mut input) = watch::channel::<Option<String>>(None);
    let (output, outgoing) = watch::channel(None);
    let (ready_tx, ready) = tokio::sync::oneshot::channel();
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    tokio::task::spawn_blocking(move || {
        let Ok(display) = std::ffi::CString::new(display) else {
            let _ = ready_tx.send(false); return;
        };
        let handle = unsafe { clipboard_open(display.as_ptr()) };
        if handle.is_null() { let _ = ready_tx.send(false); return; }
        let _ = ready_tx.send(true);
        let mut buffer = vec![0; MAX_TEXT];
        while !output.is_closed() && input.has_changed().is_ok() {
            if input.has_changed().unwrap_or(false) {
                let text = input.borrow_and_update().clone();
                if let Some(text) = text {
                    if text.len() <= MAX_TEXT {
                        let result = unsafe { clipboard_set(handle, text.as_ptr(), text.len()) };
                        if result != 0 { log::warn!("Native clipboard ownership failed"); }
                    }
                }
            }
            let length = unsafe { clipboard_poll(handle, buffer.as_mut_ptr(), buffer.len()) };
            if length >= 0 {
                if let Ok(text) = std::str::from_utf8(&buffer[..length as usize]) {
                    output.send_replace(Some(text.to_owned()));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe { clipboard_close(handle) };
    });
    #[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
    {
        let _ = (display, input, output);
        let _ = ready_tx.send(false);
    }
    ClipboardBridge { incoming, outgoing, ready }
}

fn start_wayland() -> ClipboardBridge {
    use tokio::io::{AsyncReadExt,AsyncWriteExt};
    use std::process::Stdio;
    let (incoming,mut input)=watch::channel::<Option<String>>(None);
    let (output,outgoing)=watch::channel(None);
    let (ready_tx,ready)=tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        for program in ["wl-copy","wl-paste"] {
            let result=tokio::time::timeout(std::time::Duration::from_secs(2),
                tokio::process::Command::new(program).arg("--version").kill_on_drop(true)
                .stdout(Stdio::null()).stderr(Stdio::null()).status()).await;
            if !matches!(result,Ok(Ok(status)) if status.success()) { let _=ready_tx.send(false); return; }
        }
        let _=ready_tx.send(true);
        let mut owner:Option<tokio::process::Child>=None;
        let mut last=None;
        let mut tick=tokio::time::interval(std::time::Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _=output.closed()=>break,
                changed=input.changed()=>{
                    if changed.is_err() { break; }
                    let text=input.borrow_and_update().clone();
                    if let Some(text)=text.filter(|s|s.len()<=MAX_TEXT) {
                        if let Some(mut child)=owner.take() { let _=child.kill().await; }
                        if let Ok(mut child)=tokio::process::Command::new("wl-copy")
                            .args(["--foreground","--type","text/plain;charset=utf-8"])
                            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn() {
                            if let Some(mut stdin)=child.stdin.take() {
                                let written=tokio::time::timeout(std::time::Duration::from_secs(2),stdin.write_all(text.as_bytes())).await;
                                if matches!(written,Ok(Ok(()))) { last=Some(text); }
                            }
                            owner=Some(child);
                        }
                    }
                },
                _=tick.tick()=>{
                    // Bound both content and duration; never log clipboard text.
                    let read=async {
                        let mut child=tokio::process::Command::new("wl-paste").args(["--no-newline","--type","text"])
                            .stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().ok()?;
                        let mut bytes=Vec::new();
                        child.stdout.take()?.take((MAX_TEXT+1) as u64).read_to_end(&mut bytes).await.ok()?;
                        if bytes.len()>MAX_TEXT { return None; }
                        if !child.wait().await.ok()?.success() { return None; }
                        String::from_utf8(bytes).ok()
                    };
                    if let Ok(Some(text))=tokio::time::timeout(std::time::Duration::from_secs(2),read).await {
                        if last.as_ref()!=Some(&text) { last=Some(text.clone()); output.send_replace(Some(text)); }
                    }
                },
            }
        }
        if let Some(mut child)=owner { let _=child.kill().await; }
    });
    ClipboardBridge { incoming,outgoing,ready }
}

#[cfg(all(test,target_os="linux",feature="wayland"))]
mod wayland_acceptance {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use std::process::Stdio;
    #[tokio::test]
    #[ignore = "requires the isolated uu-hyprland-test desktop"]
    async fn native_clipboard_roundtrip() {
        assert!(std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap().trim()=="uu-hyprland-test",
            "Refusing to alter clipboard outside the isolated test VM");
        assert!(std::env::var_os("WAYLAND_DISPLAY").is_some());
        let mut bridge=start("wayland:native".into());
        assert!(tokio::time::timeout(std::time::Duration::from_secs(3),&mut bridge.ready).await.unwrap().unwrap());
        let incoming="UU Wayland test\n中文 ✓";
        bridge.incoming.send_replace(Some(incoming.into()));
        let received=tokio::time::timeout(std::time::Duration::from_secs(3),async {
            loop {
                use tokio::io::AsyncReadExt;
                let mut child=tokio::process::Command::new("wl-paste").args(["--no-newline","--type","text"])
                    .kill_on_drop(true).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
                let mut bytes=Vec::new();
                child.stdout.take().unwrap().take((MAX_TEXT+1) as u64).read_to_end(&mut bytes).await.unwrap();
                if bytes.len()<=MAX_TEXT && child.wait().await.unwrap().success() && bytes==incoming.as_bytes() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }).await;
        assert!(received.is_ok(),"Native clipboard did not receive inbound text");
        let mut owner=tokio::process::Command::new("wl-copy").args(["--foreground","--type","text/plain;charset=utf-8"])
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
        let external="UU native outbound test";
        let mut pipe=owner.stdin.take().unwrap(); pipe.write_all(external.as_bytes()).await.unwrap(); drop(pipe);
        let changed=tokio::time::timeout(std::time::Duration::from_secs(3),async {
            loop {
                bridge.outgoing.changed().await.unwrap();
                if bridge.outgoing.borrow_and_update().as_deref()==Some(external) { break; }
            }
        }).await;
        assert!(changed.is_ok(),"Bridge did not report native outbound text");
        drop(bridge);
        owner.kill().await.unwrap();
    }
}

/// Portal Clipboard shares the authorized video/input Arc and survives native handle renewal.
pub fn start_portal(session: std::sync::Arc<crate::portal::PortalSession>) -> ClipboardBridge {
    let (incoming,mut input)=watch::channel::<Option<String>>(None);
    let (output,outgoing)=watch::channel(None);
    let (ready_tx,ready)=tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        // GNOME closes the native portal while locked. A desktop connection
        // starts its TEXT channel before CONTROL auto-unlock/capture recovery.
        // Wait for that same Arc to recover instead of permanently disabling
        // clipboard on an already authorized but temporarily closed session.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !session.is_alive() && std::time::Instant::now() < deadline {
            if output.is_closed() || input.has_changed().is_err() { return; }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let available=session.clipboard_ready(); let _=ready_tx.send(available);
        if !available { return; }
        let mut last=None; let mut pending=None;
        while !output.is_closed() && input.has_changed().is_ok() {
            if input.has_changed().unwrap_or(false) {
                pending=input.borrow_and_update().clone().filter(|text|text.len()<=MAX_TEXT);
            }
            if let Some(text)=pending.as_ref() {
                if session.clipboard_set(text).is_ok() { last=pending.take(); }
            }
            if let Some(text)=session.clipboard_poll() {
                if last.as_ref()!=Some(&text) { last=Some(text.clone()); output.send_replace(Some(text)); }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    ClipboardBridge {incoming,outgoing,ready}
}
