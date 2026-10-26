//! The protocol side. presage's futures are `!Send`, so the `Manager` lives on its own
//! thread (current-thread runtime + `LocalSet`) and talks to the UI over channels,
//! much like Signal-Desktop's renderer talks to its SQL worker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::channel::{mpsc as fmpsc, oneshot};
use futures::{SinkExt, Stream, StreamExt, pin_mut};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::master_key::{MasterKey, StorageServiceKey};
use presage::libsignal_service::prelude::ProfileKey;
use presage::libsignal_service::StorageService;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::proto::data_message::{Quote as ProtoQuote, Reaction};
use presage::proto::sync_message;
use presage::proto::{
    AttachmentPointer, BodyRange, DataMessage, GroupContextV2, ReceiptMessage, SyncMessage,
    TypingMessage, receipt_message, typing_message,
};
use presage::store::{ContentsStore, StateStore, Thread};
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tokio::sync::mpsc;

use crate::state::AppState;
use crate::timeline::{Delivery, Kind, Timeline};

type Mgr = Manager<SqliteStore, Registered>;

/// Rows loaded when opening a conversation (reactions/receipts count as rows).
const HISTORY_ROWS: usize = 2000;

#[derive(Clone, Debug)]
pub struct QuoteRef {
    pub id: u64,
    pub author: Uuid,
    pub text: String,
    pub ranges: Vec<BodyRange>,
}

#[derive(Debug)]
pub enum Command {
    Link { device_name: String },
    Open(Thread),
    Close,
    Send {
        thread: Thread,
        ts: u64,
        body: String,
        ranges: Vec<BodyRange>,
        attachments: Vec<PathBuf>,
        quote: Option<QuoteRef>,
    },
    React { thread: Thread, target_author: Uuid, target_ts: u64, emoji: String, remove: bool },
    Typing { thread: Thread, started: bool },
    FetchAttachment(AttachmentPointer),
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub thread: Thread,
    pub title: String,
    pub last_ts: u64,
    pub preview: String,
    pub unread: usize,
    pub is_group: bool,
}

#[derive(Clone, Debug)]
pub enum Event {
    Started(mpsc::UnboundedSender<Command>),
    NeedsLink,
    LinkUrl(String),
    LinkFailed(String),
    Ready { me: Uuid },
    Names(HashMap<Uuid, String>),
    Conversations(Vec<Conversation>),
    Timeline { thread: Thread, timeline: Timeline },
    Incoming { thread: Thread, content: Box<Content> },
    Receipt { timestamps: Vec<u64>, level: Delivery },
    Typing { thread: Thread, who: Uuid, started: bool },
    Sent { thread: Thread, ts: u64, error: Option<String> },
    Attachment { key: String, path: Result<PathBuf, String> },
    Error(String),
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Stable cache key for an attachment (digest is unique per upload).
pub fn attachment_key(p: &AttachmentPointer) -> String {
    p.digest.as_deref().map(hex::encode).unwrap_or_else(|| {
        format!("{:?}-{}", p.attachment_identifier, p.size.unwrap_or(0))
    })
}

pub fn group_id(master_key: &[u8; 32]) -> [u8; 32] {
    GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key)).get_group_identifier()
}

/// Starts the backend thread; the stream yields `Event::Started` first.
pub fn run(data_dir: PathBuf) -> impl Stream<Item = Event> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = fmpsc::unbounded();
    std::thread::Builder::new()
        .name("signal-backend".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let rt = match rt {
                Ok(rt) => rt,
                Err(e) => return drop(ev_tx.unbounded_send(Event::Error(e.to_string()))),
            };
            let local = tokio::task::LocalSet::new();
            let errors = ev_tx.clone();
            if let Err(e) = local.block_on(&rt, main(data_dir, cmd_rx, ev_tx)) {
                let _ = errors.unbounded_send(Event::Error(format!("{e:#}")));
            }
        })
        .expect("spawn backend thread");
    futures::stream::once(async move { Event::Started(cmd_tx) }).chain(ev_rx)
}

/// Local DB key, like Signal-Desktop's legacy `config.json` key (owner-only file).
fn db_passphrase(data_dir: &Path) -> anyhow::Result<String> {
    let path = data_dir.join("db.key");
    if let Ok(k) = std::fs::read_to_string(&path) {
        return Ok(k.trim().to_owned());
    }
    let key = hex::encode(rand::random::<[u8; 32]>());
    std::fs::write(&path, &key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

async fn main(
    data_dir: PathBuf,
    mut cmds: mpsc::UnboundedReceiver<Command>,
    events: fmpsc::UnboundedSender<Event>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir.join("attachments"))?;
    let pass = db_passphrase(&data_dir)?;
    let db = data_dir.join("signal.db3");
    let store =
        SqliteStore::open_with_passphrase(&db.to_string_lossy(), Some(&pass), OnNewIdentity::Trust)
            .await?;

    let manager = if store.is_registered().await {
        Manager::load_registered(store).await?
    } else {
        link(store, &mut cmds, &events).await?
    };

    let mut b = Backend {
        me: ServiceId::from(manager.registration_data().service_ids.aci()).raw_uuid(),
        manager,
        events,
        state: AppState::load(&data_dir.join("state.json")),
        attachments_dir: data_dir.join("attachments"),
        open: None,
        group_ids: HashMap::new(),
    };
    b.emit(Event::Ready { me: b.me });
    b.refresh().await;
    if let Err(e) = b.sync_storage().await {
        b.emit(Event::Error(format!("Couldn't sync your chat list: {e:#}")));
    }
    b.refresh().await;

    // Receive loop on a clone; incoming content is funneled back through this channel so
    // all state (read markers, open thread) stays on one task.
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Received>();
    let mut rx_manager = b.manager.clone();
    tokio::task::spawn_local(async move {
        loop {
            match rx_manager.receive_messages().await {
                Ok(stream) => {
                    pin_mut!(stream);
                    while let Some(r) = stream.next().await {
                        if in_tx.send(r).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => tracing::warn!(%e, "receive stream failed"),
            }
            tracing::info!("websocket closed, reconnecting");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(cmd) => b.command(cmd).await,
                None => return Ok(()), // UI gone
            },
            Some(r) = in_rx.recv() => b.received(r).await,
        }
    }
}

async fn link(
    store: SqliteStore,
    cmds: &mut mpsc::UnboundedReceiver<Command>,
    events: &fmpsc::UnboundedSender<Event>,
) -> anyhow::Result<Mgr> {
    loop {
        let _ = events.unbounded_send(Event::NeedsLink);
        let device_name = loop {
            match cmds.recv().await {
                Some(Command::Link { device_name }) => break device_name,
                Some(_) => continue,
                None => anyhow::bail!("UI closed before linking"),
            }
        };
        let (tx, rx) = oneshot::channel();
        let mut ev = events.clone();
        let (result, _) = futures::join!(
            Manager::link_secondary_device(store.clone(), SignalServers::Production, device_name, tx),
            async move {
                if let Ok(url) = rx.await {
                    let _ = ev.send(Event::LinkUrl(url.to_string())).await;
                }
            }
        );
        match result {
            Ok(mut m) => {
                if let Err(e) = m.request_contacts().await {
                    tracing::warn!(%e, "contact sync request failed");
                }
                return Ok(m);
            }
            Err(e) => {
                let _ = events.unbounded_send(Event::LinkFailed(e.to_string()));
            }
        }
    }
}

struct Backend {
    manager: Mgr,
    me: Uuid,
    events: fmpsc::UnboundedSender<Event>,
    state: AppState,
    attachments_dir: PathBuf,
    open: Option<Thread>,
    /// group identifier -> master key, to route typing messages.
    group_ids: HashMap<[u8; 32], [u8; 32]>,
}

impl Backend {
    fn emit(&self, e: Event) {
        let _ = self.events.unbounded_send(e);
    }

    fn report<T, E: std::fmt::Display>(&self, what: &str, r: Result<T, E>) -> Option<T> {
        r.map_err(|e| {
            tracing::warn!("{what}: {e}");
            self.emit(Event::Error(format!("{what}: {e}")));
        })
        .ok()
    }

    async fn names(&self) -> HashMap<Uuid, String> {
        let mut names: HashMap<Uuid, String> = self
            .state
            .profile_names
            .iter()
            .filter_map(|(id, n)| Some((Uuid::parse_str(id).ok()?, n.clone())))
            .collect();
        if let Ok(contacts) = self.manager.store().contacts().await {
            for c in contacts.flatten() {
                if !c.name.is_empty() {
                    names.insert(c.uuid, c.name);
                } else if let Some(p) = c.phone_number {
                    names.entry(c.uuid).or_insert_with(|| p.to_string());
                }
            }
        }
        names.insert(self.me, "Note to Self".into());
        names
    }

    /// Pulls the account's chat list from the storage service, as Signal-Desktop does after
    /// linking: saves profile keys, fetches groups, remembers which chats to list.
    async fn sync_storage(&mut self) -> anyhow::Result<()> {
        let store = self.manager.store();
        let master = match store.fetch_master_key().await? {
            Some(k) => k,
            None => match store.fetch_account_entropy_pool().await? {
                Some(aep) => MasterKey { inner: aep.derive_svr_key() },
                None => anyhow::bail!("the phone hasn't shared the account keys yet"),
            },
        };
        let svc = StorageService::new(self.manager.push_service(), StorageServiceKey::from_master_key(&master)).await?;
        let manifest = svc.manifest().await?;
        let keys: Vec<Vec<u8>> = manifest.identifiers.iter().map(|i| i.raw.clone()).collect();
        let ikm = (!manifest.record_ikm.is_empty()).then_some(manifest.record_ikm.as_slice());
        let mut records = Vec::new();
        for chunk in keys.chunks(500) {
            records.extend(svc.read_items(chunk.to_vec(), ikm).await?);
        }
        let plan = crate::storage::plan(&records);
        tracing::info!(records = records.len(), listed = plan.listed.len(), groups = plan.groups.len(), "storage sync");

        let mut store = self.manager.store().clone();
        for (id, key) in &plan.profile_keys {
            store.upsert_profile_key(id, ProfileKey::create(*key)).await?;
        }
        for key in &plan.groups {
            if let Err(e) = self.manager.fetch_group(key).await {
                tracing::warn!(%e, "failed to fetch group from storage service");
            }
        }
        self.state.set_storage(plan.listed, plan.names);
        Ok(())
    }

    /// Rebuilds the conversation list from the store.
    async fn refresh(&mut self) {
        let names = self.names().await;
        let store = self.manager.store();
        let mut threads: Vec<(Thread, String, bool)> = Vec::new();
        if let Ok(groups) = store.groups().await {
            for (key, g) in groups.flatten() {
                self.group_ids.insert(group_id(&key), key);
                threads.push((Thread::Group(key), g.title, true));
            }
        }
        if let Ok(contacts) = store.contacts().await {
            for c in contacts.flatten() {
                let title = names.get(&c.uuid).cloned().unwrap_or_else(|| c.uuid.to_string());
                threads.push((Thread::Contact(ServiceId::Aci(c.uuid.into())), title, false));
            }
        }
        // People from the storage service who aren't in the address book.
        for key in &self.state.listed {
            let Some(id) = ServiceId::parse_from_service_id_string(key) else { continue };
            let thread = Thread::Contact(id);
            if !threads.iter().any(|(t, ..)| *t == thread) {
                let title = names.get(&id.raw_uuid()).cloned().unwrap_or_else(|| "Unknown".into());
                threads.push((thread, title, false));
            }
        }
        let me = Thread::Contact(ServiceId::Aci(self.me.into()));
        threads.retain(|(t, ..)| *t != me); // titled "Note to Self" below, not by our own contact name
        threads.push((me.clone(), "Note to Self".into(), false));

        let mut out = Vec::new();
        for (thread, title, is_group) in threads {
            let Some((last_ts, preview)) = self.last_message(&thread).await else {
                // Groups, Note to Self and chats the storage service lists show even when empty.
                if is_group || thread == me || self.state.listed.contains(&crate::state::thread_key(&thread)) {
                    out.push(Conversation { thread, title, last_ts: 0, preview: String::new(), unread: 0, is_group });
                }
                continue; // contacts without messages live in the "new chat" search
            };
            let unread = self.unread(&thread).await;
            out.push(Conversation { thread, title, last_ts, preview, unread, is_group });
        }
        out.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
        self.emit(Event::Names(names));
        self.emit(Event::Conversations(out));
    }

    async fn last_message(&self, thread: &Thread) -> Option<(u64, String)> {
        // Search recent windows first: the sqlite store fetches whole ranges.
        let now = now_ms();
        for since in [now.saturating_sub(7 * 86_400_000), now.saturating_sub(180 * 86_400_000), 0] {
            let rows = self.manager.store().messages(thread, since..).await.ok()?;
            let t = Timeline::from_newest_first(self.me, rows.flatten(), 200);
            if let Some(m) = t.messages.last() {
                return Some((m.id, m.preview()));
            }
        }
        None
    }

    async fn unread(&self, thread: &Thread) -> usize {
        let since = self.state.last_read(thread) + 1;
        let Ok(rows) = self.manager.store().messages(thread, since..).await else { return 0 };
        let t = Timeline::from_newest_first(self.me, rows.flatten(), usize::MAX);
        t.messages.iter().filter(|m| !m.outgoing && m.kind == Kind::Normal && !m.deleted).count()
    }

    async fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Link { .. } => {}
            Command::Open(thread) => {
                let rows = self.manager.store().messages(&thread, ..).await;
                let Some(rows) = self.report("load conversation", rows) else { return };
                let timeline = Timeline::from_newest_first(self.me, rows.flatten(), HISTORY_ROWS);
                self.open = Some(thread.clone());
                self.mark_read(&thread, &timeline).await;
                self.emit(Event::Timeline { thread, timeline });
                self.refresh().await;
            }
            Command::Close => self.open = None,
            Command::Send { thread, ts, body, ranges, attachments, quote } => {
                let error = self.send(&thread, ts, body, ranges, attachments, quote).await.err();
                self.state.mark_read(&thread, ts);
                self.emit(Event::Sent { thread, ts, error: error.map(|e| format!("{e:#}")) });
                self.refresh().await;
            }
            Command::React { thread, target_author, target_ts, emoji, remove } => {
                let dm = DataMessage {
                    reaction: Some(Reaction {
                        emoji: Some(emoji),
                        remove: Some(remove),
                        target_author_aci: Some(target_author.to_string()),
                        target_sent_timestamp: Some(target_ts),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let r = self.send_body(&thread, dm, now_ms()).await;
                self.report("send reaction", r);
            }
            Command::Typing { thread, started } => {
                let action = if started { typing_message::Action::Started } else { typing_message::Action::Stopped };
                let msg = TypingMessage {
                    timestamp: Some(now_ms()),
                    action: Some(action as i32),
                    group_id: match &thread {
                        Thread::Group(k) => Some(group_id(k).to_vec()),
                        Thread::Contact(_) => None,
                    },
                };
                // Typing is best-effort, like Signal: don't surface failures.
                if let Err(e) = self.send_body(&thread, msg, now_ms()).await {
                    tracing::debug!(%e, "typing indicator failed");
                }
            }
            Command::FetchAttachment(p) => {
                let key = attachment_key(&p);
                let path = self.fetch_attachment(&p, &key).await.map_err(|e| format!("{e:#}"));
                self.emit(Event::Attachment { key, path });
            }
        }
    }

    async fn fetch_attachment(&self, p: &AttachmentPointer, key: &str) -> anyhow::Result<PathBuf> {
        let ext = mime_guess::get_mime_extensions_str(p.content_type.as_deref().unwrap_or(""))
            .and_then(|e| e.first())
            .unwrap_or(&"bin");
        let path = self.attachments_dir.join(format!("{key}.{ext}"));
        if !path.exists() {
            let data = self.manager.get_attachment(p).await?;
            let tmp = path.with_extension("part");
            tokio::fs::write(&tmp, data).await?;
            tokio::fs::rename(&tmp, &path).await?;
        }
        Ok(path)
    }

    async fn send(
        &mut self,
        thread: &Thread,
        ts: u64,
        body: String,
        ranges: Vec<BodyRange>,
        files: Vec<PathBuf>,
        quote: Option<QuoteRef>,
    ) -> anyhow::Result<()> {
        let mut specs = Vec::new();
        for path in files {
            let data = tokio::fs::read(&path).await?;
            specs.push((
                AttachmentSpec {
                    content_type: mime_guess::from_path(&path).first_or_octet_stream().to_string(),
                    length: data.len(),
                    file_name: path.file_name().map(|s| s.to_string_lossy().into_owned()),
                    preview: None,
                    voice_note: None,
                    borderless: None,
                    width: None,
                    height: None,
                    caption: None,
                    blur_hash: None,
                },
                data,
            ));
        }
        let attachments =
            self.manager.upload_attachments(specs).await?.into_iter().collect::<Result<Vec<_>, _>>()?;
        let dm = DataMessage {
            body: (!body.is_empty()).then_some(body),
            body_ranges: ranges,
            attachments,
            quote: quote.map(|q| ProtoQuote {
                id: Some(q.id),
                author_aci: Some(q.author.to_string()),
                text: Some(q.text),
                body_ranges: q.ranges,
                r#type: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.send_body(thread, dm, ts).await
    }

    async fn send_body(&mut self, thread: &Thread, body: impl Into<ContentBody>, ts: u64) -> anyhow::Result<()> {
        let mut body = body.into();
        if let ContentBody::DataMessage(dm) = &mut body {
            dm.timestamp = Some(ts);
        }
        match thread {
            Thread::Contact(id) => self.manager.send_message(*id, body, ts).await?,
            Thread::Group(key) => {
                if let ContentBody::DataMessage(dm) = &mut body {
                    let revision = self.manager.store().group(*key).await?.map_or(0, |g| g.revision);
                    dm.group_v2 = Some(GroupContextV2 {
                        master_key: Some(key.to_vec()),
                        revision: Some(revision),
                        ..Default::default()
                    });
                }
                self.manager.send_message_to_group(key, body, ts).await?
            }
        }
        Ok(())
    }

    /// Advances the read marker, sends read receipts to authors and a read sync to our devices.
    async fn mark_read(&mut self, thread: &Thread, timeline: &Timeline) {
        let since = self.state.last_read(thread);
        let unread: Vec<_> = timeline
            .messages
            .iter()
            .filter(|m| !m.outgoing && m.id > since && m.kind == Kind::Normal)
            .map(|m| (m.author, m.id))
            .collect();
        let Some(newest) = timeline.messages.last().map(|m| m.id) else { return };
        self.state.mark_read(thread, newest);
        if unread.is_empty() {
            return;
        }
        let mut by_author: HashMap<Uuid, Vec<u64>> = HashMap::new();
        for (a, ts) in &unread {
            by_author.entry(*a).or_default().push(*ts);
        }
        for (author, timestamp) in by_author {
            let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Read as i32), timestamp };
            let r = self.manager.send_message(ServiceId::Aci(author.into()), receipt, now_ms()).await;
            if let Err(e) = r {
                tracing::warn!(%e, "read receipt failed");
            }
        }
        let sync = SyncMessage {
            read: unread
                .iter()
                .map(|(a, ts)| sync_message::Read {
                    sender_aci: Some(a.to_string()),
                    timestamp: Some(*ts),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let me = ServiceId::Aci(self.me.into());
        if let Err(e) = self.manager.send_message(me, sync, now_ms()).await {
            tracing::warn!(%e, "read sync failed");
        }
    }

    async fn received(&mut self, r: Received) {
        match r {
            Received::QueueEmpty | Received::Contacts => self.refresh().await,
            Received::DecryptionError(id) => {
                tracing::warn!(sender = %id.service_id_string(), "decryption error");
            }
            Received::Content(content) => self.content(*content).await,
        }
    }

    async fn content(&mut self, c: Content) {
        let sender = c.metadata.sender.raw_uuid();
        match &c.body {
            ContentBody::ReceiptMessage(r) => {
                let level = match r.r#type.and_then(|t| receipt_message::Type::try_from(t).ok()) {
                    Some(receipt_message::Type::Delivery) => Delivery::Delivered,
                    Some(_) => Delivery::Read,
                    None => return,
                };
                self.emit(Event::Receipt { timestamps: r.timestamp.clone(), level });
                return;
            }
            ContentBody::TypingMessage(t) => {
                let thread = match t.group_id.as_deref().and_then(|g| <[u8; 32]>::try_from(g).ok()) {
                    Some(id) => match self.group_ids.get(&id) {
                        Some(key) => Thread::Group(*key),
                        None => return,
                    },
                    None => Thread::Contact(c.metadata.sender),
                };
                let started = t.action != Some(typing_message::Action::Stopped as i32);
                self.emit(Event::Typing { thread, who: sender, started });
                return;
            }
            ContentBody::SynchronizeMessage(SyncMessage { read, .. }) if !read.is_empty() => {
                // Read on another device: advance our markers too.
                for r in read {
                    let author = r.sender_aci.as_deref().and_then(|s| Uuid::parse_str(s).ok());
                    let (Some(author), Some(ts)) = (author, r.timestamp) else { continue };
                    let thread = self
                        .manager
                        .store()
                        .thread_for_sender_and_timestamp(&ServiceId::Aci(author.into()), ts)
                        .await;
                    if let Ok(Some(thread)) = thread {
                        self.state.mark_read(&thread, ts);
                    }
                }
                self.refresh().await;
                return;
            }
            _ => {}
        }
        let Ok(thread) = Thread::try_from(&c) else { return };

        let is_visible_incoming = sender != self.me
            && matches!(&c.body, ContentBody::DataMessage(dm)
                if dm.reaction.is_none() && (dm.body.is_some() || !dm.attachments.is_empty() || dm.sticker.is_some()));
        if is_visible_incoming {
            // Delivery receipt, as every Signal client does.
            let ts = match &c.body {
                ContentBody::DataMessage(dm) => dm.timestamp.unwrap_or(0),
                _ => 0,
            };
            let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Delivery as i32), timestamp: vec![ts] };
            if let Err(e) = self.manager.send_message(c.metadata.sender, receipt, now_ms()).await {
                tracing::debug!(%e, "delivery receipt failed");
            }
            if self.open.as_ref() == Some(&thread) {
                let mut t = Timeline::new(self.me);
                t.apply(&c);
                self.mark_read(&thread, &t).await;
            } else {
                self.notify(&thread, &c).await;
            }
        }
        self.emit(Event::Incoming { thread, content: Box::new(c) });
        self.refresh().await;
    }

    async fn notify(&self, thread: &Thread, c: &Content) {
        let mut t = Timeline::new(self.me);
        t.apply(c);
        let Some(m) = t.messages.last() else { return };
        let sender = self.names().await.get(&m.author).cloned().unwrap_or_else(|| "Signal".into());
        let title = match thread {
            Thread::Group(k) => match self.manager.store().group(*k).await {
                Ok(Some(g)) => format!("{sender} in {}", g.title),
                _ => sender,
            },
            Thread::Contact(_) => sender,
        };
        let body = m.preview();
        // notify-rust blocks on D-Bus; keep it off the protocol task.
        std::thread::spawn(move || {
            let _ = notify_rust::Notification::new().appname("Signal").summary(&title).body(&body).show();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_id_is_deterministic_and_distinct() {
        assert_eq!(group_id(&[1; 32]), group_id(&[1; 32]));
        assert_ne!(group_id(&[1; 32]), group_id(&[2; 32]));
        assert_ne!(group_id(&[1; 32]), [1; 32]);
    }

    #[test]
    fn attachment_keys() {
        let p = AttachmentPointer { digest: Some(vec![0xab, 0xcd]), ..Default::default() };
        assert_eq!(attachment_key(&p), "abcd");
        let q = AttachmentPointer { size: Some(3), ..Default::default() };
        assert!(attachment_key(&q).ends_with("-3"));
    }

    #[test]
    fn passphrase_is_created_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let a = db_passphrase(dir.path()).unwrap();
        let b = db_passphrase(dir.path()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("db.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
