//! Signal rich text: `BodyRange`s (UTF-16 offsets) <-> styled segments, and the
//! composer markup (`*bold*`, `_italic_`, `~strike~`, `` `mono` ``, `||spoiler||`).

use presage::proto::BodyRange;
use presage::proto::body_range::{AssociatedValue, Style as ProtoStyle};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Hash)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub mono: bool,
    pub spoiler: bool,
}

impl Style {
    fn add(&mut self, s: ProtoStyle) {
        match s {
            ProtoStyle::Bold => self.bold = true,
            ProtoStyle::Italic => self.italic = true,
            ProtoStyle::Strikethrough => self.strike = true,
            ProtoStyle::Monospace => self.mono = true,
            ProtoStyle::Spoiler => self.spoiler = true,
            ProtoStyle::None => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
    /// ACI of the mentioned user; `text` is the raw placeholder (usually U+FFFC).
    pub mention: Option<String>,
}

enum Kind {
    Style(ProtoStyle),
    Mention(String),
}

/// Byte offset in `body` for a UTF-16 offset, clamped and snapped down to a char boundary.
fn byte_at_utf16(body: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in body.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    body.len()
}

pub fn segments(body: &str, ranges: &[BodyRange]) -> Vec<Segment> {
    let spans: Vec<(usize, usize, Kind)> = ranges
        .iter()
        .filter_map(|r| {
            let start = r.start? as usize;
            let end = start + r.length? as usize;
            let kind = match r.associated_value.as_ref()? {
                AssociatedValue::Style(s) => Kind::Style(ProtoStyle::try_from(*s).ok()?),
                AssociatedValue::MentionAci(aci) => Kind::Mention(aci.clone()),
                AssociatedValue::MentionAciBinary(b) => {
                    Kind::Mention(uuid_string(b.as_slice().try_into().ok()?))
                }
            };
            let (s, e) = (byte_at_utf16(body, start), byte_at_utf16(body, end));
            (s < e).then_some((s, e, kind))
        })
        .collect();

    let mut cuts: Vec<usize> = vec![0, body.len()];
    for (s, e, _) in &spans {
        cuts.push(*s);
        cuts.push(*e);
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut out: Vec<Segment> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut style = Style::default();
        let mut mention = None;
        for (s, e, kind) in &spans {
            if *s <= a && b <= *e {
                match kind {
                    Kind::Style(st) => style.add(*st),
                    Kind::Mention(aci) => mention = Some((aci.clone(), *s)),
                }
            }
        }
        let text = &body[a..b];
        // Merge with the previous segment when style matches and it's the same mention range.
        if let Some(last) = out.last_mut() {
            let same_mention = match (&last.mention, &mention) {
                (None, None) => true,
                (Some(x), Some((y, _))) => x == y && mention_continues(&spans, a),
                _ => false,
            };
            if last.style == style && same_mention {
                last.text.push_str(text);
                continue;
            }
        }
        out.push(Segment {
            text: text.to_owned(),
            style,
            mention: mention.map(|(aci, _)| aci),
        });
    }
    out.retain(|s| !s.text.is_empty());
    out
}

/// True when position `at` is strictly inside some mention range (not its start).
fn mention_continues(spans: &[(usize, usize, Kind)], at: usize) -> bool {
    spans
        .iter()
        .any(|(s, e, k)| matches!(k, Kind::Mention(_)) && *s < at && at < *e)
}

fn uuid_string(b: [u8; 16]) -> String {
    presage::libsignal_service::prelude::Uuid::from_bytes(b).to_string()
}

pub fn style_range(start: usize, length: usize, style: ProtoStyle) -> BodyRange {
    BodyRange {
        start: Some(start as u32),
        length: Some(length as u32),
        associated_value: Some(AssociatedValue::Style(style as i32)),
    }
}

const DELIMS: [(&str, ProtoStyle); 5] = [
    ("||", ProtoStyle::Spoiler),
    ("*", ProtoStyle::Bold),
    ("_", ProtoStyle::Italic),
    ("~", ProtoStyle::Strikethrough),
    ("`", ProtoStyle::Monospace),
];

fn is_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace() || (c.is_ascii_punctuation() && c != '\\'))
}

/// Parses composer markup into a plain body plus style ranges.
/// Delimiters only count at word boundaries (so `snake_case_name` stays literal),
/// `\` escapes a delimiter, and nothing is styled inside `` `mono` ``.
pub fn parse_markup(input: &str) -> (String, Vec<BodyRange>) {
    let mut body = String::new();
    let mut ranges = Vec::new();
    parse_into(input, &mut body, &mut ranges, false);
    (body, ranges)
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn parse_into(input: &str, body: &mut String, ranges: &mut Vec<BodyRange>, literal: bool) {
    let urls = links(input);
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        // URLs are copied verbatim: `_` and `~` in them are common and never markup.
        if let Some(u) = urls.iter().find(|u| u.start == i) {
            body.push_str(&input[u.clone()]);
            i = u.end;
            continue;
        }
        if !literal {
            if let Some(after) = rest.strip_prefix('\\') {
                if let Some((d, _)) = DELIMS.iter().find(|(d, _)| after.starts_with(d)) {
                    body.push_str(d);
                    i += 1 + d.len();
                    continue;
                }
            }
            let prev = input[..i].chars().next_back();
            if let Some((delim, style)) = DELIMS.iter().find(|(d, _)| rest.starts_with(d)) {
                if let Some(close) = find_close(input, i, delim, prev) {
                    let inner = &input[i + delim.len()..close];
                    let start = utf16_len(body);
                    parse_into(inner, body, ranges, *style == ProtoStyle::Monospace);
                    let len = utf16_len(body) - start;
                    ranges.push(style_range(start, len, *style));
                    i = close + delim.len();
                    continue;
                }
            }
        }
        let c = rest.chars().next().expect("non-empty");
        body.push(c);
        i += c.len_utf8();
    }
}

/// Turns every `@Name` of a picked mention `(name, aci)` in `body` into U+FFFC plus a
/// `MentionAci` range, shifting the style `ranges` (UTF-16) onto the shortened body.
/// A name followed by a letter/digit (`@Bobby` for "Bob") doesn't count.
pub fn apply_mentions(body: &str, ranges: Vec<BodyRange>, mentions: &[(String, String)]) -> (String, Vec<BodyRange>) {
    let mut out = String::new();
    let mut out16 = 0;
    let mut in16 = 0;
    // (start, end) in the original body and start in `out`, all UTF-16.
    let mut hits: Vec<(usize, usize, usize)> = Vec::new();
    let mut mention_ranges = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let rest = &body[i..];
        let hit = rest.strip_prefix('@').and_then(|r| {
            mentions
                .iter()
                .filter(|(name, _)| {
                    !name.is_empty()
                        && r.starts_with(name.as_str())
                        && !r[name.len()..].chars().next().is_some_and(char::is_alphanumeric)
                })
                .max_by_key(|(name, _)| name.len())
        });
        if let Some((name, aci)) = hit {
            let len16 = 1 + utf16_len(name);
            hits.push((in16, in16 + len16, out16));
            mention_ranges.push(BodyRange {
                start: Some(out16 as u32),
                length: Some(1),
                associated_value: Some(AssociatedValue::MentionAci(aci.clone())),
            });
            out.push('\u{FFFC}');
            i += 1 + name.len();
            in16 += len16;
            out16 += 1;
            continue;
        }
        let c = rest.chars().next().expect("non-empty");
        out.push(c);
        i += c.len_utf8();
        in16 += c.len_utf16();
        out16 += c.len_utf16();
    }
    // An offset inside a replaced name snaps to the placeholder's start (or end, for range ends).
    let map = |x: usize, end: bool| {
        let mut shift = 0;
        for &(s, e, at) in &hits {
            if x >= e {
                shift += e - s - 1;
            } else if x > s {
                return at + usize::from(end);
            }
        }
        x - shift
    };
    let mut ranges: Vec<BodyRange> = ranges
        .into_iter()
        .filter_map(|mut r| {
            let (start, len) = (r.start? as usize, r.length? as usize);
            let (s, e) = (map(start, false), map(start + len, true));
            r.start = Some(s as u32);
            r.length = Some(e.checked_sub(s).filter(|l| *l > 0)? as u32);
            Some(r)
        })
        .collect();
    ranges.extend(mention_ranges);
    (out, ranges)
}

/// Inverse of [`parse_markup`], for putting a sent message back in the composer.
/// Round-trips anything `parse_markup` produces; ranges that start or end mid-word
/// (possible from other clients) have no markup form and come back best-effort.
/// shortcut: mentions are dropped (their U+FFFC stays); keep them if edits need them.
pub fn to_markup(body: &str, ranges: &[BodyRange]) -> String {
    // Opening order; mono must be innermost since nothing is parsed inside it.
    const ORDER: [ProtoStyle; 5] =
        [ProtoStyle::Spoiler, ProtoStyle::Bold, ProtoStyle::Italic, ProtoStyle::Strikethrough, ProtoStyle::Monospace];
    let delim = |s: ProtoStyle| DELIMS.iter().find(|(_, d)| *d == s).map_or("", |(d, _)| d);
    let has = |st: &Style, s: ProtoStyle| match s {
        ProtoStyle::Spoiler => st.spoiler,
        ProtoStyle::Bold => st.bold,
        ProtoStyle::Italic => st.italic,
        ProtoStyle::Strikethrough => st.strike,
        ProtoStyle::Monospace => st.mono,
        ProtoStyle::None => false,
    };
    let mut out = String::new();
    let mut open: Vec<ProtoStyle> = Vec::new();
    let close_to = |out: &mut String, open: &mut Vec<ProtoStyle>, want: &[ProtoStyle]| loop {
        let missing = want.iter().any(|s| !open.contains(s));
        let bad = open.iter().any(|s| !want.contains(s)) || (missing && open.contains(&ProtoStyle::Monospace));
        match open.pop() {
            Some(s) if bad => out.push_str(delim(s)),
            Some(s) => break open.push(s),
            None => break,
        }
    };
    for seg in segments(body, ranges) {
        let want: Vec<ProtoStyle> = ORDER.into_iter().filter(|s| has(&seg.style, *s)).collect();
        close_to(&mut out, &mut open, &want);
        for s in want.iter().filter(|s| !open.contains(s)).copied().collect::<Vec<_>>() {
            out.push_str(delim(s));
            open.push(s);
        }
        if open.contains(&ProtoStyle::Monospace) {
            out.push_str(&seg.text);
            continue;
        }
        let mut rest = seg.text.as_str();
        while let Some(c) = rest.chars().next() {
            match DELIMS.iter().find(|(d, _)| rest.starts_with(d)) {
                Some((d, _)) => {
                    out.push('\\');
                    out.push_str(d);
                    rest = &rest[d.len()..];
                }
                None => {
                    out.push(c);
                    rest = &rest[c.len_utf8()..];
                }
            }
        }
    }
    close_to(&mut out, &mut open, &[]);
    out
}

fn find_close(input: &str, open: usize, delim: &str, prev: Option<char>) -> Option<usize> {
    if !is_boundary(prev) {
        return None;
    }
    let content_start = open + delim.len();
    let first = input[content_start..].chars().next()?;
    if first.is_whitespace() || input[content_start..].starts_with(delim) {
        return None;
    }
    let mut j = content_start + first.len_utf8();
    while j < input.len() {
        if input[j..].starts_with(delim) {
            let before = input[..j].chars().next_back();
            let after = input[j + delim.len()..].chars().next();
            let escaped = before == Some('\\');
            if !escaped && before.is_some_and(|c| !c.is_whitespace()) && is_boundary(after) {
                return Some(j);
            }
        }
        j += input[j..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// Byte ranges of the http(s) URLs in `text`. A URL starts at a word boundary and runs over
/// letters, digits and URL punctuation, so emoji, spaces and mention placeholders end it.
/// Trailing sentence punctuation and an unbalanced closing `)` aren't part of it.
pub fn links(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("http").map(|i| i + from) {
        let rest = &text[i..];
        let scheme = if rest.starts_with("https://") { 8 } else if rest.starts_with("http://") { 7 } else { 0 };
        let word_start = text[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        if scheme == 0 || !word_start {
            from = i + 4;
            continue;
        }
        let mut end = i + rest.find(|c: char| !(c.is_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c))).unwrap_or(rest.len());
        loop {
            let url = &text[i..end];
            let unbalanced = url.ends_with(')') && url.matches(')').count() > url.matches('(').count();
            match url.chars().next_back() {
                Some(c @ ('.' | ',' | ';' | ':' | '!' | '?' | '\'' | '[' | '(')) => end -= c.len_utf8(),
                Some(')') if unbalanced => end -= 1,
                _ => break,
            }
        }
        if end > i + scheme {
            out.push(i..end);
        }
        from = end.max(i + scheme);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(f: impl Fn(&mut Style)) -> Style {
        let mut s = Style::default();
        f(&mut s);
        s
    }

    fn seg(text: &str, style: Style) -> Segment {
        Segment { text: text.into(), style, mention: None }
    }

    #[test]
    fn plain_body_is_one_segment() {
        assert_eq!(segments("hello", &[]), vec![seg("hello", Style::default())]);
        assert!(segments("", &[]).is_empty());
    }

    #[test]
    fn utf16_offsets_account_for_emoji() {
        // "😀" is 2 UTF-16 units; bold "hi" starts at unit 3.
        let r = style_range(3, 2, ProtoStyle::Bold);
        assert_eq!(
            segments("😀 hi!", &[r]),
            vec![
                seg("😀 ", Style::default()),
                seg("hi", st(|s| s.bold = true)),
                seg("!", Style::default()),
            ]
        );
    }

    #[test]
    fn overlapping_ranges_combine() {
        let ranges = [style_range(0, 4, ProtoStyle::Bold), style_range(2, 4, ProtoStyle::Italic)];
        assert_eq!(
            segments("abcdef", &ranges),
            vec![
                seg("ab", st(|s| s.bold = true)),
                seg("cd", st(|s| { s.bold = true; s.italic = true })),
                seg("ef", st(|s| s.italic = true)),
            ]
        );
    }

    #[test]
    fn out_of_bounds_and_bad_ranges_are_ignored_or_clamped() {
        let ranges = [
            style_range(2, 100, ProtoStyle::Monospace),
            style_range(50, 1, ProtoStyle::Bold),
            BodyRange { start: Some(0), length: Some(1), associated_value: Some(AssociatedValue::Style(99)) },
            BodyRange { start: None, length: Some(1), associated_value: None },
        ];
        assert_eq!(
            segments("abcd", &ranges),
            vec![seg("ab", Style::default()), seg("cd", st(|s| s.mono = true))]
        );
    }

    #[test]
    fn range_splitting_a_surrogate_pair_snaps_to_char_boundary() {
        // start=1 is the middle of "😀": must not panic on a non-char boundary.
        let out = segments("😀x", &[style_range(1, 2, ProtoStyle::Bold)]);
        assert_eq!(out.iter().map(|s| s.text.as_str()).collect::<String>(), "😀x");
    }

    #[test]
    fn mentions_carry_aci_and_stay_separate() {
        let aci = "8f2c6b9e-1a1b-4c3d-9e8f-123456789abc";
        let m = |start| BodyRange {
            start: Some(start),
            length: Some(1),
            associated_value: Some(AssociatedValue::MentionAci(aci.into())),
        };
        let out = segments("hi \u{FFFC}\u{FFFC}", &[m(3), m(4)]);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[1].mention.as_deref(), Some(aci));
        assert_eq!(out[2].mention.as_deref(), Some(aci));
    }

    #[test]
    fn binary_mention_aci_is_decoded() {
        let id = [7u8; 16];
        let r = BodyRange {
            start: Some(0),
            length: Some(1),
            associated_value: Some(AssociatedValue::MentionAciBinary(id.to_vec())),
        };
        assert_eq!(segments("\u{FFFC}", &[r])[0].mention, Some(uuid_string(id)));
    }

    fn styled(input: &str) -> Vec<Segment> {
        let (body, ranges) = parse_markup(input);
        segments(&body, &ranges)
    }

    #[test]
    fn markup_basic_styles() {
        let (body, ranges) = parse_markup("*b* _i_ ~s~ `m` ||sp||");
        assert_eq!(body, "b i s m sp");
        assert_eq!(ranges.len(), 5);
        let out = segments(&body, &ranges);
        assert_eq!(out[0], seg("b", st(|s| s.bold = true)));
        assert_eq!(out[2], seg("i", st(|s| s.italic = true)));
        assert_eq!(out[4], seg("s", st(|s| s.strike = true)));
        assert_eq!(out[6], seg("m", st(|s| s.mono = true)));
        assert_eq!(out[8], seg("sp", st(|s| s.spoiler = true)));
    }

    #[test]
    fn markup_nesting() {
        assert_eq!(
            styled("*bold _both_*"),
            vec![
                seg("bold ", st(|s| s.bold = true)),
                seg("both", st(|s| { s.bold = true; s.italic = true })),
            ]
        );
    }

    #[test]
    fn markup_ignores_intraword_unclosed_and_spaced_delimiters() {
        for literal in ["snake_case_name", "2*3*4", "a * b * c", "*open", "* no*", "**", "x_"] {
            let (body, ranges) = parse_markup(literal);
            assert_eq!(body, literal);
            assert!(ranges.is_empty(), "{literal}: {ranges:?}");
        }
    }

    #[test]
    fn markup_mono_is_literal_inside() {
        let (body, ranges) = parse_markup("`*not bold*`");
        assert_eq!(body, "*not bold*");
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn markup_escapes() {
        let (body, ranges) = parse_markup(r"\*literal\* and \||x||");
        assert_eq!(body, "*literal* and ||x||");
        assert!(ranges.is_empty());
    }

    #[test]
    fn markup_offsets_are_utf16() {
        let (body, ranges) = parse_markup("👍🏽 *ok*");
        assert_eq!(body, "👍🏽 ok");
        assert_eq!(ranges[0].start, Some(5)); // 👍🏽 is 4 units + space
        assert_eq!(ranges[0].length, Some(2));
        assert_eq!(segments(&body, &ranges)[1], seg("ok", st(|s| s.bold = true)));
    }

    #[test]
    fn markup_with_punctuation_boundaries() {
        assert_eq!(
            styled("(*yes*)!"),
            vec![
                seg("(", Style::default()),
                seg("yes", st(|s| s.bold = true)),
                seg(")!", Style::default()),
            ]
        );
    }

    const BOB: &str = "8f2c6b9e-1a1b-4c3d-9e8f-123456789abc";
    const CAROL: &str = "0e0e0e0e-1a1b-4c3d-9e8f-123456789abc";

    fn mention_markup(input: &str, picks: &[(&str, &str)]) -> (String, Vec<BodyRange>) {
        let (body, ranges) = parse_markup(input);
        let picks: Vec<(String, String)> = picks.iter().map(|(n, a)| (n.to_string(), a.to_string())).collect();
        apply_mentions(&body, ranges, &picks)
    }

    fn mention_at(ranges: &[BodyRange], aci: &str) -> Vec<(u32, u32)> {
        ranges
            .iter()
            .filter(|r| matches!(&r.associated_value, Some(AssociatedValue::MentionAci(a)) if a == aci))
            .map(|r| (r.start.unwrap(), r.length.unwrap()))
            .collect()
    }

    #[test]
    fn mention_becomes_placeholder_with_range() {
        let (body, ranges) = mention_markup("hi @Bob!", &[("Bob", BOB)]);
        assert_eq!(body, "hi \u{FFFC}!");
        assert_eq!(mention_at(&ranges, BOB), [(3, 1)]);
        let out = segments(&body, &ranges);
        assert_eq!(out[1].mention.as_deref(), Some(BOB));
        assert_eq!(out[2].text, "!");
    }

    #[test]
    fn mention_offsets_count_emoji_as_utf16() {
        // 👍🏽 is 4 units, 😀 2.
        let (body, ranges) = mention_markup("👍🏽 @Bob 😀 @Bob", &[("Bob", BOB)]);
        assert_eq!(body, "👍🏽 \u{FFFC} 😀 \u{FFFC}");
        assert_eq!(mention_at(&ranges, BOB), [(5, 1), (10, 1)]);
        // Emoji inside the name itself.
        let (body, ranges) = mention_markup("@Bob 🎉 ok", &[("Bob 🎉", BOB)]);
        assert_eq!(body, "\u{FFFC} ok");
        assert_eq!(mention_at(&ranges, BOB), [(0, 1)]);
    }

    #[test]
    fn multiple_people_and_style_after_mentions_shift() {
        let (body, ranges) = mention_markup("@Bob and @Carol Ann *done*", &[("Bob", BOB), ("Carol Ann", CAROL)]);
        assert_eq!(body, "\u{FFFC} and \u{FFFC} done");
        assert_eq!(mention_at(&ranges, BOB), [(0, 1)]);
        assert_eq!(mention_at(&ranges, CAROL), [(6, 1)]);
        let out = segments(&body, &ranges);
        assert_eq!(out.last().unwrap(), &seg("done", st(|s| s.bold = true)));
    }

    #[test]
    fn mention_inside_bold_keeps_the_bold_around_it() {
        let (body, ranges) = mention_markup("*hey @Bob you* 😀", &[("Bob", BOB)]);
        assert_eq!(body, "hey \u{FFFC} you 😀");
        let bold: Vec<_> = ranges.iter().filter(|r| r.associated_value == Some(AssociatedValue::Style(ProtoStyle::Bold as i32))).collect();
        assert_eq!((bold[0].start, bold[0].length), (Some(0), Some(9)));
        let out = segments(&body, &ranges);
        assert_eq!(out[1].mention.as_deref(), Some(BOB));
        assert!(out[1].style.bold);
        assert_eq!(out[2], seg(" you", st(|s| s.bold = true)));
    }

    #[test]
    fn range_ending_inside_a_name_snaps_to_the_placeholder() {
        // Bold "hi @B" (0..5) where "@Bob" (3..7) is replaced: bold covers the placeholder.
        let (body, ranges) = apply_mentions("hi @Bob x", vec![style_range(0, 5, ProtoStyle::Bold), style_range(5, 1, ProtoStyle::Italic)], &[("Bob".into(), BOB.into())]);
        assert_eq!(body, "hi \u{FFFC} x");
        assert_eq!((ranges[0].start, ranges[0].length), (Some(0), Some(4)));
        // Italic on part of the name styles the whole placeholder.
        assert_eq!((ranges[1].start, ranges[1].length), (Some(3), Some(1)));
        assert_eq!(ranges.len(), 3);
    }

    #[test]
    fn edited_away_or_longer_names_are_not_mentions() {
        for input in ["hi Bob", "hi @Bo", "hi @Bobby", "hi"] {
            let (body, ranges) = mention_markup(input, &[("Bob", BOB)]);
            assert_eq!(body, input);
            assert!(ranges.is_empty(), "{input}: {ranges:?}");
        }
        // The longest picked name wins.
        let (_, ranges) = mention_markup("@Bob Smith", &[("Bob", BOB), ("Bob Smith", CAROL)]);
        assert_eq!(mention_at(&ranges, CAROL), [(0, 1)]);
        assert!(mention_at(&ranges, BOB).is_empty());
    }

    #[test]
    fn to_markup_round_trips_composer_markup() {
        for input in [
            "plain",
            "*b* _i_ ~s~ `m` ||sp||",
            "*bold _both_*",
            "_ital *both*_ after",
            "||*secret bold*|| `*not bold*`",
            "*`code`* (*yes*)!",
            "👍🏽 *ok* _é_ ||🎉 party|| 😀 ~gone~",
        ] {
            let (body, ranges) = parse_markup(input);
            let markup = to_markup(&body, &ranges);
            let (body2, ranges2) = parse_markup(&markup);
            assert_eq!(body2, body, "{input} -> {markup}");
            assert_eq!(segments(&body2, &ranges2), segments(&body, &ranges), "{input} -> {markup}");
        }
        assert_eq!(to_markup("👍🏽 ok", &[style_range(5, 2, ProtoStyle::Bold)]), "👍🏽 *ok*", "UTF-16 offsets");
    }

    #[test]
    fn to_markup_escapes_literal_delimiters() {
        for body in ["a *b* c", "x ||y|| _z_ ~w~", r"back\slash \*", "2*3*4"] {
            let markup = to_markup(body, &[]);
            assert_eq!(parse_markup(&markup), (body.to_owned(), vec![]), "{body} -> {markup}");
        }
        assert_eq!(to_markup("a *b*", &[]), r"a \*b\*");
    }

    #[test]
    fn markup_leaves_urls_alone() {
        let (body, ranges) = parse_markup("see https://x.com/a_b_c~d and _this_");
        assert_eq!(body, "see https://x.com/a_b_c~d and this");
        assert_eq!(ranges.len(), 1, "only the real italic: {ranges:?}");
        assert_eq!(ranges[0].start, Some(30));
    }

    #[test]
    fn markup_never_panics_on_odd_input() {
        for s in ["", "\\", "|", "||", "|||", "||a", "*\u{FFFC}*", "_é_", "~~~", "``", "\\*"] {
            let (body, ranges) = parse_markup(s);
            let _ = segments(&body, &ranges);
        }
    }

    fn found(s: &str) -> Vec<&str> {
        links(s).into_iter().map(|r| &s[r]).collect()
    }

    #[test]
    fn links_are_found_without_trailing_punctuation() {
        assert_eq!(found("see https://signal.org."), ["https://signal.org"]);
        assert_eq!(found("http://a.b/c?d=1&e=2, then https://x.y/z!"), ["http://a.b/c?d=1&e=2", "https://x.y/z"]);
        assert_eq!(found("really? https://x.y/?q=a..."), ["https://x.y/?q=a"]);
        assert_eq!(found("'https://x.y/it's'"), ["https://x.y/it's"]);
        assert!(found("https:// nothing, http:/x, ftp://x, myhttps://x.y").is_empty());
    }

    #[test]
    fn links_balance_parentheses() {
        assert_eq!(found("(see https://en.wikipedia.org/wiki/Rust_(language))"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(found("(https://x.y/a)"), ["https://x.y/a"]);
    }

    #[test]
    fn links_stop_at_emoji_and_keep_unicode() {
        assert_eq!(found("🎉https://x.y/a🎉 ok"), ["https://x.y/a"]);
        assert_eq!(found("https://de.wikipedia.org/wiki/Äpfel。次"), ["https://de.wikipedia.org/wiki/Äpfel"]);
        assert_eq!(found("hi \u{FFFC}https://x.y"), ["https://x.y"]);
        assert_eq!(found("a\nhttps://x.y\tb"), ["https://x.y"]);
    }
}
