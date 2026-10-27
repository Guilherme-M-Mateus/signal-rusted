//! The protocol side. presage's futures are `!Send`, so the `Manager` lives on its own
//! thread (current-thread runtime + `LocalSet`) and talks to the UI over channels,
//! much like Signal-Desktop's renderer talks to its SQL worker.

use std::collections::{HashMap, HashSet};
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
use presage::libsignal_service::groups_v2::GroupMemberCandidate;
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
use presage::proto::data_message::{Delete, Flags, Quote as ProtoQuote, Reaction, Sticker};
use presage::proto::sync_message::{
    self, Content as SyncContent, DeleteForMe, MessageRequestResponse, delete_for_me::MessageDeletes,
    message_request_response::Type as RequestResponse,
};
use presage::proto::{
    AddressableMessage, AttachmentPointer, BodyRange, ConversationIdentifier, DataMessage, EditMessage,
    GroupContextV2, ReceiptMessage, SyncMessage, TypingMessage, receipt_message, typing_message,
};
use presage::store::{ContentsStore, StateStore, StickerPack, Thread};
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tokio::sync::mpsc;

use crate::groups::{GroupEdit, GroupInfo};
use crate::state::{AppState, MAX_PINNED, thread_key};
use crate::stories::{Story, StoryReply, StoryStore};
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
        /// Marks the attachments as a voice note (Signal shows them as a voice message).
        voice_note: bool,
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
    /// Disappearing-message timer in seconds (0 = off); 1:1 chats (groups use `EditGroup`).
    SetTimer { thread: Thread, seconds: u32 },
    /// Creates a GroupsV2 group with us as admin; replies `Event::GroupCreated`.
    CreateGroup { title: String, members: Vec<Uuid> },
    EditGroup { thread: Thread, edit: GroupEdit },
    Search(String),
    Chat { thread: Thread, action: ChatAction },
    /// Re-reads the installed sticker packs (answered with `Event::Stickers`).
    LoadStickers,
    /// `sticker` has no `data`: the backend uploads the pack's stored image as it.
    SendSticker { thread: Thread, ts: u64, sticker: Sticker },
    InstallStickerPack { id: Vec<u8>, key: Vec<u8> },
    /// Marks a story viewed and sends the author a viewed receipt.
    StoryViewed { author: Uuid, ts: u64 },
    /// Answers a story privately (a 1:1 message with its `story_context`).
    StoryReply { author: Uuid, ts: u64, reply: StoryReply },
}

/// What the sidebar's "⋯" menu and the message-request bar do to a chat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChatAction {
    Archive(bool),
    Pin(bool),
    Mute(bool),
    MarkUnread(bool),
    Accept,
    Delete,
    Block,
}

/// An installed pack as the picker shows it.
#[derive(Clone, Debug)]
pub struct Pack {
    pub id: Vec<u8>,
    pub key: Vec<u8>,
    pub title: String,
    pub stickers: Vec<PackSticker>,
}

#[derive(Clone, Debug)]
pub struct PackSticker {
    pub id: u32,
    pub emoji: String,
    pub image: iced::widget::image::Handle,
}

/// Packs by title, keeping only stickers whose image is downloaded (and packs that have one).
pub fn picker_packs(packs: impl IntoIterator<Item = StickerPack>) -> Vec<Pack> {
    let mut out: Vec<Pack> = packs
        .into_iter()
        .map(|p| Pack {
            title: if p.manifest.title.trim().is_empty() { "Stickers".into() } else { p.manifest.title },
            stickers: p
                .manifest
                .stickers
                .into_iter()
                .filter_map(|s| {
                    let image = iced::widget::image::Handle::from_bytes(s.bytes?);
                    Some(PackSticker { id: s.id, emoji: s.emoji.unwrap_or_default(), image })
                })
                .collect(),
            id: p.id,
            key: p.key,
        })
        .filter(|p| !p.stickers.is_empty())
        .collect();
    out.sort_by_cached_key(|p| (p.title.to_lowercase(), p.id.clone()));
    out
}

/// A sticker message: Signal clients upload the sticker image and send it as `data`.
pub fn sticker_message(sticker: Sticker, data: AttachmentPointer) -> DataMessage {
    DataMessage { sticker: Some(Sticker { data: Some(data), ..sticker }), ..Default::default() }
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
    /// A message request: someone we never accepted.
    pub request: bool,
    pub pinned: bool,
    pub archived: bool,
    pub muted: bool,
    pub marked_unread: bool,
    /// Group details for the info panel; `None` for 1:1 chats.
    pub group: Option<GroupInfo>,
}

/// Pinned chats first (in pin order), then newest first.
pub fn sort_conversations(convs: &mut [Conversation], pinned: &[String]) {
    convs.sort_by_cached_key(|c| {
        let pin = pinned.iter().position(|k| *k == thread_key(&c.thread)).unwrap_or(usize::MAX);
        (pin, std::cmp::Reverse(c.last_ts))
    });
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
    /// A group we just created, to open.
    GroupCreated { thread: Thread, title: String },
    Stickers { packs: Vec<Pack>, recent: Vec<(Vec<u8>, u32)> },
    /// Every stored (unexpired) story.
    Stories(Vec<Story>),
    Error(String),
    /// Neutral status banner (e.g. history import in progress); `None` clears it.
    Notice(Option<String>),
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Attachment cache; also where voice notes are recorded before sending.
pub fn attachments_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("attachments")
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

/// Tells our other devices how we answered a message request.
fn message_request_sync(thread: &Thread, ty: RequestResponse) -> SyncMessage {
    let (aci, group) = match thread {
        Thread::Contact(id) => (Some(id.raw_uuid()), None),
        Thread::Group(key) => (None, Some(group_id(key).to_vec())),
    };
    SyncMessage {
        content: Some(SyncContent::MessageRequestResponse(MessageRequestResponse {
            thread_aci: aci.map(|a| a.to_string()),
            thread_aci_binary: aci.map(|a| a.as_bytes().to_vec()),
            group_id: group,
            r#type: Some(ty as i32),
        })),
        ..Default::default()
    }
}

/// Our whole block list, as Signal syncs it to our devices.
fn blocked_sync(state: &AppState) -> SyncMessage {
    let mut b = sync_message::Blocked::default();
    for key in state.flags.iter().filter(|(_, f)| f.blocked).map(|(k, _)| k) {
        match crate::state::parse_thread_key(key) {
            Some(Thread::Contact(id @ ServiceId::Aci(_))) => {
                b.acis.push(id.raw_uuid().to_string());
                b.acis_binary.push(id.raw_uuid().as_bytes().to_vec());
            }
            Some(Thread::Group(k)) => b.group_ids.push(group_id(&k).to_vec()),
            _ => {}
        }
    }
    SyncMessage { content: Some(SyncContent::Blocked(b)), ..Default::default() }
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

/// Whether any stored row in a chat was sent by us (our messages, reactions or edits).
pub fn wrote_in(rows: impl IntoIterator<Item = Content>, me: Uuid) -> bool {
    rows.into_iter().any(|c| c.metadata.sender.raw_uuid() == me)
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
async fn import_history(mut manager: Mgr, data_dir: PathBuf, key: [u8; 32], me: Uuid) -> anyhow::Result<Vec<(Thread, u64)>> {
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
    // The backup lists the phone's installed packs (there's no other way to sync existing ones).
    for (id, key) in &imported.sticker_packs {
        if let Err(e) = manager.install_sticker_pack(id, key).await {
            tracing::warn!(%e, "failed to install sticker pack from history");
        }
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
    std::fs::create_dir_all(attachments_dir(&data_dir))?;
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
        stories: StoryStore::load(&data_dir.join("stories.json")),
        attachments_dir: attachments_dir(&data_dir),
        open: None,
        group_ids: HashMap::new(),
        storage_synced: false,
        threads: Vec::new(),
    };
    b.emit(Event::Ready { me: b.me });
    b.emit_stickers().await;
    b.expire_stories();
    b.emit(Event::Stories(b.stories.stories.clone()));
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
        let next = b.state.next_expiry().into_iter().chain(b.stories.next_expiry()).min().map(|at| Duration::from_millis(at.saturating_sub(now_ms())).min(Duration::from_secs(60)));
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
    stories: StoryStore,
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
        self.state.set_storage(plan.listed, plan.names, plan.flags, plan.pinned);
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
        // Address-book contacts (from contact sync) aren't message requests; presage also saves
        // a contact "on first sight" of a stranger, but without a phone number.
        let mut address_book = HashSet::new();
        let mut threads: Vec<(Thread, String, Option<GroupInfo>, Vec<Uuid>)> = Vec::new();
        if let Ok(groups) = store.groups().await {
            for (key, g) in groups.flatten() {
                self.group_ids.insert(group_id(&key), key);
                let members = g.members.iter().map(|m| ServiceId::from(m.aci).raw_uuid()).collect();
                let info = GroupInfo::from_group(&g);
                threads.push((Thread::Group(key), g.title, Some(info), members));
            }
        }
        if let Ok(contacts) = store.contacts().await {
            for c in contacts.flatten() {
                if c.phone_number.is_some() {
                    address_book.insert(c.uuid);
                }
                let title = names.get(&c.uuid).cloned().unwrap_or_else(|| c.uuid.to_string());
                threads.push((Thread::Contact(ServiceId::Aci(c.uuid.into())), title, None, Vec::new()));
            }
        }
        // People from the storage service who aren't in the address book.
        for key in &self.state.listed {
            let Some(id) = ServiceId::parse_from_service_id_string(key) else { continue };
            let thread = Thread::Contact(id);
            if !threads.iter().any(|(t, ..)| *t == thread) {
                let title = names.get(&id.raw_uuid()).cloned().unwrap_or_else(|| "Unknown".into());
                threads.push((thread, title, None, Vec::new()));
            }
        }
        let me = Thread::Contact(ServiceId::Aci(self.me.into()));
        threads.retain(|(t, ..)| *t != me); // titled "Note to Self" below, not by our own contact name
        threads.push((me.clone(), "Note to Self".into(), None, Vec::new()));

        let mut out = Vec::new();
        let now = now_ms();
        for (thread, title, group, members) in threads {
            let is_group = group.is_some();
            let f = self.state.flags(&thread);
            if f.blocked {
                continue;
            }
            let expire_timer = self.manager.store().expire_timer(&thread).await.ok().flatten().map_or(0, |(t, _)| t);
            let (last_ts, preview, unread) = match self.last_message(&thread).await {
                Some((ts, preview)) => (ts, preview, self.unread(&thread).await),
                // Groups, Note to Self and chats the storage service lists show even when empty.
                None if is_group || thread == me || self.state.listed.contains(&thread_key(&thread)) => (0, String::new(), 0),
                None => continue, // contacts without messages live in the "new chat" search
            };
            let in_contacts = matches!(&thread, Thread::Contact(id) if address_book.contains(&id.raw_uuid()));
            let request = self.state.is_request(&thread, self.me, in_contacts) && !self.accept_if_answered(&thread).await;
            out.push(Conversation {
                request,
                pinned: self.state.is_pinned(&thread),
                archived: f.archived,
                muted: f.muted_until > now,
                marked_unread: f.marked_unread,
                thread,
                title,
                last_ts,
                preview,
                unread,
                is_group,
                members,
                expire_timer,
                group,
            });
        }
        sort_conversations(&mut out, &self.state.pinned);
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

    /// `AppState::is_request`, looking the sender up in the address book.
    async fn is_request(&mut self, thread: &Thread) -> bool {
        let in_contacts = match thread {
            Thread::Contact(id) => {
                matches!(self.manager.store().contact_by_id(id).await, Ok(Some(c)) if c.phone_number.is_some())
            }
            Thread::Group(_) => false,
        };
        self.state.is_request(thread, self.me, in_contacts) && !self.accept_if_answered(thread).await
    }

    /// Signal treats a chat you've written in as accepted; imported history can contain such
    /// chats from people outside the address book. Checked only for would-be requests.
    async fn accept_if_answered(&mut self, thread: &Thread) -> bool {
        let Ok(rows) = self.manager.store().messages(thread, ..).await else { return false };
        let answered = wrote_in(rows.flatten(), self.me);
        if answered {
            self.state.accept(thread);
        }
        answered
    }

    async fn sync_to_self(&mut self, sync: SyncMessage) -> anyhow::Result<()> {
        let me = ServiceId::Aci(self.me.into());
        self.manager.send_message(me, sync, now_ms()).await?;
        Ok(())
    }

    async fn chat_action(&mut self, thread: &Thread, action: ChatAction) -> anyhow::Result<()> {
        match action {
            ChatAction::Archive(on) => self.state.update_flags(thread, |f| f.archived = on),
            ChatAction::Mute(on) => self.state.update_flags(thread, |f| f.muted_until = if on { u64::MAX } else { 0 }),
            ChatAction::MarkUnread(on) => self.state.update_flags(thread, |f| f.marked_unread = on),
            ChatAction::Pin(on) => anyhow::ensure!(self.state.set_pinned(thread, on), "you can only pin up to {MAX_PINNED} chats"),
            ChatAction::Accept => {
                self.state.accept(thread);
                self.sync_to_self(message_request_sync(thread, RequestResponse::Accept)).await?;
                if let Thread::Contact(id) = thread {
                    // Shares our profile key (presage adds it to every DataMessage), as Signal does on accept.
                    let update = DataMessage { flags: Some(Flags::ProfileKeyUpdate as u32), ..Default::default() };
                    self.manager.send_message(*id, update, now_ms()).await?;
                }
            }
            ChatAction::Delete => {
                self.manager.store().clone().clear_thread(thread).await?;
                self.sync_to_self(message_request_sync(thread, RequestResponse::Delete)).await?;
            }
            ChatAction::Block => {
                self.state.update_flags(thread, |f| f.blocked = true);
                self.sync_to_self(message_request_sync(thread, RequestResponse::Block)).await?;
                self.sync_to_self(blocked_sync(&self.state)).await?;
            }
        }
        Ok(())
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
                if self.state.flags(&thread).marked_unread {
                    self.state.update_flags(&thread, |f| f.marked_unread = false);
                }
                self.mark_read(&thread, &timeline).await;
                self.start_expiries(&thread, &timeline);
                self.emit(Event::Timeline { thread, timeline });
                self.refresh().await;
            }
            Command::Close => self.open = None,
            Command::Send { thread, ts, body, ranges, attachments, quote, voice_note } => {
                let r = self.send(&thread, ts, body, ranges, attachments, quote, voice_note).await;
                self.sent(thread, ts, r).await;
            }
            Command::SendSticker { thread, ts, sticker } => {
                let r = self.send_sticker(&thread, ts, sticker).await.map(|p| vec![p]);
                self.sent(thread, ts, r).await;
            }
            Command::LoadStickers => self.emit_stickers().await,
            Command::InstallStickerPack { id, key } => {
                // shortcut: downloads the whole pack on the protocol task (a few seconds); spawn it if that hurts.
                let r = self.manager.install_sticker_pack(&id, &key).await;
                if self.report("install sticker pack", r).is_some() {
                    self.emit_stickers().await;
                }
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
            Command::CreateGroup { title, members } => {
                let r = self.create_group(&title, &members).await;
                if let Some(key) = self.report("create group", r) {
                    self.refresh().await;
                    self.emit(Event::GroupCreated { thread: Thread::Group(key), title });
                }
            }
            Command::EditGroup { thread: Thread::Group(key), edit } => {
                let ts = now_ms();
                let r = self.edit_group(&key, &edit, ts).await;
                if self.report("update group", r).is_some() {
                    let thread = Thread::Group(key);
                    if let Ok(Some(c)) = self.manager.store().message(&thread, ts).await {
                        self.emit(Event::Incoming { thread, content: Box::new(c) });
                    }
                }
                self.refresh().await;
            }
            Command::EditGroup { .. } => {}
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
            Command::Typing { thread, .. } if self.is_request(&thread).await => {} // Signal doesn't reveal typing to requests
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
            Command::StoryViewed { author, ts } => {
                if self.stories.mark_viewed(author, ts) {
                    // Signal's "View receipts" for stories default to on.
                    let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Viewed as i32), timestamp: vec![ts] };
                    if let Err(e) = self.manager.send_message(ServiceId::Aci(author.into()), receipt, now_ms()).await {
                        tracing::warn!(%e, "story viewed receipt failed");
                    }
                }
            }
            Command::StoryReply { author, ts, reply } => {
                let thread = Thread::Contact(ServiceId::Aci(author.into()));
                let r = match self.stories.get(author, ts) {
                    Some(s) if s.allows_replies() => {
                        let sent = now_ms();
                        self.send_body(&thread, crate::stories::reply_message(author, ts, reply), sent).await.map(|()| sent)
                    }
                    _ => Err(anyhow::anyhow!("this story doesn't take replies")),
                };
                if let Some(sent) = self.report("reply to story", r) {
                    if let Ok(Some(c)) = self.manager.store().message(&thread, sent).await {
                        self.emit(Event::Incoming { thread, content: Box::new(c) });
                    }
                    self.refresh().await;
                }
            }
            Command::Chat { thread, action } => {
                let r = self.chat_action(&thread, action).await;
                self.report("update chat", r);
                self.refresh().await;
            }
        }
    }

    async fn sent(&mut self, thread: Thread, ts: u64, r: anyhow::Result<Vec<AttachmentPointer>>) {
        let (attachments, error) = match r {
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

    async fn emit_stickers(&self) {
        let packs = self.manager.store().sticker_packs().await;
        if let Some(packs) = self.report("load sticker packs", packs) {
            let recent = self.state.recent_stickers.clone();
            self.emit(Event::Stickers { packs: picker_packs(packs.flatten()), recent });
        }
    }

    /// Uploads the installed pack's image as the sticker's attachment and sends it.
    async fn send_sticker(&mut self, thread: &Thread, ts: u64, sticker: Sticker) -> anyhow::Result<AttachmentPointer> {
        use anyhow::Context;
        let pack_id = sticker.pack_id().to_vec();
        let meta = self.manager.sticker_metadata(&pack_id, sticker.sticker_id()).await?.context("sticker pack not installed")?;
        let data = meta.bytes.context("sticker not downloaded yet")?;
        let spec = AttachmentSpec {
            content_type: meta.content_type.filter(|c| !c.is_empty()).unwrap_or_else(|| "image/webp".into()),
            length: data.len(),
            file_name: None,
            preview: None,
            voice_note: None,
            borderless: None,
            width: None,
            height: None,
            caption: None,
            blur_hash: None,
        };
        let pointer = self.manager.upload_attachments(vec![(spec, data.clone())]).await?.into_iter().next().context("no upload result")??;
        if let Err(e) = tokio::fs::write(self.cache_path(&pointer), &data).await {
            tracing::warn!(%e, "failed to cache sent sticker");
        }
        self.send_body(thread, sticker_message(sticker.clone(), pointer.clone()), ts).await?;
        self.state.sent_sticker((pack_id, sticker.sticker_id()));
        Ok(pointer)
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
        voice_note: bool,
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
                    voice_note: voice_note.then_some(true),
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

    /// Unverified against the live servers (tests can't reach them): uploads the group with
    /// us as admin, contacts with a known profile key as members and the rest as invitees,
    /// then sends the first group update so members see it.
    async fn create_group(&mut self, title: &str, members: &[Uuid]) -> anyhow::Result<[u8; 32]> {
        let title = title.trim();
        anyhow::ensure!(!title.is_empty(), "the group needs a name");
        let key: [u8; 32] = rand::random();
        let me = self.manager.profile_key_credential(self.me.into()).await?;
        let me = me.ok_or_else(|| anyhow::anyhow!("our profile key is unknown"))?;
        let candidates = self.candidates(members).await?;
        let params = self.manager.server_public_params();
        let group = crate::groups::operations(&key).encrypt_group_with_credentials(
            title,
            None,
            None,
            Some(&crate::groups::new_group_access()),
            &me,
            &candidates,
            &params,
            String::new(),
            &mut rand::rng(),
        )?;
        self.manager.create_group(&key, group).await?;
        self.send_body(&Thread::Group(key), DataMessage::default(), now_ms()).await?;
        Ok(key)
    }

    /// Profile key credentials, where we know the profile key (others get invited).
    async fn candidates(&self, members: &[Uuid]) -> anyhow::Result<Vec<GroupMemberCandidate>> {
        let mut out = Vec::new();
        for id in members.iter().filter(|id| **id != self.me) {
            let credential = self.manager.profile_key_credential((*id).into()).await?;
            out.push(GroupMemberCandidate { service_id: ServiceId::Aci((*id).into()), credential });
        }
        Ok(out)
    }

    /// Unverified against the live servers: applies the change, then sends the signed change
    /// to the (new) members so their clients apply it too. Members removed by the change
    /// aren't told; their clients notice on the next group fetch.
    async fn edit_group(&mut self, key: &[u8; 32], edit: &GroupEdit, ts: u64) -> anyhow::Result<()> {
        let group = self.manager.store().group(*key).await?.ok_or_else(|| anyhow::anyhow!("unknown group"))?;
        anyhow::ensure!(GroupInfo::from_group(&group).allows_edit(self.me, edit), "not allowed in this group");
        let adds = match edit {
            GroupEdit::AddMembers(ids) => self.candidates(ids).await?,
            _ => Vec::new(),
        };
        let params = self.manager.server_public_params();
        let actions = crate::groups::change_actions(&crate::groups::operations(key), self.me, edit, &adds, &params)?;
        let (revision, change) = self.manager.modify_group(key, actions).await?;
        let dm = DataMessage {
            timestamp: Some(ts),
            group_v2: Some(GroupContextV2 { master_key: Some(key.to_vec()), revision: Some(revision), group_change: Some(change) }),
            ..Default::default()
        };
        self.manager.send_message_to_group(key, dm, ts).await?;
        if *edit == GroupEdit::Leave {
            // We can't fetch the group anymore: keep it, listed without us.
            if let Some(mut g) = self.manager.store().group(*key).await? {
                g.members.retain(|m| ServiceId::from(m.aci).raw_uuid() != self.me);
                g.revision = revision;
                self.manager.store().save_group(*key, g).await?;
            }
        }
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
        let request = self.is_request(thread).await;
        let mut by_author: HashMap<Uuid, Vec<u64>> = HashMap::new();
        for (a, ts) in &unread {
            by_author.entry(*a).or_default().push(*ts);
        }
        // Read receipts would tell a stranger we saw their request.
        for (author, timestamp) in by_author.into_iter().filter(|_| !request) {
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
        if self.expire_stories() {
            self.emit(Event::Stories(self.stories.stories.clone()));
        }
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

    /// Drops stories past Signal's 24 hours, and their downloaded media.
    fn expire_stories(&mut self) -> bool {
        let gone = self.stories.take_expired(now_ms());
        for p in gone.iter().filter_map(Story::file) {
            let _ = std::fs::remove_file(self.cache_path(p));
        }
        !gone.is_empty()
    }

    /// Keeps a story from someone we talk to, or from one of our groups.
    async fn story(&mut self, author: Uuid, ts: u64, msg: presage::proto::StoryMessage) {
        let story = Story::new(author, ts, msg);
        let owner = story.owner();
        let accepted = match &owner {
            Thread::Group(k) => matches!(self.manager.store().group(*k).await, Ok(Some(_))),
            Thread::Contact(_) => !self.is_request(&owner).await,
        };
        if author == self.me || !accepted || self.state.flags(&owner).blocked {
            return;
        }
        if self.stories.add(story, now_ms()) {
            self.emit(Event::Stories(self.stories.stories.clone()));
        }
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
        let ts = c.metadata.client_timestamp.timestamp_millis() as u64;
        if sender != self.me && self.state.flags(&Thread::Contact(c.metadata.sender)).blocked {
            // Dropped silently; presage already stored it, so forget it again.
            if let Ok(thread) = Thread::try_from(&c) {
                let _ = self.manager.store().clone().delete_message(&thread, ts).await;
            }
            return;
        }
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
            ContentBody::StoryMessage(s) => return self.story(sender, ts, s.clone()).await,
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
            ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::MessageRequestResponse(r)), .. })
                if sender == self.me =>
            {
                // Answered on another device.
                let aci = r.thread_aci_binary.as_deref().and_then(|b| Uuid::from_slice(b).ok());
                let aci = aci.or_else(|| r.thread_aci.as_deref().and_then(|s| Uuid::parse_str(s).ok()));
                let Some(thread) = aci.map(|a| Thread::Contact(ServiceId::Aci(a.into()))) else { return };
                match r.r#type.and_then(|t| RequestResponse::try_from(t).ok()) {
                    Some(RequestResponse::Accept) => self.state.accept(&thread),
                    Some(RequestResponse::Delete) => drop(self.manager.store().clone().clear_thread(&thread).await),
                    Some(RequestResponse::Block | RequestResponse::BlockAndDelete | RequestResponse::BlockAndSpam) => {
                        self.state.update_flags(&thread, |f| f.blocked = true)
                    }
                    _ => {}
                }
                self.refresh().await;
                return;
            }
            _ => {}
        }
        let Ok(thread) = Thread::try_from(&c) else { return };
        if sender != self.me && self.state.flags(&thread).blocked {
            let _ = self.manager.store().clone().delete_message(&thread, ts).await; // a blocked group
            return;
        }

        let is_visible_incoming = sender != self.me
            && matches!(&c.body, ContentBody::DataMessage(dm)
                if (dm.reaction.is_none() || dm.story_context.is_some()) && (dm.body.is_some() || dm.reaction.is_some() || !dm.attachments.is_empty() || dm.sticker.is_some()));
        if is_visible_incoming {
            // Delivery receipt, as every Signal client does, except to message requests.
            if !self.is_request(&thread).await {
                let ts = match &c.body {
                    ContentBody::DataMessage(dm) => dm.timestamp.unwrap_or(0),
                    _ => 0,
                };
                let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Delivery as i32), timestamp: vec![ts] };
                if let Err(e) = self.manager.send_message(c.metadata.sender, receipt, now_ms()).await {
                    tracing::debug!(%e, "delivery receipt failed");
                }
            }
            // A new message brings an archived chat back, unless it's muted (Signal's rule).
            if self.state.flags(&thread).archived && !self.state.is_muted(&thread, now_ms()) {
                self.state.update_flags(&thread, |f| f.archived = false);
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
        if self.state.is_muted(thread, now_ms()) {
            return;
        }
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

    fn pack(id: u8, title: &str, stickers: &[(u32, bool)]) -> StickerPack {
        use presage::store::{Sticker as StoredSticker, StickerPackManifest};
        StickerPack {
            id: vec![id],
            key: vec![id; 32],
            manifest: StickerPackManifest {
                title: title.into(),
                author: String::new(),
                cover: None,
                stickers: stickers
                    .iter()
                    .map(|(sid, downloaded)| StoredSticker {
                        id: *sid,
                        emoji: Some("😀".into()),
                        content_type: None,
                        bytes: downloaded.then(|| vec![0u8; 4]),
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn picker_packs_sort_by_title_and_skip_undownloaded_stickers() {
        let packs = picker_packs([
            pack(1, "zebra", &[(0, true)]),
            pack(2, "", &[(0, true), (1, false), (2, true)]),
            pack(3, "Apes", &[(0, true)]),
            pack(4, "Empty", &[(0, false)]),
        ]);
        let shape: Vec<_> = packs.iter().map(|p| (p.title.as_str(), p.stickers.iter().map(|s| s.id).collect::<Vec<_>>())).collect();
        assert_eq!(shape, [("Apes", vec![0]), ("Stickers", vec![0, 2]), ("zebra", vec![0])]);
        assert_eq!(packs[0].key, vec![3; 32]);
    }

    #[test]
    fn sticker_message_carries_pack_and_uploaded_image() {
        let s = Sticker { pack_id: Some(vec![1]), pack_key: Some(vec![2]), sticker_id: Some(7), emoji: Some("🐱".into()), data: None };
        let p = AttachmentPointer { cdn_number: Some(3), content_type: Some("image/webp".into()), ..Default::default() };
        let dm = sticker_message(s, p.clone());
        let got = dm.sticker.unwrap();
        assert_eq!((got.pack_id(), got.pack_key(), got.sticker_id(), got.emoji()), (&[1u8][..], &[2u8][..], 7, "🐱"));
        assert_eq!(got.data, Some(p));
        assert!(dm.body.is_none() && dm.attachments.is_empty(), "the image travels only as the sticker's data");
    }

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

    fn conv(n: u8, last_ts: u64) -> Conversation {
        Conversation {
            thread: Thread::Contact(ServiceId::Aci(uuid(n).into())),
            title: String::new(),
            last_ts,
            preview: String::new(),
            unread: 0,
            is_group: false,
            expire_timer: 0,
            members: Vec::new(),
            request: false,
            pinned: false,
            archived: false,
            muted: false,
            marked_unread: false,
            group: None,
        }
    }

    #[test]
    fn pinned_chats_sort_first_in_pin_order() {
        let mut c = vec![conv(1, 10), conv(2, 30), conv(3, 20), conv(4, 5)];
        let pinned = [thread_key(&conv(4, 0).thread), thread_key(&conv(3, 0).thread)];
        sort_conversations(&mut c, &pinned);
        let order: Vec<u64> = c.iter().map(|c| c.last_ts).collect();
        assert_eq!(order, [5, 20, 30, 10]);
    }

    #[test]
    fn request_and_block_syncs() {
        let alice = Thread::Contact(ServiceId::Aci(uuid(2).into()));
        let Some(SyncContent::MessageRequestResponse(r)) = message_request_sync(&alice, RequestResponse::Accept).content else { panic!() };
        assert_eq!((r.thread_aci, r.thread_aci_binary, r.r#type), (Some(uuid(2).to_string()), Some(uuid(2).as_bytes().to_vec()), Some(1)));
        let Some(SyncContent::MessageRequestResponse(r)) = message_request_sync(&Thread::Group([1; 32]), RequestResponse::Block).content else { panic!() };
        assert_eq!(r.group_id, Some(group_id(&[1; 32]).to_vec()), "public group id, never the master key");

        let dir = tempfile::tempdir().unwrap();
        let mut state = AppState::load(&dir.path().join("state.json"));
        state.update_flags(&alice, |f| f.blocked = true);
        state.update_flags(&Thread::Group([1; 32]), |f| f.blocked = true);
        state.update_flags(&Thread::Contact(ServiceId::Aci(uuid(3).into())), |f| f.archived = true);
        let Some(SyncContent::Blocked(b)) = blocked_sync(&state).content else { panic!() };
        assert_eq!(b.acis, [uuid(2).to_string()]);
        assert_eq!(b.group_ids, [group_id(&[1; 32]).to_vec()]);
    }

    #[test]
    fn attachment_keys() {
        let p = AttachmentPointer { digest: Some(vec![0xab, 0xcd]), ..Default::default() };
        assert_eq!(attachment_key(&p), "abcd");
        let q = AttachmentPointer { size: Some(3), ..Default::default() };
        assert!(attachment_key(&q).ends_with("-3"));
    }

    #[test]
    fn a_chat_we_wrote_in_is_answered() {
        use crate::timeline::tests::{content, text, uuid};
        assert!(!wrote_in(vec![content(uuid(2), 1, text(1, "hi"))], uuid(1)), "only their messages");
        assert!(wrote_in(vec![content(uuid(2), 1, text(1, "hi")), content(uuid(1), 2, text(2, "yo"))], uuid(1)));
        assert!(!wrote_in(Vec::new(), uuid(1)));
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
