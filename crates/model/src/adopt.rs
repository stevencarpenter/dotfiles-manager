//! Pure repo edits for `dfm adopt`: resource blocks for a module manifest and lines for a
//! Brewfile. Every edit is an append; no existing byte changes.

use toml::Value;

const MAX_ID_BYTES: usize = 64;

/// The resource id for a home-relative target. Every run of characters other than ASCII letters
/// and digits becomes one `-`, so a leading `.`, `..`, `//` and `.d` all fold the same way. An id
/// must start with a letter (the resolver's rule): a digit start gets an `f-` prefix, and an
/// empty result becomes `f`. The id is cut to 64 bytes without a trailing `-`.
pub fn resource_id(home_rel: &str) -> String {
    let mut id = String::new();
    for part in home_rel
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
    {
        if !id.is_empty() {
            id.push('-');
        }
        id.push_str(&part.to_ascii_lowercase());
    }
    if !id.starts_with(|c: char| c.is_ascii_lowercase()) {
        id.insert_str(0, if id.is_empty() { "f" } else { "f-" });
    }
    id.truncate(MAX_ID_BYTES);
    id.truncate(id.trim_end_matches('-').len());
    id
}

fn quoted(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// One `fs-link/v1` resource block, led by a blank line, in the form the manifests use. Strings
/// are written by the TOML serializer, so quotes, backslashes and control characters are escaped.
pub fn link_block(id: &str, target: &str, source: &str) -> String {
    format!(
        "\n[[resources]]\nid = {}\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = {}\n[resources.value]\nsource = {}\n",
        quoted(id),
        quoted(target),
        quoted(source),
    )
}

/// `existing` followed by `addition`. The existing bytes are always an exact prefix of the
/// result. A `\n` is added between them only when `existing` is non-empty and does not end in
/// one, so the addition starts on its own line. A CRLF file already ends in `\n` and is kept.
pub fn append(existing: &[u8], addition: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(existing.len() + addition.len() + 1);
    out.extend_from_slice(existing);
    if existing.last().is_some_and(|b| *b != b'\n') {
        out.push(b'\n');
    }
    out.extend_from_slice(addition.as_bytes());
    out
}

/// The Brewfile entry kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrewKind {
    Formula,
    Cask,
}

impl BrewKind {
    fn keyword(self) -> &'static str {
        match self {
            BrewKind::Formula => "brew",
            BrewKind::Cask => "cask",
        }
    }
}

/// A Brewfile line: `brew "name"` or `cask "name"`, with a final newline.
pub fn brewfile_line(kind: BrewKind, name: &str) -> String {
    format!("{} {}\n", kind.keyword(), quoted(name))
}

/// True when a non-comment line is the keyword of `kind` followed by the quoted `name`, then the
/// end of the line, a `,` (options) or a `#` comment. The paren form `brew("name")` counts too.
/// A formula and a cask of the same name are different packages, so `brew "docker"` does not
/// count for the cask. The match is on the whole quoted string: a tap-qualified
/// `owner/tap/tuicr` does not match `tuicr`, and a `tap "owner/tap"` line does not make
/// `brew "tuicr"` count for `owner/tap/tuicr`, because brew resolves a short name to
/// homebrew/core first. A caller that treats several spellings as one package asks for each
/// spelling.
pub fn brewfile_has(text: &str, kind: BrewKind, name: &str) -> bool {
    text.lines().any(|line| {
        let Some(rest) = line.trim_start().strip_prefix(kind.keyword()) else {
            return false;
        };
        // The keyword must end here (`brewx` is not one).
        let (paren, rest) = match rest.trim_start().strip_prefix('(') {
            Some(r) => (true, r.trim_start()),
            None if rest.starts_with(|c: char| c.is_whitespace() || matches!(c, '"' | '\'')) => {
                (false, rest.trim_start())
            }
            None => return false,
        };
        let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            return false;
        };
        let Some(after) = rest[1..]
            .strip_prefix(name)
            .and_then(|b| b.strip_prefix(quote))
        else {
            return false;
        };
        let mut after = after.trim_start();
        if paren {
            match after.strip_prefix(')') {
                Some(a) => after = a.trim_start(),
                None => return after.starts_with(','),
            }
        }
        after.is_empty() || after.starts_with([',', '#'])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::valid_id;

    #[test]
    fn resource_id_matches_the_existing_dotfiles_ids() {
        assert_eq!(
            resource_id(".config/worktrunk/config.toml"),
            "config-worktrunk-config-toml"
        );
        assert_eq!(
            resource_id(".pi/agent/AGENTS.d/20-output-policy.md"),
            "pi-agent-agents-d-20-output-policy-md"
        );
        assert_eq!(resource_id(".ssh/config"), "ssh-config");
    }

    #[test]
    fn resource_id_always_passes_the_resolver_id_rule() {
        let long = format!(".config/{}", "a".repeat(100));
        for p in [
            "..",
            "../x",
            ".config//x..y",
            "A/B",
            ".1password/x",
            "9",
            "",
            "/",
            "...",
            "é/ü",
            "a b/c",
            &long,
        ] {
            let id = resource_id(p);
            assert!(valid_id(&id), "{p:?} gave {id:?}");
        }
        assert_eq!(resource_id(".config//x..y"), "config-x-y");
        assert_eq!(resource_id("A/B"), "a-b");
    }

    #[test]
    fn link_block_is_the_manifest_form_and_parses() {
        let b = link_block("config-x", ".config/x.toml", "home/.config/x.toml");
        assert_eq!(
            b,
            "\n[[resources]]\nid = \"config-x\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/x.toml\"\n[resources.value]\nsource = \"home/.config/x.toml\"\n"
        );
        let t: toml::Table = b.parse().unwrap();
        let r = &t["resources"].as_array().unwrap()[0];
        assert_eq!(r["id"].as_str(), Some("config-x"));
        assert_eq!(r["kind"].as_str(), Some("link"));
        assert_eq!(r["adapter"].as_str(), Some("fs-link/v1"));
        assert_eq!(r["target"].as_str(), Some(".config/x.toml"));
        assert_eq!(r["value"]["source"].as_str(), Some("home/.config/x.toml"));
    }

    #[test]
    fn link_block_escapes_toml_strings() {
        let b = link_block("id", "a\"b\\c\nd", "s'\"\u{7f}");
        let t: toml::Table = b.parse().unwrap();
        let r = &t["resources"].as_array().unwrap()[0];
        assert_eq!(r["target"].as_str(), Some("a\"b\\c\nd"));
        assert_eq!(r["value"]["source"].as_str(), Some("s'\"\u{7f}"));
    }

    #[test]
    fn append_adds_a_newline_only_when_the_file_lacks_one() {
        let add = link_block("i", "t", "s");
        let r = append(b"a", &add);
        assert_eq!(r, format!("a\n{add}").into_bytes());
        let r = append(b"a\n", &add);
        assert_eq!(r, format!("a\n{add}").into_bytes());
        for existing in [&b""[..], b"a", b"a\n", b"a\r\n", b"a\r", b"\xff\xfe"] {
            assert!(append(existing, &add).starts_with(existing));
        }
        assert_eq!(append(b"", "x\n"), b"x\n");
        assert_eq!(append(b"a\r\n", "x\n"), b"a\r\nx\n");
    }

    #[test]
    fn brewfile_line_names_the_kind() {
        assert_eq!(
            brewfile_line(BrewKind::Formula, "tuicr"),
            "brew \"tuicr\"\n"
        );
        assert_eq!(brewfile_line(BrewKind::Cask, "x"), "cask \"x\"\n");
    }

    #[test]
    fn brewfile_has_ignores_comments_and_matches_options() {
        let f = |text: &str, name: &str| brewfile_has(text, BrewKind::Formula, name);
        assert!(!f("# brew \"tuicr\"\n", "tuicr"));
        assert!(f("brew \"tuicr\", link: false\n", "tuicr"));
        assert!(!f("brew \"tuicr-x\"\n", "tuicr"));
        assert!(!f("brew \"xtuicr\"\n", "tuicr"));
        assert!(brewfile_has(
            "  cask \"tuicr\" # note\n",
            BrewKind::Cask,
            "tuicr"
        ));
        assert!(f("tap \"a/b\"\nbrew 'tuicr'\n", "tuicr"));
        assert!(f("brew \"tuicr\"", "tuicr"));
        assert!(!f("tap \"tuicr\"\n", "tuicr"));
        assert!(!f("brewx \"tuicr\"\n", "tuicr"));
        assert!(!f("brew \"owner/tap/tuicr\"\n", "tuicr"));
        assert!(f("brew \"owner/tap/tuicr\"\n", "owner/tap/tuicr"));
    }

    #[test]
    fn brewfile_has_matches_only_the_asked_kind() {
        // A formula and a cask of the same name are different packages.
        assert!(!brewfile_has("brew \"docker\"\n", BrewKind::Cask, "docker"));
        assert!(!brewfile_has(
            "cask \"docker\"\n",
            BrewKind::Formula,
            "docker"
        ));
        assert!(brewfile_has(
            "brew \"docker\"\n",
            BrewKind::Formula,
            "docker"
        ));
        assert!(brewfile_has("cask \"docker\"\n", BrewKind::Cask, "docker"));
    }

    #[test]
    fn brewfile_has_matches_the_paren_form() {
        let f = |text: &str| brewfile_has(text, BrewKind::Formula, "tuicr");
        assert!(f("brew(\"tuicr\")\n"));
        assert!(f("brew ( 'tuicr' )\n"));
        assert!(f("brew(\"tuicr\", link: false)\n"));
        assert!(f("brew(\"tuicr\") # note\n"));
        assert!(brewfile_has("cask(\"tuicr\")\n", BrewKind::Cask, "tuicr"));
        assert!(!f("brew(\"tuicr-x\")\n"));
        assert!(!f("brew(\"tuicr\"\n"));
        assert!(!f("cask(\"tuicr\")\n"));
    }

    #[test]
    fn a_tap_line_does_not_make_a_short_name_count_for_the_tap_package() {
        // Homebrew resolves an unqualified name to homebrew/core first, so `brew "x"` under
        // `tap "owner/tap"` can install core's x on a fresh machine. The qualified line is kept.
        let text = "tap \"owner/tap\"\nbrew \"tuicr\"\n";
        assert!(!brewfile_has(text, BrewKind::Formula, "owner/tap/tuicr"));
    }
}
