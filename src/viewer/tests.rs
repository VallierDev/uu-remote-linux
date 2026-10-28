use super::{api::JoinData,session::{Session,Options},signal::Signal,rtc::{self,Peer,ReadyPolicy},wire::{self,Shape}};
use serde_json::{json,Value};
use std::{sync::{Arc,atomic::{AtomicUsize,Ordering}},time::Duration};
use tokio::{net::TcpListener,sync::{Mutex,mpsc}};
use futures_util::{SinkExt,StreamExt};
use tokio_tungstenite::{accept_async,tungstenite::Message};
use webrtc::{api::{APIBuilder,media_engine::MediaEngine,setting_engine::SettingEngine},peer_connection::{configuration::RTCConfiguration,sdp::session_description::RTCSessionDescription},util::vnet::{net::{Net,NetConfig},router::{Router,RouterConfig}}};

async fn virtual_network()->(SettingEngine,SettingEngine,Arc<Mutex<Router>>) {
    // RFC 5737 documentation addresses live only inside webrtc's in-memory network.
    let router=Arc::new(Mutex::new(Router::new(RouterConfig{cidr:"192.0.2.0/24".into(),..Default::default()}).unwrap()));
    let mut settings=Vec::new();
    for address in ["192.0.2.10","192.0.2.11"] {
        let net=Arc::new(Net::new(Some(NetConfig{static_ips:vec![address.into()],..Default::default()})));
        let nic=net.get_nic().unwrap();router.lock().await.add_net(nic.clone()).await.unwrap();
        nic.lock().await.set_router(router.clone()).await.unwrap();
        let mut setting=SettingEngine::default();setting.set_vnet(Some(net));
        setting.set_network_types(vec![webrtc::ice::network_type::NetworkType::Udp4]);
        settings.push(setting);
    }
    router.lock().await.start().await.unwrap();
    let b=settings.pop().unwrap();(settings.pop().unwrap(),b,router)
}
fn shape()->Shape {serde_json::from_str(include_str!("../../tests/fixtures/viewer/session-shape.json")).unwrap()}

#[test]
fn ice_configuration_preserves_credentials_without_public_fallback() {
    let result=json!({"iceServers":[{"urls":"turn:relay.example.invalid:3478","username":"synthetic-user","credential":"synthetic-pass"}],"force_relay":true});
    let config=rtc::configuration(&result).unwrap();assert_eq!(config.ice_servers.len(),1);
    assert_eq!(config.ice_servers[0].credential,"synthetic-pass");
    assert_eq!(config.ice_transport_policy,webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy::Relay);
    assert!(rtc::configuration(&json!({"iceServers":[]})).unwrap().ice_servers.is_empty());
    for result in [json!({}),json!({"iceServers":[],"force_relay":true}),json!({"iceServers":[{"urls":"https://invalid.example"}]}),json!({"iceServers":[{"urls":[7]}]})] {assert!(rtc::configuration(&result).is_err());}
}

#[tokio::test]
async fn websocket_control_ack_framing_heartbeat_failure_and_timeout() {
    for legacy in [false,true] {
        let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).await.unwrap();
        let url=format!("ws://{}",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {
            let(stream,_)=listener.accept().await.unwrap();let mut ws=accept_async(stream).await.unwrap();
            ws.send(Message::Text("0{}".into())).await.unwrap();assert_eq!(ws.next().await.unwrap().unwrap(),Message::Text("40".into()));
            ws.send(Message::Text("40{}".into())).await.unwrap();
            let Message::Text(t)=ws.next().await.unwrap().unwrap() else{panic!()};assert!(t.starts_with("451-11["));
            let Message::Binary(b)=ws.next().await.unwrap().unwrap() else{panic!()};assert_eq!(b,if legacy{vec![4,8,1]}else{vec![8,1]});
            ws.send(Message::Text("2fixture".into())).await.unwrap();assert_eq!(ws.next().await.unwrap().unwrap(),Message::Text("3fixture".into()));
            ws.send(Message::Text("43999[\"success\",{}]".into())).await.unwrap();
            ws.send(Message::Text("4311[\"fail\",{}]".into())).await.unwrap();
        });
        let mut signal=Signal::connect(&["invalid endpoint".into(),url],"synthetic-ticket",Duration::from_millis(100),legacy).await.unwrap();
        assert!(signal.control(json!({}),vec![8,1],Duration::from_secs(2)).await.unwrap_err().contains("refused"));signal.close().await;server.await.unwrap();
    }
    let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).await.unwrap();let url=format!("ws://{}",listener.local_addr().unwrap());
    let job=tokio::spawn(async move {
        let(s,_)=listener.accept().await.unwrap();let mut ws=accept_async(s).await.unwrap();ws.send(Message::Text("0{}".into())).await.unwrap();let _=ws.next().await;
        ws.send(Message::Text("40{}".into())).await.unwrap();while ws.next().await.is_some(){}
    });
    let mut signal=Signal::connect(&[url],"synthetic-ticket",Duration::from_secs(2),false).await.unwrap();
    assert!(signal.control(json!({}),vec![],Duration::from_millis(20)).await.unwrap_err().contains("timed out"));signal.close().await;job.await.unwrap();
}

#[tokio::test]
async fn complete_session_uses_loopback_signaling_and_virtual_rtc_through_echo() {
    tokio::time::timeout(Duration::from_secs(20),async {
        let(viewer_settings,host_settings,router)=virtual_network().await;
        let mut engine=MediaEngine::default();engine.register_default_codecs().unwrap();
        let host=Arc::new(APIBuilder::new().with_media_engine(engine).with_setting_engine(host_settings).build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
        use webrtc::track::track_local::{track_local_static_rtp::TrackLocalStaticRTP,TrackLocal,TrackLocalWriter};
        let track=Arc::new(TrackLocalStaticRTP::new(webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability{mime_type:"video/H264".into(),clock_rate:90000,sdp_fmtp_line:"level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(),..Default::default()},"synthetic-video".into(),"synthetic-stream".into()));
        let sender=host.add_track(track.clone() as Arc<dyn TrackLocal+Send+Sync>).await.unwrap();
        let (input_tx,mut input_rx)=mpsc::channel(8);
        let opened=Arc::new(AtomicUsize::new(0));let count=opened.clone();
        host.on_data_channel(Box::new(move |channel| {
            let count=count.clone();let input_tx=input_tx.clone();Box::pin(async move {
                assert!(wire::CHANNELS.contains(&channel.label()));assert!(channel.ordered());assert!(!channel.negotiated());
                let c=count.clone();channel.on_open(Box::new(move ||Box::pin(async move{c.fetch_add(1,Ordering::Relaxed);} )));
                if channel.label()==wire::CHANNELS[0] {
                    let weak=Arc::downgrade(&channel);
                    channel.on_message(Box::new(move |message| {let weak=weak.clone();let input_tx=input_tx.clone();Box::pin(async move {
                        if message.data.first()==Some(&b'{') {input_tx.send(serde_json::from_slice::<Value>(&message.data).unwrap()).await.unwrap();return}
                        for (action,number) in [(super::power::Action::Reboot,9),(super::power::Action::Shutdown,12)] {
                            if message.data.as_ref()==action.message(2).as_slice(){input_tx.send(json!({"power":number})).await.unwrap();return}
                        }
                        // Desktop sessions also request the video track (RPC 15), like the official viewer.
                        if message.data.as_ref()==wire::request_video_track().as_slice() {return}
                        assert_eq!(&message.data[..],&wire::echo_request(1));
                        let mut action=Vec::new();wire::int(1,1,&mut action);wire::bytes(2,br#"{"seq":1}"#,&mut action);
                        if let Some(channel)=weak.upgrade(){channel.send(&bytes::Bytes::from(wire::envelope(100,3,&action))).await.unwrap();}
                    })}));
                } else if [wire::CHANNELS[3],wire::CHANNELS[4]].contains(&channel.label()) {
                    let weak=Arc::downgrade(&channel);
                    channel.on_message(Box::new(move |message|{let weak=weak.clone();Box::pin(async move{
                        if let Some(dc)=weak.upgrade(){dc.send(&message.data).await.unwrap();}
                    })}));
                }
            })
        }));
        let(tx,mut candidates)=mpsc::channel(256);
        host.on_ice_candidate(Box::new(move |candidate| {let tx=tx.clone();Box::pin(async move {
            if let Some(candidate)=candidate {let mut c=candidate.to_json().unwrap();c.sdp_mid=Some("0".into());c.sdp_mline_index=Some(0);tx.send(c).await.unwrap();}
        })}));
        let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).await.unwrap();let url=format!("ws://{}",listener.local_addr().unwrap());
        let server_host=host.clone();let(done_tx,mut done_rx)=mpsc::channel::<()>(1);
        let server=tokio::spawn(async move {
            let(stream,_)=listener.accept().await.unwrap();
            let mut ws=tokio_tungstenite::accept_hdr_async(stream,|request:&tokio_tungstenite::tungstenite::handshake::server::Request,response| {
                assert_eq!(request.headers()["X-NRD-AUTH"],"synthetic-room-ticket");assert_eq!(request.uri().path(),"/socket.io/");Ok(response)
            }).await.unwrap();
            ws.send(Message::Text("0{\"sid\":\"fixture\"}".into())).await.unwrap();assert_eq!(ws.next().await.unwrap().unwrap(),Message::Text("40".into()));ws.send(Message::Text("40{}".into())).await.unwrap();
            let Message::Text(packet)=ws.next().await.unwrap().unwrap() else{panic!()};
            let begin=packet.find('[').unwrap();let ack=packet[4..begin].parse::<u64>().unwrap();let args:Value=serde_json::from_str(&packet[begin..]).unwrap();assert_eq!(args[0],"control");
            let app=args[1]["app_control_id"].clone();
            // As observed live: the viewer sends no ice_id and the cloud assigns one in the ack.
            assert!(args[1].get("ice_id").is_none()&&args[1].get("client_id").is_none()&&args[1].get("controller_platform").is_none());
            let ice=json!("synthetic-cloud-ice");
            let streamer:Value=serde_json::from_str(args[1]["streamer_data"].as_str().unwrap()).unwrap();assert_eq!(streamer["control_id"],app);
            let Message::Binary(b)=ws.next().await.unwrap().unwrap() else{panic!()};assert!(crate::protocol::decode_wire(&b).is_ok());
            // A queued foreign candidate must not be installed into this PC.
            ws.send(Message::Text(format!("42{}",json!(["soac",{"data":{"type":"candidate","ice_id":"synthetic-other-session","candidate":"invalid"}}])))).await.unwrap();
            ws.send(Message::Text(format!("43{ack}{}",json!(["success",{"iceServers":[],"client_id":"synthetic-host","app_control_id":app,"ice_id":ice}])))).await.unwrap();
            loop {
                tokio::select! {
                    _=done_rx.recv()=>break,
                    candidate=candidates.recv()=>if let Some(candidate)=candidate {
                        ws.send(Message::Text(format!("42{}",json!(["soac",{"client_id":"synthetic-host","data":{"type":"candidate","candidate":candidate,"app_control_id":app,"ice_id":ice}}])))).await.unwrap();
                    },
                    message=ws.next()=> {
                        let Some(Ok(Message::Text(packet)))=message else{break};
                        let value:Value=serde_json::from_str(&packet[2..]).unwrap();let body=&value[1]["data"];
                        match body["type"].as_str() {
                            Some("offer")=> {
                                let offered=body["sdp"].as_str().unwrap();
                                assert!(offered.contains("H264/90000") && offered.contains("opus/48000"));
                                assert!(offered.contains("H265/90000") && !offered.contains("VP8"));
                                assert_eq!(offered.matches("a=recvonly").count(),2);
                                server_host.set_remote_description(RTCSessionDescription::offer(offered.into()).unwrap()).await.unwrap();
                                let answer=server_host.create_answer(None).await.unwrap();let sdp=answer.sdp.clone();let mut gathered=server_host.gathering_complete_promise().await;server_host.set_local_description(answer).await.unwrap();
                                gathered.recv().await;
                                // Force one real remote candidate to arrive before the answer.
                                let candidate=candidates.recv().await.unwrap();
                                ws.send(Message::Text(format!("42{}",json!(["soac",{"client_id":"synthetic-host","data":{"type":"candidate","candidate":candidate,"app_control_id":app,"ice_id":ice}}])))).await.unwrap();
                                ws.send(Message::Text(format!("42{}",json!(["soac",{"client_id":"synthetic-host","data":{"type":"answer","sdp":sdp,"ice_id":ice,"app_control_id":app}}])))).await.unwrap();
                            }
                            Some("candidate")=>server_host.add_ice_candidate(serde_json::from_value(body["candidate"].clone()).unwrap()).await.unwrap(),
                            _=>panic!("unexpected synthetic event"),
                        }
                    }
                }
            }
        });
        let room:JoinData=serde_json::from_value(json!({"token":"synthetic-room-ticket","signaling_list":[url],"report_token":"synthetic-report","report_url":""})).unwrap();
        let options=Options{timeout:Duration::from_secs(3),legacy_prefix:false,ready_policy:ReadyPolicy::Explicit,clipboard:super::clipboard::Bridge::new(false)};
        let mut session=Session::open(&room,&shape(),"synthetic-device","synthetic-client",options,viewer_settings).await.unwrap();
        assert_eq!(session.peer.channels.len(),5);
        let(tool_io,mut tool_rx)=super::tools::attach(&session.peer).unwrap();
        assert!(super::tools::attach(&session.peer).is_err());
        loop {
            tokio::select! {_=tokio::time::sleep(Duration::from_millis(20))=>{},result=session.step()=>result.unwrap()}
            if session.peer.ready(ReadyPolicy::EchoOnly,false) && opened.load(Ordering::Relaxed)==5 {break}
        }
        assert!(!session.peer.ready(ReadyPolicy::Explicit,false));assert!(session.peer.ready(ReadyPolicy::Explicit,true));
        assert!(session.peer.observed.control_open.load(Ordering::Acquire));
        for channel in [super::tools::FILE,super::tools::BINARY] {
            let data=vec![channel as u8;32*1024];tool_io.send(channel,data.clone()).await.unwrap();
            loop{let(c,b)=tool_rx.recv().await.unwrap();if c==channel{assert_eq!(b,data);break}}
        }
        let mut input=super::input::Input::default();
        let press=input.key(4,true).unwrap();session.peer.send_input(&press).await.unwrap();assert_eq!(input_rx.recv().await.unwrap(),press);
        for release in input.release_all(){session.peer.send_input(&release).await.unwrap();assert_eq!(input_rx.recv().await.unwrap(),release);}
        let heartbeat=super::input::Input::heartbeat();session.peer.send_input(&heartbeat).await.unwrap();assert_eq!(input_rx.recv().await.unwrap(),heartbeat);
        let packet=|seq,ts,marker,payload:&[u8]|webrtc::rtp::packet::Packet{header:webrtc::rtp::header::Header{version:2,sequence_number:seq,timestamp:ts,marker,..Default::default()},payload:bytes::Bytes::copy_from_slice(payload)};
        track.write_rtp(&packet(9,1,true,&[0x65,1])).await.unwrap();
        loop {if let Some(au)=session.peer.desktop.units.pop(){assert_eq!(au.codec,super::media::Codec::H264);assert_eq!(au.data,vec![0,0,0,1,0x65,1]);break}tokio::time::sleep(Duration::from_millis(10)).await;}
        track.write_rtp(&packet(10,2,false,&[0x7c,0x85,1])).await.unwrap();
        track.write_rtp(&packet(12,2,true,&[0x7c,0x45,3])).await.unwrap();
        loop {let (packets,_)=sender.read_rtcp().await.unwrap();if packets.iter().any(|p|p.as_any().is::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()){break}}
        assert!(session.peer.desktop.units.pop().is_none());
        for (action,number) in [(super::power::Action::Reboot,9),(super::power::Action::Shutdown,12)]{
            super::power::run(&mut session,action).await.unwrap();
            assert_eq!(input_rx.recv().await.unwrap(),json!({"power":number}));
            assert!(input_rx.try_recv().is_err());
        }
        done_tx.send(()).await.unwrap();server.await.unwrap();session.close().await;
        assert!(tool_rx.recv().await.is_none());
        host.close().await.unwrap();router.lock().await.stop().await.unwrap();
    }).await.expect("offline RTC/session timed out");
}

#[tokio::test]
async fn early_candidates_and_foreign_session_messages_are_scoped() {
    let(settings,_,router)=virtual_network().await;let(tx,_rx)=mpsc::channel(256);
    let mut peer=Peer::new(RTCConfiguration::default(),&shape(),"synthetic-app","synthetic-ice","synthetic-host",tx,settings,super::clipboard::Bridge::new(false)).await.unwrap();
    assert!(!peer.matches_route(&json!({"ice_id":"foreign"})));
    assert!(!peer.matches_route(&json!({"app_control_id":"foreign"})));
    peer.receive(&json!({"type":"answer","ice_id":"foreign","sdp":"invalid"})).await.unwrap();
    for _ in 0..256 {peer.receive(&json!({"type":"candidate","candidate":{"candidate":""}})).await.unwrap();}
    assert!(peer.receive(&json!({"type":"candidate","candidate":{"candidate":""}})).await.is_err());
    peer.close().await;router.lock().await.stop().await.unwrap();
}

#[tokio::test]
async fn closing_one_peer_preserves_another_peer() {
    let(a,b,router)=virtual_network().await;
    let(tx1,_rx1)=mpsc::channel(256);let(tx2,_rx2)=mpsc::channel(256);
    let first=Peer::new(RTCConfiguration::default(),&shape(),"synthetic-first","synthetic-first-ice","synthetic-host",tx1,a,super::clipboard::Bridge::new(false)).await.unwrap();
    let second=Peer::new(RTCConfiguration::default(),&shape(),"synthetic-second","synthetic-second-ice","synthetic-host",tx2,b,super::clipboard::Bridge::new(false)).await.unwrap();
    first.close().await;
    assert_ne!(second.pc.connection_state(),webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed);
    assert_eq!(second.channels.len(),5);
    second.close().await;router.lock().await.stop().await.unwrap();
}

#[tokio::test]
async fn opus_and_video_tracks_are_received_independently_in_memory() {
    tokio::time::timeout(Duration::from_secs(10),async {
        let(viewer_settings,host_settings,router)=virtual_network().await;
        let mut engine=MediaEngine::default();engine.register_default_codecs().unwrap();
        let host=Arc::new(APIBuilder::new().with_media_engine(engine).with_setting_engine(host_settings).build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
        use webrtc::{track::track_local::{track_local_static_rtp::TrackLocalStaticRTP,TrackLocal,TrackLocalWriter},rtp_transceiver::rtp_codec::RTCRtpCodecCapability};
        let audio=Arc::new(TrackLocalStaticRTP::new(RTCRtpCodecCapability{mime_type:"audio/opus".into(),clock_rate:48000,channels:2,..Default::default()},"audio".into(),"test".into()));
        let video=Arc::new(TrackLocalStaticRTP::new(RTCRtpCodecCapability{mime_type:"video/H264".into(),clock_rate:90000,sdp_fmtp_line:"level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(),..Default::default()},"video".into(),"test".into()));
        host.add_track(audio.clone() as Arc<dyn TrackLocal+Send+Sync>).await.unwrap();host.add_track(video.clone() as Arc<dyn TrackLocal+Send+Sync>).await.unwrap();
        let(tx,_rx)=mpsc::channel(256);
        let peer=Peer::new(RTCConfiguration::default(),&shape().video_preferences(false),"app","ice","host",tx,viewer_settings,super::clipboard::Bridge::new(false)).await.unwrap();
        let mut gathered=peer.pc.gathering_complete_promise().await;peer.offer().await.unwrap();gathered.recv().await;
        let offer=peer.pc.local_description().await.unwrap();assert!(offer.sdp.contains("m=audio") && offer.sdp.contains("opus/48000/2"));
        host.set_remote_description(offer).await.unwrap();let answer=host.create_answer(None).await.unwrap();
        let mut gathered=host.gathering_complete_promise().await;host.set_local_description(answer).await.unwrap();gathered.recv().await;
        peer.pc.set_remote_description(host.local_description().await.unwrap()).await.unwrap();
        while peer.pc.connection_state()!=webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected {tokio::time::sleep(Duration::from_millis(10)).await;}
        let mut heard=false;let mut seen=false;
        for seq in 0..200 {
            let packet=|payload:&[u8]|webrtc::rtp::packet::Packet{header:webrtc::rtp::header::Header{version:2,sequence_number:seq,timestamp:u32::from(seq)*960,marker:true,..Default::default()},payload:bytes::Bytes::copy_from_slice(payload)};
            audio.write_rtp(&packet(&[0xf8,0xff,0xfe])).await.unwrap();video.write_rtp(&packet(&[0x65,1])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(data)=peer.audio.pop(){assert_eq!(data,vec![0xf8,0xff,0xfe]);heard=true;}
            if let Some(data)=peer.desktop.units.pop(){assert_eq!(data.codec,super::media::Codec::H264);assert_eq!(data.data,vec![0,0,0,1,0x65,1]);seen=true;}
            if heard && seen {break}
        }
        assert!(heard && seen);peer.close().await;host.close().await.unwrap();router.lock().await.stop().await.unwrap();
    }).await.expect("in-memory media timed out");
}

#[tokio::test]
async fn desktop_file_window_reattaches_without_replacing_peer(){
    tokio::time::timeout(Duration::from_secs(10),async{
        use super::tools::{self,files::Files};
        let(viewer_settings,host_settings,router)=virtual_network().await;
        let mut engine=MediaEngine::default();engine.register_default_codecs().unwrap();
        let host=Arc::new(APIBuilder::new().with_media_engine(engine).with_setting_engine(host_settings).build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
        let file:Arc<std::sync::Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>>=Default::default();
        let(tx,mut received)=mpsc::channel(32);
        let opened=Arc::new(AtomicUsize::new(0));let count=opened.clone();
        host.on_data_channel(Box::new(move|channel|{let(tx,file,count)=(tx.clone(),file.clone(),count.clone());Box::pin(async move{
            count.fetch_add(1,Ordering::Relaxed);
            if channel.label()==wire::CHANNELS[tools::FILE]{*file.lock().unwrap()=Some(channel.clone());}
            if channel.label()==wire::CHANNELS[tools::CONTROL]{channel.on_message(Box::new(move|m|{
                let(tx,file)=(tx.clone(),file.clone());Box::pin(async move{
                    if let Some(r)=tools::blob(&m.data,21){
                        if tools::blob(r,8).and_then(|b|tools::blob(b,10)).is_some(){
                            let id=tools::uint(tools::blob(r,1).unwrap(),1);
                            let reply=tools::rpc(id,true,7,&tools::ints(&[(4,1)]));
                            let file=file.lock().unwrap().clone().unwrap();file.send(&bytes::Bytes::from(reply)).await.unwrap();return
                        }
                    }
                    let _=tx.send(m.data.to_vec()).await;
                })
            }));}
        })}));
        let(tx,_rx)=mpsc::channel(256);
        let peer=Peer::new(RTCConfiguration::default(),&shape(),"app","ice","host",tx,viewer_settings,super::clipboard::Bridge::new(false)).await.unwrap();
        let mut gathered=peer.pc.gathering_complete_promise().await;peer.offer().await.unwrap();gathered.recv().await;
        host.set_remote_description(peer.pc.local_description().await.unwrap()).await.unwrap();
        let answer=host.create_answer(None).await.unwrap();let mut gathered=host.gathering_complete_promise().await;
        host.set_local_description(answer).await.unwrap();gathered.recv().await;
        peer.pc.set_remote_description(host.local_description().await.unwrap()).await.unwrap();
        while opened.load(Ordering::Relaxed)!=5 || peer.channels.iter().any(|c|c.ready_state()!=webrtc::data_channel::data_channel_state::RTCDataChannelState::Open){tokio::time::sleep(Duration::from_millis(10)).await;}
        let pc=peer.pc.clone();
        for _ in 0..2{
            let(io,rx)=tools::attach(&peer).unwrap();assert!(tools::attach(&peer).is_err());
            let(f,_)=Files::new(io,rx);assert!(f.list_dir(":/").await.unwrap().is_empty());drop(f);
            peer.tools.lock().unwrap().take();
            assert!(Arc::ptr_eq(&pc,&peer.pc));
            let heartbeat=super::input::Input::heartbeat();peer.send_input(&heartbeat).await.unwrap();
            loop{if serde_json::from_slice::<Value>(&received.recv().await.unwrap()).ok()==Some(heartbeat.clone()){break}}
        }
        assert_eq!(opened.load(Ordering::Relaxed),5);
        peer.close().await;host.close().await.unwrap();router.lock().await.stop().await.unwrap();
    }).await.expect("desktop file reuse timed out");
}

#[tokio::test]
async fn hevc_track_uses_negotiated_payload_type_in_memory(){
    tokio::time::timeout(Duration::from_secs(10),async {
        use super::media::Codec;
        use webrtc::{track::track_local::{track_local_static_rtp::TrackLocalStaticRTP,TrackLocal,TrackLocalWriter},
            rtp_transceiver::rtp_codec::{RTCRtpCodecCapability,RTCRtpCodecParameters,RTPCodecType}};
        let(viewer_settings,host_settings,router)=virtual_network().await;
        // The host uses a different local PT; the answer binds its track to the viewer offer.
        let mut engine=MediaEngine::default();
        let capability=RTCRtpCodecCapability{mime_type:"video/H265".into(),clock_rate:90000,..Default::default()};
        engine.register_codec(RTCRtpCodecParameters{capability:capability.clone(),payload_type:126,..Default::default()},RTPCodecType::Video).unwrap();
        let host=Arc::new(APIBuilder::new().with_media_engine(engine).with_setting_engine(host_settings).build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
        let video=Arc::new(TrackLocalStaticRTP::new(capability,"video".into(),"test".into()));
        host.add_track(video.clone() as Arc<dyn TrackLocal+Send+Sync>).await.unwrap();
        host.on_data_channel(Box::new(|channel|Box::pin(async move {
            if channel.label()=="CONTROL_DATA_CHANNEL" {
                let weak=Arc::downgrade(&channel);
                channel.on_open(Box::new(move||Box::pin(async move {
                    if let Some(channel)=weak.upgrade(){
                        let mut caps=Vec::new();for codec in [1,2]{let mut cap=Vec::new();wire::int(3,codec,&mut cap);wire::bytes(1,&cap,&mut caps);}
                        channel.send(&bytes::Bytes::from(wire::envelope(2,18,&caps))).await.unwrap();
                        // Model a host that declines 4:4:4 and encodes 4:2:0.
                        channel.send(&bytes::Bytes::from(wire::envelope(3,25,&[24,1]))).await.unwrap();
                    }
                })));
            }
        })));
        let(tx,_rx)=mpsc::channel(256);
        let peer=Peer::new(RTCConfiguration::default(),&shape().video_preferences(false),"app","ice","host",tx,viewer_settings,super::clipboard::Bridge::new(false)).await.unwrap();
        let mut gathered=peer.pc.gathering_complete_promise().await;peer.offer().await.unwrap();gathered.recv().await;
        host.set_remote_description(peer.pc.local_description().await.unwrap()).await.unwrap();
        let answer=host.create_answer(None).await.unwrap();
        assert!(answer.sdp.contains("H265/90000"));assert!(!answer.sdp.contains("H264/90000"));
        let mut gathered=host.gathering_complete_promise().await;host.set_local_description(answer).await.unwrap();gathered.recv().await;
        peer.pc.set_remote_description(host.local_description().await.unwrap()).await.unwrap();
        while peer.pc.connection_state()!=webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected {tokio::time::sleep(Duration::from_millis(10)).await;}
        let mut au=vec![0,0,0,1,64,1,9,0,0,0,1,38,1];au.extend(vec![7;200]);
        let payloads=crate::hevc::payload(80,&au).unwrap();let mut seq=0;let mut seen=false;
        for timestamp in 0..100 {
            for (i,payload) in payloads.iter().enumerate(){
                video.write_rtp(&webrtc::rtp::packet::Packet{header:webrtc::rtp::header::Header{version:2,sequence_number:seq,timestamp:timestamp*3000,marker:i+1==payloads.len(),..Default::default()},payload:payload.clone()}).await.unwrap();seq=seq.wrapping_add(1);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(unit)=peer.desktop.units.pop(){assert_eq!(unit.codec,Codec::H265);assert_eq!(unit.data,au);seen=true;}
            if seen && peer.observed.chroma.load(Ordering::Acquire)==1 {break}
        }
        assert!(seen);assert_eq!(peer.observed.encoder_codecs.load(Ordering::Acquire),3);
        assert_eq!(peer.observed.chroma.load(Ordering::Acquire),1);
        peer.close().await;host.close().await.unwrap();router.lock().await.stop().await.unwrap();
    }).await.expect("in-memory H265 timed out");
}
