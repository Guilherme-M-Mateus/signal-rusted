//! Line icons (shapes after Lucide, ISC license), drawn as SVG and tinted to the theme.

use iced::widget::{Svg, svg};
use iced::{Color, Theme};

macro_rules! icon {
    ($($body:literal),*) => {
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#,
            $($body,)*
            "</svg>"
        )
    };
}

pub const CHATS: &str = icon!(r#"<path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z"/>"#);
pub const STORIES: &str = icon!(r#"<circle cx="12" cy="12" r="9.5" stroke-dasharray="2.5 2.5"/><circle cx="12" cy="12" r="5"/>"#);
pub const SETTINGS: &str = icon!(
    r#"<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/>"#,
    r#"<circle cx="12" cy="12" r="3"/>"#
);
pub const COMPOSE: &str = icon!(
    r#"<path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/>"#,
    r#"<path d="M18.375 2.625a2.121 2.121 0 1 1 3 3L12 15l-4 1 1-4Z"/>"#
);
pub const SEARCH: &str = icon!(r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>"#);
pub const PHONE: &str = icon!(
    r#"<path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7A2 2 0 0 1 22 16.92z"/>"#
);
pub const TIMER: &str = icon!(r#"<path d="M10 2h4"/><path d="m12 14 3-3"/><circle cx="12" cy="14" r="8"/>"#);
pub const INFO: &str = icon!(r#"<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>"#);
pub const MORE: &str = icon!(r#"<circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/>"#);
pub const PIN: &str = icon!(
    r#"<path d="M12 17v5"/>"#,
    r#"<path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1z"/>"#
);
pub const MUTED: &str = icon!(
    r#"<path d="M8.7 3A6 6 0 0 1 18 8a21.3 21.3 0 0 0 .6 5"/><path d="M17 17H3s3-2 3-9a4.67 4.67 0 0 1 .3-1.7"/>"#,
    r#"<path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/><path d="m2 2 20 20"/>"#
);
pub const SMILE: &str = icon!(r#"<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2"/><path d="M9 9h.01"/><path d="M15 9h.01"/>"#);
pub const REPLY: &str = icon!(r#"<path d="M9 17 4 12l5-5"/><path d="M20 18v-2a4 4 0 0 0-4-4H4"/>"#);
pub const EDIT: &str = icon!(
    r#"<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/>"#
);
pub const TRASH: &str = icon!(r#"<path d="M3 6h18"/><path d="M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6"/><path d="M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2"/>"#);
pub const ATTACH: &str = icon!(
    r#"<path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48"/>"#
);
pub const STICKER: &str = icon!(r#"<path d="M15.5 3H5a2 2 0 0 0-2 2v14c0 1.1.9 2 2 2h14a2 2 0 0 0 2-2V8.5L15.5 3Z"/><path d="M15 3v6h6"/>"#);
pub const MIC: &str = icon!(r#"<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2"/><path d="M12 19v3"/>"#);
pub const SEND: &str = icon!(r#"<path d="m22 2-7 20-4-9-9-4Z"/><path d="M22 2 11 13"/>"#);
pub const CLOSE: &str = icon!(r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#);
pub const BACK: &str = icon!(r#"<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>"#);
pub const USER: &str = icon!(r#"<path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>"#);
pub const PALETTE: &str = icon!(
    r#"<path d="M12 2C6.5 2 2 6.5 2 12s4.5 10 10 10c.93 0 1.65-.75 1.65-1.69 0-.44-.18-.84-.44-1.13-.29-.29-.44-.65-.44-1.13a1.64 1.64 0 0 1 1.67-1.67h2c3.05 0 5.55-2.5 5.55-5.55C21.96 6.01 17.46 2 12 2z"/>"#,
    r#"<circle cx="13.5" cy="6.5" r="1"/><circle cx="17.5" cy="10.5" r="1"/><circle cx="8.5" cy="7.5" r="1"/><circle cx="6.5" cy="12.5" r="1"/>"#
);
pub const BELL: &str = icon!(r#"<path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/>"#);
pub const DEVICES: &str = icon!(r#"<rect width="20" height="14" x="2" y="3" rx="2"/><path d="M8 21h8"/><path d="M12 17v4"/>"#);
pub const ARCHIVE: &str = icon!(r#"<rect width="20" height="5" x="2" y="3" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/><path d="M10 12h4"/>"#);
pub const CHECK: &str = icon!(r#"<path d="M20 6 9 17l-5-5"/>"#);
pub const ABOUT: &str = INFO;
pub const PLAY: &str = icon!(r#"<path d="M7 4.5v15a1 1 0 0 0 1.5.86l12.5-7.5a1 1 0 0 0 0-1.72L8.5 3.64A1 1 0 0 0 7 4.5Z" fill="black"/>"#);
pub const PAUSE: &str = icon!(r#"<rect x="6" y="4" width="4" height="16" rx="1" fill="black"/><rect x="14" y="4" width="4" height="16" rx="1" fill="black"/>"#);

pub const DOWNLOAD: &str = icon!(r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>"#);
pub const SAVE: &str = icon!(r#"<path d="M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"/><path d="M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7"/><path d="M7 3v4a1 1 0 0 0 1 1h7"/>"#);
pub const RETRY: &str = icon!(r#"<path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>"#);
/// A solid page with a dog-eared corner (the fold drawn translucent, so tinting keeps it).
pub const DOCUMENT: &str = icon!(
    r#"<path d="M5.5 1h9v4.5A1.5 1.5 0 0 0 16 7h4.5v14.5A1.5 1.5 0 0 1 19 23H5.5A1.5 1.5 0 0 1 4 21.5v-19A1.5 1.5 0 0 1 5.5 1Z" fill="black" stroke="none"/>"#,
    r#"<path d="M15.5 1 20.5 6h-4a1 1 0 0 1-1-1Z" fill="black" fill-opacity="0.55" stroke="none"/>"#
);

// Receipts. The left ring of a pair is cut back (mask) so the right one reads as in front.
pub const SENDING: &str = icon!(r#"<circle cx="12" cy="12" r="8" stroke-dasharray="2.5 2.5"/>"#);
pub const SENT: &str = icon!(r#"<circle cx="12" cy="12" r="8"/><path d="m8.5 12 2.5 2.5 4.5-5"/>"#);
pub const DELIVERED: &str = icon!(
    r#"<defs><mask id="l"><rect width="24" height="24" fill="white" stroke="none"/><circle cx="15.5" cy="12" r="9" fill="black" stroke="none"/></mask></defs>"#,
    r#"<circle cx="8.5" cy="12" r="6.5" mask="url(#l)"/><circle cx="15.5" cy="12" r="6.5"/><path d="m12.7 12 2 2 3.5-4"/>"#
);
pub const READ: &str = icon!(
    r#"<defs><mask id="l"><rect width="24" height="24" fill="white" stroke="none"/><circle cx="15.5" cy="12" r="9" fill="black" stroke="none"/></mask>"#,
    r#"<mask id="c"><rect width="24" height="24" fill="white" stroke="none"/><path d="m12.7 12 2 2 3.5-4" stroke="black"/></mask></defs>"#,
    r#"<circle cx="8.5" cy="12" r="7.5" fill="black" stroke="none" mask="url(#l)"/><circle cx="15.5" cy="12" r="7.5" fill="black" stroke="none" mask="url(#c)"/>"#
);

/// `src` at `size` px in the theme's text color.
pub fn icon<'a>(src: &'static str, size: impl Into<iced::Length> + Copy) -> Svg<'a> {
    svg(svg::Handle::from_memory(src.as_bytes())).width(size).height(size).style(|t: &Theme, _| svg::Style { color: Some(t.palette().text) })
}

/// `src` at `size` px in a fixed color.
pub fn tinted<'a>(src: &'static str, size: impl Into<iced::Length> + Copy, color: Color) -> Svg<'a> {
    icon(src, size).style(move |_, _| svg::Style { color: Some(color) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_icon_is_well_formed_svg() {
        use super::*;
        for src in [
            CHATS, STORIES, SETTINGS, COMPOSE, SEARCH, PHONE, TIMER, INFO, MORE, PIN, MUTED, SMILE, REPLY, EDIT, TRASH, ATTACH, STICKER, MIC,
            SEND, CLOSE, BACK, USER, PALETTE, BELL, DEVICES, ARCHIVE, CHECK, PLAY, PAUSE, DOWNLOAD, SAVE, RETRY, DOCUMENT, SENDING, SENT, DELIVERED, READ,
        ] {
            assert!(src.starts_with("<svg") && src.ends_with("</svg>"));
            assert_eq!(src.matches('<').count(), src.matches('>').count());
        }
    }
}
