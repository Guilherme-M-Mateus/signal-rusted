//! Rendering. Signal-Desktop's layout: conversation list left, timeline + composer right.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{Local, TimeZone};
use iced::border::Radius;
use iced::font::{Family, Style as FontStyle, Weight};
use iced::keyboard::{self, key};
use iced::widget::text::Span;
use iced::widget::text_editor::{Binding, KeyPress};
use iced::widget::{
    Column, Space, button, center, checkbox, column, container, image, mouse_area, opaque, progress_bar, qr_code, rich_text, row,
    rule, scrollable, span, stack, svg, text, text_editor, text_input, tooltip,
};
use iced::{Alignment, Background, Border, Color, ContentFit, Element, Font, Length, Theme, color, mouse};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::data_message::Sticker;
use presage::proto::{AttachmentPointer, BodyRange, Preview};

use crate::app::{App, Attach, Fun, Message, Open, Screen, SettingsSection, StickerTab, TIMERS, Tab, display_name, is_image};
use crate::gifs::Provider as GifProvider;
use crate::icons::{self, icon, tinted};
use crate::state::ThemeChoice;
use crate::backend::{ChatAction, Conversation, attachment_key, now_ms};
use crate::calls::{Call, CallCommand, Phase};
use crate::stories::{Story, Viewer};
use crate::timeline::{Delivery, Kind, Message as Msg, attachment_label, short_duration};
use crate::{emoji, richtext};

pub const COMPOSER: &str = "composer";
pub const STORY_REPLY: &str = "story-reply";
const BLUE: Color = color!(0x2c6bed);

/// The themes the settings offer, in order.
pub const THEMES: [(ThemeChoice, &str); 9] = [
    (ThemeChoice::System, "System"),
    (ThemeChoice::Light, "Light"),
    (ThemeChoice::Dark, "Dark"),
    (ThemeChoice::Midnight, "Midnight"),
    (ThemeChoice::Rusted, "Rusted"),
    (ThemeChoice::Nord, "Nord"),
    (ThemeChoice::Sakura, "Sakura"),
    (ThemeChoice::Mint, "Mint"),
    (ThemeChoice::Everforest, "Everforest"),
];

/// `choice`'s colors; `dark` picks Light or Dark for "System".
pub fn app_theme(choice: ThemeChoice, dark: bool) -> Theme {
    use iced::theme::Palette;
    let (name, background, text, primary) = match (choice, dark) {
        (ThemeChoice::Midnight, _) => ("Midnight", color!(0x000000), color!(0xe9e9e9), BLUE),
        (ThemeChoice::Rusted, _) => ("Rusted", color!(0x1d1f23), color!(0xeeeae6), color!(0xe8641f)),
        (ThemeChoice::Nord, _) => ("Nord", color!(0x2e3440), color!(0xeceff4), color!(0x5e81ac)),
        (ThemeChoice::Sakura, _) => ("Sakura", color!(0xfff7f9), color!(0x2b1b22), color!(0xd6457a)),
        (ThemeChoice::Mint, _) => ("Mint", color!(0xf3faf6), color!(0x18281f), color!(0x2e8b57)),
        (ThemeChoice::Everforest, _) => ("Everforest", color!(0x2d353b), color!(0xd3c6aa), color!(0xa7c080)),
        (_, true) => ("Dark", color!(0x121212), color!(0xe9e9e9), BLUE),
        (_, false) => ("Light", color!(0xffffff), color!(0x1b1b1b), BLUE),
    };
    let danger = if dark { color!(0xf44336) } else { color!(0xe53935) };
    let palette = Palette { background, text, primary, success: color!(0x4caf50), warning: color!(0xffc107), danger };
    Theme::custom(format!("signal-rusted {name}"), palette)
}

/// A custom theme's colors as an iced theme; `None` if a color doesn't parse.
pub fn custom_theme(t: &crate::themes::CustomTheme) -> Option<Theme> {
    let (background, text, primary) = t.colors()?;
    let danger = if background.r + background.g + background.b < 1.5 { color!(0xf44336) } else { color!(0xe53935) };
    let palette = iced::theme::Palette { background, text, primary, success: color!(0x4caf50), warning: color!(0xffc107), danger };
    Some(Theme::custom(format!("signal-rusted {}", t.name), palette))
}

/// Text on the accent color: white, or the background on pale accents (Everforest's green).
fn on_accent_color(theme: &Theme) -> Color {
    let a = accent(theme);
    if 0.299 * a.r + 0.587 * a.g + 0.114 * a.b > 0.6 { theme.palette().background } else { Color::WHITE }
}

fn is_dark(theme: &Theme) -> bool {
    theme.palette().background.r < 0.5
}

/// `a` moved `t` of the way towards `b`.
fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::from_rgb(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t)
}

fn muted(theme: &Theme) -> Color {
    let p = theme.palette();
    mix(p.background, p.text, 0.6)
}

fn surface(theme: &Theme) -> Color {
    let p = theme.palette();
    mix(p.background, p.text, if is_dark(theme) { 0.12 } else { 0.085 })
}

/// The navigation rail's slightly tinted background.
fn rail(theme: &Theme) -> Color {
    let p = theme.palette();
    mix(p.background, p.text, 0.04)
}

fn accent(theme: &Theme) -> Color {
    theme.palette().primary
}

thread_local! {
    /// Appearance → Rounding: square corners everywhere. Ambient because every style closure needs it;
    /// per thread since iced runs `update` and `view` on one thread (and each test on its own).
    static SQUARE: Cell<bool> = const { Cell::new(false) };
}

pub fn set_square(square: bool) {
    SQUARE.set(square);
}

/// A corner radius of `px`, or nearly square in the square appearance.
fn round(px: f32) -> Radius {
    square_or(px, SQUARE.get()).into()
}

fn square_or(px: f32, square: bool) -> f32 {
    if square { px.min(3.0) } else { px }
}

const BOLD: Font = Font { weight: Weight::Bold, ..Font::DEFAULT };

/// The app icon (assets/icon.svg).
fn logo<'a>(size: f32) -> iced::widget::Svg<'a> {
    svg(svg::Handle::from_memory(include_bytes!("../assets/icon.svg").as_slice())).width(size).height(size)
}

/// A round, borderless icon button with a tooltip; `label` is also its widget id (for tests).
fn icon_button<'a>(src: &'static str, label: &'static str, msg: Option<Message>) -> Element<'a, Message> {
    let b = button(icon(src, 20)).padding(8).on_press_maybe(msg).style(|t: &Theme, status| button::Style {
        background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| surface(t).into()),
        text_color: t.palette().text,
        border: Border { radius: round(20.0), ..Default::default() },
        ..Default::default()
    });
    tip(container(b).id(label), label, tooltip::Position::Bottom)
}

fn tip<'a>(content: impl Into<Element<'a, Message>>, label: &'static str, at: tooltip::Position) -> Element<'a, Message> {
    let label = container(text(label).size(12)).padding([4, 8]).style(container::rounded_box);
    tooltip(content, label, at).gap(4).into()
}

/// Rounded "pill" fill for the search box and the composer.
fn pill(t: &Theme) -> Background {
    surface(t).scale_alpha(0.7).into()
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
        return image(h.clone()).width(size).height(size).content_fit(ContentFit::Cover).border_radius(round(size / 2.0)).into();
    }
    let initial = title.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string());
    let h = seed.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    let bg = AVATAR_COLORS[h % AVATAR_COLORS.len()];
    container(text(initial.unwrap_or_else(|| "#".into())).size(size * 0.45).color(color!(0x3b3b6b)))
        .center(size)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            border: Border { radius: round(size / 2.0), ..Default::default() },
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
            nav_rail(app),
            rule::vertical(1),
            match &app.settings_panel {
                Some(s) => settings_nav(app, s),
                None => sidebar(app),
            },
            rule::vertical(1),
            match (&app.settings_panel, &app.new_group, &app.open) {
                (Some(s), ..) => settings(app, s),
                (None, Some(g), _) => new_group(app, g),
                (None, None, Some(o)) => conversation(app, o),
                (None, None, None) => welcome(),
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
    let body: Element<_> = match app.ringing.iter().chain(&app.notice).collect::<Vec<_>>() {
        banners if banners.is_empty() => body,
        banners => column(banners.into_iter().map(|n| {
            container(text(n.clone()).color(Color::WHITE))
                .padding([6, 12])
                .width(Length::Fill)
                .style(|t: &Theme| container::Style { background: Some(accent(t).into()), ..Default::default() })
                .into()
        }))
        .push(body)
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

fn welcome<'a>() -> Element<'a, Message> {
    center(
        column![
            logo(112.0),
            text("Welcome to signal-rusted").size(24).font(BOLD),
            muted_text("Select a chat to start messaging").size(14),
        ]
        .spacing(12)
        .align_x(Alignment::Center),
    )
    .into()
}

/// Signal-Desktop's left rail: Chats and Stories up top, Settings and your avatar at the bottom.
fn nav_rail(app: &App) -> Element<'_, Message> {
    let current = if app.settings_panel.is_some() {
        Tab::Settings
    } else if app.stories_open {
        Tab::Stories
    } else {
        Tab::Chats
    };
    let item = |src: &'static str, label: &'static str, tab: Tab, dot: bool| {
        let selected = current == tab;
        let mut face = stack![container(icon(src, 22)).center(44)];
        if dot {
            face = face.push(container(container(Space::new()).width(9).height(9).style(|t: &Theme| container::Style {
                background: Some(accent(t).into()),
                border: Border { radius: round(5.0), ..Default::default() },
                ..Default::default()
            })).padding([8, 7]).align_right(44));
        }
        let b = button(face).padding(0).on_press(Message::ShowTab(tab)).style(move |t: &Theme, status| button::Style {
            background: (selected || matches!(status, button::Status::Hovered)).then(|| surface(t).scale_alpha(if selected { 1.0 } else { 0.6 }).into()),
            text_color: t.palette().text,
            border: Border { radius: round(12.0), ..Default::default() },
            ..Default::default()
        });
        tip(container(b).id(label), label, tooltip::Position::Right)
    };
    let unread = app.conversations.iter().any(|c| !c.archived && !c.request && (c.unread > 0 || c.marked_unread));
    let unviewed = app.stories.iter().any(|s| !s.viewed && s.expires_at() > now_ms());
    let me: Element<_> = match app.profile.as_ref().filter(|p| !p.given.is_empty()) {
        Some(p) => avatar(app, &p.given, &app.me.to_string(), 32.0),
        None => container(icon(icons::USER, 20)).center(32).into(),
    };
    let profile = button(me).padding(6).on_press(Message::ShowTab(Tab::Settings)).style(button::text);
    container(
        column![
            container(logo(36.0)).padding([4, 0]),
            item(icons::CHATS, "Chats", Tab::Chats, unread && current != Tab::Chats),
            item(icons::STORIES, "Stories", Tab::Stories, unviewed),
            Space::new().height(Length::Fill),
            item(icons::SETTINGS, "Settings", Tab::Settings, false),
            tip(profile, "Profile", tooltip::Position::Right),
        ]
        .spacing(8)
        .padding([12, 10])
        .align_x(Alignment::Center),
    )
    .height(Length::Fill)
    .style(|t: &Theme| container::Style { background: Some(rail(t).into()), ..Default::default() })
    .into()
}

fn link_view<'a>(
    device_name: &'a str,
    qr: Option<&'a qr_code::Data>,
    waiting: bool,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let mut col = column![
        logo(96.0),
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
    let flag = |src: &'static str, id: &'static str| container(icon(src, 14).style(|t: &Theme, _| svg::Style { color: Some(muted(t)) })).id(id);
    if c.pinned {
        top = top.push(flag(icons::PIN, "pinned"));
    }
    if c.muted {
        top = top.push(flag(icons::MUTED, "muted"));
    }
    if c.last_ts > 0 {
        top = top.push(text(short_time(c.last_ts)).size(12).style(dim));
    }
    let typing = app.typing_names(&c.thread);
    let preview = if !typing.is_empty() { "typing…".to_string() } else { truncate(&c.preview, 48) };
    let mut bottom = row![text(preview).size(13).wrapping(text::Wrapping::None).width(Length::Fill).style(dim)];
    if c.unread > 0 {
        bottom = bottom.push(
            container(text(c.unread.to_string()).size(12).style(|t: &Theme| text::Style { color: Some(on_accent_color(t)) }))
                .padding([1, 7])
                .style(|t: &Theme| container::Style {
                    background: Some(accent(t).into()),
                    border: Border { radius: round(10.0), ..Default::default() },
                    ..Default::default()
                }),
        );
    } else if c.marked_unread {
        bottom = bottom.push(container(Space::new()).width(10).height(10).style(|t: &Theme| container::Style {
            background: Some(accent(t).into()),
            border: Border { radius: round(5.0), ..Default::default() },
            ..Default::default()
        }));
    }
    let item = row![avatar(app, &c.title, &seed, 48.0), column![top, bottom].spacing(4)]
        .spacing(12)
        .align_y(Alignment::Center);
    let open = button(item)
        .width(Length::Fill)
        .padding([10, 10])
        .on_press(Message::OpenThread(c.thread.clone(), c.title.clone(), c.is_group))
        .style(move |t: &Theme, status| {
            let bg = match (selected, status) {
                (true, _) => Some(surface(t).into()),
                (_, button::Status::Hovered) => Some(surface(t).scale_alpha(0.5).into()),
                _ => None,
            };
            button::Style { background: bg, text_color: t.palette().text, border: Border { radius: round(10.0), ..Default::default() }, ..Default::default() }
        });
    // iced has no context menu: a "⋯" on hover opens the row's actions (not for requests).
    let menu_open = app.row_menu.as_ref() == Some(&c.thread);
    let mut line = row![open].align_y(Alignment::Center);
    if !c.request && (menu_open || app.row_hovered.as_ref() == Some(&c.thread)) {
        line = line.push(icon_button(icons::MORE, "Chat actions", Some(Message::RowMenu(Some(c.thread.clone())))));
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
    mouse_area(container(col).padding([0, 8])).on_enter(Message::RowHover(Some(c.thread.clone()))).on_exit(Message::RowHover(None)).into()
}

fn sidebar(app: &App) -> Element<'_, Message> {
    let q = app.search.trim().to_lowercase();
    let shown = |c: &&Conversation| q.is_empty() || c.title.to_lowercase().contains(&q);
    let section = |src: &'static str, label: String, msg| {
        let content = row![icon(src, 16).style(|t: &Theme, _| svg::Style { color: Some(muted(t)) }), text(label).size(13)]
            .spacing(10)
            .align_y(Alignment::Center);
        container(button(content).width(Length::Fill).padding([10, 12]).on_press(msg).style(button::text)).padding([0, 8])
    };
    let mut list = Column::new();
    if app.show_archived {
        list = list.push(section(icons::BACK, "Archived chats".into(), Message::ShowArchived(false)));
        for c in app.conversations.iter().filter(|c| c.archived && !c.request).filter(shown) {
            list = list.push(chat_row(app, c));
        }
    } else {
        let requests: Vec<&Conversation> = app.conversations.iter().filter(|c| c.request).filter(shown).collect();
        if !requests.is_empty() {
            let arrow = if app.requests_open { "▾" } else { "▸" };
            list = list.push(section(icons::USER, format!("{arrow} Message requests ({})", requests.len()), Message::ToggleRequests));
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
            list = list.push(section(icons::ARCHIVE, format!("Archived chats ({archived})"), Message::ShowArchived(true)));
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
    let (title, list) = if app.stories_open { ("Stories", story_rows(app)) } else { ("Chats", list) };
    let mut header = row![text(title).size(22).font(BOLD).width(Length::Fill)].align_y(Alignment::Center);
    if !app.stories_open {
        header = header.push(icon_button(icons::COMPOSE, "New group", Some(Message::NewGroup)));
    }
    let search = row![
        icon(icons::SEARCH, 16).style(|t: &Theme, _| svg::Style { color: Some(muted(t)) }),
        text_input("Search", &app.search).on_input(Message::Search).padding([6, 0]).style(|t: &Theme, st| text_input::Style {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            ..text_input::default(t, st)
        }),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let search = container(search).padding([2, 12]).style(|t: &Theme| container::Style {
        background: Some(pill(t)),
        border: Border { radius: round(18.0), ..Default::default() },
        ..Default::default()
    });
    column![
        container(header).padding(iced::Padding { top: 14.0, right: 12.0, bottom: 0.0, left: 18.0 }),
        container(search).padding([0, 16]),
        scrollable(list).height(Length::Fill),
    ]
    .spacing(12)
    .width(340)
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
            border: Border { radius: round(22.0), width: 2.0, color: if unviewed { accent(t) } else { muted(t) } },
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
                border: Border { radius: round(6.0), ..Default::default() },
                ..Default::default()
            });
        let bg = story_background(t);
        return container(label)
            .padding(24)
            .width(360)
            .height(640)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .style(move |_| container::Style { background: Some(bg), border: Border { radius: round(12.0), ..Default::default() }, ..Default::default() })
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
            border: Border { radius: round(2.0), ..Default::default() },
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
        avatar(app, &o.title, &crate::state::thread_key(&o.thread), 36.0),
        column![text(o.title.clone()).size(16).font(BOLD), subtitle],
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    // Like Signal-Desktop, clicking a group's header opens its details.
    let who: Element<_> = if o.is_group {
        button(who).on_press(Message::ToggleGroupInfo).padding(0).style(button::text).into()
    } else {
        who.into()
    };
    let mut actions = row![call_button(app, o), timer_control(app, o)].spacing(4).align_y(Alignment::Center);
    if o.is_group {
        actions = actions.push(icon_button(icons::INFO, "Group details", Some(Message::ToggleGroupInfo)));
    }
    let header = container(row![container(who).width(Length::Fill), actions].spacing(12).align_y(Alignment::Center)).padding([10, 16]);

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
    // The timer menu drops down over the timeline, like Signal-Desktop's.
    let body: Element<_> = match &o.timer_menu {
        Some(custom) => stack![body, container(timer_menu(app, o, custom)).align_right(Length::Fill).padding([6, 16])].into(),
        None => body,
    };
    column![header, rule::horizontal(1), body, bottom].into()
}

/// "📞" in a 1:1 chat's header, when this build has calls; idle while another call is up.
fn call_button<'a>(app: &App, o: &Open) -> Element<'a, Message> {
    let presage::store::Thread::Contact(peer) = &o.thread else { return Space::new().into() };
    if !crate::calls::ENABLED || app.is_request(&o.thread) {
        return Space::new().into();
    }
    let idle = app.call.as_ref().is_none_or(|c| matches!(c.phase, Phase::Ended(_)));
    icon_button(icons::PHONE, "Call", idle.then(|| Message::Call(CallCommand::Start(peer.raw_uuid()))))
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
    let label = row![icon(icons::TIMER, 18), text(label).size(13)].spacing(6).align_y(Alignment::Center);
    let b = button(label).padding([8, 10]).style(|t: &Theme, status| button::Style {
        background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| surface(t).into()),
        text_color: t.palette().text,
        border: Border { radius: round(18.0), ..Default::default() },
        ..Default::default()
    });
    tip(b.on_press_maybe(app.can_set_timer().then_some(Message::ToggleTimerMenu)), "Disappearing messages", tooltip::Position::Bottom)
}

/// "30 seconds", "1 week", "Off": the timer menu's labels.
pub fn long_duration(secs: u32) -> String {
    if secs == 0 {
        return "Off".into();
    }
    let &(u, unit) = TIMER_UNITS.iter().rev().find(|&&(u, _)| secs.is_multiple_of(u)).expect("every time divides into seconds");
    match secs / u {
        1 => format!("1 {unit}"),
        n => format!("{n} {unit}s"),
    }
}

/// Units for a custom disappearing time: (seconds, name).
pub const TIMER_UNITS: [(u32, &str); 5] = [(1, "second"), (60, "minute"), (3_600, "hour"), (86_400, "day"), (604_800, "week")];

/// The disappearing-messages dropdown: Signal's presets with a check on the current one, then a custom time.
fn timer_menu<'a>(app: &'a App, o: &'a Open, custom: &'a crate::app::CustomTimer) -> Element<'a, Message> {
    let current = app.timer(&o.thread);
    let row_style = |selected: bool| {
        move |t: &Theme, status: button::Status| button::Style {
            background: (selected || matches!(status, button::Status::Hovered)).then(|| surface(t).scale_alpha(if selected { 1.0 } else { 0.6 }).into()),
            text_color: t.palette().text,
            border: Border { radius: round(8.0), ..Default::default() },
            ..Default::default()
        }
    };
    let mut list = column![
        text("Disappearing messages").font(BOLD),
        muted_text("New messages in this chat disappear after the time you pick, for everyone."),
    ]
    .spacing(4)
    .width(300);
    let preset = TIMERS.iter().any(|&(s, _)| s == current);
    for &(secs, _) in &TIMERS {
        let check: Element<_> = if secs == current { icon(icons::CHECK, 16).style(|t: &Theme, _| svg::Style { color: Some(accent(t)) }).into() } else { Space::new().width(16).into() };
        let label = row![text(long_duration(secs)).width(Length::Fill), check].align_y(Alignment::Center);
        list = list.push(button(label).width(Length::Fill).padding([7, 10]).on_press(Message::SetTimer(secs)).style(row_style(secs == current)));
    }
    if !preset {
        let label = row![text(format!("Custom: {}", long_duration(current))).width(Length::Fill), icon(icons::CHECK, 16).style(|t: &Theme, _| svg::Style { color: Some(accent(t)) })];
        list = list.push(container(label.align_y(Alignment::Center)).padding([7, 10]).style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: round(8.0), ..Default::default() },
            ..Default::default()
        }));
    }
    list = list.push(rule::horizontal(1));
    let units = TIMER_UNITS.iter().fold(row![].spacing(4), |r, &(u, name)| {
        let selected = custom.unit == u;
        r.push(button(text(format!("{name}s")).size(12)).padding([4, 8]).on_press(Message::TimerUnit(u)).style(move |t: &Theme, st| {
            let mut b = row_style(selected)(t, st);
            b.border.radius = round(12.0);
            b
        }))
    });
    let set = custom.seconds().filter(|&s| s != current).map(Message::SetTimer);
    list = list
        .push(text("Custom time").size(13).font(BOLD))
        .push(
            row![
                text_input("Amount", &custom.amount).id("timer-amount").on_input(Message::TimerAmount).on_submit_maybe(set.clone()).padding([6, 10]).width(90),
                button(text("Set")).padding([6, 14]).on_press_maybe(set).style(button::primary),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .push(units.wrap());
    opaque(container(list.spacing(4)).padding(12).style(|t: &Theme| container::Style {
        background: Some(t.palette().background.into()),
        border: Border { radius: round(14.0), width: 1.0, color: surface(t) },
        shadow: iced::Shadow { color: Color::BLACK.scale_alpha(0.25), offset: iced::Vector::new(0.0, 4.0), blur_radius: 16.0 },
        ..Default::default()
    }))
}

fn muted_text<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(13).style(|t: &Theme| text::Style { color: Some(muted(t)) })
}

fn badge<'a>(s: &'a str) -> Element<'a, Message> {
    container(text(s).size(11))
        .padding([1, 6])
        .style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: round(8.0), ..Default::default() },
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

const SECTIONS: [(SettingsSection, &str, &str); 7] = [
    (SettingsSection::Profile, icons::USER, "Profile"),
    (SettingsSection::Appearance, icons::PALETTE, "Appearance"),
    (SettingsSection::Chats, icons::CHATS, "Chats"),
    (SettingsSection::Notifications, icons::BELL, "Notifications"),
    (SettingsSection::Plugins, icons::PUZZLE, "Plugins"),
    (SettingsSection::Devices, icons::DEVICES, "Linked devices"),
    (SettingsSection::About, icons::ABOUT, "About"),
];

/// The settings' page list, in the chat list's place (as in Signal-Desktop).
fn settings_nav<'a>(app: &'a App, s: &'a crate::app::SettingsPanel) -> Element<'a, Message> {
    let p = &s.profile;
    let name = format!("{} {}", p.given, p.family).trim().to_owned();
    let card = row![
        avatar(app, &p.given, &app.me.to_string(), 48.0),
        column![text(if name.is_empty() { "Your profile".into() } else { name }).font(BOLD), muted_text(app.phone.clone())].spacing(2),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let mut col = column![
        container(text("Settings").size(22).font(BOLD)).padding(iced::Padding { top: 14.0, right: 12.0, bottom: 4.0, left: 18.0 }),
        container(button(card).width(Length::Fill).padding([10, 10]).on_press(Message::SettingsSection(SettingsSection::Profile)).style(button::text)).padding([0, 8]),
    ]
    .spacing(6);
    for (section, src, label) in SECTIONS {
        let selected = s.section == section;
        let item = button(row![icon(src, 20), text(label)].spacing(14).align_y(Alignment::Center))
            .width(Length::Fill)
            .padding([10, 12])
            .on_press(Message::SettingsSection(section))
            .style(move |t: &Theme, status| button::Style {
                background: (selected || matches!(status, button::Status::Hovered))
                    .then(|| surface(t).scale_alpha(if selected { 1.0 } else { 0.5 }).into()),
                text_color: t.palette().text,
                border: Border { radius: round(10.0), ..Default::default() },
                ..Default::default()
            });
        col = col.push(container(item).padding([0, 8]));
    }
    scrollable(col).height(Length::Fill).width(340).into()
}

/// A titled group of settings rows on a raised card.
fn settings_card<'a>(title: &'static str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![
        text(title).size(13).font(BOLD).style(|t: &Theme| text::Style { color: Some(muted(t)) }),
        container(body).padding(16).width(Length::Fill).style(|t: &Theme| container::Style {
            background: Some(rail(t).into()),
            border: Border { radius: round(12.0), width: 1.0, color: surface(t) },
            ..Default::default()
        }),
    ]
    .spacing(8)
    .into()
}

/// One theme as a little chat preview: an incoming and an outgoing bubble on its background.
fn theme_card<'a>(app: &App, choice: ThemeChoice, label: &'static str) -> Element<'a, Message> {
    let dark = crate::state::Settings { theme: choice, ..app.settings.clone() }.dark(app.system_dark);
    theme_swatch(app_theme(choice, dark), label.to_owned(), app.settings.theme == choice, Message::SetTheme(choice))
}

fn theme_swatch<'a>(preview: Theme, label: String, selected: bool, press: Message) -> Element<'a, Message> {
    let (p, incoming) = (preview.palette(), surface(&preview));
    let bar = |w: f32, c: Color| container(Space::new()).width(w).height(14).style(move |_| container::Style {
        background: Some(c.into()),
        border: Border { radius: round(7.0), ..Default::default() },
        ..Default::default()
    });
    let chat = column![
        bar(78.0, incoming),
        container(bar(64.0, p.primary)).align_right(Length::Fill),
        bar(52.0, incoming),
    ]
    .spacing(7);
    let swatch = container(chat).padding(12).width(150).height(90).style(move |t: &Theme| container::Style {
        background: Some(p.background.into()),
        border: Border { radius: round(12.0), width: if selected { 3.0 } else { 1.0 }, color: if selected { accent(t) } else { surface(t) } },
        ..Default::default()
    });
    let mut caption = row![text(label).size(14)].spacing(6).align_y(Alignment::Center);
    if selected {
        caption = caption.push(icon(icons::CHECK, 16).style(|t: &Theme, _| svg::Style { color: Some(accent(t)) }));
    }
    button(column![swatch, caption].spacing(8).align_x(Alignment::Center))
        .padding(4)
        .on_press(press)
        .style(button::text)
        .into()
}

/// Custom themes: a card each (with Edit), "New theme", and the editor when one is open.
fn custom_themes<'a>(app: &'a App, s: &'a crate::app::SettingsPanel) -> Element<'a, Message> {
    if let Some((saved, d)) = &s.theme_draft {
        return theme_editor(saved.is_some(), d);
    }
    let selected = app.selected_custom_theme().map(|t| t.name.as_str());
    let cards = app.custom_themes.iter().filter_map(|t| Some((t, custom_theme(t)?))).fold(row![].spacing(12), |r, (t, preview)| {
        let edit = button(text("Edit").size(12)).padding([2, 10]).on_press(Message::EditTheme(Some(t.clone()))).style(button::text);
        r.push(column![theme_swatch(preview, t.name.clone(), selected == Some(t.name.as_str()), Message::SetCustomTheme(t.name.clone())), edit].align_x(Alignment::Center))
    });
    let new = button(container(column![icon(icons::EDIT, 22), text("New theme").size(14)].spacing(8).align_x(Alignment::Center)).center_x(150).center_y(90))
        .padding(4)
        .on_press(Message::EditTheme(None))
        .style(|t: &Theme, st| button::Style {
            background: matches!(st, button::Status::Hovered).then(|| surface(t).into()),
            text_color: t.palette().text,
            border: Border { radius: round(12.0), width: 1.0, color: surface(t) },
            ..Default::default()
        });
    column![
        cards.push(new).wrap().vertical_spacing(12),
        row![
            muted_text("Themes are JSON files: share them, or drop others' into the themes folder.").width(Length::Fill),
            button(text("Open themes folder").size(13)).on_press(Message::OpenThemesFolder).style(button::text),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(12)
    .into()
}

fn theme_editor<'a>(saved: bool, d: &'a crate::themes::CustomTheme) -> Element<'a, Message> {
    let field = |label: &'static str, value: &'a str, set: fn(&mut crate::themes::CustomTheme, String)| {
        let swatch: Element<_> = match crate::themes::hex(value) {
            Some(c) => container(Space::new()).width(28).height(28).style(move |t: &Theme| container::Style {
                background: Some(c.into()),
                border: Border { radius: round(14.0), width: 1.0, color: surface(t) },
                ..Default::default()
            }).into(),
            None => text("✕").style(text::danger).into(),
        };
        let d = d.clone();
        row![
            text(label).width(110),
            text_input("#rrggbb", value).on_input(move |v| {
                let mut d = d.clone();
                set(&mut d, v);
                Message::ThemeDraft(d)
            }).padding(8).width(140),
            swatch,
        ]
        .spacing(10)
        .align_y(Alignment::Center)
    };
    let name = {
        let d = d.clone();
        text_input("Theme name", &d.name.clone()).id("theme-name").on_input(move |v| Message::ThemeDraft(crate::themes::CustomTheme { name: v, ..d.clone() })).padding(8)
    };
    let preview: Element<_> = match custom_theme(d) {
        Some(t) => theme_swatch(t, if d.name.trim().is_empty() { "Preview".into() } else { d.name.clone() }, false, Message::ThemeDraft(d.clone())),
        None => muted_text("Enter three colors as #rrggbb to see a preview.").into(),
    };
    let mut actions = row![
        button(text("Save theme")).padding([8, 16]).on_press_maybe(d.is_valid().then_some(Message::SaveTheme)).style(button::primary),
        button(text("Cancel")).padding([8, 16]).on_press(Message::CancelThemeEdit).style(button::secondary),
    ]
    .spacing(8);
    if saved {
        actions = actions.push(button(text("Delete")).padding([8, 16]).on_press(Message::DeleteTheme).style(button::danger));
    }
    row![
        column![
            name,
            field("Background", &d.background, |d, v| d.background = v),
            field("Text", &d.text, |d, v| d.text = v),
            field("Accent", &d.accent, |d, v| d.accent = v),
            actions,
        ]
        .spacing(10)
        .width(Length::Fill),
        preview,
    ]
    .spacing(20)
    .align_y(Alignment::Center)
    .into()
}

/// A row of mutually exclusive choices, the selected one in the accent color.
fn choices<'a, T: Copy + PartialEq + 'a>(current: T, options: &[(T, &'a str)], msg: impl Fn(T) -> Message) -> Element<'a, Message> {
    options
        .iter()
        .fold(row![].spacing(6), |r, &(v, label)| {
            let selected = current == v;
            r.push(button(text(label)).padding([8, 16]).on_press(msg(v)).style(move |t: &Theme, status| button::Style {
                background: Some(if selected {
                    accent(t).into()
                } else if matches!(status, button::Status::Hovered) {
                    surface(t).into()
                } else {
                    t.palette().background.into()
                }),
                text_color: if selected { on_accent_color(t) } else { t.palette().text },
                border: Border { radius: round(18.0), width: 1.0, color: if selected { accent(t) } else { surface(t) } },
                ..Default::default()
            }))
        })
        .into()
}

/// Our signal.me link as a QR code, with "Copy link".
fn share_card(app: &App) -> Element<'_, Message> {
    let Some((link, qr)) = &app.share else { return muted_text("Loading…").into() };
    let code = container(qr_code(qr).cell_size(5).style(qr_style)).padding(12).style(|_| container::Style {
        background: Some(Color::WHITE.into()),
        border: Border { radius: round(12.0), ..Default::default() },
        ..Default::default()
    });
    let copy = button(row![tinted(icons::COPY, 16, Color::WHITE), text("Copy link")].spacing(8).align_y(Alignment::Center))
        .padding([8, 16])
        .on_press(Message::Copy(link.clone()))
        .style(button::primary);
    row![
        code,
        column![
            row![icon(icons::QR, 18), text("Scan to chat").font(BOLD)].spacing(8).align_y(Alignment::Center),
            muted_text("Anyone who scans this with Signal can start a chat with you, and sees your phone number."),
            text(link.clone()).size(13),
            copy,
        ]
        .spacing(10),
    ]
    .spacing(20)
    .align_y(Alignment::Center)
    .into()
}

/// The selected settings page: profile, appearance, chats, notifications, linked devices or about.
fn settings<'a>(app: &'a App, s: &'a crate::app::SettingsPanel) -> Element<'a, Message> {
    let title = SECTIONS.iter().find(|(sec, ..)| *sec == s.section).map_or("Settings", |(_, _, l)| l);
    let mut col = Column::new().spacing(20).padding([24, 32]).max_width(640);
    col = col.push(text(title).size(22).font(BOLD));
    match s.section {
        SettingsSection::Profile => {
            let p = &s.profile;
            let edit = |f: fn(&mut crate::backend::OwnProfile, String)| {
                move |v: String| {
                    let mut p = p.clone();
                    f(&mut p, v);
                    Message::EditProfile(p)
                }
            };
            let has_photo = app.avatars.contains_key(&app.me.to_string());
            let photo_actions: Element<_> = if s.photo_busy {
                muted_text("Uploading…").into()
            } else {
                let mut r = row![button(text("Change photo")).padding([6, 14]).on_press(Message::ChangePhoto).style(button::secondary)].spacing(8);
                if has_photo {
                    r = r.push(button(text("Remove")).padding([6, 14]).on_press(Message::RemovePhoto).style(button::text));
                }
                r.into()
            };
            col = col.push(
                column![
                    avatar(app, &p.given, &app.me.to_string(), 88.0),
                    photo_actions,
                    text(if app.phone.is_empty() { "…".to_string() } else { app.phone.clone() }).size(16),
                    muted_text(format!("ACI {}", app.me)).size(12),
                ]
                .spacing(6)
                .align_x(Alignment::Center)
                .width(Length::Fill),
            );
            let form = column![
                row![
                    text_input("First name (required)", &p.given).id("profile-given").on_input(edit(|p, v| p.given = v)).padding(10),
                    text_input("Last name", &p.family).on_input(edit(|p, v| p.family = v)).padding(10),
                ]
                .spacing(8),
                row![
                    text_input("😀", &p.emoji).on_input(edit(|p, v| p.emoji = v)).padding(10).width(64),
                    text_input("About", &p.about).on_input(edit(|p, v| p.about = v)).padding(10),
                ]
                .spacing(8),
                button(text("Save profile")).padding([8, 16]).on_press_maybe(app.can_save_profile(p).then_some(Message::SaveProfile)).style(button::primary),
            ]
            .spacing(12);
            col = col.push(settings_card("Name and about", form));
            col = col.push(settings_card("Share your profile", share_card(app)));
        }
        SettingsSection::Appearance => {
            let cards = THEMES.iter().fold(row![].spacing(12), |r, &(choice, label)| r.push(theme_card(app, choice, label)));
            col = col.push(settings_card("Theme", cards.wrap().vertical_spacing(12)));
            col = col.push(muted_text("System follows your desktop's light or dark mode. The others always look the same."));
            let swatch = container(Space::new()).width(56).height(32).style(|t: &Theme| container::Style {
                background: Some(bubble_fill(accent(t))),
                border: Border { radius: round(16.0), ..Default::default() },
                ..Default::default()
            });
            let rounding = column![
                row![choices(app.settings.square, &[(false, "Round"), (true, "Square")], Message::SetSquare), swatch]
                    .spacing(16)
                    .align_y(Alignment::Center),
                muted_text("The corners of message bubbles, avatars, photos, panels and buttons."),
            ]
            .spacing(12);
            col = col.push(settings_card("Rounding", rounding));
            col = col.push(settings_card("Custom themes", custom_themes(app, s)));
        }
        SettingsSection::Chats => {
            let (key, signup) = match app.settings.gif_provider {
                GifProvider::Giphy => ("GIPHY API key", "Get a free key at developers.giphy.com."),
                GifProvider::Tenor => ("Tenor API key", "Get a key in the Google Cloud console (Tenor API); restrict it to that API."),
            };
            let gifs = column![
                choices(app.settings.gif_provider, &[(GifProvider::Giphy, "GIPHY"), (GifProvider::Tenor, "Tenor")], Message::SetGifProvider),
                text_input(key, app.settings.gif_key()).id("gif-key").on_input(Message::SetGifKey).padding(10),
                muted_text(format!("{signup} Searches go straight to the provider, which sees your IP address.")),
            ]
            .spacing(12);
            col = col.push(settings_card("GIFs", gifs));
        }
        SettingsSection::Notifications => {
            let text_toggle = checkbox(app.settings.notification_text).label("Show message text in notifications");
            let body = column![
                checkbox(app.settings.notifications).label("Show notifications").on_toggle(Message::SetNotifications),
                if app.settings.notifications { text_toggle.on_toggle(Message::SetNotificationText) } else { text_toggle },
            ]
            .spacing(14);
            col = col.push(settings_card("Notifications", body));
        }
        SettingsSection::Plugins => {
            let mut list = Column::new().spacing(14);
            if app.plugins.is_empty() {
                list = list.push(muted_text("No plugins yet. Put an executable program in the plugins folder, then come back here."));
            }
            for p in &app.plugins {
                let on = app.settings.plugins.contains(&p.name);
                let name = p.name.clone();
                let status = app.plugin_status.get(&p.name).cloned().unwrap_or_else(|| if on { "Starting…".into() } else { "Off".into() });
                list = list.push(
                    row![
                        icon(icons::PUZZLE, 20),
                        column![text(p.name.clone()).font(BOLD), muted_text(status).size(12)].spacing(2).width(Length::Fill),
                        checkbox(on).label("Enabled").on_toggle(move |v| Message::TogglePlugin(name.clone(), v)),
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center),
                );
            }
            col = col.push(settings_card("Installed", list));
            col = col.push(
                row![
                    muted_text("Plugins are programs that read your messages and can send messages for you. Only enable ones you trust; they run with your user's permissions. See docs/plugins.md.")
                        .width(Length::Fill),
                    button(text("Open plugins folder").size(13)).on_press(Message::OpenPluginsFolder).style(button::text),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        }
        SettingsSection::Devices => {
            let mut list = Column::new().spacing(12);
            if app.devices.is_empty() {
                list = list.push(muted_text("Loading…"));
            }
            for d in &app.devices {
                let mut r = row![icon(icons::DEVICES, 20), text(d.name.clone()).width(Length::Fill)].spacing(12).align_y(Alignment::Center);
                if d.this {
                    r = r.push(badge("This device"));
                }
                list = list.push(r);
            }
            col = col.push(settings_card("Devices", list));
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
        }
        SettingsSection::About => {
            col = col.push(
                column![
                    logo(128.0),
                    text("signal-rusted").size(24).font(BOLD),
                    muted_text(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    text("A native Signal desktop client written in Rust.").align_x(Alignment::Center),
                    muted_text("Unofficial: not affiliated with or endorsed by Signal Messenger, LLC. AGPL-3.0-only."),
                ]
                .spacing(10)
                .align_x(Alignment::Center)
                .width(Length::Fill),
            );
        }
    }
    scrollable(col).height(Length::Fill).width(Length::Fill).into()
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

/// Signal's receipt marks: dashed ring, ringed check, two rings, two filled rings.
fn delivery_mark<'a>(d: Delivery, color: Color) -> Element<'a, Message> {
    let (src, id) = match d {
        Delivery::Failed => return text("⚠ Not sent").size(11).color(color).into(),
        Delivery::Sending => (icons::SENDING, "receipt-sending"),
        Delivery::Sent => (icons::SENT, "receipt-sent"),
        Delivery::Delivered => (icons::DELIVERED, "receipt-delivered"),
        Delivery::Read => (icons::READ, "receipt-read"),
    };
    container(tinted(src, 15, color)).id(id).into()
}

/// A bubble's diagonal sheen: lighter top-left, deeper bottom-right.
fn bubble_fill(c: Color) -> Background {
    // Pale fills (light-theme incoming) only get a faint shade, or they look dirty.
    let (light, dark) = if c.r + c.g + c.b > 2.4 { (0.0, 0.05) } else { (0.12, 0.12) };
    let linear = iced::gradient::Linear::new(iced::Radians(std::f32::consts::FRAC_PI_4 * 3.0))
        .add_stop(0.0, mix(c, Color::WHITE, light))
        .add_stop(1.0, mix(c, Color::BLACK, dark));
    Background::Gradient(linear.into())
}

/// A round button that stands out on either bubble: accent on grey, white on accent.
/// `label` is its widget id (for tests).
fn bubble_button<'a>(src: &'static str, label: &'static str, press: Option<Message>, on_accent: bool) -> Element<'a, Message> {
    let b = button(container(icon(src, 18).style(move |t: &Theme, _| svg::Style { color: Some(if on_accent { accent(t) } else { on_accent_color(t) }) })).center(18))
        .padding(9)
        .on_press_maybe(press)
        .style(move |t: &Theme, status| {
            let base = if on_accent { Color::WHITE } else { accent(t) };
            let bg = match status {
                button::Status::Hovered => mix(base, Color::BLACK, 0.1),
                button::Status::Disabled => base.scale_alpha(0.5),
                _ => base,
            };
            button::Style { background: Some(bg.into()), border: Border { radius: round(18.0), ..Default::default() }, ..Default::default() }
        });
    container(b).id(label).into()
}

/// The badge text and color of a file's icon: its extension, colored by kind (Word blue,
/// Excel green, PowerPoint orange, PDF red…); the MIME type covers names without one.
pub fn file_kind(name: Option<&str>, content_type: Option<&str>) -> (String, Color) {
    let ext = name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).filter(|e| !e.is_empty() && e.len() <= 8);
    let ct = content_type.unwrap_or("");
    let has = |words: &[&str]| words.iter().any(|w| ct.contains(w));
    let color = match ext.as_deref().unwrap_or("") {
        "pdf" => color!(0xe5484d),
        "doc" | "docx" | "odt" | "rtf" | "pages" => color!(0x2b6be5),
        "xls" | "xlsx" | "ods" | "csv" | "tsv" | "numbers" => color!(0x1f9d55),
        "ppt" | "pptx" | "odp" | "key" => color!(0xe8710a),
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" => color!(0xc08a00),
        "mp3" | "wav" | "ogg" | "m4a" | "flac" | "aac" | "opus" => color!(0x8e4ec6),
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" => color!(0xd6409f),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "svg" | "bmp" | "tiff" => color!(0x12a594),
        "rs" | "py" | "js" | "ts" | "json" | "html" | "css" | "c" | "cpp" | "h" | "java" | "go" | "sh" | "toml" | "yaml" | "yml" | "xml" => {
            color!(0x5b5bd6)
        }
        "exe" | "msi" | "apk" | "dmg" | "deb" | "rpm" | "appimage" => color!(0x4b4f58),
        _ if ct == "application/pdf" => color!(0xe5484d),
        _ if has(&["msword", "wordprocessing"]) => color!(0x2b6be5),
        _ if has(&["excel", "spreadsheet"]) => color!(0x1f9d55),
        _ if has(&["powerpoint", "presentation"]) => color!(0xe8710a),
        _ if has(&["zip", "compressed", "x-tar"]) => color!(0xc08a00),
        _ if ct.starts_with("audio/") => color!(0x8e4ec6),
        _ if ct.starts_with("video/") => color!(0xd6409f),
        _ if ct.starts_with("image/") => color!(0x12a594),
        _ => color!(0x8b8d98),
    };
    let badge = ext.map_or_else(
        || ct.split_once('/').map(|(_, sub)| sub.rsplit(['.', '-', '+']).next().unwrap_or(sub)).filter(|s| s.len() <= 4 && !s.is_empty()).unwrap_or("file").to_owned(),
        |e| e.chars().take(4).collect(),
    );
    (badge.to_uppercase(), color)
}

/// "820 B", "14 KB", "3.2 MB".
fn file_size(bytes: u32) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{} KB", b.div_ceil(1024)),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

/// A file attachment as Signal shows it: a colored page with the extension, the name, its
/// size, and a round download / save / retry button.
fn file_card<'a>(app: &'a App, a: &AttachmentPointer, key: String, fg: Color) -> Element<'a, Message> {
    let ct = a.content_type.as_deref();
    let (badge, color) = file_kind(a.file_name.as_deref(), ct);
    let page = stack![
        tinted(icons::DOCUMENT, 40, color),
        container(text(badge.clone()).size(if badge.len() > 3 { 8 } else { 10 }).font(BOLD).color(Color::WHITE)).center_x(40).padding(iced::Padding { top: 20.0, ..Default::default() }),
    ];
    let tile = container(page).center(52).style(|t: &Theme| container::Style {
        background: Some(t.palette().background.into()),
        border: Border { radius: round(12.0), ..Default::default() },
        ..Default::default()
    });
    let fallback = match ct.unwrap_or("") {
        c if c.starts_with("image/") => "Photo",
        c if c.starts_with("video/") => "Video",
        _ => "File",
    };
    let name = a.file_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| fallback.into());
    let size = a.size.filter(|s| *s > 0).map(file_size);
    let on_accent = fg != app.theme().palette().text;
    let (detail, action) = match app.attachments.get(&key) {
        Some(Attach::Ready(_)) => (size, Some(bubble_button(icons::SAVE, "Save", Some(Message::SaveAttachment(key)), on_accent))),
        Some(Attach::Loading) => (Some("Downloading…".into()), Some(bubble_button(icons::SENDING, "Downloading", None, on_accent))),
        Some(Attach::Failed(e)) => (Some(format!("Download failed: {e}")), Some(bubble_button(icons::RETRY, "Retry", Some(Message::FetchAttachment(key)), on_accent))),
        // Our own just-sent attachment, not uploaded yet.
        None if a.cdn_number.is_none() && a.attachment_identifier.is_none() => (size, None),
        None => (size, Some(bubble_button(icons::DOWNLOAD, "Download", Some(Message::FetchAttachment(key)), on_accent))),
    };
    let mut info = column![text(name).font(BOLD).color(fg).wrapping(text::Wrapping::WordOrGlyph)].spacing(2).width(Length::Fill);
    if let Some(d) = detail {
        info = info.push(text(d).size(12).color(Color { a: 0.75, ..fg }));
    }
    let mut r = row![tile, info].spacing(12).align_y(Alignment::Center).width(300);
    if let Some(b) = action {
        r = r.push(b);
    }
    r.into()
}

/// Inline player: play/pause, progress and time; the first press downloads.
fn audio_player<'a>(app: &'a App, a: &AttachmentPointer, key: String, fg: Color) -> Element<'a, Message> {
    let now = std::time::Instant::now();
    let total = app.durations.get(&key).copied();
    let playing = app.playback.as_ref().filter(|p| p.key == key);
    let pos = playing.map(|p| p.position(now)).unwrap_or_default();
    let (src, label, press) = match app.attachments.get(&key) {
        Some(Attach::Loading) => (icons::SENDING, "Downloading", None),
        _ if a.cdn_number.is_none() && a.attachment_identifier.is_none() && !app.attachments.contains_key(&key) => (icons::PLAY, "Play", None),
        _ if playing.is_some_and(|p| !p.is_paused()) => (icons::PAUSE, "Pause", Some(Message::PlayPause(key.clone()))),
        _ => (icons::PLAY, "Play", Some(Message::PlayPause(key.clone()))),
    };
    let on_accent = fg != app.theme().palette().text;
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
        bubble_button(src, label, press, on_accent),
        progress_bar(0.0..=1.0, frac).length(140).girth(4).style(move |t: &Theme| progress_bar::Style {
            background: Color { a: 0.3, ..fg }.into(),
            bar: (if on_accent { Color::WHITE } else { accent(t) }).into(),
            border: Border { radius: round(2.0), ..Default::default() },
        }),
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
        Some(Attach::Ready(path)) if is_image => clickable_image(app, key, path, Length::Shrink, ContentFit::Contain),
        _ => file_card(app, a, key, fg),
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
fn clickable_image<'a>(app: &App, key: String, path: &std::path::Path, size: Length, fit: ContentFit) -> Element<'a, Message> {
    let img = image(image_handle(app, &key, path)).width(size).height(size).content_fit(fit).border_radius(round(14.0));
    mouse_area(container(img).id(format!("image:{key}")))
        .on_press(Message::ViewImage(key))
        .interaction(mouse::Interaction::Pointer)
        .into()
}

/// The image file, or the current frame of an animated GIF.
fn image_handle(app: &App, key: &str, path: &std::path::Path) -> image::Handle {
    match app.animations.get(key) {
        Some(a) => a.frame(app.now.saturating_duration_since(app.started)).clone(),
        None => image::Handle::from_path(path),
    }
}

/// Several images in one message: a grid, two per row.
fn image_grid<'a>(app: &'a App, images: &[&'a AttachmentPointer], fg: Color) -> Element<'a, Message> {
    images
        .chunks(2)
        .fold(Column::new().spacing(4), |c, pair| {
            c.push(pair.iter().fold(row![].spacing(4), |r, a| {
                let key = attachment_key(a);
                r.push(match app.attachments.get(&key) {
                    Some(Attach::Ready(path)) => clickable_image(app, key, path, Length::Fixed(THUMB), ContentFit::Cover),
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
        border: Border { radius: round(8.0), ..Default::default() },
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
            image(image_handle(app, key, path)).width(Length::Fill).height(Length::Fill).into()
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
    let images_only = !m.deleted
        && m.body.is_empty()
        && m.quote.is_none()
        && m.story_reply.is_none()
        && m.previews.is_empty()
        && !m.attachments.is_empty()
        && m.attachments.iter().all(is_image);
    // Stickers and bare photos have no bubble, so their footer sits on the timeline background.
    let bare = is_sticker || images_only;
    let fg = if m.outgoing && !bare { on_accent_color(&app.theme()) } else { app.theme().palette().text };
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
                        border: Border { radius: round(8.0), width: 0.0, color: Color::TRANSPARENT },
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
                    border: Border { radius: round(8.0), width: 0.0, color: Color::TRANSPARENT },
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
        footer = footer.push(delivery_mark(m.delivery, Color { a: 0.85, ..fg }));
    }
    bubble = bubble.push(footer);

    let bubble = container(bubble).padding(if images_only { [2, 4] } else { [8, 12] }).max_width(520).style(move |t: &Theme| container::Style {
        background: (!bare).then(|| bubble_fill(if m.outgoing { accent(t) } else { surface(t) })),
        border: Border { radius: round(18.0), ..Default::default() },
        ..Default::default()
    });

    let now = now_ms();
    let hovered = o.hovered == Some(key) || o.reacting == Some(key) || o.deleting == Some(key);
    let actions: Element<_> = if hovered && !m.deleted {
        let mut r = row![
            icon_button(icons::SMILE, "React", Some(Message::ToggleReactions(Some(key)))),
            icon_button(icons::REPLY, "Reply", Some(Message::Reply(m.author, m.id))),
        ];
        if m.can_edit(now) {
            r = r.push(icon_button(icons::EDIT, "Edit", Some(Message::Edit(m.id))));
        }
        r.push(icon_button(icons::TRASH, "Delete", Some(Message::ToggleDelete(Some(key))))).into()
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
            border: Border { radius: round(12.0), ..Default::default() },
            ..Default::default()
        }));
    }
    if o.reacting == Some(key) {
        let bar = emoji::REACTIONS
            .iter()
            .fold(row![].spacing(2), |r, e| r.push(button(text(*e).size(22)).on_press(Message::React(m.author, m.id, (*e).into())).style(button::text)));
        col = col.push(container(bar).padding(4).style(|t: &Theme| container::Style {
            background: Some(surface(t).into()),
            border: Border { radius: round(20.0), ..Default::default() },
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
                background: Some(if mine { accent(t).scale_alpha(0.35).into() } else { surface(t).into() }),
                text_color: t.palette().text,
                border: Border { radius: round(12.0), width: 1.0, color: t.palette().background },
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
    if let Some(panel) = fun_panel(app, o) {
        col = col.push(panel);
    }
    if let Some(q) = &o.quote {
        let who = if q.author == app.me { "You".into() } else { display_name(&app.names, q.author) };
        col = col.push(
            container(row![
                column![text(format!("Replying to {who}")).size(12).font(Font { weight: Weight::Bold, ..Font::DEFAULT }), text(truncate(&q.text, 90)).size(13)].width(Length::Fill),
                icon_button(icons::CLOSE, "Cancel reply", Some(Message::CancelReply)),
            ])
            .padding([4, 10])
            .style(|t: &Theme| container::Style {
                background: Some(surface(t).into()),
                border: Border { radius: round(8.0), ..Default::default() },
                ..Default::default()
            }),
        );
    }
    if let Some(id) = o.editing {
        let original = o.timeline.as_ref().and_then(|t| t.find(app.me, id)).map(Msg::preview).unwrap_or_default();
        col = col.push(
            container(row![
                column![text("Editing message").size(12).font(Font { weight: Weight::Bold, ..Font::DEFAULT }), text(truncate(&original, 90)).size(13)].width(Length::Fill),
                icon_button(icons::CLOSE, "Cancel edit", Some(Message::CancelEdit)),
            ])
            .padding([4, 10])
            .style(|t: &Theme| container::Style {
                background: Some(surface(t).into()),
                border: Border { radius: round(8.0), ..Default::default() },
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
            border: Border { radius: round(8.0), ..Default::default() },
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
        .placeholder("Message")
        .on_action(Message::Compose)
        .key_binding(|kp: KeyPress| {
            let enter = matches!(kp.key, keyboard::Key::Named(key::Named::Enter));
            if enter && !kp.modifiers.shift() {
                Some(Binding::Custom(Message::Send))
            } else {
                Binding::from_key_press(kp)
            }
        })
        .padding([10, 16])
        .height(Length::Shrink)
        .style(|t: &Theme, st| text_editor::Style {
            background: pill(t),
            border: Border { radius: round(20.0), ..Default::default() },
            ..text_editor::default(t, st)
        });
    let action: Element<_> = if draft.trim().is_empty() && o.files.is_empty() {
        icon_button(icons::MIC, "Voice note", Some(Message::Record))
    } else {
        let send = button(container(icon(icons::SEND, 18).style(|t: &Theme, _| svg::Style { color: Some(on_accent_color(t)) })).center(20)).padding(9).on_press(Message::Send).style(|t: &Theme, status| {
            button::Style {
                background: Some(if matches!(status, button::Status::Hovered) { mix(accent(t), Color::BLACK, 0.1) } else { accent(t) }.into()),
                border: Border { radius: round(20.0), ..Default::default() },
                ..Default::default()
            }
        });
        tip(container(send).id("Send"), "Send", tooltip::Position::Top)
    };
    col.push(
        row![
            icon_button(icons::SMILE, "Emoji", Some(Message::TogglePicker)),
            container(editor).max_height(180).width(Length::Fill),
            icon_button(icons::STICKER, "Stickers", Some(Message::ToggleStickers)),
            icon_button(icons::ATTACH, "Attach file", Some(Message::Attach)),
            action,
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .into()
}

/// Signal's "fun" panel: emoji, sticker and GIF pickers as tabs.
fn fun_panel<'a>(app: &'a App, o: &'a Open) -> Option<Element<'a, Message>> {
    let (active, body) = match (&o.picker, &o.stickers, &o.gifs) {
        (Some(p), ..) => (Fun::Emoji, emoji_picker(p)),
        (_, Some(tab), _) => (Fun::Stickers, sticker_picker(app, tab)),
        (.., Some(g)) => (Fun::Gifs, gif_picker(app, g)),
        _ => return None,
    };
    let tabs = [(Fun::Emoji, "Emoji"), (Fun::Stickers, "Stickers"), (Fun::Gifs, "GIFs")].into_iter().fold(row![].spacing(4), |r, (fun, label)| {
        let selected = fun == active;
        r.push(button(text(label).size(13)).padding([5, 14]).on_press(Message::OpenFun(fun)).style(move |t: &Theme, status| button::Style {
            background: if selected {
                Some(accent(t).into())
            } else {
                matches!(status, button::Status::Hovered).then(|| t.palette().background.into())
            },
            text_color: if selected { on_accent_color(t) } else { t.palette().text },
            border: Border { radius: round(14.0), ..Default::default() },
            ..Default::default()
        }))
    });
    Some(
        container(column![tabs, body].spacing(8))
            .width(Length::Fill)
            .padding(10)
            .style(|t: &Theme| container::Style {
                background: Some(surface(t).into()),
                border: Border { radius: round(14.0), ..Default::default() },
                ..Default::default()
            })
            .into(),
    )
}

/// Highlights the selected tab of a picker's sub-navigation.
fn tab_style(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |t, s| {
        let mut st = button::text(t, s);
        if selected {
            st.background = Some(t.palette().background.into());
        }
        st.border.radius = round(8.0);
        st
    }
}

/// The category icon for an emoji group, as in Signal's picker.
fn group_icon(g: emojis::Group) -> &'static str {
    use emojis::Group::*;
    match g {
        SmileysAndEmotion => icons::SMILE,
        PeopleAndBody => icons::USER,
        AnimalsAndNature => icons::PAW,
        FoodAndDrink => icons::APPLE,
        TravelAndPlaces => icons::PLANE,
        Activities => icons::TROPHY,
        Objects => icons::BULB,
        Symbols => icons::HEART,
        Flags => icons::FLAG,
    }
}

fn emoji_picker(p: &crate::app::Picker) -> Element<'_, Message> {
    let tabs = emojis::Group::iter().fold(row![].spacing(2), |r, g| {
        let selected = g == p.group && p.query.is_empty();
        r.push(button(icon(group_icon(g), 18)).padding(6).on_press(Message::PickerGroup(g)).style(tab_style(selected)))
    });
    let grid = emoji::search(&p.query, p.group)
        .into_iter()
        .take(600)
        .fold(row![].spacing(2), |r, e| {
            r.push(button(text(e.as_str()).size(22)).padding(2).on_press(Message::InsertEmoji(e.as_str())).style(button::text))
        })
        .wrap();
    column![search_box("Search emoji", &p.query, Message::PickerQuery), tabs, scrollable(grid).height(220)].spacing(6).into()
}

/// A pill-shaped search field, like the chat list's.
fn search_box<'a>(placeholder: &'a str, value: &'a str, on_input: fn(String) -> Message) -> Element<'a, Message> {
    text_input(placeholder, value)
        .on_input(on_input)
        .padding([6, 12])
        .style(|t: &Theme, st| text_input::Style {
            background: t.palette().background.into(),
            border: Border { radius: round(16.0), ..Default::default() },
            ..text_input::default(t, st)
        })
        .into()
}

fn sticker_picker<'a>(app: &'a App, tab: &StickerTab) -> Element<'a, Message> {
    let mut tabs = row![button(icon(icons::CLOCK, 18)).padding(6).on_press(Message::StickerTab(StickerTab::Recent)).style(tab_style(*tab == StickerTab::Recent))]
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
        StickerTab::Recent if cells.is_empty() && app.packs.is_empty() => "No sticker packs yet: install one from a sticker someone sends you",
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
    column![tabs, muted_text(title).size(13), scrollable(grid).height(240)].spacing(6).into()
}

/// GIF search through the provider picked in settings; a click sends the GIF.
fn gif_picker<'a>(app: &'a App, g: &'a crate::app::GifPanel) -> Element<'a, Message> {
    // Both providers' terms ask for attribution next to results.
    let attribution = match app.settings.gif_provider {
        GifProvider::Giphy => "Powered by GIPHY",
        GifProvider::Tenor => "Via Tenor",
    };
    let status: Element<_> = match (&g.error, g.loading) {
        (Some(e), _) => text(e.clone()).size(13).style(text::danger).into(),
        (None, true) => muted_text("Loading…").size(13).into(),
        (None, false) if g.results.is_empty() => muted_text("No GIFs found").size(13).into(),
        (None, false) => muted_text(if g.query.trim().is_empty() { "Trending" } else { "Results" }).size(13).into(),
    };
    let grid = g
        .results
        .iter()
        .enumerate()
        .fold(row![].spacing(4), |r, (i, (gif, h))| {
            let cell = container(image(h.clone()).width(128).height(96).content_fit(ContentFit::Cover).border_radius(round(8.0))).id(format!("gif-{i}"));
            r.push(button(cell).padding(2).on_press(Message::SendGif(gif.url.clone())).style(button::text))
        })
        .wrap();
    column![
        row![container(search_box("Search GIFs", &g.query, Message::GifQuery)).width(Length::Fill), muted_text(attribution).size(11)]
            .spacing(8)
            .align_y(Alignment::Center),
        status,
        scrollable(grid).height(260),
    ]
    .spacing(6)
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
    fn ringing_banner_shows_above_the_notice_until_cleared() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let banner = "📞 Alice is calling (video) · answer on your phone";
        app.update(Message::Backend(Event::Notice(Some("Importing…".into()))));
        app.update(Message::Backend(Event::Ringing(Some(banner.into()))));
        let mut ui = simulator(view(&app));
        assert!(ui.find(banner).is_ok() && ui.find("Importing…").is_ok());
        assert!(ui.find("hi there").is_ok(), "the chat stays usable");
        drop(ui);
        app.update(Message::Backend(Event::Ringing(None)));
        let mut ui = simulator(view(&app));
        assert!(ui.find(banner).is_err() && ui.find("Importing…").is_ok());
    }

    #[test]
    fn qr_is_dark_on_light_in_every_theme() {
        for dark in [true, false] {
            let s = qr_style(&app_theme(ThemeChoice::System, dark));
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

    /// Writes `snapshots/files-*.png`: file cards in each state, incoming and outgoing.
    #[test]
    fn files_snapshot() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let file = |digest: u8, name: &str, ct: &str, size: u32| presage::proto::AttachmentPointer {
            content_type: Some(ct.into()),
            digest: Some(vec![digest]),
            cdn_number: Some(3),
            size: Some(size),
            file_name: Some(name.into()),
            ..Default::default()
        };
        let files = [
            (uuid(2), file(0xc1, "Quarterly report.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", 48_213)),
            (uuid(2), file(0xc2, "Launch deck.pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation", 3_355_443)),
            (app.me, file(0xc3, "contributos.txt", "text/plain", 820)),
            (uuid(2), file(0xc4, "invoice.pdf", "application/pdf", 120_000)),
            (app.me, file(0xc5, "budget.xlsx", "application/vnd.ms-excel", 22_000)),
            (uuid(2), file(0xc6, "photos.zip", "application/zip", 9_000_000)),
        ];
        let mut keys = vec![];
        for (i, (who, a)) in files.into_iter().enumerate() {
            keys.push(attachment_key(&a));
            let ts = 100 + i as u64;
            let dm = presage::proto::DataMessage { timestamp: Some(ts), attachments: vec![a], ..Default::default() };
            app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(who, ts, dm)) }));
        }
        app.attachments.insert(keys[1].clone(), Attach::Ready("/nonexistent/deck.pptx".into()));
        app.attachments.insert(keys[3].clone(), Attach::Loading);
        app.attachments.insert(keys[5].clone(), Attach::Failed("timed out".into()));
        app.attachments.remove(&keys[0]);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for theme in [ThemeChoice::Light, ThemeChoice::Rusted] {
            app.update(Message::SetTheme(theme));
            let name = format!("files-{theme:?}").to_lowercase();
            for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if f.file_name().to_string_lossy().starts_with(&name) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
            let snap = ui.snapshot(&app.theme()).expect("render");
            assert!(snap.matches_image(dir.join(name)).expect("write png"));
        }
        assert!(simulator(view(&app)).find(iced::widget::Id::new("Retry")).is_ok(), "failed download offers a retry");
    }

    #[test]
    fn file_icons_follow_the_file_type() {
        assert_eq!(file_kind(Some("Report.DOCX"), None), ("DOCX".into(), color!(0x2b6be5)));
        assert_eq!(file_kind(Some("deck.pptx"), None).1, color!(0xe8710a));
        assert_eq!(file_kind(Some("budget.xlsx"), None).1, color!(0x1f9d55));
        assert_eq!(file_kind(Some("scan.pdf"), Some("application/pdf")).0, "PDF");
        assert_eq!(file_kind(None, Some("application/pdf")), ("PDF".into(), color!(0xe5484d)));
        assert_eq!(file_kind(Some("notes"), Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")).1, color!(0x2b6be5));
        assert_eq!(file_kind(Some("contributos.txt"), Some("text/plain")).0, "TXT");
        assert_eq!(file_kind(None, None).0, "FILE");
        assert_eq!((file_size(820), file_size(14_000), file_size(3_355_443)), ("820 B".into(), "14 KB".into(), "3.2 MB".into()));
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

    /// README screenshots and GIF frames, from fake data only: `scripts/readme-media.sh` runs this
    /// and turns `snapshots/readme/` into `docs/images/`.
    #[test]
    #[ignore = "writes README media; run through scripts/readme-media.sh"]
    fn readme_media() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots/readme");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frame = |app: &App, name: &str| {
            let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(app));
            assert!(ui.snapshot(&app.theme()).expect("render").matches_image(dir.join(name)).expect("write png"));
        };

        // A fuller chat list around the demo conversation.
        let mut app = demo();
        let base = app.conversations[0].clone();
        let chat = |title: &str, n: u8, preview: &str, ts, group: bool, unread| Conversation {
            thread: if group { presage::store::Thread::Group([n; 32]) } else { presage::store::Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(uuid(n).into())) },
            title: title.into(),
            preview: preview.into(),
            last_ts: ts,
            unread,
            is_group: group,
            ..base.clone()
        };
        let mut list = vec![
            Conversation { last_ts: now_ms(), preview: "👍🏽❤️".into(), unread: 0, ..base.clone() },
            chat("Weekend trip", 7, "Bob: who's bringing the tent?", now_ms() - 600_000, true, 3),
            chat("Bob", 3, "Voice message", now_ms() - 3_600_000, false, 0),
            chat("Rust meetup", 8, "Slides are up 🎉", now_ms() - 86_400_000, true, 0),
            chat("Note to Self", 1, "groceries: oat milk, coffee", now_ms() - 2 * 86_400_000, false, 0),
        ];
        list[1].pinned = true;
        list[4].muted = true;
        app.update(Message::Backend(Event::Conversations(list)));
        app.update(Message::SetTheme(ThemeChoice::Dark));
        frame(&app, "hero");

        // Every theme, then a custom one.
        for (i, &(choice, _)) in THEMES.iter().enumerate().skip(1) {
            app.update(Message::SetTheme(choice));
            frame(&app, &format!("theme-{i:02}"));
        }
        app.custom_themes = vec![crate::themes::CustomTheme { name: "Ocean".into(), background: "#0b1d2a".into(), text: "#e0f2ff".into(), accent: "#2bb3c0".into() }];
        app.update(Message::SetCustomTheme("Ocean".into()));
        frame(&app, "theme-99");

        // Round and square corners.
        app.update(Message::SetTheme(ThemeChoice::Rusted));
        frame(&app, "rounding-1");
        app.update(Message::SetSquare(true));
        frame(&app, "rounding-2");
        app.update(Message::SetSquare(false));

        // The emoji / sticker / GIF panel.
        app.update(Message::SetTheme(ThemeChoice::Dark));
        fake_packs(&mut app, vec![(vec![8], 1), (vec![8], 2), (vec![9], 3)]);
        let tile = |r, g, b| image::Handle::from_rgba(4, 3, [r, g, b, 255].repeat(12));
        // Snapshots don't decode encoded images; raw pixels show up.
        for (i, st) in app.packs.iter_mut().flat_map(|p| p.stickers.iter_mut()).enumerate() {
            st.image = [tile(0xe8, 0x64, 0x1f), tile(0x2b, 0xb3, 0xc0), tile(0xa7, 0xc0, 0x80)][i % 3].clone();
        }
        app.update(Message::OpenFun(Fun::Emoji));
        frame(&app, "fun-1");
        app.update(Message::OpenFun(Fun::Stickers));
        frame(&app, "fun-2");
        app.update(Message::OpenFun(Fun::Gifs));
        let gifs = [(0xe8, 0x64, 0x1f), (0x2b, 0xb3, 0xc0), (0xa7, 0xc0, 0x80), (0xd6, 0x45, 0x7a), (0x5e, 0x81, 0xac), (0xff, 0xc1, 0x07), (0x4c, 0xaf, 0x50), (0x7a, 0xa5, 0xf7)]
            .into_iter()
            .map(|(r, g, b)| (crate::gifs::Gif { title: String::new(), preview: String::new(), url: String::new() }, tile(r, g, b)))
            .collect();
        app.update(Message::GifResults(String::new(), Ok(gifs)));
        frame(&app, "fun-3");

        // The disappearing-messages menu.
        app.update(Message::TogglePicker);
        app.update(Message::ToggleTimerMenu);
        frame(&app, "timer-menu");

        // Settings pages.
        let (mut s, _rx) = settings_app();
        s.plugins = vec![
            crate::plugins::Plugin { name: "auto-reply.py".into(), path: "/plugins/auto-reply.py".into() },
            crate::plugins::Plugin { name: "reminders".into(), path: "/plugins/reminders".into() },
        ];
        s.update(Message::TogglePlugin("auto-reply.py".into(), true));
        s.plugin_status.insert("auto-reply.py".into(), "Running".into());
        s.update(Message::SetTheme(ThemeChoice::Dark));
        for (i, section) in [SettingsSection::Profile, SettingsSection::Appearance, SettingsSection::Chats, SettingsSection::Plugins, SettingsSection::Devices]
            .into_iter()
            .enumerate()
        {
            s.update(Message::SettingsSection(section));
            frame(&s, &format!("settings-{i}"));
        }
    }

    #[test]
    fn timeline_renders_quotes_reactions_and_status() {
        let app = demo();
        let mut ui = simulator(view(&app));
        assert!(ui.find("😂").is_ok(), "reaction pill");
        assert!(ui.find("👍🏽❤️").is_ok(), "jumbo emoji message");
        assert!(ui.find(iced::widget::Id::new("receipt-read")).is_ok(), "read receipt mark");
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
        for (dark, theme, name) in [
            (true, ThemeChoice::System, "conversation-dark"),
            (false, ThemeChoice::System, "conversation-light"),
            (true, ThemeChoice::Rusted, "conversation-rusted"),
        ] {
            app.dark = dark;
            app.settings.theme = theme;
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
        ui.click(iced::widget::Id::new("Send")).expect("send button");
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

    /// Clicks an icon button by its id (its tooltip label).
    fn click_id(app: &mut App, id: &'static str) {
        let mut ui = simulator(view(app));
        ui.click(iced::widget::Id::new(id)).unwrap_or_else(|e| panic!("{id}: {e:?}"));
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
    }

    fn has_id(app: &App, id: &'static str) -> bool {
        simulator(view(app)).find(iced::widget::Id::new(id)).is_ok()
    }

    #[test]
    fn hover_edit_then_send_through_the_ui() {
        let (mut app, mut rx) = app();
        let id = crate::app::tests::with_own_message(&mut app, &mut rx, "*typo*");
        app.open.as_mut().unwrap().hovered = Some((uuid(1), id));
        click_id(&mut app, "Edit");
        assert!(simulator(view(&app)).find("Editing message").is_ok(), "edit bar");
        assert_eq!(app.open.as_ref().unwrap().composer.text().trim_end(), "*typo*");
        app.update(Message::Compose(text_editor::Action::Edit(text_editor::Edit::Insert('!'))));
        click_id(&mut app, "Send");
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
        click_id(&mut app, "Delete");
        click(&mut app, "Delete for everyone");
        assert!(matches!(rx.try_recv(), Ok(Command::DeleteForEveryone { target_ts, .. }) if target_ts == id));
        assert!(simulator(view(&app)).find("This message was deleted.").is_ok());

        // Alice's message: no edit, and only "Delete for me".
        app.open.as_mut().unwrap().hovered = Some((uuid(2), 10));
        assert!(!has_id(&app, "Edit"));
        click_id(&mut app, "Delete");
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
        click_id(&mut app, "Stickers");
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
        ui.click(iced::widget::Id::new("Play")).expect("play button");
        assert!(ui.into_messages().any(|m| matches!(m, Message::PlayPause(k) if k == key)));

        app.attachments.insert(key.clone(), Attach::Ready("/nonexistent/a0.m4a".into()));
        app.durations.insert(key.clone(), std::time::Duration::from_secs(75));
        app.playback = Some(crate::audio::Playback::new(key, std::time::Instant::now()));
        let mut ui = simulator(view(&app));
        assert!(ui.find("0:00 / 1:15").is_ok(), "progress / length");
        assert!(ui.find(iced::widget::Id::new("Pause")).is_ok(), "pause while playing");
    }

    #[test]
    fn mic_replaces_send_and_records_a_voice_note() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        let audio = fake_audio(&mut app);
        let mut ui = simulator(view(&app));
        assert!(ui.find(iced::widget::Id::new("Send")).is_err(), "empty composer shows the mic instead of Send");
        ui.click(iced::widget::Id::new("Voice note")).expect("mic button");
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
        assert!(has_id(&app, "Voice note"), "back to the composer");
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
        ui.click("Off").expect("timer control in header");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        let mut ui = simulator(view(&app));
        ui.click("5 minutes").expect("timer choices");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert!(matches!(rx.try_recv(), Ok(crate::backend::Command::SetTimer { seconds: 300, .. })));
        assert!(simulator(view(&app)).find("5m").is_ok(), "header shows the new timer");
    }

    #[test]
    fn timer_menu_sets_a_custom_time() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::ToggleTimerMenu);
        assert!(simulator(view(&app)).find("4 weeks").is_ok());
        app.update(Message::TimerAmount("9x0".into()));
        assert_eq!(app.open.as_ref().unwrap().timer_menu.as_ref().unwrap().amount, "90", "digits only");
        click(&mut app, "minutes");
        click(&mut app, "Set");
        assert!(matches!(rx.try_recv(), Ok(Command::SetTimer { seconds: 5400, .. })));
        app.update(Message::ToggleTimerMenu);
        assert!(simulator(view(&app)).find("Custom: 90 minutes").is_ok(), "a non-preset time is shown as custom");
        assert_eq!(app.open.as_ref().unwrap().timer_menu.as_ref().unwrap().seconds(), None, "fresh entry");
    }

    /// Writes `snapshots/timer-menu-*.png`: the disappearing-messages dropdown over the chat.
    #[test]
    fn timer_menu_snapshot() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::SetTheme(ThemeChoice::Everforest));
        app.update(Message::ToggleTimerMenu);
        app.update(Message::TimerAmount("90".into()));
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots");
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().starts_with("timer-menu") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
        let snap = ui.snapshot(&app.theme()).expect("render");
        assert!(snap.matches_image(dir.join("timer-menu")).expect("write png"));
    }

    #[test]
    fn timer_labels() {
        assert_eq!(
            [0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200, 5_400, 90].map(long_duration),
            ["Off", "30 seconds", "5 minutes", "1 hour", "8 hours", "1 day", "1 week", "4 weeks", "90 minutes", "90 seconds"]
        );
        let c = |amount: &str, unit| crate::app::CustomTimer { amount: amount.into(), unit };
        assert_eq!((c("2", 86_400).seconds(), c("0", 60).seconds(), c("53", 604_800).seconds()), (Some(172_800), None, None), "a year at most");
    }

    #[test]
    fn everforest_puts_dark_text_on_its_pale_accent() {
        let t = app_theme(ThemeChoice::Everforest, true);
        assert_eq!(on_accent_color(&t), t.palette().background);
        assert_eq!(on_accent_color(&app_theme(ThemeChoice::Dark, true)), Color::WHITE);
    }

    #[test]
    fn plugin_actions_send_only_to_listed_chats() {
        use crate::plugins::{Action, Update};
        let (mut app, mut rx) = app();
        let send = |chat: String| Message::Plugin("bot".into(), Update::Action(Action::Send { chat, body: "pong".into() }));
        app.update(send(crate::state::thread_key(&alice())));
        assert!(drain(&mut rx).is_empty(), "disabled plugins are ignored");
        app.update(Message::TogglePlugin("bot".into(), true));
        drain(&mut rx);
        app.update(send(crate::state::thread_key(&alice())));
        let cmds = drain(&mut rx);
        assert!(matches!(cmds.as_slice(), [Command::Send { thread, body, .. }] if *thread == alice() && body == "pong"), "{cmds:?}");
        app.update(send(crate::state::thread_key(&presage::store::Thread::Group([9; 32]))));
        assert!(drain(&mut rx).is_empty(), "not to chats outside the list");
    }

    #[test]
    fn plugins_get_incoming_and_sent_messages() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        app.update(Message::TogglePlugin("bot".into(), true));
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        app.update(Message::Plugin("bot".into(), crate::plugins::Update::Ready(tx)));
        app.update(Message::Backend(Event::Incoming { thread: alice(), content: Box::new(content(uuid(2), 30, crate::timeline::tests::text(30, "!ping"))) }));
        let e: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!((e["type"].as_str(), e["body"].as_str(), e["from_name"].as_str(), e["outgoing"].as_bool()), (Some("message"), Some("!ping"), Some("Alice"), Some(false)));
        assert_eq!(e["chat"].as_str(), Some(crate::state::thread_key(&alice()).as_str()));
        app.open.as_mut().unwrap().composer = iced::widget::text_editor::Content::with_text("hello");
        app.update(Message::Send);
        let e: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!((e["type"].as_str(), e["body"].as_str(), e["chat_name"].as_str()), (Some("sent"), Some("hello"), Some("Alice")));
    }

    #[test]
    fn picked_photo_is_uploaded_once() {
        let (mut app, mut rx) = settings_app();
        app.update(Message::PhotoReady(Ok(vec![0xff, 0xd8])));
        app.update(Message::PhotoReady(Ok(vec![0xff, 0xd8])));
        let cmds = drain(&mut rx);
        assert!(matches!(cmds.as_slice(), [Command::SetAvatar(Some(_))]), "one upload at a time: {cmds:?}");
        assert!(simulator(view(&app)).find("Uploading…").is_ok());
        app.update(Message::Backend(Event::Profile(app.profile.clone().unwrap())));
        assert!(simulator(view(&app)).find("Change photo").is_ok(), "done");
    }

    #[test]
    fn group_timer_is_shown_read_only() {
        let (mut app, _rx) = app();
        app.update(Message::OpenThread(presage::store::Thread::Group([5; 32]), "Team".into(), true));
        let mut ui = simulator(view(&app));
        ui.click("Off").expect("timer shown");
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
        assert!(!has_id(&app, "Chat actions"), "only on hover");
        app.row_hovered = Some(alice());
        click_id(&mut app, "Chat actions");
        click(&mut app, "Pin");
        app.row_hovered = Some(alice());
        click_id(&mut app, "Chat actions");
        click(&mut app, "Mute");
        {
            let mut ui = simulator(view(&app));
            assert!(ui.find(iced::widget::Id::new("pinned")).is_ok() && ui.find(iced::widget::Id::new("muted")).is_ok(), "pinned and muted marks");
        }

        app.conversations[0].unread = 0;
        click_id(&mut app, "Chat actions");
        click(&mut app, "Mark unread");
        assert!(app.conversations[0].marked_unread);
        click_id(&mut app, "Chat actions");
        assert!(simulator(view(&app)).find("Unpin").is_ok() && simulator(view(&app)).find("Mark read").is_ok());
        click(&mut app, "Archive");
        let mut ui = simulator(view(&app));
        assert!(ui.find("Alice").is_err(), "gone from the inbox");
        ui.click("Archived chats (1)").expect("archived link");
        for m in ui.into_messages().collect::<Vec<_>>() {
            let _ = app.update(m);
        }
        assert!(simulator(view(&app)).find("Alice").is_ok(), "listed under Archived");
        click(&mut app, "Archived chats");
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
        assert!(!has_id(&app, "Chat actions"), "no menu on requests");
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
        click_id(&mut app, "New group");
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
        click_id(&mut app, "Settings");
        assert!(matches!(crate::app::tests::drain(&mut rx).as_slice(), [Command::LoadSettings]));
        let profile = crate::backend::OwnProfile { given: "Fake".into(), family: "Person".into(), about: "Testing".into(), emoji: "🧪".into() };
        app.update(Message::Backend(Event::Profile(profile)));
        app.update(Message::Backend(Event::Account { phone: "+15550100".into() }));
        app.update(Message::Backend(Event::Devices(vec![
            crate::backend::Device { id: 1, name: "Fake Phone".into(), this: false },
            crate::backend::Device { id: 2, name: "signal-rusted on desk".into(), this: true },
        ])));
        (app, rx)
    }

    #[test]
    fn settings_screen_switches_theme_and_toggles_notifications() {
        let (mut app, mut rx) = settings_app();
        let mut ui = simulator(view(&app));
        assert!(ui.find("+15550100").is_ok(), "number shown");
        drop(ui);
        click(&mut app, "Linked devices");
        assert!(simulator(view(&app)).find("This device").is_ok(), "this device marked");
        app.system_dark = true;
        click(&mut app, "Appearance");
        click(&mut app, "Light");
        assert!(!app.dark, "overrides the dark system theme");
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(matches!(cmds.as_slice(), [Command::SaveSettings(s)] if s.theme == crate::state::ThemeChoice::Light), "{cmds:?}");
        click(&mut app, "Rusted");
        assert!(app.dark && app.theme().palette().primary == color!(0xe8641f), "a named theme applies its colors");
        click(&mut app, "Notifications");
        click(&mut app, "Show message text in notifications");
        assert!(!app.settings.notification_text);
        click(&mut app, "Show notifications");
        assert!(!app.settings.notifications);
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(matches!(cmds.last(), Some(Command::SaveSettings(s)) if !s.notifications && !s.notification_text), "{cmds:?}");
        click(&mut app, "Appearance");
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
    fn profile_shares_a_signal_me_qr_code() {
        let (mut app, _rx) = settings_app();
        click(&mut app, "Profile");
        let mut ui = simulator(view(&app));
        ui.click("Copy link").expect("copy button");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(matches!(msgs.as_slice(), [Message::Copy(l)] if l == "https://signal.me/#p/+15550100"), "{msgs:?}");
    }

    #[test]
    fn appearance_rounding_and_gif_provider_are_saved() {
        let (mut app, mut rx) = settings_app();
        click(&mut app, "Appearance");
        click(&mut app, "Square");
        assert!(app.settings.square);
        click(&mut app, "Chats");
        click(&mut app, "Tenor");
        let cmds = crate::app::tests::drain(&mut rx);
        assert!(matches!(cmds.last(), Some(Command::SaveSettings(s)) if s.square && s.gif_provider == GifProvider::Tenor), "{cmds:?}");
        app.update(Message::SetGifKey("abc".into()));
        assert_eq!((app.settings.tenor_key.as_str(), app.settings.giphy_key.as_str()), ("abc", ""), "the key goes to the selected provider");
        assert_eq!((square_or(18.0, true), square_or(2.0, true), square_or(18.0, false)), (3.0, 2.0, 18.0));
    }

    #[test]
    fn fun_panel_tabs_and_sending_a_gif() {
        let (mut app, mut rx) = app();
        open_alice(&mut app, &mut rx);
        click_id(&mut app, "Emoji");
        assert!(app.open.as_ref().unwrap().picker.is_some(), "emoji tab first");
        click(&mut app, "GIFs");
        let o = app.open.as_ref().unwrap();
        assert!(o.gifs.is_some() && o.picker.is_none() && o.stickers.is_none());
        app.update(Message::GifResults(String::new(), Err("Add a Giphy API key in Settings → Chats".into())));
        assert!(simulator(view(&app)).find("Add a Giphy API key in Settings → Chats").is_ok());
        let gif = crate::gifs::Gif { title: "cat".into(), preview: "https://p".into(), url: "https://media0.giphy.com/cat.gif".into() };
        let still = image::Handle::from_rgba(1, 1, vec![0; 4]);
        app.update(Message::GifResults(String::new(), Ok(vec![(gif, still)])));
        let mut ui = simulator(view(&app));
        ui.click(iced::widget::Id::new("gif-0")).expect("gif cell");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(matches!(msgs.as_slice(), [Message::SendGif(u)] if u == "https://media0.giphy.com/cat.gif"), "{msgs:?}");
        drain(&mut rx);
        app.update(Message::GifDownloaded(Ok("/tmp/gif-1.gif".into())));
        let cmds = drain(&mut rx);
        assert!(cmds.iter().any(|c| matches!(c, Command::Send { attachments, .. } if attachments.len() == 1)), "{cmds:?}");
        app.update(Message::OpenFun(Fun::Stickers));
        assert!(app.open.as_ref().unwrap().stickers.is_some());
        click_id(&mut app, "Emoji");
        let o = app.open.as_ref().unwrap();
        assert!(o.picker.is_none() && o.stickers.is_none() && o.gifs.is_none(), "the emoji button closes any open tab");
    }

    #[test]
    fn unlinking_needs_confirmation() {
        let (mut app, mut rx) = settings_app();
        click(&mut app, "Linked devices");
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
        for (name, section, theme) in [
            ("settings-profile", SettingsSection::Profile, ThemeChoice::Light),
            ("settings-devices", SettingsSection::Devices, ThemeChoice::Light),
            ("settings-appearance", SettingsSection::Appearance, ThemeChoice::Light),
            ("settings-appearance-rusted", SettingsSection::Appearance, ThemeChoice::Rusted),
            ("settings-chats", SettingsSection::Chats, ThemeChoice::Dark),
            ("settings-plugins", SettingsSection::Plugins, ThemeChoice::Everforest),
            ("settings-about", SettingsSection::About, ThemeChoice::Dark),
        ] {
            app.update(Message::SettingsSection(section));
            app.update(Message::AskUnlink(true));
            app.update(Message::SetTheme(theme));
            let mut ui = iced_test::Simulator::with_size(iced::Settings::default(), (1100.0, 760.0), view(&app));
            let snap = ui.snapshot(&app.theme()).expect("render");
            assert!(snap.matches_image(dir.join(name)).expect("write png"));
        }
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
        ui.click(iced::widget::Id::new("Stories")).expect("Stories tab");
        let msgs: Vec<Message> = ui.into_messages().collect();
        run(&mut app, msgs);
        let mut ui = simulator(view(&app));
        assert!(ui.find("Bob").is_ok() && ui.find("Stories").is_ok());
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
        assert_eq!(has_id(&app, "Call"), crate::calls::ENABLED);
        if crate::calls::ENABLED {
            click_id(&mut app, "Call");
            assert_eq!(sent_calls(&mut rx), [CallCommand::Start(uuid(2))]);
            // Busy with a call: the button does nothing.
            app.update(Message::Backend(Event::Call(call(Phase::Ringing, false))));
            click_id(&mut app, "Call");
            assert!(sent_calls(&mut rx).is_empty());
        }
        let (group, _rx) = crate::app::tests::open_group();
        assert!(!has_id(&group, "Call"));
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
