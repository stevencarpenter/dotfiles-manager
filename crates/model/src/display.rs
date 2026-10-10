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
/// rewrite a terminal or disguise itself.
pub fn visible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else if is_invisible(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
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
