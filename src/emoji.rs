//! Emoji helpers: `:shortcode:` expansion, emoji-only ("jumbomoji") detection, picker search.

/// Longest emoji sequence (ZWJ families with skin tones) is well under this many bytes.
const MAX_EMOJI_BYTES: usize = 64;

/// Splits `s` into emoji, or `None` if anything other than emoji/whitespace is present.
pub fn only_emoji(s: &str) -> Option<Vec<&'static emojis::Emoji>> {
    let mut out = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_EMOJI_BYTES);
        let found = loop {
            if end == 0 {
                break None;
            }
            if rest.is_char_boundary(end) {
                // Tolerate a trailing VS16 that some keyboards add or drop.
                if let Some(e) = emojis::get(&rest[..end])
                    .or_else(|| emojis::get(rest[..end].trim_end_matches('\u{FE0F}')))
                {
                    break Some((e, end));
                }
            }
            end -= 1;
        };
        let (e, len) = found?;
        out.push(e);
        rest = rest[len..].trim_start();
    }
    (!out.is_empty()).then_some(out)
}

/// Signal Desktop renders messages of 1-5 emoji (and nothing else) large.
pub fn is_jumbo(s: &str) -> bool {
    only_emoji(s).is_some_and(|e| e.len() <= 5)
}

/// Replaces complete `:shortcode:` tokens with their emoji; unknown codes stay as typed.
pub fn replace_shortcodes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let code_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '-'))
            .unwrap_or(after.len());
        if code_len > 0 && after[code_len..].starts_with(':') {
            if let Some(e) = emojis::get_by_shortcode(&after[..code_len]) {
                out.push_str(e.as_str());
                rest = &after[code_len + 1..];
                continue;
            }
        }
        out.push(':');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Picker search over name and shortcodes; empty query returns the given group.
pub fn search(query: &str, group: emojis::Group) -> Vec<&'static emojis::Emoji> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return group.emojis().collect();
    }
    emojis::iter()
        .filter(|e| e.name().contains(&q) || e.shortcodes().any(|c| c.contains(&q)))
        .collect()
}

/// Signal's default reaction set.
pub const REACTIONS: [&str; 6] = ["❤️", "👍", "👎", "😂", "😮", "😢"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jumbo_detection() {
        assert!(is_jumbo("😀"));
        assert!(is_jumbo(" 😀 👍🏽 "));
        assert!(is_jumbo("👨‍👩‍👧‍👦"), "ZWJ family is one emoji");
        assert!(is_jumbo("❤️"));
        assert!(is_jumbo("❤"), "missing VS16 still counts");
        assert!(is_jumbo("🇵🇹"), "flag");
        assert!(!is_jumbo("😀😀😀😀😀😀"), "more than five");
        assert!(!is_jumbo("hi 😀"));
        assert!(!is_jumbo(""));
        assert!(!is_jumbo("   "));
        assert!(!is_jumbo("1"));
    }

    #[test]
    fn only_emoji_counts_sequences() {
        assert_eq!(only_emoji("👍🏽👍").unwrap().len(), 2);
    }

    #[test]
    fn shortcodes() {
        assert_eq!(replace_shortcodes("hi :smile:!"), "hi 😄!");
        assert_eq!(replace_shortcodes(":+1::heart:"), "👍❤️");
        assert_eq!(replace_shortcodes("time 10:30:00"), "time 10:30:00");
        assert_eq!(replace_shortcodes(":notacode: :"), ":notacode: :");
        assert_eq!(replace_shortcodes("a:b"), "a:b");
        assert_eq!(replace_shortcodes(""), "");
        assert_eq!(replace_shortcodes("é:smile:é"), "é😄é");
    }

    #[test]
    fn picker_search() {
        assert!(search("thumbs", emojis::Group::Activities).iter().any(|e| e.as_str() == "👍"));
        assert!(!search("", emojis::Group::SmileysAndEmotion).is_empty());
        assert!(search("zzzzqqq", emojis::Group::SmileysAndEmotion).is_empty());
    }

    #[test]
    fn reaction_set_is_valid_emoji() {
        assert!(REACTIONS.iter().all(|r| is_jumbo(r)));
    }
}
