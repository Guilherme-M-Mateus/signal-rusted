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

use crate::backend::{self, ChatAction, Command, Conversation, Event, Pack, QuoteRef, SearchResult, attachment_key, now_ms};
use crate::state::MAX_PINNED;
use crate::groups::{GroupEdit, GroupInfo};
use crate::timeline::{Delivery, Message as Msg, Timeline};
use crate::{audio, emoji, richtext};

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
    /// Message whose delete options are shown.
    pub deleting: Option<(Uuid, u64)>,
    /// Our message being edited in the composer.
    pub editing: Option<u64>,
    pub picker: Option<Picker>,
    /// The sticker picker's open tab.
    pub stickers: Option<StickerTab>,
    pub spoilers: HashSet<(u64, usize)>,
    /// (author, sent ts) -> when it disappears (ms).
    pub expires: HashMap<(Uuid, u64), u64>,
    pub timer_menu: bool,
    /// Members picked from the @-autocomplete: (inserted name, ACI).
    pub mentions: Vec<(String, Uuid)>,
    /// When the current voice-note recording started.
    pub recording: Option<Instant>,
    /// The group info panel, shown instead of the timeline.
    pub info: Option<GroupPanel>,
    /// Attachment key of the image shown full-window.
    pub viewer: Option<String>,
    last_keystroke: Option<Instant>,
    typing_sent: bool,
}

/// Edits in progress in the group info panel.
pub struct GroupPanel {
    pub title: String,
    pub description: String,
    pub add_query: String,
}

/// The "New group" dialog.
#[derive(Default)]
pub struct NewGroup {
    pub title: String,
    pub query: String,
    pub picked: Vec<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StickerTab {
    Recent,
    /// A pack id.
    Pack(Vec<u8>),
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
    pub new_group: Option<NewGroup>,
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
    /// Installed sticker packs, by title.
    pub packs: Vec<Pack>,
    /// Recently sent stickers, newest first: (pack id, sticker id).
    pub recent_stickers: Vec<(Vec<u8>, u32)>,
    /// Audio to start once its download finishes.
    play_when_ready: Option<String>,
    /// The sidebar shows archived chats instead of the inbox.
    pub show_archived: bool,
    /// The "Message requests" section is expanded.
    pub requests_open: bool,
    /// Sidebar row under the mouse (shows its "⋯"), and the row whose menu is open.
    pub row_hovered: Option<Thread>,
    pub row_menu: Option<Thread>,
    /// Opens a link in the system browser; tests swap it so nothing is ever launched.
    pub open_url: fn(&str),
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
    Edit(u64),
    CancelEdit,
    ToggleDelete(Option<(Uuid, u64)>),
    DeleteForEveryone(u64),
    DeleteForMe(Uuid, u64),
    Hover(Option<(Uuid, u64)>),
    ToggleReactions(Option<(Uuid, u64)>),
    React(Uuid, u64, String),
    TogglePicker,
    PickerQuery(String),
    PickerGroup(emojis::Group),
    InsertEmoji(&'static str),
    ToggleStickers,
    StickerTab(StickerTab),
    /// (pack id, sticker id)
    SendSticker(Vec<u8>, u32),
    /// (pack id, pack key)
    InstallPack(Vec<u8>, Vec<u8>),
    Mention(Uuid),
    Spoiler(u64, usize),
    LoadOlder,
    ToggleTimerMenu,
    SetTimer(u32),
    ShowArchived(bool),
    ToggleRequests,
    RowHover(Option<Thread>),
    RowMenu(Option<Thread>),
    Chat(Thread, ChatAction),
    ToggleGroupInfo,
    InfoTitle(String),
    InfoDescription(String),
    InfoAddQuery(String),
    EditGroup(GroupEdit),
    /// Opens or closes the "New group" dialog.
    NewGroup,
    NewGroupTitle(String),
    NewGroupQuery(String),
    NewGroupPick(Uuid),
    CreateGroup,
    FetchAttachment(String),
    PlayPause(String),
    AudioDuration(String, Option<Duration>),
    Record,
    RecordStarted(Result<(), String>),
    CancelRecording,
    SendRecording,
    Recorded(Result<PathBuf, String>),
    SaveAttachment(String),
    /// Shows an image attachment full-window.
    ViewImage(String),
    /// Steps the viewer through the conversation's images (-1 back, 1 forward).
    ViewerStep(isize),
    CloseViewer,
    OpenUrl(String),
    Saved(Result<(), String>),
    Theme(iced::theme::Mode),
    Tick,
    DismissError,
}

pub use crate::timeline::display_name;

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
            new_group: None,
            dark: true,
            error: None,
            notice: None,
            attachments: HashMap::new(),
            typing: HashMap::new(),
            audio: None,
            playback: None,
            durations: HashMap::new(),
            packs: Vec::new(),
            recent_stickers: Vec::new(),
            play_when_ready: None,
            show_archived: false,
            requests_open: false,
            row_hovered: None,
            row_menu: None,
            open_url: open_in_browser,
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
        if self.open.as_ref().is_some_and(|o| o.viewer.is_some()) {
            subs.push(iced::keyboard::listen().filter_map(viewer_key));
        }
        if audio_active {
            subs.push(iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick));
        } else if typing_active || self.open.as_ref().is_some_and(|o| o.typing_sent || !o.expires.is_empty()) {
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
                if self.open.as_ref().is_some_and(|o| o.recording.is_some()) {
                    self.audio(audio::Cmd::Finish(None));
                }
                self.send(Command::Open(thread.clone()));
                self.row_menu = None;
                if let Some(c) = self.conversations.iter_mut().find(|c| c.thread == thread) {
                    c.marked_unread = false; // the backend clears it too
                }
                self.new_group = None;
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
                    deleting: None,
                    editing: None,
                    picker: None,
                    stickers: None,
                    spoilers: HashSet::new(),
                    expires: HashMap::new(),
                    timer_menu: false,
                    mentions: Vec::new(),
                    recording: None,
                    info: None,
                    viewer: None,
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
                    if !o.typing_sent && !o.composer.text().trim().is_empty() && !self.conversations.iter().any(|c| c.thread == o.thread && c.request) {
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
            Message::Edit(id) => {
                let me = self.me;
                let Some(o) = &mut self.open else { return Task::none() };
                let Some(m) = o.timeline.as_ref().and_then(|t| t.find(me, id)).filter(|m| m.can_edit(now_ms())) else {
                    return Task::none();
                };
                o.composer = text_editor::Content::with_text(&richtext::to_markup(&m.body, &m.ranges));
                o.composer.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                o.editing = Some(id);
                (o.quote, o.reacting, o.deleting) = (None, None, None);
                return iced::widget::operation::focus(crate::view::COMPOSER);
            }
            Message::CancelEdit => {
                if let Some(o) = self.open.as_mut().filter(|o| o.editing.is_some()) {
                    o.editing = None;
                    o.composer = text_editor::Content::new();
                }
            }
            Message::ToggleDelete(target) => {
                if let Some(o) = &mut self.open {
                    o.deleting = if o.deleting == target { None } else { target };
                    o.reacting = None;
                }
            }
            Message::DeleteForEveryone(id) => self.delete(self.me, id, true),
            Message::DeleteForMe(author, id) => self.delete(author, id, false),
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
                    o.stickers = None;
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
            Message::ToggleStickers => {
                let tab = self.default_sticker_tab();
                let Some(o) = &mut self.open else { return Task::none() };
                o.picker = None;
                o.stickers = match o.stickers {
                    Some(_) => None,
                    None => Some(tab),
                };
                if o.stickers.is_some() {
                    self.send(Command::LoadStickers); // picks up packs synced from the phone since
                }
            }
            Message::StickerTab(tab) => {
                if let Some(o) = self.open.as_mut().filter(|o| o.stickers.is_some()) {
                    o.stickers = Some(tab);
                }
            }
            Message::SendSticker(pack_id, id) => self.send_sticker(pack_id, id),
            Message::InstallPack(id, key) => self.send(Command::InstallStickerPack { id, key }),
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
                if self.can_set_timer() {
                    if let Some(o) = &mut self.open {
                        o.timer_menu = !o.timer_menu;
                    }
                }
            }
            Message::SetTimer(seconds) => {
                // In groups it's a group change, so it needs the right to edit the group's details.
                if !self.can_set_timer() {
                    return Task::none();
                }
                let Some(o) = &mut self.open else { return Task::none() };
                o.timer_menu = false;
                let (thread, is_group) = (o.thread.clone(), o.is_group);
                if self.timer(&thread) == seconds {
                    return Task::none();
                }
                if let Some(c) = self.conversations.iter_mut().find(|c| c.thread == thread) {
                    c.expire_timer = seconds;
                }
                self.send(if is_group {
                    Command::EditGroup { thread, edit: GroupEdit::Timer(seconds) }
                } else {
                    Command::SetTimer { thread, seconds }
                });
            }
            Message::ToggleGroupInfo => {
                let description = self.group_info().map(|g| g.description.clone()).unwrap_or_default();
                if let Some(o) = self.open.as_mut().filter(|o| o.is_group) {
                    o.info = match o.info {
                        Some(_) => None,
                        None => Some(GroupPanel { title: o.title.clone(), description, add_query: String::new() }),
                    };
                }
            }
            Message::InfoTitle(t) => {
                if let Some(p) = self.open.as_mut().and_then(|o| o.info.as_mut()) {
                    p.title = t;
                }
            }
            Message::InfoDescription(d) => {
                if let Some(p) = self.open.as_mut().and_then(|o| o.info.as_mut()) {
                    p.description = d;
                }
            }
            Message::InfoAddQuery(q) => {
                if let Some(p) = self.open.as_mut().and_then(|o| o.info.as_mut()) {
                    p.add_query = q;
                }
            }
            Message::EditGroup(edit) => {
                let Some(o) = &self.open else { return Task::none() };
                if !self.group_info().is_some_and(|g| g.allows_edit(self.me, &edit)) {
                    return Task::none();
                }
                let thread = o.thread.clone();
                if let Some(p) = self.open.as_mut().and_then(|o| o.info.as_mut()).filter(|_| matches!(edit, GroupEdit::AddMembers(_))) {
                    p.add_query.clear();
                }
                self.send(Command::EditGroup { thread, edit });
            }
            Message::NewGroup => {
                self.new_group = match self.new_group {
                    Some(_) => None,
                    None => Some(NewGroup::default()),
                };
            }
            Message::NewGroupTitle(t) => {
                if let Some(g) = &mut self.new_group {
                    g.title = t;
                }
            }
            Message::NewGroupQuery(q) => {
                if let Some(g) = &mut self.new_group {
                    g.query = q;
                }
            }
            Message::NewGroupPick(id) => {
                if let Some(g) = &mut self.new_group {
                    match g.picked.iter().position(|p| *p == id) {
                        Some(i) => drop(g.picked.remove(i)),
                        None => g.picked.push(id),
                    }
                }
            }
            Message::CreateGroup => {
                let Some(g) = self.new_group.take_if(|g| !g.title.trim().is_empty()) else { return Task::none() };
                self.send(Command::CreateGroup { title: g.title.trim().to_owned(), members: g.picked });
            }
            Message::ShowArchived(on) => {
                self.show_archived = on;
                self.row_menu = None;
            }
            Message::ToggleRequests => self.requests_open = !self.requests_open,
            Message::RowHover(t) => self.row_hovered = t,
            Message::RowMenu(t) => self.row_menu = if self.row_menu == t { None } else { t },
            Message::Chat(thread, action) => self.chat(thread, action),
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
            Message::ViewImage(key) => {
                let Some(o) = &mut self.open else { return Task::none() };
                o.viewer = Some(key.clone());
                self.fetch(&key);
                // Unfocus the composer so arrows and Esc reach the viewer.
                return iced::widget::operation::focus("lightbox");
            }
            Message::ViewerStep(delta) => {
                let images = self.images();
                let Some(i) = self.open.as_ref().and_then(|o| o.viewer.as_ref()).and_then(|k| images.iter().position(|(_, a)| attachment_key(a) == *k)) else {
                    return Task::none();
                };
                let Some((_, a)) = i.checked_add_signed(delta).and_then(|j| images.get(j)) else { return Task::none() };
                let key = attachment_key(a);
                if let Some(o) = &mut self.open {
                    o.viewer = Some(key.clone());
                }
                self.fetch(&key);
            }
            Message::CloseViewer => {
                if let Some(o) = self.open.as_mut().filter(|o| o.viewer.is_some()) {
                    o.viewer = None;
                    return iced::widget::operation::focus(crate::view::COMPOSER);
                }
            }
            Message::OpenUrl(url) => {
                // Only web links: message text is untrusted, never hand other schemes to the OS.
                if url::Url::parse(&url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
                    (self.open_url)(&url);
                }
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
            Event::GroupCreated { thread, title } => return self.update(Message::OpenThread(thread, title, true)),
            Event::Stickers { packs, recent } => {
                self.packs = packs;
                self.recent_stickers = recent;
                let tab = self.default_sticker_tab();
                let packs = &self.packs;
                if let Some(t) = self.open.as_mut().and_then(|o| o.stickers.as_mut()) {
                    let gone = match t {
                        StickerTab::Recent => self.recent_stickers.is_empty(),
                        StickerTab::Pack(id) => !packs.iter().any(|p| p.id == *id),
                    };
                    if gone {
                        *t = tab;
                    }
                }
            }
            Event::Error(e) => self.error = Some(e),
            Event::Notice(n) => self.notice = n,
        }
        Task::none()
    }

    fn send_message(&mut self, voice_note: bool) {
        let me = self.me;
        if self.open.as_ref().is_some_and(|o| self.is_request(&o.thread)) {
            return; // accept first, like Signal
        }
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
        if let Some(target) = o.editing {
            if body.trim().is_empty() {
                return; // Signal can't edit a message down to nothing
            }
            o.editing = None;
            o.composer = text_editor::Content::new();
            o.typing_sent = false;
            o.last_keystroke = None;
            let Some(m) = o.timeline.as_mut().and_then(|t| t.messages.iter_mut().find(|m| m.id == target && m.author == me))
            else {
                return;
            };
            if m.body == body && m.ranges == ranges {
                return;
            }
            (m.body, m.ranges, m.edited, m.delivery) = (body.clone(), ranges.clone(), true, Delivery::Sending);
            let quote = m.quote.as_ref().and_then(|q| {
                Some(QuoteRef { id: q.id, author: q.author?, text: q.text.clone(), ranges: q.ranges.clone() })
            });
            let cmd = Command::Edit {
                thread: o.thread.clone(),
                target_ts: target,
                ts,
                body,
                ranges,
                attachments: m.attachments.clone(),
                quote,
            };
            self.send(cmd);
            return;
        }
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

    /// Applies a menu or request-bar choice right away; the backend persists it and refreshes.
    fn chat(&mut self, thread: Thread, action: ChatAction) {
        self.row_menu = None;
        if action == ChatAction::Pin(true) && self.conversations.iter().filter(|c| c.pinned).count() >= MAX_PINNED {
            self.error = Some(format!("You can only pin up to {MAX_PINNED} chats"));
            return;
        }
        if let Some(c) = self.conversations.iter_mut().find(|c| c.thread == thread) {
            match action {
                ChatAction::Archive(on) => c.archived = on,
                ChatAction::Pin(on) => c.pinned = on,
                ChatAction::Mute(on) => c.muted = on,
                ChatAction::MarkUnread(on) => c.marked_unread = on,
                ChatAction::Accept => c.request = false,
                ChatAction::Delete | ChatAction::Block => {}
            }
        }
        if matches!(action, ChatAction::Delete | ChatAction::Block) {
            self.conversations.retain(|c| c.thread != thread);
            if self.open.as_ref().is_some_and(|o| o.thread == thread) {
                self.open = None;
            }
        }
        self.send(Command::Chat { thread, action });
    }

    /// Whether `thread` is an unanswered message request.
    pub fn is_request(&self, thread: &Thread) -> bool {
        self.conversations.iter().any(|c| c.thread == *thread && c.request)
    }

    /// Recents if there are any, like Signal Desktop, else the first pack.
    fn default_sticker_tab(&self) -> StickerTab {
        match self.packs.first() {
            Some(p) if self.recent_stickers.is_empty() => StickerTab::Pack(p.id.clone()),
            _ => StickerTab::Recent,
        }
    }

    fn send_sticker(&mut self, pack_id: Vec<u8>, id: u32) {
        let me = self.me;
        let Some(pack) = self.packs.iter().find(|p| p.id == pack_id) else { return };
        let Some(s) = pack.stickers.iter().find(|s| s.id == id) else { return };
        let Some(o) = &mut self.open else { return };
        let sticker = presage::proto::data_message::Sticker {
            pack_id: Some(pack_id.clone()),
            pack_key: Some(pack.key.clone()),
            sticker_id: Some(id),
            emoji: Some(s.emoji.clone()).filter(|e| !e.is_empty()),
            data: None,
        };
        let ts = now_ms();
        let mut m = Msg::new(ts, me, true);
        m.delivery = Delivery::Sending;
        m.sticker = Some(sticker.clone());
        if let Some(t) = &mut o.timeline {
            t.messages.push(m);
        }
        o.stickers = None;
        crate::state::push_recent(&mut self.recent_stickers, (pack_id, id));
        let thread = o.thread.clone();
        self.send(Command::SendSticker { thread, ts, sticker });
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

    /// Deletes for everyone (our own messages, shown as deleted) or just here (removed).
    fn delete(&mut self, author: Uuid, id: u64, for_everyone: bool) {
        let Some(o) = &mut self.open else { return };
        o.deleting = None;
        let Some(t) = &mut o.timeline else { return };
        let Some(i) = t.messages.iter().position(|m| m.id == id && m.author == author) else { return };
        if for_everyone {
            if !t.messages[i].can_delete_for_everyone(now_ms()) {
                return;
            }
            t.messages[i].deleted = true;
        } else {
            t.messages.remove(i);
        }
        if o.editing == Some(id) && author == self.me {
            o.editing = None;
            o.composer = text_editor::Content::new();
        }
        let thread = o.thread.clone();
        self.send(if for_everyone {
            Command::DeleteForEveryone { thread, target_ts: id }
        } else {
            Command::DeleteForMe { thread, author, target_ts: id }
        });
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
            .and_then(|t| t.messages.iter().flat_map(Msg::pointers).find(|a| attachment_key(a) == key))
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
            .flat_map(Msg::pointers)
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

    /// Image attachments in the open conversation, oldest first: what the viewer steps through.
    pub fn images(&self) -> Vec<(&Msg, &presage::proto::AttachmentPointer)> {
        let Some(t) = self.open.as_ref().and_then(|o| o.timeline.as_ref()) else { return Vec::new() };
        t.messages
            .iter()
            .filter(|m| !m.deleted)
            .flat_map(|m| m.attachments.iter().filter(|a| is_image(a)).map(move |a| (m, a)))
            .collect()
    }

    /// The thread's disappearing-message timer in seconds (0 = off).
    pub fn timer(&self, thread: &Thread) -> u32 {
        self.conversations.iter().find(|c| c.thread == *thread).map_or(0, |c| c.expire_timer)
    }

    /// The open group's details, if it's a group.
    pub fn group_info(&self) -> Option<&GroupInfo> {
        let o = self.open.as_ref()?;
        self.conversations.iter().find(|c| c.thread == o.thread)?.group.as_ref()
    }

    /// 1:1 timers are always ours to set; a group's needs the right to edit its details.
    pub fn can_set_timer(&self) -> bool {
        self.open.as_ref().is_some_and(|o| !o.is_group || self.group_info().is_some_and(|g| g.can_edit_details(self.me)))
    }

    /// People (not us, not in `exclude`) whose name matches `query`, by name.
    pub fn people_matching(&self, query: &str, exclude: &[Uuid]) -> Vec<(Uuid, String)> {
        let q = query.trim().to_lowercase();
        let mut v: Vec<(Uuid, String)> = self
            .names
            .iter()
            .filter(|(id, n)| **id != self.me && !exclude.contains(id) && n.to_lowercase().contains(&q))
            .map(|(id, n)| (*id, n.clone()))
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v.truncate(50);
        v
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

pub fn is_image(a: &presage::proto::AttachmentPointer) -> bool {
    a.content_type.as_deref().is_some_and(|c| c.starts_with("image/"))
}

fn viewer_key(e: iced::keyboard::Event) -> Option<Message> {
    use iced::keyboard::{Event, Key, key::Named};
    match e {
        Event::KeyPressed { key: Key::Named(Named::ArrowLeft), .. } => Some(Message::ViewerStep(-1)),
        Event::KeyPressed { key: Key::Named(Named::ArrowRight), .. } => Some(Message::ViewerStep(1)),
        Event::KeyPressed { key: Key::Named(Named::Escape), .. } => Some(Message::CloseViewer),
        _ => None,
    }
}

fn open_in_browser(url: &str) {
    if let Err(e) = open::that_detached(url) {
        tracing::warn!(%e, "failed to open link");
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
    use presage::libsignal_service::groups_v2::AccessRequired;

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
        app.open_url = record_url; // ...nor launch a browser
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
            request: false,
            pinned: false,
            archived: false,
            muted: false,
            marked_unread: false,
            group: None,
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
            request: false,
            pinned: false,
            archived: false,
            muted: false,
            marked_unread: false,
            group: Some(GroupInfo {
                description: "Weekend plans".into(),
                members: vec![(uuid(ME), false), (uuid(ALICE), true), (uuid(3), false), (uuid(4), false)],
                pending: 1,
                requesting: 0,
                attributes: AccessRequired::Member,
                add_members: AccessRequired::Member,
            }),
        });
        app.update(Message::Backend(Event::Conversations(convs)));
        app.update(Message::OpenThread(group(), "Friends".into(), true));
        app.update(Message::Backend(Event::Timeline { thread: group(), timeline: Timeline::new(uuid(ME)) }));
        drain(&mut rx);
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

    thread_local! {
        pub static OPENED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn record_url(url: &str) {
        OPENED.with(|o| o.borrow_mut().push(url.to_owned()));
    }

    pub fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.update(Message::Compose(text_editor::Action::Edit(text_editor::Edit::Insert(c))));
        }
    }

    pub fn drain(rx: &mut UnboundedReceiver<Command>) -> Vec<Command> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// A 1×1 orange PNG (generated with Python's zlib; fake sticker art).
    pub const PNG: [u8; 69] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49,
        0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xe2, 0x6c, 0x06, 0x00, 0x03, 0x9c, 0x01, 0x6e, 0xa2, 0x1e, 0xd8, 0x93,
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// Installed packs "Cats" (id [8], stickers 1 and 2) and "Dogs" (id [9], sticker 3).
    pub fn fake_packs(app: &mut App, recent: Vec<(Vec<u8>, u32)>) {
        use presage::store::{Sticker, StickerPack, StickerPackManifest};
        let pack = |id: u8, title: &str, ids: &[u32]| StickerPack {
            id: vec![id],
            key: vec![id; 32],
            manifest: StickerPackManifest {
                title: title.into(),
                author: String::new(),
                cover: None,
                stickers: ids
                    .iter()
                    .map(|i| Sticker { id: *i, emoji: Some("🐱".into()), content_type: Some("image/png".into()), bytes: Some(PNG.to_vec()) })
                    .collect(),
            },
        };
        let packs = backend::picker_packs([pack(9, "Dogs", &[3]), pack(8, "Cats", &[1, 2])]);
        app.update(Message::Backend(Event::Stickers { packs, recent }));
    }

    #[test]
    fn sticker_picker_sends_optimistically_and_remembers_recents() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        fake_packs(&mut app, Vec::new());
        app.update(Message::ToggleStickers);
        assert!(matches!(drain(&mut rx).as_slice(), [Command::LoadStickers]));
        let o = app.open.as_ref().unwrap();
        assert_eq!(o.stickers, Some(StickerTab::Pack(vec![8])), "no recents yet: the first pack by title");

        app.update(Message::SendSticker(vec![8], 2));
        let cmds = drain(&mut rx);
        let [Command::SendSticker { thread, ts, sticker }] = cmds.as_slice() else { panic!("{cmds:?}") };
        assert_eq!(*thread, alice());
        assert_eq!((sticker.pack_id(), sticker.pack_key(), sticker.sticker_id(), sticker.emoji()), (&[8u8][..], &[8u8; 32][..], 2, "🐱"));
        let o = app.open.as_ref().unwrap();
        assert!(o.stickers.is_none(), "sending closes the picker");
        let m = o.timeline.as_ref().unwrap().messages.last().unwrap();
        assert_eq!((m.id, m.delivery, m.sticker.as_ref()), (*ts, Delivery::Sending, Some(sticker)));
        assert_eq!(app.recent_stickers, [(vec![8], 2)]);

        let pointer = presage::proto::AttachmentPointer { cdn_number: Some(2), ..Default::default() };
        app.update(Message::Backend(Event::Sent { thread: alice(), ts: *ts, attachments: vec![pointer], error: None }));
        assert_eq!(app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.last().unwrap().delivery, Delivery::Sent);

        app.update(Message::ToggleStickers);
        assert_eq!(app.open.as_ref().unwrap().stickers, Some(StickerTab::Recent), "recents first once there are some");
        app.update(Message::TogglePicker);
        assert!(app.open.as_ref().unwrap().stickers.is_none(), "one picker at a time");
    }

    #[test]
    fn sticker_tab_falls_back_when_its_pack_goes_away() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        fake_packs(&mut app, Vec::new());
        app.update(Message::ToggleStickers);
        app.update(Message::StickerTab(StickerTab::Pack(vec![9])));
        app.update(Message::Backend(Event::Stickers { packs: Vec::new(), recent: Vec::new() }));
        assert_eq!(app.open.as_ref().unwrap().stickers, Some(StickerTab::Recent));
        app.update(Message::SendSticker(vec![8], 1));
        assert!(!drain(&mut rx).iter().any(|c| matches!(c, Command::SendSticker { .. })), "uninstalled packs can't be sent from");
        app.update(Message::InstallPack(vec![8], vec![1]));
        assert!(matches!(drain(&mut rx).as_slice(), [Command::InstallStickerPack { id, key }] if *id == [8] && *key == [1]));
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

    /// Alice's chat plus our own message sent a minute ago; returns its id.
    pub fn with_own_message(app: &mut App, rx: &mut UnboundedReceiver<Command>, markup: &str) -> u64 {
        open_alice(app, rx);
        let ts = now_ms() - 60_000;
        let (body, body_ranges) = richtext::parse_markup(markup);
        let dm = presage::proto::DataMessage { body: Some(body), body_ranges, timestamp: Some(ts), ..Default::default() };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(ME), ts, dm)) }));
        ts
    }

    fn msgs(app: &App) -> &[Msg] {
        &app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages
    }

    #[test]
    fn edit_puts_markup_in_composer_and_sends_an_edit() {
        let (mut app, mut rx) = app();
        let id = with_own_message(&mut app, &mut rx, "*hi* 👍🏽 _there_");
        app.update(Message::Reply(uuid(ALICE), 10));
        app.update(Message::Edit(id));
        let o = app.open.as_ref().unwrap();
        assert_eq!((o.editing, o.quote.is_none()), (Some(id), true), "editing replaces the reply");
        assert_eq!(o.composer.text().trim_end(), "*hi* 👍🏽 _there_");
        type_text(&mut app, " ~now~");
        app.update(Message::Send);
        let cmds = drain(&mut rx);
        let Some(Command::Edit { target_ts, ts, body, ranges, .. }) = cmds.iter().find(|c| matches!(c, Command::Edit { .. }))
        else {
            panic!("{cmds:?}")
        };
        assert_eq!((*target_ts, body.as_str(), ranges.len()), (id, "hi 👍🏽 there now", 3));
        assert!(*ts > id, "the edit has its own timestamp");
        assert!(!cmds.iter().any(|c| matches!(c, Command::Send { .. })));
        let m = msgs(&app).iter().find(|m| m.id == id).unwrap();
        assert_eq!((m.body.as_str(), m.edited, m.delivery), ("hi 👍🏽 there now", true, Delivery::Sending));
        assert_eq!(msgs(&app).len(), 2, "no new message row");
        let o = app.open.as_ref().unwrap();
        assert!(o.editing.is_none() && o.composer.text().trim().is_empty());

        app.update(Message::Backend(Event::Sent { thread: alice(), ts: id, attachments: vec![], error: None }));
        assert_eq!(msgs(&app).iter().find(|m| m.id == id).unwrap().delivery, Delivery::Sent);
    }

    #[test]
    fn unchanged_or_cancelled_edits_send_nothing() {
        let (mut app, mut rx) = app();
        let id = with_own_message(&mut app, &mut rx, "same");
        app.update(Message::Edit(id));
        app.update(Message::Send);
        app.update(Message::Edit(id));
        type_text(&mut app, "!");
        app.update(Message::CancelEdit);
        assert!(app.open.as_ref().unwrap().composer.text().trim().is_empty());
        app.update(Message::Send);
        assert!(drain(&mut rx).iter().all(|c| matches!(c, Command::Typing { .. })));
        assert!(!msgs(&app)[1].edited);
    }

    #[test]
    fn only_recent_own_messages_can_be_edited_or_deleted_for_everyone() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(ME), 20, text(20, "old"))) }));
        app.update(Message::Edit(20));
        app.update(Message::Edit(10)); // Alice's
        assert!(app.open.as_ref().unwrap().editing.is_none());
        app.update(Message::DeleteForEveryone(20));
        assert!(!msgs(&app)[1].deleted);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn delete_for_everyone_and_for_me() {
        let (mut app, mut rx) = app();
        let id = with_own_message(&mut app, &mut rx, "oops");
        app.update(Message::Edit(id));
        app.update(Message::ToggleDelete(Some((uuid(ME), id))));
        app.update(Message::DeleteForEveryone(id));
        let o = app.open.as_ref().unwrap();
        assert!(o.deleting.is_none() && o.editing.is_none(), "menu closed, edit cancelled");
        assert!(msgs(&app)[1].deleted);
        assert!(matches!(drain(&mut rx).as_slice(), [Command::DeleteForEveryone { target_ts, .. }] if *target_ts == id));

        app.update(Message::DeleteForMe(uuid(ALICE), 10));
        assert_eq!(msgs(&app).len(), 1, "removed from the timeline");
        let cmds = drain(&mut rx);
        assert!(
            matches!(cmds.as_slice(), [Command::DeleteForMe { author, target_ts: 10, thread }] if *author == uuid(ALICE) && *thread == alice()),
            "{cmds:?}"
        );
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

    pub fn bob() -> Thread {
        Thread::Contact(ServiceId::Aci(uuid(3).into()))
    }

    /// Like `app()`, plus a message request from Bob, a stranger.
    pub fn with_request() -> (App, UnboundedReceiver<Command>) {
        let (mut app, rx) = app();
        let mut convs = app.conversations.clone();
        let mut c = convs[0].clone();
        (c.thread, c.title, c.preview, c.last_ts, c.request) = (bob(), "Bob".into(), "hey, it's Bob".into(), 20, true);
        convs.insert(0, c);
        app.update(Message::Backend(Event::Conversations(convs)));
        (app, rx)
    }

    #[test]
    fn requests_send_no_typing_or_messages_until_accepted() {
        let (mut app, mut rx) = with_request();
        app.update(Message::OpenThread(bob(), "Bob".into(), false));
        drain(&mut rx);
        type_text(&mut app, "hi");
        app.update(Message::Send);
        assert!(drain(&mut rx).is_empty(), "no typing, no send");
        app.update(Message::Chat(bob(), ChatAction::Accept));
        assert!(!app.is_request(&bob()));
        assert!(matches!(drain(&mut rx).as_slice(), [Command::Chat { action: ChatAction::Accept, .. }]));
        app.update(Message::Send);
        assert!(drain(&mut rx).iter().any(|c| matches!(c, Command::Send { .. })));
    }

    #[test]
    fn deleting_or_blocking_a_request_drops_the_chat() {
        for action in [ChatAction::Delete, ChatAction::Block] {
            let (mut app, mut rx) = with_request();
            app.update(Message::OpenThread(bob(), "Bob".into(), false));
            drain(&mut rx);
            app.update(Message::Chat(bob(), action));
            assert!(app.open.is_none() && !app.conversations.iter().any(|c| c.thread == bob()));
            assert!(matches!(drain(&mut rx).as_slice(), [Command::Chat { thread, action: a }] if *thread == bob() && *a == action));
        }
    }

    #[test]
    fn menu_actions_apply_locally_and_reach_the_backend() {
        let (mut app, mut rx) = app();
        app.update(Message::RowMenu(Some(alice())));
        assert_eq!(app.row_menu, Some(alice()));
        for action in [ChatAction::Pin(true), ChatAction::Mute(true), ChatAction::Archive(true), ChatAction::MarkUnread(true)] {
            app.update(Message::Chat(alice(), action));
        }
        assert!(app.row_menu.is_none(), "choosing closes the menu");
        let c = &app.conversations[0];
        assert!(c.pinned && c.muted && c.archived && c.marked_unread);
        assert_eq!(drain(&mut rx).len(), 4);
        app.update(Message::OpenThread(alice(), "Alice".into(), false));
        assert!(!app.conversations[0].marked_unread, "opening clears the unread mark");
    }

    #[test]
    fn at_most_four_pins() {
        let (mut app, mut rx) = app();
        let convs: Vec<Conversation> = (10..15u8)
            .map(|n| Conversation { thread: Thread::Contact(ServiceId::Aci(uuid(n).into())), pinned: n < 14, ..app.conversations[0].clone() })
            .collect();
        app.update(Message::Backend(Event::Conversations(convs)));
        app.update(Message::Chat(Thread::Contact(ServiceId::Aci(uuid(14).into())), ChatAction::Pin(true)));
        assert!(drain(&mut rx).is_empty() && app.error.as_deref() == Some("You can only pin up to 4 chats"));
    }

    #[test]
    fn window_title_counts_unread() {
        let (app, _rx) = app();
        assert_eq!(app.title(), "Signal (2)");
    }

    /// Makes us an admin of `open_group()`'s group.
    pub fn make_me_admin(app: &mut App) {
        let mut convs = app.conversations.clone();
        let g = convs.iter_mut().find_map(|c| c.group.as_mut()).unwrap();
        g.members[0].1 = true;
        app.update(Message::Backend(Event::Conversations(convs)));
    }

    #[test]
    fn group_info_panel_edits_are_permission_checked() {
        let (mut app, mut rx) = open_group();
        app.update(Message::ToggleGroupInfo);
        let p = app.open.as_ref().unwrap().info.as_ref().expect("panel open");
        assert_eq!((p.title.as_str(), p.description.as_str()), ("Friends", "Weekend plans"));
        app.update(Message::InfoTitle("Hikers".into()));
        app.update(Message::EditGroup(GroupEdit::Title("Hikers".into())));
        app.update(Message::EditGroup(GroupEdit::Remove(uuid(ALICE))));
        let cmds = drain(&mut rx);
        assert!(
            matches!(cmds.as_slice(), [Command::EditGroup { thread, edit: GroupEdit::Title(t) }] if *thread == group() && t == "Hikers"),
            "members may rename but not remove: {cmds:?}"
        );
        make_me_admin(&mut app);
        app.update(Message::EditGroup(GroupEdit::Remove(uuid(ALICE))));
        assert!(matches!(drain(&mut rx).as_slice(), [Command::EditGroup { edit: GroupEdit::Remove(_), .. }]));
        app.update(Message::InfoAddQuery("bo".into()));
        app.update(Message::EditGroup(GroupEdit::AddMembers(vec![uuid(9)])));
        assert!(app.open.as_ref().unwrap().info.as_ref().unwrap().add_query.is_empty(), "adding clears the search");
        app.update(Message::ToggleGroupInfo);
        assert!(app.open.as_ref().unwrap().info.is_none());
    }

    #[test]
    fn group_timer_is_a_group_change_when_allowed() {
        let (mut app, mut rx) = open_group();
        app.update(Message::ToggleTimerMenu);
        assert!(app.open.as_ref().unwrap().timer_menu);
        app.update(Message::SetTimer(300));
        let cmds = drain(&mut rx);
        assert!(matches!(cmds.as_slice(), [Command::EditGroup { edit: GroupEdit::Timer(300), .. }]), "{cmds:?}");
        assert_eq!(app.timer(&group()), 300);
    }

    #[test]
    fn new_group_dialog_creates_then_opens_the_group() {
        let (mut app, mut rx) = app();
        app.update(Message::NewGroup);
        app.update(Message::CreateGroup);
        assert!(drain(&mut rx).is_empty(), "a name is required");
        app.update(Message::NewGroupTitle(" Hikers ".into()));
        app.update(Message::NewGroupQuery("b".into()));
        assert_eq!(app.people_matching("b", &[]), vec![(uuid(3), "Bob".to_string())], "never us");
        for id in [ALICE, 3, 3] {
            app.update(Message::NewGroupPick(uuid(id)));
        }
        app.update(Message::CreateGroup);
        let cmds = drain(&mut rx);
        assert!(
            matches!(cmds.as_slice(), [Command::CreateGroup { title, members }] if title == "Hikers" && *members == vec![uuid(ALICE)]),
            "{cmds:?}"
        );
        assert!(app.new_group.is_none());
        app.update(Message::Backend(Event::GroupCreated { thread: group(), title: "Hikers".into() }));
        assert!(matches!(drain(&mut rx).as_slice(), [Command::Open(t)] if *t == group()));
        assert!(app.open.as_ref().is_some_and(|o| o.is_group && o.title == "Hikers"));
    }

    fn photo(digest: u8, size: u32) -> presage::proto::AttachmentPointer {
        presage::proto::AttachmentPointer {
            content_type: Some("image/jpeg".into()),
            digest: Some(vec![digest]),
            cdn_number: Some(3),
            size: Some(size),
            file_name: Some(format!("photo{digest}.jpg")),
            ..Default::default()
        }
    }

    /// Alice sends two photos in one message (ts 50), then a third too big to auto-download (ts 60).
    pub fn photos(app: &mut App) -> Vec<String> {
        let (a, b, c) = (photo(0xb1, 1000), photo(0xb2, 1000), photo(0xb3, AUTO_DOWNLOAD_BYTES + 1));
        let two = presage::proto::DataMessage { timestamp: Some(50), attachments: vec![a.clone(), b.clone()], ..Default::default() };
        let big = presage::proto::DataMessage { timestamp: Some(60), attachments: vec![c.clone()], ..Default::default() };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(ALICE), 50, two)) }));
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(ALICE), 60, big)) }));
        [a, b, c].iter().map(attachment_key).collect()
    }

    fn viewing(app: &App) -> Option<&str> {
        app.open.as_ref().and_then(|o| o.viewer.as_deref())
    }

    #[test]
    fn viewer_steps_within_bounds_fetches_and_closes() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let keys = photos(&mut app);
        let fetches = |rx: &mut UnboundedReceiver<Command>| drain(rx).into_iter().filter(|c| matches!(c, Command::FetchAttachment(_))).count();
        assert_eq!(fetches(&mut rx), 2, "the big one isn't auto-downloaded");

        app.update(Message::ViewImage(keys[0].clone()));
        app.update(Message::ViewerStep(-1));
        assert_eq!(viewing(&app), Some(keys[0].as_str()), "no wrap before the first");
        app.update(Message::ViewerStep(1));
        assert_eq!(viewing(&app), Some(keys[1].as_str()));
        assert_eq!(fetches(&mut rx), 0, "already loading");
        app.update(Message::ViewerStep(1));
        assert_eq!(viewing(&app), Some(keys[2].as_str()));
        assert_eq!(fetches(&mut rx), 1, "stepping onto an undownloaded image fetches it");
        app.update(Message::ViewerStep(1));
        assert_eq!(viewing(&app), Some(keys[2].as_str()), "no wrap after the last");

        use iced::keyboard::{Event as Key, Location, Modifiers, key::Named};
        let press = |named| Key::KeyPressed {
            key: iced::keyboard::Key::Named(named),
            modified_key: iced::keyboard::Key::Named(named),
            physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
        };
        assert!(matches!(viewer_key(press(Named::ArrowLeft)), Some(Message::ViewerStep(-1))));
        assert!(matches!(viewer_key(press(Named::ArrowRight)), Some(Message::ViewerStep(1))));
        assert!(matches!(viewer_key(press(Named::Enter)), None));
        let esc = viewer_key(press(Named::Escape)).expect("Esc closes");
        app.update(esc);
        assert_eq!(viewing(&app), None);
    }

    #[test]
    fn link_preview_images_auto_download() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let preview = presage::proto::Preview {
            url: Some("https://signal.org".into()),
            title: Some("Signal".into()),
            image: Some(photo(0xc1, 1000)),
            ..Default::default()
        };
        let dm = presage::proto::DataMessage { preview: vec![preview], ..text(70, "https://signal.org") };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(ALICE), 70, dm)) }));
        assert!(drain(&mut rx).iter().any(|c| matches!(c, Command::FetchAttachment(p) if p.digest.as_deref() == Some(&[0xc1][..]))));
    }

    #[test]
    fn only_web_links_are_opened() {
        let (mut app, _rx) = app();
        for url in ["https://signal.org/a?b=1", "javascript:alert(1)", "file:///etc/passwd", "not a url", "http://x.y"] {
            app.update(Message::OpenUrl(url.into()));
        }
        assert_eq!(OPENED.with(|o| o.borrow().clone()), ["https://signal.org/a?b=1", "http://x.y"]);
    }
}
