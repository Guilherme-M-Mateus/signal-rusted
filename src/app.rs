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

use crate::backend::{self, Command, Conversation, Event, QuoteRef, attachment_key, now_ms};
use crate::timeline::{Delivery, Message as Msg, Timeline};
use crate::{audio, emoji, richtext};

/// Messages rendered at once; "Load older" extends it (keeps layout cheap on long threads).
pub const PAGE: usize = 150;
/// Images up to this size are downloaded automatically, like Signal's auto-download.
const AUTO_DOWNLOAD_BYTES: u32 = 25 * 1024 * 1024;
const TYPING_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// When the current voice-note recording started.
    pub recording: Option<Instant>,
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
    pub open: Option<Open>,
    pub dark: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub attachments: HashMap<String, Attach>,
    pub typing: HashMap<Thread, HashMap<Uuid, Instant>>,
    /// Device thread for playback/recording, started on first use (tests inject a channel).
    pub audio: Option<std::sync::mpsc::Sender<audio::Cmd>>,
    pub playback: Option<audio::Playback>,
    /// Lengths of downloaded audio attachments, by attachment key.
    pub durations: HashMap<String, Duration>,
    /// Audio to start once its download finishes.
    play_when_ready: Option<String>,
    data_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum Message {
    Backend(Event),
    DeviceName(String),
    Link,
    Search(String),
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
    Spoiler(u64, usize),
    LoadOlder,
    FetchAttachment(String),
    PlayPause(String),
    AudioDuration(String, Option<Duration>),
    Record,
    RecordStarted(Result<(), String>),
    CancelRecording,
    SendRecording,
    Recorded(Result<PathBuf, String>),
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
            open: None,
            dark: true,
            error: None,
            notice: None,
            attachments: HashMap::new(),
            typing: HashMap::new(),
            audio: None,
            playback: None,
            durations: HashMap::new(),
            play_when_ready: None,
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
        let audio_active = self.playback.as_ref().is_some_and(|p| !p.is_paused())
            || self.open.as_ref().is_some_and(|o| o.recording.is_some());
        if audio_active {
            subs.push(iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick));
        } else if typing_active || self.open.as_ref().is_some_and(|o| o.typing_sent) {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn send(&self, c: Command) {
        if let Some(tx) = &self.cmd {
            let _ = tx.send(c);
        }
    }

    fn audio(&mut self, c: audio::Cmd) {
        let _ = self.audio.get_or_insert_with(audio::spawn).send(c);
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
            Message::Search(s) => self.search = s,
            Message::OpenThread(thread, title, is_group) => {
                if self.open.as_ref().is_some_and(|o| o.thread == thread) {
                    return Task::none();
                }
                self.stop_typing();
                if self.open.as_ref().is_some_and(|o| o.recording.is_some()) {
                    self.audio(audio::Cmd::Finish(None));
                }
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
                    recording: None,
                    last_keystroke: None,
                    typing_sent: false,
                });
                self.search.clear();
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
            Message::Send => self.send_message(false),
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
            Message::FetchAttachment(key) => self.fetch(&key),
            Message::PlayPause(key) => self.play_pause(key),
            Message::AudioDuration(key, d) => {
                if let Some(d) = d {
                    self.durations.insert(key, d);
                }
            }
            Message::Record => {
                let Some(o) = self.open.as_mut().filter(|o| o.recording.is_none()) else { return Task::none() };
                o.recording = Some(Instant::now());
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.audio(audio::Cmd::Record(tx));
                return Task::perform(async { rx.await.unwrap_or_else(|_| Err("audio stopped".into())) }, Message::RecordStarted);
            }
            Message::RecordStarted(r) => {
                if let Err(e) = r {
                    if let Some(o) = &mut self.open {
                        o.recording = None;
                    }
                    self.error = Some(e);
                }
            }
            Message::CancelRecording => {
                if let Some(o) = self.open.as_mut().filter(|o| o.recording.is_some()) {
                    o.recording = None;
                    self.audio(audio::Cmd::Finish(None));
                }
            }
            Message::SendRecording => {
                let Some(o) = self.open.as_mut().filter(|o| o.recording.is_some()) else { return Task::none() };
                o.recording = None;
                // shortcut: the recorded file stays next to the cache after sending; sweep it if they pile up.
                let path = backend::attachments_dir(&self.data_dir).join(format!("voice-note-{}.mp3", now_ms()));
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.audio(audio::Cmd::Finish(Some((path, tx))));
                return Task::perform(async { rx.await.unwrap_or_else(|_| Err("audio stopped".into())) }, Message::Recorded);
            }
            Message::Recorded(Ok(path)) => {
                if let Some(o) = &mut self.open {
                    o.files = vec![path];
                }
                self.send_message(true);
            }
            Message::Recorded(Err(e)) => self.error = Some(format!("Voice note not sent: {e}")),
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
                let mut task = Task::none();
                if let Some(p) = path.as_ref().ok().filter(|p| mime_guess::from_path(p).first_raw().is_some_and(|m| m.starts_with("audio/"))) {
                    let (k, p) = (key.clone(), p.clone());
                    task = Task::perform(
                        async move { tokio::task::spawn_blocking(move || audio::duration(&p)).await.ok().flatten() },
                        move |d| Message::AudioDuration(k.clone(), d),
                    );
                }
                let a = match path {
                    Ok(p) => Attach::Ready(p),
                    Err(e) => Attach::Failed(e),
                };
                self.attachments.insert(key.clone(), a);
                if self.play_when_ready.as_ref() == Some(&key) {
                    self.play_when_ready = None;
                    self.play_pause(key);
                }
                return task;
            }
            Event::Error(e) => self.error = Some(e),
            Event::Notice(n) => self.notice = n,
        }
        Task::none()
    }

    fn send_message(&mut self, voice_note: bool) {
        let me = self.me;
        let Some(o) = &mut self.open else { return };
        let raw = o.composer.text();
        let raw = raw.trim_end_matches('\n');
        if raw.trim().is_empty() && o.files.is_empty() {
            return;
        }
        let (body, ranges) = richtext::parse_markup(&emoji::replace_shortcodes(raw.trim()));
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
                flags: voice_note.then_some(presage::proto::attachment_pointer::Flags::VoiceMessage as u32),
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
            voice_note,
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

    fn play_pause(&mut self, key: String) {
        let now = Instant::now();
        match &mut self.playback {
            Some(p) if p.key == key && p.is_paused() => {
                p.resume(now);
                self.audio(audio::Cmd::Resume);
            }
            Some(p) if p.key == key => {
                p.pause(now);
                self.audio(audio::Cmd::Pause);
            }
            _ => match self.attachments.get(&key) {
                Some(Attach::Ready(path)) => {
                    let path = path.clone();
                    self.playback = Some(audio::Playback::new(key, now));
                    self.audio(audio::Cmd::Play(path));
                }
                _ => {
                    self.fetch(&key);
                    self.play_when_ready = Some(key);
                }
            },
        }
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
        if self.playback.as_ref().is_some_and(|p| p.finished(now, self.durations.get(&p.key).copied())) {
            self.playback = None;
        }
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

    pub fn typing_names(&self, thread: &Thread) -> Vec<String> {
        self.typing
            .get(thread)
            .map(|by| by.keys().map(|id| display_name(&self.names, *id)).collect())
            .unwrap_or_default()
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
        fake_audio(&mut app); // never touch real audio devices from tests
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
        }])));
        (app, rx)
    }

    /// Replaces the audio device thread with a captured channel.
    pub fn fake_audio(app: &mut App) -> std::sync::mpsc::Receiver<audio::Cmd> {
        let (tx, rx) = std::sync::mpsc::channel();
        app.audio = Some(tx);
        rx
    }

    /// An incoming voice note from Alice (ts 40), not downloaded yet.
    pub fn voice_note(app: &mut App) -> String {
        let a = presage::proto::AttachmentPointer {
            content_type: Some("audio/aac".into()),
            digest: Some(vec![0xa0]),
            cdn_number: Some(3),
            flags: Some(presage::proto::attachment_pointer::Flags::VoiceMessage as u32),
            ..Default::default()
        };
        let dm = presage::proto::DataMessage { timestamp: Some(40), attachments: vec![a.clone()], ..Default::default() };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(ALICE), 40, dm)) }));
        attachment_key(&a)
    }

    pub fn open_alice(app: &mut App, rx: &mut UnboundedReceiver<Command>) {
        app.update(Message::OpenThread(alice(), "Alice".into(), false));
        assert!(matches!(rx.try_recv(), Ok(Command::Open(t)) if t == alice()));
        let mut t = Timeline::new(uuid(ME));
        t.apply(&content(uuid(ALICE), 10, text(10, "hi there")));
        app.update(Message::Backend(Event::Timeline { thread: alice(), timeline: t }));
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.update(Message::Compose(text_editor::Action::Edit(text_editor::Edit::Insert(c))));
        }
    }

    fn drain(rx: &mut UnboundedReceiver<Command>) -> Vec<Command> {
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
    fn voice_note_downloads_on_play_then_plays_pauses_and_finishes() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let audio = fake_audio(&mut app);
        let key = voice_note(&mut app);
        assert!(drain(&mut rx).is_empty(), "audio isn't auto-downloaded");

        app.update(Message::PlayPause(key.clone()));
        assert!(matches!(drain(&mut rx).as_slice(), [Command::FetchAttachment(_)]));
        assert!(app.playback.is_none() && audio.try_recv().is_err(), "nothing to play yet");
        app.update(Message::PlayPause(key.clone()));
        assert!(drain(&mut rx).is_empty(), "download requested once");

        let path = PathBuf::from("/nonexistent/a0.m4a");
        app.update(Message::Backend(Event::Attachment { key: key.clone(), path: Ok(path.clone()) }));
        assert!(matches!(audio.try_recv(), Ok(audio::Cmd::Play(p)) if p == path), "starts once downloaded");
        assert!(app.playback.as_ref().is_some_and(|p| p.key == key && !p.is_paused()));

        app.update(Message::PlayPause(key.clone()));
        assert!(matches!(audio.try_recv(), Ok(audio::Cmd::Pause)));
        assert!(app.playback.as_ref().unwrap().is_paused());
        app.update(Message::Tick);
        assert!(app.playback.is_some(), "length unknown: keeps its place");
        app.update(Message::PlayPause(key.clone()));
        assert!(matches!(audio.try_recv(), Ok(audio::Cmd::Resume)));

        app.update(Message::AudioDuration(key.clone(), Some(Duration::ZERO)));
        app.update(Message::Tick);
        assert!(app.playback.is_none(), "finished");
    }

    #[test]
    fn recording_sends_a_voice_note_or_cancels() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let audio = fake_audio(&mut app);

        app.update(Message::Record);
        assert!(matches!(audio.try_recv(), Ok(audio::Cmd::Record(_))));
        app.update(Message::CancelRecording);
        assert!(matches!(audio.try_recv(), Ok(audio::Cmd::Finish(None))));
        assert!(app.open.as_ref().unwrap().recording.is_none());

        app.update(Message::Record);
        app.update(Message::RecordStarted(Err("Microphone unavailable".into())));
        assert!(app.open.as_ref().unwrap().recording.is_none() && app.error.is_some(), "mic failure reported");
        app.error = None;

        app.update(Message::Record);
        app.update(Message::SendRecording);
        let cmds: Vec<audio::Cmd> = audio.try_iter().collect();
        let Some(audio::Cmd::Finish(Some((path, _)))) = cmds.into_iter().last() else { panic!() };
        assert!(path.starts_with("/nonexistent/attachments") && path.extension() == Some("mp3".as_ref()), "{path:?}");
        assert!(drain(&mut rx).is_empty(), "sent only once encoded");

        app.update(Message::Recorded(Ok(path.clone())));
        let cmds = drain(&mut rx);
        let Some(Command::Send { attachments, voice_note, body, .. }) = cmds.iter().find(|c| matches!(c, Command::Send { .. })) else {
            panic!("{cmds:?}")
        };
        assert_eq!((attachments.as_slice(), *voice_note, body.as_str()), (&[path][..], true, ""));
        let m = app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.last().unwrap();
        assert_eq!(m.attachments[0].content_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(m.attachments[0].flags, Some(presage::proto::attachment_pointer::Flags::VoiceMessage as u32));
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
    fn window_title_counts_unread() {
        let (app, _rx) = app();
        assert_eq!(app.title(), "Signal (2)");
    }
}
