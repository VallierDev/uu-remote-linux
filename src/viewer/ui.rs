//! Launcher window (`uuremote-viewer ui`): QR sign-in, device list, and one child process per
//! remote session so each SDL window is isolated. Account data never reaches the logs.
use serde_json::Value;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::time::{Duration, Instant, SystemTime};

extern "C" {
    fn uu_viewer_ui_start() -> c_int;
    fn uu_viewer_ui_set_devices(tsv: *const c_char);
    fn uu_viewer_ui_set_login(signed_in: c_int, qr_png: *const c_char, status: *const c_char, user: *const c_char);
    fn uu_viewer_ui_take_command(out: *mut c_char, len: usize) -> c_int;
    fn uu_viewer_ui_closed() -> c_int;
    fn uu_viewer_ui_confirm_replace() -> c_int;
    fn uu_viewer_ui_toast(text: *const c_char);
}
fn toast(text: &str) { let t = c(text); unsafe { uu_viewer_ui_toast(t.as_ptr()) } }

/// Runs one session child; on failure shows its last error line (log lines start with '[').
fn watch_session(mut child: tokio::process::Child, mut cancel: tokio::sync::oneshot::Receiver<()>, success: Option<&'static str>) -> tokio::task::JoinHandle<()> {
    use tokio::io::AsyncBufReadExt;
    let stderr = child.stderr.take();
    tokio::spawn(async move {
        let mut last = String::new();
        let mut lines = stderr.map(|e| tokio::io::BufReader::new(e).lines());
        let status = loop {
            tokio::select! {
                _ = &mut cancel => {
                    if let Some(id)=child.id(){unsafe{libc::kill(id as i32,libc::SIGTERM);}}
                    if tokio::time::timeout(Duration::from_secs(6),child.wait()).await.is_err(){let _=child.kill().await;}return;
                }
                status = child.wait() => break status,
                line = async { match lines.as_mut() { Some(l) => l.next_line().await, None => std::future::pending().await } } => match line {
                    Ok(Some(l)) => { if !l.starts_with('[') && !l.trim().is_empty() { last = l.chars().take(160).collect(); } }
                    _ => lines = None,
                },
            }
        };
        // wait() may win before the final stderr line is read.
        let _ = tokio::time::timeout(Duration::from_millis(200), async {
            if let Some(lines) = lines.as_mut() {
                while let Ok(Some(l)) = lines.next_line().await {
                    if !l.starts_with('[') && !l.trim().is_empty() { last = l.chars().take(160).collect(); }
                }
            }
        }).await;
        if !status.is_ok_and(|s| s.success()) {
            let reason = match last.as_str() {
                "" => "连接已断开".to_owned(),
                "CONTROL echo timed out" => "连接超时：被控端没有响应（官方主机互通尚未完成）".to_owned(),
                "Remote session released" => "被控端结束了本次连接".to_owned(),
                other => other.to_owned(),
            };
            toast(&format!("连接失败：{reason}"));
        } else if let Some(message) = success { toast(message); }
    })
}

struct Running { cancel:tokio::sync::oneshot::Sender<()>, task:tokio::task::JoinHandle<()>, input:Option<tokio::process::ChildStdin> }
impl Running { async fn stop(self){let _=self.cancel.send(());let _=self.task.await;} }
async fn replace(sessions:&mut std::collections::HashMap<String,Running>,id:&str)->bool{
    if sessions.get(id).is_some_and(|s|!s.task.is_finished()) && unsafe{uu_viewer_ui_confirm_replace()}!=1{return false}
    if let Some(old)=sessions.remove(id){old.stop().await;}true
}

fn c(text: &str) -> CString { CString::new(text.replace('\0', "")).unwrap_or_default() }
fn set_login(signed_in: bool, qr: &str, status: &str, user: &str) {
    let (q, s, u) = (c(qr), c(status), c(user));
    unsafe { uu_viewer_ui_set_login(signed_in as c_int, q.as_ptr(), s.as_ptr(), u.as_ptr()) }
}
fn take() -> Option<String> {
    let mut buf = [0 as c_char; 200];
    (unsafe { uu_viewer_ui_take_command(buf.as_mut_ptr(), buf.len()) } != 0)
        .then(|| unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}
/// "id\tname\tplatform\tonline\tcurrent" lines; tabs and newlines in names are flattened.
pub fn device_rows(devices: &[Value], current: Option<&str>) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    devices.iter().filter_map(|d| {
        let id = d.get("device_id")?.as_str()?;
        let name = d.get("alias").and_then(Value::as_str).unwrap_or(id);
        let platform = d.get("platform").and_then(Value::as_i64).unwrap_or(0);
        let online = d.get("status").and_then(Value::as_str) == Some("CONNECTED");
        Some(format!("{}\t{}\t{}\t{}\t{}\n", clean(id), clean(name), platform, online as u8, (current == Some(id)) as u8))
    }).collect()
}

pub async fn run(credentials: impl Fn() -> Result<crate::auth::Credentials, String>) -> Result<(), String> {
    if unsafe { uu_viewer_ui_start() } != 0 { return Err("Cannot open the window (GTK3 and a graphical session are required)".into()); }
    let exe = std::env::current_exe().map_err(|_| "Cannot locate executable")?;
    let qr = std::env::temp_dir().join(format!("uu-viewer-qr-{}.png", std::process::id()));
    let mut login: Option<tokio::process::Child> = None;
    let mut qr_seen: Option<SystemTime> = None;
    let mut next_refresh = Instant::now();
    let mut sessions:std::collections::HashMap<String,Running>=Default::default();
    let mut names: std::collections::HashMap<String, String> = Default::default();
    let refresh = |creds: &crate::auth::Credentials| {
        let api = super::api::Api::new(crate::auth::UUAuthClient::new(creds.clone()), crate::auth::BASE_URL);
        async move { api.device_list().await }
    };
    loop {
        if unsafe { uu_viewer_ui_closed() } != 0 { break; }
        sessions.retain(|_,s| !s.task.is_finished());
        if let Some(child) = login.as_mut() {
            if let Ok(meta) = std::fs::metadata(&qr) {
                if meta.modified().ok() != qr_seen { qr_seen = meta.modified().ok(); set_login(false, &qr.to_string_lossy(), "请用手机扫码并确认", ""); }
            }
            if let Ok(Some(_)) = child.try_wait() { login = None; let _ = std::fs::remove_file(&qr); next_refresh = Instant::now(); }
        } else if Instant::now() >= next_refresh {
            next_refresh = Instant::now() + Duration::from_secs(30);
            match credentials() {
                Ok(creds) => {
                    set_login(true, "", "", "已登录");
                    match refresh(&creds).await {
                        Ok(data) => {
                            let list = data.get("my_binded_devices").and_then(Value::as_array).cloned().unwrap_or_default();
                            let current = data.pointer("/current_device/device_id").and_then(Value::as_str);
                            names = list.iter().filter_map(|d| Some((d.get("device_id")?.as_str()?.to_owned(),
                                d.get("alias").and_then(Value::as_str).unwrap_or("远端设备").to_owned()))).collect();
                            let rows = c(&device_rows(&list, current)); unsafe { uu_viewer_ui_set_devices(rows.as_ptr()) }
                        }
                        Err(super::api::Error::AuthenticationExpired) => set_login(false, "", "登录已过期，请重新登录", ""),
                        Err(_) => {}
                    }
                }
                Err(_) => set_login(false, "", "尚未登录", ""),
            }
        }
        while let Some(cmd) = take() {
            let parts: Vec<&str> = cmd.split('\t').collect();
            match parts.as_slice() {
                ["quit"] => break,
                ["refresh"] => next_refresh = Instant::now(),
                ["logout"] => {
                    // Sign out locally: drop the account token, keep the device identity.
                    let path = crate::config::path();
                    if let Ok(mut v) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).ok_or(()) {
                        if let Some(o) = v.as_object_mut() { o.remove("token"); o.remove("csrf"); }
                        let _ = std::fs::write(&path, v.to_string());
                    }
                    for (_,s) in sessions.drain() { s.stop().await; }
                    unsafe { uu_viewer_ui_set_devices(c("").as_ptr()) }
                    set_login(false, "", "已退出登录", "");
                    next_refresh = Instant::now() + Duration::from_secs(3600);
                }
                ["login"] if login.is_none() => {
                    let server = exe.with_file_name("uuremote-server");
                    login = tokio::process::Command::new(server).args(["login", "--no-gui"])
                        .env("UU_LOGIN_QR_PNG", &qr).stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                        .kill_on_drop(true).spawn().ok();
                    if login.is_none() { set_login(false, "", "找不到配套的 uuremote-server", ""); }
                }
                ["connect", id, kind] if matches!(*kind, "desktop" | "watch") => {
                    if !replace(&mut sessions,id).await{continue}
                    // One process per session; "-" selects the built-in desktop connection shape.
                    let mut command=tokio::process::Command::new(&exe);
                    command.args(["connect", id, "false", "-", "echo", "legacy"]);
                    command.env("UU_VIEWER_DEVICE_NAME", super::device_label(names.get(*id).map(String::as_str)));
                    if *kind=="watch" {command.arg("--view-only");}
                    match command.env("UU_VIEWER_COMMANDS","1").stdin(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn() {
                        Ok(mut child) => { let input=child.stdin.take();let (cancel,rx)=tokio::sync::oneshot::channel();let task=watch_session(child,rx,None);sessions.insert((*id).into(),Running{cancel,task,input}); }
                        Err(_) => toast("连接失败：无法启动会话进程"),
                    }
                }
                ["assist", id, code] => {
                    match super::assist::command(&exe, id).spawn() {
                        Ok(mut child) => match super::assist::send_code(&mut child, code).await {
                            Ok(()) => { let (cancel,rx)=tokio::sync::oneshot::channel();let task=watch_session(child,rx,None);sessions.insert(format!("assist:{id}"),Running{cancel,task,input:None}); }
                            Err(reason) => toast(&format!("连接失败：{reason}")),
                        },
                        Err(_) => toast("连接失败：无法启动协助会话进程"),
                    }
                }
                ["connect", id, kind @ ("file" | "portmap" | "terminal")] => {
                    if *kind=="file" {if let Some(s)=sessions.get_mut(*id){if let Some(input)=s.input.as_mut(){
                        use tokio::io::AsyncWriteExt;
                        if input.write_all(b"files\n").await.is_err(){toast("桌面会话命令发送失败，请重试")}continue
                    }}}
                    if !replace(&mut sessions,id).await{continue}
                    let name = names.get(*id).cloned().unwrap_or_else(|| "远端设备".into());
                    let tool = if *kind == "file" { "files-ui" } else if *kind == "terminal" { "terminal-ui" } else { "portmap-ui" };
                    match tokio::process::Command::new(&exe).args([tool, id, &name])
                        .stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn() {
                        Ok(child) => { let (cancel,rx)=tokio::sync::oneshot::channel();let task=watch_session(child,rx,None);sessions.insert((*id).into(),Running{cancel,task,input:None}); }
                        Err(_) => toast("连接失败：无法启动工具进程"),
                    }
                }
                ["connect", id, kind @ ("reboot" | "shutdown")] => {
                    if !replace(&mut sessions,id).await{continue}
                    match tokio::process::Command::new(&exe).args(["power", id, kind])
                        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn() {
                        Ok(child) => { let (cancel,rx)=tokio::sync::oneshot::channel();let task=watch_session(child,rx,Some("电源指令已发送，未确认远端执行结果"));sessions.insert((*id).into(),Running{cancel,task,input:None}); }
                        Err(_) => toast("电源指令失败：无法启动会话进程"),
                    }
                }
                ["connect", _, kind] => log::info!("Viewer UI: {kind} is not supported yet"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for (_,s) in sessions {s.stop().await;}
    Ok(())
}

#[cfg(test)] mod tests {
    #[test] fn device_rows_flatten_names_and_mark_online() {
        let rows = super::device_rows(&[
            serde_json::json!({"device_id":"d1","alias":"a\tb","platform":4,"status":"CONNECTED"}),
            serde_json::json!({"device_id":"d2","platform":1,"status":"DISCONNECTED"}),
            serde_json::json!({"alias":"no id"}),
        ], Some("d2"));
        assert_eq!(rows, "d1\ta b\t4\t1\t0\nd2\td2\t1\t0\t1\n");
    }
}
