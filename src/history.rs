//! Link-time message history ("Transfer message history" on the phone). The primary uploads an
//! encrypted backup archive; we decrypt it with libsignal's backup reader and store each
//! message as the same `Content` presage saves for live traffic, so the timeline needs nothing new.
//! Mirrors Signal-Desktop's `backups.downloadAndImport` for the ephemeral-key case.

use std::collections::HashMap;
use std::path::Path;

use libsignal_account_keys::BackupKey;
use libsignal_message_backup::frame::{FileReaderFactory, FramesReader};
use libsignal_message_backup::key::MessageBackupKey;
use libsignal_message_backup::parse::VarintDelimitedReader;
use libsignal_message_backup::proto::backup as pb;
use presage::libsignal_service::content::{Content, ContentBody, Metadata};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::proto::attachment_pointer::AttachmentIdentifier;
use presage::proto::body_range::{AssociatedValue, Style};
use presage::proto::data_message::{Quote, Reaction, Sticker};
use presage::proto::{AttachmentPointer, BodyRange, DataMessage, EditMessage, NullMessage};
use presage::store::Thread;
use protobuf::Message as _;

enum Who {
    Me,
    Contact(Uuid),
    Group([u8; 32]),
}

/// Converts decrypted backup frames into `(thread, content)` rows to save, oldest first.
#[derive(Default)]
pub struct Importer {
    me: Uuid,
    recipients: HashMap<u64, Who>,
    chats: HashMap<u64, u64>,
    pub rows: Vec<(Thread, Content)>,
}

fn content(sender: Uuid, me: Uuid, ts: u64, body: impl Into<ContentBody>) -> Content {
    let at = chrono::DateTime::from_timestamp_millis(ts as i64).unwrap_or_default();
    Content::from_body(
        body,
        Metadata {
            sender: ServiceId::Aci(sender.into()),
            destination: ServiceId::Aci(me.into()),
            sender_device: 1u8.try_into().expect("1 is a valid device id"),
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

fn ranges(text: &pb::Text) -> Vec<BodyRange> {
    text.bodyRanges
        .iter()
        .filter_map(|r| {
            let value = match r.associatedValue.as_ref()? {
                pb::body_range::AssociatedValue::MentionAci(b) => AssociatedValue::MentionAciBinary(b.clone()),
                pb::body_range::AssociatedValue::Style(s) => AssociatedValue::Style(match s.enum_value().ok()? {
                    pb::body_range::Style::BOLD => Style::Bold,
                    pb::body_range::Style::ITALIC => Style::Italic,
                    pb::body_range::Style::SPOILER => Style::Spoiler,
                    pb::body_range::Style::STRIKETHROUGH => Style::Strikethrough,
                    pb::body_range::Style::MONOSPACE => Style::Monospace,
                    _ => return None,
                } as i32),
                _ => return None,
            };
            Some(BodyRange { start: Some(r.start), length: Some(r.length), associated_value: Some(value) })
        })
        .collect()
}

/// Only transit-tier files (still on the attachment CDN) can be fetched by a linked device.
fn attachment(f: &pb::FilePointer) -> Option<AttachmentPointer> {
    let loc = f.locatorInfo.as_ref()?;
    let digest = match loc.integrityCheck.as_ref()? {
        pb::file_pointer::locator_info::IntegrityCheck::EncryptedDigest(d) => d.clone(),
        _ => return None, // plaintext-hash-only files live on the media tier we can't reach
    };
    Some(AttachmentPointer {
        cdn_number: loc.transitCdnNumber,
        attachment_identifier: Some(AttachmentIdentifier::CdnKey(loc.transitCdnKey.clone()?)),
        key: Some(loc.key.clone()),
        digest: Some(digest),
        size: Some(loc.size),
        content_type: f.contentType.clone(),
        file_name: f.fileName.clone(),
        width: f.width,
        height: f.height,
        caption: f.caption.clone(),
        blur_hash: f.blurHash.clone(),
        ..Default::default()
    })
}

impl Importer {
    pub fn new(me: Uuid) -> Self {
        Importer { me, ..Default::default() }
    }

    fn uuid(&self, recipient: u64) -> Option<Uuid> {
        match self.recipients.get(&recipient)? {
            Who::Me => Some(self.me),
            Who::Contact(u) => Some(*u),
            Who::Group(_) => None,
        }
    }

    pub fn frame(&mut self, frame: pb::Frame) {
        match frame.item {
            Some(pb::frame::Item::Recipient(r)) => {
                let who = match r.destination {
                    Some(pb::recipient::Destination::Self_(_)) => Who::Me,
                    Some(pb::recipient::Destination::Contact(c)) => {
                        match c.aci.as_deref().and_then(|b| Uuid::from_slice(b).ok()) {
                            Some(u) => Who::Contact(u),
                            None => return, // e164-only contacts can't be addressed
                        }
                    }
                    Some(pb::recipient::Destination::Group(g)) => match <[u8; 32]>::try_from(g.masterKey.as_slice()) {
                        Ok(k) => Who::Group(k),
                        Err(_) => return,
                    },
                    _ => return,
                };
                self.recipients.insert(r.id, who);
            }
            Some(pb::frame::Item::Chat(c)) => {
                self.chats.insert(c.id, c.recipientId);
            }
            Some(pb::frame::Item::ChatItem(item)) => self.chat_item(item),
            _ => {}
        }
    }

    fn chat_item(&mut self, item: pb::ChatItem) {
        let Some(thread) = self.chats.get(&item.chatId).and_then(|r| self.recipients.get(r)).map(|w| match w {
            Who::Me => Thread::Contact(ServiceId::Aci(self.me.into())),
            Who::Contact(u) => Thread::Contact(ServiceId::Aci((*u).into())),
            Who::Group(k) => Thread::Group(*k),
        }) else {
            return;
        };
        let Some(author) = self.uuid(item.authorId) else { return };
        // The original sent timestamp identifies the message; edits keep it (like presage's store).
        let id = item.revisions.first().map_or(item.dateSent, |r| r.dateSent);
        let me = self.me;

        let (mut dm, reactions) = match item.item {
            Some(pb::chat_item::Item::StandardMessage(m)) => {
                let text = m.text.as_ref();
                let dm = DataMessage {
                    body: text.map(|t| t.body.clone()).filter(|b| !b.is_empty()),
                    body_ranges: text.map(ranges).unwrap_or_default(),
                    attachments: m.attachments.iter().filter_map(|a| a.pointer.as_ref().and_then(attachment)).collect(),
                    quote: m.quote.as_ref().map(|q| Quote {
                        id: q.targetSentTimestamp,
                        author_aci: self.uuid(q.authorId).map(|u| u.to_string()),
                        text: q.text.as_ref().map(|t| t.body.clone()),
                        body_ranges: q.text.as_ref().map(ranges).unwrap_or_default(),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                (dm, m.reactions)
            }
            Some(pb::chat_item::Item::StickerMessage(m)) => {
                let s = m.sticker.as_ref();
                let dm = DataMessage {
                    sticker: s.map(|s| Sticker {
                        pack_id: Some(s.packId.clone()),
                        pack_key: Some(s.packKey.clone()),
                        sticker_id: Some(s.stickerId),
                        emoji: s.emoji.clone(),
                        data: s.data.as_ref().and_then(attachment),
                    }),
                    ..Default::default()
                };
                (dm, m.reactions)
            }
            Some(pb::chat_item::Item::RemoteDeletedMessage(_)) => {
                // presage's representation of a deleted-for-everyone message.
                self.rows.push((thread, content(Uuid::nil(), me, id, NullMessage::default())));
                return;
            }
            _ => return, // updates, payments, calls, polls: not shown by this client yet
        };
        if dm.body.is_none() && dm.attachments.is_empty() && dm.sticker.is_none() {
            return;
        }
        dm.timestamp = Some(item.dateSent);
        let row = if item.revisions.is_empty() {
            content(author, me, id, dm)
        } else {
            content(author, me, id, EditMessage { target_sent_timestamp: Some(id), data_message: Some(dm) })
        };
        self.rows.push((thread.clone(), row));

        for r in reactions {
            let Some(reactor) = self.uuid(r.authorId) else { continue };
            // shortcut: a reaction row shares the (timestamp, thread) key space with messages; a
            // collision overwrites one row. Signal timestamps are ms-precise, so this is rare.
            let ts = if r.sentTimestamp == 0 { id + 1 + r.sortOrder } else { r.sentTimestamp };
            let reaction = DataMessage {
                reaction: Some(Reaction {
                    emoji: Some(r.emoji),
                    remove: Some(false),
                    target_author_aci: Some(author.to_string()),
                    target_sent_timestamp: Some(id),
                    ..Default::default()
                }),
                timestamp: Some(ts),
                ..Default::default()
            };
            self.rows.push((thread.clone(), content(reactor, me, ts, reaction)));
        }
    }
}

/// Decrypts and parses a downloaded archive (HMAC is verified before any frame is used).
pub async fn read_archive(path: &Path, ephemeral_key: [u8; 32], me: Uuid) -> anyhow::Result<Importer> {
    let backup_key: BackupKey = BackupKey(ephemeral_key);
    let backup_id = backup_key.derive_backup_id(&Aci::from(me));
    let key = MessageBackupKey::derive(&backup_key, &backup_id, None);
    let frames = FramesReader::new(&key, FileReaderFactory { path }).await?;
    let mut reader = VarintDelimitedReader::new(frames);
    // First frame is BackupInfo (version, timestamps); nothing in it we need.
    reader.read_next().await?.ok_or_else(|| anyhow::anyhow!("empty history archive"))?;
    let mut importer = Importer::new(me);
    while let Some(bytes) = reader.read_next().await? {
        importer.frame(pb::Frame::parse_from_bytes(&bytes)?);
    }
    Ok(importer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{Delivery, Timeline};

    fn recipient(id: u64, d: pb::recipient::Destination) -> pb::Frame {
        let mut r = pb::Recipient::new();
        r.id = id;
        r.destination = Some(d);
        let mut f = pb::Frame::new();
        f.item = Some(pb::frame::Item::Recipient(r));
        f
    }

    fn chat(id: u64, recipient: u64) -> pb::Frame {
        let mut c = pb::Chat::new();
        c.id = id;
        c.recipientId = recipient;
        let mut f = pb::Frame::new();
        f.item = Some(pb::frame::Item::Chat(c));
        f
    }

    fn message(chat: u64, author: u64, sent: u64, body: &str, f: impl FnOnce(&mut pb::StandardMessage, &mut pb::ChatItem)) -> pb::Frame {
        let mut m = pb::StandardMessage::new();
        let mut t = pb::Text::new();
        t.body = body.into();
        m.text = Some(t).into();
        let mut item = pb::ChatItem::new();
        item.chatId = chat;
        item.authorId = author;
        item.dateSent = sent;
        f(&mut m, &mut item);
        item.item = Some(pb::chat_item::Item::StandardMessage(m));
        let mut fr = pb::Frame::new();
        fr.item = Some(pb::frame::Item::ChatItem(item));
        fr
    }

    const ME: u64 = 1;
    const ALICE: u64 = 2;
    const GROUP: u64 = 3;

    fn importer() -> Importer {
        let mut i = Importer::new(Uuid::from_bytes([1; 16]));
        i.frame(recipient(ME, pb::recipient::Destination::Self_(pb::Self_::new())));
        let mut c = pb::Contact::new();
        c.aci = Some(vec![2; 16]);
        i.frame(recipient(ALICE, pb::recipient::Destination::Contact(c)));
        let mut g = pb::Group::new();
        g.masterKey = vec![7; 32];
        i.frame(recipient(GROUP, pb::recipient::Destination::Group(g)));
        i.frame(chat(10, ALICE));
        i.frame(chat(11, GROUP));
        i
    }

    fn timeline(i: &Importer, thread: &Thread) -> Timeline {
        let mut t = Timeline::new(i.me);
        for (th, c) in &i.rows {
            if th == thread {
                t.apply(c);
            }
        }
        t
    }

    fn alice() -> Thread {
        Thread::Contact(ServiceId::Aci(Uuid::from_bytes([2; 16]).into()))
    }

    #[test]
    fn messages_land_in_the_right_threads_with_direction() {
        let mut i = importer();
        i.frame(message(10, ALICE, 100, "hi", |_, _| {}));
        i.frame(message(10, ME, 200, "hello back", |_, _| {}));
        i.frame(message(11, ALICE, 300, "group hi", |_, _| {}));
        let t = timeline(&i, &alice());
        assert_eq!(t.messages.iter().map(|m| (m.body.as_str(), m.outgoing)).collect::<Vec<_>>(), [("hi", false), ("hello back", true)]);
        assert_eq!(t.messages[1].delivery, Delivery::Sent);
        let g = timeline(&i, &Thread::Group([7; 32]));
        assert_eq!(g.messages.len(), 1);
        assert_eq!(g.messages[0].author, Uuid::from_bytes([2; 16]));
    }

    #[test]
    fn rich_text_reactions_and_quotes_survive() {
        let mut i = importer();
        i.frame(message(10, ALICE, 100, "bold", |m, _| {
            let mut r = pb::BodyRange::new();
            r.start = 0;
            r.length = 4;
            r.associatedValue = Some(pb::body_range::AssociatedValue::Style(pb::body_range::Style::BOLD.into()));
            m.text.as_mut().unwrap().bodyRanges.push(r);
            let mut re = pb::Reaction::new();
            re.emoji = "❤️".into();
            re.authorId = ME;
            re.sentTimestamp = 150;
            m.reactions.push(re);
        }));
        i.frame(message(10, ME, 200, "reply", |m, _| {
            let mut q = pb::Quote::new();
            q.targetSentTimestamp = Some(100);
            q.authorId = ALICE;
            let mut t = pb::Text::new();
            t.body = "bold".into();
            q.text = Some(t).into();
            m.quote = Some(q).into();
        }));
        let t = timeline(&i, &alice());
        assert_eq!(t.messages.len(), 2, "reactions are not rows");
        assert_eq!(t.messages[0].ranges.len(), 1);
        assert_eq!(t.messages[0].reactions, vec![(Uuid::from_bytes([1; 16]), "❤️".to_string())]);
        let q = t.messages[1].quote.as_ref().unwrap();
        assert_eq!((q.id, q.author, q.text.as_str()), (100, Some(Uuid::from_bytes([2; 16])), "bold"));
    }

    #[test]
    fn edits_keep_the_original_id_and_deletes_become_placeholders() {
        let mut i = importer();
        i.frame(message(10, ALICE, 500, "v2", |_, item| {
            let mut old = pb::ChatItem::new();
            old.dateSent = 400;
            item.revisions.push(old);
        }));
        let mut del = pb::ChatItem::new();
        del.chatId = 10;
        del.authorId = ALICE;
        del.dateSent = 600;
        del.item = Some(pb::chat_item::Item::RemoteDeletedMessage(pb::RemoteDeletedMessage::new()));
        let mut f = pb::Frame::new();
        f.item = Some(pb::frame::Item::ChatItem(del));
        i.frame(f);
        let t = timeline(&i, &alice());
        assert_eq!((t.messages[0].id, t.messages[0].edited, t.messages[0].body.as_str()), (400, true, "v2"));
        assert!(t.messages[1].deleted);
    }

    #[test]
    fn unknown_chats_authors_and_empty_items_are_skipped() {
        let mut i = importer();
        i.frame(message(99, ALICE, 1, "no such chat", |_, _| {}));
        i.frame(message(10, 42, 2, "no such author", |_, _| {}));
        i.frame(message(10, ALICE, 3, "", |_, _| {}));
        assert!(i.rows.is_empty());
    }

    /// libsignal's own encrypted fixture, made with the same key scheme a link-time transfer uses.
    #[tokio::test]
    async fn decrypts_a_real_encrypted_archive_and_rejects_a_wrong_key() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-account.binproto.encrypted");
        let key: BackupKey = BackupKey::derive_from_account_entropy_pool(&"m".repeat(64).parse().unwrap());
        let me = Uuid::from_bytes([0x11; 16]);
        let imported = read_archive(&path, key.0, me).await.expect("decrypts with the right key");
        assert!(imported.rows.is_empty(), "account-only fixture has no messages");

        let mut wrong = key.0;
        wrong[0] ^= 1;
        assert!(read_archive(&path, wrong, me).await.is_err(), "HMAC must reject a wrong key");
        assert!(read_archive(&path, key.0, Uuid::from_bytes([0x12; 16])).await.is_err(), "key is bound to the ACI");
    }

    #[test]
    fn transit_attachments_map_and_media_tier_ones_are_skipped() {
        let mut fp = pb::FilePointer::new();
        let mut loc = pb::file_pointer::LocatorInfo::new();
        loc.key = vec![1; 64];
        loc.size = 10;
        loc.transitCdnKey = Some("abc".into());
        loc.transitCdnNumber = Some(3);
        loc.integrityCheck = Some(pb::file_pointer::locator_info::IntegrityCheck::EncryptedDigest(vec![9]));
        fp.locatorInfo = Some(loc.clone()).into();
        fp.contentType = Some("image/jpeg".into());
        let p = attachment(&fp).unwrap();
        assert_eq!((p.cdn_number, p.digest.as_deref()), (Some(3), Some(&[9u8][..])));
        assert!(matches!(p.attachment_identifier, Some(AttachmentIdentifier::CdnKey(ref k)) if k == "abc"));

        loc.integrityCheck = Some(pb::file_pointer::locator_info::IntegrityCheck::PlaintextHash(vec![1]));
        fp.locatorInfo = Some(loc).into();
        assert!(attachment(&fp).is_none());
    }
}
