//! Rendering. Signal-Desktop's layout: conversation list left, timeline + composer right.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{Local, TimeZone};
use iced::font::{Family, Style as FontStyle, Weight};
use iced::keyboard::{self, key};
use iced::widget::text::Span;
use iced::widget::text_editor::{Binding, KeyPress};
use iced::widget::{
    Column, Space, button, center, column, container, image, mouse_area, progress_bar, qr_code, rich_text, row, rule,
    scrollable, span, text, text_editor, text_input,
};
use iced::{Alignment, Background, Border, Color, Element, Font, Length, Theme, color};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::data_message::Sticker;
use presage::proto::{AttachmentPointer, BodyRange};

use crate::app::{App, Attach, Message, Open, Screen, StickerTab, TIMERS, display_name};
use crate::backend::{attachment_key, now_ms};
use crate::timeline::{Delivery, Kind, Message as Msg, attachment_label, short_duration};
use crate::{emoji, richtext};

pub const COMPOSER: &str = "composer";
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

fn avatar<'a>(title: &str, seed: &str, size: f32) -> Element<'a, Message> {
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
            match &app.open {
                Some(o) => conversation(app, o),
                None => center(text("Select a conversation to start messaging").style(|t: &Theme| text::Style {
                    color: Some(muted(t))
                }))
                .into(),
            }
        ]
        .into(),
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

fn sidebar(app: &App) -> Element<'_, Message> {
    let q = app.search.trim().to_lowercase();
    let mut list = Column::new();
    for c in app.conversations.iter().filter(|c| q.is_empty() || c.title.to_lowercase().contains(&q)) {
        let selected = app.open.as_ref().is_some_and(|o| o.thread == c.thread);
        let seed = crate::state::thread_key(&c.thread);
        let mut top = row![text(c.title.clone()).font(Font { weight: Weight::Bold, ..Font::DEFAULT }).width(Length::Fill)];
        if c.last_ts > 0 {
            top = top.push(text(short_time(c.last_ts)).size(12).style(|t: &Theme| text::Style { color: Some(muted(t)) }));
        }
        let typing = app.typing_names(&c.thread);
        let preview = if !typing.is_empty() { "typing…".to_string() } else { truncate(&c.preview, 48) };
        let mut bottom = row![text(preview).size(13).wrapping(text::Wrapping::None).width(Length::Fill).style(|t: &Theme| text::Style { color: Some(muted(t)) })];
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
        }
        let item = row![avatar(&c.title, &seed, 44.0), column![top, bottom].spacing(2)]
            .spacing(12)
            .align_y(Alignment::Center);
        list = list.push(
            button(item)
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
                }),
        );
    }
    let contacts = app.contact_matches();
    if !contacts.is_empty() {
        list = list.push(container(text("Contacts").size(13)).padding([8, 12]));
        for (id, name) in contacts {
            let thread = presage::store::Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(id.into()));
            list = list.push(
                button(row![avatar(&name, &id.to_string(), 36.0), text(name.clone())].spacing(12).align_y(Alignment::Center))
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
    column![
        container(text("Signal").size(22).font(Font { weight: Weight::Bold, ..Font::DEFAULT })).padding([12, 16]),
        container(text_input("Search", &app.search).on_input(Message::Search).padding(8)).padding([0, 12]),
        scrollable(list).height(Length::Fill),
    ]
    .spacing(8)
    .width(320)
    .into()
}

fn conversation<'a>(app: &'a App, o: &'a Open) -> Element<'a, Message> {
    let typing = app.typing_names(&o.thread);
    let subtitle: Element<_> = if typing.is_empty() {
        Space::new().into()
    } else {
        text(format!("{} typing…", typing.join(", "))).size(12).style(|t: &Theme| text::Style { color: Some(muted(t)) }).into()
    };
    let header = container(
        row![
            avatar(&o.title, &crate::state::thread_key(&o.thread), 32.0),
            column![text(o.title.clone()).font(Font { weight: Weight::Bold, ..Font::DEFAULT }), subtitle].width(Length::Fill),
            timer_control(app, o),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
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

    column![header, rule::horizontal(1), body, composer(app, o)].into()
}

/// "⏱ 5m": opens the timer choices in 1:1 chats; read-only in groups (changing it there is a
/// group change, which needs the group admin APIs).
fn timer_control<'a>(app: &App, o: &Open) -> Element<'a, Message> {
    let secs = app.timer(&o.thread);
    let label = TIMERS.iter().find(|(s, _)| *s == secs).map_or_else(|| short_duration(secs.into()), |(_, l)| l.to_string());
    let label = text(format!("⏱ {label}")).size(13);
    if o.is_group {
        label.style(|t: &Theme| text::Style { color: Some(muted(t)) }).into()
    } else {
        button(label).on_press(Message::ToggleTimerMenu).style(button::text).into()
    }
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
    let spans: Vec<Span<'a, Message>> = richtext::segments(body, ranges)
        .into_iter()
        .enumerate()
        .map(|(i, seg)| {
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
            let mut sp = span(content).font(font).strikethrough(s.strike);
            if s.spoiler && !revealed.contains(&(msg_id, i)) {
                // Hidden: text drawn in its own background color until clicked.
                let hide = Color { a: 1.0, ..fg };
                sp = sp.color(hide).background(hide).link(Message::Spoiler(msg_id, i));
            }
            sp
        })
        .collect();
    rich_text(spans).on_link_click(|m| m).into()
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
        Some(Attach::Ready(path)) if is_image => image(image::Handle::from_path(path)).width(Length::Shrink).height(Length::Shrink).into(),
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

fn message<'a>(app: &'a App, o: &'a Open, m: &'a Msg, show_author: bool) -> Element<'a, Message> {
    if let Kind::System(s) = &m.kind {
        return center(text(s.clone()).size(13).style(|t: &Theme| text::Style { color: Some(muted(t)) }))
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
            for a in &m.attachments {
                bubble = bubble.push(attachment(app, a, fg));
            }
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
                button(row![avatar(&name, &id.to_string(), 24.0), text(name.clone())].spacing(8).align_y(Alignment::Center))
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
    use crate::app::tests::{alice, app, fake_audio, fake_packs, open_alice, voice_note};
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
    fn link_screen_renders_qr() {
        let (mut app, _rx) = app();
        app.update(Message::Backend(Event::NeedsLink));
        assert!(simulator(view(&app)).find("Generate QR code").is_ok());
        app.update(Message::Link);
        app.update(Message::Backend(Event::LinkUrl("sgnl://linkdevice?uuid=a&pub_key=b".into())));
        let mut ui = simulator(view(&app));
        assert!(ui.find("Waiting for your phone…").is_ok());
    }
}
