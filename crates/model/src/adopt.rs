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

/// A Brewfile line: `brew "name"` or `cask "name"`, with a final newline.
pub fn brewfile_line(kind: BrewKind, name: &str) -> String {
    let word = match kind {
        BrewKind::Formula => "brew",
        BrewKind::Cask => "cask",
    };
    format!("{word} {}\n", quoted(name))
}

/// True when a non-comment line is `brew` or `cask` followed by the quoted `name`, then the end
/// of the line, a `,` (options) or a `#` comment. The match is on the whole quoted string: a
/// tap-qualified `owner/tap/tuicr` does not match `tuicr`, because another tap can ship a
/// different formula of that short name. A caller that treats several spellings as one package
/// asks for each spelling.
pub fn brewfile_has(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        let Some(rest) = line
            .strip_prefix("brew")
            .or_else(|| line.strip_prefix("cask"))
        else {
            return false;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            return false;
        };
        // The keyword must be followed by whitespace or the quote itself (`brewx` is not one).
        if line.len() - rest.len() == 4 && !line[4..].starts_with(quote) {
            return false;
        }
        let Some(body) = rest[1..].strip_prefix(name) else {
            return false;
        };
        let Some(after) = body.strip_prefix(quote) else {
            return false;
        };
        let after = after.trim_start();
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
        assert!(!brewfile_has("# brew \"tuicr\"\n", "tuicr"));
        assert!(brewfile_has("brew \"tuicr\", link: false\n", "tuicr"));
        assert!(!brewfile_has("brew \"tuicr-x\"\n", "tuicr"));
        assert!(!brewfile_has("brew \"xtuicr\"\n", "tuicr"));
        assert!(brewfile_has("  cask \"tuicr\" # note\n", "tuicr"));
        assert!(brewfile_has("tap \"a/b\"\nbrew 'tuicr'\n", "tuicr"));
        assert!(brewfile_has("brew \"tuicr\"", "tuicr"));
        assert!(!brewfile_has("tap \"tuicr\"\n", "tuicr"));
        assert!(!brewfile_has("brewx \"tuicr\"\n", "tuicr"));
        assert!(!brewfile_has("brew \"owner/tap/tuicr\"\n", "tuicr"));
        assert!(brewfile_has(
            "brew \"owner/tap/tuicr\"\n",
            "owner/tap/tuicr"
        ));
    }
}
