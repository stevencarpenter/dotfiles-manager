//! Build-controls rules for a staged source tree (docs/architecture.md section 8).
//!
//! These functions take already-read text. They implement a restricted TOML
//! grammar that fails closed: any construct they do not recognise is refused.
//! `scripts/bootstrap.sh` implements the same grammar and rules in awk. The two
//! must agree on every fixture in `tests/fixtures/build-controls` (docs/bootstrap.md).

use std::collections::{BTreeMap, HashSet};

pub const MAX_FILE_BYTES: u64 = 1 << 20;

// Cargo also accepts the underscore spellings of the dev and build tables.
const DEP_TABLES: [&str; 5] = [
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "dev_dependencies",
    "build_dependencies",
];
const DEP_KEYS: [&str; 11] = [
    "version",
    "features",
    "optional",
    "default-features",
    "default_features",
    "package",
    "workspace",
    "path",
    "artifact",
    "lib",
    "target",
];
const DENY_TABLE_VALUES: [&str; 9] = [
    "patch",
    "replace",
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "dev_dependencies",
    "build_dependencies",
    "target",
    "workspace",
];
const CRATES_IO: [&str; 2] = [
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
];

type Res = Result<(), String>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Val {
    Str(String),
    Scalar,
    /// Items; `None` marks a non-string scalar.
    Arr(Vec<Option<String>>),
    Tbl,
}

fn is_ws(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

fn is_bare(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Bytes outside printable ASCII, tab and newline are refused for every parsed file.
pub fn ascii_only(text: &[u8]) -> bool {
    text.iter()
        .all(|&b| b == b'\t' || b == b'\n' || (0x20..=0x7e).contains(&b))
}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl Cur<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.p).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(is_ws) {
            self.p += 1;
        }
    }

    fn bare(&mut self) -> Option<String> {
        let s = self.p;
        while self.peek().is_some_and(is_bare) {
            self.p += 1;
        }
        (self.p > s).then(|| String::from_utf8_lossy(&self.b[s..self.p]).into_owned())
    }

    fn scalar(&mut self) -> Result<Val, String> {
        let rest = &self.b[self.p.min(self.b.len())..];
        match rest.first() {
            Some(&q) if q == b'"' || q == b'\'' => {
                let end = rest[1..]
                    .iter()
                    .position(|&c| c == q)
                    .ok_or("unterminated string")?;
                let s = &rest[1..1 + end];
                if q == b'"' && s.contains(&b'\\') {
                    return Err("escape in string".into());
                }
                self.p += end + 2;
                Ok(Val::Str(String::from_utf8_lossy(s).into_owned()))
            }
            _ => {
                for word in ["true", "false"] {
                    if rest.starts_with(word.as_bytes()) {
                        self.p += word.len();
                        if self.peek().is_some_and(is_bare) {
                            return Err("unrecognised value".into());
                        }
                        return Ok(Val::Scalar);
                    }
                }
                let mut n = usize::from(matches!(rest.first(), Some(b'+' | b'-')));
                let digits = n;
                while rest
                    .get(n)
                    .is_some_and(|c| c.is_ascii_digit() || *c == b'_')
                {
                    n += 1;
                }
                if n == digits || !rest[digits].is_ascii_digit() {
                    return Err("unrecognised value".into());
                }
                self.p += n;
                if self
                    .peek()
                    .is_some_and(|c| is_bare(c) || c == b'.' || c == b':')
                {
                    return Err("unrecognised number".into());
                }
                Ok(Val::Scalar)
            }
        }
    }

    fn array(&mut self) -> Result<Val, String> {
        self.p += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.p += 1;
            return Ok(Val::Arr(items));
        }
        loop {
            self.ws();
            items.push(match self.scalar()? {
                Val::Str(s) => Some(s),
                _ => None,
            });
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.p += 1;
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.p += 1;
                        break;
                    }
                }
                Some(b']') => {
                    self.p += 1;
                    break;
                }
                _ => return Err("array syntax".into()),
            }
        }
        Ok(Val::Arr(items))
    }

    fn inline(&mut self, key: &str, h: &mut dyn Hooks) -> Result<Val, String> {
        self.p += 1;
        self.ws();
        if self.peek() == Some(b'}') {
            self.p += 1;
            return Ok(Val::Tbl);
        }
        let mut subs = HashSet::new();
        loop {
            self.ws();
            let sk = self.bare().ok_or("inline table key")?;
            if !subs.insert(sk.clone()) {
                return Err("duplicate inline table key".into());
            }
            self.ws();
            if self.peek() != Some(b'=') {
                return Err("inline table syntax".into());
            }
            self.p += 1;
            self.ws();
            let v = if self.peek() == Some(b'[') {
                self.array()?
            } else {
                self.scalar()?
            };
            h.inline(key, &sk, &v)?;
            self.ws();
            match self.peek() {
                Some(b',') => self.p += 1,
                Some(b'}') => {
                    self.p += 1;
                    break;
                }
                _ => return Err("inline table syntax".into()),
            }
        }
        Ok(Val::Tbl)
    }
}

trait Hooks {
    fn header(&mut self, name: &str, array: bool) -> Res;
    fn pair(&mut self, key: &str, v: &Val) -> Res;
    fn inline(&mut self, key: &str, sub: &str, v: &Val) -> Res;
}

fn valid_table_name(n: &str) -> bool {
    !n.is_empty()
        && n.split('.')
            .all(|s| !s.is_empty() && s.bytes().all(is_bare))
}

/// Names already seen: duplicate tables and keys are refused, because a TOML reader
/// that rejects them and one that keeps the last value would disagree about the file.
#[derive(Default)]
struct Seen {
    tables: HashSet<String>,
    keys: HashSet<String>,
}

fn scan(text: &str, h: &mut dyn Hooks) -> Res {
    if !ascii_only(text.as_bytes()) {
        return Err("non-ASCII or control character".into());
    }
    let mut seen = Seen::default();
    for (i, raw) in text.split('\n').enumerate() {
        scan_line(raw.trim_matches([' ', '\t']), h, &mut seen)
            .map_err(|e| format!("line {}: {e}", i + 1))?;
    }
    Ok(())
}

fn scan_line(line: &str, h: &mut dyn Hooks, seen: &mut Seen) -> Res {
    if line.is_empty() || line.starts_with('#') {
        return Ok(());
    }
    let mut c = Cur {
        b: line.as_bytes(),
        p: 0,
    };
    let trailing = |c: &mut Cur, what: &str| -> Res {
        c.ws();
        match c.peek() {
            None | Some(b'#') => Ok(()),
            _ => Err(format!("text after {what}")),
        }
    };
    if line.starts_with('[') {
        let array = line.starts_with("[[");
        c.p = if array { 2 } else { 1 };
        let s = c.p;
        while c.peek().is_some_and(|b| is_bare(b) || b == b'.') {
            c.p += 1;
        }
        let name = &line[s..c.p];
        let close = if array { "]]" } else { "]" };
        if !valid_table_name(name) || !line[c.p..].starts_with(close) {
            return Err("table header".into());
        }
        c.p += close.len();
        trailing(&mut c, "header")?;
        if !array && !seen.tables.insert(name.to_string()) {
            return Err("duplicate table".into());
        }
        seen.keys.clear();
        return h.header(name, array);
    }
    let key = c.bare().ok_or("key")?;
    if !seen.keys.insert(key.clone()) {
        return Err("duplicate key".into());
    }
    c.ws();
    if c.peek() != Some(b'=') {
        return Err("expected =".into());
    }
    c.p += 1;
    c.ws();
    let v = match c.peek() {
        Some(b'[') => c.array()?,
        Some(b'{') => c.inline(&key, h)?,
        _ => c.scalar()?,
    };
    trailing(&mut c, "value")?;
    h.pair(&key, &v)
}

/// True when `v` is a relative path that stays inside the root. `depth` is the
/// number of directory components between the root and the declaring file.
/// A `..` after a normal component is refused, so lexical analysis is exact.
pub fn path_ok(v: &str, depth: usize) -> bool {
    if v.is_empty() || v.starts_with('/') || v.starts_with('~') || v.contains('\\') {
        return false;
    }
    let (mut d, mut seen) = (depth as i64, false);
    for c in v.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if seen {
                    return false;
                }
                d -= 1;
                if d < 0 {
                    return false;
                }
            }
            _ => seen = true,
        }
    }
    true
}

#[derive(PartialEq)]
enum Ctx {
    Ord,
    Dep,
    DepSub,
}

fn classify(table: &str) -> Result<Ctx, String> {
    let s: Vec<&str> = table.split('.').collect();
    if s[0] == "replace" {
        return Err("[replace] is not allowed".into());
    }
    let rest = if s[0] == "patch" {
        if s.len() < 2 || s[1] != "crates-io" {
            return Err("[patch] source is not crates-io".into());
        }
        s.len() - 2
    } else {
        match s.iter().position(|x| DEP_TABLES.contains(x)) {
            None => return Ok(Ctx::Ord),
            Some(k) => s.len() - 1 - k,
        }
    };
    match rest {
        0 => Ok(Ctx::Dep),
        1 => Ok(Ctx::DepSub),
        _ => Err("nested dependency table".into()),
    }
}

struct ManifestRules {
    depth: usize,
    ctx: Ctx,
    table: String,
}

impl Hooks for ManifestRules {
    fn header(&mut self, name: &str, array: bool) -> Res {
        self.ctx = classify(name)?;
        self.table = name.to_string();
        if array && self.ctx != Ctx::Ord {
            return Err("array of dependency tables".into());
        }
        Ok(())
    }

    fn pair(&mut self, key: &str, v: &Val) -> Res {
        if key == "path" && !matches!(v, Val::Str(s) if path_ok(s, self.depth)) {
            return Err("path escapes the staged tree".into());
        }
        match self.ctx {
            Ctx::Dep => {
                if !matches!(v, Val::Str(_) | Val::Tbl) {
                    return Err("unsupported dependency form".into());
                }
            }
            Ctx::DepSub => {
                if !DEP_KEYS.contains(&key) {
                    return Err(format!("unsupported dependency key {key}"));
                }
                if *v == Val::Tbl {
                    return Err("inline table in dependency table".into());
                }
            }
            Ctx::Ord => {
                if *v == Val::Tbl && DENY_TABLE_VALUES.contains(&key) {
                    return Err(format!("table-valued key {key}"));
                }
                if let Val::Str(s) = v
                    && (key == "build" || key == "workspace")
                    && !path_ok(s, self.depth)
                {
                    return Err(format!("{key} escapes the staged tree"));
                }
                if self.table == "workspace"
                    && (key == "members" || key == "exclude" || key == "default-members")
                {
                    let Val::Arr(items) = v else {
                        return Err(format!("{key} is not an array"));
                    };
                    if !items
                        .iter()
                        .all(|i| i.as_deref().is_some_and(|s| path_ok(s, self.depth)))
                    {
                        return Err(format!("{key} entry escapes the staged tree"));
                    }
                }
            }
        }
        Ok(())
    }

    fn inline(&mut self, _key: &str, sub: &str, v: &Val) -> Res {
        if sub == "path" && !matches!(v, Val::Str(s) if path_ok(s, self.depth)) {
            return Err("path escapes the staged tree".into());
        }
        if self.ctx == Ctx::Dep && !DEP_KEYS.contains(&sub) {
            return Err(format!("unsupported dependency key {sub}"));
        }
        Ok(())
    }
}

/// Checks one `Cargo.toml`. `depth` is its directory depth below the staged root.
pub fn check_manifest(text: &str, depth: usize) -> Res {
    scan(
        text,
        &mut ManifestRules {
            depth,
            ctx: Ctx::Ord,
            table: String::new(),
        },
    )
}

struct LockRules {
    table: String,
    manager_seen: bool,
    pin: Option<String>,
    tree: Option<String>,
}

impl Hooks for LockRules {
    fn header(&mut self, name: &str, _array: bool) -> Res {
        if name == "manager" {
            if self.manager_seen {
                return Err("duplicate [manager]".into());
            }
            self.manager_seen = true;
        }
        self.table = name.to_string();
        Ok(())
    }

    fn pair(&mut self, key: &str, v: &Val) -> Res {
        if self.table != "manager" {
            return Ok(());
        }
        let Val::Str(s) = v else {
            return if key == "toolchain" || key == "toolchain_tree_sha256" {
                Err(format!("{key} must be a string"))
            } else {
                Ok(())
            };
        };
        match key {
            "toolchain" if valid_version(s) && self.pin.is_none() => self.pin = Some(s.clone()),
            "toolchain_tree_sha256" if valid_sha(s) && self.tree.is_none() => {
                self.tree = Some(s.clone())
            }
            "toolchain" | "toolchain_tree_sha256" => {
                return Err(format!("invalid or repeated {key}"));
            }
            _ => {}
        }
        Ok(())
    }

    fn inline(&mut self, _: &str, _: &str, _: &Val) -> Res {
        Err("inline table in lock".into())
    }
}

fn valid_version(s: &str) -> bool {
    let p: Vec<&str> = s.split('.').collect();
    p.len() == 3
        && p.iter()
            .all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parses `dotfiles-manager.lock` and returns `(toolchain version, toolchain tree digest)`.
pub fn parse_source_lock(text: &str) -> Result<(String, String), String> {
    let mut r = LockRules {
        table: String::new(),
        manager_seen: false,
        pin: None,
        tree: None,
    };
    scan(text, &mut r)?;
    match (r.pin, r.tree) {
        (Some(p), Some(t)) => Ok((p, t)),
        _ => Err("[manager] needs toolchain and toolchain_tree_sha256".into()),
    }
}

struct ToolchainFile<'a> {
    pin: &'a str,
    table: String,
    seen: bool,
}

impl Hooks for ToolchainFile<'_> {
    fn header(&mut self, name: &str, array: bool) -> Res {
        if name != "toolchain" || array || !self.table.is_empty() {
            return Err("unsupported table".into());
        }
        self.table = name.to_string();
        Ok(())
    }

    fn pair(&mut self, key: &str, v: &Val) -> Res {
        match v {
            Val::Str(s)
                if self.table == "toolchain" && key == "channel" && s == self.pin && !self.seen =>
            {
                self.seen = true;
                Ok(())
            }
            // The formatter and linter components are what a developer checkout asks for.
            Val::Arr(items)
                if self.table == "toolchain"
                    && key == "components"
                    && items
                        .iter()
                        .all(|i| matches!(i.as_deref(), Some("rustfmt" | "clippy"))) =>
            {
                Ok(())
            }
            _ => Err("rust-toolchain.toml differs from the lock pin".into()),
        }
    }

    fn inline(&mut self, _: &str, _: &str, _: &Val) -> Res {
        Err("inline table".into())
    }
}

/// A staged `rust-toolchain.toml` must name exactly the lock pin. Only the `rustfmt` and
/// `clippy` components are allowed besides it.
pub fn check_toolchain_file(text: &str, pin: &str) -> Res {
    let mut r = ToolchainFile {
        pin,
        table: String::new(),
        seen: false,
    };
    scan(text, &mut r)?;
    if r.seen {
        Ok(())
    } else {
        Err("no channel".into())
    }
}

/// `Cargo.lock` must be registry-only. Unrecognised lines are refused.
pub fn check_cargo_lock(text: &str) -> Res {
    if !ascii_only(text.as_bytes()) {
        return Err("non-ASCII or control character".into());
    }
    let quoted = |s: &str| s.len() >= 2 && s.starts_with('"') && s.ends_with('"');
    for (i, raw) in text.split('\n').enumerate() {
        let t = raw.trim_matches([' ', '\t']);
        let ok = t.is_empty()
            || t.starts_with('#')
            || matches!(t, "[[package]]" | "]" | "dependencies = [")
            || t.strip_prefix("version = ").is_some_and(|v| {
                (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                    || (quoted(v)
                        && v[1..v.len() - 1]
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b))
                        && v.len() > 2)
            })
            || t.strip_prefix("name = ").is_some_and(|v| {
                quoted(v) && v.len() > 2 && v[1..v.len() - 1].bytes().all(is_bare)
            })
            || t.strip_prefix("checksum = ").is_some_and(|v| {
                quoted(v)
                    && v.len() > 2
                    && v[1..v.len() - 1]
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
            || t.strip_prefix("source = ")
                .is_some_and(|v| quoted(v) && CRATES_IO.contains(&&v[1..v.len() - 1]))
            || {
                let e = t.strip_suffix(',').unwrap_or(t);
                quoted(e)
                    && e.len() > 2
                    && e[1..e.len() - 1]
                        .bytes()
                        .all(|b| is_bare(b) || b"._+ ():/".contains(&b))
            };
        if !ok {
            return Err(format!(
                "line {}: Cargo.lock line not recognised or not registry-only",
                i + 1
            ));
        }
    }
    Ok(())
}

/// True when the manifest has a `[workspace]` table. A staged root without one could be
/// adopted as a member of a `Cargo.toml` planted in an ancestor directory, which would
/// bring that file's `[patch]` and `[replace]` tables into the build.
pub fn declares_workspace(text: &str) -> bool {
    text.lines().any(|l| {
        l.trim_matches([' ', '\t'])
            .strip_prefix("[workspace]")
            .is_some_and(|rest| {
                let rest = rest.trim_matches([' ', '\t']);
                rest.is_empty() || rest.starts_with('#')
            })
    })
}

/// Cargo, rustup and the file system treat names case-insensitively on macOS. A name
/// that differs only in case from another path would let a config file hide, so
/// two paths (or directory prefixes) that fold to the same name are refused.
pub fn case_collision<'a>(files: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for f in files {
        let mut prefix = String::new();
        for part in f.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            match seen.get(&prefix.to_ascii_lowercase()) {
                Some(prev) if *prev != prefix => return Some(prefix),
                Some(_) => {}
                None => {
                    seen.insert(prefix.to_ascii_lowercase(), prefix.clone());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rule_is_lexical_and_strict() {
        for (v, depth, ok) in [
            ("../b", 2, true),
            ("../../x", 2, true),
            ("../../../x", 2, false),
            ("crates/b", 0, true),
            ("..", 0, false),
            ("a/../b", 5, false),
            ("/abs", 3, false),
            ("~/x", 3, false),
            ("a\\b", 3, false),
            ("", 3, false),
        ] {
            assert_eq!(path_ok(v, depth), ok, "{v} at depth {depth}");
        }
    }

    #[test]
    fn manifest_rules() {
        let ok = "[package]\nname = \"a\"\n\n[dependencies]\nb = { path = \"../b\", version = \"1\" } # c\n";
        assert!(check_manifest(ok, 2).is_ok());
        assert!(check_manifest(ok, 1).is_ok());
        assert!(
            check_manifest(ok, 0).is_err(),
            "../b leaves the root from depth 0"
        );
        for bad in [
            "[dependencies]\nx = { git = \"u\" }\n",
            "[dependencies.x]\nregistry = \"r\"\n",
            "[replace]\n",
            "[patch.other]\n",
            "k = [\n]\n",
            "k.sub = 1\n",
            "k = 1.5\n",
        ] {
            assert!(check_manifest(bad, 1).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn workspace_header_detection() {
        assert!(declares_workspace("[package]\n  [workspace]  # c\n"));
        assert!(!declares_workspace(
            "[workspace.dependencies]\n# [workspace]\nx = \"[workspace]\"\n"
        ));
    }

    #[test]
    fn source_lock_and_toolchain_file() {
        let sha = "a".repeat(64);
        let lock = format!(
            "schema = 1\n[manager]\ntoolchain = \"1.2.3\"\ntoolchain_tree_sha256 = \"{sha}\"\n"
        );
        assert_eq!(parse_source_lock(&lock).unwrap(), ("1.2.3".into(), sha));
        assert!(parse_source_lock("schema = 1\n").is_err());
        assert_eq!(
            case_collision(["crates/a/x", "crates/A/y"]),
            Some("crates/A".into())
        );
        assert_eq!(case_collision(["a/x", "a/y", "b"]), None);
        assert!(check_toolchain_file("[toolchain]\nchannel = \"1.2.3\"\n", "1.2.3").is_ok());
        assert!(check_toolchain_file("[toolchain]\nchannel = \"1.2.4\"\n", "1.2.3").is_err());
        assert!(
            check_toolchain_file(
                "[toolchain]\nchannel = \"1.2.3\"\nprofile = \"minimal\"\n",
                "1.2.3"
            )
            .is_err()
        );
    }

    #[test]
    fn cargo_lock_is_registry_only() {
        let ok = "version = 4\n\n[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\n \"y\",\n]\n";
        assert!(check_cargo_lock(ok).is_ok());
        assert!(check_cargo_lock("source = \"git+https://example.invalid/x\"\n").is_err());
        assert!(check_cargo_lock("[[patch.unused]]\n").is_err());
    }
}
