//! mIRC control-code → Fluxer (Discord-flavoured) markdown translation.
//!
//! Modules and themes speak mIRC formatting (`\x02` bold, `\x1f` underline, `\x16` reverse,
//! `\x03fg[,bg]` / `\x04RRGGBB` colours, `\x0f` reset). Fluxer renders Discord-style markdown,
//! which has bold/underline/strikethrough but no inline colour. This converter runs once at the
//! Fluxer outbound boundary, so module and theme text needs no changes. mIRC formatting is
//! stateful (a marker toggles, `\x0f` resets everything), so this is a small state machine, not
//! a regex: open markers are tracked and explicitly closed at reset points and at end of input.

/// Convert mIRC formatting codes in `text` to Fluxer markdown. Colour codes are dropped (Fluxer
/// has no inline colour); bold and underline become `**` / `__`; reverse-video and reset close or
/// are consumed. Any marker still open at end of input is closed so formatting cannot leak into
/// the next message.
pub fn irc_to_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Open markdown markers, in the order they were opened (closed in reverse).
    let mut open: Vec<&'static str> = Vec::new();
    let mut chars = text.chars().peekable();

    fn toggle(open: &mut Vec<&'static str>, out: &mut String, marker: &'static str) {
        if let Some(pos) = open.iter().position(|m| *m == marker) {
            open.remove(pos);
        } else {
            open.push(marker);
        }
        out.push_str(marker);
    }

    while let Some(c) = chars.next() {
        match c {
            '\u{2}' => toggle(&mut open, &mut out, "**"),
            '\u{1f}' => toggle(&mut open, &mut out, "__"),
            '\u{16}' => {} // reverse video: no markdown equivalent, drop
            '\u{f}' => {
                // reset: close everything still open, innermost first
                while let Some(m) = open.pop() {
                    out.push_str(m);
                }
            }
            '\u{3}' => {
                // \x03fg[,bg] — 1-2 digits each, dropped. No digits resets colour only.
                consume_digits(&mut chars, 2);
                if chars.peek() == Some(&',') {
                    chars.next();
                    consume_digits(&mut chars, 2);
                }
            }
            '\u{4}' => {
                // \x04RRGGBB[,RRGGBB] — hex colours, dropped.
                consume_hex(&mut chars, 6);
                if chars.peek() == Some(&',') {
                    chars.next();
                    consume_hex(&mut chars, 6);
                }
            }
            _ => out.push(c),
        }
    }
    while let Some(m) = open.pop() {
        out.push_str(m);
    }
    out
}

/// Consume up to `max` ASCII digits (mIRC colour parameters).
fn consume_digits(chars: &mut std::iter::Peekable<std::str::Chars>, max: usize) {
    for _ in 0..max {
        match chars.peek() {
            Some(c) if c.is_ascii_digit() => {
                chars.next();
            }
            _ => break,
        }
    }
}

/// Consume up to `max` hex digits (mIRC \x04 colour parameters).
fn consume_hex(chars: &mut std::iter::Peekable<std::str::Chars>, max: usize) {
    for _ in 0..max {
        match chars.peek() {
            Some(c) if c.is_ascii_hexdigit() => {
                chars.next();
            }
            _ => break,
        }
    }
}

/// Strip characters that must never reach the wire (CRLF injection, stray C0 controls) and
/// truncate to Fluxer's per-message content limit (4000 on fluxer.com, per instance discovery).
pub fn sanitize_outbound(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || (*c >= '\u{20}' && *c != '\u{7f}'))
        .collect();
    const LIMIT: usize = 4000;
    if cleaned.chars().count() <= LIMIT {
        cleaned
    } else {
        let mut end = 0;
        for (i, (idx, c)) in cleaned.char_indices().enumerate() {
            if i >= LIMIT {
                break;
            }
            end = idx + c.len_utf8();
        }
        cleaned[..end].to_string()
    }
}

/// Split long outbound text into word-aligned chunks that each fit Fluxer's content limit.
pub fn split_outbound(text: &str) -> Vec<String> {
    const LIMIT: usize = 3900; // headroom under the 4000-char content cap
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.chars().count() > LIMIT {
        let mut end = 0;
        let mut last_space = None;
        for (i, (idx, c)) in rest.char_indices().enumerate() {
            if i >= LIMIT {
                break;
            }
            end = idx + c.len_utf8();
            if c == ' ' || c == '\n' {
                last_space = Some(end);
            }
        }
        let cut = last_space.unwrap_or(end).min(rest.len());
        let (chunk, remaining) = rest.split_at(cut);
        chunks.push(chunk.trim_end().to_string());
        rest = remaining.trim_start();
    }
    if !rest.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(irc_to_markdown("hello there"), "hello there");
    }

    #[test]
    fn bold_and_underline_toggle() {
        assert_eq!(irc_to_markdown("\u{2}bold\u{2} plain"), "**bold** plain");
        assert_eq!(
            irc_to_markdown("\u{1f}under\u{1f} \u{2}both\u{2}"),
            "__under__ **both**"
        );
    }

    #[test]
    fn colors_are_dropped_but_text_kept() {
        assert_eq!(irc_to_markdown("\u{3}12[Module]\u{f} hi"), "[Module] hi");
        assert_eq!(
            irc_to_markdown("\u{3}04,12red\u{3} and \u{2}bold\u{f}"),
            "red and **bold**"
        );
        assert_eq!(irc_to_markdown("\u{4}ff0000hex"), "hex");
        // The comma is part of the colour spec and is consumed with it.
        assert_eq!(irc_to_markdown("\u{3}4,x"), "x");
    }

    #[test]
    fn reset_closes_open_markers() {
        assert_eq!(irc_to_markdown("\u{2}a\u{1f}b\u{f}c"), "**a__b__**c");
    }

    #[test]
    fn unclosed_markers_are_closed_at_end() {
        assert_eq!(irc_to_markdown("\u{2}never closed"), "**never closed**");
    }

    #[test]
    fn reverse_video_is_dropped() {
        assert_eq!(irc_to_markdown("\u{16}rev\u{16}"), "rev");
    }

    #[test]
    fn sanitize_strips_crlf_and_controls_but_keeps_newlines() {
        assert_eq!(sanitize_outbound("a\r\nb\u{7} c"), "a\nb c");
        let long = "x".repeat(4500);
        assert_eq!(sanitize_outbound(&long).chars().count(), 4000);
    }

    #[test]
    fn split_keeps_short_text_as_one_chunk() {
        assert_eq!(split_outbound("short"), vec!["short".to_string()]);
    }

    #[test]
    fn split_breaks_long_text_word_aligned_under_limit() {
        let word = "abcdefghij"; // 10 chars
        let line = format!("{} ", word.repeat(500)); // 5500 chars
        let chunks = split_outbound(&line);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.chars().count() <= 3900);
        }
        assert_eq!(chunks.concat().replace(' ', ""), word.repeat(500));
    }
}
