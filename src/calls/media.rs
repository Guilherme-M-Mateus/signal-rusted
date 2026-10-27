//! RingRTC's CallManager for 1:1 calls, like Signal-Desktop's `CallingClass`
//! (ts/services/calling.preload.ts): RingRTC does the call state machine and WebRTC media, we
//! carry its signaling over Signal `CallMessage`s and turn its states into the UI's `Call`.
//! Audio devices are RingRTC's own (cubeb) module; video is never sent (no camera capture).

use std::time::Duration;

use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::websocket::calling::TurnServer;
use presage::proto::CallMessage;
use presage::proto::call_message::{self, offer, Busy, Hangup, IceUpdate};
use ringrtc::common::{CallConfig, CallEndReason, CallId, CallMediaType, DeviceId, Result};
use ringrtc::core::call_manager::CallManager;
use ringrtc::core::{group_call, signaling};
use ringrtc::lite::{http, sfu};
use ringrtc::native::{
    CallState, CallStateHandler, GroupUpdate, GroupUpdateHandler, NativeCallContext, NativePlatform, SignalingSender,
};
use ringrtc::webrtc::media::{AudioTrack, VideoFrame, VideoSink, VideoTrack};
use ringrtc::webrtc::peer_connection::AudioLevel;
use ringrtc::webrtc::peer_connection_factory::{AudioConfig, IceServer, PeerConnectionFactory};
use ringrtc::webrtc::peer_connection_observer::NetworkRoute;
use tokio::sync::mpsc::UnboundedSender;

use super::{Call, Phase};

/// What RingRTC asks of the backend, from its own threads.
pub enum Out {
    /// Signaling to send to `peer` (an ACI); `device` is unset for a broadcast to all their devices.
    Signal { peer: String, id: CallId, device: Option<DeviceId>, msg: signaling::Message },
    State { peer: String, id: CallId, state: CallState },
}

impl std::fmt::Debug for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Out::Signal { id, msg, .. } => write!(f, "Signal({id}, {:?})", msg.typ()),
            Out::State { id, state, .. } => write!(f, "State({id}, {state:?})"),
        }
    }
}

/// A Signal `CallMessage` as RingRTC's signaling, with its call id; `None` for group-call
/// `opaque` messages and anything malformed.
pub fn from_proto(m: &CallMessage) -> Option<(CallId, signaling::Message)> {
    let msg = if let Some(o) = &m.offer {
        let media = if o.r#type == Some(offer::Type::OfferVideoCall as i32) { CallMediaType::Video } else { CallMediaType::Audio };
        (o.id?, signaling::Message::Offer(signaling::Offer::new(media, o.opaque.clone()?).ok()?))
    } else if let Some(a) = &m.answer {
        (a.id?, signaling::Message::Answer(signaling::Answer::new(a.opaque.clone()?).ok()?))
    } else if let Some(first) = m.ice_update.first() {
        // RingRTC sends one id for all of a message's candidates.
        let candidates = m.ice_update.iter().filter_map(|i| Some(signaling::IceCandidate::new(i.opaque.clone()?))).collect();
        (first.id?, signaling::Message::Ice(signaling::Ice { candidates }))
    } else if let Some(h) = &m.hangup {
        let typ = signaling::HangupType::from_i32(h.r#type.unwrap_or(0))?;
        (h.id?, signaling::Message::Hangup(signaling::Hangup::from_type_and_device_id(typ, h.device_id.unwrap_or(0))))
    } else if let Some(b) = &m.busy {
        (b.id?, signaling::Message::Busy)
    } else {
        return None;
    };
    Some((CallId::new(msg.0), msg.1))
}

/// RingRTC's signaling as the `CallMessage` to send (Desktop's `callingMessageToProto`).
pub fn to_proto(id: CallId, device: Option<DeviceId>, msg: signaling::Message) -> CallMessage {
    let id = Some(id.as_u64());
    let mut m = CallMessage { destination_device_id: device, ..Default::default() };
    match msg {
        signaling::Message::Offer(o) => {
            let ty = match o.call_media_type {
                CallMediaType::Audio => offer::Type::OfferAudioCall,
                CallMediaType::Video => offer::Type::OfferVideoCall,
            };
            m.offer = Some(call_message::Offer { id, r#type: Some(ty as i32), opaque: Some(o.opaque) });
        }
        signaling::Message::Answer(a) => m.answer = Some(call_message::Answer { id, opaque: Some(a.opaque) }),
        signaling::Message::Ice(ice) => {
            m.ice_update = ice.candidates.into_iter().map(|c| IceUpdate { id, opaque: Some(c.opaque) }).collect();
        }
        signaling::Message::Hangup(h) => {
            let (typ, device_id) = h.to_type_and_device_id();
            m.hangup = Some(Hangup { id, r#type: Some(typ as i32), device_id: Some(device_id.unwrap_or(0)) });
        }
        signaling::Message::Busy => m.busy = Some(Busy { id }),
    }
    m
}

/// TURN servers as Desktop hands them to RingRTC: the IP URLs (with the hostname for TLS)
/// first, then the DNS ones.
pub fn ice_servers(relays: &[TurnServer]) -> Vec<IceServer> {
    relays
        .iter()
        .flat_map(|r| {
            [
                IceServer::new(r.username.clone(), r.password.clone(), r.hostname.clone().unwrap_or_default(), r.urls_with_ips.clone()),
                IceServer::new(r.username.clone(), r.password.clone(), String::new(), r.urls.clone()),
            ]
        })
        .collect()
}

/// An identity key as RingRTC wants it: without libsignal's one-byte type prefix.
pub fn ringrtc_key(serialized: &[u8]) -> Vec<u8> {
    serialized.get(1..).unwrap_or_default().to_vec()
}

fn end_text(reason: CallEndReason) -> &'static str {
    use CallEndReason::*;
    match reason {
        LocalHangup | RemoteHangup | AppDroppedCall => "Call ended",
        RemoteHangupNeedPermission => "They need to accept your message request first",
        RemoteHangupAccepted => "Answered on another device",
        RemoteHangupDeclined => "Declined on another device",
        RemoteHangupBusy | RemoteBusy => "Busy",
        Timeout => "No answer",
        SignalingFailure | ConnectionFailure | InternalFailure => "Call failed",
        _ => "Call ended",
    }
}

/// The call RingRTC is running for us.
pub struct Active {
    pub id: CallId,
    pub call: Call,
}

/// Applies a state RingRTC reported to the current call and returns what the UI should show,
/// or `None` if it doesn't concern the current call. Ends clear `active`.
pub fn apply(active: &mut Option<Active>, peer: Uuid, id: CallId, state: &CallState, now_ms: u64) -> Option<Call> {
    if let CallState::Incoming(media) | CallState::Outgoing(media) = state {
        if active.as_ref().is_some_and(|a| a.id != id) {
            return None; // RingRTC answers a second call as busy itself
        }
        let call = Call {
            peer,
            incoming: matches!(state, CallState::Incoming(_)),
            video: *media == CallMediaType::Video,
            phase: Phase::Calling,
            muted: false,
            connected_at: None,
        };
        *active = Some(Active { id, call: call.clone() });
        return Some(call);
    }
    let a = active.as_mut().filter(|a| a.id == id)?;
    match state {
        CallState::Ringing => a.call.phase = Phase::Ringing,
        CallState::Connected => {
            a.call.phase = Phase::Connected;
            a.call.connected_at.get_or_insert(now_ms);
        }
        CallState::Connecting => a.call.phase = Phase::Reconnecting,
        CallState::Ended(reason, _) => a.call.phase = Phase::Ended(end_text(*reason).into()),
        CallState::Rejected(_) => a.call.phase = Phase::Ended("Call failed".into()),
        CallState::Concluded | CallState::Incoming(_) | CallState::Outgoing(_) => return None,
    }
    let call = a.call.clone();
    if matches!(call.phase, Phase::Ended(_)) {
        *active = None;
    }
    Some(call)
}

/// Forwards RingRTC's callbacks to the backend thread.
#[derive(Clone)]
struct Handler(UnboundedSender<Out>);

impl SignalingSender for Handler {
    fn send_signaling(&self, peer: &str, id: CallId, device: Option<DeviceId>, msg: signaling::Message) -> Result<()> {
        Ok(self.0.send(Out::Signal { peer: peer.into(), id, device, msg })?)
    }
    // Group calls aren't supported: these never happen for 1:1 calls.
    fn send_call_message(&self, _: sfu::UserId, _: Vec<u8>, _: group_call::SignalingMessageUrgency) -> Result<()> {
        Ok(())
    }
    fn send_call_message_to_group(
        &self,
        _: group_call::GroupId,
        _: Vec<u8>,
        _: group_call::SignalingMessageUrgency,
        _: std::collections::HashSet<sfu::UserId>,
    ) -> Result<()> {
        Ok(())
    }
    fn send_call_message_to_adhoc_group(
        &self,
        _: Vec<u8>,
        _: group_call::SignalingMessageUrgency,
        _: u64,
        _: std::collections::HashMap<sfu::UserId, Vec<u8>>,
    ) -> Result<()> {
        Ok(())
    }
}

impl CallStateHandler for Handler {
    fn handle_call_state(&self, peer: &str, id: CallId, state: CallState) -> Result<()> {
        Ok(self.0.send(Out::State { peer: peer.into(), id, state })?)
    }
    fn handle_remote_audio_state(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    fn handle_remote_video_state(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    fn handle_remote_sharing_screen(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    fn handle_network_route(&self, _: &str, _: NetworkRoute) -> Result<()> {
        Ok(())
    }
    fn handle_audio_levels(&self, _: &str, _: AudioLevel, _: AudioLevel) -> Result<()> {
        Ok(())
    }
    fn handle_low_bandwidth_for_video(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
}

impl GroupUpdateHandler for Handler {
    fn handle_group_update(&self, _: GroupUpdate) -> Result<()> {
        Ok(())
    }
}

impl http::Delegate for Handler {
    fn send_request(&self, _: u32, _: http::Request) {} // group calls only
}

/// Drops remote video: calls are audio-only for now.
#[derive(Clone)]
struct NoVideo;

impl VideoSink for NoVideo {
    fn on_video_frame(&self, _: sfu::DemuxId, _: VideoFrame) {}
    fn box_clone(&self) -> Box<dyn VideoSink> {
        Box::new(NoVideo)
    }
}

/// RingRTC with the default microphone and speaker. Created on first use: it opens the audio
/// devices (waiting up to 5 s for them to show up).
pub struct Engine {
    cm: CallManager<NativePlatform>,
    audio: AudioTrack,
    video: VideoTrack,
}

impl Engine {
    pub fn new(tx: UnboundedSender<Out>) -> anyhow::Result<Self> {
        let mut pcf = PeerConnectionFactory::new(&AudioConfig::default(), false, "", None)?;
        ringrtc::bin::utils::audio::set_default_audio_devices(&mut pcf)?;
        let audio = pcf.create_outgoing_audio_track()?;
        audio.set_enabled(false);
        let video = pcf.create_outgoing_video_track(&pcf.create_outgoing_video_source()?)?;
        video.set_enabled(false);
        let h = Handler(tx);
        // `false`: we report each signaling send with `sent`, as Desktop does.
        let platform = NativePlatform::new(pcf, Box::new(h.clone()), false, Box::new(h.clone()), Box::new(h.clone()));
        let cm = CallManager::new(platform, http::DelegatingClient::new(h))?;
        Ok(Engine { cm, audio, video })
    }

    pub fn start(&mut self, peer: Uuid, local_device: DeviceId) {
        self.cm.create_outgoing_call(peer.to_string(), CallId::random(), CallMediaType::Audio, local_device);
    }

    /// Lets a call RingRTC announced (`CallState::Incoming`/`Outgoing`) go ahead over `relays`.
    pub fn proceed(&mut self, id: CallId, relays: &[TurnServer]) {
        self.audio.set_enabled(true);
        let ctx = NativeCallContext::new(false, ice_servers(relays), self.audio.clone(), self.video.clone(), Box::new(NoVideo));
        self.cm.proceed(id, ctx, CallConfig::default(), None);
    }

    pub fn drop_call(&mut self, id: CallId) {
        self.cm.drop_call(id);
    }

    pub fn accept(&mut self, id: CallId) {
        self.cm.accept_call(id);
    }

    pub fn hangup(&mut self) {
        self.cm.hangup();
    }

    pub fn mute(&mut self, muted: bool) {
        self.audio.set_enabled(!muted);
        // Tells the other side too (Desktop's setOutgoingAudioEnabled); fails before connecting.
        if let Ok(mut c) = self.cm.active_connection() {
            let _ = c.update_sender_status(signaling::SenderStatus { audio_enabled: Some(!muted), ..Default::default() });
        }
    }

    /// Whether the last `Out::Signal` went out; RingRTC sends the next one after this.
    pub fn sent(&mut self, id: CallId, ok: bool) {
        if ok { self.cm.message_sent(id) } else { self.cm.message_send_failure(id) }
    }

    /// Hands RingRTC a received message. Identity keys are `ringrtc_key`s; `age` is how long the
    /// server held it (old offers are missed calls).
    #[allow(clippy::too_many_arguments)]
    pub fn received(
        &mut self,
        peer: Uuid,
        sender_device: DeviceId,
        local_device: DeviceId,
        id: CallId,
        msg: signaling::Message,
        age: Duration,
        sender_key: Vec<u8>,
        receiver_key: Vec<u8>,
    ) {
        let peer = peer.to_string();
        let cm = &mut self.cm;
        match msg {
            signaling::Message::Offer(offer) => cm.received_offer(
                peer,
                id,
                signaling::ReceivedOffer {
                    offer,
                    age,
                    sender_device_id: sender_device,
                    receiver_device_id: local_device,
                    sender_identity_key: sender_key,
                    receiver_identity_key: receiver_key,
                },
            ),
            signaling::Message::Answer(answer) => cm.received_answer(
                peer,
                id,
                signaling::ReceivedAnswer {
                    answer,
                    sender_device_id: sender_device,
                    sender_identity_key: sender_key,
                    receiver_identity_key: receiver_key,
                },
            ),
            signaling::Message::Ice(ice) => cm.received_ice(peer, id, signaling::ReceivedIce { ice, sender_device_id: sender_device }),
            signaling::Message::Hangup(hangup) => {
                cm.received_hangup(peer, id, signaling::ReceivedHangup { hangup, sender_device_id: sender_device })
            }
            signaling::Message::Busy => cm.received_busy(peer, id, signaling::ReceivedBusy { sender_device_id: sender_device }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringrtc::core::call_summary::CallSummary;

    fn opaque_offer() -> Vec<u8> {
        let v4 = ringrtc::protobuf::signaling::ConnectionParametersV4 { public_key: Some(vec![7; 32]), ..Default::default() };
        signaling::Offer::from_v4(CallMediaType::Audio, v4).unwrap().opaque
    }

    /// Every 1:1 message kind survives proto -> RingRTC -> proto unchanged.
    #[test]
    fn signaling_round_trips() {
        let id = Some(0xCA11);
        let messages = [
            CallMessage {
                offer: Some(call_message::Offer { id, r#type: Some(offer::Type::OfferVideoCall as i32), opaque: Some(opaque_offer()) }),
                destination_device_id: Some(2),
                ..Default::default()
            },
            CallMessage { answer: Some(call_message::Answer { id, opaque: Some(opaque_offer()) }), ..Default::default() },
            CallMessage {
                ice_update: vec![IceUpdate { id, opaque: Some(vec![1]) }, IceUpdate { id, opaque: Some(vec![2, 3]) }],
                destination_device_id: Some(3),
                ..Default::default()
            },
            CallMessage {
                hangup: Some(Hangup { id, r#type: Some(call_message::hangup::Type::HangupAccepted as i32), device_id: Some(4) }),
                ..Default::default()
            },
            CallMessage { hangup: Some(Hangup { id, r#type: Some(0), device_id: Some(0) }), ..Default::default() },
            CallMessage { busy: Some(Busy { id }), ..Default::default() },
        ];
        for m in messages {
            let (call_id, msg) = from_proto(&m).unwrap_or_else(|| panic!("{m:?}"));
            assert_eq!(call_id.as_u64(), 0xCA11);
            assert_eq!(to_proto(call_id, m.destination_device_id, msg), m);
        }
    }

    #[test]
    fn signaling_details() {
        let offer = |ty: Option<i32>| CallMessage {
            offer: Some(call_message::Offer { id: Some(1), r#type: ty, opaque: Some(opaque_offer()) }),
            ..Default::default()
        };
        let media = |m: &CallMessage| match from_proto(m) {
            Some((_, signaling::Message::Offer(o))) => o.call_media_type,
            _ => panic!("not an offer"),
        };
        assert_eq!(media(&offer(None)), CallMediaType::Audio, "a missing type is an audio call");
        assert_eq!(media(&offer(Some(1))), CallMediaType::Video);

        let hangup = CallMessage { hangup: Some(Hangup { id: Some(1), r#type: Some(1), device_id: Some(5) }), ..Default::default() };
        let Some((_, signaling::Message::Hangup(h))) = from_proto(&hangup) else { panic!() };
        assert_eq!(h, signaling::Hangup::AcceptedOnAnotherDevice(5));

        // Unusable: no id, no opaque, unknown hangup type, group-call opaque only.
        let no_id = CallMessage { busy: Some(Busy { id: None }), ..Default::default() };
        let no_opaque = CallMessage { answer: Some(call_message::Answer { id: Some(1), opaque: None }), ..Default::default() };
        let bad_type = CallMessage { hangup: Some(Hangup { id: Some(1), r#type: Some(9), device_id: None }), ..Default::default() };
        let group = CallMessage { opaque: Some(call_message::Opaque { data: Some(vec![1]), urgency: None }), ..Default::default() };
        for m in [no_id, no_opaque, bad_type, group] {
            assert!(from_proto(&m).is_none(), "{m:?}");
        }
    }

    #[test]
    fn identity_keys_lose_their_type_byte() {
        assert_eq!(ringrtc_key(&[5, 1, 2, 3]), [1, 2, 3]);
        assert!(ringrtc_key(&[]).is_empty());
    }

    #[test]
    fn ice_servers_ip_urls_first() {
        let relay = TurnServer {
            username: "u".into(),
            password: "p".into(),
            ttl_seconds: 60,
            urls: vec!["turn:turn.example".into()],
            urls_with_ips: vec!["turn:192.0.2.1".into()],
            hostname: Some("turn.example".into()),
        };
        assert_eq!(ice_servers(&[relay.clone(), relay]).len(), 4);
    }

    fn peer() -> Uuid {
        Uuid::from_u128(2)
    }

    #[test]
    fn outgoing_call_rings_connects_and_ends() {
        let (id, mut active) = (CallId::new(9), None);
        let c = apply(&mut active, peer(), id, &CallState::Outgoing(CallMediaType::Audio), 0).unwrap();
        assert_eq!((c.peer, c.incoming, c.video, c.phase), (peer(), false, false, Phase::Calling));
        assert_eq!(apply(&mut active, peer(), id, &CallState::Ringing, 1).unwrap().phase, Phase::Ringing);
        let c = apply(&mut active, peer(), id, &CallState::Connected, 1_000).unwrap();
        assert_eq!((c.phase, c.connected_at), (Phase::Connected, Some(1_000)));
        // A reconnect keeps the timer running from the first connect.
        assert_eq!(apply(&mut active, peer(), id, &CallState::Connecting, 5_000).unwrap().phase, Phase::Reconnecting);
        let c = apply(&mut active, peer(), id, &CallState::Connected, 9_000).unwrap();
        assert_eq!(c.connected_at, Some(1_000));
        let ended = CallState::Ended(CallEndReason::RemoteHangup, CallSummary::default());
        assert_eq!(apply(&mut active, peer(), id, &ended, 10_000).unwrap().phase, Phase::Ended("Call ended".into()));
        assert!(active.is_none(), "an ended call frees RingRTC for the next");
        assert_eq!(apply(&mut active, peer(), id, &CallState::Concluded, 10_001), None);
    }

    #[test]
    fn incoming_call_and_a_second_caller() {
        let mut active = None;
        let c = apply(&mut active, peer(), CallId::new(1), &CallState::Incoming(CallMediaType::Video), 0).unwrap();
        assert!(c.incoming && c.video);
        // Someone else calls meanwhile: RingRTC handles that one; our call is untouched.
        let other = Uuid::from_u128(3);
        assert_eq!(apply(&mut active, other, CallId::new(2), &CallState::Incoming(CallMediaType::Audio), 1), None);
        assert_eq!(apply(&mut active, other, CallId::new(2), &CallState::Ringing, 1), None);
        assert_eq!(active.as_ref().map(|a| a.id), Some(CallId::new(1)));

        let missed = CallState::Ended(CallEndReason::Timeout, CallSummary::default());
        assert_eq!(apply(&mut active, peer(), CallId::new(1), &missed, 2).unwrap().phase, Phase::Ended("No answer".into()));
    }
}
