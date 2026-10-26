//! The protocol side. presage's futures are `!Send`, so the `Manager` lives on its own
//! thread (current-thread runtime + `LocalSet`) and talks to the UI over channels,
//! much like Signal-Desktop's renderer talks to its SQL worker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::channel::{mpsc as fmpsc, oneshot};
use futures::{SinkExt, Stream, StreamExt, pin_mut};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::master_key::{MasterKey, StorageServiceKey};
use presage::libsignal_service::prelude::ProfileKey;
use presage::libsignal_service::StorageService;
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::push_service::HttpAuthOverride;
use presage::proto::attachment_pointer::AttachmentIdentifier;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::proto::addressable_message::Author;
use presage::proto::conversation_identifier::Identifier;
use presage::proto::data_message::{Delete, Flags, Quote as ProtoQuote, Reaction};
use presage::proto::sync_message::{self, Content as SyncContent, DeleteForMe, delete_for_me::MessageDeletes};
use presage::proto::{
    AddressableMessage, AttachmentPointer, BodyRange, ConversationIdentifier, DataMessage, EditMessage,
    GroupContextV2, ReceiptMessage, SyncMessage, TypingMessage, receipt_message, typing_message,
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
    /// Replaces our message `target_ts`; `attachments`/`quote` are the original's, which an edit keeps.
    Edit {
        thread: Thread,
        target_ts: u64,
        ts: u64,
        body: String,
        ranges: Vec<BodyRange>,
        attachments: Vec<AttachmentPointer>,
        quote: Option<QuoteRef>,
    },
    DeleteForEveryone { thread: Thread, target_ts: u64 },
    DeleteForMe { thread: Thread, author: Uuid, target_ts: u64 },
    React { thread: Thread, target_author: Uuid, target_ts: u64, emoji: String, remove: bool },
    Typing { thread: Thread, started: bool },
    FetchAttachment(AttachmentPointer),
    /// Disappearing-message timer in seconds (0 = off); 1:1 chats only.
    SetTimer { thread: Thread, seconds: u32 },
    Search(String),
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub thread: Thread,
    pub title: String,
    pub last_ts: u64,
    pub preview: String,
    pub unread: usize,
    pub is_group: bool,
    /// Disappearing-message timer in seconds (0 = off).
    pub expire_timer: u32,
    /// Group members (for @mentions); empty for 1:1 chats.
    pub members: Vec<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchResult {
    pub thread: Thread,
    pub title: String,
    /// Sent timestamp of the matching message.
    pub id: u64,
    pub author: Uuid,
    pub snippet: String,
}

pub const SEARCH_LIMIT: usize = 100;

/// Case-insensitive substring search over one chat's stored rows (newest first, as the store
/// yields them), merged into `results`, which stays newest first and capped at `SEARCH_LIMIT`.
pub fn search_thread(
    me: Uuid,
    query: &str,
    thread: &Thread,
    title: &str,
    rows: impl IntoIterator<Item = Content>,
    results: &mut Vec<SearchResult>,
) {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return;
    }
    let t = Timeline::from_newest_first(me, rows, usize::MAX);
    for m in t.messages.iter().filter(|m| m.kind == Kind::Normal && !m.deleted) {
        let text = m.body.replace('\u{FFFC}', "@");
        let lower = text.to_lowercase();
        let Some(at) = lower.find(&q) else { continue };
        // Start a little before the match so it shows in the one-line sidebar row.
        let skip = lower[..at].chars().count().saturating_sub(20);
        let rest: String = text.chars().skip(skip).collect();
        let rest = rest.split_whitespace().collect::<Vec<_>>().join(" ");
        let snippet = if skip > 0 { format!("…{rest}") } else { rest };
        results.push(SearchResult { thread: thread.clone(), title: title.to_owned(), id: m.id, author: m.author, snippet });
    }
    results.sort_by(|a, b| b.id.cmp(&a.id));
    results.truncate(SEARCH_LIMIT);
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
    SearchResults { query: String, results: Vec<SearchResult> },
    Timeline { thread: Thread, timeline: Timeline },
    Incoming { thread: Thread, content: Box<Content> },
    Receipt { timestamps: Vec<u64>, level: Delivery },
    Typing { thread: Thread, who: Uuid, started: bool },
    /// `attachments` are the uploaded pointers, replacing the optimistic placeholders.
    /// For an edit, `ts` is the edited message's id.
    Sent { thread: Thread, ts: u64, attachments: Vec<AttachmentPointer>, error: Option<String> },
    Attachment { key: String, path: Result<PathBuf, String> },
    /// Running countdowns in a thread: (author, sent ts, expires at ms).
    Expiring { thread: Thread, expires: Vec<(Uuid, u64, u64)> },
    /// Disappeared messages, already deleted from the store: (author, sent ts).
    Expired { thread: Thread, ids: Vec<(Uuid, u64)> },
    Error(String),
    /// Neutral status banner (e.g. history import in progress); `None` clears it.
    Notice(Option<String>),
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

/// Countdowns a shown timeline starts, as (author, sent ts, expires at): Signal starts
/// incoming ones when read (`now_ms`) and outgoing ones when sent. Notices never expire.
pub fn countdowns(t: &Timeline, now_ms: u64) -> impl Iterator<Item = (Uuid, u64, u64)> + '_ {
    t.messages
        .iter()
        .filter(|m| m.expire_timer > 0 && m.kind == Kind::Normal && !m.deleted)
        .map(move |m| {
            let start = if m.outgoing { m.id } else { now_ms };
            (m.author, m.id, crate::state::expires_at(start, m.expire_timer))
        })
}

/// Signal's timer-change message. `expire_timer` being set makes presage bump the chat's
/// `expire_timer_version` and store the new timer once sent.
pub fn timer_update(seconds: u32) -> DataMessage {
    DataMessage {
        flags: Some(Flags::ExpirationTimerUpdate as u32),
        expire_timer: Some(seconds),
        ..Default::default()
    }
}

pub fn group_id(master_key: &[u8; 32]) -> [u8; 32] {
    GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key)).get_group_identifier()
}

fn data_message(body: String, ranges: Vec<BodyRange>, attachments: Vec<AttachmentPointer>, quote: Option<QuoteRef>) -> DataMessage {
    DataMessage {
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
    }
}

/// The `DataMessage` a body carries: its own, or an edit's new version.
fn data_message_mut(body: &mut ContentBody) -> Option<&mut DataMessage> {
    match body {
        ContentBody::DataMessage(dm) | ContentBody::EditMessage(EditMessage { data_message: Some(dm), .. }) => Some(dm),
        _ => None,
    }
}

/// Timestamps of the stored rows holding `author`'s edits of message `target`
/// (presage keys each edit by its own timestamp, not the original's).
fn edit_rows(rows: impl IntoIterator<Item = Content>, author: Uuid, target: u64) -> Vec<u64> {
    rows.into_iter()
        .filter(|c| c.metadata.sender.raw_uuid() == author)
        .filter(|c| {
            let edit = match &c.body {
                ContentBody::EditMessage(e) => Some(e),
                ContentBody::SynchronizeMessage(SyncMessage {
                    content: Some(SyncContent::Sent(sync_message::Sent { edit_message: Some(e), .. })),
                    ..
                }) => Some(e),
                _ => None,
            };
            edit.is_some_and(|e| e.target_sent_timestamp == Some(target))
        })
        .map(|c| c.metadata.client_timestamp.timestamp_millis() as u64)
        .collect()
}

/// Sync telling our other devices that we deleted `author`'s message `ts` for ourselves.
fn delete_for_me_sync(thread: &Thread, author: Uuid, ts: u64) -> SyncMessage {
    let conversation = ConversationIdentifier {
        identifier: Some(match thread {
            Thread::Contact(id) => Identifier::ThreadServiceId(id.service_id_string()),
            Thread::Group(key) => Identifier::ThreadGroupId(group_id(key).to_vec()),
        }),
    };
    let message = AddressableMessage {
        author: Some(Author::AuthorServiceId(ServiceId::Aci(author.into()).service_id_string())),
        sent_timestamp: Some(ts),
    };
    SyncMessage {
        content: Some(SyncContent::DeleteForMe(DeleteForMe {
            message_deletes: vec![MessageDeletes { conversation: Some(conversation), messages: vec![message] }],
            ..Default::default()
        })),
        ..Default::default()
    }
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
    write_private(&path, &key)?;
    Ok(key)
}

/// Writes a secret readable only by the owner.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// presage's QR omits capabilities; `backup5` is what makes the phone offer
/// "Transfer message history" (Signal-Desktop sends the same).
pub fn link_url(url: &url::Url) -> String {
    let mut url = url.clone();
    url.query_pairs_mut().append_pair("capabilities", "backup5");
    url.to_string()
}

#[derive(serde::Deserialize, Debug, PartialEq)]
#[serde(untagged)]
enum TransferArchive {
    Ready { cdn: u32, key: String },
    /// RELINK_REQUESTED or CONTINUE_WITHOUT_UPLOAD: the phone chose not to send history.
    Skipped { error: String },
}

/// Waits for the phone's history upload, downloads and imports it.
/// Returns the newest timestamp per imported thread (to mark them read).
async fn import_history(manager: Mgr, data_dir: PathBuf, key: [u8; 32], me: Uuid) -> anyhow::Result<Vec<(Thread, u64)>> {
    let mut push = manager.push_service();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let archive = loop {
        anyhow::ensure!(std::time::Instant::now() < deadline, "the phone didn't upload history within an hour");
        let resp = push
            .request(reqwest::Method::GET, Endpoint::service("/v1/devices/transfer_archive?timeout=300"), HttpAuthOverride::NoOverride)?
            .send()
            .await?;
        match resp.status().as_u16() {
            204 => continue, // long-poll timed out; the phone is still uploading
            200 => break resp.json::<TransferArchive>().await?,
            s => anyhow::bail!("transfer archive request failed: HTTP {s}"),
        }
    };
    let (cdn, cdn_key) = match archive {
        TransferArchive::Ready { cdn, key } => (cdn, key),
        TransferArchive::Skipped { error } => {
            tracing::info!(error, "phone skipped history transfer");
            let _ = std::fs::remove_file(data_dir.join("transfer.key"));
            return Ok(Vec::new());
        }
    };
    let pointer = AttachmentPointer {
        cdn_number: Some(cdn),
        attachment_identifier: Some(AttachmentIdentifier::CdnKey(cdn_key)),
        ..Default::default()
    };
    let download = push.get_attachment(&pointer).await?;
    let path = data_dir.join("transfer.bin");
    let mut file = futures::io::AllowStdIo::new(std::fs::File::create(&path)?);
    futures::io::copy(download.stream, &mut file).await?;
    drop(file);

    let imported = crate::history::read_archive(&path, key, me).await?;
    tracing::info!(rows = imported.rows.len(), "importing message history");
    let store = manager.store().clone();
    let mut newest: HashMap<Thread, u64> = HashMap::new();
    for (thread, content) in imported.rows {
        let ts = content.metadata.client_timestamp.timestamp_millis() as u64;
        let e = newest.entry(thread.clone()).or_default();
        *e = (*e).max(ts);
        store.save_message(&thread, content).await?;
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(data_dir.join("transfer.key"));
    Ok(newest.into_iter().collect())
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
        link(store, &data_dir, &mut cmds, &events).await?
    };

    let mut b = Backend {
        me: ServiceId::from(manager.registration_data().service_ids.aci()).raw_uuid(),
        manager,
        events,
        state: AppState::load(&data_dir.join("state.json")),
        attachments_dir: data_dir.join("attachments"),
        open: None,
        group_ids: HashMap::new(),
        storage_synced: false,
        threads: Vec::new(),
    };
    b.emit(Event::Ready { me: b.me });
    b.refresh().await;
    b.try_sync_storage().await;

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

    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let transfer_key = std::fs::read_to_string(data_dir.join("transfer.key"))
        .ok()
        .and_then(|k| <[u8; 32]>::try_from(hex::decode(k.trim()).ok()?).ok());
    if let Some(key) = transfer_key {
        b.emit(Event::Notice(Some("Importing your message history from your phone…".into())));
        let (m, dir, me) = (b.manager.clone(), data_dir.clone(), b.me);
        tokio::task::spawn_local(async move {
            let _ = done_tx.send(import_history(m, dir, key, me).await);
        });
    }

    loop {
        // Capped so a suspended machine (monotonic clock paused) still catches up within a minute.
        let next = b.state.next_expiry().map(|at| Duration::from_millis(at.saturating_sub(now_ms())).min(Duration::from_secs(60)));
        tokio::select! {
            _ = async { match next { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => b.expire().await,
            cmd = cmds.recv() => match cmd {
                Some(cmd) => b.command(cmd).await,
                None => return Ok(()), // UI gone
            },
            Some(r) = in_rx.recv() => b.received(r).await,
            Some(result) = done_rx.recv() => {
                b.emit(Event::Notice(None));
                match result {
                    Ok(threads) => {
                        for (thread, ts) in &threads {
                            b.state.mark_read(thread, *ts);
                        }
                        b.refresh().await;
                    }
                    // The key is kept, so the next start retries.
                    Err(e) => b.emit(Event::Error(format!("History import failed: {e:#}"))),
                }
            }
        }
    }
}

async fn link(
    store: SqliteStore,
    data_dir: &Path,
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
                    let _ = ev.send(Event::LinkUrl(link_url(&url))).await;
                }
            }
        );
        match result {
            Ok(mut m) => {
                // presage keeps this key in memory only; persist it until the import finishes,
                // as Signal-Desktop does, so a restart mid-transfer can resume.
                if let Some(key) = m.registration_data().ephemeral_backup_key() {
                    write_private(&data_dir.join("transfer.key"), &hex::encode(key))?;
                }
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
    storage_synced: bool,
    /// The listed chats (thread, title), searched by `Command::Search`.
    threads: Vec<(Thread, String)>,
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

    /// A failure (usually being offline) isn't shown: the last synced list is cached in
    /// `AppState`, and the next reconnect retries.
    async fn try_sync_storage(&mut self) {
        match self.sync_storage().await {
            Ok(()) => self.storage_synced = true,
            Err(e) => tracing::warn!("storage sync failed, will retry on reconnect: {e:#}"),
        }
        self.refresh().await;
    }

    /// Rebuilds the conversation list from the store.
    async fn refresh(&mut self) {
        let names = self.names().await;
        let store = self.manager.store();
        let mut threads: Vec<(Thread, String, bool, Vec<Uuid>)> = Vec::new();
        if let Ok(groups) = store.groups().await {
            for (key, g) in groups.flatten() {
                self.group_ids.insert(group_id(&key), key);
                let members = g.members.iter().map(|m| ServiceId::from(m.aci).raw_uuid()).collect();
                threads.push((Thread::Group(key), g.title, true, members));
            }
        }
        if let Ok(contacts) = store.contacts().await {
            for c in contacts.flatten() {
                let title = names.get(&c.uuid).cloned().unwrap_or_else(|| c.uuid.to_string());
                threads.push((Thread::Contact(ServiceId::Aci(c.uuid.into())), title, false, Vec::new()));
            }
        }
        // People from the storage service who aren't in the address book.
        for key in &self.state.listed {
            let Some(id) = ServiceId::parse_from_service_id_string(key) else { continue };
            let thread = Thread::Contact(id);
            if !threads.iter().any(|(t, ..)| *t == thread) {
                let title = names.get(&id.raw_uuid()).cloned().unwrap_or_else(|| "Unknown".into());
                threads.push((thread, title, false, Vec::new()));
            }
        }
        let me = Thread::Contact(ServiceId::Aci(self.me.into()));
        threads.retain(|(t, ..)| *t != me); // titled "Note to Self" below, not by our own contact name
        threads.push((me.clone(), "Note to Self".into(), false, Vec::new()));

        let mut out = Vec::new();
        for (thread, title, is_group, members) in threads {
            let expire_timer = self.manager.store().expire_timer(&thread).await.ok().flatten().map_or(0, |(t, _)| t);
            let Some((last_ts, preview)) = self.last_message(&thread).await else {
                // Groups, Note to Self and chats the storage service lists show even when empty.
                if is_group || thread == me || self.state.listed.contains(&crate::state::thread_key(&thread)) {
                    out.push(Conversation { thread, title, last_ts: 0, preview: String::new(), unread: 0, is_group, members, expire_timer });
                }
                continue; // contacts without messages live in the "new chat" search
            };
            let unread = self.unread(&thread).await;
            out.push(Conversation { thread, title, last_ts, preview, unread, is_group, members, expire_timer });
        }
        out.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
        self.threads = out.iter().map(|c| (c.thread.clone(), c.title.clone())).collect();
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
                self.start_expiries(&thread, &timeline);
                self.emit(Event::Timeline { thread, timeline });
                self.refresh().await;
            }
            Command::Close => self.open = None,
            Command::Send { thread, ts, body, ranges, attachments, quote } => {
                let (attachments, error) = match self.send(&thread, ts, body, ranges, attachments, quote).await {
                    Ok(a) => (a, None),
                    Err(e) => (Vec::new(), Some(format!("{e:#}"))),
                };
                self.state.mark_read(&thread, ts);
                // presage put the thread's timer on the message; its countdown starts now.
                let timer = self.manager.store().expire_timer(&thread).await.ok().flatten().map_or(0, |(t, _)| t);
                if error.is_none() && timer > 0 {
                    self.state.start_expiry(&thread, self.me, ts, crate::state::expires_at(ts, timer));
                    self.emit_expiring(&thread);
                }
                self.emit(Event::Sent { thread, ts, attachments, error });
                self.refresh().await;
            }
            Command::SetTimer { thread, seconds } => {
                let ts = now_ms();
                let r = self.send_body(&thread, timer_update(seconds), ts).await;
                if self.report("change disappearing messages", r).is_some() {
                    // Show our notice; presage saved it and bumped the stored timer version.
                    if let Ok(Some(c)) = self.manager.store().message(&thread, ts).await {
                        self.emit(Event::Incoming { thread, content: Box::new(c) });
                    }
                }
            }
            Command::Edit { thread, target_ts, ts, body, ranges, attachments, quote } => {
                let edit = EditMessage {
                    target_sent_timestamp: Some(target_ts),
                    data_message: Some(data_message(body, ranges, attachments, quote)),
                };
                // presage stores the edit as its own row (keyed by `ts`); `Timeline` folds it onto the original.
                let error = self.send_body(&thread, edit, ts).await.err().map(|e| format!("{e:#}"));
                self.emit(Event::Sent { thread, ts: target_ts, attachments: Vec::new(), error });
                self.refresh().await;
            }
            Command::DeleteForEveryone { thread, target_ts } => {
                let dm = DataMessage {
                    delete: Some(Delete { target_sent_timestamp: Some(target_ts) }),
                    ..Default::default()
                };
                // presage's save_message turns our stored copy into the deleted tombstone.
                let r = self.send_body(&thread, dm, now_ms()).await;
                self.report("delete for everyone", r);
                self.refresh().await;
            }
            Command::DeleteForMe { thread, author, target_ts } => {
                let r = self.delete_for_me(&thread, author, target_ts).await;
                self.report("delete message", r);
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
                let path = self.fetch_attachment(&p).await.map_err(|e| format!("{e:#}"));
                self.emit(Event::Attachment { key, path });
            }
            Command::Search(query) => {
                // shortcut: scans every stored row per search; move to SQL (FTS) if it gets slow.
                let mut results = Vec::new();
                for (thread, title) in &self.threads {
                    if let Ok(rows) = self.manager.store().messages(thread, ..).await {
                        search_thread(self.me, &query, thread, title, rows.flatten(), &mut results);
                    }
                }
                self.emit(Event::SearchResults { query, results });
            }
        }
    }

    fn cache_path(&self, p: &AttachmentPointer) -> PathBuf {
        let ext = mime_guess::get_mime_extensions_str(p.content_type.as_deref().unwrap_or(""))
            .and_then(|e| e.first())
            .unwrap_or(&"bin");
        self.attachments_dir.join(format!("{}.{ext}", attachment_key(p)))
    }

    async fn fetch_attachment(&self, p: &AttachmentPointer) -> anyhow::Result<PathBuf> {
        let path = self.cache_path(p);
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
    ) -> anyhow::Result<Vec<AttachmentPointer>> {
        let mut specs = Vec::new();
        for path in &files {
            let data = tokio::fs::read(&path).await?;
            specs.push((
                AttachmentSpec {
                    content_type: mime_guess::from_path(path).first_or_octet_stream().to_string(),
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
        // Cache our own uploads so they display without downloading them back.
        for (path, pointer) in files.iter().zip(&attachments) {
            if let Err(e) = tokio::fs::copy(path, self.cache_path(pointer)).await {
                tracing::warn!(%e, "failed to cache sent attachment");
            }
        }
        let dm = data_message(body, ranges, attachments, quote);
        let sent = dm.attachments.clone();
        self.send_body(thread, dm, ts).await?;
        Ok(sent)
    }

    /// Deletes the message and its edits locally, then tells our other devices to do the same.
    async fn delete_for_me(&mut self, thread: &Thread, author: Uuid, ts: u64) -> anyhow::Result<()> {
        let mut store = self.manager.store().clone();
        let rows = store.messages(thread, ts..).await?;
        for row_ts in edit_rows(rows.flatten(), author, ts).into_iter().chain([ts]) {
            store.delete_message(thread, row_ts).await?;
        }
        let me = ServiceId::Aci(self.me.into());
        self.manager.send_message(me, delete_for_me_sync(thread, author, ts), now_ms()).await?;
        Ok(())
    }

    async fn send_body(&mut self, thread: &Thread, body: impl Into<ContentBody>, ts: u64) -> anyhow::Result<()> {
        let mut body = body.into();
        if let Some(dm) = data_message_mut(&mut body) {
            dm.timestamp = Some(ts);
        }
        match thread {
            Thread::Contact(id) => self.manager.send_message(*id, body, ts).await?,
            Thread::Group(key) => {
                if let Some(dm) = data_message_mut(&mut body) {
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

    fn emit_expiring(&self, thread: &Thread) {
        let expires = self.state.expiries(thread).map(|e| (e.author, e.ts, e.expires_at)).collect();
        self.emit(Event::Expiring { thread: thread.clone(), expires });
    }

    /// Starts the countdowns of disappearing messages now shown in the open conversation.
    fn start_expiries(&mut self, thread: &Thread, timeline: &Timeline) {
        let mut any = false;
        for (author, ts, at) in countdowns(timeline, now_ms()) {
            any |= self.state.start_expiry(thread, author, ts, at);
        }
        if any || self.state.expiries(thread).next().is_some() {
            self.emit_expiring(thread);
        }
    }

    /// Deletes messages whose countdown ran out (their reactions go with them: nothing to attach to).
    async fn expire(&mut self) {
        let due = self.state.take_due(now_ms());
        if due.is_empty() {
            return;
        }
        let mut by_thread: HashMap<String, Vec<(Uuid, u64)>> = HashMap::new();
        for e in due {
            by_thread.entry(e.thread).or_default().push((e.author, e.ts));
        }
        for (key, ids) in by_thread {
            let Some(thread) = crate::state::parse_thread_key(&key) else { continue };
            for (_, ts) in &ids {
                if let Err(e) = self.manager.store().clone().delete_message(&thread, *ts).await {
                    tracing::warn!(%e, "failed to delete expired message");
                }
            }
            self.emit(Event::Expired { thread, ids });
        }
        self.refresh().await;
    }

    async fn received(&mut self, r: Received) {
        match r {
            // QueueEmpty means we're connected again: retry a storage sync that failed offline.
            Received::QueueEmpty if !self.storage_synced => self.try_sync_storage().await,
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
        if self.open.as_ref() == Some(&thread) {
            let mut t = Timeline::new(self.me);
            t.apply(&c);
            self.start_expiries(&thread, &t);
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
    use crate::timeline::tests::{content, text, uuid};

    #[test]
    fn countdowns_start_on_read_for_incoming_and_on_send_for_outgoing() {
        let mut t = Timeline::new(uuid(1));
        t.apply(&content(uuid(2), 1_000, DataMessage { expire_timer: Some(30), ..text(1_000, "theirs") }));
        t.apply(&content(uuid(1), 2_000, DataMessage { expire_timer: Some(60), ..text(2_000, "mine") }));
        t.apply(&content(uuid(2), 3_000, text(3_000, "no timer")));
        t.apply(&content(uuid(2), 4_000, timer_update(30)));
        let c: Vec<_> = countdowns(&t, 10_000).collect();
        assert_eq!(c, [(uuid(2), 1_000, 40_000), (uuid(1), 2_000, 62_000)], "timer notices never expire");
    }

    #[test]
    fn timer_update_message() {
        let dm = timer_update(300);
        assert_eq!((dm.flags, dm.expire_timer), (Some(Flags::ExpirationTimerUpdate as u32), Some(300)));
        assert_eq!(dm.expire_timer_version, None, "presage fills in the bumped version");
        assert_eq!(timer_update(0).expire_timer, Some(0), "Off is an explicit 0, not 'keep the timer'");
    }

    #[test]
    fn group_id_is_deterministic_and_distinct() {
        assert_eq!(group_id(&[1; 32]), group_id(&[1; 32]));
        assert_ne!(group_id(&[1; 32]), group_id(&[2; 32]));
        assert_ne!(group_id(&[1; 32]), [1; 32]);
    }

    #[test]
    fn search_finds_messages_case_insensitively_newest_first() {
        use crate::timeline::tests::{content, text, uuid};
        use presage::proto::data_message::Delete;
        let me = uuid(1);
        let alice = Thread::Contact(ServiceId::Aci(uuid(2).into()));
        let group = Thread::Group([9; 32]);
        let delete = DataMessage { delete: Some(Delete { target_sent_timestamp: Some(25) }), ..Default::default() };
        let alice_rows = vec![
            content(uuid(2), 30, text(30, "say HELLO to everyone")),
            content(uuid(2), 26, delete),
            content(uuid(2), 25, text(25, "hello, deleted")),
            content(uuid(2), 20, text(20, "nothing here")),
            content(me, 10, text(10, "hello alice")),
        ];
        let long = format!("{} hello at the end", "word ".repeat(10));
        let group_rows = vec![content(uuid(3), 27, text(27, &long))];

        let mut results = Vec::new();
        search_thread(me, " hello ", &alice, "Alice", alice_rows, &mut results);
        search_thread(me, "hello", &group, "Friends", group_rows, &mut results);
        let got: Vec<_> = results.iter().map(|r| (r.title.as_str(), r.id, r.author)).collect();
        assert_eq!(got, [("Alice", 30, uuid(2)), ("Friends", 27, uuid(3)), ("Alice", 10, me)]);
        assert_eq!(results[1].thread, group);
        assert_eq!(results[0].snippet, "say HELLO to everyone");
        assert!(results[1].snippet.starts_with('…') && results[1].snippet.contains("hello at the end"), "{}", results[1].snippet);

        search_thread(me, "  ", &alice, "Alice", vec![content(uuid(2), 40, text(40, "x"))], &mut results);
        assert_eq!(results.len(), 3, "blank query matches nothing");
    }

    #[test]
    fn search_is_capped_to_the_newest() {
        use crate::timeline::tests::{content, text, uuid};
        let alice = Thread::Contact(ServiceId::Aci(uuid(2).into()));
        let rows: Vec<_> = (1..=150u64).rev().map(|i| content(uuid(2), i, text(i, "ping"))).collect();
        let mut results = Vec::new();
        search_thread(uuid(1), "ping", &alice, "Alice", rows, &mut results);
        assert_eq!(results.len(), SEARCH_LIMIT);
        assert_eq!((results[0].id, results[99].id), (150, 51));
    }

    #[test]
    fn timestamp_and_group_context_reach_into_edits() {
        let mut body: ContentBody =
            EditMessage { target_sent_timestamp: Some(10), data_message: Some(DataMessage::default()) }.into();
        data_message_mut(&mut body).unwrap().timestamp = Some(12);
        let ContentBody::EditMessage(e) = &body else { panic!() };
        assert_eq!(e.data_message.as_ref().unwrap().timestamp, Some(12));
        assert!(data_message_mut(&mut ContentBody::from(SyncMessage::default())).is_none());
    }

    #[test]
    fn edit_rows_find_own_and_synced_edits_of_the_target() {
        use crate::timeline::tests::{content, text, uuid};
        let edit = |target| EditMessage { target_sent_timestamp: Some(target), data_message: Some(text(0, "v2")) };
        let synced = SyncMessage {
            content: Some(SyncContent::Sent(sync_message::Sent { edit_message: Some(edit(10)), ..Default::default() })),
            ..Default::default()
        };
        let rows = vec![
            content(uuid(1), 10, text(10, "v1")),
            content(uuid(1), 12, edit(10)),
            content(uuid(1), 13, edit(11)),
            content(uuid(2), 14, edit(10)),
            content(uuid(1), 15, synced),
        ];
        assert_eq!(edit_rows(rows, uuid(1), 10), [12, 15]);
    }

    #[test]
    fn delete_for_me_sync_addresses_the_message() {
        let alice = Uuid::from_bytes([2; 16]);
        let sync = delete_for_me_sync(&Thread::Contact(ServiceId::Aci(alice.into())), alice, 10);
        let Some(SyncContent::DeleteForMe(d)) = sync.content else { panic!() };
        let m = &d.message_deletes[0];
        assert_eq!(
            m.conversation.as_ref().unwrap().identifier,
            Some(Identifier::ThreadServiceId(alice.to_string()))
        );
        assert_eq!(m.messages[0].author, Some(Author::AuthorServiceId(alice.to_string())));
        assert_eq!(m.messages[0].sent_timestamp, Some(10));
        let group = delete_for_me_sync(&Thread::Group([1; 32]), alice, 10);
        let Some(SyncContent::DeleteForMe(d)) = group.content else { panic!() };
        assert_eq!(
            d.message_deletes[0].conversation.as_ref().unwrap().identifier,
            Some(Identifier::ThreadGroupId(group_id(&[1; 32]).to_vec())),
            "groups are addressed by their public id, never the master key"
        );
    }

    #[test]
    fn attachment_keys() {
        let p = AttachmentPointer { digest: Some(vec![0xab, 0xcd]), ..Default::default() };
        assert_eq!(attachment_key(&p), "abcd");
        let q = AttachmentPointer { size: Some(3), ..Default::default() };
        assert!(attachment_key(&q).ends_with("-3"));
    }

    #[test]
    fn link_url_advertises_history_transfer() {
        let url = url::Url::parse("sgnl://linkdevice?uuid=a&pub_key=b%2Bc").unwrap();
        let out = url::Url::parse(&link_url(&url)).unwrap();
        let q: HashMap<_, _> = out.query_pairs().into_owned().collect();
        assert_eq!(q["capabilities"], "backup5");
        assert_eq!((q["uuid"].as_str(), q["pub_key"].as_str()), ("a", "b+c"), "existing params intact");
    }

    #[test]
    fn transfer_archive_responses() {
        let ready: TransferArchive = serde_json::from_str(r#"{"cdn":3,"key":"abc"}"#).unwrap();
        assert_eq!(ready, TransferArchive::Ready { cdn: 3, key: "abc".into() });
        let skipped: TransferArchive = serde_json::from_str(r#"{"error":"CONTINUE_WITHOUT_UPLOAD"}"#).unwrap();
        assert_eq!(skipped, TransferArchive::Skipped { error: "CONTINUE_WITHOUT_UPLOAD".into() });
        assert!(serde_json::from_str::<TransferArchive>("{}").is_err());
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
