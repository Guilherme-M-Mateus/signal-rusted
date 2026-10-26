//! Folds a thread's raw stored `Content`s (messages, reactions, edits, deletes, receipts)
//! into the timeline the UI renders. `apply` works both for loading history and live updates.

use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::data_message::{Flags, Quote as ProtoQuote, Reaction};
use presage::proto::sync_message::{self, Content as SyncContent};
use presage::proto::{AttachmentPointer, BodyRange, DataMessage, EditMessage, receipt_message};

/// Signal's window for editing and deleting for everyone.
pub const EDIT_WINDOW_MS: u64 = 24 * 3600 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Delivery {
    Failed,
    Sending,
    Sent,
    Delivered,
    Read,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Quote {
    pub id: u64,
    pub author: Option<Uuid>,
    pub text: String,
    pub ranges: Vec<BodyRange>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Normal,
    /// Timeline notices: group changes, timer changes, calls.
    System(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Sent timestamp: Signal's message id together with the author.
    pub id: u64,
    pub author: Uuid,
    pub outgoing: bool,
    pub kind: Kind,
    pub body: String,
    pub ranges: Vec<BodyRange>,
    pub attachments: Vec<AttachmentPointer>,
    pub quote: Option<Quote>,
    /// (reactor, emoji); one reaction per person, like Signal.
    pub reactions: Vec<(Uuid, String)>,
    pub edited: bool,
    pub deleted: bool,
    pub delivery: Delivery,
}

impl Message {
    pub fn new(id: u64, author: Uuid, outgoing: bool) -> Self {
        Message {
            id,
            author,
            outgoing,
            kind: Kind::Normal,
            body: String::new(),
            ranges: Vec::new(),
            attachments: Vec::new(),
            quote: None,
            reactions: Vec::new(),
            edited: false,
            deleted: false,
            delivery: if outgoing { Delivery::Sent } else { Delivery::Delivered },
        }
    }

    /// Delete for everyone: our own sent messages inside Signal's 24h window.
    pub fn can_delete_for_everyone(&self, now: u64) -> bool {
        self.outgoing
            && !self.deleted
            && self.kind == Kind::Normal
            && self.delivery >= Delivery::Sent
            && now.saturating_sub(self.id) < EDIT_WINDOW_MS
    }

    /// Edit: the same window, and only messages with text.
    pub fn can_edit(&self, now: u64) -> bool {
        self.can_delete_for_everyone(now) && !self.body.trim().is_empty()
    }

    /// One-line text for the conversation list and quote previews.
    pub fn preview(&self) -> String {
        if self.deleted {
            return "This message was deleted.".into();
        }
        if let Kind::System(s) = &self.kind {
            return s.clone();
        }
        let text = self.body.replace('\u{FFFC}', "@");
        match (text.trim().is_empty(), self.attachments.first()) {
            (false, _) => text,
            (true, Some(a)) => attachment_label(a),
            (true, None) => String::new(),
        }
    }
}

pub fn attachment_label(a: &AttachmentPointer) -> String {
    let ct = a.content_type.as_deref().unwrap_or("");
    let kind = if ct.starts_with("image/") {
        "📷 Photo"
    } else if ct.starts_with("video/") {
        "🎥 Video"
    } else if ct.starts_with("audio/") {
        "🎤 Audio"
    } else {
        "📎 File"
    };
    match &a.file_name {
        Some(n) if !ct.starts_with("image/") => format!("{kind}: {n}"),
        _ => kind.into(),
    }
}

#[derive(Clone, Debug, Default)]
pub struct Timeline {
    pub me: Uuid,
    /// Sorted by `id`.
    pub messages: Vec<Message>,
}

fn parse_uuid(s: Option<&String>, bin: Option<&Vec<u8>>) -> Option<Uuid> {
    bin.and_then(|b| Uuid::from_slice(b).ok())
        .or_else(|| s.and_then(|s| Uuid::parse_str(s).ok()))
}

fn ts_of(c: &Content) -> u64 {
    c.metadata.client_timestamp.timestamp_millis() as u64
}

impl Timeline {
    pub fn new(me: Uuid) -> Self {
        Timeline { me, messages: Vec::new() }
    }

    /// Builds from the store's newest-first iterator, keeping roughly the newest `limit` rows.
    pub fn from_newest_first(me: Uuid, rows: impl IntoIterator<Item = Content>, limit: usize) -> Self {
        let mut rows: Vec<Content> = rows.into_iter().take(limit).collect();
        rows.reverse();
        let mut t = Timeline::new(me);
        for c in &rows {
            t.apply(c);
        }
        t
    }

    pub fn find(&self, author: Uuid, id: u64) -> Option<&Message> {
        self.messages.iter().rev().find(|m| m.id == id && m.author == author)
    }

    fn find_mut(&mut self, author: Uuid, id: u64) -> Option<&mut Message> {
        self.messages.iter_mut().rev().find(|m| m.id == id && m.author == author)
    }

    fn upsert(&mut self, m: Message) {
        if let Some(existing) = self.find_mut(m.author, m.id) {
            let (reactions, delivery) = (std::mem::take(&mut existing.reactions), existing.delivery);
            *existing = m;
            existing.reactions = reactions;
            existing.delivery = existing.delivery.max(delivery);
            return;
        }
        let at = self.messages.partition_point(|x| x.id <= m.id);
        self.messages.insert(at, m);
    }

    /// Applies one stored or live `Content`. Returns true if the timeline changed.
    pub fn apply(&mut self, c: &Content) -> bool {
        let sender = c.metadata.sender.raw_uuid();
        match &c.body {
            ContentBody::DataMessage(dm) => {
                let outgoing = sender == self.me;
                self.data_message(dm, sender, outgoing, dm.timestamp.unwrap_or_else(|| ts_of(c)))
            }
            ContentBody::SynchronizeMessage(sync) => match &sync.content {
                Some(SyncContent::Sent(sync_message::Sent { message: Some(dm), timestamp, .. })) => {
                    let id = timestamp.or(dm.timestamp).unwrap_or_else(|| ts_of(c));
                    self.data_message(dm, self.me, true, id)
                }
                Some(SyncContent::Sent(sync_message::Sent { edit_message: Some(edit), .. })) => {
                    self.edit(edit, self.me, true)
                }
                Some(SyncContent::CallEvent(_)) => {
                    let mut m = Message::new(ts_of(c), self.me, false);
                    m.kind = Kind::System("📞 Call".into());
                    self.upsert(m);
                    true
                }
                _ => false,
            },
            ContentBody::EditMessage(edit) => self.edit(edit, sender, sender == self.me),
            ContentBody::ReceiptMessage(r) => {
                let level = match r.r#type.and_then(|t| receipt_message::Type::try_from(t).ok()) {
                    Some(receipt_message::Type::Delivery) => Delivery::Delivered,
                    Some(receipt_message::Type::Read | receipt_message::Type::Viewed) => Delivery::Read,
                    None => return false,
                };
                self.receipt(&r.timestamp, level)
            }
            // presage replaces a deleted-for-everyone message with a NullMessage from the nil UUID.
            ContentBody::NullMessage(_) if sender.is_nil() => {
                let id = ts_of(c);
                match self.messages.iter_mut().rev().find(|m| m.id == id) {
                    Some(m) => m.deleted = true,
                    None => {
                        let mut m = Message::new(id, sender, false);
                        m.deleted = true;
                        self.upsert(m);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Marks own messages with these sent timestamps; receipts only ever upgrade status.
    pub fn receipt(&mut self, timestamps: &[u64], level: Delivery) -> bool {
        let mut changed = false;
        for m in self.messages.iter_mut().filter(|m| m.outgoing && timestamps.contains(&m.id)) {
            if level > m.delivery {
                m.delivery = level;
                changed = true;
            }
        }
        changed
    }

    fn data_message(&mut self, dm: &DataMessage, author: Uuid, outgoing: bool, id: u64) -> bool {
        if let Some(r) = &dm.reaction {
            return self.reaction(r, author);
        }
        if let Some(target) = dm.delete.as_ref().and_then(|d| d.target_sent_timestamp) {
            return self.find_mut(author, target).map(|m| m.deleted = true).is_some();
        }
        let mut m = Message::new(id, author, outgoing);
        let flags = dm.flags.unwrap_or(0);
        if flags & Flags::ExpirationTimerUpdate as u32 != 0 {
            m.kind = Kind::System(timer_notice(dm.expire_timer.unwrap_or(0)));
        } else if dm.body.is_none() && dm.attachments.is_empty() && dm.sticker.is_none() {
            if dm.group_v2.as_ref().is_some_and(|g| g.group_change.is_some()) {
                m.kind = Kind::System("Group updated".into());
            } else {
                return false; // profile key updates and other invisible messages
            }
        }
        fill(&mut m, dm);
        self.upsert(m);
        true
    }

    fn edit(&mut self, edit: &EditMessage, author: Uuid, outgoing: bool) -> bool {
        let (Some(target), Some(dm)) = (edit.target_sent_timestamp, &edit.data_message) else {
            return false;
        };
        // A stored delete (nil author, see `apply`) sorts before the edit rows: keep it deleted.
        if self.messages.iter().any(|m| m.id == target && m.deleted && (m.author == author || m.author.is_nil())) {
            return false;
        }
        let mut m = Message::new(target, author, outgoing);
        fill(&mut m, dm);
        m.edited = true;
        if let Some(old) = self.find(author, target) {
            m.quote = m.quote.or_else(|| old.quote.clone());
            if m.attachments.is_empty() {
                m.attachments = old.attachments.clone();
            }
        }
        self.upsert(m);
        true
    }

    fn reaction(&mut self, r: &Reaction, reactor: Uuid) -> bool {
        let Some(target_ts) = r.target_sent_timestamp else { return false };
        let Some(target_author) =
            parse_uuid(r.target_author_aci.as_ref(), r.target_author_aci_binary.as_ref())
        else {
            return false;
        };
        let Some(m) = self.find_mut(target_author, target_ts) else { return false };
        m.reactions.retain(|(who, _)| *who != reactor);
        if !r.remove.unwrap_or(false) {
            if let Some(e) = &r.emoji {
                m.reactions.push((reactor, e.clone()));
            }
        }
        true
    }
}

fn fill(m: &mut Message, dm: &DataMessage) {
    m.body = dm.body.clone().unwrap_or_default();
    m.ranges = dm.body_ranges.clone();
    m.attachments = dm.attachments.clone();
    if let Some(sticker) = dm.sticker.as_ref().and_then(|s| s.data.clone()) {
        m.attachments.push(sticker);
    }
    m.quote = dm.quote.as_ref().map(quote);
}

fn quote(q: &ProtoQuote) -> Quote {
    Quote {
        id: q.id.unwrap_or(0),
        author: parse_uuid(q.author_aci.as_ref(), q.author_aci_binary.as_ref()),
        text: q.text.clone().unwrap_or_default(),
        ranges: q.body_ranges.clone(),
    }
}

pub fn timer_notice(seconds: u32) -> String {
    if seconds == 0 {
        return "Disappearing messages turned off".into();
    }
    format!("Disappearing message time set to {}", human_duration(seconds))
}

pub fn human_duration(s: u32) -> String {
    let (n, unit) = match s {
        s if s % 604_800 == 0 => (s / 604_800, "week"),
        s if s % 86_400 == 0 => (s / 86_400, "day"),
        s if s % 3_600 == 0 => (s / 3_600, "hour"),
        s if s % 60 == 0 => (s / 60, "minute"),
        s => (s, "second"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use presage::libsignal_service::content::Metadata;
    use presage::libsignal_service::protocol::ServiceId;
    use presage::proto::data_message::Delete;
    use presage::proto::{GroupContextV2, NullMessage, ReceiptMessage, SyncMessage};

    pub fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    pub fn content(sender: Uuid, ts: u64, body: impl Into<ContentBody>) -> Content {
        let at = chrono::DateTime::from_timestamp_millis(ts as i64).unwrap();
        Content::from_body(
            body,
            Metadata {
                sender: ServiceId::Aci(sender.into()),
                destination: ServiceId::Aci(uuid(1).into()),
                sender_device: 1u8.try_into().unwrap(),
                pni_verified: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
        )
    }

    pub fn text(ts: u64, body: &str) -> DataMessage {
        DataMessage { body: Some(body.into()), timestamp: Some(ts), ..Default::default() }
    }

    fn react(target_author: Uuid, target: u64, emoji: &str, remove: bool) -> DataMessage {
        DataMessage {
            reaction: Some(Reaction {
                emoji: Some(emoji.into()),
                remove: Some(remove),
                target_author_aci: Some(target_author.to_string()),
                target_sent_timestamp: Some(target),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    const ME: u8 = 1;
    const ALICE: u8 = 2;
    const BOB: u8 = 3;

    fn tl() -> Timeline {
        Timeline::new(uuid(ME))
    }

    #[test]
    fn incoming_and_outgoing_messages_sorted() {
        let mut t = tl();
        assert!(t.apply(&content(uuid(ALICE), 20, text(20, "second"))));
        assert!(t.apply(&content(uuid(ME), 10, text(10, "first"))));
        assert_eq!(t.messages.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        assert!(t.messages[0].outgoing && !t.messages[1].outgoing);
        assert_eq!(t.messages[0].delivery, Delivery::Sent);
    }

    #[test]
    fn sync_sent_from_other_device_is_outgoing() {
        let mut t = tl();
        let sync = SyncMessage {
            content: Some(SyncContent::Sent(sync_message::Sent {
                timestamp: Some(5),
                message: Some(text(5, "from phone")),
                ..Default::default()
            })),
            ..Default::default()
        };
        t.apply(&content(uuid(ME), 6, sync));
        assert_eq!(t.messages.len(), 1);
        assert!(t.messages[0].outgoing);
        assert_eq!(t.messages[0].id, 5);
        assert_eq!(t.messages[0].author, uuid(ME));
    }

    #[test]
    fn reactions_add_replace_and_remove_per_person() {
        let mut t = tl();
        t.apply(&content(uuid(ME), 10, text(10, "hi")));
        t.apply(&content(uuid(ALICE), 11, react(uuid(ME), 10, "👍", false)));
        t.apply(&content(uuid(BOB), 12, react(uuid(ME), 10, "😂", false)));
        t.apply(&content(uuid(ALICE), 13, react(uuid(ME), 10, "❤️", false)));
        assert_eq!(
            t.messages[0].reactions,
            vec![(uuid(BOB), "😂".into()), (uuid(ALICE), "❤️".into())]
        );
        t.apply(&content(uuid(BOB), 14, react(uuid(ME), 10, "😂", true)));
        assert_eq!(t.messages[0].reactions, vec![(uuid(ALICE), "❤️".into())]);
        assert_eq!(t.messages.len(), 1, "reactions never become timeline rows");
    }

    #[test]
    fn reaction_to_unknown_or_other_authors_message_is_ignored() {
        let mut t = tl();
        t.apply(&content(uuid(ALICE), 10, text(10, "hi")));
        // Same timestamp but wrong author: must not attach.
        assert!(!t.apply(&content(uuid(BOB), 11, react(uuid(ME), 10, "👍", false))));
        assert!(!t.apply(&content(uuid(BOB), 11, react(uuid(ALICE), 99, "👍", false))));
        assert!(t.messages[0].reactions.is_empty());
    }

    #[test]
    fn edits_replace_body_keep_reactions_and_mark_edited() {
        let mut t = tl();
        t.apply(&content(uuid(ALICE), 10, text(10, "helo")));
        t.apply(&content(uuid(ME), 11, react(uuid(ALICE), 10, "👍", false)));
        let edit = EditMessage { target_sent_timestamp: Some(10), data_message: Some(text(12, "hello")) };
        t.apply(&content(uuid(ALICE), 12, edit));
        assert_eq!(t.messages.len(), 1);
        let m = &t.messages[0];
        assert_eq!((m.body.as_str(), m.edited, m.reactions.len()), ("hello", true, 1));
    }

    #[test]
    fn stored_edit_without_original_still_shows() {
        // The sqlite store overwrites the original row with the edit (same ts key).
        let mut t = tl();
        let edit = EditMessage { target_sent_timestamp: Some(10), data_message: Some(text(12, "v2")) };
        t.apply(&content(uuid(ALICE), 10, edit));
        assert_eq!((t.messages[0].id, t.messages[0].edited), (10, true));
    }

    #[test]
    fn delete_for_everyone_live_and_stored() {
        let mut t = tl();
        t.apply(&content(uuid(ALICE), 10, text(10, "oops")));
        let del = DataMessage { delete: Some(Delete { target_sent_timestamp: Some(10) }), ..Default::default() };
        t.apply(&content(uuid(ALICE), 11, del));
        assert!(t.messages[0].deleted);
        assert_eq!(t.messages[0].preview(), "This message was deleted.");

        let mut stored = tl();
        stored.apply(&content(Uuid::nil(), 10, NullMessage::default()));
        assert!(stored.messages[0].deleted);
    }

    #[test]
    fn edits_never_resurrect_deleted_messages() {
        let edit = || EditMessage { target_sent_timestamp: Some(10), data_message: Some(text(12, "v2")) };
        // Stored order: the original row became a nil-author NullMessage; the edit row stays.
        let mut stored = tl();
        stored.apply(&content(Uuid::nil(), 10, NullMessage::default()));
        assert!(!stored.apply(&content(uuid(ME), 12, edit())));
        assert_eq!(stored.messages.len(), 1);
        assert!(stored.messages[0].deleted);

        let mut live = tl();
        live.apply(&content(uuid(ME), 10, text(10, "v1")));
        let del = DataMessage { delete: Some(Delete { target_sent_timestamp: Some(10) }), ..Default::default() };
        live.apply(&content(uuid(ME), 11, del));
        assert!(!live.apply(&content(uuid(ME), 12, edit())));
        assert!(live.messages[0].deleted);
    }

    #[test]
    fn edit_and_delete_for_everyone_window() {
        let day = EDIT_WINDOW_MS;
        let mut m = Message::new(day, uuid(ME), true);
        m.body = "hi".into();
        assert!(m.can_edit(day + 1) && m.can_delete_for_everyone(day + 1));
        assert!(!m.can_edit(2 * day) && !m.can_delete_for_everyone(2 * day), "24h old");
        m.delivery = Delivery::Sending;
        assert!(!m.can_edit(day), "not sent yet");
        m.delivery = Delivery::Read;
        m.body = " ".into();
        assert!(!m.can_edit(day) && m.can_delete_for_everyone(day), "photo without text");
        let incoming = Message::new(day, uuid(ALICE), false);
        assert!(!incoming.can_edit(day) && !incoming.can_delete_for_everyone(day));
        m.deleted = true;
        assert!(!m.can_delete_for_everyone(day));
    }

    #[test]
    fn non_nil_null_messages_are_ignored() {
        let mut t = tl();
        assert!(!t.apply(&content(uuid(ALICE), 10, NullMessage::default())));
        assert!(t.messages.is_empty());
    }

    #[test]
    fn receipts_only_upgrade_outgoing() {
        let mut t = tl();
        t.apply(&content(uuid(ME), 10, text(10, "a")));
        t.apply(&content(uuid(ALICE), 10, text(10, "same ts, incoming")));
        let receipt = |ty: receipt_message::Type| ReceiptMessage { r#type: Some(ty as i32), timestamp: vec![10] };
        t.apply(&content(uuid(ALICE), 20, receipt(receipt_message::Type::Read)));
        t.apply(&content(uuid(ALICE), 21, receipt(receipt_message::Type::Delivery)));
        let mine = t.find(uuid(ME), 10).unwrap();
        assert_eq!(mine.delivery, Delivery::Read, "delivery after read must not downgrade");
        assert_eq!(t.find(uuid(ALICE), 10).unwrap().delivery, Delivery::Delivered);
    }

    #[test]
    fn quotes_and_attachments() {
        let mut t = tl();
        let dm = DataMessage {
            body: None,
            timestamp: Some(10),
            attachments: vec![AttachmentPointer {
                content_type: Some("image/jpeg".into()),
                ..Default::default()
            }],
            quote: Some(ProtoQuote {
                id: Some(5),
                author_aci: Some(uuid(ME).to_string()),
                text: Some("original".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        t.apply(&content(uuid(ALICE), 10, dm));
        let m = &t.messages[0];
        assert_eq!(m.preview(), "📷 Photo");
        let q = m.quote.as_ref().unwrap();
        assert_eq!((q.id, q.author, q.text.as_str()), (5, Some(uuid(ME)), "original"));
    }

    #[test]
    fn system_notices_and_invisible_messages() {
        let mut t = tl();
        let timer = DataMessage {
            flags: Some(Flags::ExpirationTimerUpdate as u32),
            expire_timer: Some(3600),
            timestamp: Some(1),
            ..Default::default()
        };
        t.apply(&content(uuid(ALICE), 1, timer));
        let group = DataMessage {
            group_v2: Some(GroupContextV2 { group_change: Some(vec![1]), ..Default::default() }),
            timestamp: Some(2),
            ..Default::default()
        };
        t.apply(&content(uuid(ALICE), 2, group));
        let profile_key_update = DataMessage {
            flags: Some(Flags::ProfileKeyUpdate as u32),
            timestamp: Some(3),
            ..Default::default()
        };
        assert!(!t.apply(&content(uuid(ALICE), 3, profile_key_update)));
        let notices: Vec<_> = t.messages.iter().map(Message::preview).collect();
        assert_eq!(notices, ["Disappearing message time set to 1 hour", "Group updated"]);
    }

    #[test]
    fn history_from_newest_first_respects_limit_and_order() {
        let rows: Vec<Content> = (1..=10u64)
            .rev()
            .map(|ts| content(uuid(ALICE), ts, text(ts, &ts.to_string())))
            .collect();
        let t = Timeline::from_newest_first(uuid(ME), rows, 3);
        assert_eq!(t.messages.iter().map(|m| m.id).collect::<Vec<_>>(), [8, 9, 10]);
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(30), "30 seconds");
        assert_eq!(human_duration(60), "1 minute");
        assert_eq!(human_duration(86_400 * 2), "2 days");
        assert_eq!(human_duration(604_800), "1 week");
        assert_eq!(timer_notice(0), "Disappearing messages turned off");
    }
}
