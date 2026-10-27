//! Call awareness without media. This device can't take calls (no RingRTC), so it only shows
//! that the phone is ringing and records how the call ended, as `CallEvent` rows like the ones
//! our other devices sync. It never sends a call message: Signal-Desktop answers offers only
//! through RingRTC (busy when already in a call, hangup "need permission" to message requests),
//! and any of those from here would stop the user's phone ringing.

use presage::libsignal_service::prelude::{ProtobufMessage, Uuid};
use presage::proto::call_message::{hangup, offer};
use presage::proto::sync_message::{CallEvent, call_event};
use presage::proto::CallMessage;
use presage::store::Thread;

/// How long an offer rings before it counts as missed (RingRTC's incoming-call timeout).
pub const RING_MS: u64 = 60_000;

/// `CallEvent.event` for a missed call. Signal never syncs missed calls (Desktop's
/// `statusToProto` maps Missed to null), so the proto has no value; ours stay local.
pub const MISSED: i32 = 100;

#[derive(Clone, Debug, PartialEq)]
pub struct Ring {
    /// The caller's chat, or the group's.
    pub thread: Thread,
    pub caller: Uuid,
    /// The call id (a group ring's ring id).
    pub id: u64,
    pub video: bool,
    /// When the call started (the offer's sent timestamp).
    pub started: u64,
    /// Local time it stops ringing.
    pub until: u64,
}

impl Ring {
    pub fn group(&self) -> bool {
        matches!(self.thread, Thread::Group(_))
    }

    /// The row recording how this 1:1 call ended (`event`: a `call_event::Event` or `MISSED`).
    pub fn event(&self, event: i32) -> CallEvent {
        let Thread::Contact(peer) = &self.thread else { unreachable!("group rings leave no notice") };
        let ty = if self.video { call_event::Type::VideoCall } else { call_event::Type::AudioCall };
        CallEvent {
            conversation_id: Some(peer.service_id_binary()),
            call_id: Some(self.id),
            timestamp: Some(self.started),
            r#type: Some(ty as i32),
            direction: Some(call_event::Direction::Incoming as i32),
            event: Some(event),
        }
    }
}

/// What a call message changed: a ring that started (to notify about) and 1:1 calls that ended.
#[derive(Debug, Default, PartialEq)]
pub struct Change {
    pub started: Option<Ring>,
    pub ended: Vec<(Ring, i32)>,
}

/// RingRTC's opaque signaling (`signaling.CallMessage`); only the group ring is read.
#[derive(Clone, PartialEq, prost::Message)]
struct Signaling {
    #[prost(message, optional, tag = "2")]
    ring_intention: Option<RingIntention>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct RingIntention {
    #[prost(bytes = "vec", optional, tag = "1")]
    group_id: Option<Vec<u8>>,
    /// 0 ring, 1 cancelled.
    #[prost(int32, optional, tag = "2")]
    r#type: Option<i32>,
    #[prost(sfixed64, optional, tag = "3")]
    ring_id: Option<i64>,
}

#[derive(Debug, Default)]
pub struct Calls {
    /// Oldest first; the banner shows the newest.
    pub ringing: Vec<Ring>,
}

impl Calls {
    /// One call message from `caller` (in their chat `from`), sent at `sent` and taken in by the
    /// server at `received` (both ms). `group` finds a group by its id, for group rings.
    pub fn handle(
        &mut self,
        from: Thread,
        caller: Uuid,
        (sent, received): (u64, u64),
        m: &CallMessage,
        now: u64,
        group: impl Fn(&[u8]) -> Option<Thread>,
    ) -> Change {
        let mut change = Change::default();
        if let Some(o) = &m.offer
            && let Some(id) = o.id
        {
            // Calling again replaces the old call (RingRTC's ReCall: missed).
            if let Some(i) = self.ringing.iter().position(|r| r.thread == from) {
                change.ended.push((self.ringing.remove(i), MISSED));
            }
            let video = o.r#type() == offer::Type::OfferVideoCall;
            let ring = Ring { thread: from, caller, id, video, started: sent, until: received + RING_MS };
            if ring.until <= now {
                change.ended.push((ring, MISSED)); // came in while we were offline: already over
            } else {
                self.ringing.push(ring.clone());
                change.started = Some(ring);
            }
        }
        let ended = match (&m.hangup, &m.busy) {
            (Some(h), _) => h.id.map(|id| {
                let event = match h.r#type() {
                    hangup::Type::HangupAccepted => call_event::Event::Accepted as i32,
                    hangup::Type::HangupDeclined => call_event::Event::NotAccepted as i32,
                    // The caller gave up, or another of our devices was busy.
                    _ => MISSED,
                };
                (id, event)
            }),
            (None, Some(b)) => b.id.map(|id| (id, MISSED)),
            _ => None,
        };
        if let Some((id, event)) = ended
            && let Some(i) = self.ringing.iter().position(|r| r.id == id && !r.group())
        {
            change.ended.push((self.ringing.remove(i), event));
        }
        let ring = m.opaque.as_ref().and_then(|o| Signaling::decode(o.data.as_deref()?).ok()?.ring_intention);
        if let Some(RingIntention { group_id: Some(gid), r#type, ring_id: Some(ring_id) }) = ring
            && let Some(thread) = group(&gid)
        {
            let id = ring_id as u64;
            self.ringing.retain(|r| !(r.group() && r.id == id));
            if r#type.unwrap_or(0) == 0 && received + RING_MS > now {
                let ring = Ring { thread, caller, id, video: true, started: sent, until: received + RING_MS };
                self.ringing.push(ring.clone());
                change.started = Some(ring);
            }
        }
        change
    }

    /// Rings past their time: 1:1 calls as missed; group rings just stop (whether someone
    /// answered on the phone isn't visible here, and the phone syncs the outcome).
    pub fn expire(&mut self, now: u64) -> Vec<(Ring, i32)> {
        let (over, ringing) = std::mem::take(&mut self.ringing).into_iter().partition(|r| r.until <= now);
        self.ringing = ringing;
        over.into_iter().filter(|r: &Ring| !r.group()).map(|r| (r, MISSED)).collect()
    }

    pub fn next_timeout(&self) -> Option<u64> {
        self.ringing.iter().map(|r| r.until).min()
    }
}

/// A call record as Signal's timeline words it (`getCallingNotificationText`); `None` hides it
/// (deleted calls, and call-link calls, which have no chat).
pub fn notice(e: &CallEvent) -> Option<String> {
    use call_event::{Direction, Event, Type};
    let group = match e.r#type() {
        Type::AdHocCall => return None,
        t => t == Type::GroupCall,
    };
    let media = if group || e.r#type() == Type::VideoCall { "video" } else { "voice" };
    let outgoing = e.direction() == Direction::Outgoing;
    let missed = e.event == Some(MISSED);
    let what = match e.event() {
        Event::Delete => return None,
        _ if outgoing && group => "Outgoing",
        _ if outgoing && (missed || e.event() == Event::NotAccepted) => "Unanswered",
        _ if outgoing => "Outgoing",
        _ if missed => "Missed",
        Event::NotAccepted => "Declined",
        // Accepted, observed (a group call we saw), or unknown (backups: "interpret as accepted").
        _ => "Incoming",
    };
    Some(format!("📞 {what} {media} call"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::tests::uuid;
    use presage::libsignal_service::protocol::ServiceId;
    use presage::proto::call_message::{Busy, Hangup, Offer, Opaque};

    fn alice() -> Thread {
        Thread::Contact(ServiceId::Aci(uuid(2).into()))
    }

    fn offer(id: u64, video: bool) -> CallMessage {
        let ty = if video { offer::Type::OfferVideoCall } else { offer::Type::OfferAudioCall };
        CallMessage { offer: Some(Offer { id: Some(id), r#type: Some(ty as i32), opaque: None }), ..Default::default() }
    }

    fn hangup(id: u64, ty: hangup::Type) -> CallMessage {
        CallMessage { hangup: Some(Hangup { id: Some(id), r#type: Some(ty as i32), device_id: Some(2) }), ..Default::default() }
    }

    fn handle(c: &mut Calls, m: CallMessage, now: u64) -> Change {
        c.handle(alice(), uuid(2), (1_000, 1_000), &m, now, |_| None)
    }

    #[test]
    fn offer_rings_until_hangup_answer_elsewhere_busy_or_timeout() {
        let mut c = Calls::default();
        let started = handle(&mut c, offer(7, true), 1_000).started.unwrap();
        assert_eq!((started.id, started.video, started.until), (7, true, 1_000 + RING_MS));
        assert_eq!(c.ringing, [started.clone()]);
        assert_eq!(c.next_timeout(), Some(1_000 + RING_MS));
        // Signaling for other calls, and ICE updates, change nothing.
        assert_eq!(handle(&mut c, hangup(8, hangup::Type::HangupNormal), 2_000), Change::default());
        assert_eq!(handle(&mut c, CallMessage { ice_update: vec![Default::default()], ..Default::default() }, 2_000), Change::default());

        let accepted = call_event::Event::Accepted as i32;
        let declined = call_event::Event::NotAccepted as i32;
        for (m, event) in [
            (hangup(7, hangup::Type::HangupNormal), MISSED),
            (hangup(7, hangup::Type::HangupAccepted), accepted),
            (hangup(7, hangup::Type::HangupDeclined), declined),
            (hangup(7, hangup::Type::HangupBusy), MISSED),
            (CallMessage { busy: Some(Busy { id: Some(7) }), ..Default::default() }, MISSED),
        ] {
            let mut c = Calls::default();
            handle(&mut c, offer(7, true), 1_000);
            assert_eq!(handle(&mut c, m, 2_000).ended, [(started.clone(), event)]);
            assert!(c.ringing.is_empty());
        }

        assert!(c.expire(1_000 + RING_MS - 1).is_empty());
        assert_eq!(c.expire(1_000 + RING_MS), [(started, MISSED)]);
        assert!(c.ringing.is_empty() && c.next_timeout().is_none());
    }

    #[test]
    fn stale_offers_are_missed_and_a_new_offer_replaces_the_old() {
        let mut c = Calls::default();
        let late = c.handle(alice(), uuid(2), (1_000, 1_000), &offer(7, false), 1_000 + RING_MS, |_| None);
        assert!(late.started.is_none() && c.ringing.is_empty());
        assert_eq!(late.ended.len(), 1);

        handle(&mut c, offer(7, false), 1_000);
        let again = handle(&mut c, offer(9, false), 1_500);
        assert_eq!((again.ended[0].0.id, again.ended[0].1), (7, MISSED));
        assert_eq!(c.ringing.iter().map(|r| r.id).collect::<Vec<_>>(), [9]);
    }

    fn ring(group_id: &[u8], ty: i32, ring_id: i64) -> CallMessage {
        let s = Signaling {
            ring_intention: Some(RingIntention { group_id: Some(group_id.to_vec()), r#type: Some(ty), ring_id: Some(ring_id) }),
        };
        CallMessage { opaque: Some(Opaque { data: Some(s.encode_to_vec()), urgency: None }), ..Default::default() }
    }

    #[test]
    fn group_rings_show_until_cancelled_or_timed_out_and_leave_no_notice() {
        let friends = Thread::Group([7; 32]);
        let find = |id: &[u8]| (id == [1; 32]).then(|| friends.clone());
        let mut c = Calls::default();
        let ch = c.handle(alice(), uuid(2), (1_000, 1_000), &ring(&[1; 32], 0, 5), 1_000, find);
        assert_eq!(ch.started.map(|r| (r.thread, r.id)), Some((friends.clone(), 5)));
        // Unknown groups and undecodable payloads are ignored.
        assert_eq!(c.handle(alice(), uuid(2), (1_000, 1_000), &ring(&[2; 32], 0, 6), 1_000, find), Change::default());
        let junk = CallMessage { opaque: Some(Opaque { data: Some(vec![0xff, 0xff]), urgency: None }), ..Default::default() };
        assert_eq!(c.handle(alice(), uuid(2), (1_000, 1_000), &junk, 1_000, find), Change::default());
        assert_eq!(c.ringing.len(), 1);

        assert_eq!(c.handle(alice(), uuid(2), (1_000, 1_000), &ring(&[1; 32], 1, 5), 2_000, find), Change::default());
        assert!(c.ringing.is_empty());

        c.handle(alice(), uuid(2), (1_000, 1_000), &ring(&[1; 32], 0, 5), 1_000, find);
        assert!(c.expire(1_000 + RING_MS).is_empty());
        assert!(c.ringing.is_empty());
    }

    #[test]
    fn notices_read_like_signal() {
        use call_event::{Direction as D, Event as E, Type as T};
        let ev = |t: T, d: D, e: i32| CallEvent {
            r#type: Some(t as i32),
            direction: Some(d as i32),
            event: Some(e),
            ..Default::default()
        };
        let (acc, not, del, obs, unk) = (E::Accepted as i32, E::NotAccepted as i32, E::Delete as i32, E::Observed as i32, E::UnknownEvent as i32);
        let table = [
            (T::AudioCall, D::Incoming, acc, Some("📞 Incoming voice call")),
            (T::VideoCall, D::Incoming, acc, Some("📞 Incoming video call")),
            (T::AudioCall, D::Outgoing, acc, Some("📞 Outgoing voice call")),
            (T::VideoCall, D::Outgoing, acc, Some("📞 Outgoing video call")),
            (T::AudioCall, D::Incoming, not, Some("📞 Declined voice call")),
            (T::VideoCall, D::Outgoing, not, Some("📞 Unanswered video call")),
            (T::AudioCall, D::Incoming, MISSED, Some("📞 Missed voice call")),
            (T::VideoCall, D::Incoming, MISSED, Some("📞 Missed video call")),
            (T::AudioCall, D::Incoming, unk, Some("📞 Incoming voice call")),
            (T::UnknownType, D::UnknownDirection, unk, Some("📞 Incoming voice call")),
            (T::AudioCall, D::Incoming, del, None),
            (T::GroupCall, D::Incoming, acc, Some("📞 Incoming video call")),
            (T::GroupCall, D::Incoming, obs, Some("📞 Incoming video call")),
            (T::GroupCall, D::Incoming, not, Some("📞 Declined video call")),
            (T::GroupCall, D::Incoming, MISSED, Some("📞 Missed video call")),
            (T::GroupCall, D::Outgoing, not, Some("📞 Outgoing video call")),
            (T::AdHocCall, D::Outgoing, acc, None),
        ];
        for (t, d, e, want) in table {
            assert_eq!(notice(&ev(t, d, e)).as_deref(), want, "{t:?} {d:?} {e}");
        }
    }
}
