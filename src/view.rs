//! Rendering. Signal-Desktop's layout: conversation list left, timeline + composer right.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{Local, TimeZone};
use iced::font::{Family, Style as FontStyle, Weight};
use iced::keyboard::{self, key};
use iced::widget::text::Span;
use iced::widget::text_editor::{Binding, KeyPress};
use iced::widget::{
    Column, Space, button, center, checkbox, column, container, image, mouse_area, opaque, progress_bar, qr_code, rich_text, row,
    rule, scrollable, span, stack, text, text_editor, text_input,
};
use iced::{Alignment, Background, Border, Color, ContentFit, Element, Font, Length, Theme, color, mouse};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::data_message::Sticker;
use presage::proto::{AttachmentPointer, BodyRange, Preview};

use crate::app::{App, Attach, Message, Open, Screen, StickerTab, TIMERS, display_name, is_image};
use crate::backend::{ChatAction, Conversation, attachment_key, now_ms};
use crate::calls::{Call, CallCommand, Phase};
use crate::stories::{Story, Viewer};
use crate::timeline::{Delivery, Kind, Message as Msg, attachment_label, short_duration};
use crate::{emoji, richtext};

pub const COMPOSER: &str = "composer";
pub const STORY_REPLY: &str = "story-reply";
const BLUE: Color = color!(0x2c6bed);

pub fn signal_theme(dark: bool) -> Theme {
    use iced::theme::Palette;
    let palette = if dark {
        Palette {
            background: color!(0x121212),
            text: color!(0xe9e9e9),
            primary: BLUE,
            success: color!(0x4caf50),
            warning: color!(0xffc107),
            danger: color!(0xf44336),
        }
    } else {
        Palette {
            background: color!(0xffffff),
            text: color!(0x1b1b1b),
            primary: BLUE,
            success: color!(0x4caf50),
            warning: color!(0xffc107),
            danger: color!(0xe53935),
        }
    };
    Theme::custom(if dark { "Signal Dark" } else { "Signal Light" }, palette)
}

fn is_dark(theme: &Theme) -> bool {
    theme.palette().background.r < 0.5
}

fn muted(theme: &Theme) -> Color {
    if is_dark(theme) { color!(0x9a9a9a) } else { color!(0x6b6b6b) }
}

fn surface(theme: &Theme) -> Color {
    if is_dark(theme) { color!(0x2e2e2e) } else { color!(0xe9e9e9) }
}

const AVATAR_COLORS: [Color; 8] = [
    color!(0xe3e3fe),
    color!(0xdde7fc),
    color!(0xd8e8f0),
    color!(0xcde4cd),
    color!(0xeae0f8),
    color!(0xf5e3fe),
    color!(0xf6d8ec),
    color!(0xf5d7d7),
];

/// The chat's downloaded avatar (`seed` is its thread key), cropped round; else its initial.
fn avatar<'a>(app: &App, title: &str, seed: &str, size: f32) -> Element<'a, Message> {
    if let Some(h) = app.avatars.get(seed) {
        return image(h.clone()).width(size).height(size).content_fit(ContentFit::Cover).border_radius(size / 2.0).into();
    }
    let initial = title.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string());
    let h = seed.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    let bg = AVATAR_COLORS[h % AVATAR_COLORS.len()];
    container(text(initial.unwrap_or_else(|| "#".into())).size(size * 0.45).color(color!(0x3b3b6b)))
        .center(size)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            border: Border { radius: (size / 2.0).into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

fn short_time(ms: u64) -> String {
    let Some(t) = Local.timestamp_millis_opt(ms as i64).single() else { return String::new() };
    let now = Local::now();
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if (now.date_naive() - t.date_naive()).num_days() < 7 {
        t.format("%a").to_string()
    } else {
        t.format("%b %-d").to_string()
    }
}

fn day_label(ms: u64) -> String {
    let Some(t) = Local.timestamp_millis_opt(ms as i64).single() else { return String::new() };
    match (Local::now().date_naive() - t.date_naive()).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => t.format("%A, %B %-d, %Y").to_string(),
    }
}

fn day_of(ms: u64) -> Option<chrono::NaiveDate> {
    Local.timestamp_millis_opt(ms as i64).single().map(|t| t.date_naive())
}

fn truncate(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    match line.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None if s.lines().nth(1).is_some() => format!("{line}…"),
        None => line.to_owned(),
    }
}

pub fn view(app: &App) -> Element<'_, Message> {
    let body: Element<_> = match &app.screen {
        Screen::Connecting => center(text("Connecting…").size(18)).into(),
        Screen::Link { device_name, qr, waiting, error } => link_view(device_name, qr.as_ref(), *waiting, error.as_deref()),
        Screen::Main => row![
            sidebar(app),
            rule::vertical(1),
            match (&app.settings_panel, &app.new_group, &app.open) {
                (Some(s), ..) => settings(app, s),
                (None, Some(g), _) => new_group(app, g),
                (None, None, Some(o)) => conversation(app, o),
                (None, None, None) => center(text("Select a conversation to start messaging").style(|t: &Theme| text::Style {
                    color: Some(muted(t))
                }))
                .into(),
            }
        ]
        .into(),
    };
    let body: Element<_> = match app.open.as_ref().and_then(|o| o.viewer.as_ref()) {
        Some(key) => stack![body, opaque(lightbox(app, key))].into(),
        None => body,
    };
    let body: Element<_> = match &app.story_viewer {
        Some(v) => stack![body, opaque(story_viewer(app, v))].into(),
        None => body,
    };
    let body: Element<_> = match &app.call {
        Some(c) => column![call_bar(app, c), body].into(),
        None => body,
    };
    let body: Element<_> = match &app.notice {
        None => body,
        Some(n) => column![
            container(text(n.clone()).color(Color::WHITE))
                .padding([6, 12])
                .width(Length::Fill)
                .style(|_| container::Style { background: Some(BLUE.into()), ..Default::default() }),
            body
        ]
        .into(),
    };
    match &app.error {
        None => body,
        Some(e) => column![
            container(
                row![
                    text(e.clone()).width(Length::Fill),
                    button(text("Dismiss")).on_press(Message::DismissError).style(button::text),
                ]
                .align_y(Alignment::Center)
            )
            .padding([6, 12])
            .style(|t: &Theme| container::Style {
                background: Some(t.palette().danger.into()),
                text_color: Some(Color::WHITE),
                ..Default::default()
            }),
            body
        ]
        .into(),
    }
}

fn link_view<'a>(
    device_name: &'a str,
    qr: Option<&'a qr_code::Data>,
    waiting: bool,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let mut col = column![
        text("Link this device").size(28),
        text("On your phone open Signal → Settings → Linked devices → Link new device, then scan the code."),
    ]
    .spacing(16)
    .max_width(460)
    .align_x(Alignment::Center);
    if let Some(e) = error {
        col = col.push(text(format!("Linking failed: {e}")).style(text::danger));
    }
    col = match (qr, waiting) {
        (Some(data), _) => col
            .push(container(qr_code(data).cell_size(6).style(qr_style)).padding(24).style(|_| container::Style {
                background: Some(Color::WHITE.into()),
                ..Default::default()
            }))
            .push(text("Waiting for your phone…")),
        (None, true) => col.push(text("Requesting a link code…")),
        (None, false) => col
            .push(text_input("Device name", device_name).on_input(Message::DeviceName).on_submit(Message::Link))
            .push(button(text("Generate QR code")).on_press(Message::Link)),
    };
    center(col).padding(24).into()
}

/// Always dark-on-light: phone scanners (Signal's included) can't read inverted codes,
/// and iced's default follows the theme, which inverts it in dark mode.
fn qr_style(_: &Theme) -> qr_code::Style {
    qr_code::Style { cell: Color::BLACK, background: Color::WHITE }
}

fn chat_row<'a>(app: &'a App, c: &'a Conversation) -> Element<'a, Message> {
    let selected = app.open.as_ref().is_some_and(|o| o.thread == c.thread);
    let seed = crate::state::thread_key(&c.thread);
    let dim = |t: &Theme| text::Style { color: Some(muted(t)) };
    let mut top = row![text(c.title.clone()).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).width(Length::Fill)].spacing(4);
    if c.pinned {
        top = top.push(text("📌").size(12));
    }
    if c.muted {
        top = top.push(text("🔕").size(12));
    }
    if c.last_ts > 0 {
        top = top.push(text(short_time(c.last_ts)).size(12).style(dim));
    }
    let typing = app.typing_names(&c.thread);
    let preview = if !typing.is_empty() { "typing…".to_string() } else { truncate(&c.preview, 48) };
    let mut bottom = row![text(preview).size(13).wrapping(text::Wrapping::None).width(Length::Fill).style(dim)];
    if c.unread > 0 {
        bottom = bottom.push(
            container(text(c.unread.to_string()).size(12).color(Color::WHITE))
                .padding([1, 7])
                .style(|_| container::Style {
                    background: Some(BLUE.into()),
                    border: Border { radius: 10.0.into(), ..Default::default() },
                    ..Default::default()
                }),
        );
    } else if c.marked_unread {
        bottom = bottom.push(container(Space::new()).width(10).height(10).style(|_| container::Style {
            background: Some(BLUE.into()),
            border: Border { radius: 5.0.into(), ..Default::default() },
            ..Default::default()
        }));
    }
    let item = row![avatar(app, &c.title, &seed, 44.0), column![top, bottom].spacing(2)]
        .spacing(12)
        .align_y(Alignment::Center);
    let open = button(item)
        .width(Length::Fill)
        .padding([10, 12])
        .on_press(Message::OpenThread(c.thread.clone(), c.title.clone(), c.is_group))
        .style(move |t: &Theme, status| {
            let bg = match (selected, status) {
                (true, _) => Some(surface(t).into()),
                (_, button::Status::Hovered) => Some(surface(t).scale_alpha(0.5).into()),
                _ => None,
            };
            button::Style { background: bg, text_color: t.palette().text, ..Default::default() }
        });
    // iced has no context menu: a "⋯" on hover opens the row's actions (not for requests).
    let menu_open = app.row_menu.as_ref() == Some(&c.thread);
    let mut line = row![open].align_y(Alignment::Center);
    if !c.request && (menu_open || app.row_hovered.as_ref() == Some(&c.thread)) {
        line = line.push(button(text("⋯").size(16)).on_press(Message::RowMenu(Some(c.thread.clone()))).style(button::text));
    }
    let mut col = column![line];
    if menu_open {
        let item = |label: &'static str, action| {
            button(text(label).size(13)).on_press(Message::Chat(c.thread.clone(), action)).style(button::secondary)
        };
        let menu = row![
            item(if c.pinned { "Unpin" } else { "Pin" }, ChatAction::Pin(!c.pinned)),
            item(if c.archived { "Unarchive" } else { "Archive" }, ChatAction::Archive(!c.archived)),
            item(if c.muted { "Unmute" } else { "Mute" }, ChatAction::Mute(!c.muted)),
            item(if c.marked_unread { "Mark read" } else { "Mark unread" }, ChatAction::MarkUnread(!c.marked_unread)),
        ]
        .spacing(4);
        col = col.push(container(menu.wrap()).padding([4, 12]));
    }
    mouse_area(col).on_enter(Message::RowHover(Some(c.thread.clone()))).on_exit(Message::RowHover(None)).into()
}

fn sidebar(app: &App) -> Element<'_, Message> {
    let q = app.search.trim().to_lowercase();
    let shown = |c: &&Conversation| q.is_empty() || c.title.to_lowercase().contains(&q);
    let section = |label: String, msg| {
        button(text(label).size(13)).width(Length::Fill).padding([8, 12]).on_press(msg).style(button::text)
    };
    let mut list = Column::new();
    if app.show_archived {
        list = list.push(section("← Archived chats".into(), Message::ShowArchived(false)));
        for c in app.conversations.iter().filter(|c| c.archived && !c.request).filter(shown) {
            list = list.push(chat_row(app, c));
        }
    } else {
        let requests: Vec<&Conversation> = app.conversations.iter().filter(|c| c.request).filter(shown).collect();
        if !requests.is_empty() {
            let arrow = if app.requests_open { "▾" } else { "▸" };
            list = list.push(section(format!("{arrow} Message requests ({})", requests.len()), Message::ToggleRequests));
            if app.requests_open {
                for c in requests {
                    list = list.push(chat_row(app, c));
                }
            }
        }
        for c in app.conversations.iter().filter(|c| !c.archived && !c.request).filter(shown) {
            list = list.push(chat_row(app, c));
        }
        let archived = app.conversations.iter().filter(|c| c.archived && !c.request).count();
        if archived > 0 {
            list = list.push(section(format!("Archived chats ({archived})"), Message::ShowArchived(true)));
        }
    }
    let contacts = app.contact_matches();
    if !contacts.is_empty() {
        list = list.push(container(text("Contacts").size(13)).padding([8, 12]));
        for (id, name) in contacts {
            let thread = presage::store::Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(id.into()));
            list = list.push(
                button(row![avatar(app, &name, &id.to_string(), 36.0), text(name.clone())].spacing(12).align_y(Alignment::Center))
                    .width(Length::Fill)
                    .padding([6, 12])
                    .on_press(Message::OpenThread(thread, name, false))
                    .style(button::text),
            );
        }
    }
    if !app.results.is_empty() && app.search.trim().chars().count() >= 2 {
        list = list.push(container(text("Messages").size(13)).padding([8, 12]));
        for r in &app.results {
            let is_group = matches!(r.thread, presage::store::Thread::Group(_));
            let item = column![
                row![
                    text(r.title.clone()).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).width(Length::Fill),
                    text(short_time(r.id)).size(12).style(|t: &Theme| text::Style { color: Some(muted(t)) }),
                ],
                text(truncate(&r.snippet, 48)).size(13).wrapping(text::Wrapping::None).style(|t: &Theme| text::Style { color: Some(muted(t)) }),
            ]
            .spacing(2);
            list = list.push(
                button(item)
                    .width(Length::Fill)
                    .padding([6, 12])
                    .on_press(Message::OpenThread(r.thread.clone(), r.title.clone(), is_group))
                    .style(button::text),
            );
        }
    }
    let list = if app.stories_open { story_rows(app) } else { list };
    let unviewed = app.stories.iter().any(|s| !s.viewed && s.expires_at() > now_ms());
    let stories = match (app.stories_open, unviewed) {
        (true, _) => "Chats",
        (false, true) => "● Stories",
        (false, false) => "Stories",
    };
    column![
        container(
            row![
                text("Signal").size(22).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).width(Length::Fill),
                button(text(stories).size(13)).on_press(Message::ToggleStories).style(button::secondary),
                button(text("New group").size(13)).on_press(Message::NewGroup).style(button::secondary),
                button(text("⚙").size(18)).on_press(Message::ToggleSettings).style(button::text),
            ]
            .spacing(4)
            .align_y(Alignment::Center)
        )
        .padding([12, 16]),
        container(text_input("Search", &app.search).on_input(Message::Search).padding(8)).padding([0, 12]),
        scrollable(list).height(Length::Fill),
    ]
    .spacing(8)
    .width(320)
    .into()
}

fn story_owner_title(app: &App, owner: &presage::store::Thread) -> String {
    match owner {
        presage::store::Thread::Contact(id) => display_name(&app.names, id.raw_uuid()),
        g => app.conversations.iter().find(|c| c.thread == *g).map_or_else(|| "Group".into(), |c| c.title.clone()),
    }
}

/// The Stories tab: people and groups with stories, a blue ring while some are unviewed.
fn story_rows(app: &App) -> Column<'_, Message> {
    let entries = crate::stories::list(&app.stories, now_ms());
    if entries.is_empty() {
        return Column::new().push(container(muted_text("No stories from the last 24 hours")).padding([8, 12]));
    }
    entries.into_iter().fold(Column::new(), |col, e| {
        let title = story_owner_title(app, &e.owner);
        let unviewed = e.unviewed();
        let ring = container(avatar(app, &title, &crate::state::thread_key(&e.owner), 36.0)).padding(2).style(move |t: &Theme| container::Style {
            border: Border { radius: 22.0.into(), width: 2.0, color: if unviewed { BLUE } else { muted(t) } },
            ..Default::default()
        });
        let n = e.stories.len();
        let sub = format!("{} · {n} {}", short_time(e.newest()), if n == 1 { "story" } else { "stories" });
        let weight = if unviewed { Weight::Bold } else { Weight::Normal };
        let info = column![text(title).font(Font { weight, ..Font::DEFAULT }), muted_text(sub).size(12)];
        col.push(
            button(row![ring, info].spacing(12).align_y(Alignment::Center))
                .width(Length::Fill)
                .padding([6, 12])
                .on_press(Message::OpenStories(e.owner))
                .style(button::text),
        )
    })
}

/// Story colors are Android ARGB ints.
pub fn argb(c: u32) -> Color {
    let [a, r, g, b] = c.to_be_bytes();
    Color::from_rgba8(r, g, b, f32::from(a) / 255.0)
}

/// A text story's backdrop: its color or gradient (Signal's angles match iced's: 0° is bottom-to-top).
pub fn story_background(t: &presage::proto::TextAttachment) -> Background {
    use presage::proto::text_attachment::Background as Bg;
    match &t.background {
        Some(Bg::Color(c)) => argb(*c).into(),
        Some(Bg::Gradient(g)) => {
            let colors: Vec<u32> = if g.colors.is_empty() { g.start_color.into_iter().chain(g.end_color).collect() } else { g.colors.clone() };
            let last = colors.len().saturating_sub(1).max(1) as f32;
            let linear = colors.iter().enumerate().take(8).fold(
                iced::gradient::Linear::new(iced::Degrees(g.angle.unwrap_or(0) as f32)),
                |l, (i, c)| l.add_stop(g.positions.get(i).copied().unwrap_or(i as f32 / last), argb(*c)),
            );
            Background::Gradient(linear.into())
        }
        None => color!(0x5a5a5a).into(),
    }
}

fn story_font(t: &presage::proto::TextAttachment) -> Font {
    use presage::proto::text_attachment::Style;
    match t.text_style.and_then(|s| Style::try_from(s).ok()) {
        Some(Style::Bold) => Font { weight: Weight::Bold, ..Font::DEFAULT },
        Some(Style::Serif) => Font { family: Family::Serif, ..Font::DEFAULT },
        Some(Style::Script) => Font { family: Family::Cursive, ..Font::DEFAULT },
        Some(Style::Condensed) => Font { stretch: iced::font::Stretch::Condensed, ..Font::DEFAULT },
        _ => Font::DEFAULT,
    }
}

/// The story itself: a 9:16 text card, an image, or a video placeholder (no playback here).
fn story_card<'a>(app: &'a App, s: &'a Story) -> Element<'a, Message> {
    if let Some(t) = s.text() {
        let fg = t.text_foreground_color.map_or(Color::WHITE, argb);
        let highlight = t.text_background_color.map(argb);
        let label = container(text(t.text.clone().unwrap_or_default()).size(26).font(story_font(t)).color(fg).align_x(Alignment::Center))
            .padding([4, 8])
            .style(move |_| container::Style {
                background: highlight.map(Background::Color),
                border: Border { radius: 6.0.into(), ..Default::default() },
                ..Default::default()
            });
        let bg = story_background(t);
        return container(label)
            .padding(24)
            .width(360)
            .height(640)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .style(move |_| container::Style { background: Some(bg), border: Border { radius: 12.0.into(), ..Default::default() }, ..Default::default() })
            .into();
    }
    let Some(p) = s.file() else { return text("This story can't be shown").color(Color::WHITE).into() };
    let key = attachment_key(p);
    let shown: Element<_> = match (app.attachments.get(&key), s.is_video()) {
        (Some(Attach::Ready(path)), false) => image(image::Handle::from_path(path)).height(Length::Fill).into(),
        (Some(Attach::Ready(_)), true) => column![
            text("▶ Video").size(28).color(Color::WHITE),
            text("Video playback isn't supported yet").color(Color { a: 0.7, ..Color::WHITE }),
            button(text("Save…")).on_press(Message::SaveAttachment(key)).style(button::secondary),
        ]
        .spacing(8)
        .align_x(Alignment::Center)
        .into(),
        (Some(Attach::Failed(e)), _) => text(format!("Download failed: {e}")).color(Color::WHITE).into(),
        _ => text("Loading…").color(Color::WHITE).into(),
    };
    match p.caption.clone().filter(|c| !c.is_empty()) {
        Some(c) => column![shown, text(c).color(Color::WHITE)].spacing(8).align_x(Alignment::Center).into(),
        None => shown,
    }
}

/// Full-window story viewer: progress, author and time, ‹ ›, and a reply box when replies are on.
fn story_viewer<'a>(app: &'a App, v: &'a Viewer) -> Element<'a, Message> {
    let now = now_ms();
    let Some(s) = v.current(&app.stories, now) else { return Space::new().into() };
    let (at, n) = v.progress(&app.stories, now);
    let bars = (0..n).fold(row![].spacing(4), |r, j| {
        let a = if j <= at { 0.9 } else { 0.3 };
        r.push(container(Space::new()).width(Length::Fill).height(3).style(move |_| container::Style {
            background: Some(Color { a, ..Color::WHITE }.into()),
            border: Border { radius: 2.0.into(), ..Default::default() },
            ..Default::default()
        }))
    });
    let who = if s.author == app.me { "You".into() } else { display_name(&app.names, s.author) };
    let title = match s.group {
        Some(_) => format!("{who} · {}", story_owner_title(app, &s.owner())),
        None => who.clone(),
    };
    let top = row![
        avatar(app, &who, &s.author.to_string(), 32.0),
        text(title).color(Color::WHITE),
        text(short_time(s.ts)).size(12).color(Color { a: 0.7, ..Color::WHITE }).width(Length::Fill),
        button(text("✕").size(18)).on_press(Message::CloseStories).style(on_dark),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let arrow = |label, delta: isize| button(text(label).size(32)).on_press(Message::StoryStep(delta)).style(on_dark);
    let middle = row![arrow("‹", -1), center(story_card(app, s)), arrow("›", 1)].align_y(Alignment::Center).height(Length::Fill);
    let mut col = column![bars, top, middle].spacing(10);
    if s.allows_replies() && s.author != app.me {
        let reactions = emoji::REACTIONS
            .iter()
            .fold(row![].spacing(2), |r, e| r.push(button(text(*e).size(20)).on_press(Message::StoryReact((*e).into())).style(on_dark)));
        let input = text_input("Reply…", &v.reply).id(STORY_REPLY).on_input(Message::StoryReplyText).on_submit(Message::SendStoryReply).padding(8);
        col = col.push(row![input, reactions].spacing(8).align_y(Alignment::Center));
    }
    container(col)
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style { background: Some(Color { a: 0.95, ..Color::BLACK }.into()), ..Default::default() })
        .into()
}

fn conversation<'a>(app: &'a App, o: &'a Open) -> Element<'a, Message> {
    let typing = app.typing_names(&o.thread);
    let subtitle: Element<_> = if let (true, Some(g)) = (typing.is_empty(), app.group_info()) {
        muted_text(format!("{} members", g.members.len())).size(12).into()
    } else if typing.is_empty() {
        Space::new().into()
    } else {
        text(format!("{} typing…", typing.join(", "))).size(12).style(|t: &Theme| text::Style { color: Some(muted(t)) }).into()
    };
    let who = row![
        avatar(app, &o.title, &crate::state::thread_key(&o.thread), 32.0),
        column![text(o.title.clone()).font(Font { weight: Weight::Bold, ..Font::DEFAULT }), subtitle],
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    // Like Signal-Desktop, clicking a group's header opens its details.
    let who: Element<_> = if o.is_group {
        button(who).on_press(Message::ToggleGroupInfo).padding(0).style(button::text).into()
    } else {
        who.into()
    };
    let header = container(
        row![container(who).width(Length::Fill), call_button(app, o), timer_control(app, o)].spacing(12).align_y(Alignment::Center),
    )
    .padding([10, 16]);
    let header: Element<_> = if o.timer_menu {
        let current = app.timer(&o.thread);
        let options = TIMERS.iter().fold(row![].spacing(6), |r, &(secs, label)| {
            let style = if secs == current { button::primary } else { button::secondary };
            r.push(button(text(label).size(13)).on_press(Message::SetTimer(secs)).style(style))
        });
        column![header, container(options).padding([0, 16]).align_right(Length::Fill)].spacing(4).into()
    } else {
        header.into()
    };

    if let Some(p) = &o.info {
        return column![header, rule::horizontal(1), group_panel(app, o, p)].into();
    }
    let body: Element<_> = match &o.timeline {
        None => center(text("Loading…")).into(),
        Some(t) => {
            let start = t.messages.len().saturating_sub(o.shown);
            let mut col = Column::new().spacing(4).padding([8, 16]);
            if start > 0 {
                col = col.push(center(button(text("Load older messages")).on_press(Message::LoadOlder).style(button::text)).height(Length::Shrink));
            }
            let mut prev_day = None;
            let mut prev_author = None;
            for m in &t.messages[start..] {
                let day = day_of(m.id);
                if day != prev_day {
                    col = col.push(center(text(day_label(m.id)).size(12).style(|t: &Theme| text::Style { color: Some(muted(t)) })).height(Length::Shrink).padding(8));
                    prev_day = day;
                    prev_author = None;
                }
                let show_author = o.is_group && !m.outgoing && prev_author != Some(m.author);
                prev_author = Some(m.author);
                col = col.push(message(app, o, m, show_author));
            }
            // anchor_bottom opens at the newest message; a per-chat id gives each chat its own scroll state.
            scrollable(col).id(format!("timeline:{}", crate::state::thread_key(&o.thread))).anchor_bottom().height(Length::Fill).into()
        }
    };

    let bottom = if app.is_request(&o.thread) { request_bar(o) } else { composer(app, o) };
    column![header, rule::horizontal(1), body, bottom].into()
}

/// "📞" in a 1:1 chat's header, when this build has calls; idle while another call is up.
fn call_button<'a>(app: &App, o: &Open) -> Element<'a, Message> {
    let presage::store::Thread::Contact(peer) = &o.thread else { return Space::new().into() };
    if !crate::calls::ENABLED || app.is_request(&o.thread) {
        return Space::new().into();
    }
    let idle = app.call.as_ref().is_none_or(|c| matches!(c.phase, Phase::Ended(_)));
    let b = button(text("📞 Call").size(13)).style(button::text);
    b.on_press_maybe(idle.then(|| Message::Call(CallCommand::Start(peer.raw_uuid())))).into()
}

/// The current call, above everything: who, its state or timer, and its controls.
fn call_bar<'a>(app: &App, c: &Call) -> Element<'a, Message> {
    let who = display_name(&app.names, c.peer);
    let kind = if c.video { "video call" } else { "call" };
    let status = match (&c.phase, c.incoming) {
        (Phase::Calling, true) => format!("Incoming {kind} from {who}…"),
        (Phase::Ringing, true) => format!("Incoming {kind} from {who}"),
        (Phase::Calling, false) => format!("Calling {who}…"),
        (Phase::Ringing, false) => format!("Ringing {who}…"),
        (Phase::Connected, _) => format!("{who} · {}", crate::calls::elapsed(c.connected_at.unwrap_or(0), now_ms())),
        (Phase::Reconnecting, _) => format!("{who} · Reconnecting…"),
        (Phase::Ended(why), _) => format!("{who} · {why}"),
    };
    let act = |label: &'static str, cmd: CallCommand, style: fn(&Theme, button::Status) -> button::Style| {
        button(text(label).size(13)).on_press(Message::Call(cmd)).style(style)
    };
    let mut controls = row![].spacing(8);
    match (&c.phase, c.incoming) {
        (Phase::Ended(_), _) => controls = controls.push(button(text("Close").size(13)).on_press(Message::DismissCall).style(button::secondary)),
        (Phase::Ringing, true) => {
            controls = controls.push(act("Decline", CallCommand::Hangup, button::danger)).push(act("Accept", CallCommand::Accept, button::success));
        }
        (Phase::Calling, true) => controls = controls.push(act("Decline", CallCommand::Hangup, button::danger)),
        _ => {
            let mute = if c.muted { act("Unmute", CallCommand::Mute(false), button::primary) } else { act("Mute", CallCommand::Mute(true), button::secondary) };
            controls = controls.push(mute).push(act("Hang up", CallCommand::Hangup, button::danger));
        }
    }
    container(row![text(status).width(Length::Fill), controls].spacing(12).align_y(Alignment::Center))
        .padding([6, 12])
        .width(Length::Fill)
        .style(|t: &Theme| container::Style { background: Some(surface(t).into()), ..Default::default() })
        .into()
}

/// Shown instead of the composer until a message request is answered.
fn request_bar(o: &Open) -> Element<'_, Message> {
    let t = &o.thread;
    let choice = |label: &'static str, action, style: fn(&Theme, button::Status) -> button::Style| {
        button(text(label)).on_press(Message::Chat(t.clone(), action)).style(style)
    };
    container(
        column![
            text(format!(
                "{} isn't in your contacts. They won't know you've seen their messages until you accept.",
                o.title
            ))
            .size(13)
            .style(|t: &Theme| text::Style { color: Some(muted(t)) }),
            row![
                choice("Block", ChatAction::Block, button::danger),
                choice("Delete", ChatAction::Delete, button::secondary),
                choice("Accept", ChatAction::Accept, button::primary),
            ]
            .spacing(8),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .padding([12, 16])
    .center_x(Length::Fill)
    .into()
}

/// "⏱ 5m": opens the timer choices; read-only in groups whose details we may not edit.
fn timer_control<'a>(app: &App, o: &Open) -> Element<'a, Message> {
    let secs = app.timer(&o.thread);
    let label = TIMERS.iter().find(|(s, _)| *s == secs).map_or_else(|| short_duration(secs.into()), |(_, l)| l.to_string());
    let label = text(format!("⏱ {label}")).size(13);
    if app.can_set_timer() {
        button(label).on_press(Message::ToggleTimerMenu).style(button::text).into()
    } else {
        label.style(|t: &Theme| text::Style { color: Some(muted(t)) }).into()
    }
}

fn muted_text<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(13).style(|t: &Theme| text::Style { color: Some(muted(t)) })
}

fn badge<'a>(s: &'a str) -> Element<'a, Message> {
    container(text(s).size(11))
        .padding([1, 6])
        .style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: 8.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

/// Group details: name, description, members with admin badges and, where our role and the
/// group's access control allow, the admin actions.
fn group_panel<'a>(app: &'a App, o: &'a Open, p: &'a crate::app::GroupPanel) -> Element<'a, Message> {
    use crate::groups::GroupEdit;
    let me = app.me;
    let Some(g) = app.group_info() else { return center(text("Loading…")).into() };
    let bold = Font { weight: Weight::Bold, ..Font::DEFAULT };
    let mut col = Column::new().spacing(10).padding([16, 24]).max_width(560);
    col = col.push(row![button(text("← Back")).on_press(Message::ToggleGroupInfo).style(button::text)]);

    if g.can_edit_details(me) {
        let title = (p.title.trim() != o.title && !p.title.trim().is_empty()).then(|| Message::EditGroup(GroupEdit::Title(p.title.clone())));
        col = col.push(row![
            text_input("Group name", &p.title).on_input(Message::InfoTitle).padding(8),
            button(text("Save name")).on_press_maybe(title),
        ].spacing(8));
        let description = (p.description.trim() != g.description).then(|| Message::EditGroup(GroupEdit::Description(p.description.clone())));
        col = col.push(row![
            text_input("Description", &p.description).on_input(Message::InfoDescription).padding(8),
            button(text("Save description")).on_press_maybe(description),
        ].spacing(8));
    } else {
        col = col.push(text(o.title.clone()).size(20).font(bold));
        if !g.description.is_empty() {
            col = col.push(text(g.description.clone()));
        }
    }

    let role = if g.is_admin(me) { "Admin" } else if g.is_member(me) { "Member" } else { "Not a member" };
    col = col.push(muted_text(format!("Your role: {role}")));
    let mut counts = vec![format!("{} members", g.members.len())];
    if g.pending > 0 {
        counts.push(format!("{} invited", g.pending));
    }
    if g.requesting > 0 {
        counts.push(format!("{} requesting to join", g.requesting));
    }
    col = col.push(text(counts.join(" · ")).font(bold));

    let mut members: Vec<(Uuid, bool, String)> =
        g.members.iter().map(|(id, admin)| (*id, *admin, if *id == me { "You".into() } else { display_name(&app.names, *id) })).collect();
    members.sort_by(|a, b| (a.0 != me, &a.2).cmp(&(b.0 != me, &b.2)));
    for (id, admin, name) in members {
        let mut r = row![avatar(app, &name, &id.to_string(), 32.0), text(name).width(Length::Fill)].spacing(10).align_y(Alignment::Center);
        if admin {
            r = r.push(badge("Admin"));
        }
        let role_edit = GroupEdit::SetAdmin(id, !admin);
        if g.allows_edit(me, &role_edit) {
            let label = if admin { "Remove admin" } else { "Make admin" };
            r = r.push(button(text(label).size(13)).on_press(Message::EditGroup(role_edit)).style(button::text));
        }
        let remove = GroupEdit::Remove(id);
        if g.allows_edit(me, &remove) {
            r = r.push(button(text("Remove").size(13)).on_press(Message::EditGroup(remove)).style(button::danger));
        }
        col = col.push(r);
    }

    if g.can_add_members(me) {
        col = col.push(text_input("Add members", &p.add_query).on_input(Message::InfoAddQuery).padding(8));
        if !p.add_query.trim().is_empty() {
            let current: Vec<Uuid> = g.members.iter().map(|(id, _)| *id).collect();
            for (id, name) in app.people_matching(&p.add_query, &current).into_iter().take(8) {
                col = col.push(
                    button(row![avatar(app, &name, &id.to_string(), 28.0), text(format!("Add {name}"))].spacing(10).align_y(Alignment::Center))
                        .on_press(Message::EditGroup(GroupEdit::AddMembers(vec![id])))
                        .style(button::text),
                );
            }
        }
    }

    if g.is_member(me) {
        col = col.push(if g.must_appoint_admin(me) {
            Element::from(muted_text("Make another member an admin before leaving."))
        } else {
            button(text("Leave group")).on_press(Message::EditGroup(GroupEdit::Leave)).style(button::danger).into()
        });
    } else {
        col = col.push(muted_text("You're no longer a member of this group."));
    }
    scrollable(col).height(Length::Fill).into()
}

/// Profile, appearance, notifications and linked devices.
fn settings<'a>(app: &'a App, s: &'a crate::app::SettingsPanel) -> Element<'a, Message> {
    use crate::state::ThemeChoice;
    let bold = Font { weight: Weight::Bold, ..Font::DEFAULT };
    let heading = |t: &'static str| text(t).size(16).font(bold);
    let p = &s.profile;
    let edit = |f: fn(&mut crate::backend::OwnProfile, String)| {
        move |v: String| {
            let mut p = p.clone();
            f(&mut p, v);
            Message::EditProfile(p)
        }
    };
    let mut col = Column::new().spacing(10).padding([16, 24]).max_width(560);
    col = col.push(row![
        text("Settings").size(20).font(bold).width(Length::Fill),
        button(text("Close")).on_press(Message::ToggleSettings).style(button::text),
    ].align_y(Alignment::Center));

    col = col.push(heading("Profile"));
    col = col.push(row![
        avatar(app, &p.given, &app.me.to_string(), 56.0),
        column![
            text(if app.phone.is_empty() { "…".to_string() } else { app.phone.clone() }),
            muted_text(format!("ACI {}", app.me)),
        ].spacing(2),
    ].spacing(12).align_y(Alignment::Center));
    col = col.push(row![
        text_input("First name (required)", &p.given).id("profile-given").on_input(edit(|p, v| p.given = v)).padding(8),
        text_input("Last name", &p.family).on_input(edit(|p, v| p.family = v)).padding(8),
    ].spacing(8));
    col = col.push(row![
        text_input("😀", &p.emoji).on_input(edit(|p, v| p.emoji = v)).padding(8).width(64),
        text_input("About", &p.about).on_input(edit(|p, v| p.about = v)).padding(8),
    ].spacing(8));
    col = col.push(row![
        button(text("Save profile")).on_press_maybe(app.can_save_profile(p).then_some(Message::SaveProfile)).style(button::primary),
        muted_text("Change your photo on your phone: signal-rs can't upload one yet."),
    ].spacing(12).align_y(Alignment::Center));

    col = col.push(heading("Appearance"));
    let theme = [(ThemeChoice::System, "System"), (ThemeChoice::Light, "Light"), (ThemeChoice::Dark, "Dark")]
        .into_iter()
        .fold(row![].spacing(6), |r, (choice, label)| {
            let style = if app.settings.theme == choice { button::primary } else { button::secondary };
            r.push(button(text(label)).on_press(Message::SetTheme(choice)).style(style))
        });
    col = col.push(theme);

    col = col.push(heading("Notifications"));
    col = col.push(checkbox(app.settings.notifications).label("Show notifications").on_toggle(Message::SetNotifications));
    let text_toggle = checkbox(app.settings.notification_text).label("Show message text in notifications");
    col = col.push(if app.settings.notifications { text_toggle.on_toggle(Message::SetNotificationText) } else { text_toggle });

    col = col.push(heading("Linked devices"));
    if app.devices.is_empty() {
        col = col.push(muted_text("Loading…"));
    }
    for d in &app.devices {
        let mut r = row![text(d.name.clone()).width(Length::Fill)].spacing(8).align_y(Alignment::Center);
        if d.this {
            r = r.push(badge("This device"));
        }
        col = col.push(r);
    }
    col = col.push(if s.confirm_unlink {
        Element::from(column![
            text("Unlink this device? It stops receiving messages, and all its local data (messages, attachments, keys) is deleted.")
                .style(text::danger),
            row![
                button(text("Cancel")).on_press(Message::AskUnlink(false)).style(button::secondary),
                button(text("Unlink and delete data")).on_press(Message::ConfirmUnlink).style(button::danger),
            ].spacing(8),
        ].spacing(8))
    } else {
        button(text("Unlink this device")).on_press(Message::AskUnlink(true)).style(button::danger).into()
    });
    scrollable(col).height(Length::Fill).into()
}

/// "New group": name it and pick members from contacts.
fn new_group<'a>(app: &'a App, g: &'a crate::app::NewGroup) -> Element<'a, Message> {
    let mut col = Column::new().spacing(10).padding([16, 24]).max_width(560);
    col = col.push(row![
        text("New group").size(20).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).width(Length::Fill),
        button(text("Cancel")).on_press(Message::NewGroup).style(button::text),
    ].align_y(Alignment::Center));
    col = col.push(text_input("Group name (required)", &g.title).on_input(Message::NewGroupTitle).padding(8));
    if !g.picked.is_empty() {
        let names: Vec<String> = g.picked.iter().map(|id| display_name(&app.names, *id)).collect();
        col = col.push(muted_text(format!("Members: {}", names.join(", "))));
    }
    col = col.push(text_input("Search contacts", &g.query).on_input(Message::NewGroupQuery).padding(8));
    for (id, name) in app.people_matching(&g.query, &[]) {
        let mark = if g.picked.contains(&id) { "☑" } else { "☐" };
        col = col.push(
            button(row![text(mark), avatar(app, &name, &id.to_string(), 28.0), text(name)].spacing(10).align_y(Alignment::Center))
                .width(Length::Fill)
                .on_press(Message::NewGroupPick(id))
                .style(button::text),
        );
    }
    let create = (!g.title.trim().is_empty()).then_some(Message::CreateGroup);
    col = col.push(button(text("Create")).on_press_maybe(create).style(button::primary));
    scrollable(col).height(Length::Fill).into()
}

fn rich<'a>(
    body: &str,
    ranges: &[BodyRange],
    names: &HashMap<Uuid, String>,
    msg_id: u64,
    revealed: &HashSet<(u64, usize)>,
    fg: Color,
) -> Element<'a, Message> {
    if ranges.is_empty() && emoji::is_jumbo(body) {
        return text(body.trim().to_owned()).size(44).into();
    }
    rich_text(spans(body, ranges, names, msg_id, revealed, fg)).on_link_click(|m| m).into()
}

/// One span per styled segment; visible URLs in plain (non-mention) text become their own
/// underlined spans that open the link.
fn spans<'a>(
    body: &str,
    ranges: &[BodyRange],
    names: &HashMap<Uuid, String>,
    msg_id: u64,
    revealed: &HashSet<(u64, usize)>,
    fg: Color,
) -> Vec<Span<'a, Message>> {
    richtext::segments(body, ranges)
        .into_iter()
        .enumerate()
        .flat_map(|(i, seg)| {
            let s = &seg.style;
            let font = Font {
                weight: if s.bold || seg.mention.is_some() { Weight::Bold } else { Weight::Normal },
                style: if s.italic { FontStyle::Italic } else { FontStyle::Normal },
                family: if s.mono { Family::Monospace } else { Family::SansSerif },
                ..Font::DEFAULT
            };
            let content = match &seg.mention {
                Some(aci) => {
                    let name = Uuid::parse_str(aci).map(|id| display_name(names, id)).unwrap_or_else(|_| aci.clone());
                    format!("@{name}")
                }
                None => seg.text,
            };
            let strike = s.strike;
            let mk = move |t: String| span(t).font(font).strikethrough(strike);
            if s.spoiler && !revealed.contains(&(msg_id, i)) {
                // Hidden: text drawn in its own background color until clicked.
                let hide = Color { a: 1.0, ..fg };
                return vec![mk(content).color(hide).background(hide).link(Message::Spoiler(msg_id, i))];
            }
            let links = if seg.mention.is_some() { Vec::new() } else { richtext::links(&content) };
            let mut out = Vec::new();
            let mut at = 0;
            for r in links {
                if at < r.start {
                    out.push(mk(content[at..r.start].to_owned()));
                }
                let url = content[r.clone()].to_owned();
                out.push(mk(url.clone()).underline(true).link(Message::OpenUrl(url)));
                at = r.end;
            }
            if at < content.len() || out.is_empty() {
                out.push(mk(content[at..].to_owned()));
            }
            out
        })
        .collect()
}

fn delivery_mark(d: Delivery) -> &'static str {
    match d {
        Delivery::Failed => "⚠ Not sent",
        Delivery::Sending => "◷",
        Delivery::Sent => "✓",
        Delivery::Delivered | Delivery::Read => "✓✓",
    }
}

/// Inline player: play/pause, progress and time; the first press downloads.
fn audio_player<'a>(app: &'a App, a: &AttachmentPointer, key: String, fg: Color) -> Element<'a, Message> {
    let now = std::time::Instant::now();
    let total = app.durations.get(&key).copied();
    let playing = app.playback.as_ref().filter(|p| p.key == key);
    let pos = playing.map(|p| p.position(now)).unwrap_or_default();
    let (icon, press) = match app.attachments.get(&key) {
        Some(Attach::Loading) => ("…", None),
        _ if a.cdn_number.is_none() && a.attachment_identifier.is_none() && !app.attachments.contains_key(&key) => ("▶", None),
        _ if playing.is_some_and(|p| !p.is_paused()) => ("⏸", Some(Message::PlayPause(key.clone()))),
        _ => ("▶", Some(Message::PlayPause(key.clone()))),
    };
    let time = match (playing, total) {
        (Some(_), Some(t)) => format!("{} / {}", crate::audio::mmss(pos.min(t)), crate::audio::mmss(t)),
        (Some(_), None) => crate::audio::mmss(pos),
        (None, Some(t)) => crate::audio::mmss(t),
        (None, None) => "--:--".into(),
    };
    let frac = match total {
        Some(t) if !t.is_zero() => (pos.as_secs_f32() / t.as_secs_f32()).min(1.0),
        _ => 0.0,
    };
    let mut r = row![
        button(text(icon).size(16)).on_press_maybe(press).style(button::secondary),
        progress_bar(0.0..=1.0, frac).length(140).girth(4),
        text(time).size(12).color(fg),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let voice = a.flags.unwrap_or(0) & presage::proto::attachment_pointer::Flags::VoiceMessage as u32 != 0;
    if !voice && a.file_name.is_some() {
        r = r.push(text(attachment_label(a)).size(12).color(fg));
    }
    match app.attachments.get(&key) {
        Some(Attach::Failed(e)) => column![r, text(format!("Download failed: {e}")).size(12).color(fg)].into(),
        _ => r.into(),
    }
}

fn attachment<'a>(app: &'a App, a: &AttachmentPointer, fg: Color) -> Element<'a, Message> {
    let key = attachment_key(a);
    if a.content_type.as_deref().is_some_and(|c| c.starts_with("audio/")) {
        return audio_player(app, a, key, fg);
    }
    let is_image = a.content_type.as_deref().is_some_and(|c| c.starts_with("image/"));
    match app.attachments.get(&key) {
        Some(Attach::Ready(path)) if is_image => clickable_image(key, path, Length::Shrink, ContentFit::Contain),
        Some(Attach::Ready(_)) => row![
            text(attachment_label(a)).color(fg),
            button(text("Save…")).on_press(Message::SaveAttachment(key)).style(button::secondary),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
        Some(Attach::Loading) => text(format!("{} — downloading…", attachment_label(a))).color(fg).into(),
        Some(Attach::Failed(e)) => column![
            text(attachment_label(a)).color(fg),
            text(format!("Download failed: {e}")).size(12).color(fg),
            button(text("Retry")).on_press(Message::FetchAttachment(key)).style(button::secondary),
        ]
        .into(),
        None if a.cdn_number.is_none() && a.attachment_identifier.is_none() => {
            // Our own just-sent attachment, not uploaded yet.
            text(attachment_label(a)).color(fg).into()
        }
        None => row![
            text(attachment_label(a)).color(fg),
            button(text("Download")).on_press(Message::FetchAttachment(key)).style(button::secondary),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
    }
}

/// Signal's sticker size in the timeline.
const STICKER: f32 = 128.0;

/// Id of a sticker picker cell, for tests.
fn sticker_cell_id(pack: &[u8], id: u32) -> String {
    format!("sticker-{}-{id}", hex::encode(pack))
}

/// The installed pack's image, else the message's downloaded copy; an uninstalled pack offers installing.
fn sticker<'a>(app: &'a App, m: &'a Msg, s: &'a Sticker, fg: Color) -> Element<'a, Message> {
    let pack = app.packs.iter().find(|p| p.id == s.pack_id());
    let installed = pack.and_then(|p| p.stickers.iter().find(|x| x.id == s.sticker_id())).map(|x| x.image.clone());
    let downloaded = m.attachments.last().and_then(|a| match app.attachments.get(&attachment_key(a)) {
        Some(Attach::Ready(p)) => Some(image::Handle::from_path(p)),
        _ => None,
    });
    let mut col = Column::new().spacing(4);
    col = match (installed.or(downloaded), m.attachments.last()) {
        (Some(h), _) => col.push(image(h).width(STICKER).height(STICKER)),
        (None, Some(a)) => col.push(attachment(app, a, fg)),
        (None, None) => col.push(text(m.preview()).color(fg)),
    };
    if pack.is_none() && s.pack_key.is_some() {
        col = col.push(
            button(text("Install pack").size(13))
                .on_press(Message::InstallPack(s.pack_id().to_vec(), s.pack_key().to_vec()))
                .style(button::secondary),
        );
    }
    col.into()
}

/// Square thumbnails for messages with several images.
const THUMB: f32 = 200.0;

/// An inline image that opens the viewer; its container id lets tests click it.
fn clickable_image<'a>(key: String, path: &std::path::Path, size: Length, fit: ContentFit) -> Element<'a, Message> {
    mouse_area(container(image(image::Handle::from_path(path)).width(size).height(size).content_fit(fit)).id(format!("image:{key}")))
        .on_press(Message::ViewImage(key))
        .interaction(mouse::Interaction::Pointer)
        .into()
}

/// Several images in one message: a grid, two per row.
fn image_grid<'a>(app: &'a App, images: &[&'a AttachmentPointer], fg: Color) -> Element<'a, Message> {
    images
        .chunks(2)
        .fold(Column::new().spacing(4), |c, pair| {
            c.push(pair.iter().fold(row![].spacing(4), |r, a| {
                let key = attachment_key(a);
                r.push(match app.attachments.get(&key) {
                    Some(Attach::Ready(path)) => clickable_image(key, path, Length::Fixed(THUMB), ContentFit::Cover),
                    _ => container(attachment(app, a, fg)).center(THUMB).into(),
                })
            }))
        })
        .into()
}

/// A received link preview: image (once downloaded), title, description and domain; opens the link.
fn preview_card<'a>(app: &'a App, p: &'a Preview, fg: Color) -> Option<Element<'a, Message>> {
    let url = p.url.as_deref()?;
    let domain = url::Url::parse(url).ok().filter(|u| matches!(u.scheme(), "http" | "https"))?.host_str()?.to_owned();
    let mut col = Column::new().spacing(2);
    if let Some(Attach::Ready(path)) = p.image.as_ref().and_then(|i| app.attachments.get(&attachment_key(i))) {
        col = col.push(image(image::Handle::from_path(path)).width(Length::Fill).height(160).content_fit(ContentFit::Cover));
    }
    if let Some(t) = p.title.as_deref().filter(|t| !t.trim().is_empty()) {
        col = col.push(text(t).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).color(fg));
    }
    if let Some(d) = p.description.as_deref().filter(|d| !d.trim().is_empty()) {
        col = col.push(text(truncate(d, 140)).size(13).color(fg));
    }
    col = col.push(text(domain).size(12).color(Color { a: 0.7, ..fg }));
    let card = container(col).padding([6, 8]).width(320).style(|_| container::Style {
        background: Some(Color { a: 0.18, ..Color::BLACK }.into()),
        border: Border { radius: 8.0.into(), ..Default::default() },
        ..Default::default()
    });
    Some(mouse_area(card).on_press(Message::OpenUrl(url.to_owned())).interaction(mouse::Interaction::Pointer).into())
}

/// White-on-dark buttons for the viewer's backdrop.
fn on_dark(_: &Theme, status: button::Status) -> button::Style {
    let a = match status {
        button::Status::Disabled => 0.3,
        button::Status::Hovered => 1.0,
        _ => 0.85,
    };
    button::Style { text_color: Color { a, ..Color::WHITE }, ..Default::default() }
}

/// Full-window image viewer over the app: caption, Save…, ✕, and arrows through the chat's images.
fn lightbox<'a>(app: &'a App, key: &'a str) -> Element<'a, Message> {
    let images = app.images();
    let i = images.iter().position(|(_, a)| attachment_key(a) == key);
    let caption = i.map_or_else(String::new, |i| {
        let (m, a) = images[i];
        let who = if m.author == app.me { "You".into() } else { display_name(&app.names, m.author) };
        let when = Local.timestamp_millis_opt(m.id as i64).single().map(|t| t.format("%b %-d, %Y %H:%M").to_string());
        [Some(who), when, a.file_name.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ")
    });
    let mut top = row![text(caption).color(Color::WHITE).width(Length::Fill)].spacing(8).align_y(Alignment::Center);
    let shown: Element<_> = match app.attachments.get(key) {
        Some(Attach::Ready(path)) => {
            top = top.push(button(text("Save…")).on_press(Message::SaveAttachment(key.to_owned())).style(button::secondary));
            image(image::Handle::from_path(path)).width(Length::Fill).height(Length::Fill).into()
        }
        Some(Attach::Failed(e)) => text(format!("Download failed: {e}")).color(Color::WHITE).into(),
        _ => text("Loading…").color(Color::WHITE).into(),
    };
    top = top.push(button(text("✕").size(18)).on_press(Message::CloseViewer).style(on_dark));
    let arrow = |label, delta: isize, ok: bool| button(text(label).size(32)).on_press_maybe(ok.then_some(Message::ViewerStep(delta))).style(on_dark);
    let has_prev = i.is_some_and(|i| i > 0);
    let has_next = i.is_some_and(|i| i + 1 < images.len());
    let middle = row![arrow("‹", -1, has_prev), center(shown), arrow("›", 1, has_next)].align_y(Alignment::Center).height(Length::Fill);
    container(column![top, middle].spacing(8))
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style { background: Some(Color { a: 0.92, ..Color::BLACK }.into()), ..Default::default() })
        .into()
}

fn message<'a>(app: &'a App, o: &'a Open, m: &'a Msg, show_author: bool) -> Element<'a, Message> {
    let notice = match &m.kind {
        Kind::Normal => None,
        Kind::System(s) => Some(s.clone()),
        Kind::Group(_) => Some(m.notice_lines(app.me, &app.names).join("\n")),
    };
    if let Some(s) = notice {
        return center(text(s).size(13).align_x(Alignment::Center).style(|t: &Theme| text::Style { color: Some(muted(t)) }))
            .height(Length::Shrink)
            .padding(6)
            .into();
    }
    let key = (m.author, m.id);
    let is_sticker = m.sticker.is_some() && !m.deleted;
    // Stickers have no bubble, so their footer sits on the timeline background.
    let fg = if m.outgoing && !is_sticker { Color::WHITE } else { app.theme().palette().text };
    let mut bubble = Column::new().spacing(4);
    if show_author {
        bubble = bubble.push(text(display_name(&app.names, m.author)).size(13).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).color(color!(0x7aa5f7)));
    }
    if m.deleted {
        bubble = bubble.push(text("This message was deleted.").font(Font { style: FontStyle::Italic, ..Font::DEFAULT }).color(fg));
    } else {
        if let Some(label) = m.story_reply {
            bubble = bubble.push(
                container(text(label).size(12).font(Font { style: FontStyle::Italic, ..Font::DEFAULT }).color(fg))
                    .padding([4, 8])
                    .style(move |_| container::Style {
                        background: Some(Color { a: 0.18, ..Color::BLACK }.into()),
                        border: Border { radius: 8.0.into(), width: 0.0, color: Color::TRANSPARENT },
                        ..Default::default()
                    }),
            );
        }
        if let Some(q) = &m.quote {
            let who = match q.author {
                Some(a) if a == app.me => "You".to_string(),
                Some(a) => display_name(&app.names, a),
                None => "Unknown".into(),
            };
            bubble = bubble.push(
                container(column![
                    text(who).size(12).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).color(fg),
                    text(truncate(&q.text, 80)).size(13).color(fg),
                ])
                .padding([4, 8])
                .style(move |_| container::Style {
                    background: Some(Color { a: 0.18, ..Color::BLACK }.into()),
                    border: Border { radius: 8.0.into(), width: 0.0, color: Color::TRANSPARENT },
                    ..Default::default()
                }),
            );
        }
        if let Some(s) = &m.sticker {
            bubble = bubble.push(sticker(app, m, s, fg));
        } else {
            let images: Vec<&AttachmentPointer> = m.attachments.iter().filter(|a| is_image(a)).collect();
            if images.len() > 1 {
                bubble = bubble.push(image_grid(app, &images, fg));
            }
            for a in m.attachments.iter().filter(|a| images.len() < 2 || !is_image(a)) {
                bubble = bubble.push(attachment(app, a, fg));
            }
        }
        for card in m.previews.iter().filter_map(|p| preview_card(app, p, fg)) {
            bubble = bubble.push(card);
        }
        if !m.body.is_empty() {
            bubble = bubble.push(container(rich(&m.body, &m.ranges, &app.names, m.id, &o.spoilers, fg)).style(move |_| container::Style {
                text_color: Some(fg),
                ..Default::default()
            }));
        }
    }
    let mut footer = row![text(short_time(m.id)).size(11).color(Color { a: 0.7, ..fg })].spacing(6);
    if m.edited && !m.deleted {
        footer = footer.push(text("Edited").size(11).color(Color { a: 0.7, ..fg }));
    }
    if let Some(at) = o.expires.get(&key) {
        let left = short_duration(crate::state::remaining_s(*at, crate::backend::now_ms()));
        footer = footer.push(text(format!("⏳ {left}")).size(11).color(Color { a: 0.7, ..fg }));
    } else if m.expire_timer > 0 {
        footer = footer.push(text("⏳").size(11).color(Color { a: 0.7, ..fg })); // starts when read
    }
    if m.outgoing {
        let mark_color = if m.delivery == Delivery::Read { color!(0xa8d0ff) } else { Color { a: 0.8, ..fg } };
        footer = footer.push(text(delivery_mark(m.delivery)).size(11).color(mark_color));
    }
    bubble = bubble.push(footer);

    let bubble = container(bubble).padding([8, 12]).max_width(520).style(move |t: &Theme| container::Style {
        background: if is_sticker {
            None
        } else if m.outgoing {
            Some(Background::Color(BLUE))
        } else {
            Some(surface(t).into())
        },
        border: Border { radius: 18.0.into(), ..Default::default() },
        ..Default::default()
    });

    let now = now_ms();
    let hovered = o.hovered == Some(key) || o.reacting == Some(key) || o.deleting == Some(key);
    let actions: Element<_> = if hovered && !m.deleted {
        let mut r = row![
            button(text("☺").size(16)).on_press(Message::ToggleReactions(Some(key))).style(button::text),
            button(text("↩").size(16)).on_press(Message::Reply(m.author, m.id)).style(button::text),
        ];
        if m.can_edit(now) {
            r = r.push(button(text("✎").size(16)).on_press(Message::Edit(m.id)).style(button::text));
        }
        r.push(button(text("🗑").size(16)).on_press(Message::ToggleDelete(Some(key))).style(button::text)).into()
    } else {
        Space::new().width(72).into()
    };

    let mut col = column![if m.outgoing { row![actions, bubble] } else { row![bubble, actions] }.align_y(Alignment::Center)];
    if o.deleting == Some(key) {
        let mut bar = row![].spacing(6).align_y(Alignment::Center);
        if m.can_delete_for_everyone(now) {
            bar = bar.push(button(text("Delete for everyone").size(13)).on_press(Message::DeleteForEveryone(m.id)).style(button::danger));
        }
        bar = bar
            .push(button(text("Delete for me").size(13)).on_press(Message::DeleteForMe(m.author, m.id)).style(button::secondary))
            .push(button(text("Cancel").size(13)).on_press(Message::ToggleDelete(None)).style(button::text));
        col = col.push(container(bar).padding(4).style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: 12.0.into(), ..Default::default() },
            ..Default::default()
        }));
    }
    if o.reacting == Some(key) {
        let bar = emoji::REACTIONS
            .iter()
            .fold(row![].spacing(2), |r, e| r.push(button(text(*e).size(22)).on_press(Message::React(m.author, m.id, (*e).into())).style(button::text)));
        col = col.push(container(bar).padding(4).style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: 20.0.into(), ..Default::default() },
            ..Default::default()
        }));
    }
    if !m.reactions.is_empty() {
        let mut counts: BTreeMap<&str, (usize, bool)> = BTreeMap::new();
        for (who, e) in &m.reactions {
            let c = counts.entry(e.as_str()).or_default();
            c.0 += 1;
            c.1 |= *who == app.me;
        }
        let pills = counts.into_iter().fold(row![].spacing(4), |r, (e, (n, mine))| {
            let label = if n > 1 { format!("{e} {n}") } else { e.to_owned() };
            r.push(button(text(label).size(14)).padding([2, 8]).on_press(Message::React(m.author, m.id, e.to_owned())).style(move |t: &Theme, _| button::Style {
                background: Some(if mine { BLUE.scale_alpha(0.35).into() } else { surface(t).into() }),
                text_color: t.palette().text,
                border: Border { radius: 12.0.into(), width: 1.0, color: t.palette().background },
                ..Default::default()
            }))
        });
        col = col.push(pills);
    }
    let col = col.spacing(2).align_x(if m.outgoing { Alignment::End } else { Alignment::Start });
    mouse_area(container(col).width(Length::Fill).align_x(if m.outgoing { Alignment::End } else { Alignment::Start }))
        .on_enter(Message::Hover(Some(key)))
        .on_exit(Message::Hover(None))
        .into()
}

fn composer<'a>(app: &'a App, o: &'a Open) -> Element<'a, Message> {
    let mut col = Column::new().spacing(6).padding([8, 16]);
    if let Some(p) = &o.picker {
        col = col.push(emoji_picker(p));
    }
    if let Some(tab) = &o.stickers {
        col = col.push(sticker_picker(app, tab));
    }
    if let Some(q) = &o.quote {
        let who = if q.author == app.me { "You".into() } else { display_name(&app.names, q.author) };
        col = col.push(
            container(row![
                column![text(format!("Replying to {who}")).size(12).font(Font { weight: Weight::Bold, ..Font::DEFAULT }), text(truncate(&q.text, 90)).size(13)].width(Length::Fill),
                button(text("✕")).on_press(Message::CancelReply).style(button::text),
            ])
            .padding([4, 10])
            .style(|t: &Theme| container::Style {
                background: Some(surface(t).into()),
                border: Border { radius: 8.0.into(), ..Default::default() },
                ..Default::default()
            }),
        );
    }
    if let Some(id) = o.editing {
        let original = o.timeline.as_ref().and_then(|t| t.find(app.me, id)).map(Msg::preview).unwrap_or_default();
        col = col.push(
            container(row![
                column![text("Editing message").size(12).font(Font { weight: Weight::Bold, ..Font::DEFAULT }), text(truncate(&original, 90)).size(13)].width(Length::Fill),
                button(text("✕")).on_press(Message::CancelEdit).style(button::text),
            ])
            .padding([4, 10])
            .style(|t: &Theme| container::Style {
                background: Some(surface(t).into()),
                border: Border { radius: 8.0.into(), ..Default::default() },
                ..Default::default()
            }),
        );
    }
    if !o.files.is_empty() {
        let chips = o.files.iter().enumerate().fold(row![].spacing(6), |r, (i, f)| {
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            r.push(button(text(format!("📎 {name}  ✕")).size(13)).on_press(Message::RemoveFile(i)).style(button::secondary))
        });
        col = col.push(chips.wrap());
    }
    let matches = app.mention_matches();
    if !matches.is_empty() {
        let list = matches.into_iter().fold(Column::new(), |c, (id, name)| {
            c.push(
                button(row![avatar(app, &name, &id.to_string(), 24.0), text(name.clone())].spacing(8).align_y(Alignment::Center))
                    .width(Length::Fill)
                    .padding([4, 8])
                    .on_press(Message::Mention(id))
                    .style(button::text),
            )
        });
        col = col.push(container(list).padding(4).style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: 8.0.into(), ..Default::default() },
            ..Default::default()
        }));
    }
    let draft = o.composer.text();
    let (body, ranges) = richtext::parse_markup(&emoji::replace_shortcodes(draft.trim()));
    if !ranges.is_empty() {
        col = col.push(
            container(rich(&body, &ranges, &app.names, 0, &HashSet::new(), app.theme().palette().text))
                .padding([2, 4])
                .style(|t: &Theme| container::Style { text_color: Some(muted(t)), ..Default::default() }),
        );
    }
    if let Some(started) = o.recording {
        return col
            .push(
                row![
                    text("🔴").size(14),
                    text(format!("Recording  {}", crate::audio::mmss(started.elapsed()))).width(Length::Fill),
                    button(text("Cancel")).on_press(Message::CancelRecording).style(button::secondary),
                    button(text("Send")).on_press(Message::SendRecording),
                ]
                .spacing(8)
                .padding([8, 0])
                .align_y(Alignment::Center),
            )
            .into();
    }
    let editor = text_editor(&o.composer)
        .id(COMPOSER)
        .placeholder("Message  (*bold* _italic_ ~strike~ `mono` ||spoiler|| :emoji:)")
        .on_action(Message::Compose)
        .key_binding(|kp: KeyPress| {
            let enter = matches!(kp.key, keyboard::Key::Named(key::Named::Enter));
            if enter && !kp.modifiers.shift() {
                Some(Binding::Custom(Message::Send))
            } else {
                Binding::from_key_press(kp)
            }
        })
        .padding(10)
        .height(Length::Shrink);
    col.push(
        row![
            button(text("📎").size(18)).on_press(Message::Attach).style(button::text),
            button(text("😀").size(18)).on_press(Message::TogglePicker).style(button::text),
            button(text("🗒").size(18)).on_press(Message::ToggleStickers).style(button::text),
            container(editor).max_height(180).width(Length::Fill),
            if draft.trim().is_empty() && o.files.is_empty() {
                button(text("🎤").size(18)).on_press(Message::Record).style(button::text)
            } else {
                button(text("Send")).on_press(Message::Send)
            },
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .into()
}

fn emoji_picker(p: &crate::app::Picker) -> Element<'_, Message> {
    let tabs = emojis::Group::iter()
        .fold(row![].spacing(2), |r, g| {
            let icon = g.emojis().next().map_or("?", |e| e.as_str());
            r.push(button(text(icon).size(18)).on_press(Message::PickerGroup(g)).style(move |t: &Theme, s| {
                let mut st = button::text(t, s);
                if g == p.group && p.query.is_empty() {
                    st.background = Some(surface(t).into());
                }
                st
            }))
        });
    let grid = emoji::search(&p.query, p.group)
        .into_iter()
        .take(600)
        .fold(row![].spacing(2), |r, e| {
            r.push(button(text(e.as_str()).size(22)).padding(2).on_press(Message::InsertEmoji(e.as_str())).style(button::text))
        })
        .wrap();
    container(column![
        text_input("Search emoji", &p.query).on_input(Message::PickerQuery).padding(6),
        tabs,
        scrollable(grid).height(220),
    ]
    .spacing(6))
    .padding(8)
    .style(|t: &Theme| container::Style {
        background: Some(surface(t).into()),
        border: Border { radius: 12.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

fn sticker_picker<'a>(app: &'a App, tab: &StickerTab) -> Element<'a, Message> {
    let tab_style = |selected: bool| {
        move |t: &Theme, s| {
            let mut st = button::text(t, s);
            if selected {
                st.background = Some(t.palette().background.into());
            }
            st
        }
    };
    let mut tabs = row![button(text("🕘").size(18)).on_press(Message::StickerTab(StickerTab::Recent)).style(tab_style(*tab == StickerTab::Recent))]
        .spacing(2)
        .align_y(Alignment::Center);
    for p in &app.packs {
        let selected = *tab == StickerTab::Pack(p.id.clone());
        // Signal shows the pack's cover; its first sticker is close enough.
        let icon: Element<_> = match p.stickers.first() {
            Some(s) => image(s.image.clone()).width(24).height(24).into(),
            None => text(p.title.clone()).size(13).into(),
        };
        tabs = tabs.push(button(icon).padding(4).on_press(Message::StickerTab(StickerTab::Pack(p.id.clone()))).style(tab_style(selected)));
    }
    let cells: Vec<(&[u8], &crate::backend::PackSticker)> = match tab {
        StickerTab::Recent => app
            .recent_stickers
            .iter()
            .filter_map(|(pack, id)| {
                let p = app.packs.iter().find(|p| p.id == *pack)?;
                Some((p.id.as_slice(), p.stickers.iter().find(|s| s.id == *id)?))
            })
            .collect(),
        StickerTab::Pack(id) => app.packs.iter().filter(|p| p.id == *id).flat_map(|p| p.stickers.iter().map(|s| (p.id.as_slice(), s))).collect(),
    };
    let title = match tab {
        StickerTab::Recent if cells.is_empty() => "No recent stickers",
        StickerTab::Recent => "Recently used",
        StickerTab::Pack(id) => app.packs.iter().find(|p| p.id == *id).map_or("", |p| p.title.as_str()),
    };
    let grid = cells
        .into_iter()
        .fold(row![].spacing(4), |r, (pack, s)| {
            let cell = container(image(s.image.clone()).width(72).height(72)).id(sticker_cell_id(pack, s.id));
            r.push(button(cell).padding(2).on_press(Message::SendSticker(pack.to_vec(), s.id)).style(button::text))
        })
        .wrap();
    container(column![tabs, text(title).size(13).style(|t: &Theme| text::Style { color: Some(muted(t)) }), scrollable(grid).height(240)].spacing(6))
        .width(Length::Fill)
        .padding(8)
        .style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: 12.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use crate::app::tests::{alice, app, drain, fake_audio, fake_packs, open_alice, voice_note};
    use crate::backend::{Command, Event};
    use crate::timeline::tests::{content, uuid};
    use iced_test::simulator;

    #[test]
    fn qr_is_dark_on_light_in_every_theme() {
        for dark in [true, false] {
            let s = qr_style(&signal_theme(dark));
            assert_eq!((s.cell, s.background), (Color::BLACK, Color::WHITE));
        }
    }

    /// Writes `snapshots/link-dark-*.png`; `scripts/decode_qr.py` checks a phone could read it.
    #[test]
    fn link_screen_snapshot() {
        let (mut app, _rx) = app();
        app.update(Message::Backend(Event::NeedsLink));
        app.update(Message::Link);
        app.update(Message::Backend(Event::LinkUrl("sgnl://linkdevice?uuid=AbC123&pub_key=BQ%2Bxyz".into())));
        app.dark = true;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("link-dark") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("link-dark")).expect("write png"));
    }

    /// Writes `snapshots/long-thread-*.png`: a fresh 60-message chat must open at the newest message.
    #[test]
    fn long_thread_opens_at_newest() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let mut t = crate::timeline::Timeline::new(uuid(1));
        for i in 1..=60u64 {
            t.apply(&content(uuid(2), i * 60_000, crate::timeline::tests::text(i * 60_000, &format!("fake message {i}"))));
        }
        app.update(Message::Backend(Event::Timeline { thread: alice(), timeline: t }));
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("long-thread") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("long-thread")).expect("write png"));
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("line1\nline2", 10), "line1…");
        assert_eq!(truncate("ééééé", 2), "éé…");
    }

    #[test]
    fn sidebar_click_opens_conversation() {
        let (mut app, mut rx) = app();
        let mut ui = simulator(view(&app));
        assert!(ui.find("hi there").is_ok(), "preview shown");
        assert!(ui.find("2").is_ok(), "unread badge shown");
        ui.click("Alice").expect("conversation listed");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(m, Message::OpenThread(t, ..) if *t == alice())));
        for m in msgs {
            let _ = app.update(m);
        }
        assert!(matches!(rx.try_recv(), Ok(crate::backend::Command::Open(_))));
    }

    /// Alice's conversation with rich text, a quote, a reaction, jumbo emoji and receipts.
    fn demo() -> App {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let (body, ranges) = richtext::parse_markup("*bold*, _italic_, ~strike~, `mono` and ||secret|| 🎉");
        let dm = presage::proto::DataMessage {
            body: Some(body),
            body_ranges: ranges,
            timestamp: Some(20),
            quote: Some(presage::proto::data_message::Quote {
                id: Some(10),
                author_aci: Some(uuid(2).to_string()),
                text: Some("hi there".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(2), 20, dm)) }));
        app.update(Message::React(uuid(2), 20, "😂".into()));
        let mine = crate::timeline::tests::text(30, "👍🏽❤️");
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(1), 30, mine)) }));
        app.update(Message::Backend(Event::Receipt { timestamps: vec![30], level: Delivery::Read }));
        let key = voice_note(&mut app);
        app.attachments.insert(key.clone(), Attach::Ready("/nonexistent/a0.m4a".into()));
        app.durations.insert(key, std::time::Duration::from_secs(12));
        app
    }

    #[test]
    fn timeline_renders_quotes_reactions_and_status() {
        let app = demo();
        let mut ui = simulator(view(&app));
        assert!(ui.find("😂").is_ok(), "reaction pill");
        assert!(ui.find("👍🏽❤️").is_ok(), "jumbo emoji message");
        assert!(ui.find("✓✓").is_ok(), "read receipt mark");
        assert!(ui.find(day_label(10).as_str()).is_ok(), "day separator");
    }

    #[test]
    fn clicking_a_spoiler_reveals_it() {
        let (body, ranges) = richtext::parse_markup("||secret||");
        let el = rich(&body, &ranges, &HashMap::new(), 7, &HashSet::new(), Color::WHITE);
        let mut ui = simulator(el);
        ui.point_at((6.0, 8.0));
        ui.simulate(iced_test::simulator::click());
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(matches!(msgs.as_slice(), [Message::Spoiler(7, 0)]), "{msgs:?}");

        // Once revealed, it's plain text: no link, so clicking does nothing.
        let el = rich(&body, &ranges, &HashMap::new(), 7, &HashSet::from([(7, 0)]), Color::WHITE);
        let mut ui = simulator(el);
        ui.point_at((6.0, 8.0));
        ui.simulate(iced_test::simulator::click());
        assert_eq!(ui.into_messages().count(), 0);
    }

    /// Renders real screenshots to `snapshots/` for visual review (fonts, emoji, layout).
    #[test]
    fn snapshots() {
        let mut app = demo();
        for dark in [true, false] {
            app.dark = dark;
            let name = if dark { "conversation-dark" } else { "conversation-light" };
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots").join(name);
            let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
            let snap = ui.snapshot(&app.theme()).expect("render");
            for f in std::fs::read_dir(path.parent().unwrap()).into_iter().flatten().flatten() {
                if f.file_name().to_string_lossy().starts_with(name) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            assert!(snap.matches_image(&path).expect("write png"));
        }
    }

    #[test]
    fn typing_and_sending_through_the_ui() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let mut ui = simulator(view(&app));
        ui.click(iced::widget::Id::new(COMPOSER)).expect("composer");
        let _ = ui.typewrite("hello :wave:");
        let msgs: Vec<Message> = ui.into_messages().collect();
        for m in msgs {
            let _ = app.update(m);
        }
        assert_eq!(app.open.as_ref().unwrap().composer.text().trim(), "hello :wave:");
        let mut ui = simulator(view(&app));
        ui.click("Send").expect("send button");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        let sent = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|c| match c {
            crate::backend::Command::Send { body, .. } => Some(body),
            _ => None,
        });
        assert_eq!(sent.as_deref(), Some("hello 👋"));
    }

    fn click(app: &mut App, label: &str) {
        let mut ui = simulator(view(app));
        ui.click(label).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
    }

    #[test]
    fn hover_edit_then_send_through_the_ui() {
        let (mut app, mut rx) = app();
        let id = crate::app::tests::with_own_message(&mut app, &mut rx, "*typo*");
        app.open.as_mut().unwrap().hovered = Some((uuid(1), id));
        click(&mut app, "✎");
        assert!(simulator(view(&app)).find("Editing message").is_ok(), "edit bar");
        assert_eq!(app.open.as_ref().unwrap().composer.text().trim_end(), "*typo*");
        app.update(Message::Compose(text_editor::Action::Edit(text_editor::Edit::Insert('!'))));
        click(&mut app, "Send");
        let cmds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Edit { target_ts, body, .. } if *target_ts == id && body == "typo!")),
            "{cmds:?}"
        );
        assert!(simulator(view(&app)).find("Editing message").is_err(), "bar gone after sending");
    }

    #[test]
    fn hover_delete_offers_everyone_only_for_own_recent_messages() {
        let (mut app, mut rx) = app();
        let id = crate::app::tests::with_own_message(&mut app, &mut rx, "oops");
        app.open.as_mut().unwrap().hovered = Some((uuid(1), id));
        click(&mut app, "🗑");
        click(&mut app, "Delete for everyone");
        assert!(matches!(rx.try_recv(), Ok(Command::DeleteForEveryone { target_ts, .. }) if target_ts == id));
        assert!(simulator(view(&app)).find("This message was deleted.").is_ok());

        // Alice's message: no edit, and only "Delete for me".
        app.open.as_mut().unwrap().hovered = Some((uuid(2), 10));
        assert!(simulator(view(&app)).find("✎").is_err());
        click(&mut app, "🗑");
        assert!(simulator(view(&app)).find("Delete for everyone").is_err());
        click(&mut app, "Delete for me");
        assert!(matches!(rx.try_recv(), Ok(Command::DeleteForMe { target_ts: 10, .. })));
        assert_eq!(app.open.as_ref().unwrap().timeline.as_ref().unwrap().messages.len(), 1);
    }

    /// An incoming sticker from Alice (ts 50) from pack `pack`.
    fn incoming_sticker(app: &mut App, pack: u8) {
        let s = Sticker {
            pack_id: Some(vec![pack]),
            pack_key: Some(vec![pack; 32]),
            sticker_id: Some(1),
            emoji: Some("🐱".into()),
            data: Some(AttachmentPointer { content_type: Some("image/webp".into()), digest: Some(vec![0x51]), cdn_number: Some(3), ..Default::default() }),
        };
        let dm = presage::proto::DataMessage { timestamp: Some(50), sticker: Some(s), ..Default::default() };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(2), 50, dm)) }));
    }

    #[test]
    fn sticker_picker_opens_and_clicking_a_sticker_sends_it() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        fake_packs(&mut app, vec![(vec![9], 3)]);
        click(&mut app, "🗒");
        assert!(simulator(view(&app)).find("Recently used").is_ok(), "recents tab first");
        app.update(Message::StickerTab(StickerTab::Pack(vec![8])));
        let mut ui = simulator(view(&app));
        assert!(ui.find("Cats").is_ok(), "pack title");
        ui.click(iced::widget::Id::from(sticker_cell_id(&[8], 2))).expect("sticker cell");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(m, Message::SendSticker(p, 2) if *p == [8])), "{msgs:?}");
        for m in msgs {
            app.update(m);
        }
        assert!(crate::app::tests::drain(&mut rx).iter().any(|c| matches!(c, Command::SendSticker { .. })));
        assert!(simulator(view(&app)).find("Cats").is_err(), "picker closed");
    }

    #[test]
    fn received_sticker_from_an_uninstalled_pack_offers_install() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        fake_packs(&mut app, Vec::new());
        incoming_sticker(&mut app, 8);
        assert!(simulator(view(&app)).find("Install pack").is_err(), "installed pack: just the image");
        incoming_sticker(&mut app, 5);
        click(&mut app, "Install pack");
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(cmds.iter().any(|c| matches!(c, Command::InstallStickerPack { id, key } if *id == [5] && *key == [5; 32])), "{cmds:?}");
    }

    /// Writes `snapshots/stickers-*.png`: borderless stickers and the open picker.
    #[test]
    fn sticker_snapshot() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        fake_packs(&mut app, Vec::new());
        incoming_sticker(&mut app, 8);
        app.update(Message::SendSticker(vec![9], 3));
        app.update(Message::ToggleStickers);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("stickers") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("stickers")).expect("write png"));
    }

    #[test]
    fn voice_note_player_in_bubble() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let key = voice_note(&mut app);
        let mut ui = simulator(view(&app));
        assert!(ui.find("--:--").is_ok(), "length unknown until downloaded");
        ui.click("▶").expect("play button");
        assert!(ui.into_messages().any(|m| matches!(m, Message::PlayPause(k) if k == key)));

        app.attachments.insert(key.clone(), Attach::Ready("/nonexistent/a0.m4a".into()));
        app.durations.insert(key.clone(), std::time::Duration::from_secs(75));
        app.playback = Some(crate::audio::Playback::new(key, std::time::Instant::now()));
        let mut ui = simulator(view(&app));
        assert!(ui.find("0:00 / 1:15").is_ok(), "progress / length");
        assert!(ui.find("⏸").is_ok(), "pause while playing");
    }

    #[test]
    fn mic_replaces_send_and_records_a_voice_note() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let audio = fake_audio(&mut app);
        let mut ui = simulator(view(&app));
        assert!(ui.find("Send").is_err(), "empty composer shows the mic instead of Send");
        ui.click("🎤").expect("mic button");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert!(matches!(audio.try_recv(), Ok(crate::audio::Cmd::Record(_))));

        let mut ui = simulator(view(&app));
        assert!(ui.find("Recording  0:00").is_ok(), "elapsed time");
        assert!(ui.find("Cancel").is_ok());
        ui.click("Send").expect("send recording");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        let Ok(crate::audio::Cmd::Finish(Some((path, reply)))) = audio.try_recv() else { panic!("finish") };
        let _ = reply.send(Ok(path.clone())); // what the device thread does after encoding
        app.update(Message::Recorded(Ok(path)));
        let sent = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|c| match c {
            crate::backend::Command::Send { voice_note, attachments, .. } => Some((voice_note, attachments.len())),
            _ => None,
        });
        assert_eq!(sent, Some((true, 1)));
        assert!(simulator(view(&app)).find("🎤").is_ok(), "back to the composer");
    }

    #[test]
    fn emoji_picker_inserts() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::TogglePicker);
        app.update(Message::PickerQuery("thumbs up".into()));
        let mut ui = simulator(view(&app));
        ui.click("👍").expect("search result");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert!(app.open.as_ref().unwrap().composer.text().contains('👍'));
    }

    #[test]
    fn header_timer_control_changes_the_timer() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let mut ui = simulator(view(&app));
        ui.click("⏱ Off").expect("timer control in header");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        let mut ui = simulator(view(&app));
        ui.click("5m").expect("timer choices");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert!(matches!(rx.try_recv(), Ok(crate::backend::Command::SetTimer { seconds: 300, .. })));
        assert!(simulator(view(&app)).find("⏱ 5m").is_ok(), "header shows the new timer");
    }

    #[test]
    fn group_timer_is_shown_read_only() {
        let (mut app, _rx) = app();
        app.update(Message::OpenThread(presage::store::Thread::Group([5; 32]), "Team".into(), true));
        let mut ui = simulator(view(&app));
        ui.click("⏱ Off").expect("timer shown");
        assert_eq!(ui.into_messages().count(), 0, "not clickable in groups");
    }

    #[test]
    fn disappearing_messages_show_a_countdown() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let dm = presage::proto::DataMessage { expire_timer: Some(300), ..crate::timeline::tests::text(20, "poof") };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(2), 20, dm)) }));
        assert!(simulator(view(&app)).find("⏳").is_ok(), "hourglass before the countdown starts");
        let at = crate::backend::now_ms() + 290_000;
        app.update(Message::Backend(Event::Expiring { thread: alice(), expires: vec![(uuid(2), 20, at)] }));
        assert!(simulator(view(&app)).find("⏳ 4m").is_ok());
    }

    #[test]
    fn mention_autocomplete_click_inserts_name() {
        let (mut app, mut rx) = crate::app::tests::open_group();
        crate::app::tests::type_text(&mut app, "hi @bobb");
        let mut ui = simulator(view(&app));
        ui.click("Bobby").expect("member suggested"); // (Alice is also a sidebar chat)
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert_eq!(app.open.as_ref().unwrap().composer.text().trim_end(), "hi @Bobby");
        app.update(Message::Send);
        let sent = crate::app::tests::drain(&mut rx).into_iter().find_map(|c| match c {
            crate::backend::Command::Send { body, ranges, .. } => Some((body, ranges)),
            _ => None,
        });
        let (body, ranges) = sent.expect("sent");
        assert_eq!(body, "hi \u{FFFC}");
        assert_eq!(richtext::segments(&body, &ranges)[1].mention, Some(uuid(4).to_string()));
    }

    #[test]
    fn clicking_a_message_result_opens_its_conversation() {
        let (mut app, mut rx) = app();
        let bob = presage::store::Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(uuid(3).into()));
        app.update(Message::Search("lunch".into()));
        let result = crate::backend::SearchResult {
            thread: bob.clone(),
            title: "Bob".into(),
            id: 99,
            author: uuid(3),
            snippet: "lunch tomorrow?".into(),
        };
        app.update(Message::Backend(Event::SearchResults { query: "lunch".into(), results: vec![result] }));
        let mut ui = simulator(view(&app));
        assert!(ui.find("Messages").is_ok(), "section header");
        ui.click("lunch tomorrow?").expect("result listed");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert!(matches!(rx.try_recv(), Ok(crate::backend::Command::Open(t)) if t == bob));
        assert!(app.results.is_empty(), "search cleared once opened");
    }

    #[test]
    fn row_menu_pins_mutes_archives_and_marks_unread() {
        let (mut app, mut rx) = app();
        assert!(simulator(view(&app)).find("⋯").is_err(), "only on hover");
        app.row_hovered = Some(alice());
        click(&mut app, "⋯");
        click(&mut app, "Pin");
        app.row_hovered = Some(alice());
        click(&mut app, "⋯");
        click(&mut app, "Mute");
        {
            let mut ui = simulator(view(&app));
            assert!(ui.find("📌").is_ok() && ui.find("🔕").is_ok(), "pinned and muted marks");
        }

        app.conversations[0].unread = 0;
        click(&mut app, "⋯");
        click(&mut app, "Mark unread");
        assert!(app.conversations[0].marked_unread);
        click(&mut app, "⋯");
        assert!(simulator(view(&app)).find("Unpin").is_ok() && simulator(view(&app)).find("Mark read").is_ok());
        click(&mut app, "Archive");
        let mut ui = simulator(view(&app));
        assert!(ui.find("Alice").is_err(), "gone from the inbox");
        ui.click("Archived chats (1)").expect("archived link");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert!(simulator(view(&app)).find("Alice").is_ok(), "listed under Archived");
        click(&mut app, "← Archived chats");
        assert!(!app.show_archived);

        let actions: Vec<ChatAction> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|c| match c {
                Command::Chat { action, .. } => Some(action),
                _ => None,
            })
            .collect();
        assert_eq!(actions, [ChatAction::Pin(true), ChatAction::Mute(true), ChatAction::MarkUnread(true), ChatAction::Archive(true)]);
    }

    #[test]
    fn message_requests_are_collapsed_and_answered_from_the_bar() {
        let (mut app, mut rx) = crate::app::tests::with_request();
        let bob = crate::app::tests::bob();
        assert!(simulator(view(&app)).find("Bob").is_err(), "collapsed");
        click(&mut app, "▸ Message requests (1)");
        app.row_hovered = Some(bob.clone());
        assert!(simulator(view(&app)).find("⋯").is_err(), "no menu on requests");
        click(&mut app, "Bob");
        {
            let mut ui = simulator(view(&app));
            assert!(ui.find("Accept").is_ok() && ui.find("Block").is_ok() && ui.find("Delete").is_ok());
            assert!(ui.find(iced::widget::Id::new(COMPOSER)).is_err(), "no composer until accepted");
        }
        click(&mut app, "Accept");
        assert!(simulator(view(&app)).find(iced::widget::Id::new(COMPOSER)).is_ok(), "composer once accepted");
        assert!(simulator(view(&app)).find("Message requests (1)").is_err());
        let cmds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(cmds.iter().any(|c| matches!(c, Command::Chat { thread, action: ChatAction::Accept } if *thread == bob)), "{cmds:?}");

        let (mut app, mut rx) = crate::app::tests::with_request();
        app.update(Message::OpenThread(bob.clone(), "Bob".into(), false));
        click(&mut app, "Block");
        assert!(app.open.is_none());
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|c| matches!(c, Command::Chat { action: ChatAction::Block, .. })));
    }

    /// Writes `snapshots/sidebar-flags-*.png`: requests, pin, mute and unread-dot marks.
    #[test]
    fn sidebar_flags_snapshot() {
        let (mut app, _rx) = crate::app::tests::with_request();
        app.requests_open = true;
        let c = &mut app.conversations[1];
        (c.pinned, c.muted, c.unread, c.marked_unread) = (true, true, 0, true);
        app.row_hovered = Some(alice());
        app.row_menu = Some(alice());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("sidebar-flags") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("sidebar-flags")).expect("write png"));
    }

    #[test]
    fn link_screen_renders_qr() {
        let (mut app, _rx) = app();
        app.update(Message::Backend(Event::NeedsLink));
        assert!(simulator(view(&app)).find("Generate QR code").is_ok());
        app.update(Message::Link);
        app.update(Message::Backend(Event::LinkUrl("sgnl://linkdevice?uuid=a&pub_key=b".into())));
        let mut ui = simulator(view(&app));
        assert!(ui.find("Waiting for your phone…").is_ok());
    }

    #[test]
    fn group_updates_render_with_names() {
        use crate::groups::GroupEdit;
        use crate::timeline::tests::{edit, group_update};
        let (mut app, _rx) = crate::app::tests::open_group();
        let dm = group_update(uuid(2), 50, |ops, acts| {
            edit(ops, acts, GroupEdit::SetAdmin(uuid(4), true));
            edit(ops, acts, GroupEdit::Title("Hikers".into()));
        });
        let thread = crate::app::tests::group();
        app.update(Message::Backend(Event::Incoming { thread, content: Box::new(content(uuid(2), 50, dm)) }));
        let mut ui = simulator(view(&app));
        assert!(ui.find("Alice made Bobby an admin\nAlice changed the group name to “Hikers”").is_ok());
    }

    #[test]
    fn group_header_opens_the_info_panel() {
        let (mut app, mut rx) = crate::app::tests::open_group();
        click(&mut app, "4 members");
        let mut ui = simulator(view(&app));
        for s in ["Weekend plans", "Your role: Member", "4 members · 1 invited", "Admin", "Leave group"] {
            assert!(ui.find(s).is_ok(), "{s}");
        }
        assert!(ui.find("Remove").is_err() && ui.find("Make admin").is_err(), "members can't manage others");
        drop(ui);
        crate::app::tests::make_me_admin(&mut app);
        click(&mut app, "Make admin");
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(
            matches!(cmds.as_slice(), [Command::EditGroup { edit: crate::groups::GroupEdit::SetAdmin(_, true), .. }]),
            "{cmds:?}"
        );
        click(&mut app, "← Back");
        assert!(app.open.as_ref().unwrap().info.is_none());
    }

    #[test]
    fn new_group_dialog_emits_create() {
        let (mut app, mut rx) = app();
        click(&mut app, "New group");
        app.update(Message::NewGroupTitle("Hikers".into()));
        click(&mut app, "Bob");
        click(&mut app, "Create");
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(matches!(cmds.as_slice(), [Command::CreateGroup { title, members }] if title == "Hikers" && *members == vec![uuid(3)]), "{cmds:?}");
    }

    /// Writes `snapshots/group-info-*.png` for visual review.
    #[test]
    fn group_info_snapshot() {
        let (mut app, _rx) = crate::app::tests::open_group();
        crate::app::tests::make_me_admin(&mut app);
        app.update(Message::ToggleGroupInfo);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("group-info") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("group-info")).expect("write png"));
    }

    fn apply(app: &mut App, msgs: Vec<Message>) {
        for m in msgs {
            let _ = app.update(m);
        }
    }

    #[test]
    fn clicking_an_image_opens_the_viewer_and_it_steps_and_closes() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let keys = crate::app::tests::photos(&mut app);
        for k in &keys[..2] {
            app.attachments.insert(k.clone(), Attach::Ready(format!("/nonexistent/{k}.jpg").into()));
        }
        let mut ui = simulator(view(&app));
        assert!(ui.find("Save…").is_err(), "no viewer yet");
        ui.click(iced::widget::Id::from(format!("image:{}", keys[1]))).expect("grid thumbnail");
        let msgs: Vec<Message> = ui.into_messages().collect();
        apply(&mut app, msgs.clone());
        assert!(msgs.iter().any(|m| matches!(m, Message::ViewImage(k) if *k == keys[1])), "{msgs:?}");

        let mut ui = simulator(view(&app));
        assert!(ui.find("Save…").is_ok(), "downloaded: can be saved");
        let when = Local.timestamp_millis_opt(50).unwrap().format("%b %-d, %Y %H:%M");
        assert!(ui.find(format!("Alice · {when} · photo178.jpg").as_str()).is_ok(), "caption: sender, date, file name");
        ui.click("›").expect("next");
        let msgs: Vec<Message> = ui.into_messages().collect();
        apply(&mut app, msgs);
        let mut ui = simulator(view(&app));
        assert!(ui.find("Loading…").is_ok(), "the big photo is being fetched");
        assert!(ui.find("Save…").is_err());
        ui.click("✕").expect("close");
        let msgs: Vec<Message> = ui.into_messages().collect();
        apply(&mut app, msgs);
        assert!(app.open.as_ref().unwrap().viewer.is_none());
    }

    #[test]
    fn clicking_a_link_emits_open_url() {
        let el = rich("https://signal.org/ ok", &[], &HashMap::new(), 7, &HashSet::new(), Color::WHITE);
        let mut ui = simulator(el);
        ui.point_at((6.0, 8.0));
        ui.simulate(iced_test::simulator::click());
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(matches!(msgs.as_slice(), [Message::OpenUrl(u)] if u == "https://signal.org/"), "{msgs:?}");
    }

    #[test]
    fn links_split_segments_but_keep_styles_mentions_and_spoilers() {
        let (body, mut ranges) = richtext::parse_markup("*see https://a.b/c!* ||https://hidden.x|| ");
        let body = format!("{body}\u{FFFC}");
        ranges.push(BodyRange {
            start: Some(body.encode_utf16().count() as u32 - 1),
            length: Some(1),
            associated_value: Some(presage::proto::body_range::AssociatedValue::MentionAci("https://not.a.link".into())),
        });
        let sp = spans(&body, &ranges, &HashMap::new(), 1, &HashSet::new(), Color::WHITE);
        let parts: Vec<(&str, bool, bool)> = sp
            .iter()
            .map(|s| (s.text.as_ref(), s.font.is_some_and(|f| f.weight == Weight::Bold), matches!(s.link, Some(Message::OpenUrl(_)))))
            .collect();
        assert_eq!(parts[..3], [("see ", true, false), ("https://a.b/c", true, true), ("!", true, false)]);
        assert!(matches!(sp[4].link, Some(Message::Spoiler(1, _))), "hidden spoiler stays a spoiler: {parts:?}");
        assert!(sp.iter().filter(|s| matches!(s.link, Some(Message::OpenUrl(_)))).count() == 1, "{parts:?}");

        // Revealed, the spoiler's link works.
        let sp = spans(&body, &ranges, &HashMap::new(), 1, &HashSet::from([(1, 2)]), Color::WHITE);
        assert!(sp.iter().any(|s| matches!(&s.link, Some(Message::OpenUrl(u)) if u == "https://hidden.x")));
    }

    #[test]
    fn received_link_preview_renders_and_opens() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let preview = presage::proto::Preview {
            url: Some("https://signal.org/blog/".into()),
            title: Some("Signal Blog".into()),
            description: Some("Fake description".into()),
            ..Default::default()
        };
        let dm = presage::proto::DataMessage { preview: vec![preview], ..crate::timeline::tests::text(20, "look https://signal.org/blog/") };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(2), 20, dm)) }));
        let mut ui = simulator(view(&app));
        assert!(ui.find("Fake description").is_ok());
        assert!(ui.find("signal.org").is_ok(), "domain");
        ui.click("Signal Blog").expect("card title");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(m, Message::OpenUrl(u) if u == "https://signal.org/blog/")), "{msgs:?}");
    }

    /// Settings open with a fake profile and two fake devices.
    fn settings_app() -> (App, tokio::sync::mpsc::UnboundedReceiver<Command>) {
        let (mut app, mut rx) = app();
        click(&mut app, "⚙");
        assert!(matches!(crate::app::tests::drain(&mut rx).as_slice(), [Command::LoadSettings]));
        let profile = crate::backend::OwnProfile { given: "Fake".into(), family: "Person".into(), about: "Testing".into(), emoji: "🧪".into() };
        app.update(Message::Backend(Event::Profile(profile)));
        app.update(Message::Backend(Event::Account { phone: "+15550100".into() }));
        app.update(Message::Backend(Event::Devices(vec![
            crate::backend::Device { id: 1, name: "Fake Phone".into(), this: false },
            crate::backend::Device { id: 2, name: "signal-rs on desk".into(), this: true },
        ])));
        (app, rx)
    }

    #[test]
    fn settings_screen_switches_theme_and_toggles_notifications() {
        let (mut app, mut rx) = settings_app();
        let mut ui = simulator(view(&app));
        assert!(ui.find("+15550100").is_ok() && ui.find("This device").is_ok(), "number and this device marked");
        drop(ui);
        app.system_dark = true;
        click(&mut app, "Light");
        assert!(!app.dark, "overrides the dark system theme");
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(matches!(cmds.as_slice(), [Command::SaveSettings(s)] if s.theme == crate::state::ThemeChoice::Light), "{cmds:?}");
        click(&mut app, "Show message text in notifications");
        assert!(!app.settings.notification_text);
        click(&mut app, "Show notifications");
        assert!(!app.settings.notifications);
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(matches!(cmds.last(), Some(Command::SaveSettings(s)) if !s.notifications && !s.notification_text), "{cmds:?}");
        click(&mut app, "System");
        app.update(Message::Theme(iced::theme::Mode::Light));
        assert!(!app.dark, "follows the system again");
    }

    #[test]
    fn editing_the_profile_name_emits_update_profile() {
        let (mut app, mut rx) = settings_app();
        let mut ui = simulator(view(&app));
        assert!(ui.click("Save profile").is_ok());
        assert_eq!(ui.into_messages().count(), 0, "nothing changed yet");
        let mut ui = simulator(view(&app));
        ui.click(iced::widget::Id::new("profile-given")).expect("name field");
        let _ = ui.typewrite("r");
        for m in ui.into_messages().collect::<Vec<_>>() {
            app.update(m);
        }
        click(&mut app, "Save profile");
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(
            matches!(cmds.as_slice(), [Command::UpdateProfile { given, family, about, emoji }]
                if given == "Faker" && family == "Person" && about == "Testing" && emoji == "🧪"),
            "{cmds:?}"
        );
    }

    #[test]
    fn unlinking_needs_confirmation() {
        let (mut app, mut rx) = settings_app();
        click(&mut app, "Unlink this device");
        assert!(crate::app::tests::drain(&mut rx).is_empty(), "the first click only asks");
        click(&mut app, "Cancel");
        assert!(simulator(view(&app)).find("Unlink and delete data").is_err());
        click(&mut app, "Unlink this device");
        click(&mut app, "Unlink and delete data");
        assert!(matches!(crate::app::tests::drain(&mut rx).as_slice(), [Command::Unlink]));
        app.update(Message::Backend(Event::NeedsLink));
        assert!(matches!(app.screen, Screen::Link { .. }) && app.conversations.is_empty(), "back to linking, old chats gone");
    }

    #[test]
    fn avatars_replace_the_initial() {
        let (mut app, _rx) = app();
        assert!(simulator(view(&app)).find("A").is_ok(), "Alice's initial");
        let key = crate::state::thread_key(&alice());
        app.update(Message::Backend(Event::Avatars(HashMap::from([(key, image::Handle::from_bytes(crate::app::tests::PNG.to_vec()))]))));
        assert!(simulator(view(&app)).find("A").is_err(), "her photo instead");
    }

    /// Writes `snapshots/settings-*.png` for visual review (light theme, avatar shown).
    #[test]
    fn settings_snapshot() {
        let (mut app, _rx) = settings_app();
        app.update(Message::AskUnlink(true));
        app.update(Message::SetTheme(crate::state::ThemeChoice::Light));
        let png = image::Handle::from_bytes(crate::app::tests::PNG.to_vec());
        app.avatars = HashMap::from([(crate::state::thread_key(&alice()), png)]);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("settings") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("settings")).expect("write png"));
    }

    fn run(app: &mut App, msgs: Vec<Message>) {
        for m in msgs {
            let _ = app.update(m);
        }
    }

    #[test]
    fn stories_tab_opens_the_viewer_and_sends_a_reply() {
        let (mut app, mut rx) = app();
        crate::app::tests::with_stories(&mut app);
        let mut ui = simulator(view(&app));
        ui.click("● Stories").expect("unviewed dot on the Stories button");
        let msgs: Vec<Message> = ui.into_messages().collect();
        run(&mut app, msgs);
        let mut ui = simulator(view(&app));
        assert!(ui.find("Bob").is_ok() && ui.find("Chats").is_ok());
        ui.click("Alice").expect("Alice's stories listed");
        let msgs: Vec<Message> = ui.into_messages().collect();
        run(&mut app, msgs);
        let mut ui = simulator(view(&app));
        assert!(ui.find("hello story").is_ok(), "text story shown");
        ui.click(iced::widget::Id::new(STORY_REPLY)).expect("reply box");
        let _ = ui.typewrite("so cool");
        let _ = ui.tap_key(keyboard::Key::Named(key::Named::Enter));
        let msgs: Vec<Message> = ui.into_messages().collect();
        run(&mut app, msgs);
        let mut ui = simulator(view(&app));
        ui.click("😂").expect("reaction buttons");
        let msgs: Vec<Message> = ui.into_messages().collect();
        run(&mut app, msgs);
        let replies: Vec<_> = drain(&mut rx).into_iter().filter_map(|c| match c {
            Command::StoryReply { reply, .. } => Some(reply),
            _ => None,
        }).collect();
        use crate::stories::StoryReply;
        assert_eq!(replies, [StoryReply::Text("so cool".into()), StoryReply::Reaction("😂".into())]);
        let mut ui = simulator(view(&app));
        ui.click("✕").expect("close");
        let msgs: Vec<Message> = ui.into_messages().collect();
        run(&mut app, msgs);
        assert!(app.story_viewer.is_none());
    }

    #[test]
    fn story_without_replies_has_no_reply_box() {
        let (mut app, _rx) = app();
        crate::app::tests::with_stories(&mut app);
        app.update(Message::OpenStories(presage::store::Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(uuid(3).into()))));
        let mut ui = simulator(view(&app));
        assert!(ui.find("bob's story").is_ok());
        assert!(ui.find(iced::widget::Id::new(STORY_REPLY)).is_err());
    }

    #[test]
    fn story_replies_show_a_quote_line() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let ctx = Some(presage::proto::data_message::StoryContext { author_aci: Some(uuid(1).to_string()), sent_timestamp: Some(5), ..Default::default() });
        let dm = presage::proto::DataMessage { story_context: ctx, ..crate::timeline::tests::text(20, "great story") };
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(2), 20, dm)) }));
        let mut ui = simulator(view(&app));
        assert!(ui.find("Replied to a story").is_ok(), "quote line");
    }

    #[test]
    fn story_colors_and_gradients() {
        assert_eq!(argb(0x80ff0000), Color::from_rgba8(255, 0, 0, 128.0 / 255.0));
        use presage::proto::text_attachment::{Background as Bg, Gradient};
        let t = |bg| presage::proto::TextAttachment { background: Some(bg), ..Default::default() };
        assert_eq!(story_background(&t(Bg::Color(0xff2c6bed))), Background::Color(BLUE));
        let g = Gradient { colors: vec![0xffff0000, 0xff0000ff], angle: Some(90), ..Default::default() };
        let Background::Gradient(iced::Gradient::Linear(l)) = story_background(&t(Bg::Gradient(g))) else { panic!("gradient") };
        assert_eq!(l.angle, iced::Radians::from(iced::Degrees(90.0)));
        let stops: Vec<_> = l.stops.iter().flatten().map(|s| (s.offset, s.color)).collect();
        assert_eq!(stops, [(0.0, Color::from_rgb8(255, 0, 0)), (1.0, Color::from_rgb8(0, 0, 255))], "evenly spread without positions");
    }

    /// Writes `snapshots/stories-*.png`: the Stories list and a gradient text story in the viewer.
    #[test]
    fn stories_snapshot() {
        let (mut app, _rx) = app();
        crate::app::tests::with_stories(&mut app);
        if let Some(presage::proto::story_message::Attachment::TextAttachment(t)) = &mut app.stories[0].msg.attachment {
            t.text_style = Some(presage::proto::text_attachment::Style::Serif as i32);
            t.background = Some(presage::proto::text_attachment::Background::Gradient(presage::proto::text_attachment::Gradient {
                colors: vec![0xfff5a623, 0xffd0021b, 0xff4a148c],
                angle: Some(180),
                ..Default::default()
            }));
        }
        app.stories[2].viewed = true;
        app.update(Message::ToggleStories);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("stories-") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        for (name, open) in [("stories-list", false), ("stories-viewer", true)] {
            if open {
                app.update(Message::OpenStories(alice()));
            }
            let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
            let snap = ui.snapshot(&app.theme()).expect("render");
            assert!(snap.matches_image(dir.join(name)).expect("write png"));
        }
    }

    fn call(phase: Phase, incoming: bool) -> Call {
        Call { peer: uuid(2), incoming, video: false, phase, muted: false, connected_at: None }
    }

    fn sent_calls(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<CallCommand> {
        drain(rx).into_iter().filter_map(|c| if let Command::Call(c) = c { Some(c) } else { None }).collect()
    }

    #[test]
    fn incoming_call_is_answered_or_declined_from_the_call_bar() {
        let (mut app, mut rx) = app();
        app.update(Message::Backend(Event::Call(call(Phase::Calling, true))));
        {
            let mut ui = simulator(view(&app));
            assert!(ui.find("Incoming call from Alice…").is_ok());
            assert!(ui.find("Accept").is_err(), "nothing to accept until it rings");
        }

        app.update(Message::Backend(Event::Call(call(Phase::Ringing, true))));
        assert!(simulator(view(&app)).find("Incoming call from Alice").is_ok());
        click(&mut app, "Accept");
        assert_eq!(sent_calls(&mut rx), [CallCommand::Accept]);
        click(&mut app, "Decline");
        assert_eq!(sent_calls(&mut rx), [CallCommand::Hangup]);
    }

    #[test]
    fn connected_call_shows_its_timer_and_mutes_and_hangs_up() {
        let (mut app, mut rx) = app();
        let connected = Call { connected_at: Some(now_ms() - 65_000), ..call(Phase::Connected, false) };
        app.update(Message::Backend(Event::Call(connected.clone())));
        assert!(simulator(view(&app)).find("Alice · 1:05").is_ok());
        click(&mut app, "Mute");
        assert_eq!(sent_calls(&mut rx), [CallCommand::Mute(true)]);
        app.update(Message::Backend(Event::Call(Call { muted: true, ..connected })));
        click(&mut app, "Unmute");
        assert_eq!(sent_calls(&mut rx), [CallCommand::Mute(false)]);
        click(&mut app, "Hang up");
        assert_eq!(sent_calls(&mut rx), [CallCommand::Hangup]);
    }

    #[test]
    fn ended_call_stays_until_closed() {
        let (mut app, mut rx) = app();
        app.update(Message::Backend(Event::Call(call(Phase::Ended("No answer".into()), false))));
        assert!(simulator(view(&app)).find("Alice · No answer").is_ok());
        click(&mut app, "Close");
        assert!(app.call.is_none());
        assert!(sent_calls(&mut rx).is_empty(), "closing is local");
    }

    /// The header's call button exists only in `calls` builds, and only for 1:1 chats.
    #[test]
    fn header_call_button() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        assert_eq!(simulator(view(&app)).find("📞 Call").is_ok(), crate::calls::ENABLED);
        if crate::calls::ENABLED {
            click(&mut app, "📞 Call");
            assert_eq!(sent_calls(&mut rx), [CallCommand::Start(uuid(2))]);
            // Busy with a call: the button does nothing.
            app.update(Message::Backend(Event::Call(call(Phase::Ringing, false))));
            click(&mut app, "📞 Call");
            assert!(sent_calls(&mut rx).is_empty());
        }
        let (group, _rx) = crate::app::tests::open_group();
        assert!(simulator(view(&group)).find("📞 Call").is_err());
    }

    /// Writes `snapshots/call-bar-*.png`: a connected call over Alice's chat.
    #[test]
    fn call_bar_snapshot() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::Backend(Event::Call(Call { connected_at: Some(now_ms() - 65_000), ..call(Phase::Connected, false) })));
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("call-bar") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("call-bar")).expect("write png"));
    }
}
