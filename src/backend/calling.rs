//! Glue between presage and `calls::media` (feature `calls`): received `CallMessage`s go into
//! RingRTC, its signaling goes out as `CallMessage`s, its states go to the UI.

use std::time::Duration;

use anyhow::Context;
use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{IdentityKeyStore, ProtocolAddress, ServiceId};
use presage::proto::CallMessage;
use presage::store::{Store, Thread};
use ringrtc::core::signaling;
use ringrtc::native::CallState;
use tokio::sync::mpsc::UnboundedSender;

use super::{Backend, Event, now_ms};
use crate::calls::media::{self, Active, Engine};
use crate::calls::{CallCommand, Out};

pub(super) struct Calling {
    tx: UnboundedSender<Out>,
    /// Started on the first call (it opens the audio devices).
    engine: Option<Engine>,
    active: Option<Active>,
}

impl Calling {
    pub(super) fn new(tx: UnboundedSender<Out>) -> Self {
        Calling { tx, engine: None, active: None }
    }
}

impl Backend {
    fn engine(&mut self) -> Option<&mut Engine> {
        if self.calling.engine.is_none() {
            let e = Engine::new(self.calling.tx.clone());
            self.calling.engine = self.report("start calling", e);
        }
        self.calling.engine.as_mut()
    }

    fn local_device(&self) -> u32 {
        u32::from(self.manager.device_id())
    }

    pub(super) fn call_command(&mut self, c: CallCommand) {
        let id = self.calling.active.as_ref().map(|a| a.id);
        match c {
            CallCommand::Start(peer) if id.is_none() => {
                let device = self.local_device();
                if let Some(e) = self.engine() {
                    e.start(peer, device);
                }
            }
            CallCommand::Start(_) => {} // one call at a time
            CallCommand::Accept => {
                if let (Some(id), Some(e)) = (id, &mut self.calling.engine) {
                    e.accept(id);
                }
            }
            CallCommand::Hangup => {
                if let Some(e) = &mut self.calling.engine {
                    e.hangup();
                }
            }
            CallCommand::Mute(muted) => {
                if let (Some(a), Some(e)) = (&mut self.calling.active, &mut self.calling.engine) {
                    e.mute(muted);
                    a.call.muted = muted;
                    let call = a.call.clone();
                    self.emit(Event::Call(call));
                }
            }
        }
    }

    pub(super) async fn rtc_call_message(&mut self, c: &Content, m: &CallMessage) {
        let device = self.local_device();
        // Our own devices' messages, and ones for another of our devices, aren't for this one.
        if c.metadata.sender.raw_uuid() == self.me || m.destination_device_id.is_some_and(|d| d != device) {
            return;
        }
        let Some((id, msg)) = media::from_proto(m) else { return };
        let sender_device = u32::from(c.metadata.sender_device);
        let is_offer = matches!(msg, signaling::Message::Offer(_));
        if is_offer && self.is_request(&Thread::Contact(c.metadata.sender)).await {
            // Like Desktop: no ringing for message requests; the caller learns why.
            let hangup = signaling::Message::Hangup(signaling::Hangup::NeedPermission(Some(sender_device)));
            let body = ContentBody::CallMessage(media::to_proto(id, None, hangup));
            if let Err(e) = self.manager.send_message(c.metadata.sender, body, now_ms()).await {
                tracing::warn!(%e, "call rejection failed");
            }
            return;
        }
        let keys = match self.identity_keys(c).await {
            Ok(k) => k,
            Err(e) => return tracing::warn!("call message without identity keys: {e:#}"),
        };
        let held = now_ms().saturating_sub(c.metadata.server_timestamp.timestamp_millis() as u64);
        // Only an offer starts RingRTC; anything else belongs to a call it's already running.
        let engine = if is_offer { self.engine() } else { self.calling.engine.as_mut() };
        if let Some(e) = engine {
            let peer = c.metadata.sender.raw_uuid();
            e.received(peer, sender_device, device, id, msg, Duration::from_millis(held), keys.0, keys.1);
        }
    }

    /// (theirs, ours), as RingRTC binds the call's keys to them.
    async fn identity_keys(&self, c: &Content) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let store = self.manager.store().aci_protocol_store();
        let addr = ProtocolAddress::new(c.metadata.sender.service_id_string(), c.metadata.sender_device);
        let theirs = store.get_identity(&addr).await?.context("no identity key for the caller")?;
        let ours = store.get_identity_key_pair().await?;
        Ok((media::ringrtc_key(&theirs.serialize()), media::ringrtc_key(&ours.identity_key().serialize())))
    }

    pub(super) async fn call_out(&mut self, o: Out) {
        match o {
            Out::Signal { peer, id, device, msg } => {
                let ok = match Uuid::parse_str(&peer) {
                    Ok(aci) => {
                        let body = ContentBody::CallMessage(media::to_proto(id, device, msg));
                        let r = self.manager.send_message(ServiceId::Aci(aci.into()), body, now_ms()).await;
                        r.map_err(|e| tracing::warn!(%e, "call signaling failed")).is_ok()
                    }
                    Err(_) => false,
                };
                if let Some(e) = &mut self.calling.engine {
                    e.sent(id, ok);
                }
            }
            Out::State { peer, id, state } => {
                let Ok(peer) = Uuid::parse_str(&peer) else { return };
                let Some(call) = media::apply(&mut self.calling.active, peer, id, &state, now_ms()) else { return };
                self.emit(Event::Call(call));
                if let CallState::Incoming(_) | CallState::Outgoing(_) = state {
                    // Fresh TURN credentials for every call, as they expire.
                    let relays = self.manager.calling_relays().await;
                    let relays = self.report("start the call", relays);
                    if let Some(e) = &mut self.calling.engine {
                        match relays {
                            Some(r) => e.proceed(id, &r),
                            None => e.drop_call(id),
                        }
                    }
                }
            }
        }
    }
}
