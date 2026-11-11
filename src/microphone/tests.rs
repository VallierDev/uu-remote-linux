#![cfg(target_os = "linux")]
use super::*;
use tokio::io::AsyncReadExt;
use webrtc::track::track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocalWriter};
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::interceptor::{mock::{mock_builder::MockBuilder, mock_interceptor::MockInterceptor},
    registry::Registry, Attributes, RTPReader};

fn have_opus() -> bool {
    match opus() {
        Ok(_) => true,
        Err(error) => { eprintln!("Skipping Opus-dependent microphone test: {error}"); false }
    }
}

#[derive(Default)]
struct ReadPause {
    requested: AtomicBool, entered: tokio::sync::Notify,
    resume: tokio::sync::Notify, completed: tokio::sync::Notify,
}
struct PausedReader { reader: Arc<dyn RTPReader + Send + Sync>, pause: Arc<ReadPause> }
#[async_trait::async_trait]
impl RTPReader for PausedReader {
    async fn read(&self, buffer: &mut [u8], attributes: &Attributes) -> Result<(Packet, Attributes), webrtc::interceptor::Error> {
        let result = self.reader.read(buffer, attributes).await?;
        if self.pause.requested.swap(false, Ordering::AcqRel) {
            // The packet has already been consumed; cancelling here would lose it.
            self.pause.entered.notify_one();
            self.pause.resume.notified().await;
            self.pause.completed.notify_one();
        }
        Ok(result)
    }
}
fn pause_registry(pause: Arc<ReadPause>) -> Registry {
    let mut registry = Registry::new();
    registry.add(Box::new(MockBuilder::new(move |_| {
        let pause = pause.clone();
        Ok(Arc::new(MockInterceptor { bind_remote_stream_fn: Some(Box::new(move |_, reader| {
            let pause = pause.clone();
            Box::pin(async move {
                Arc::new(PausedReader { reader, pause }) as Arc<dyn RTPReader + Send + Sync>
            })
        })), ..Default::default() }))
    })));
    registry
}

fn tone() -> Vec<u8> {
    tone_samples(960)
}
fn tone_samples(samples: usize) -> Vec<u8> {
    let handle = opus().unwrap()._handle as *mut libc::c_void;
    let create: unsafe extern "C" fn(i32, i32, i32, *mut i32) -> *mut libc::c_void = unsafe {
        std::mem::transmute(libc::dlsym(handle, c"opus_encoder_create".as_ptr()))
    };
    let encode: unsafe extern "C" fn(*mut libc::c_void, *const i16, i32, *mut u8, i32) -> i32 = unsafe {
        std::mem::transmute(libc::dlsym(handle, c"opus_encode".as_ptr()))
    };
    let destroy: unsafe extern "C" fn(*mut libc::c_void) = unsafe {
        std::mem::transmute(libc::dlsym(handle, c"opus_encoder_destroy".as_ptr()))
    };
    let pcm: Vec<i16> = (0..samples).flat_map(|i| {
        let s = ((i as f64 * std::f64::consts::TAU * 1000. / 48000.).sin() * 6000.) as i16;
        [s, s]
    }).collect();
    let mut error = 0;
    let enc = unsafe { create(48000, 2, 2048, &mut error) };
    assert_eq!(error, 0);
    assert!(!enc.is_null());
    let mut out = vec![0; 4000];
    let n = unsafe { encode(enc, pcm.as_ptr(), samples as i32, out.as_mut_ptr(), out.len() as i32) };
    unsafe { destroy(enc); }
    assert!(n > 0);
    out.truncate(n as usize);
    out
}
fn packet(seq: u16, data: &[u8]) -> Packet {
    packet_at(seq, u32::from(seq) * 960, data)
}
fn packet_at(seq: u16, timestamp: u32, data: &[u8]) -> Packet {
    Packet { header: webrtc::rtp::header::Header { version: 2, sequence_number: seq,
        timestamp, marker: true, ..Default::default() },
        payload: bytes::Bytes::copy_from_slice(data) }
}
fn rms(bytes: &[u8]) -> f64 {
    let sum: f64 = bytes.chunks_exact(2).map(|s| f64::from(i16::from_le_bytes([s[0], s[1]])).powi(2)).sum();
    (sum / (bytes.len() / 2).max(1) as f64).sqrt()
}

#[test]
fn opus_decodes_generated_tone_and_conceals_loss() {
    if !have_opus() { return; }
    assert!(!valid_packet(&[]));
    assert!(!valid_packet(&vec![0; 8193]));
    let data = tone();
    let mut decoder = Decoder::new().unwrap();
    let (pcm, frames) = decoder.decode(&Frame::Opus(data)).unwrap();
    assert_eq!(frames, 960);
    assert_eq!(pcm.len(), 3840);
    assert!(rms(&pcm) > 1000.);
    assert_eq!(decoder.decode(&Frame::Conceal(960)).unwrap().0.len(), 3840);
    assert!(decoder.decode(&Frame::Opus(vec![])).is_err());
    assert!(decoder.decode(&Frame::Conceal(0)).is_err());
}

#[test]
fn reorder_handles_wrap_loss_duplicates_and_bounded_backlog() {
    if !have_opus() { return; }
    let data = tone();
    let mut jitter = Jitter::default();
    for seq in [65535, 1, 0, 0] { jitter.push(packet(seq, &data)); }
    for _ in 0..3 { assert_eq!(jitter.pop(), Some(Frame::Opus(data.clone()))); }
    assert!(jitter.pop().is_none());
    jitter.push(packet(3, &data));
    assert_eq!(jitter.pop(), Some(Frame::Conceal(960))); // sequence 2 was lost
    assert_eq!(jitter.pop(), Some(Frame::Opus(data.clone())));
    jitter.push(packet(0, &data)); // late retransmission
    assert!(jitter.pop().is_none());
    for seq in 4..1000 { jitter.push(packet(seq, &data)); assert!(jitter.packets.len() <= MAX_PACKETS as usize); }
    assert!(jitter.next.unwrap() >= 988);
}

#[test]
fn loss_duration_follows_rtp_clock_across_wrap_and_packet_sizes() {
    if !have_opus() { return; }
    for samples in [480, 960, 1920, 2880] {
        let data = tone_samples(samples);
        let mut decoder = Decoder::new().unwrap();
        let mut jitter = Jitter::default();
        let timestamp = u32::MAX - 239;
        jitter.push(packet_at(65535, timestamp, &data));
        assert_eq!(decoder.decode(&jitter.pop().unwrap()).unwrap().1, samples as u32);
        jitter.push(packet_at(1, timestamp.wrapping_add(samples as u32 * 2), &data));
        let loss = jitter.pop().unwrap();
        assert_eq!(loss, Frame::Conceal(samples as u32));
        assert_eq!(decoder.decode(&loss).unwrap().1, samples as u32);
        assert_eq!(decoder.decode(&jitter.pop().unwrap()).unwrap().1, samples as u32);
        assert!(jitter.pop().is_none());
    }
}

#[test]
fn variable_losses_preserve_total_time_and_bound_discontinuities() {
    if !have_opus() { return; }
    let data = tone();
    let mut jitter = Jitter::default();
    let mut decoder = Decoder::new().unwrap();
    jitter.push(packet_at(0, 0, &data));
    decoder.decode(&jitter.pop().unwrap()).unwrap();
    // Two missing packets total 50 ms; their individual durations cannot be inferred.
    jitter.push(packet_at(3, 960 + 2400, &data));
    let mut concealed = 0;
    for _ in 0..2 { concealed += decoder.decode(&jitter.pop().unwrap()).unwrap().1; }
    assert_eq!(concealed, 2400);
    decoder.decode(&jitter.pop().unwrap()).unwrap();
    jitter.push(packet_at(5, 48_000_000, &data));
    assert_eq!(jitter.pop(), Some(Frame::Conceal(960))); // discontinuity is not a huge loss
}

#[test]
fn ownership_lock_is_shared_across_config_directories() {
    const CHILD: &str = "UU_MICROPHONE_TEST_LOCK";
    if let Ok(source) = std::env::var(CHILD) {
        assert!(route_lock(&source).is_err(), "a different UU_CONFIG bypassed the endpoint lock");
        return;
    }
    let source = format!("uu-microphone-test-{}", std::process::id());
    let lock = route_lock(&source).unwrap();
    for config in ["first/config.json", "second/config.json"] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["microphone::tests::ownership_lock_is_shared_across_config_directories", "--exact"])
            .env(CHILD, &source).env("UU_CONFIG", std::env::temp_dir().join(config))
            .env("XDG_RUNTIME_DIR", std::env::temp_dir().join(config).with_extension("runtime"))
            .env("TMPDIR", std::env::temp_dir().join(config).with_extension("temporary"))
            .output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
    }
    drop(lock);
    drop(route_lock(&source).expect("the lock must be released on drop"));
    std::fs::remove_file(route_lock_path(&source).unwrap()).unwrap();
}

#[test]
fn recovery_requires_exact_module_arguments_and_master() {
    let args = vec!["source_name=uu-remote-microphone".into(), "master=uu-remote-mic-input.monitor".into(),
        "source_properties=device.description=UU-Remote-Microphone".into(), "rate=48000".into(), "channels=2".into()];
    let line = format!("42\tmodule-remap-source\t{}", args.join(" "));
    assert_eq!(Module::matching_id(&line, "module-remap-source", &args), Some(42));
    for foreign in [line.replace("master=uu-remote-mic-input.monitor", "master=physical-microphone"),
        line.replace("description=UU-Remote-Microphone", "description=UU-Remote-Microphone-Foreign"),
        format!("{line} remix=no")] {
        assert_eq!(Module::matching_id(&foreign, "module-remap-source", &args), None);
    }
}

#[test]
fn native_sink_options_are_not_applied_to_pipewire_or_unsupported_servers() {
    assert_eq!(native_pulse("Server Name: pulseaudio\nServer Version: 13.0"), Ok(true));
    assert_eq!(native_pulse("Server Name: PulseAudio (on PipeWire 1.6.9)\nServer Version: 15.0.0"), Ok(false));
    assert!(native_pulse("Server Name: pulseaudio\nServer Version: 12.2").is_err());
    assert!(native_pulse("Server Name: unsupported\nServer Version: 99.0").is_err());
    assert!(native_pulse("").is_err());
}

#[test]
fn concurrent_track_claims_have_only_one_current_owner() {
    let session = Session::new();
    let claimed = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2).map(|_| {
        let (session, claimed) = (session.clone(), claimed.clone());
        std::thread::spawn(move || {
            let (generation, mut owner) = session.claim_track();
            claimed.wait(); // both workers start receiving only after both tracks have claimed
            let current = *owner.borrow_and_update() == generation;
            (generation, current)
        })
    }).collect();
    let owners: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(owners.iter().filter(|(_, current)| *current).count(), 1);
    assert!(owners.contains(&(2, true)));
    assert!(owners.contains(&(1, false)));
}

#[tokio::test]
async fn congested_reply_cannot_overwrite_a_later_close() {
    let session = Session::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (pending, wait_pending) = tokio::sync::oneshot::channel();
    let (resume, wait_resume) = tokio::sync::oneshot::channel();
    let sending = tokio::spawn(async move {
        let notice = rx.recv().await.unwrap();
        pending.send(()).unwrap();
        wait_resume.await.unwrap(); // the ordered outbox is blocked in its network send
        notice
    });
    session.action_with(23, || { tx.send(23).unwrap(); });
    wait_pending.await.unwrap();
    assert!(*session.open.borrow());
    session.action(24); // independent TEXT callback closes while the CONTROL reply is pending
    assert!(!*session.open.borrow());
    resume.send(()).unwrap();
    assert_eq!(sending.await.unwrap(), 23);
    assert!(!*session.open.borrow(), "completing an old send must not apply an old policy");
}

#[test]
fn state_and_notice_enqueue_are_serialized_across_channels() {
    let session = Session::new();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let (inside, entered) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let open = {
        let (session, notices, release) = (session.clone(), notices.clone(), release.clone());
        std::thread::spawn(move || { session.action_with(23, || {
            inside.send(()).unwrap();
            release.wait();
            notices.lock().unwrap().push(23);
        }); })
    };
    entered.recv().unwrap();
    let close = {
        let (session, notices) = (session.clone(), notices.clone());
        std::thread::spawn(move || { session.action_with(24, || { notices.lock().unwrap().push(24); }); })
    };
    release.wait();
    open.join().unwrap(); close.join().unwrap();
    assert_eq!(*notices.lock().unwrap(), [23, 24]);
    assert!(!*session.open.borrow());
}

#[tokio::test]
async fn shutdown_waits_for_receivers_before_removing_endpoints() {
    const CHILD: &str = "UU_MICROPHONE_TEST_SHUTDOWN";
    if std::env::var_os(CHILD).is_none() {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["microphone::tests::shutdown_waits_for_receivers_before_removing_endpoints", "--exact"])
            .env(CHILD, "1").output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
        return;
    }
    let worker = Worker::new();
    let stopped = Arc::new(AtomicBool::new(false));
    let finished = stopped.clone();
    let task = tokio::spawn(async move {
        let mut stopping = workers().stopped.subscribe();
        stopping.wait_for(|stopped| *stopped).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        finished.store(true, Ordering::Release);
        drop(worker);
    });
    tokio::time::timeout(Duration::from_secs(1), shutdown()).await.unwrap();
    assert!(stopped.load(Ordering::Acquire));
    task.await.unwrap();
}

#[tokio::test]
async fn webrtc_microphone_obeys_mute_desktop_role_and_disconnect() {
    if !have_opus() { return; }
    tokio::time::timeout(Duration::from_secs(15), async {
        let (left, right, router) = crate::test_network::virtual_network().await;
        async fn peer(settings: webrtc::api::setting_engine::SettingEngine, registry: Registry) -> Arc<RTCPeerConnection> {
            let mut m = webrtc::api::media_engine::MediaEngine::default();
            m.register_default_codecs().unwrap();
            Arc::new(webrtc::api::APIBuilder::new().with_media_engine(m).with_setting_engine(settings).with_interceptor_registry(registry)
                .build().new_peer_connection(Default::default()).await.unwrap())
        }
        let pause = Arc::new(ReadPause::default());
        let sender = peer(left, Registry::new()).await;
        let host = peer(right, pause_registry(pause.clone())).await;
        // Match the real host: its existing desktop sound track negotiates a bidirectional m-line.
        host.add_track(Arc::new(TrackLocalStaticRTP::new(RTCRtpCodecCapability {
            mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, ..Default::default()
        }, "desktop-sound".into(), "host".into()))).await.unwrap();
        let track = Arc::new(TrackLocalStaticRTP::new(RTCRtpCodecCapability {
            mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, ..Default::default()
        }, "mic".into(), "generated-tone".into()));
        sender.add_track(track.clone()).await.unwrap();
        let replacement = Arc::new(TrackLocalStaticRTP::new(RTCRtpCodecCapability {
            mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, ..Default::default()
        }, "replacement-mic".into(), "generated-tone".into()));
        sender.add_track(replacement.clone()).await.unwrap();
        let (created, mut outputs) = tokio::sync::mpsc::unbounded_channel();
        let session = Session { factory: Arc::new(move || {
            let (writer, reader) = tokio::io::duplex(65536);
            created.send(reader).unwrap();
            Ok(Playback { decoder: Decoder::new()?, writer: Box::pin(writer), output: None })
        }), ..Session::new() };
        assert!(!session.action(2));
        assert!(session.action(24));
        let (desktop, active) = watch::channel(true);
        session.attach(&host, active);
        let mut gathered = sender.gathering_complete_promise().await;
        let offer = sender.create_offer(None).await.unwrap();
        sender.set_local_description(offer).await.unwrap(); gathered.recv().await;
        host.set_remote_description(sender.local_description().await.unwrap()).await.unwrap();
        let mut gathered = host.gathering_complete_promise().await;
        let answer = host.create_answer(None).await.unwrap();
        host.set_local_description(answer).await.unwrap(); gathered.recv().await;
        sender.set_remote_description(host.local_description().await.unwrap()).await.unwrap();
        while sender.connection_state() != RTCPeerConnectionState::Connected { tokio::time::sleep(Duration::from_millis(10)).await; }
        let data = tone();
        let sending = tokio::spawn(async move {
            for seq in 0..500 { let _ = track.write_rtp(&packet(seq, &data)).await; tokio::time::sleep(Duration::from_millis(20)).await; }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(outputs.try_recv().is_err()); // muted before the first packet
        session.action(23);
        let mut pcm = outputs.recv().await.unwrap();
        let mut frame = vec![0; 3840];
        pcm.read_exact(&mut frame).await.unwrap();
        assert!(rms(&frame) > 1000.);
        // Playback must keep pace with a continuous 20 ms source, not add a polling delay
        // to every frame (which eventually overflows the jitter buffer and skips speech).
        let mut played_frames = 0;
        let _ = tokio::time::timeout(Duration::from_millis(600), async {
            loop { pcm.read_exact(&mut frame).await.unwrap(); played_frames += 1; }
        }).await;
        assert!((25..=35).contains(&played_frames), "wrong real-time playout rate: {played_frames} frames / 600 ms");
        pause.requested.store(true, Ordering::Release);
        pause.entered.notified().await;
        // Span several playout ticks and at least two health ticks with a consumed RTP packet.
        tokio::time::sleep(Duration::from_millis(250)).await;
        pause.resume.notify_one();
        tokio::time::timeout(Duration::from_secs(1), pause.completed.notified()).await
            .expect("timer ticks cancelled a pending RTP read and lost its packet");
        let data = tone();
        let replacing = tokio::spawn(async move {
            for seq in 0..500 { let _ = replacement.write_rtp(&packet(seq, &data)).await; tokio::time::sleep(Duration::from_millis(20)).await; }
        });
        let mut replacement_pcm = outputs.recv().await.unwrap();
        replacement_pcm.read_exact(&mut frame).await.unwrap();
        assert!(rms(&frame) > 1000.);
        tokio::time::timeout(Duration::from_secs(1), pcm.read_to_end(&mut Vec::new())).await.unwrap().unwrap();
        let mut pcm = replacement_pcm;
        session.action(24);
        let mut remainder = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), pcm.read_to_end(&mut remainder)).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(outputs.try_recv().is_err());
        desktop.send_replace(false);
        session.action(23);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(outputs.try_recv().is_err()); // a demoted desktop cannot inject audio
        desktop.send_replace(true);
        let mut resumed = outputs.recv().await.unwrap();
        resumed.read_exact(&mut frame).await.unwrap();
        assert!(rms(&frame) > 1000.);
        host.close().await.unwrap(); sender.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), resumed.read_to_end(&mut Vec::new())).await.unwrap().unwrap();
        sending.abort(); replacing.abort(); router.lock().await.stop().await.unwrap();
    }).await.expect("microphone WebRTC integration timed out");
}

/// Explicit opt-in only; records the generated tone from our isolated virtual source in memory.
#[tokio::test]
#[ignore]
async fn pipewire_virtual_microphone_receives_generated_tone() {
    assert!(requested(), "run with UU_MICROPHONE=1; the test creates independent audio endpoints");
    let sink = format!("uu-mic-test-input-{}", std::process::id());
    let source = format!("uu-mic-test-source-{}", std::process::id());
    let route = Route::new(&sink, &source).unwrap();
    let mut recorder = tokio::process::Command::new("parec").args(["--raw", "--format=s16le", "--rate=48000", "--channels=2",
        &format!("--device={source}"), "--latency-msec=20"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let mut capture = recorder.stdout.take().unwrap();
    let mut player = Playback::at(&sink).unwrap();
    let (stop, mut stopped) = watch::channel(false);
    let generating = tokio::spawn(async move {
        let data = tone();
        for _ in 0..100 {
            if *stopped.borrow() { break; }
            player.write(&Frame::Opus(data.clone())).await.unwrap();
            tokio::select! { _ = stopped.changed() => break, _ = tokio::time::sleep(Duration::from_millis(20)) => {} }
        }
        player.stop().await;
    });
    let received = tokio::time::timeout(Duration::from_secs(4), async {
        let mut frame = vec![0; 3840];
        loop { capture.read_exact(&mut frame).await?; if rms(&frame) > 1000. { return Ok::<_, std::io::Error>(()); } }
    }).await;
    let inputs = pactl(&["--format=json", "list", "sink-inputs"]);
    let devices = [pactl(&["--format=json", "list", "sinks"]), pactl(&["--format=json", "list", "sources"])];
    let stopped = recorder.kill().await;
    stop.send_replace(true);
    let generated = generating.await;
    drop(route);
    let sources = pactl(&["list", "short", "sources"]).unwrap();
    assert!(!sources.lines().any(|line| line.split('\t').nth(1) == Some(source.as_str())));
    std::fs::remove_file(route_lock_path(&source).unwrap()).unwrap();
    stopped.unwrap();
    assert!(!generated.is_err_and(|e| e.is_panic()), "tone generator panicked");
    received.expect("generated tone did not reach the virtual microphone").unwrap();
    let inputs: serde_json::Value = serde_json::from_str(&inputs.unwrap()).unwrap();
    let pid = std::process::id().to_string();
    let properties = inputs.as_array().unwrap().iter().find_map(|input| {
        let properties = input.get("properties")?;
        (properties.get("application.process.id")?.as_str()? == pid
            && properties.get("media.name")?.as_str()? == crate::i18n::s("远程麦克风", "Remote-Microphone")).then_some(properties)
    }).expect("localized microphone stream missing");
    assert_eq!(properties["application.name"], crate::i18n::s("UU 远程麦克风", "UU Remote microphone"));
    for (data, name, description) in [
        (&devices[0], &sink, crate::i18n::s("UU-远程麦克风输入", "UU-Remote-Microphone-Input")),
        (&devices[1], &source, crate::i18n::s("UU-远程麦克风", "UU-Remote-Microphone")),
    ] {
        let devices: serde_json::Value = serde_json::from_str(data.as_ref().unwrap()).unwrap();
        let device = devices.as_array().unwrap().iter().find(|device| device["name"] == name.as_str()).unwrap();
        assert_eq!(device["description"], description);
    }
}

/// Run through contrib/check-microphone-pulse.py, never against the desktop's audio server.
#[tokio::test]
#[ignore]
async fn pulse_target_loss_never_routes_to_default_output() {
    assert_eq!(std::env::var("UU_MICROPHONE_ISOLATED_TEST").as_deref(), Ok("1"));
    assert!(std::env::var("PULSE_SERVER").is_ok_and(|s| s.starts_with("unix:")));
    assert!(pactl(&["list", "short", "sinks"]).unwrap().is_empty(), "requires an empty isolated server");
    assert!(pactl(&["list", "short", "sources"]).unwrap().is_empty(), "requires an empty isolated server");
    let fallback_name = format!("uu-mic-test-default-{}", std::process::id());
    let fallback = Module::open("module-null-sink", sink_args(&fallback_name).unwrap()).unwrap();
    struct Fallback(Module);
    impl Drop for Fallback { fn drop(&mut self) { self.0.unload(); } }
    let _fallback = Fallback(fallback);
    pactl(&["set-default-sink", &fallback_name]).unwrap();
    let sink = format!("uu-mic-test-pinned-{}", std::process::id());
    let source = format!("uu-mic-test-pinned-source-{}", std::process::id());
    let route = Route::new(&sink, &source).unwrap();
    let mut player = Playback::at(&sink).unwrap();
    let generating = tokio::spawn(async move {
        let frame = Frame::Opus(tone());
        let mut failed_closed = false;
        for _ in 0..150 {
            if player.write(&frame).await.is_err() { failed_closed = true; break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        player.stop().await;
        failed_closed
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while pactl(&["list", "short", "sink-inputs"]).unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let mut probe = tokio::process::Command::new("parec").args(["--raw", "--format=s16le", "--rate=48000", "--channels=2",
        &format!("--device={source}"), "--latency-msec=20"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let mut baseline = probe.stdout.take().unwrap();
    let flowing = tokio::time::timeout(Duration::from_secs(3), async {
        let mut frame = vec![0; 3840];
        let mut audible = 0;
        loop {
            baseline.read_exact(&mut frame).await?;
            audible = if rms(&frame) > 1000. { audible + 1 } else { 0 };
            if audible >= 15 { return Ok::<_, std::io::Error>(()); } // 300 ms of healthy playback
        }
    }).await;
    let probe_stopped = probe.kill().await;
    if !matches!(&flowing, Ok(Ok(()))) {
        generating.abort(); let _ = generating.await;
        panic!("no sustained generated tone before removing the target: {flowing:?}");
    }
    probe_stopped.unwrap();
    flowing.expect("no sustained generated tone before removing the target").unwrap();
    let mut recorder = tokio::process::Command::new("parec").args(["--raw", "--format=s16le", "--rate=48000", "--channels=2",
        &format!("--device={fallback_name}.monitor"), "--latency-msec=20"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let mut capture = recorder.stdout.take().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!generating.is_finished(), "playback failed before the target was removed");
    route.source.unload(); route.sink.unload(); // remove the target while audio is still flowing
    let mut generating = generating;
    let ended = tokio::time::timeout(Duration::from_secs(2), &mut generating).await;
    if ended.is_err() { generating.abort(); let _ = generating.await; }
    let mut peak = 0.0_f64;
    let observed = async {
        let deadline = tokio::time::sleep(Duration::from_millis(350));
        tokio::pin!(deadline);
        let mut frame = vec![0; 3840];
        loop { tokio::select! {
            _ = &mut deadline => break,
            read = capture.read_exact(&mut frame) => { read?; peak = peak.max(rms(&frame)); },
        } }
        Ok::<_, std::io::Error>(())
    }.await;
    recorder.kill().await.unwrap();
    observed.unwrap();
    assert!(ended.expect("playback did not stop after its target disappeared").unwrap());
    assert!(pactl(&["list", "short", "sink-inputs"]).unwrap().is_empty());
    assert_eq!(peak, 0.0, "microphone audio reached the default output");
    drop(route);
    std::fs::remove_file(route_lock_path(&source).unwrap()).unwrap();
}
