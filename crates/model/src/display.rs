//! Safe display of untrusted text (docs/architecture.md section 9, item 5: every untrusted string
//! rendered to a terminal has control characters and escape sequences replaced by visible escapes).

/// Characters that change how text is laid out or hide in it: zero-width and joining controls,
/// the byte order mark, line and paragraph separators, bidirectional embedding, override and
/// isolate controls, variation selectors, tag characters, invisible fillers and soft hyphens.
/// Rust's standard library has no general category, so this is an explicit list of the format
/// and filler characters that render as nothing or reorder text.
pub fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{2800}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e007f}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

/// Replaces control and invisible characters with visible escapes so untrusted text cannot
/// rewrite a terminal or disguise itself. A backslash becomes `\\`, so literal text such as
/// `\n` cannot pass for the escape of a real newline. Apply it once, to the text that is
/// printed: a second pass escapes the first pass's backslashes again.
pub fn visible(s: &str) -> String {
    escape(s, true)
}

/// Like [`visible`], but a backslash stays as it is. Use it only for text a user pastes back
/// into single quotes of a shell, where a backslash is literal and a doubled one would change
/// the value (`--allow-disruptive 'remove@core.x@a\b'`).
pub fn visible_for_paste(s: &str) -> String {
    escape(s, false)
}

fn escape(s: &str, double_backslash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' && double_backslash {
            out.push_str("\\\\");
        } else if c == '\\' {
            out.push(c);
        } else if c.is_control() {
            out.extend(c.escape_default());
        } else if is_invisible(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// `visible(s)` cut to `max` characters of the visible text. The cut falls between the escapes
/// of two source characters, never inside one, and counts what is shown: a control character
/// grows to up to ten characters (`\u{1b}` is six), so cutting the source first would let a
/// subject exceed the bound.
pub fn bounded_visible(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut shown = 0;
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let piece = visible(c.encode_utf8(&mut buf));
        shown += piece.chars().count();
        if shown > max {
            break;
        }
        out.push_str(&piece);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_invisible_characters_become_visible_escapes() {
        assert_eq!(
            visible("a\nb\u{202e}c\u{200b}d\u{feff}e\u{2066}f\u{7}"),
            "a\\nb\\u{202e}c\\u{200b}d\\u{feff}e\\u{2066}f\\u{7}"
        );
        assert_eq!(visible("plain ünï text"), "plain ünï text");
    }

    #[test]
    fn visible_for_paste_keeps_a_backslash_and_still_escapes_controls() {
        assert_eq!(visible_for_paste("'a\\b'\n\u{202e}"), "'a\\b'\\n\\u{202e}");
    }

    #[test]
    fn bounded_visible_counts_the_escaped_text_and_never_cuts_an_escape() {
        let long = "\u{1b}".repeat(5000);
        let cut = bounded_visible(&long, 1024);
        assert!(cut.chars().count() <= 1024, "{}", cut.chars().count());
        assert_eq!(cut, "\\u{1b}".repeat(1024 / 6));
        assert_eq!(bounded_visible("a\\b", 1024), "a\\\\b");
        assert_eq!(bounded_visible("abc", 2), "ab");
    }

    #[test]
    fn a_backslash_is_escaped_so_literal_text_cannot_pass_for_an_escape() {
        // A real newline and the two characters `\` `n` must print differently.
        assert_eq!(visible("a\nb"), "a\\nb");
        assert_eq!(visible("a\\nb"), "a\\\\nb");
        assert_ne!(visible("\n"), visible("\\n"));
        assert_eq!(visible("C:\\x"), "C:\\\\x");
        // The braille blank renders as an empty cell.
        assert_eq!(visible("a\u{2800}b"), "a\\u{2800}b");
        // Text with no backslash that was escaped once is escaped again, so apply it once.
        assert_ne!(visible(&visible("\n")), visible("\n"));
    }

    #[test]
    fn the_wider_invisible_set_is_covered() {
        for c in [
            '\u{061c}',
            '\u{2028}',
            '\u{2029}',
            '\u{034f}',
            '\u{180e}',
            '\u{e0001}',
            '\u{e0041}',
            '\u{e007f}',
            '\u{fe0f}',
            '\u{e0100}',
            '\u{115f}',
            '\u{1160}',
            '\u{3164}',
            '\u{ffa0}',
            '\u{17b4}',
            '\u{00ad}',
            '\u{200d}',
            '\u{2064}',
            '\u{206f}',
            '\u{2800}',
        ] {
            assert!(is_invisible(c), "{:x}", c as u32);
            assert!(!visible(&c.to_string()).contains(c), "{:x}", c as u32);
        }
        for c in [
            'a', ' ', 'é', '日', '😀', '\u{00a0}', '\u{2010}', '\u{ff21}',
        ] {
            assert!(!is_invisible(c), "{:x}", c as u32);
        }
    }
}
