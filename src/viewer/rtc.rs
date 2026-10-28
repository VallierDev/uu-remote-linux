use super::wire::{self, Shape, CHANNELS};
use serde_json::{json, Value};
use std::sync::{Arc,atomic::{AtomicBool,AtomicU32,AtomicU64,Ordering}};
use tokio::sync::mpsc;
use webrtc::{api::{APIBuilder,media_engine::MediaEngine,setting_engine::SettingEngine}, peer_connection::{RTCPeerConnection,configuration::RTCConfiguration,sdp::session_description::RTCSessionDescription,policy::ice_transport_policy::RTCIceTransportPolicy}, ice_transport::{ice_server::RTCIceServer,ice_candidate::RTCIceCandidateInit}, data_channel::{RTCDataChannel,data_channel_init::RTCDataChannelInit}, rtp_transceiver::{rtp_codec::RTPCodecType,rtp_transceiver_direction::RTCRtpTransceiverDirection,RTCRtpTransceiverInit}};

pub fn configuration(result:&Value)->Result<RTCConfiguration,String> {
    let list=result.get("iceServers").and_then(Value::as_array).ok_or("Control acknowledgement missing iceServers")?;
    let mut servers=Vec::new();
    for server in list {
        let urls=match server.get("urls") {
            Some(Value::String(s))=>vec![s.clone()],
            Some(Value::Array(a))=>a.iter().map(|v|v.as_str().map(str::to_owned).ok_or("Invalid ICE URL")).collect::<Result<Vec<_>,_>>()?,
            _=>return Err("Invalid ICE URLs".into()),
        };
        if urls.is_empty() || urls.iter().any(|u| !["stun:","stuns:","turn:","turns:"].iter().any(|p|u.starts_with(p))) {return Err("Invalid ICE URL scheme".into())}
        let string=|key|match server.get(key) {Some(Value::String(s))=>Ok(s.clone()),None=>Ok(String::new()),_=>Err("Invalid ICE authentication")};
        // webrtc-rs rejects TURN unless the credential type is Password (as in the host's session_ice.rs).
        servers.push(RTCIceServer { urls,username:string("username")?,credential:string("credential")?,
            credential_type:webrtc::ice_transport::ice_credential_type::RTCIceCredentialType::Password });
    }
    let relay=match result.get("force_relay") {None=>false,Some(Value::Bool(b))=>*b,_=>return Err("Invalid relay policy".into())};
    if relay && !servers.iter().any(|s|s.urls.iter().any(|u|u.starts_with("turn:")||u.starts_with("turns:"))) {return Err("Relay-only session without TURN".into())}
    // An empty cloud list means host candidates only. Never invent public STUN.
    Ok(RTCConfiguration{ice_servers:servers,ice_transport_policy:if relay{RTCIceTransportPolicy::Relay}else{RTCIceTransportPolicy::All},..Default::default()})
}
#[derive(Clone,Copy)]
pub enum ReadyPolicy { EchoOnly, Explicit }
#[derive(Default)]
pub struct Observed {
    pub control_open:AtomicBool,
    pub echo_received:AtomicBool,
    pub clipboard_supported:AtomicBool,
    pub control_messages:AtomicU64,
    pub other_messages:AtomicU64,
    pub media_packets:AtomicU64,
    pub encoder_codecs:AtomicU32,
    pub chroma:AtomicU32,
    /// Received RTP payload bytes and video packets vs. sequence span, for the network-status panel.
    pub media_bytes:AtomicU64,
    pub video_received:AtomicU64,
    pub video_expected:AtomicU64,
}
pub struct Peer {
    clipboard:super::clipboard::Bridge,
    pub tools:super::tools::Inbox,
    pub pc:Arc<RTCPeerConnection>,
    pub channels:Vec<Arc<RTCDataChannel>>,
    pub observed:Arc<Observed>,
    pub desktop:super::desktop::Desktop,
    pub audio:super::audio::Audio,
    answered:bool,
    early:Vec<RTCIceCandidateInit>,
    app_id:String,
    ice_id:String,
    remote_client:String,
}
fn media_engine(shape:&Shape)->Result<MediaEngine,String>{
    use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecParameters,RTCRtpCodecCapability};
    let mut engine=MediaEngine::default();
    let mut codecs=vec![(RTPCodecType::Video,"video/H264",90000,0,"level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",102),
        (RTPCodecType::Audio,"audio/opus",48000,2,"minptime=10;useinbandfec=1",111)];
    // RFC 7798 single-layer non-interleaved mode: no sprop-max-don-diff, hence no DONL.
    if shape.decoders.iter().any(|d|d.codec==2){codecs.insert(0,(RTPCodecType::Video,"video/H265",90000,0,"",104));}
    for (kind,mime,rate,channels,fmtp,payload) in codecs {
        engine.register_codec(RTCRtpCodecParameters{capability:RTCRtpCodecCapability{mime_type:mime.into(),clock_rate:rate,channels,sdp_fmtp_line:fmtp.into(),..Default::default()},payload_type:payload,..Default::default()},kind).map_err(|_|"Codec initialization failed")?;
    }Ok(engine)
}
impl Peer {
    pub async fn new(config:RTCConfiguration, shape:&Shape, app_id:&str, ice_id:&str, remote_client:&str, outbound:mpsc::Sender<Value>, settings:SettingEngine, mut clipboard:super::clipboard::Bridge)->Result<Self,String> {
        shape.validate_session()?;
        let mut engine=media_engine(shape)?;
        // UU RTP metadata the official Mac viewer offers (src/rtp_metadata.rs on the host side).
        for uri in ["http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index","http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame"] {
            engine.register_header_extension(webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionCapability{uri:uri.into()},RTPCodecType::Video,None).map_err(|_|"RTP header extension registration failed")?;
        }
        let registry=webrtc::api::interceptor_registry::register_default_interceptors(webrtc::interceptor::registry::Registry::new(),&mut engine).map_err(|_|"RTP interceptor initialization failed")?;
        let api=APIBuilder::new().with_media_engine(engine).with_setting_engine(settings).with_interceptor_registry(registry).build();
        let pc=Arc::new(api.new_peer_connection(config).await.map_err(|e|format!("Peer creation failed: {}",crate::auth::transport_error(&e)))?);
        let tools:super::tools::Inbox=Arc::new(std::sync::Mutex::new(None));
        let inbox=tools.clone();
        pc.on_peer_connection_state_change(Box::new(move |state|{
            use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
            log::info!("Viewer peer connection state: {state}");
            if matches!(state,RTCPeerConnectionState::Failed|RTCPeerConnectionState::Closed){inbox.lock().unwrap().take();}
            Box::pin(async{})
        }));
        let observed=Arc::new(Observed::default());
        let (app,ice,client)=(app_id.to_owned(),ice_id.to_owned(),remote_client.to_owned());
        let weak_pc=Arc::downgrade(&pc);
        pc.on_ice_candidate(Box::new(move |candidate| {
            let weak_pc=weak_pc.clone();let tx=outbound.clone();let(app,ice,client)=(app.clone(),ice.clone(),client.clone());
            Box::pin(async move {if let Some(candidate)=candidate {if let Ok(mut value)=candidate.to_json() {
                // Candidate callbacks can omit MID. Bind to the first actual bundled m-line.
                if let Some(pc)=weak_pc.upgrade() {
                    if let Some(local)=pc.local_description().await {
                        if let Some(mid)=local.sdp.lines().find_map(|line|line.strip_prefix("a=mid:")) {
                            value.sdp_mid=Some(mid.trim().into());value.sdp_mline_index=Some(0);
                        }
                    }
                }
                let _=tx.send(json!({"client_id":client,"data":{"type":"candidate","candidate":value,"ice_id":ice,"app_control_id":app}})).await;
            }}})
        }));
        let counters=observed.clone();
        let desktop=super::desktop::Desktop::default();
        let audio=super::audio::Audio::default();let sound=audio.clone();
        let receiver=desktop.clone();let weak_pc=Arc::downgrade(&pc);
        pc.on_track(Box::new(move |track,_,_| {
            let counters=counters.clone();let desktop=receiver.clone();let audio=sound.clone();let weak_pc=weak_pc.clone();
            // The handler must return: webrtc-rs awaits it before surfacing the next track,
            // so an inline read loop on the audio track starved the video track.
            Box::pin(async move { tokio::spawn(async move {
                let video=track.kind()==RTPCodecType::Video;
                log::info!("Viewer track: kind={} mime={} payload_type={}",track.kind(),track.codec().capability.mime_type,track.payload_type());
                let opus=track.kind()==RTPCodecType::Audio && track.codec().capability.mime_type.eq_ignore_ascii_case("audio/opus");
                if video && super::media::Codec::from_mime(&track.codec().capability.mime_type).is_none() {return}
                if video {desktop.ssrc.store(track.ssrc(),Ordering::Release);}
                let mut receiver=super::media::Receiver::new(super::media::Codec::from_mime(&track.codec().capability.mime_type).unwrap_or_default());
                let mut tick=tokio::time::interval(std::time::Duration::from_millis(40));
                let mut last_pli=std::time::Instant::now()-std::time::Duration::from_secs(1);
                let mut span:Option<(u16,u64)>=None;
                loop {
                    let output=tokio::select! {
                        packet=track.read_rtp()=>match packet {
                            Ok((p,_))=>{
                                counters.media_packets.fetch_add(1,Ordering::Relaxed);
                                counters.media_bytes.fetch_add(p.payload.len() as u64+12,Ordering::Relaxed);
                                if video {
                                    // Extended sequence span: forward steps add, reordered/old packets do not.
                                    let seq=p.header.sequence_number;
                                    let (last,expected)=span.unwrap_or((seq.wrapping_sub(1),0));
                                    let step=seq.wrapping_sub(last);
                                    span=Some(if step!=0 && step<32768 {(seq,expected+u64::from(step))} else {(last,expected)});
                                    counters.video_expected.store(span.map_or(0,|s|s.1),Ordering::Relaxed);
                                    counters.video_received.fetch_add(1,Ordering::Relaxed);
                                }
                                if !video {if opus {audio.push(p.header.sequence_number,&p.payload);}continue}
                                // TrackRemote updates codec from the negotiated payload type on each read.
                                let Some(codec)=super::media::Codec::from_mime(&track.codec().capability.mime_type) else{continue};
                                if receiver.codec()!=codec {
                                    receiver=super::media::Receiver::new(codec);
                                    desktop.reset.store(true,Ordering::Release);desktop.pli.store(true,Ordering::Release);
                                }
                                receiver.push(super::media::Packet{seq:p.header.sequence_number,timestamp:p.header.timestamp,marker:p.header.marker,payload:p.payload.to_vec()})
                            },
                            Err(_)=>break,
                        },
                        _=tick.tick()=>{if !video {continue} receiver.expire()},
                    };
                    if output.pli {desktop.reset.store(true,Ordering::Release);desktop.pli.store(true,Ordering::Release);}
                    for au in output.units {if desktop.units.push(super::media::AccessUnit{codec:receiver.codec(),data:au}) {desktop.reset.store(true,Ordering::Release);desktop.pli.store(true,Ordering::Release);}}
                    if desktop.pli.load(Ordering::Acquire) && last_pli.elapsed()>=std::time::Duration::from_millis(250) {
                        desktop.pli.swap(false,Ordering::AcqRel);
                        if let Some(pc)=weak_pc.upgrade() {
                            use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
                            let _=pc.write_rtcp(&[Box::new(PictureLossIndication{sender_ssrc:0,media_ssrc:track.ssrc()})]).await;
                        } else {break}
                        last_pli=std::time::Instant::now();
                    }
                }
            }); })
        }));
        let result=async {
            if shape.capture_type==1 {
                // The official Mac viewer offers five recvonly video sections (observed live).
                let videos=std::env::var("UU_VIEWER_VIDEO_SECTIONS").ok().and_then(|v|v.parse::<usize>().ok()).filter(|n|(1..=8).contains(n)).unwrap_or(1);
                for kind in std::iter::repeat(RTPCodecType::Video).take(videos).chain([RTPCodecType::Audio]) {
                    pc.add_transceiver_from_kind(kind,Some(RTCRtpTransceiverInit{direction:RTCRtpTransceiverDirection::Recvonly,send_encodings:vec![]})).await.map_err(|_|"Media transceiver failed")?;
                }
            }
            let mut channels=Vec::new();
            let clipboard_enabled=clipboard.enabled;
            // Diagnostic: hosts may open their own channels; log the label and message count only.
            pc.on_data_channel(Box::new(|channel|Box::pin(async move {
                let label=channel.label().to_owned();log::info!("Viewer remote data channel opened: label={label}");
                let count=Arc::new(std::sync::atomic::AtomicU64::new(0));
                channel.on_message(Box::new(move |m|{let n=count.fetch_add(1,Ordering::Relaxed);if n<8 {log::info!("Viewer remote channel message: label={label} bytes={} fields={:?}",m.data.len(),crate::protocol::decode_wire(&m.data).map(|f|f.iter().map(|x|x.number).collect::<Vec<_>>()).unwrap_or_default());}Box::pin(async{})}));
            })));
            for label in CHANNELS {
                channels.push(pc.create_data_channel(label,Some(RTCDataChannelInit{ordered:Some(true),..Default::default()})).await.map_err(|_|"Data channel creation failed")?);
            }
            // Native host TEXT offers are RPC 21/case 9; CONTROL replies hit a separate empty State.
            // Keep clipboard offers/replies on TEXT, where the host desktop bridge lives.
            clipboard.start(&channels[super::clipboard::CHANNEL],&channels[0],observed.clone());
            for (index,channel) in channels.iter().enumerate() {
                if index==0 {
                    let weak=Arc::downgrade(&channel);let state=observed.clone();
                    let chroma444=shape.capture_type==1 && std::env::var("UU_VIEWER_CHROMA").is_ok_and(|v|v=="444");
                    let desktop_track=shape.capture_type==1;
                    channel.on_open(Box::new(move ||Box::pin(async move {
                        state.control_open.store(true,Ordering::Release);
                        if let Some(channel)=weak.upgrade(){
                            let _=channel.send(&bytes::Bytes::from(wire::echo_request_with_clipboard(1,clipboard_enabled))).await;
                            if desktop_track {let _=channel.send(&bytes::Bytes::from(wire::request_video_track())).await;}
                            if chroma444 {let _=channel.send(&bytes::Bytes::from(wire::request_chroma444())).await;}
                        }
                    })));
                }
                let closed=tools.clone();let state=observed.clone();
                channel.on_close(Box::new(move ||{let closed=closed.clone();let state=state.clone();Box::pin(async move {
                    if index==0 {state.control_open.store(false,Ordering::Release);}
                    if [0,3,4].contains(&index){closed.lock().unwrap().take();}
                })}));
                let state=observed.clone();let inbox=tools.clone();let clipboard_tx=clipboard.sender();
                let control_for_echo=if index==0 {Arc::downgrade(&channel)} else {std::sync::Weak::new()};
                channel.on_message(Box::new(move |message| {let state=state.clone();let inbox=inbox.clone();let control_for_echo=control_for_echo.clone();let clipboard_tx=clipboard_tx.clone();Box::pin(async move {
                    if index==0 {
                        let (codecs,chroma)=wire::media_control(&message.data);
                        if let Some(codecs)=codecs {state.encoder_codecs.store(codecs,Ordering::Release);}
                        if let Some(chroma)=chroma {
                            if state.chroma.swap(chroma,Ordering::AcqRel)!=chroma {log::info!("Viewer capture sync: chroma={chroma}");}
                        }
                        let count=state.control_messages.fetch_add(1,Ordering::Relaxed);
                        if wire::echo_matches(&message.data,1) {let supported=wire::echo_clipboard_supported(&message.data);
                            if state.clipboard_supported.swap(supported,Ordering::AcqRel)!=supported || !state.echo_received.load(Ordering::Acquire) {log::info!("Viewer echo: clipboard_supported={supported}");}state.echo_received.store(true,Ordering::Release);}
                        if let Some(request)=wire::echo_request_seq(&message.data) {
                            if let Some(control)=control_for_echo.upgrade() {let _=control.send(&bytes::Bytes::from(wire::echo_response_with_clipboard(1_000_000+request,request,clipboard_enabled))).await;}
                        }
                        // Observe only protobuf field numbers; never print channel payloads.
                        if count<32 {if let Ok(fields)=crate::protocol::decode_wire(&message.data) {let action=fields.iter().find_map(|f|match(f.number,&f.value){(3,crate::protocol::WireValue::Bytes(b))=>Some(super::tools::uint(b,1)),_=>None});
                            log::info!("Viewer CONTROL fields={:?} simple_action={:?}",fields.iter().take(32).map(|f|f.number).collect::<Vec<_>>(),action);}}
                    }
                    if [0,1].contains(&index) && message.data.len()<=super::clipboard::MAX_TEXT*2+1024 {
                        if let Some(tx)=clipboard_tx {
                            if crate::protocol::decode_wire(&message.data).is_ok_and(|f|f.iter().any(|f|[17,21,22].contains(&f.number))){let _=tx.try_send(message.data.to_vec());}
                        }
                    }
                    // Tools need FILE/BINARY traffic and CONTROL RPC replies (Message 22) only. Official
                    // hosts stream many other CONTROL messages; a full queue drops that message instead of
                    // detaching the tool, which previously lost every later reply.
                    if index!=0 {let n=state.other_messages.fetch_add(1,Ordering::Relaxed);if n<8 {
                        // Shape only: protobuf field numbers or JSON keys, never values.
                        let shape=match serde_json::from_slice::<serde_json::Value>(&message.data){Ok(serde_json::Value::Object(o))=>format!("json{:?}",o.keys().collect::<Vec<_>>()),
                            _=>format!("pb{:?}",crate::protocol::decode_wire(&message.data).map(|f|f.iter().map(|x|x.number).collect::<Vec<_>>()).unwrap_or_default())};
                        let inner=crate::protocol::decode_wire(&message.data).ok().and_then(|f|f.into_iter().find_map(|x|match x.value{crate::protocol::WireValue::Bytes(b) if x.number>2=>Some((x.number,crate::protocol::decode_wire(b).map(|g|g.iter().map(|y|y.number).collect::<Vec<_>>()).unwrap_or_default())),_=>None}));
                        log::info!("Viewer channel message: index={index} bytes={} shape={shape} inner={inner:?}",message.data.len());}}
                    // The official Windows host answers file RPCs (Message 21/22) on TEXT_DATA_CHANNEL (observed
                    // on SHARE: a 320-byte Message 22 on index 1); our host uses FILE. Treat both as the file channel.
                    let has=|n:&[u32]|crate::protocol::decode_wire(&message.data).is_ok_and(|f|f.iter().any(|x|n.contains(&x.number)));
                    let route=if index==1 && has(&[21,22,27]) {3} else {index};
                    // CONTROL carries host-initiated file RPCs (Message 21 with ftp case 8) during downloads too.
                    let route=if index==0 && crate::protocol::decode_wire(&message.data).ok().and_then(|f|f.into_iter().find_map(|x|match x.value{crate::protocol::WireValue::Bytes(b) if x.number==21=>Some(super::tools::blob(b,8).is_some()),_=>None}))==Some(true) {3} else {route};
                    let wanted=index==3 || index==4 || route==3 || (index==0 && has(&[22]));
                    let sender=inbox.lock().unwrap().clone();
                    if let (true,Some(sender))=(wanted,sender) {
                        if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_))=sender.try_send((route,message.data.to_vec())) {
                            let mut slot=inbox.lock().unwrap();if slot.as_ref().is_some_and(|s|s.same_channel(&sender)){slot.take();}
                        }
                    }
                })}));
            }
            Ok::<_,String>((channels,tools,clipboard))
        }.await;
        match result {
            Ok((channels,tools,clipboard))=>Ok(Self{clipboard,tools,pc,channels,observed,desktop,audio,answered:false,early:Vec::new(),app_id:app_id.into(),ice_id:ice_id.into(),remote_client:remote_client.into()}),
            Err(e)=>{let _=pc.close().await;Err(e)},
        }
    }
    pub async fn offer(&self)->Result<Value,String> {
        let offer=self.pc.create_offer(None).await.map_err(|_|"Create offer failed")?;
        let mut sdp=offer.sdp.clone();self.pc.set_local_description(offer).await.map_err(|_|"Set local offer failed")?;
        // The official Mac viewer's offer carries a bare session-level a=x-uuremote-quic. Only the sent
        // copy is changed: webrtc-rs rejects a modified local description.
        if !sdp.contains("a=x-uuremote-quic") { if let Some(i)=sdp.find("a=group:BUNDLE") { sdp.insert_str(i,"a=x-uuremote-quic\r\n"); } }
        // webrtc-rs omits a=max-message-size, so the host assumes 64 KB and drops larger replies (a 5600-entry
        // directory listing compresses to ~73 KB). Advertise libwebrtc's 256 KB like the host does; the
        // vendored data channel already accepts messages of that size.
        sdp=with_max_message_size(&sdp);
        // Local diagnostic: UU_VIEWER_DUMP_OFFER=path writes the sent offer SDP (stays on the operator's machine).
        if let Some(path)=std::env::var_os("UU_VIEWER_DUMP_OFFER") {let _=std::fs::write(path,&sdp);}
        // ice_network_type=3 as sent by the official Mac viewer (observed on our host's SOAC shape log).
        Ok(json!({"client_id":self.remote_client,"data":{"type":"offer","sdp":sdp,"ice_id":self.ice_id,"app_control_id":self.app_id,"ice_network_type":3}}))
    }
    pub async fn receive(&mut self,event:&Value)->Result<(),String> {
        let body=event.get("data").unwrap_or(event);
        if !self.matches_route(event) {return Ok(())}
        match body.get("type").and_then(Value::as_str) {
            Some("answer")=> {
                if self.answered {return Ok(())}
                let sdp=answer_sdp(body)?;
                let answer=RTCSessionDescription::answer(sdp).map_err(|_|"Invalid answer")?;
                self.pc.set_remote_description(answer).await.map_err(|_|"Set remote answer failed")?;
                self.answered=true;
                for c in self.early.drain(..) {self.pc.add_ice_candidate(c).await.map_err(|_|"Queued ICE candidate failed")?;}
            }
            Some("candidate")=> {
                let value=body.get("candidate").ok_or("Missing candidate")?;
                let candidate=if let Some(s)=value.as_str() {
                    RTCIceCandidateInit {candidate:s.into(),sdp_mid:body.get("sdp_mid").and_then(Value::as_str).map(str::to_owned),sdp_mline_index:body.get("sdp_mline_index").and_then(Value::as_u64).and_then(|v|u16::try_from(v).ok()),..Default::default()}
                } else {serde_json::from_value(value.clone()).map_err(|_|"Invalid candidate")?};
                if self.answered {self.pc.add_ice_candidate(candidate).await.map_err(|_|"ICE candidate failed")?;}
                else {if self.early.len()>=256{return Err("Too many early ICE candidates".into())}self.early.push(candidate);}
            }
            _=>{},
        }
        Ok(())
    }
    pub fn matches_route(&self,event:&Value)->bool {
        let body=event.get("data").unwrap_or(event);
        !body.get("ice_id").is_some_and(|v|v.as_str()!=Some(self.ice_id.as_str()))
            && !body.get("app_control_id").is_some_and(|v|v.as_str()!=Some(self.app_id.as_str()))
            && !event.get("client_id").is_some_and(|v|v.as_str()!=Some(self.remote_client.as_str()))
    }
    pub fn ready(&self,policy:ReadyPolicy,explicit:bool)->bool {
        self.observed.echo_received.load(Ordering::Acquire) && match policy {ReadyPolicy::EchoOnly=>true,ReadyPolicy::Explicit=>explicit}
    }
    pub async fn send_input(&self,value:&Value)->Result<(),String> {
        let channel=&self.channels[0];
        if channel.ready_state()!=webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {return Err("CONTROL is not open".into())}
        channel.send(&bytes::Bytes::from(value.to_string())).await.map_err(|_|"CONTROL input send failed")?;Ok(())
    }
    /// Binary CONTROL message (protobuf envelope), e.g. capture settings and SimpleActions.
    pub async fn send_control(&self,data:Vec<u8>)->Result<(),String> {
        let channel=&self.channels[0];
        if channel.ready_state()!=webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {return Err("CONTROL is not open".into())}
        channel.send(&bytes::Bytes::from(data)).await.map_err(|_|"CONTROL send failed")?;Ok(())
    }
    /// Selected ICE pair: (round-trip ms, "UDP · direct" / "TCP · relay" ...). Addresses are not kept.
    pub async fn route(&self)->(Option<u32>,String) {
        use webrtc::stats::StatsReportType as T;
        let report=self.pc.get_stats().await;
        let pair=report.reports.values().filter_map(|r|match r{T::CandidatePair(p) if p.nominated=>Some(p),_=>None}).max_by_key(|p|p.bytes_received);
        let Some(pair)=pair else {return (None,String::new())};
        let local=report.reports.get(&pair.local_candidate_id).and_then(|r|match r{T::LocalCandidate(c)=>Some(c),_=>None});
        let remote=report.reports.get(&pair.remote_candidate_id).and_then(|r|match r{T::RemoteCandidate(c)=>Some(c),_=>None});
        let relay=[local,remote].iter().flatten().any(|c|c.candidate_type==webrtc::ice::candidate::CandidateType::Relay);
        let tcp=local.is_some_and(|c|c.network_type.to_string().to_ascii_lowercase().contains("tcp") || c.relay_protocol.eq_ignore_ascii_case("tcp"));
        let rtt=(pair.current_round_trip_time>0.).then(||(pair.current_round_trip_time*1000.).round() as u32);
        (rtt,format!("{} · {}",if tcp{"TCP"}else{"UDP"},if relay{"relay"}else{"direct"}))
    }
    pub async fn close(&self) {self.clipboard.stop();self.tools.lock().unwrap().take();let _=self.pc.close().await;}
}
fn answer_sdp(body:&Value)->Result<String,String> {
    if let Some(s)=body.get("sdp").and_then(Value::as_str) {if s.len()<=1024*1024{return Ok(s.into())}}
    if let Some(s)=body.get("gzip_sdp").and_then(Value::as_str) {
        use base64::Engine;use std::io::Read;
        if s.len()>2*1024*1024 {return Err("Compressed answer too large".into())}
        let data=base64::engine::general_purpose::STANDARD.decode(s).map_err(|_|"Invalid compressed answer")?;
        let mut output=String::new();flate2::read::GzDecoder::new(data.as_slice()).take(1024*1024+1).read_to_string(&mut output).map_err(|_|"Invalid compressed answer")?;
        if output.len()<=1024*1024 {return Ok(output)}
    }
    Err("Answer missing or too large".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compressed_answers_are_supported_and_bounded() {
        use base64::Engine;use std::io::Write;
        let mut encoder=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::default());
        encoder.write_all(b"synthetic-sdp").unwrap();
        let encoded=base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap());
        assert_eq!(answer_sdp(&json!({"gzip_sdp":encoded})).unwrap(),"synthetic-sdp");
        assert!(answer_sdp(&json!({"gzip_sdp":"invalid!"})).is_err());
        assert!(answer_sdp(&json!({"sdp":"x".repeat(1024*1024+1)})).is_err());
    }
}

/// Adds `a=max-message-size:262144` after `a=sctp-port` when missing (sent copy only).
pub fn with_max_message_size(sdp:&str)->String{
    if sdp.contains("a=max-message-size:"){return sdp.to_owned()}
    let eol=if sdp.contains("\r\n"){"\r\n"}else{"\n"};
    let mut out=String::with_capacity(sdp.len()+32);
    for line in sdp.split_inclusive('\n'){out.push_str(line);if line.trim_end().starts_with("a=sctp-port:"){out.push_str("a=max-message-size:262144");out.push_str(eol);}}
    out
}
#[cfg(test)] #[test] fn offer_advertises_max_message_size(){
    assert_eq!(with_max_message_size("m=application 9\r\na=sctp-port:5000\r\na=mid:0\r\n"),"m=application 9\r\na=sctp-port:5000\r\na=max-message-size:262144\r\na=mid:0\r\n");
    assert_eq!(with_max_message_size("a=max-message-size:1\r\n"),"a=max-message-size:1\r\n");
}

#[cfg(test)] #[tokio::test] async fn offers_both_codecs_or_h264_only(){
    for force in [false,true] {
        let shape:Shape=serde_json::from_str(include_str!("../../tests/fixtures/viewer/session-shape.json")).unwrap();
        let shape=shape.video_preferences(force);
        let api=APIBuilder::new().with_media_engine(media_engine(&shape).unwrap()).build();
        let pc=api.new_peer_connection(RTCConfiguration::default()).await.unwrap();
        pc.add_transceiver_from_kind(RTPCodecType::Video,Some(RTCRtpTransceiverInit{direction:RTCRtpTransceiverDirection::Recvonly,send_encodings:vec![]})).await.unwrap();
        let offer=pc.create_offer(None).await.unwrap();
        assert!(offer.sdp.contains("a=rtpmap:102 H264/90000"));
        assert_eq!(offer.sdp.contains("a=rtpmap:104 H265/90000"),!force);
        assert!(!offer.sdp.contains("sprop-max-don-diff"));pc.close().await.unwrap();
    }
}
