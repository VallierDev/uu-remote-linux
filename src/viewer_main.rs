//! Experimental controller session, built only with --features native-viewer.
#![allow(dead_code)]
#[path = "auth.rs"] mod auth;
#[path = "config.rs"] mod config;
#[path = "i18n.rs"] mod i18n;
#[path = "protocol.rs"] mod protocol;
mod ws_transport;
mod viewer;
#[cfg(test)] #[path="hevc.rs"] mod hevc;
use viewer::clipboard;
#[path="clipboard_protocol.rs"] mod clipboard_protocol;
#[path="clipboard_rich.rs"] mod clipboard_rich;
#[cfg(test)] #[path="terminal.rs"] mod terminal;
#[cfg(test)] #[path="file_trans.rs"] mod file_trans;
#[cfg(test)] #[path="initialization.rs"] mod initialization;
use viewer::{api::Api,rtc::ReadyPolicy,session::{Options,Session},wire::Shape};
use std::time::Duration;
const HELP:&str="Experimental native viewer.\n  uuremote-viewer ui   (GTK window: sign-in, devices, sessions) Desktop UI requires viewer-sdl (decodes with the ffmpeg CLI, or FFmpeg 6.x libraries with viewer-ffmpeg).\n  uuremote-viewer power DEVICE reboot|shutdown\n  uuremote-viewer type DEVICE [options]   (secret from UU_INPUT_ENV or stdin; reuses running viewer)\n    options: --delay-ms MS --omit-last N --snapshot PATH --dry-run --new-session\n  uuremote-viewer unbind DEVICE [--shared]   (从账号删除设备 / remove a device from the account)\n  uuremote-viewer terminal DEVICE [SESSION]\n  uuremote-viewer files DEVICE ls PATH | get REMOTE LOCAL | put LOCAL REMOTE\n  uuremote-viewer portmap DEVICE LOCAL_PORT HOST PORT\n  uuremote-viewer files-ui|portmap-ui|terminal-ui DEVICE NAME   (GTK tool windows)\n  uuremote-viewer login [--no-gui]\n  uuremote-viewer devices METHOD ARRAY_POINTER ID_POINTER NAME_POINTER\n  uuremote-viewer assist CONNECT_ID   (验证码从 stdin 读取一行)\n  uuremote-viewer connect DEVICE true|false SHAPE_JSON echo|manual raw|legacy [--view-only]\nVideo: UU_VIEWER_CODEC=h264 forces H.264; UU_VIEWER_CHROMA=444 requests true color (default 4:2:0).\nAudio: UU_VIEWER_AUDIO=0 disables playback. UU_VIEWER_VIEW_ONLY=1 disables desktop input.\nDevice-list method/JSON pointers and compatibility shape require verified operator configuration.\nQR login reuses the sibling uuremote-server login command. Never run login in a captured log.\nEcho readiness is transport observation only; desktop sessions send one sendVideoTrack RPC (like the official iPhone viewer).";
const DEFAULT_SHAPE:&str=r#"{"capture_type":1,"platform":3,"client_version":"4.42.0","width":1920,"height":1080,"fps":60,"chroma":1,
 "decoders":[{"fps":60,"codec":1,"width":3840,"height":2160,"chroma":1}],"display_modes":[[1920,1080,60]],
 "displays":[{"id":1,"fps":60,"type":0,"hdr":0}],
 "codecs":[{"video_codec":1,"chroma_sampling":1,"codec_impl":34,"bit_depth":8,"width":3840,"height":2160}],
 "feature_flag":[[1,6],[2,2],[3,1],[4,2],[6,2],[7,2],[8,3],[9,1]]}"#;
#[tokio::main]
async fn main() {
    // Do not inherit broad RUST_LOG filters that could expose third-party wire diagnostics.
    let args:Vec<String>=std::env::args().skip(1).collect();
    // The terminal tool owns the TTY: diagnostics would interleave with the remote shell's screen.
    let level=if args.first().is_some_and(|m|matches!(m.as_str(),"terminal"|"terminal-local")){log::LevelFilter::Off}else{log::LevelFilter::Info};
    env_logger::Builder::new().filter_level(log::LevelFilter::Off).filter_module("uuremote_viewer",level).init();
    match run(&args).await {Ok(())=>{},Err(error)=>{eprintln!("{error}");std::process::exit(2)}}
}
async fn run(args:&[String])->Result<(),String> {
    match args.first().map(String::as_str) {
        None|Some("--help"|"help")=>{println!("{HELP}");Ok(())},
        Some("login") if args.len()==1 || (args.len()==2 && args[1]=="--no-gui")=> {
            let executable=std::env::current_exe().map_err(|_|"Cannot locate companion login binary")?.with_file_name("uuremote-server");
            login_command(&executable,&args[1..]).await
        }
        #[cfg(uu_gtk)]
        Some("ui") if args.len()==1=> viewer::ui::run(credentials).await,
        Some("devices-shape") if args.len()==2=> {
            let method=match args[1].as_str(){"GET"=>reqwest::Method::GET,"POST"=>reqwest::Method::POST,_=>return Err("Device list method must be GET or POST".into())};
            println!("{}",api()?.devices_shape(method).await.map_err(|e|e.to_string())?);
            Ok(())
        }
        Some("devices") if args.len()==5=> {
            let method=match args[1].as_str(){"GET"=>reqwest::Method::GET,"POST"=>reqwest::Method::POST,_=>return Err("Device list method must be GET or POST".into())};
            let api=api()?;
            let devices=api.devices(method,&args[2]).await.map_err(|e|e.to_string())?;
            for device in devices {
                // Deliberate UI output, restricted to selected identity/display fields.
                // Strings verbatim; numbers/booleans (platform, status flags) as their JSON text.
                let id=device.pointer(&args[3]).map(|v|v.as_str().map(str::to_owned).unwrap_or_else(||v.to_string())).unwrap_or_default();
                let name=device.pointer(&args[4]).and_then(serde_json::Value::as_str).unwrap_or("");
                println!("{}",serde_json::json!({"device":id,"name":name}));
            }
            Ok(())
        }
        Some("unbind") if (args.len()==2 || (args.len()==3 && args[2]=="--shared")) && !args[1].is_empty() => {
            api()?.unbind(&args[1],args.len()==3).await.map_err(|e|crate::tr!("删除设备失败：{e}","Removing the device failed: {e}"))?;
            println!("{}",crate::i18n::s("已从账号删除设备","Device removed from the account"));Ok(())
        }
        Some("power")=>power_command(args).await,
        Some("type")=>type_command(args).await,
        Some("terminal-local") if args.len()==2 || args.len()==3=>{
            let session=if args.len()==3{Some(args[2].parse::<u32>().ok().filter(|n|*n!=0).ok_or("Invalid terminal session id")?)}else{None};
            viewer::tools::terminal_ipc::client(std::path::Path::new(&args[1]),session).await
        },
        Some("terminal"|"files"|"portmap"|"files-ui"|"portmap-ui"|"terminal-ui")=>tool_command(args).await,
        Some("assist") if args.len()==2=> {
            let code=viewer::assist::read_code(std::io::stdin().lock())?;
            desktop_command(&["connect".into(),args[1].clone(),"false".into(),"-".into(),"echo".into(),"legacy".into()],Some(code)).await.map_err(|e|crate::tr!("协助连接失败：{e}","Assistance connection failed: {e}"))
        }
        Some("connect") if args.len()==6 || args.len()==7=> desktop_command(args,None).await,
        _=>Err(HELP.into()),
    }
}
async fn desktop_command(args:&[String],code:Option<String>)->Result<(),String> {
    let view_only=viewer::input::view_only(args.get(6).map(String::as_str),std::env::var("UU_VIEWER_VIEW_ONLY").ok().as_deref())?;
    let force=match args[2].as_str(){"true"=>true,"false"=>false,_=>return Err("force_join must be explicit true or false".into())};
    let policy=match args[4].as_str(){"echo"=>ReadyPolicy::EchoOnly,"manual"=>ReadyPolicy::Explicit,_=>return Err("Ready policy must be echo or manual".into())};
    let legacy=match args[5].as_str(){"raw"=>false,"legacy"=>true,_=>return Err("Attachment framing must be raw or legacy".into())};
    // "-" selects the built-in desktop shape (mirrors the official Mac viewer's observed request).
    let text=if args[3]=="-" {DEFAULT_SHAPE.to_owned()} else {std::fs::read_to_string(&args[3]).map_err(|_|"Cannot read explicit viewer shape")?};
    let shape:Shape=serde_json::from_str(&text).map_err(|_|"Invalid explicit viewer shape JSON")?;shape.validate_session()?;
    #[cfg(feature="viewer-sdl")]
    let mut desktop=if shape.capture_type==1 {
        if matches!(policy,ReadyPolicy::Explicit) {return Err("Desktop manual readiness has no verified activation protocol".into())}
        // Library decoding with viewer-ffmpeg (FFmpeg 6.x headers); otherwise an ffmpeg CLI process.
        #[cfg(feature="viewer-ffmpeg")]
        let decoder:Box<dyn viewer::desktop::Decoder>=Box::new(viewer::decoder::Ffmpeg::new()?);
        #[cfg(not(feature="viewer-ffmpeg"))]
        let decoder:Box<dyn viewer::desktop::Decoder>=Box::new(viewer::decoder_cli::Cli::new()?);
        Some((viewer::sdl::Window::new()?,decoder))
    }else{None};
    let creds=credentials()?;
    let api=Api::new(auth::UUAuthClient::new(creds.clone()),auth::BASE_URL);
    let account_connection=code.is_none();
    #[cfg(feature="viewer-sdl")]
    macro_rules! wait {($stage:expr,$fut:expr)=>{viewer::sdl::loading(desktop.as_mut().map(|d|&mut d.0),$stage,$fut).await?}}
    #[cfg(not(feature="viewer-sdl"))]
    macro_rules! wait {($stage:expr,$fut:expr)=>{{let _=$stage;$fut.await}}}
    let contacting=i18n::s("正在连接远端设备…","Contacting the remote device…");
    let room=wait!(contacting,async{match code {
        Some(code)=>api.join_share(&args[1],&code).await,
        None=>api.join(&args[1],force,true).await,
    }}).map_err(|e|e.to_string())?;
    let target_mac=if shape.capture_type==1 {wait!(contacting,api.target_is_mac(&room,account_connection.then_some(args[1].as_str())))} else {Err(viewer::api::Error::UnknownKeyboardPlatform)};
    #[cfg(feature="viewer-sdl")]
    if let Some((window,_))=desktop.as_mut().filter(|_|!view_only){
        window.set_mac(*target_mac.as_ref().map_err(|e|e.to_string())?);
    }
    #[cfg(feature="viewer-sdl")]
    let mut clipboard=desktop.as_ref().map(|(window,_)|window.clipboard_bridge()).unwrap_or_else(||clipboard::Bridge::new(false));
    #[cfg(not(feature="viewer-sdl"))]
    let mut clipboard=clipboard::Bridge::new(shape.capture_type==1);
    clipboard.set_target_mac(target_mac.ok());
    let options=Options{timeout:Duration::from_secs(12),legacy_prefix:legacy,ready_policy:policy,clipboard};
    let mut session=wait!(i18n::s("正在建立安全连接…","Establishing a secure connection…"),Session::open(&room,&shape,&creds.device_id,&creds.client_id,options,Default::default()))?;
    #[cfg(feature="viewer-sdl")]
    let result=if let Some((window,decoder))=desktop.as_mut(){viewer::sdl::run(&mut session,window,decoder.as_mut(),view_only,&args[1]).await}else{session.run().await};
    #[cfg(not(feature="viewer-sdl"))]
    let result=session.run().await;
    session.close().await;result
}
async fn type_command(args:&[String])->Result<(),String>{
    // type DEVICE [--delay-ms MS] [--omit-last N] [--snapshot PATH] [--dry-run] [--new-session]
    if args.len()<2 || args[1].trim().is_empty() { return Err(HELP.into()) }
    let device=&args[1];
    let mut delay_ms:Option<&str>=None;
    let mut omit_last:Option<&str>=None;
    let mut snapshot:Option<String>=None;
    let mut dry_run=false;
    let mut new_session=false;
    let mut i=2;
    while i<args.len() {
        match args[i].as_str() {
            "--delay-ms" => {
                i+=1; delay_ms=Some(args.get(i).map(String::as_str).ok_or_else(||HELP.to_string())?);
            }
            "--omit-last" => {
                i+=1; omit_last=Some(args.get(i).map(String::as_str).ok_or_else(||HELP.to_string())?);
            }
            "--snapshot" => {
                i+=1; snapshot=Some(args.get(i).cloned().ok_or_else(||HELP.to_string())?);
            }
            "--dry-run" => { dry_run=true; }
            "--new-session" => { new_session=true; }
            _ => return Err(HELP.into()),
        }
        i+=1;
    }
    let delay=viewer::secure_input::parse_delay_ms(delay_ms)?;
    let omit=viewer::secure_input::parse_omit_last(omit_last)?;
    let mut secret=viewer::secure_input::load_secret(std::io::stdin().lock())?;
    if dry_run {
        let report=viewer::secure_input::dry_run_report(&secret, omit, false)?;
        drop(secret);
        println!("{report}");
        return Ok(());
    }
    secret.omit_last_chars(omit)?;
    // Prefer a running desktop viewer (avoids force-replacing the live session, upstream #30).
    match viewer::type_ipc::socket_path(device) {
        Ok(path) if path.exists() && !new_session => {
            let req=viewer::type_ipc::TypeRequest{
                delay_ms: delay.as_millis() as u64,
                omit_last: 0, // already applied locally
                snapshot,
                secret: secret.as_str().to_owned(),
            };
            drop(secret);
            let response=viewer::type_ipc::send_to_viewer(device, req).await?;
            if !response.ok {
                return Err(response.error.unwrap_or_else(||"Secure input failed on running viewer".into()));
            }
            println!("{}",crate::tr!("安全输入已发送（{} 个按键事件，{} 字符）。","Secure input sent ({} key events, {} characters).", response.events, response.chars));
            if let Some(path)=response.snapshot {
                println!("snapshot={path}");
                log::info!("Secure input snapshot: {path}");
            }
            return Ok(());
        }
        _ => {}
    }
    if !new_session {
        drop(secret);
        return Err("No running desktop viewer for this device; open a connect/ui session first, or pass --new-session".into());
    }
    let mut shape:Shape=serde_json::from_str(DEFAULT_SHAPE).map_err(|_|"Invalid built-in type shape")?;
    shape.capture_type=1;
    let creds=credentials()?;let api=Api::new(auth::UUAuthClient::new(creds.clone()),auth::BASE_URL);
    let room=api.join(device,false,true).await.map_err(|e|e.to_string())?;
    let mac=api.target_is_mac(&room,Some(device)).await.map_err(|e|e.to_string())?;
    let actions=viewer::secure_input::expand(secret.as_str(),mac)?;
    let event_count=actions.len();
    let char_count=secret.chars_count();
    drop(secret);
    log::info!("Secure input prepared (new session): chars={} events={} delay_ms={} mac={}", char_count, event_count, delay.as_millis(), mac);
    let options=Options{timeout:Duration::from_secs(12),legacy_prefix:true,ready_policy:ReadyPolicy::EchoOnly,clipboard:clipboard::Bridge::new(false)};
    let mut session=Session::open(&room,&shape,&creds.device_id,&creds.client_id,options,Default::default()).await?;
    let result=viewer::secure_input::run_session(&mut session,actions.as_slice(),delay).await;
    session.close().await;
    if result.is_ok(){println!("{}",crate::tr!("安全输入已发送（{event_count} 个按键事件）；新会话路径无截图。","Secure input sent ({event_count} key events); no snapshot on the new-session path."));}
    result
}

fn credentials()->Result<auth::Credentials,String> {auth::Credentials::load(config::read_fresh().map_err(|_|"Cannot read credential configuration")?.as_ref())}
fn api()->Result<Api,String> {Ok(Api::new(auth::UUAuthClient::new(credentials()?),auth::BASE_URL))}

async fn login_command(executable:&std::path::Path,options:&[String])->Result<(),String> {
    let status=tokio::process::Command::new(executable).arg("login").args(options).status().await.map_err(|_|"Build sibling uuremote-server to reuse QR login")?;
    if status.success(){Ok(())}else{Err("QR login did not complete".into())}
}
#[cfg(test)]
mod cli_tests {
    use super::*;
    #[tokio::test]
    async fn invalid_commands_never_fall_through_to_login_or_host_service() {
        for args in [vec!["invalid"],vec!["power"],vec!["power","device","logout"],vec!["power","device","reboot","extra"],vec!["type"],vec!["type","device","--delay-ms"],vec!["type","device","--delay-ms","x"],vec!["type","device","extra"],vec!["type","device","--omit-last"],vec!["type","device","--snapshot"],vec!["login","--invalid"],vec!["connect"],vec!["assist"],vec!["assist","id","forbidden-code-argument"],vec!["terminal"],vec!["terminal","device","extra"],vec!["terminal","device","0"],vec!["terminal","device","4294967296"],vec!["terminal-ui","device"],vec!["files","device","bad","path"],vec!["portmap","device","bad","host","80"],vec!["portmap","device","8000","host","0"],vec!["devices","DELETE","/data","/id","/name"],vec!["unbind"],vec!["unbind",""],vec!["unbind","device","--other"],vec!["unbind","device","--shared","extra"]] {
            assert!(run(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).await.is_err());
        }
    }
    #[tokio::test]
    async fn type_dry_run_and_no_viewer_paths_are_redacted() {
        let _g = viewer::secure_input::ENV_LOCK.lock().unwrap();
        std::env::set_var("UU_INPUT_ENV", "UU_TYPE_SENTINEL_ENV");
        std::env::set_var("UU_TYPE_SENTINEL_ENV", "sentinel-SECRET-9f3a");
        let out = {
            // dry-run
            let args: Vec<String> = ["type","missing-device-id","--dry-run"].into_iter().map(str::to_owned).collect();
            // reload env for dry-run — previous load_secret removed the var
            std::env::set_var("UU_INPUT_ENV", "UU_TYPE_SENTINEL_ENV");
            std::env::set_var("UU_TYPE_SENTINEL_ENV", "sentinel-SECRET-9f3a");
            run(&args).await
        };
        assert!(out.is_ok(), "{out:?}");
        std::env::set_var("UU_INPUT_ENV", "UU_TYPE_SENTINEL_ENV");
        std::env::set_var("UU_TYPE_SENTINEL_ENV", "sentinel-SECRET-9f3a");
        let err = run(&["type".into(),"missing-device-id".into()]).await.unwrap_err();
        assert!(err.contains("No running desktop viewer") || err.contains("--new-session"), "{err}");
        assert!(!err.contains("sentinel"));
        assert!(!err.contains("SECRET"));
        std::env::remove_var("UU_INPUT_ENV");
        std::env::remove_var("UU_TYPE_SENTINEL_ENV");
    }
    #[tokio::test]
    async fn login_reuses_companion_subcommand_with_a_local_process_mock() {
        use std::os::unix::fs::PermissionsExt;
        let path=std::env::temp_dir().join(format!("uu-viewer-login-mock-{}",viewer::wire::uuid()));
        std::fs::write(&path,b"#!/bin/sh\n[ \"$#\" -eq 2 ] && [ \"$1\" = login ] && [ \"$2\" = --no-gui ]\n".as_slice()).unwrap();
        std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o700)).unwrap();
        let result=login_command(&path,&["--no-gui".into()]).await;
        std::fs::remove_file(path).unwrap();assert!(result.is_ok());
    }
}

async fn tool_command(args:&[String])->Result<(),String>{
    let capture=match args[0].as_str(){"terminal" if args.len()==2 || args.len()==3=>8,"terminal-ui" if args.len()==3=>8,"files" if (args.len()==4 && args[2]=="ls") || (args.len()==5 && ["get","put"].contains(&args[2].as_str()))=>5,"portmap" if args.len()==5=>9,"files-ui" if args.len()==3=>5,"portmap-ui" if args.len()==3=>9,_=>return Err(HELP.into())};
    let terminal_session=if args[0]=="terminal" && args.len()==3{Some(args[2].parse::<u32>().ok().filter(|n|*n!=0).ok_or("Invalid terminal session id")?)}else{None};
    #[cfg(not(uu_gtk))] if args[0].ends_with("-ui"){return Err("Tool windows require the GTK build".into())}
    let ports=if args[0]=="portmap"{Some((args[2].parse::<u16>().map_err(|_|"Invalid local port")?,args[4].parse::<u16>().map_err(|_|"Invalid target port")?))}else{None};
    if ports.is_some_and(|(_,port)|port==0) || (args[0]=="portmap" && args[3].trim().is_empty()){return Err("Invalid forwarding target".into())}
    let mut shape:Shape=serde_json::from_str(DEFAULT_SHAPE).map_err(|_|"Invalid built-in tool shape")?;
    shape.capture_type=capture;shape.feature_flag.clear();
    if capture!=5{shape.decoders.clear();shape.display_modes.clear();shape.displays.clear();shape.codecs.clear();}
    let creds=credentials()?;let api=Api::new(auth::UUAuthClient::new(creds.clone()),auth::BASE_URL);
    let room=api.join(&args[1],false,true).await.map_err(|e|e.to_string())?;
    // Match the existing UI connection framing (9bb42d5); the host tolerates this prefix.
    let options=Options{timeout:Duration::from_secs(12),legacy_prefix:true,ready_policy:ReadyPolicy::EchoOnly,clipboard:clipboard::Bridge::new(false)};
    let mut session=Session::open(&room,&shape,&creds.device_id,&creds.client_id,options,Default::default()).await?;
    let result=async{
        let(io,rx)=viewer::tools::attach(&session.peer)?;
        let ready=async{
            loop{if session.peer.channels.iter().enumerate().filter(|(i,_)|[0,if capture==8{4}else{3}].contains(i)).all(|(_,c)|c.ready_state()==webrtc::data_channel::data_channel_state::RTCDataChannelState::Open) && session.peer.observed.echo_received.load(std::sync::atomic::Ordering::Acquire){break}tokio::select!{r=session.step()=>r?,_=tokio::time::sleep(Duration::from_millis(20))=>{}}}Ok::<_,String>(())
        };
        tokio::time::timeout(Duration::from_secs(30),ready).await.map_err(|_|"Tool connection timed out")??;
        let operation=async{match args[0].as_str(){"terminal"=>viewer::tools::cli::terminal(io,rx,terminal_session).await,"files"=>viewer::tools::cli::files(io,rx,&args[2..]).await,
            #[cfg(uu_gtk)] "terminal-ui"=>viewer::tools_ui::terminal(io,rx,&args[1],&args[2]).await,
            #[cfg(uu_gtk)] "files-ui"=>viewer::tools_ui::files(io,rx,&args[2]).await,
            #[cfg(uu_gtk)] "portmap-ui"=>viewer::tools_ui::portmap(io,rx,&args[2]).await,
            "portmap"=>{let(local,port)=ports.unwrap();viewer::tools::cli::portmap(io,rx,local,&args[3],port).await}
            _=>Err("Tool windows require the GTK build".into())}};
        tokio::pin!(operation);
        let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|_|"Cannot watch termination")?;
        loop{tokio::select!{r=&mut operation=>break r,r=session.step()=>r?,_=terminate.recv()=>break Ok(()),_=tokio::signal::ctrl_c()=>break Ok(())}}
    }.await;
    session.close().await;result
}

async fn power_command(args:&[String])->Result<(),String>{
    if args.len()!=3 || args[1].trim().is_empty(){return Err(HELP.into())}
    let action=viewer::power::Action::parse(&args[2])?;
    let mut shape:Shape=serde_json::from_str(DEFAULT_SHAPE).map_err(|_|"Invalid built-in power shape")?;
    // File-tool peers carry CONTROL without starting desktop capture or clipboard.
    shape.capture_type=5;shape.feature_flag.clear();
    let creds=credentials()?;let api=Api::new(auth::UUAuthClient::new(creds.clone()),auth::BASE_URL);
    let room=api.join(&args[1],false,true).await.map_err(|e|e.to_string())?;
    let options=Options{timeout:Duration::from_secs(12),legacy_prefix:true,ready_policy:ReadyPolicy::EchoOnly,clipboard:clipboard::Bridge::new(false)};
    let mut session=Session::open(&room,&shape,&creds.device_id,&creds.client_id,options,Default::default()).await?;
    let result=viewer::power::run(&mut session,action).await;
    session.close().await;
    if result.is_ok(){println!("{}",crate::i18n::s("电源指令已发送；未确认远端执行结果。","Power command sent; the result on the remote device is not confirmed."));}
    result
}
