//! UI state and update logic (Elm architecture). Rendering lives in `view.rs`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::widget::{qr_code, text_editor};
use iced::{Subscription, Task, Theme};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::store::Thread;
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::{self, Command, Conversation, Event, QuoteRef, SearchResult, attachment_key, now_ms};
use crate::timeline::{Delivery, Message as Msg, Timeline};
use crate::{emoji, richtext};

/// Messages rendered at once; "Load older" extends it (keeps layout cheap on long threads).
pub const PAGE: usize = 150;
/// Images up to this size are downloaded automatically, like Signal's auto-download.
const AUTO_DOWNLOAD_BYTES: u32 = 25 * 1024 * 1024;
const TYPING_TIMEOUT: Duration = Duration::from_secs(15);
/// Signal's disappearing-message choices: (seconds, label).
pub const TIMERS: [(u32, &str); 8] = [
    (0, "Off"),
    (30, "30s"),
    (300, "5m"),
    (3_600, "1h"),
    (28_800, "8h"),
    (86_400, "1d"),
    (604_800, "1w"),
    (2_419_200, "4w"),
];
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

pub enum Screen {
    Connecting,
    Link { device_name: String, qr: Option<qr_code::Data>, waiting: bool, error: Option<String> },
    Main,
}

pub enum Attach {
    Loading,
    Ready(PathBuf),
    Failed(String),
}

pub struct Open {
    pub thread: Thread,
    pub title: String,
    pub is_group: bool,
    pub timeline: Option<Timeline>,
    pub shown: usize,
    pub composer: text_editor::Content,
    pub quote: Option<QuoteRef>,
    pub files: Vec<PathBuf>,
    pub hovered: Option<(Uuid, u64)>,
    pub reacting: Option<(Uuid, u64)>,
    pub picker: Option<Picker>,
    pub spoilers: HashSet<(u64, usize)>,
    /// (author, sent ts) -> when it disappears (ms).
    pub expires: HashMap<(Uuid, u64), u64>,
    pub timer_menu: bool,
    /// Members picked from the @-autocomplete: (inserted name, ACI).
    pub mentions: Vec<(String, Uuid)>,
    last_keystroke: Option<Instant>,
    typing_sent: bool,
}

pub struct Picker {
    pub query: String,
    pub group: emojis::Group,
}

pub struct App {
    pub cmd: Option<UnboundedSender<Command>>,
    pub screen: Screen,
    pub me: Uuid,
    pub names: HashMap<Uuid, String>,
    pub conversations: Vec<Conversation>,
    pub search: String,
    /// Message search results for `search`, newest first.
    pub results: Vec<SearchResult>,
    pub open: Option<Open>,
    pub dark: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub attachments: HashMap<String, Attach>,
    pub typing: HashMap<Thread, HashMap<Uuid, Instant>>,
    data_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum Message {
    Backend(Event),
    DeviceName(String),
    Link,
    Search(String),
    /// Debounced: searches messages if the query is still current.
    RunSearch(String),
    OpenThread(Thread, String, bool),
    Compose(text_editor::Action),
    Send,
    Attach,
    Picked(Vec<PathBuf>),
    RemoveFile(usize),
    Reply(Uuid, u64),
    CancelReply,
    Hover(Option<(Uuid, u64)>),
    ToggleReactions(Option<(Uuid, u64)>),
    React(Uuid, u64, String),
    TogglePicker,
    PickerQuery(String),
    PickerGroup(emojis::Group),
    InsertEmoji(&'static str),
    Mention(Uuid),
    Spoiler(u64, usize),
    LoadOlder,
    ToggleTimerMenu,
    SetTimer(u32),
    FetchAttachment(String),
    SaveAttachment(String),
    Saved(Result<(), String>),
    Theme(iced::theme::Mode),
    Tick,
    DismissError,
}

pub fn display_name(names: &HashMap<Uuid, String>, id: Uuid) -> String {
    names.get(&id).cloned().unwrap_or_else(|| {
        let s = id.to_string();
        format!("User {}", &s[..8])
    })
}

impl App {
    pub fn new(data_dir: PathBuf) -> (Self, Task<Message>) {
        let app = App {
            cmd: None,
            screen: Screen::Connecting,
            me: Uuid::nil(),
            names: HashMap::new(),
            conversations: Vec::new(),
            search: String::new(),
            results: Vec::new(),
            open: None,
            dark: true,
            error: None,
            notice: None,
            attachments: HashMap::new(),
            typing: HashMap::new(),
            data_dir,
        };
        (app, iced::system::theme().map(Message::Theme))
    }

    pub fn title(&self) -> String {
        let unread: usize = self.conversations.iter().map(|c| c.unread).sum();
        match unread {
            0 => "Signal".into(),
            n => format!("Signal ({n})"),
        }
    }

    pub fn theme(&self) -> Theme {
        crate::view::signal_theme(self.dark)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            Subscription::run_with(self.data_dir.clone(), |d| backend::run(d.clone())).map(Message::Backend),
            iced::system::theme_changes().map(Message::Theme),
        ];
        let typing_active = self.typing.values().any(|m| !m.is_empty());
        if typing_active || self.open.as_ref().is_some_and(|o| o.typing_sent || !o.expires.is_empty()) {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn send(&self, c: Command) {
        if let Some(tx) = &self.cmd {
            let _ = tx.send(c);
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Backend(e) => return self.backend(e),
            Message::DeviceName(n) => {
                if let Screen::Link { device_name, .. } = &mut self.screen {
                    *device_name = n;
                }
            }
            Message::Link => {
                if let Screen::Link { device_name, waiting, error, .. } = &mut self.screen {
                    let name = device_name.trim();
                    if name.is_empty() || *waiting {
                        return Task::none();
                    }
                    *waiting = true;
                    *error = None;
                    let device_name = name.to_owned();
                    self.send(Command::Link { device_name });
                }
            }
            Message::Search(s) => {
                self.search = s;
                if self.search.trim().chars().count() < 2 {
                    self.results.clear();
                    return Task::none();
                }
                let q = self.search.clone();
                return Task::perform(
                    async move {
                        tokio::time::sleep(SEARCH_DEBOUNCE).await;
                        q
                    },
                    Message::RunSearch,
                );
            }
            Message::RunSearch(q) => {
                if q == self.search {
                    self.send(Command::Search(q.trim().to_owned()));
                }
            }
            Message::OpenThread(thread, title, is_group) => {
                if self.open.as_ref().is_some_and(|o| o.thread == thread) {
                    return Task::none();
                }
                self.stop_typing();
                self.send(Command::Open(thread.clone()));
                self.open = Some(Open {
                    thread,
                    title,
                    is_group,
                    timeline: None,
                    shown: PAGE,
                    composer: text_editor::Content::new(),
                    quote: None,
                    files: Vec::new(),
                    hovered: None,
                    reacting: None,
                    picker: None,
                    spoilers: HashSet::new(),
                    expires: HashMap::new(),
                    timer_menu: false,
                    mentions: Vec::new(),
                    last_keystroke: None,
                    typing_sent: false,
                });
                self.search.clear();
                self.results.clear();
                return iced::widget::operation::focus(crate::view::COMPOSER);
            }
            Message::Compose(action) => {
                let Some(o) = &mut self.open else { return Task::none() };
                let edit = action.is_edit();
                o.composer.perform(action);
                if edit {
                    o.last_keystroke = Some(Instant::now());
                    if !o.typing_sent && !o.composer.text().trim().is_empty() {
                        o.typing_sent = true;
                        let thread = o.thread.clone();
                        self.send(Command::Typing { thread, started: true });
                    }
                }
            }
            Message::Send => self.send_message(),
            Message::Attach => {
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title("Attach files")
                            .pick_files()
                            .await
                            .map(|f| f.into_iter().map(|f| f.path().to_owned()).collect())
                            .unwrap_or_default()
                    },
                    Message::Picked,
                );
            }
            Message::Picked(files) => {
                if let Some(o) = &mut self.open {
                    o.files.extend(files);
                }
            }
            Message::RemoveFile(i) => {
                if let Some(o) = self.open.as_mut().filter(|o| i < o.files.len()) {
                    o.files.remove(i);
                }
            }
            Message::Reply(author, id) => {
                let Some(o) = &mut self.open else { return Task::none() };
                let Some(m) = o.timeline.as_ref().and_then(|t| t.find(author, id)) else {
                    return Task::none();
                };
                o.quote = Some(QuoteRef { id, author, text: m.preview(), ranges: m.ranges.clone() });
                o.reacting = None;
                return iced::widget::operation::focus(crate::view::COMPOSER);
            }
            Message::CancelReply => {
                if let Some(o) = &mut self.open {
                    o.quote = None;
                }
            }
            Message::Hover(h) => {
                if let Some(o) = &mut self.open {
                    o.hovered = h;
                }
            }
            Message::ToggleReactions(target) => {
                if let Some(o) = &mut self.open {
                    o.reacting = if o.reacting == target { None } else { target };
                }
            }
            Message::React(author, id, emoji) => self.react(author, id, emoji),
            Message::TogglePicker => {
                if let Some(o) = &mut self.open {
                    o.picker = match o.picker {
                        Some(_) => None,
                        None => Some(Picker { query: String::new(), group: emojis::Group::SmileysAndEmotion }),
                    };
                }
            }
            Message::PickerQuery(q) => {
                if let Some(p) = self.open.as_mut().and_then(|o| o.picker.as_mut()) {
                    p.query = q;
                }
            }
            Message::PickerGroup(g) => {
                if let Some(p) = self.open.as_mut().and_then(|o| o.picker.as_mut()) {
                    p.group = g;
                    p.query.clear();
                }
            }
            Message::InsertEmoji(e) => {
                if let Some(o) = &mut self.open {
                    o.composer.perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                        std::sync::Arc::new(e.to_owned()),
                    )));
                    return iced::widget::operation::focus(crate::view::COMPOSER);
                }
            }
            Message::Mention(id) => {
                let Some(typed) = self.mention_query().map(|q| q.chars().count()) else { return Task::none() };
                let name = display_name(&self.names, id);
                let Some(o) = &mut self.open else { return Task::none() };
                // Replace the typed "@query" before the cursor with the full name.
                for _ in 0..=typed {
                    o.composer.perform(text_editor::Action::Edit(text_editor::Edit::Backspace));
                }
                o.composer.perform(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new(format!(
                    "@{name} "
                )))));
                o.mentions.push((name, id));
                return iced::widget::operation::focus(crate::view::COMPOSER);
            }
            Message::Spoiler(id, seg) => {
                if let Some(o) = &mut self.open {
                    o.spoilers.insert((id, seg));
                }
            }
            Message::LoadOlder => {
                if let Some(o) = &mut self.open {
                    o.shown += PAGE;
                }
            }
            Message::ToggleTimerMenu => {
                if let Some(o) = self.open.as_mut().filter(|o| !o.is_group) {
                    o.timer_menu = !o.timer_menu;
                }
            }
            Message::SetTimer(seconds) => {
                // Group timers are a group change (admin APIs); the header is read-only there.
                let Some(o) = self.open.as_mut().filter(|o| !o.is_group) else { return Task::none() };
                o.timer_menu = false;
                let thread = o.thread.clone();
                if self.timer(&thread) == seconds {
                    return Task::none();
                }
                if let Some(c) = self.conversations.iter_mut().find(|c| c.thread == thread) {
                    c.expire_timer = seconds;
                }
                self.send(Command::SetTimer { thread, seconds });
            }
            Message::FetchAttachment(key) => self.fetch(&key),
            Message::SaveAttachment(key) => {
                let Some(Attach::Ready(src)) = self.attachments.get(&key) else { return Task::none() };
                let src = src.clone();
                let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return Task::perform(
                    async move {
                        let Some(dest) = rfd::AsyncFileDialog::new().set_file_name(name).save_file().await else {
                            return Ok(());
                        };
                        tokio::fs::copy(&src, dest.path()).await.map(drop).map_err(|e| e.to_string())
                    },
                    Message::Saved,
                );
            }
            Message::Saved(r) => self.error = r.err().map(|e| format!("Save failed: {e}")),
            Message::Theme(mode) => self.dark = mode != iced::theme::Mode::Light,
            Message::Tick => self.tick(),
            Message::DismissError => self.error = None,
        }
        Task::none()
    }

    fn backend(&mut self, e: Event) -> Task<Message> {
        match e {
            Event::Started(tx) => self.cmd = Some(tx),
            Event::NeedsLink => {
                let error = match &self.screen {
                    Screen::Link { error, .. } => error.clone(),
                    _ => None,
                };
                self.screen = Screen::Link { device_name: default_device_name(), qr: None, waiting: false, error };
            }
            Event::LinkUrl(url) => {
                if let Screen::Link { qr, .. } = &mut self.screen {
                    *qr = qr_code::Data::new(url).ok();
                }
            }
            Event::LinkFailed(err) => {
                if let Screen::Link { error, waiting, qr, .. } = &mut self.screen {
                    *error = Some(err);
                    *waiting = false;
                    *qr = None;
                }
            }
            Event::Ready { me } => {
                self.me = me;
                self.screen = Screen::Main;
            }
            Event::Names(n) => self.names = n,
            Event::SearchResults { query, results } => {
                if query == self.search.trim() {
                    self.results = results;
                }
            }
            Event::Conversations(c) => {
                self.conversations = c;
                if let Some(o) = &mut self.open {
                    if let Some(c) = self.conversations.iter().find(|c| c.thread == o.thread) {
                        o.title = c.title.clone();
                    }
                }
            }
            Event::Timeline { thread, timeline } => {
                if let Some(o) = self.open.as_mut().filter(|o| o.thread == thread) {
                    // Keep optimistic messages that the store doesn't know yet.
                    let mut timeline = timeline;
                    if let Some(old) = &o.timeline {
                        for m in old.messages.iter().filter(|m| m.delivery == Delivery::Sending) {
                            if timeline.find(m.author, m.id).is_none() {
                                timeline.messages.push(m.clone());
                            }
                        }
                    }
                    o.timeline = Some(timeline);
                    self.auto_download();
                }
            }
            Event::Incoming { thread, content } => {
                if let Some(t) = self.open.as_mut().filter(|o| o.thread == thread).and_then(|o| o.timeline.as_mut()) {
                    if t.apply(&content) {
                        if let Some(by) = self.typing.get_mut(&thread) {
                            by.remove(&content.metadata.sender.raw_uuid());
                        }
                        self.auto_download();
                    }
                }
            }
            Event::Receipt { timestamps, level } => {
                if let Some(t) = self.open.as_mut().and_then(|o| o.timeline.as_mut()) {
                    t.receipt(&timestamps, level);
                }
            }
            Event::Typing { thread, who, started } => {
                let by = self.typing.entry(thread).or_default();
                if started {
                    by.insert(who, Instant::now());
                } else {
                    by.remove(&who);
                }
            }
            Event::Sent { thread, ts, attachments, error } => {
                if let Some(t) = self.open.as_mut().filter(|o| o.thread == thread).and_then(|o| o.timeline.as_mut()) {
                    let me = t.me;
                    if let Some(m) = t.messages.iter_mut().find(|m| m.id == ts && m.author == me) {
                        m.delivery = if error.is_some() { Delivery::Failed } else { m.delivery.max(Delivery::Sent) };
                        if !attachments.is_empty() {
                            m.attachments = attachments; // real pointers; their files are already cached
                        }
                    }
                }
                self.auto_download();
                if let Some(e) = error {
                    self.error = Some(format!("Message not sent: {e}"));
                }
            }
            Event::Attachment { key, path } => {
                let a = match path {
                    Ok(p) => Attach::Ready(p),
                    Err(e) => Attach::Failed(e),
                };
                self.attachments.insert(key, a);
            }
            Event::Expiring { thread, expires } => {
                if let Some(o) = self.open.as_mut().filter(|o| o.thread == thread) {
                    o.expires = expires.into_iter().map(|(a, ts, at)| ((a, ts), at)).collect();
                }
            }
            Event::Expired { thread, ids } => {
                if let Some(o) = self.open.as_mut().filter(|o| o.thread == thread) {
                    if let Some(t) = &mut o.timeline {
                        t.messages.retain(|m| !ids.contains(&(m.author, m.id)));
                    }
                    for id in &ids {
                        o.expires.remove(id);
                    }
                }
            }
            Event::Error(e) => self.error = Some(e),
            Event::Notice(n) => self.notice = n,
        }
        Task::none()
    }

    fn send_message(&mut self) {
        let me = self.me;
        let Some(o) = &mut self.open else { return };
        let raw = o.composer.text();
        let raw = raw.trim_end_matches('\n');
        if raw.trim().is_empty() && o.files.is_empty() {
            return;
        }
        let (body, ranges) = richtext::parse_markup(&emoji::replace_shortcodes(raw.trim()));
        let picked: Vec<(String, String)> = o.mentions.drain(..).map(|(n, id)| (n, id.to_string())).collect();
        let (body, ranges) = richtext::apply_mentions(&body, ranges, &picked);
        let ts = now_ms();
        let mut m = Msg::new(ts, me, true);
        m.body = body.clone();
        m.ranges = ranges.clone();
        m.delivery = Delivery::Sending;
        m.quote = o.quote.as_ref().map(|q| crate::timeline::Quote {
            id: q.id,
            author: Some(q.author),
            text: q.text.clone(),
            ranges: q.ranges.clone(),
        });
        m.attachments = o
            .files
            .iter()
            .map(|p| presage::proto::AttachmentPointer {
                content_type: Some(mime_guess::from_path(p).first_or_octet_stream().to_string()),
                file_name: p.file_name().map(|n| n.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .collect();
        if let Some(t) = &mut o.timeline {
            t.messages.push(m);
        }
        let cmd = Command::Send {
            thread: o.thread.clone(),
            ts,
            body,
            ranges,
            attachments: std::mem::take(&mut o.files),
            quote: o.quote.take(),
        };
        o.composer = text_editor::Content::new();
        o.picker = None;
        o.typing_sent = false; // a sent message implicitly stops typing for recipients
        o.last_keystroke = None;
        self.send(cmd);
    }

    fn react(&mut self, author: Uuid, id: u64, emoji: String) {
        let me = self.me;
        let Some(o) = &mut self.open else { return };
        o.reacting = None;
        let Some(t) = &mut o.timeline else { return };
        let Some(m) = t.messages.iter_mut().find(|m| m.id == id && m.author == author) else { return };
        let remove = m.reactions.iter().any(|(who, e)| *who == me && *e == emoji);
        m.reactions.retain(|(who, _)| *who != me);
        if !remove {
            m.reactions.push((me, emoji.clone()));
        }
        let thread = o.thread.clone();
        self.send(Command::React { thread, target_author: author, target_ts: id, emoji, remove });
    }

    fn stop_typing(&mut self) {
        if let Some(o) = self.open.as_mut().filter(|o| o.typing_sent) {
            o.typing_sent = false;
            let thread = o.thread.clone();
            self.send(Command::Typing { thread, started: false });
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        for by in self.typing.values_mut() {
            by.retain(|_, at| now.duration_since(*at) < TYPING_TIMEOUT);
        }
        let idle = self
            .open
            .as_ref()
            .and_then(|o| o.last_keystroke)
            .is_none_or(|t| now.duration_since(t) > Duration::from_secs(5));
        if idle {
            self.stop_typing();
        }
    }

    fn fetch(&mut self, key: &str) {
        if matches!(self.attachments.get(key), Some(Attach::Loading | Attach::Ready(_))) {
            return;
        }
        let pointer = self
            .open
            .as_ref()
            .and_then(|o| o.timeline.as_ref())
            .and_then(|t| t.messages.iter().flat_map(|m| &m.attachments).find(|a| attachment_key(a) == key))
            .cloned();
        if let Some(p) = pointer.filter(|p| p.cdn_number.is_some() || p.attachment_identifier.is_some()) {
            self.attachments.insert(key.to_owned(), Attach::Loading);
            self.send(Command::FetchAttachment(p));
        }
    }

    /// Requests images (and stickers) in the visible window.
    fn auto_download(&mut self) {
        let Some(o) = &self.open else { return };
        let Some(t) = &o.timeline else { return };
        let keys: Vec<String> = t
            .messages
            .iter()
            .rev()
            .take(o.shown)
            .flat_map(|m| &m.attachments)
            .filter(|a| {
                a.content_type.as_deref().is_some_and(|c| c.starts_with("image/"))
                    && a.size.unwrap_or(0) <= AUTO_DOWNLOAD_BYTES
            })
            .map(attachment_key)
            .collect();
        for k in keys {
            self.fetch(&k);
        }
    }

    /// The thread's disappearing-message timer in seconds (0 = off).
    pub fn timer(&self, thread: &Thread) -> u32 {
        self.conversations.iter().find(|c| c.thread == *thread).map_or(0, |c| c.expire_timer)
    }

    pub fn typing_names(&self, thread: &Thread) -> Vec<String> {
        self.typing
            .get(thread)
            .map(|by| by.keys().map(|id| display_name(&self.names, *id)).collect())
            .unwrap_or_default()
    }

    /// The `@query` typed just before the cursor in a group chat's composer.
    pub fn mention_query(&self) -> Option<String> {
        let o = self.open.as_ref().filter(|o| o.is_group)?;
        let at = o.composer.cursor().position;
        let line = o.composer.line(at.line)?;
        mention_query(line.text.get(..at.column)?).map(str::to_owned)
    }

    /// Group members (not us) whose name matches the `@query` being typed.
    pub fn mention_matches(&self) -> Vec<(Uuid, String)> {
        let Some(q) = self.mention_query().map(|q| q.to_lowercase()) else { return Vec::new() };
        let Some(c) = self.open.as_ref().and_then(|o| self.conversations.iter().find(|c| c.thread == o.thread)) else {
            return Vec::new();
        };
        let mut v: Vec<(Uuid, String)> = c
            .members
            .iter()
            .filter(|id| **id != self.me)
            .map(|id| (*id, display_name(&self.names, *id)))
            .filter(|(_, n)| n.to_lowercase().contains(&q))
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v.truncate(8);
        v
    }

    /// Contacts matching the search that have no conversation yet ("new chat").
    pub fn contact_matches(&self) -> Vec<(Uuid, String)> {
        let q = self.search.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut v: Vec<(Uuid, String)> = self
            .names
            .iter()
            .filter(|(id, n)| {
                n.to_lowercase().contains(&q)
                    && !self.conversations.iter().any(|c| c.thread == Thread::Contact(ServiceId::Aci((**id).into())))
            })
            .map(|(id, n)| (*id, n.clone()))
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v.truncate(50);
        v
    }
}

/// The letters after an `@` that ends `before` (the `@` at the start or after a space).
fn mention_query(before: &str) -> Option<&str> {
    let at = before.rfind('@')?;
    let q = &before[at + 1..];
    let starts_word = before[..at].chars().next_back().is_none_or(char::is_whitespace);
    (starts_word && q.chars().all(char::is_alphanumeric)).then_some(q)
}

fn default_device_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_owned()).unwrap_or_default();
    if host.is_empty() { "signal-rs".into() } else { format!("signal-rs on {host}") }
}

#[cfg(test)]
#[allow(unused_must_use)]
pub(crate) mod tests {
    use super::*;
    use crate::timeline::tests::{content, text, uuid};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    pub const ME: u8 = 1;
    pub const ALICE: u8 = 2;

    pub fn alice() -> Thread {
        Thread::Contact(ServiceId::Aci(uuid(ALICE).into()))
    }

    /// An app on the main screen with one open conversation and a captured command channel.
    pub fn app() -> (App, UnboundedReceiver<Command>) {
        let (tx, rx) = unbounded_channel();
        let (mut app, _) = App::new(PathBuf::from("/nonexistent"));
        app.update(Message::Backend(Event::Started(tx)));
        app.update(Message::Backend(Event::Ready { me: uuid(ME) }));
        app.update(Message::Backend(Event::Names(HashMap::from([
            (uuid(ALICE), "Alice".to_string()),
            (uuid(3), "Bob".to_string()),
        ]))));
        app.update(Message::Backend(Event::Conversations(vec![Conversation {
            thread: alice(),
            title: "Alice".into(),
            last_ts: 10,
            preview: "hi there".into(),
            unread: 2,
            is_group: false,
            expire_timer: 0,
            members: Vec::new(),
        }])));
        (app, rx)
    }

    pub fn group() -> Thread {
        Thread::Group([7; 32])
    }

    /// Like `app()`, plus an open group "Friends" with us, Alice, Bob and Bobby.
    pub fn open_group() -> (App, UnboundedReceiver<Command>) {
        let (mut app, mut rx) = app();
        app.names.insert(uuid(4), "Bobby".into());
        let mut convs = app.conversations.clone();
        convs.push(Conversation {
            thread: group(),
            title: "Friends".into(),
            last_ts: 5,
            preview: String::new(),
            unread: 0,
            is_group: true,
            members: vec![uuid(ME), uuid(ALICE), uuid(3), uuid(4)],
            expire_timer: 0,
        });
        app.update(Message::Backend(Event::Conversations(convs)));
        app.update(Message::OpenThread(group(), "Friends".into(), true));
        app.update(Message::Backend(Event::Timeline { thread: group(), timeline: Timeline::new(uuid(ME)) }));
        drain(&mut rx);
        (app, rx)
    }

    pub fn open_alice(app: &mut App, rx: &mut UnboundedReceiver<Command>) {
        app.update(Message::OpenThread(alice(), "Alice".into(), false));
        assert!(matches!(rx.try_recv(), Ok(Command::Open(t)) if t == alice()));
        let mut t = Timeline::new(uuid(ME));
        t.apply(&content(uuid(ALICE), 10, text(10, "hi there")));
        app.update(Message::Backend(Event::Timeline { thread: alice(), timeline: t }));
    }

    pub fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.update(Message::Compose(text_editor::Action::Edit(text_editor::Edit::Insert(c))));
        }
    }

    pub fn drain(rx: &mut UnboundedReceiver<Command>) -> Vec<Command> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn startup_link_flow() {
        let (tx, mut rx) = unbounded_channel();
        let (mut app, _) = App::new(PathBuf::from("/x"));
        app.update(Message::Backend(Event::Started(tx)));
        assert!(matches!(app.screen, Screen::Connecting));
        app.update(Message::Backend(Event::NeedsLink));
        app.update(Message::DeviceName("  ".into()));
        app.update(Message::Link);
        assert!(drain(&mut rx).is_empty(), "blank device name is rejected");
        app.update(Message::DeviceName("desk".into()));
        app.update(Message::Link);
        app.update(Message::Link);
        let cmds = drain(&mut rx);
        assert_eq!(cmds.len(), 1, "double click links once");
        assert!(matches!(&cmds[0], Command::Link { device_name } if device_name == "desk"));
        app.update(Message::Backend(Event::LinkUrl("sgnl://linkdevice?uuid=x&pub_key=y".into())));
        assert!(matches!(&app.screen, Screen::Link { qr: Some(_), .. }));
        app.update(Message::Backend(Event::LinkFailed("timeout".into())));
        app.update(Message::Backend(Event::NeedsLink));
        assert!(matches!(&app.screen, Screen::Link { error: Some(e), waiting: false, .. } if e == "timeout"));
        app.update(Message::Backend(Event::Ready { me: uuid(ME) }));
        assert!(matches!(app.screen, Screen::Main));
    }

    #[test]
    fn send_rich_text_with_shortcodes_optimistically() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        type_text(&mut app, "*hi* :smile:");
        let typing = drain(&mut rx);
        assert!(matches!(typing.as_slice(), [Command::Typing { started: true, .. }]), "typing sent once");
        app.update(Message::Send);
        let cmds = drain(&mut rx);
        let Some(Command::Send { body, ranges, ts, .. }) = cmds.first() else { panic!("{cmds:?}") };
        assert_eq!(body, "hi 😄");
        assert_eq!(ranges.len(), 1);
        let o = app.open.as_ref().unwrap();
        assert_eq!(o.composer.text().trim(), "", "composer cleared");
        let m = o.timeline.as_ref().unwrap().messages.last().unwrap();
        assert_eq!((m.id, m.delivery, m.outgoing), (*ts, Delivery::Sending, true));

        let ts = *ts;
        app.update(Message::Backend(Event::Sent { thread: alice(), ts, attachments: vec![], error: None }));
        let m = app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.last().unwrap();
        assert_eq!(m.delivery, Delivery::Sent);
        app.update(Message::Backend(Event::Receipt { timestamps: vec![ts], level: Delivery::Read }));
        let m = app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.last().unwrap();
        assert_eq!(m.delivery, Delivery::Read);
    }

    #[test]
    fn empty_message_is_not_sent_and_failure_is_reported() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        type_text(&mut app, "   ");
        app.update(Message::Send);
        assert!(drain(&mut rx).iter().all(|c| !matches!(c, Command::Send { .. })));

        type_text(&mut app, "x");
        app.update(Message::Send);
        let Some(Command::Send { ts, .. }) = drain(&mut rx).into_iter().find(|c| matches!(c, Command::Send { .. }))
        else {
            panic!()
        };
        app.update(Message::Backend(Event::Sent { thread: alice(), ts, attachments: vec![], error: Some("rate limited".into()) }));
        let m = app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.last().unwrap();
        assert_eq!(m.delivery, Delivery::Failed);
        assert!(app.error.as_deref().unwrap().contains("rate limited"));
    }

    #[test]
    fn reply_attaches_quote_and_clears() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::Reply(uuid(ALICE), 10));
        assert_eq!(app.open.as_ref().unwrap().quote.as_ref().unwrap().text, "hi there");
        type_text(&mut app, "yes");
        app.update(Message::Send);
        let cmds = drain(&mut rx);
        let Some(Command::Send { quote: Some(q), .. }) = cmds.iter().find(|c| matches!(c, Command::Send { .. })) else {
            panic!("{cmds:?}")
        };
        assert_eq!((q.id, q.author), (10, uuid(ALICE)));
        assert!(app.open.as_ref().unwrap().quote.is_none());
    }

    #[test]
    fn reactions_toggle_and_replace() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let reactions = |app: &App| app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages[0].reactions.clone();
        app.update(Message::React(uuid(ALICE), 10, "👍".into()));
        assert_eq!(reactions(&app), vec![(uuid(ME), "👍".to_string())]);
        app.update(Message::React(uuid(ALICE), 10, "❤️".into()));
        assert_eq!(reactions(&app), vec![(uuid(ME), "❤️".to_string())]);
        app.update(Message::React(uuid(ALICE), 10, "❤️".into()));
        assert!(reactions(&app).is_empty());
        let removes: Vec<bool> = drain(&mut rx)
            .into_iter()
            .filter_map(|c| match c {
                Command::React { remove, .. } => Some(remove),
                _ => None,
            })
            .collect();
        assert_eq!(removes, [false, false, true]);
    }

    #[test]
    fn incoming_live_message_appends_to_open_thread_only() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let bob = Thread::Contact(ServiceId::Aci(uuid(3).into()));
        app.update(Message::Backend(Event::Incoming {
            thread: bob,
            content: Box::new(content(uuid(3), 20, text(20, "not here"))),
        }));
        app.update(Message::Backend(Event::Typing { thread: alice(), who: uuid(ALICE), started: true }));
        assert_eq!(app.typing_names(&alice()), ["Alice"]);
        app.update(Message::Backend(Event::Incoming {
            thread: alice(),
            content: Box::new(content(uuid(ALICE), 21, text(21, "new"))),
        }));
        let msgs = &app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages;
        assert_eq!(msgs.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["hi there", "new"]);
        assert!(app.typing_names(&alice()).is_empty(), "a message clears that sender's typing");
    }

    #[test]
    fn images_auto_download_once() {
        let (mut app, mut rx) = app();
        app.update(Message::OpenThread(alice(), "Alice".into(), false));
        drain(&mut rx);
        let dm = presage::proto::DataMessage {
            timestamp: Some(30),
            attachments: vec![
                presage::proto::AttachmentPointer {
                    content_type: Some("image/png".into()),
                    digest: Some(vec![1]),
                    cdn_number: Some(3),
                    size: Some(1000),
                    ..Default::default()
                },
                presage::proto::AttachmentPointer {
                    content_type: Some("application/pdf".into()),
                    digest: Some(vec![2]),
                    cdn_number: Some(3),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut t = Timeline::new(uuid(ME));
        t.apply(&content(uuid(ALICE), 30, dm));
        app.update(Message::Backend(Event::Timeline { thread: alice(), timeline: t.clone() }));
        app.update(Message::Backend(Event::Timeline { thread: alice(), timeline: t }));
        let fetches = drain(&mut rx).into_iter().filter(|c| matches!(c, Command::FetchAttachment(_))).count();
        assert_eq!(fetches, 1, "only the image, only once");
        app.update(Message::FetchAttachment("02".into()));
        assert!(matches!(rx.try_recv(), Ok(Command::FetchAttachment(_))), "files download on request");
    }

    #[test]
    fn sent_photo_swaps_placeholder_for_uploaded_pointer() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::Picked(vec![PathBuf::from("/tmp/cat.png")]));
        app.update(Message::Send);
        let Some(Command::Send { ts, .. }) = drain(&mut rx).into_iter().find(|c| matches!(c, Command::Send { .. })) else { panic!() };
        let placeholder = &app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.last().unwrap().attachments[0];
        assert!(placeholder.cdn_number.is_none(), "optimistic placeholder until upload");

        let uploaded = presage::proto::AttachmentPointer {
            content_type: Some("image/png".into()),
            digest: Some(vec![0xcd]),
            cdn_number: Some(3),
            size: Some(10),
            ..Default::default()
        };
        app.update(Message::Backend(Event::Sent { thread: alice(), ts, attachments: vec![uploaded], error: None }));
        let m = app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.last().unwrap();
        assert_eq!(m.attachments[0].digest.as_deref(), Some(&[0xcd][..]));
        assert!(
            drain(&mut rx).iter().any(|c| matches!(c, Command::FetchAttachment(p) if p.digest.as_deref() == Some(&[0xcd][..]))),
            "image requested (served from the cache the backend filled)"
        );
    }

    #[test]
    fn typing_stops_after_idle_and_on_switch() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        type_text(&mut app, "a");
        drain(&mut rx);
        app.open.as_mut().unwrap().last_keystroke = Some(Instant::now() - Duration::from_secs(6));
        app.update(Message::Tick);
        assert!(matches!(rx.try_recv(), Ok(Command::Typing { started: false, .. })));
        app.update(Message::Tick);
        assert!(rx.try_recv().is_err(), "stop is sent once");
    }

    #[test]
    fn search_lists_contacts_without_conversations() {
        let (mut app, _rx) = app();
        app.update(Message::Search("o".into()));
        assert_eq!(app.contact_matches(), vec![(uuid(3), "Bob".to_string())], "Alice already has a chat");
        app.update(Message::Search("".into()));
        assert!(app.contact_matches().is_empty());
    }

    #[test]
    fn setting_the_timer_sends_once_and_updates_the_header() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::ToggleTimerMenu);
        assert!(app.open.as_ref().unwrap().timer_menu);
        app.update(Message::SetTimer(300));
        app.update(Message::SetTimer(300));
        let cmds = drain(&mut rx);
        assert!(matches!(cmds.as_slice(), [Command::SetTimer { thread, seconds: 300 }] if *thread == alice()), "{cmds:?}");
        assert_eq!(app.timer(&alice()), 300);
        assert!(!app.open.as_ref().unwrap().timer_menu, "choosing closes the menu");
    }

    #[test]
    fn group_timer_is_read_only() {
        let (mut app, mut rx) = app();
        app.update(Message::OpenThread(Thread::Group([5; 32]), "Team".into(), true));
        drain(&mut rx);
        app.update(Message::ToggleTimerMenu);
        app.update(Message::SetTimer(30));
        assert!(drain(&mut rx).is_empty());
        assert!(!app.open.as_ref().unwrap().timer_menu);
    }

    #[test]
    fn expired_messages_leave_the_open_timeline() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::Backend(Event::Expiring { thread: alice(), expires: vec![(uuid(ALICE), 10, 99)] }));
        assert_eq!(app.open.as_ref().unwrap().expires.get(&(uuid(ALICE), 10)), Some(&99));
        let other = Thread::Contact(ServiceId::Aci(uuid(3).into()));
        app.update(Message::Backend(Event::Expired { thread: other, ids: vec![(uuid(ALICE), 10)] }));
        assert_eq!(app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.len(), 1, "other chats' expiries don't touch this one");
        app.update(Message::Backend(Event::Expired { thread: alice(), ids: vec![(uuid(ALICE), 10)] }));
        let o = app.open.as_ref().unwrap();
        assert!(o.timeline.as_ref().unwrap().messages.is_empty() && o.expires.is_empty());
    }

    #[test]
    fn mention_query_needs_a_word_start() {
        assert_eq!(mention_query("hi @Bo"), Some("Bo"));
        assert_eq!(mention_query("@"), Some(""));
        assert_eq!(mention_query("mail@Bo"), None);
        assert_eq!(mention_query("hi @Bob "), None, "a space ends it");
        assert_eq!(mention_query("hi"), None);
    }

    #[test]
    fn mention_autocomplete_lists_members_but_not_me_or_in_1to1() {
        let (mut app, _rx) = open_group();
        type_text(&mut app, "hey @bo");
        let names: Vec<String> = app.mention_matches().into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, ["Bob", "Bobby"]);
        type_text(&mut app, " ");
        assert!(app.mention_matches().is_empty(), "closed after a space");

        let (mut app, mut rx) = self::app();
        open_alice(&mut app, &mut rx);
        type_text(&mut app, "@a");
        assert!(app.mention_matches().is_empty(), "no mentions in 1:1 chats");
    }

    fn sent(rx: &mut UnboundedReceiver<Command>) -> (String, Vec<presage::proto::BodyRange>) {
        drain(rx)
            .into_iter()
            .find_map(|c| match c {
                Command::Send { body, ranges, .. } => Some((body, ranges)),
                _ => None,
            })
            .expect("sent")
    }

    #[test]
    fn picked_mention_is_sent_as_placeholder_with_aci() {
        let (mut app, mut rx) = open_group();
        type_text(&mut app, "*hi @bo");
        app.update(Message::Mention(uuid(3)));
        type_text(&mut app, "and @Ali");
        app.update(Message::Mention(uuid(ALICE)));
        type_text(&mut app, "😀*");
        assert_eq!(app.open.as_ref().unwrap().composer.text().trim_end(), "*hi @Bob and @Alice 😀*");
        app.update(Message::Send);
        let (body, ranges) = sent(&mut rx);
        assert_eq!(body, "hi \u{FFFC} and \u{FFFC} 😀");
        let segs = richtext::segments(&body, &ranges);
        let mentions: Vec<_> = segs.iter().filter_map(|s| s.mention.clone()).collect();
        assert_eq!(mentions, [uuid(3).to_string(), uuid(ALICE).to_string()]);
        assert!(segs.iter().all(|s| s.style.bold), "bold spans the mentions: {segs:?}");
        assert!(app.open.as_ref().unwrap().mentions.is_empty(), "picks reset after sending");
    }

    #[test]
    fn mention_edited_away_is_not_sent() {
        let (mut app, mut rx) = open_group();
        type_text(&mut app, "@bo");
        app.update(Message::Mention(uuid(3)));
        // Delete "b " so it reads "@Bo", then make it "@Boo!".
        for _ in 0..2 {
            app.update(Message::Compose(text_editor::Action::Edit(text_editor::Edit::Backspace)));
        }
        type_text(&mut app, "o!");
        app.update(Message::Send);
        let (body, ranges) = sent(&mut rx);
        assert_eq!(body, "@Boo!");
        assert!(ranges.is_empty());
    }

    #[test]
    fn search_is_debounced_and_ignores_stale_results() {
        let (mut app, mut rx) = app();
        assert!(app.update(Message::Search("h".into())).units() == 0, "too short to search");
        app.update(Message::Search("hel".into()));
        app.update(Message::RunSearch("he".into()));
        assert!(drain(&mut rx).is_empty(), "no longer current");
        app.update(Message::RunSearch("hel".into()));
        assert!(matches!(drain(&mut rx).as_slice(), [Command::Search(q)] if q == "hel"));

        let result = SearchResult { thread: alice(), title: "Alice".into(), id: 10, author: uuid(ALICE), snippet: "hello".into() };
        app.update(Message::Backend(Event::SearchResults { query: "he".into(), results: vec![result.clone()] }));
        assert!(app.results.is_empty(), "stale");
        app.update(Message::Backend(Event::SearchResults { query: "hel".into(), results: vec![result] }));
        assert_eq!(app.results.len(), 1);
        app.update(Message::Search("".into()));
        assert!(app.results.is_empty());
    }

    #[test]
    fn window_title_counts_unread() {
        let (app, _rx) = app();
        assert_eq!(app.title(), "Signal (2)");
    }
}
