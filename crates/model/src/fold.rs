//! One case fold for every path comparison (docs/state.md, "Ledger").

/// Case folding close to what APFS applies: upper then lower case maps the long s (U+017F) to
/// `s`, the Kelvin sign to `k` and the sharp s to `ss`, which a plain lowercase does not.
/// Unicode normalization forms are not folded (a known limit, docs/planner.md).
pub fn fold(s: &str) -> String {
    s.to_uppercase().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::fold;

    #[test]
    fn folds_what_a_plain_lowercase_misses() {
        assert_eq!(fold("Config"), "config");
        assert_eq!(fold("\u{17f}"), "s");
        assert_eq!(fold("\u{212a}"), "k");
    }
}
