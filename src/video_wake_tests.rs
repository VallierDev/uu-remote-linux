//! Exercise the real video loop and RTP transport without cloud or desktop access.
//! Thread-local faults are confined to each test's current-thread Tokio runtime.
use super::*;
use std::cell::RefCell;
use webrtc::{api::{APIBuilder, media_engine::MediaEngine},
    peer_connection::configuration::RTCConfiguration,
    rtp_transceiver::rtp_codec::RTPCodecType};

pub(super) const DISPLAY: &str = "wayland:test-wake";
struct Faults { wake_calls: AtomicUsize, fail_wake: usize, fail_capture: bool, stop: AtomicBool }
thread_local! { static FAULTS: RefCell<Option<Arc<Faults>>> = const { RefCell::new(None) }; }
struct Reset;
impl Drop for Reset { fn drop(&mut self) { FAULTS.with(|value| *value.borrow_mut() = None); } }
pub(super) fn active() -> bool { FAULTS.with(|value| value.borrow().is_some()) }
pub(super) fn wake() -> Option<Result<(), String>> {
    FAULTS.with(|value| value.borrow().as_ref().map(|faults| {
        let call = faults.wake_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == faults.fail_wake { Err("Wayland output power control unavailable".into()) } else { Ok(()) }
    }))
}
pub(super) async fn encode(latest: mpsc::Sender<Arc<CompleteFrame>>, settings: VideoSettings,
    sequence: Arc<std::sync::atomic::AtomicU64>, budget: Arc<Semaphore>) -> Result<(), String> {
    let faults = FAULTS.with(|value| value.borrow().clone()).expect("test faults installed");
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if faults.stop.load(Ordering::SeqCst) { return Ok(()); }
        if faults.fail_capture && sequence.load(Ordering::Relaxed) >= 4 {
            return Err("synthetic capture failed".into());
        }
        let permit = budget.clone().acquire_owned().await.map_err(|_| "test budget closed")?;
        let frame = Arc::new(CompleteFrame {
            source_generation: settings.source_generation, source_id: settings.source_id,
            codec: settings.codec, sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
            completed_at: Instant::now(), data: vec![0, 0, 1, 65, 1, 2], _permit: Some(permit),
        });
        if latest.send(frame).await.is_err() { return Ok(()); }
    }
}

async fn exercise(fail_wake: usize, fail_capture: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let faults = Arc::new(Faults { wake_calls: AtomicUsize::new(0), fail_wake, fail_capture, stop: AtomicBool::new(false) });
        FAULTS.with(|value| *value.borrow_mut() = Some(faults.clone()));
        let _reset = Reset;
        let (host_setting, remote_setting, router) = crate::test_network::virtual_network().await;
        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let host = Arc::new(APIBuilder::new().with_setting_engine(host_setting).with_media_engine(media)
            .build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let remote = APIBuilder::new().with_setting_engine(remote_setting).with_media_engine(media)
            .build().new_peer_connection(RTCConfiguration::default()).await.unwrap();
        remote.add_transceiver_from_kind(RTPCodecType::Video, None).await.unwrap();
        let _control = remote.create_data_channel("CONTROL_DATA_CHANNEL", None).await.unwrap();
        let track = Arc::new(crate::video_track::VideoTrack::new());
        host.add_track(track.clone()).await.unwrap();
        let (packets, mut received) = mpsc::unbounded_channel();
        remote.on_track(Box::new(move |track, _, _| {
            let packets = packets.clone();
            Box::pin(async move {
                tokio::spawn(async move {
                    while let Ok((packet, _)) = track.read_rtp().await {
                        if packets.send(packet).is_err() { break; }
                    }
                });
            })
        }));
        let mut gathered = remote.gathering_complete_promise().await;
        remote.set_local_description(remote.create_offer(None).await.unwrap()).await.unwrap();
        gathered.recv().await;
        host.set_remote_description(remote.local_description().await.unwrap()).await.unwrap();
        let mut gathered = host.gathering_complete_promise().await;
        host.set_local_description(host.create_answer(None).await.unwrap()).await.unwrap();
        gathered.recv().await;
        remote.set_remote_description(host.local_description().await.unwrap()).await.unwrap();
        let settings = VideoSettings { source_id: 0, source_generation: 0, ..Default::default() };
        let (settings_tx, settings_rx) = tokio::sync::watch::channel(settings);
        let selection = crate::video_select::resolve_pair(Some("cpu"), None, Some("h264"), None, false, None);
        let mut task = tokio::spawn(stream_desktop(host.clone(), track.clone(), (640, 480),
            DISPLAY.into(), None, settings_rx, Arc::new(settings_tx), Arc::new(AtomicBool::new(false)), selection, None));
        let result = if fail_capture {
            Some((&mut task).await.unwrap())
        } else {
            let mut previous: Option<u16> = None;
            for _ in 0..6 {
                tokio::select! {
                    result = &mut task => panic!("video stopped after a recoverable wake failure: {result:?}"),
                    packet = received.recv() => {
                        let packet = packet.expect("RTP receiver closed");
                        if let Some(sequence) = previous {
                            assert_eq!(packet.header.sequence_number, sequence.wrapping_add(1));
                        }
                        previous = Some(packet.header.sequence_number);
                    }
                }
            }
            assert!(faults.wake_calls.load(Ordering::SeqCst) > fail_wake, "wake was not retried");
            None
        };
        assert!(track.revision() >= 4, "video must continue across wake failure");
        let result = match result {
            Some(result) => result,
            None => {
                faults.stop.store(true, Ordering::SeqCst);
                task.await.unwrap()
            },
        };
        host.close().await.unwrap();
        remote.close().await.unwrap();
        router.lock().await.stop().await.unwrap();
        assert_eq!(result, if fail_capture { Err("synthetic capture failed".into()) } else { Ok(()) });
    }).await.expect("video wake regression timed out");
}

#[tokio::test]
async fn startup_wake_failure_does_not_stop_video() { exercise(1, false).await; }

#[tokio::test]
async fn periodic_wake_failure_does_not_stop_video() { exercise(2, false).await; }

#[tokio::test]
async fn capture_failure_still_stops_video_after_wake_failure() { exercise(2, true).await; }
