//! ANSI color for the text output, on a terminal only. JSON output is never colored.

use std::io::IsTerminal;

/// Whether to color a stream: a terminal, without `NO_COLOR` (no-color.org) or `TERM=dumb`.
/// A test-isolation build also colors a pipe when `DFM_COLOR` is set, so tests can see it.
pub(crate) fn enabled(stream: &impl IsTerminal) -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if set("NO_COLOR") || std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
        return false;
    }
    stream.is_terminal() || (cfg!(feature = "test-isolation") && set("DFM_COLOR"))
}

/// `s` in the SGR style `code` when `on`, else `s` as it is.
pub(crate) fn style(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// A status or result name in its color: bold red for what stopped, bold yellow for what will
/// change, yellow for what waits, green for what needs nothing, bold green for what changed or
/// ended well, magenta for what another scope acts on, cyan for what is held on purpose.
pub(crate) fn status(on: bool, word: &str) -> String {
    style(on, status_code(word), word)
}

pub(crate) fn status_code(word: &str) -> &'static str {
    match word {
        "ok" | "updated" => "1;32",
        "current" => "32",
        "planned" => "1;33",
        "pending" | "not_started" => "33",
        "held" => "36",
        "not_in_scope" => "35",
        _ => "1;31",
    }
}

/// A line label such as `summary:` or `plan digest:`: bold blue.
pub(crate) fn label(on: bool, s: &str) -> String {
    style(on, "1;34", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_off_leaves_the_text_and_on_wraps_it() {
        assert_eq!(status(false, "blocked"), "blocked");
        assert_eq!(status(true, "blocked"), "\x1b[1;31mblocked\x1b[0m");
        assert_eq!(status(true, "planned"), "\x1b[1;33mplanned\x1b[0m");
        assert_eq!(status(true, "current"), "\x1b[32mcurrent\x1b[0m");
        assert_eq!(status(true, "not_in_scope"), "\x1b[35mnot_in_scope\x1b[0m");
        assert_eq!(status(true, "ok"), "\x1b[1;32mok\x1b[0m");
        assert_eq!(label(false, "summary:"), "summary:");
    }
}
