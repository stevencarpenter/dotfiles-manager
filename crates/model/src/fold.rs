//! One case fold for every path comparison (docs/state.md, "Ledger entries" and "Ledger paths").

/// Case folding close to what APFS applies: upper then lower case maps the long s (U+017F) to
/// `s`, the Kelvin sign to `k` and the sharp s to `ss`, which a plain lowercase does not.
/// Unicode normalization forms are not folded (a known limit, docs/planner.md).
pub fn fold(s: &str) -> String {
    s.to_uppercase().to_lowercase()
}

/// Whether two paths or names are the same after the case fold.
pub fn eq(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}

#[cfg(test)]
mod tests {
    use super::{eq, fold};

    #[test]
    fn eq_compares_the_folded_text() {
        assert!(eq("Config", "cONFIG"));
        assert!(eq("\u{17f}", "S"));
        assert!(!eq("a", "b"));
    }

    #[test]
    fn folds_what_a_plain_lowercase_misses() {
        assert_eq!(fold("Config"), "config");
        assert_eq!(fold("\u{17f}"), "s");
        assert_eq!(fold("\u{212a}"), "k");
    }
}
