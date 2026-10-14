//! UU Remote Pure Native Linux Server (Rust Edition)
//! Zero-Wine, Zero-Python, Single-Binary, 5 Native Tools Integrated

mod desktop_backend;
mod portal;
mod network_diagnostics;
mod session_ice;
mod wayland;
mod auth;
mod config;
mod login;
mod login_ui;
mod session_lock;
mod capture;
mod file_trans;
mod input;
mod input_mapping;
mod port_forward;
mod power;
mod signaling;
mod terminal;
mod webrtc;
mod video;
#[cfg(feature = "inproc-av")]
mod damage_capture;
#[cfg(feature = "inproc-av")]
mod inproc_encoder;
mod capture_setting;
mod audio;
mod system_actions;
mod capability;
mod protocol;
mod control_protocol;
mod rtp_metadata;
mod initialization;
mod system_metrics;
mod clipboard;
mod clipboard_protocol;
mod clipboard_rich;
mod viewer_metrics;
mod cursor;

use auth::{Credentials, UUAuthClient};
mod clipboard_probe;
use capture::ScreenCapture;
use input::InputInjector;
use signaling::{SignalingClient, SignalingEvent, SoacMessage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use webrtc::WebRtcSession;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // An unknown argument must never fall through to starting a second full host server.
    const SUBCOMMANDS: [&str; 9] = ["login", "unlock-check", "desktop-probe", "desktop-video-probe", "desktop-audio-probe", "wayland-capture",
        "wayland-input-probe", "clipboard-acceptance", "portal-acceptance"];
    if let Some(arg) = std::env::args().nth(1).filter(|a| !SUBCOMMANDS.contains(&a.as_str())) {
        let usage = format!("usage: uuremote-server [{}]\n  (no argument: run the host)", SUBCOMMANDS.join(" | "));
        if matches!(arg.as_str(), "help" | "-h" | "--help") { println!("{usage}"); return Ok(()); }
        eprintln!("unknown argument: {arg}\n{usage}");
        std::process::exit(2);
    }
    if std::env::args().nth(1).as_deref() == Some("clipboard-acceptance") {
        return tokio::task::spawn_blocking(clipboard_probe::run).await?.map_err(Into::into);
    }
    if std::env::args().nth(1).as_deref() == Some("portal-acceptance") {
        if std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim()!="uu-hyprland-test" {
            return Err("Portal input acceptance is restricted to the isolated test VM".into());
        }
        let screen=ScreenCapture::new()?;
        if screen.portal.is_none() { return Err("Portal acceptance requires the Portal backend".into()); }
        // Give the consent dialog time to release focus before fixed lab events.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let input=InputInjector::new();
        input.bind_portal(screen.portal.clone());
        if std::env::args().nth(2).as_deref() == Some("--clipboard-targets") {
            let read = || -> Result<serde_json::Value, Box<dyn std::error::Error>> {
                Ok(serde_json::from_slice(&std::fs::read("/tmp/uuremote-clipboard-official.json")?)?)
            };
            let before = read()?;
            if before["native_wayland"] != true || before["window_active"] != true { return Err("Native clipboard fixture must be focused".into()); }
            for name in ["check", "copy"] {
                let bounds = before[format!("{name}_bounds")].as_array().ok_or("Missing test button bounds")?;
                let number = |index: usize| bounds[index].as_f64().ok_or("Invalid button bounds");
                let x = number(0)? + number(2)? / 2.0;
                let y = number(1)? + number(3)? / 2.0;
                let width = before["window_size"][0].as_f64().ok_or("Missing test window width")?;
                let height = before["window_size"][1].as_f64().ok_or("Missing test window height")?;
                input.mouse_absolute_normalized(x / width, y / height, (screen.width,screen.height));
                input.mouse_button(1,true); input.mouse_button(1,false);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let after = read()?;
                let key = format!("{name}_activations");
                if after[&key].as_u64() != before[&key].as_u64().map(|n| n+1) {
                    return Err(format!("Portal small-target click missed: {name}").into());
                }
                let position = after["last_position"].as_array().ok_or("Missing native test pointer position")?;
                if (position[0].as_f64().ok_or("Invalid pointer x")? - x).abs() > 2.0 || (position[1].as_f64().ok_or("Invalid pointer y")? - y).abs() > 2.0 {
                    return Err("Portal small-target pointer error exceeds two logical pixels".into());
                }
                println!("Portal native small-target click passed: {name}; pointer_error<=2px");
            }
            return Ok(());
        }
        input.mouse_absolute_normalized(0.25,0.25,(screen.width,screen.height));input.flush_pointer();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        input.mouse_absolute_normalized(0.5,0.5,(screen.width,screen.height));input.flush_pointer();
        input.mouse_button(1,true);input.mouse_button(1,false);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        input.mouse_relative(32,0);input.flush_pointer();
        for key in b"UUWAYLAND" { input.windows_key_event(i32::from(*key),true);input.windows_key_event(i32::from(*key),false); }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        input.release_held();
        let report:serde_json::Value=serde_json::from_slice(&std::fs::read("/tmp/uuremote-input-acceptance.json")?)?;
        for key in ["native_wayland","window_active","motion","click","keyboard","text_input"] {
            if report.get(key).and_then(serde_json::Value::as_bool)!=Some(true) { return Err(format!("Portal native GTK acceptance failed: {key}").into()); }
        }
        println!("Portal native GTK input acceptance passed: motion/click/keyboard/text");
        if std::env::args().nth(2).as_deref() == Some("--lock-recovery") {
            let session = screen.portal.as_ref().unwrap();
            let before = report["key_presses"].as_u64().ok_or("Missing GTK key count")?;
            let run_lock = |action: &str| -> Result<(), String> {
                if std::process::Command::new("timeout").args(["5s", "loginctl", "--no-ask-password", action]).arg(
                    String::from_utf8(std::process::Command::new("loginctl").args(["show-user", &unsafe { libc::getuid() }.to_string(), "-p", "Display", "--value"]).output().map_err(|_| "Display session unavailable")?.stdout).map_err(|_| "Invalid display session")?.trim()
                ).status().map_err(|_| "loginctl unavailable")?.success() {
                    Ok(())
                } else { Err("Test VM lock command failed".into()) }
            };
            let locked = session_lock::watch_locked();
            run_lock("lock-session")?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while *locked.borrow() != Some(true) && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if *locked.borrow() != Some(true) { return Err("Test VM did not lock".into()); }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let closed = !session.is_alive();
            run_lock("unlock-session")?;
            let mut frame = vec![0; screen.width as usize * screen.height as usize * 4];
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                session.capture(&mut frame)?;
                if *locked.borrow() == Some(false) && session.is_alive() { break; }
                if std::time::Instant::now() >= deadline { return Err("Test VM portal did not recover".into()); }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            // Keep the original InputInjector and Arc: no rebind after recovery.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            input.mouse_absolute_normalized(0.5,0.5,(screen.width,screen.height));
            input.mouse_button(1,true); input.mouse_button(1,false);
            for key in b"UUWAYLAND" { input.windows_key_event(i32::from(*key),true); input.windows_key_event(i32::from(*key),false); }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            input.release_held();
            let after:serde_json::Value=serde_json::from_slice(&std::fs::read("/tmp/uuremote-input-acceptance.json")?)?;
            if after["key_presses"].as_u64() != Some(before + 9) || after["key_releases"].as_u64() != Some(before + 9) {
                return Err("Original portal input binding failed after lock recovery".into());
            }
            println!("Portal same-Arc lock recovery input passed: session_closed={} balanced_new_keys=9", closed);
            return Ok(());
        }
        let path=std::env::args().nth(2);
        return video::probe((screen.width,screen.height),screen.display.clone(),screen.portal.clone(),path).await.map_err(|e|e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("wayland-input-probe") {
        return wayland::input_probe().map_err(|e|e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("wayland-capture") {
        return wayland::capture_stdout().map_err(|e| e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("desktop-probe") {
        let screen=ScreenCapture::new()?;
        capture::keep_display_awake()?;
        let frame=screen.capture_frame()?;
        if let Some(path)=std::env::args().nth(2) {
            use std::io::Write;
            let mut rgba=frame.clone();
            for pixel in rgba.chunks_exact_mut(4) { pixel.swap(0,2); pixel[3]=255; }
            let png=cursor::encode_png(screen.width,screen.height,&rgba);
            let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
            file.write_all(&png)?;
        }
        println!("backend={:?} width={} height={} frame_bytes={}",screen.backend,screen.width,screen.height,frame.len());
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("desktop-video-probe") {
        let screen=ScreenCapture::new()?;
        capture::keep_display_awake()?;
        return video::probe((screen.width,screen.height),screen.display.clone(),screen.portal.clone(),std::env::args().nth(2)).await.map_err(|e|e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("desktop-audio-probe") {
        return audio::probe(std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(5)).await.map_err(|e| e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("unlock-check") {
        return session_lock::check_from_stdin().map_err(|e| e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("login") {
        // Interactive: register this host and bind it to an account via QR scan
        // (GTK window when available; `login --no-gui` keeps it terminal-only).
        let gui = !std::env::args().skip(2).any(|a| a == "--no-gui");
        if login::run(gui).await.is_err() { std::process::exit(1); } // error already shown
        return Ok(());
    }
    log::info!("Starting uu-remote-linux host v{}...", env!("CARGO_PKG_VERSION"));

    let mut capture = Arc::new((if desktop_backend::detect()?==desktop_backend::DesktopBackend::Portal
        && std::env::var("UU_PORTAL_PREAUTHORIZE").ok().as_deref()!=Some("1") {
        Err("Portal screen access requires a desktop connection and local consent".to_owned())
    } else { ScreenCapture::new() }).unwrap_or_else(|error| {
        log::warn!("Desktop unavailable; terminal and file tools remain available: {error}");
        ScreenCapture::unavailable(error)
    }));
    let input = Arc::new(InputInjector::new());
    let _pointer_worker = input.start_pointer_worker();
    #[cfg(target_os="linux")]
    {
        let shutdown_input=input.clone();
        tokio::spawn(async move {
            if let Ok(mut term)=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                term.recv().await; shutdown_input.release_held(); terminal::shutdown_all(); std::process::exit(0);
            }
        });
    }
    let creds = match config::file().and_then(Credentials::load) {
        Ok(creds) => creds,
        Err(e) => {
            log::warn!("Not logged in: {e} ({} or UU_* environment variables)", config::path().display());
            login::wait_for_credentials(None).await
        }
    };
    let mut auth = Arc::new(UUAuthClient::new(creds));

    // One peer per viewer connection (desktop, file transfer, terminal...), keyed by ice_id.
    let webrtc_sessions: Arc<Mutex<HashMap<String, WebRtcSession>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut next_capture_type: Option<(u64,crate::session_ice::SessionIce)> = None;
    let mut capture_types: HashMap<String, (u64,crate::session_ice::SessionIce)> = HashMap::new();

    loop {
        log::info!("Requesting fresh room credentials from NetEase cloud...");
        let room_data = match auth.create_room().await {
            Ok(data) => data,
            Err(auth::RoomError::LoginExpired) => {
                // Like the official client: an expired token needs a new QR login, not retries.
                log::error!("UU login is invalid or expired; a new QR login is required");
                let stale = config::read_fresh().ok().flatten().and_then(|f| f.get("token").and_then(|t| t.as_str()).map(str::to_owned));
                auth = Arc::new(UUAuthClient::new(login::wait_for_credentials(stale).await));
                continue;
            }
            Err(e) => {
                log::error!("Failed to create room: {}. Retrying in 5s...", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        log::info!(
            "Acquired room signaling target: {}",
            room_data.signaling_server
        );

        let (tx_event, mut rx_event) = mpsc::channel::<SignalingEvent>(64);
        let tx_timer = tx_event.clone();
        let signaling_client = match SignalingClient::connect(
            &room_data.signaling_server,
            &room_data.token,
            tx_event,
        )
        .await
        {
            Ok(cli) => {
                let renewal_peers=webrtc_sessions.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(900)).await;
                    loop {
                        if tx_timer.is_closed() { return; }
                        let active=renewal_peers.lock().await.values().any(|session| {
                            use ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                            session.pc.connection_state()==RTCPeerConnectionState::Connected
                        });
                        if !active {
                            log::info!("Idle room renewal timer elapsed; refreshing signaling room");
                            let _=tx_timer.send(SignalingEvent::Disconnected).await; return;
                        }
                        log::info!("Room renewal deferred while remote connection is active");
                        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    }
                });
                Arc::new(cli)
            }
            Err(e) => {
                log::error!("Failed to connect signaling: {}. Retrying in 5s...", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        // 事件处理主循环
        while let Some(evt) = rx_event.recv().await {
            match evt {
                SignalingEvent::Connected => {
                    log::info!("Signaling handshake established.");
                }
                SignalingEvent::RoomInfoAck(val) => {
                    log::info!(
                        "RoomInfo acknowledged by NetEase server: room_id={}",
                        val.get("room_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                    );
                }
                SignalingEvent::DeviceInfoChanged(val) => {
                    log::info!(
                        "Device status synchronized: status={:?}, availability={:?}",
                        val.get("data")
                            .and_then(|d| d.get("status"))
                            .and_then(|s| s.as_str()),
                        val.get("data")
                            .and_then(|d| d.get("publisher_availability_status"))
                            .and_then(|s| s.as_str())
                    );
                }
                SignalingEvent::ConnectRequest { capture_type, ice_id, transport } => {
                    // Pending cloud credentials live only within this room; cap
                    // requests that never receive an offer rather than retain them.
                    if capture_types.len()>=32 { capture_types.clear(); }
                    match ice_id { Some(id) => { capture_types.insert(id, (capture_type,transport)); } None => next_capture_type = Some((capture_type,transport)) }
                }
                SignalingEvent::BeControlled(val) => {
                    let _ = val;
                    log::info!("Incoming remote control session request; awaiting offer");
                }

                SignalingEvent::Soac(soac) => {
                    log::info!(
                        "SOAC signaling received: type={}, ice_id={:?}",
                        soac.msg_type,
                        soac.ice_id
                    );
                    let ice = soac.ice_id.as_ref().map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default();
                    let mut sessions = webrtc_sessions.lock().await;
                    // Drop peers that already ended.
                    let ended: Vec<String> = sessions.iter().filter(|(_, s)| matches!(s.pc.connection_state(),
                        ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed |
                        ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed)).map(|(k, _)| k.clone()).collect();
                    for k in ended { if let Some(old) = sessions.remove(&k) { let _ = old.pc.close().await; } }

                    if soac.msg_type == "offer" {
                        let (capture_type,transport) = capture_types.remove(&ice).or_else(|| next_capture_type.take()).unwrap_or((1,crate::session_ice::SessionIce::default()));
                        // CaptureType: 0 unknown, 1 desktop, 2 window; 5 file transfer, 8 terminal, 9 port mapping.
                        let desktop = capture_type <= 2;
                        log::info!("Offer for connection: capture_type={} desktop={} active_peers={}", capture_type, desktop, sessions.len());
                        if let Some(old) = sessions.remove(&ice) { let _ = old.pc.close().await; }
                        if desktop {
                            // One desktop stream at a time: a new desktop offer replaces stale ones.
                            let stale: Vec<String> = sessions.iter().filter(|(_, s)| s.desktop).map(|(k, _)| k.clone()).collect();
                            for k in stale { if let Some(old) = sessions.remove(&k) { let _ = old.pc.close().await; } }
                        }
                        let stale_portal=capture.portal.as_ref().is_some_and(|session|!session.is_alive())
                            && crate::session_lock::locked_now() == Some(false);
                        let session_capture=if desktop && (capture.init_error.is_some() || stale_portal) {
                            if stale_portal {
                                input.release_held();input.bind_portal(None);
                                capture=Arc::new(ScreenCapture::unavailable("Portal authorization expired".into()));
                            }
                            match tokio::task::spawn_blocking(ScreenCapture::new).await {
                                Ok(Ok(screen))=>{ capture=Arc::new(screen);capture.clone() },
                                Ok(Err(error))=>{ log::warn!("Desktop connection unavailable: {error}"); continue; },
                                Err(_)=>{log::warn!("Desktop initialization worker failed");continue;},
                            }
                        } else { capture.clone() };
                        // Independent file/terminal peers must not detach the
                        // current desktop's authorized Portal input session.
                        if desktop { input.bind_portal(session_capture.portal.clone()); }
                        match WebRtcSession::new(session_capture, input.clone(), signaling_client.clone(), desktop, transport).await {
                            Ok(sess) => match sess.handle_offer(&soac).await {
                                Ok(answer_sdp) => {
                                    log::info!("Local SDP Answer generated, emitting soac response...");
                                    let resp = SoacMessage {
                                        msg_type: "answer".into(),
                                        sdp: Some(answer_sdp),
                                        gzip_sdp: None,
                                        candidate: None,
                                        ice_id: soac.ice_id.clone(),
                                        sdp_mid: None,
                                        sdp_mline_index: None,
                                        client_id: soac.client_id.clone(),
                                        app_control_id: soac.app_control_id.clone(),
                                    };
                                    if let Err(e) = signaling_client.emit_soac(resp).await { log::error!("Answer signaling send failed: {}", e); }
                                    sessions.insert(ice, sess);
                                }
                                Err(e) => { log::error!("Failed to handle offer: {}", e); let _ = sess.pc.close().await; }
                            },
                            Err(e) => log::error!("Create peer failed: {}", e),
                        }
                    } else if soac.msg_type == "candidate" {
                        if let Some(sess) = sessions.get(&ice) {
                            if let Err(e) = sess.add_candidate(&soac).await { log::error!("Remote ICE candidate failed: {}", e); }
                        } else {
                            log::warn!("Remote ICE candidate for unknown connection");
                        }
                    }
                }
                SignalingEvent::Released(val) | SignalingEvent::Left(val) => {
                    let ice = ["ice_id", "iceId"].iter().find_map(|k| val.get(*k).or_else(|| val.get("data").and_then(|d| d.get(*k))))
                        .and_then(|v| v.as_str()).map(str::to_owned);
                    let keys: Vec<String> = val.as_object().map(|o| o.keys().take(16).cloned().collect()).unwrap_or_default();
                    let mut sessions = webrtc_sessions.lock().await;
                    let targets: Vec<String> = match &ice {
                        Some(id) if sessions.contains_key(id) => vec![id.clone()],
                        _ => sessions.keys().cloned().collect(),
                    };
                    log::info!("Remote controller disconnected: keys={:?} ice_matched={} closing={}", keys, ice.as_ref().is_some_and(|i| sessions.contains_key(i)), targets.len());
                    for k in targets {
                        if let Some(old) = sessions.remove(&k) {
                            if let Err(e) = old.pc.close().await { log::warn!("Close released peer failed: {}", e); }
                        }
                    }
                }
                SignalingEvent::Disconnected => {
                    capture_types.clear();
                    next_capture_type=None;
                    log::warn!("Signaling connection lost. Initiating reconnect loop...");
                    break;
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}
