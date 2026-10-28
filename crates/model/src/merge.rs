//! The co-owned JSON merges (docs/adapters.md: "claude-settings/v1", "codex-hooks/v1" and
//! "omlx-model-settings/v1"). Pure:
//! it takes the live bytes, the declared value and the handler entries the ledger records as
//! owned, and returns the new bytes. It opens no file.
//!
//! This code lives in `dfm-model`, which depends on `serde_json` (docs/state.md). The adapter
//! glue (registry entry, `Generator` impl) is in the CLI crate.
//!
//! The live document is the base. The declared `managed` values merge into it: an object merges
//! recursively (managed leaves win, foreign leaves survive), an array replaces, except at a
//! pointer listed in `append`, where the unique declared items are appended and the manager
//! records them as its own. Fragments (`settings.d`) merge over the declared values first. The
//! declared `seed` values are set only where the live file has nothing, at any depth, and the
//! declared handlers are added to `hooks.<event>` as owned entries. Every other key and every
//! foreign hook entry is carried over by value. A result equal to the live document as JSON keeps
//! the live bytes, so key order and spacing alone are never a change. Any other result is
//! re-serialized with sorted keys and a 2-space indent, so formatting is normalized.
//!
//! Nothing in an error message repeats a byte of the live file: a parse error names a category,
//! a line and a column only.

use crate::digest::sha256_hex;
use crate::refusal::{Code, Refusal};
use crate::state::{AppendedItem, CoOwned, OwnedEntry};
use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet};
use toml::Value as Toml;

/// The adapter id of the Claude settings merge.
pub const ADAPTER: &str = "claude-settings/v1";
/// The adapter id of the Codex hooks merge. `~/.codex/hooks.json` has the shape of the `hooks`
/// object of the Claude settings, so the same merge serves it with handlers only.
pub const CODEX_HOOKS: &str = "codex-hooks/v1";

/// The adapter id of the oMLX per-model settings merge (`{"version":1,"models":{...}}`), with
/// `managed` only. The application saves the file with mode 0644 and holds no credential in it.
pub const OMLX_MODEL_SETTINGS: &str = "omlx-model-settings/v1";

/// Whether `adapter` is a co-owned merge this module implements.
pub fn is_merge(adapter: &str) -> bool {
    matches!(adapter, ADAPTER | CODEX_HOOKS | OMLX_MODEL_SETTINGS)
}

/// The mode of a published merge file. A live file with a group or other bit outside it is
/// refused.
pub const MODE: u32 = 0o600;

/// The mode `adapter` publishes its file with: 0644 for the oMLX model settings, which the
/// application itself writes with that mode, and [`MODE`] for every other adapter.
pub fn mode_for(adapter: &str) -> u32 {
    if adapter == OMLX_MODEL_SETTINGS {
        0o644
    } else {
        MODE
    }
}

/// The most managed leaves, hook entries and appended items one merge output may record. The
/// ledger holds a salted hash for each, so the bound keeps the ledger inside its size limit.
pub const MAX_RECORDED: usize = 10_000;

/// The longest hook timeout the manager declares, in seconds.
const MAX_TIMEOUT: i64 = 3600;

/// One declared handler: a hook command the manager adds and may later retire.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Handler {
    id: String,
    event: String,
    /// The hook element the manager adds to `hooks.<event>`.
    entry: Json,
    /// SHA-256 of the canonical JSON of `entry`. Declared content, not live content.
    sha256: String,
}

/// The declared value of a `claude-settings/v1` resource, converted to JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desired {
    /// The declared values. `managed_sha256` covers these only.
    managed: Map<String, Json>,
    /// The declared values with the fragments merged over them. The merge and the salted
    /// managed-fields hash use this view.
    effective: Map<String, Json>,
    seed: Map<String, Json>,
    handlers: Vec<Handler>,
    /// JSON pointers (escaped) of the arrays whose unique items are appended, not replaced.
    append: BTreeSet<String>,
    /// The fragment directory (a HOME path) and the file name pattern, when declared.
    fragments: Option<(String, String)>,
    /// HOME as the machine spells it. Only used to recognize an existing hook that spells the
    /// same command with the absolute home path.
    home: Option<String>,
}

/// The result of a merge. `Debug` prints lengths only: the bytes can hold credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Merged {
    /// The new file content.
    pub bytes: Vec<u8>,
    /// Canonical JSON of the managed keys as declared.
    pub rendered_view: Vec<u8>,
    /// The leaves of the live and the rendered managed view by JSON pointer, as canonical JSON. A leaf
    /// is a value that is not an object. The conflict check compares leaf by leaf, so a changed
    /// key set (a dropped key or fragment) is not an edit.
    pub live_leaves: BTreeMap<String, Vec<u8>>,
    pub rendered_leaves: BTreeMap<String, Vec<u8>>,
    /// The handler entries the manager owns after this merge. The ledger records them.
    pub entries: Vec<OwnedEntry>,
    /// The array items the manager owns after this merge (`append`). The ledger records them.
    pub appended: Vec<AppendedItem>,
    /// Whether the merge changes the live document. When it does not, `bytes` are the live bytes.
    pub changed: bool,
}

impl std::fmt::Debug for Merged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Merged")
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field(
                "rendered_view",
                &format_args!("{} bytes", self.rendered_view.len()),
            )
            .field("entries", &self.entries)
            .field("appended", &self.appended)
            .field("changed", &self.changed)
            .finish()
    }
}

fn invalid(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::ManifestInvalid, detail)
}

fn conflict(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::Conflict, detail)
}

/// `1.0`, `1e0` and `1` are one value in JSON. A float with no fractional part that fits an
/// `i64` is spelled as that integer, so a live file that spells a number differently is not an
/// edit. Larger floats keep their spelling.
fn integral(v: &Json) -> Json {
    match v {
        Json::Number(n) => match n.as_f64() {
            Some(f)
                if n.is_f64()
                    && f.fract() == 0.0
                    && (-(2f64.powi(63))..2f64.powi(63)).contains(&f) =>
            {
                // In range and integral, so the conversion is exact.
                Json::from(f as i64)
            }
            _ => v.clone(),
        },
        Json::Array(a) => Json::Array(a.iter().map(integral).collect()),
        Json::Object(o) => Json::Object(o.iter().map(|(k, x)| (k.clone(), integral(x))).collect()),
        _ => v.clone(),
    }
}

fn canonical(v: &Json) -> Vec<u8> {
    // `serde_json::Map` is a sorted map here (no `preserve_order`), so this is canonical.
    serde_json::to_vec(&integral(v))
        .expect("a serde_json::Value always serializes: its keys are strings")
}

fn to_json(v: &Toml, at: &str) -> Result<Json, Refusal> {
    Ok(match v {
        Toml::String(s) => Json::String(s.clone()),
        Toml::Integer(i) => Json::from(*i),
        Toml::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .ok_or_else(|| {
                invalid(format!(
                    "{at} is not a finite number, which JSON cannot hold"
                ))
            })?,
        Toml::Boolean(b) => Json::Bool(*b),
        Toml::Datetime(_) => {
            return Err(invalid(format!(
                "{at} is a TOML date or time, which has no JSON form; write it as a string"
            )));
        }
        Toml::Array(a) => Json::Array(
            a.iter()
                .enumerate()
                .map(|(i, x)| to_json(x, &format!("{at}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        Toml::Table(t) => Json::Object(table(t, at)?),
    })
}

fn table(t: &toml::Table, at: &str) -> Result<Map<String, Json>, Refusal> {
    t.iter()
        .map(|(k, v)| Ok((k.clone(), to_json(v, &format!("{at}.{k}"))?)))
        .collect()
}

/// A shell word: plain when it only holds safe characters, otherwise single-quoted. One
/// exception: a word that starts with `${HOME}` keeps that reference live, in double quotes, so
/// the hook finds the user's home on each machine. Every other character is quoted literally.
fn shell_word(a: &str) -> String {
    if let Some(rest) = a.strip_prefix("${HOME}")
        && (rest.is_empty() || rest.starts_with('/'))
    {
        let mut out = String::from("\"${HOME}");
        for c in rest.chars() {
            if matches!(c, '\\' | '"' | '$' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        return out;
    }
    // Quoted although safe elsewhere: a leading `=` (zsh expands `=cmd`), a leading `%` (a job
    // spec in command position) and `NAME=value` (an environment assignment in command position).
    let assignment = a.split_once('=').is_some_and(|(n, _)| {
        n.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    let plain = !a.is_empty()
        && !a.starts_with(['=', '%'])
        && !assignment
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if plain {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

fn handler(id: &str, v: &Toml) -> Result<Handler, Refusal> {
    // The id becomes `OwnedEntry.id` in the ledger, which refuses an empty id or a control
    // character. Refuse it here, before any write.
    if id.is_empty() || id.chars().any(char::is_control) {
        return Err(invalid(
            "a handler id must be a non-empty name without control characters",
        ));
    }
    let at = format!("handlers.{id}");
    let t = v
        .as_table()
        .ok_or_else(|| invalid(format!("{at} must be a table")))?;
    let text = |k: &str| {
        t.get(k)
            .and_then(Toml::as_str)
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .ok_or_else(|| invalid(format!("{at}.{k} must be a non-empty string")))
    };
    let (event, kind) = (text("event")?, text("type")?);
    if kind != "command" {
        return Err(invalid(format!(
            "{at}.type is `{kind}`; a merge handler builds only `command` hooks. Fix: use type = \"command\", or add the hook by hand (the manager cannot reverse any other hook type safely)"
        )));
    }
    let argv: Vec<&str> = t
        .get("argv")
        .and_then(Toml::as_array)
        .and_then(|a| a.iter().map(Toml::as_str).collect::<Option<Vec<_>>>())
        .filter(|a| !a.is_empty() && a.iter().all(|s| !s.contains('\0')))
        .ok_or_else(|| invalid(format!("{at}.argv must be a non-empty array of strings")))?;
    let command = argv
        .iter()
        .map(|a| shell_word(a))
        .collect::<Vec<_>>()
        .join(" ");
    let mut hook = serde_json::json!({ "command": command, "type": "command" });
    if let Some(v) = t.get("timeout") {
        let secs = v
            .as_integer()
            .filter(|n| (1..=MAX_TIMEOUT).contains(n))
            .ok_or_else(|| {
                invalid(format!(
                    "{at}.timeout must be an integer from 1 to {MAX_TIMEOUT} (seconds)"
                ))
            })?;
        hook["timeout"] = Json::from(secs);
    }
    let mut entry = serde_json::json!({ "hooks": [hook] });
    if let Some(v) = t.get("matcher") {
        let m = v
            .as_str()
            .filter(|s| !s.chars().any(char::is_control))
            .ok_or_else(|| {
                invalid(format!(
                    "{at}.matcher must be a string without control characters (an empty string matches everything)"
                ))
            })?;
        entry["matcher"] = Json::String(m.into());
    }
    let sha256 = sha256_hex(&canonical(&entry));
    Ok(Handler {
        id: id.into(),
        event: event.into(),
        entry,
        sha256,
    })
}

/// A key the ledger cannot spell as a JSON pointer: empty, or with a control character.
fn unspellable(k: &str) -> bool {
    k.is_empty() || k.chars().any(char::is_control)
}

/// Whether any key at any object depth is [`unspellable`].
fn has_unspellable_key(m: &Map<String, Json>) -> bool {
    m.iter()
        .any(|(k, v)| unspellable(k) || matches!(v, Json::Object(o) if has_unspellable_key(o)))
}

/// A managed key the ledger cannot spell as a JSON pointer would drop out of the leaf-by-leaf
/// conflict check, so an edit of it would go unseen. Such a key is refused at the source.
fn managed_keys_usable(m: &Map<String, Json>, at: &str) -> Result<(), Refusal> {
    for (k, v) in m {
        if unspellable(k) {
            return Err(invalid(format!(
                "{at} holds a key that is empty or has a control character; use a plain key name"
            )));
        }
        if let Json::Object(o) = v {
            managed_keys_usable(o, &format!("{at}.{k}"))?;
        }
    }
    Ok(())
}

impl Desired {
    /// Converts the resolved resource value. Every section is optional here; the registry schema
    /// of each adapter says which it requires. `hooks` is reserved for handlers, so a managed or
    /// seed key named `hooks` is refused.
    pub fn from_value(value: &Toml) -> Result<Desired, Refusal> {
        let root = value
            .as_table()
            .ok_or_else(|| invalid("the claude-settings value must be a table"))?;
        let section = |name: &str| -> Result<Map<String, Json>, Refusal> {
            match root.get(name) {
                Some(Toml::Table(t)) => table(t, name),
                Some(_) => Err(invalid(format!("{name} must be a table"))),
                None => Ok(Map::new()),
            }
        };
        let (managed, seed) = (section("managed")?, section("seed")?);
        managed_keys_usable(&managed, "managed")?;
        for (name, m) in [("managed", &managed), ("seed", &seed)] {
            if m.contains_key("hooks") {
                return Err(invalid(format!(
                    "{name}.hooks is not allowed: hooks are declared as handlers so the manager can retire only its own entries"
                )));
            }
        }
        let mut handlers = Vec::new();
        if let Some(h) = root.get("handlers") {
            let h = h
                .as_table()
                .ok_or_else(|| invalid("handlers must be a table"))?;
            for (id, v) in h {
                handlers.push(handler(id, v)?);
            }
        }
        let mut append = BTreeSet::new();
        if let Some(a) = root.get("append") {
            let list = a
                .as_array()
                .ok_or_else(|| invalid("append must be an array of JSON pointers"))?;
            for p in list {
                let p = p.as_str().and_then(normal_pointer).ok_or_else(|| {
                    invalid("append must hold JSON pointers such as /sandbox/filesystem/allowWrite")
                })?;
                if p == "/hooks" || p.starts_with("/hooks/") {
                    return Err(invalid(
                        "append cannot name hooks: hooks are declared as handlers",
                    ));
                }
                if !matches!(pointer_get(&managed, &p), Some(Json::Array(_))) {
                    return Err(invalid(format!(
                        "append pointer {p} does not name an array in managed (a pointer is case-sensitive, and a mistyped one would turn append into replace). Fix: declare the array under managed or correct the pointer"
                    )));
                }
                append.insert(p);
            }
        }
        let text = |k: &str| match root.get(k) {
            None => Ok(None),
            Some(Toml::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(_) => Err(invalid(format!("{k} must be a non-empty string"))),
        };
        let fragments = match (text("fragments_dir")?, text("pattern")?) {
            (Some(d), Some(p)) => Some((d, p)),
            (None, None) => None,
            _ => {
                return Err(invalid(
                    "fragments_dir and pattern go together: declare both or neither",
                ));
            }
        };
        Ok(Desired {
            effective: managed.clone(),
            managed,
            seed,
            handlers,
            append,
            fragments,
            home: None,
        })
    }

    /// Sets HOME, so an existing hook that spells the home directory as an absolute path is
    /// recognized as the declared hook.
    pub fn with_home(mut self, home: &str) -> Desired {
        self.home = Some(home.trim_end_matches('/').to_string()).filter(|h| !h.is_empty());
        self
    }

    /// The fragment directory and the file name pattern, when the resource declares them.
    pub fn fragment_spec(&self) -> Option<(&str, &str)> {
        self.fragments
            .as_ref()
            .map(|(d, p)| (d.as_str(), p.as_str()))
    }

    /// The declared values with `fragments` merged over them, in the order given (the caller
    /// passes byte order of the names). Objects merge recursively and a later fragment wins; an
    /// array or a scalar replaces. A fragment that is not a JSON object, or that sets `hooks`,
    /// is `E_CONFLICT`. The message names the fragment and the error category, line and column,
    /// and repeats no content.
    pub fn with_fragments<'a>(
        &self,
        fragments: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<Desired, Refusal> {
        let mut out = self.clone();
        for (name, bytes) in fragments {
            let fix = "Fix: repair or remove the fragment; the manager writes nothing until every fragment is a JSON object";
            let obj = match serde_json::from_slice::<Json>(bytes) {
                Ok(Json::Object(m)) => m,
                Ok(_) => {
                    return Err(conflict(format!(
                        "fragment {name} is JSON but not an object. {fix}"
                    )));
                }
                Err(e) => {
                    return Err(conflict(format!(
                        "fragment {name} is not valid JSON ({:?} at line {}, column {}). {fix}",
                        e.classify(),
                        e.line(),
                        e.column()
                    )));
                }
            };
            if obj.contains_key("hooks") {
                return Err(conflict(format!(
                    "fragment {name} sets hooks. Fix: declare hooks as handlers in the module so the manager can retire only its own entries"
                )));
            }
            if has_unspellable_key(&obj) {
                return Err(conflict(format!(
                    "fragment {name} holds a key that is empty or has a control character, so an edit of it could not be detected. {fix}"
                )));
            }
            overlay(&mut out.effective, &obj, "", &out.append.clone());
        }
        Ok(out)
    }

    /// SHA-256 of the canonical JSON of the declared managed values. The module declares them,
    /// so the digest is not secret and may appear in the plan. Fragments are not part of it.
    pub fn managed_sha256(&self) -> String {
        sha256_hex(&canonical(&Json::Object(self.managed.clone())))
    }

    /// The number of declared handlers.
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }

    /// The setting paths the manager sets or seeds, sorted, never values. A path is the keys
    /// from the top of the file to a leaf (a value that is not an object), joined with dots.
    /// Fragments are included: their keys are module or user data, not live content.
    pub fn setting_names(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        leaf_paths(&self.effective, "", &mut names);
        leaf_paths(&self.seed, "", &mut names);
        names.into_iter().collect()
    }

    fn rendered_map(&self) -> Map<String, Json> {
        view_of(&self.effective, &self.effective, "", &self.append)
    }

    fn rendered_view(&self) -> Vec<u8> {
        canonical(&Json::Object(self.rendered_map()))
    }
}

fn leaf_paths(m: &Map<String, Json>, prefix: &str, out: &mut BTreeSet<String>) {
    for (k, v) in m {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Json::Object(o) if !o.is_empty() => leaf_paths(o, &path, out),
            _ => {
                out.insert(path);
            }
        }
    }
}

/// Deep merge of fragment objects: objects merge and everything else replaces. At an `append`
/// pointer two arrays concatenate with unique items, so a fragment adds to a declared list.
fn overlay(
    dst: &mut Map<String, Json>,
    src: &Map<String, Json>,
    at: &str,
    append: &BTreeSet<String>,
) {
    for (k, v) in src {
        let here = format!("{at}/{}", crate::resolve::pointer::escape(k));
        match (dst.get_mut(k), v) {
            (Some(Json::Object(d)), Json::Object(s)) => overlay(d, s, &here, append),
            (Some(Json::Array(d)), Json::Array(s)) if append.contains(&here) => {
                for item in s {
                    let sha = sha_of(item);
                    if !d.iter().any(|e| sha_of(e) == sha) {
                        d.push(item.clone());
                    }
                }
            }
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

/// A JSON pointer that names at least one key, in its canonical spelling. `None` when the text
/// is not a pointer (RFC 6901) or names the whole document.
pub(crate) fn normal_pointer(p: &str) -> Option<String> {
    let rest = p.strip_prefix('/')?;
    let tokens: Vec<String> = rest.split('/').map(unescape_token).collect::<Option<_>>()?;
    if tokens
        .iter()
        .any(|t| t.is_empty() || t.chars().any(char::is_control))
    {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|t| format!("/{}", crate::resolve::pointer::escape(t)))
            .collect(),
    )
}

fn unescape_token(t: &str) -> Option<String> {
    let mut out = String::new();
    let mut it = t.chars();
    while let Some(c) = it.next() {
        if c == '~' {
            match it.next()? {
                '0' => out.push('~'),
                '1' => out.push('/'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn pointer_tokens(p: &str) -> Vec<String> {
    p.strip_prefix('/')
        .map(|r| r.split('/').filter_map(unescape_token).collect())
        .unwrap_or_default()
}

/// Parses the live document. Absent is `{}`. Anything that is not a JSON object is refused with
/// a message that carries no content.
fn parse_live(live: Option<&[u8]>) -> Result<Map<String, Json>, Refusal> {
    let Some(bytes) = live else {
        return Ok(Map::new());
    };
    let fix = "Fix: repair or move the file; the manager writes nothing until it is a JSON object";
    match serde_json::from_slice::<Json>(bytes) {
        Ok(Json::Object(_)) if has_wide_integer(bytes) => Err(conflict(format!(
            "the live settings file holds an integer outside the 64-bit range, which would be rewritten as a float. {fix}"
        ))),
        Ok(Json::Object(m)) => Ok(m),
        Ok(_) => Err(conflict(format!(
            "the live settings file is JSON but not an object. {fix}"
        ))),
        Err(e) => Err(conflict(format!(
            "the live settings file is not valid JSON ({:?} at line {}, column {}). {fix}",
            e.classify(),
            e.line(),
            e.column()
        ))),
    }
}

/// Whether valid JSON text holds an integer literal that fits neither `i64` nor `u64`.
/// `serde_json` reads such a number as `f64` and writes it back in another spelling, so the
/// merge would change a value it does not own. Strings are skipped.
fn has_wide_integer(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E')
                {
                    i += 1;
                }
                let tok = &bytes[start..i];
                let integer = tok.iter().all(|b| b.is_ascii_digit() || *b == b'-');
                if integer
                    && let Ok(t) = std::str::from_utf8(tok)
                    && t.parse::<i64>().is_err()
                    && t.parse::<u64>().is_err()
                {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// The managed view of `doc`: the leaves of `managed` as `doc` holds them, at the same paths.
/// A foreign leaf beside a managed one is not part of it. The items of an `append` array are
/// left out, like hook entries: the manager restores a deleted item and does not call it an
/// edit. An object that holds nothing of ours is left out.
fn view_of(
    doc: &Map<String, Json>,
    managed: &Map<String, Json>,
    at: &str,
    append: &BTreeSet<String>,
) -> Map<String, Json> {
    let mut out = Map::new();
    for (k, mv) in managed {
        let here = format!("{at}/{}", crate::resolve::pointer::escape(k));
        if append.contains(&here) {
            continue;
        }
        let Some(lv) = doc.get(k) else { continue };
        match (mv, lv) {
            (Json::Object(mo), Json::Object(lo)) => {
                let sub = view_of(lo, mo, &here, append);
                if !sub.is_empty() {
                    out.insert(k.clone(), Json::Object(sub));
                }
            }
            (Json::Object(_), _) => {}
            _ => {
                out.insert(k.clone(), lv.clone());
            }
        }
    }
    out
}

/// The leaves of a managed view by JSON pointer.
fn leaves_of(view: &Map<String, Json>) -> BTreeMap<String, Vec<u8>> {
    fn walk(m: &Map<String, Json>, at: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        for (k, v) in m {
            // A key the ledger cannot spell as a JSON pointer is not compared leaf by leaf.
            if unspellable(k) {
                continue;
            }
            let here = format!("{at}/{}", crate::resolve::pointer::escape(k));
            match v {
                Json::Object(o) => walk(o, &here, out),
                leaf => {
                    out.insert(here, canonical(leaf));
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(view, "", &mut out);
    out
}

/// The salted content token of one fragment (`fragments:` precondition of a sensitive merge).
pub fn fragment_token(salt: &[u8], name: &str, content: &[u8]) -> String {
    let mut buf = name.as_bytes().to_vec();
    buf.push(0);
    buf.extend_from_slice(content);
    salted(salt, &buf)
}

/// The salted hash of one managed leaf, as the ledger records it (`coowned.leaves`).
pub fn leaf_hash(salt: &[u8], pointer: &str, value: &[u8]) -> String {
    let mut buf = pointer.as_bytes().to_vec();
    buf.push(0);
    buf.extend_from_slice(value);
    salted(salt, &buf)
}

/// The salted hashes of every managed leaf this merge declares, by JSON pointer.
pub fn leaf_hashes(salt: &[u8], m: &Merged) -> BTreeMap<String, String> {
    m.rendered_leaves
        .iter()
        .map(|(p, v)| (p.clone(), leaf_hash(salt, p, v)))
        .collect()
}

/// The bytes to publish and whether they change the file. A document equal to the live one as JSON
/// keeps the live bytes: an application that rewrites the file in its own key order changes no
/// value, so the merge is not planned again. A whole number counts as one value whether it is
/// spelled `1` or `1.0`, as in the leaf check.
fn output(
    live: Option<&[u8]>,
    before: Map<String, Json>,
    doc: Map<String, Json>,
) -> (Vec<u8>, bool) {
    let (before, doc) = (Json::Object(before), Json::Object(doc));
    match live {
        Some(l) if integral(&doc) == integral(&before) => (l.to_vec(), false),
        _ => (serialize(&doc), true),
    }
}

fn serialize(doc: &Json) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(doc)
        .expect("a serde_json::Value always serializes: its keys are strings");
    out.push(b'\n');
    out
}

/// `hooks.<event>` as a mutable array, or `None` when it is absent. A present value of another
/// shape is refused: the manager does not guess how to extend a foreign shape.
fn event_array<'a>(
    hooks: &'a mut Map<String, Json>,
    event: &str,
) -> Result<Option<&'a mut Vec<Json>>, Refusal> {
    match hooks.get_mut(event) {
        None => Ok(None),
        Some(Json::Array(a)) => Ok(Some(a)),
        Some(_) => Err(conflict(format!(
            "hooks.{event} in the live file is not an array. Fix: correct it by hand; the manager does not rewrite a foreign shape"
        ))),
    }
}

fn hooks_map(doc: &mut Map<String, Json>) -> Result<Option<&mut Map<String, Json>>, Refusal> {
    match doc.get_mut("hooks") {
        None => Ok(None),
        Some(Json::Object(m)) => Ok(Some(m)),
        Some(_) => Err(conflict(
            "hooks in the live file is not an object. Fix: correct it by hand; the manager does not rewrite a foreign shape",
        )),
    }
}

fn sha_of(v: &Json) -> String {
    sha256_hex(&canonical(v))
}

/// Removes every element of `hooks.<event>` whose canonical hash is `sha256`. Records the event
/// in `touched` when something was removed, so [`tidy`] can drop the key it emptied.
fn remove_owned(
    doc: &mut Map<String, Json>,
    event: &str,
    sha256: &str,
    touched: &mut BTreeSet<String>,
) -> Result<(), Refusal> {
    let Some(hooks) = hooks_map(doc)? else {
        return Ok(());
    };
    let Some(arr) = event_array(hooks, event)? else {
        return Ok(());
    };
    // One ownership record authorizes the removal of one element. An identical copy that the
    // user added is foreign configuration and stays.
    if let Some(i) = arr.iter().position(|e| sha_of(e) == sha256) {
        arr.remove(i);
        touched.insert(event.into());
    }
    Ok(())
}

/// Drops an event array that a removal emptied, and `hooks` when that left it empty.
fn tidy(doc: &mut Map<String, Json>, touched: &BTreeSet<String>) {
    let Some(Json::Object(hooks)) = doc.get_mut("hooks") else {
        return;
    };
    for ev in touched {
        if matches!(hooks.get(ev), Some(Json::Array(a)) if a.is_empty()) {
            hooks.remove(ev);
        }
    }
    if hooks.is_empty() && !touched.is_empty() {
        doc.remove("hooks");
    }
}

/// Removes the item whose canonical hash is `sha256` from the array at `tokens`. Returns true
/// when `map` became empty because of this removal, so the caller can drop the key it emptied.
/// A path of another shape is left alone.
fn remove_item(map: &mut Map<String, Json>, tokens: &[String], sha256: &str) -> bool {
    let Some((first, rest)) = tokens.split_first() else {
        return false;
    };
    let emptied = if rest.is_empty() {
        match map.get_mut(first) {
            // One record removes one item: an identical copy of the user's stays.
            Some(Json::Array(a)) => match a.iter().position(|e| sha_of(e) == sha256) {
                Some(i) => {
                    a.remove(i);
                    a.is_empty()
                }
                None => false,
            },
            _ => false,
        }
    } else {
        match map.get_mut(first) {
            Some(Json::Object(m)) => remove_item(m, rest, sha256),
            _ => false,
        }
    };
    if emptied {
        map.remove(first);
        return map.is_empty();
    }
    false
}

/// What one merge walk needs besides the documents.
struct Walk<'a> {
    desired: &'a Desired,
    prior: &'a [AppendedItem],
    takeover: bool,
    appended: Vec<AppendedItem>,
}

fn not_an_object(pointer: &str) -> Refusal {
    conflict(format!(
        "{pointer} in the live file is not an object, but the module sets keys below it. Fix: correct it by hand; the manager does not rewrite a foreign shape"
    ))
}

/// Merges the declared managed object into `live`. An object merges into an object, a leaf or an
/// array replaces, and an `append` array adds its unique items.
fn apply_managed(
    live: &mut Map<String, Json>,
    managed: &Map<String, Json>,
    path: &mut Vec<String>,
    w: &mut Walk<'_>,
) -> Result<(), Refusal> {
    for (k, mv) in managed {
        path.push(k.clone());
        let ptr: String = path
            .iter()
            .map(|t| format!("/{}", crate::resolve::pointer::escape(t)))
            .collect();
        if w.desired.append.contains(&ptr) {
            append_items(live, k, mv, &ptr, w)?;
        } else if let Json::Object(mo) = mv {
            match live.get_mut(k) {
                Some(Json::Object(lo)) => apply_managed(lo, mo, path, w)?,
                None | Some(Json::Null) => {
                    let mut fresh = Map::new();
                    apply_managed(&mut fresh, mo, path, w)?;
                    live.insert(k.clone(), Json::Object(fresh));
                }
                Some(_) => return Err(not_an_object(&ptr)),
            }
        } else {
            if matches!(live.get(k), Some(Json::Object(_))) {
                return Err(conflict(format!(
                    "{ptr} in the live file is a table, but the module sets a value there. Fix: correct it by hand; the manager does not discard a foreign table"
                )));
            }
            live.insert(k.clone(), mv.clone());
        }
        path.pop();
    }
    Ok(())
}

fn append_items(
    live: &mut Map<String, Json>,
    key: &str,
    declared: &Json,
    ptr: &str,
    w: &mut Walk<'_>,
) -> Result<(), Refusal> {
    let Json::Array(items) = declared else {
        return Err(invalid(format!(
            "{ptr} is listed in append, but its declared value is not an array"
        )));
    };
    let mut seen = BTreeSet::new();
    let unique: Vec<(&Json, String)> = items
        .iter()
        .map(|i| (i, sha_of(i)))
        .filter(|(_, h)| seen.insert(h.clone()))
        .collect();
    if unique.is_empty() {
        return Ok(());
    }
    let slot = live
        .entry(key.to_string())
        .or_insert_with(|| Json::Array(Vec::new()));
    if slot.is_null() {
        *slot = Json::Array(Vec::new());
    }
    let Json::Array(arr) = slot else {
        return Err(conflict(format!(
            "{ptr} in the live file is not an array. Fix: correct it by hand; the manager does not rewrite a foreign shape"
        )));
    };
    for (item, sha256) in unique {
        let present = arr.iter().any(|e| sha_of(e) == sha256);
        if !present {
            arr.push(item.clone());
        }
        // The manager owns an item it added now or recorded before and, at an approved
        // take-over, an identical item already there. Any other identical item is the user's.
        let was_owned = w
            .prior
            .iter()
            .any(|a| a.pointer == ptr && a.sha256 == sha256);
        if !present || was_owned || w.takeover {
            w.appended.push(AppendedItem {
                pointer: ptr.to_string(),
                sha256,
            });
        }
    }
    Ok(())
}

/// Sets the seed values where the live document has nothing, at any depth.
fn apply_seed(live: &mut Map<String, Json>, seed: &Map<String, Json>) {
    for (k, sv) in seed {
        match (live.get_mut(k), sv) {
            (None | Some(Json::Null), _) => {
                live.insert(k.clone(), sv.clone());
            }
            (Some(Json::Object(lo)), Json::Object(so)) => apply_seed(lo, so),
            _ => {}
        }
    }
}

/// Merges the declared value into the live document.
///
/// `owned` is what the ledger recorded after the last merge. An owned entry whose handler is
/// gone or changed is removed first (matched by hash, so a foreign entry is never touched), then
/// every declared handler is added unless an identical entry is already in place.
pub fn merge(
    live: Option<&[u8]>,
    desired: &Desired,
    owned: &[OwnedEntry],
) -> Result<Merged, Refusal> {
    merge_with(live, desired, owned, &[], false)
}

/// As [`merge`]. `appended` is what the ledger recorded as appended array items. With `takeover`
/// (the first write into an existing file with no ledger entry, approved with `--allow-adopt`),
/// an identical entry or item already in place is recorded as owned. The approval covers it:
/// after an interrupted first merge the file already holds the manager's own entries, and
/// nothing tells them from the user's copy.
pub fn merge_with(
    live: Option<&[u8]>,
    desired: &Desired,
    owned: &[OwnedEntry],
    appended: &[AppendedItem],
    takeover: bool,
) -> Result<Merged, Refusal> {
    let before = parse_live(live)?;
    let mut doc = before.clone();
    let live_map = view_of(&doc, &desired.effective, "", &desired.append);
    // An appended item the module no longer declares goes first, matched by hash.
    for a in appended {
        let declared = desired.append.contains(&a.pointer)
            && matches!(
                pointer_get(&desired.effective, &a.pointer),
                Some(Json::Array(items)) if items.iter().any(|i| sha_of(i) == a.sha256)
            );
        if !declared {
            remove_item(&mut doc, &pointer_tokens(&a.pointer), &a.sha256);
        }
    }
    let mut walk = Walk {
        desired,
        prior: appended,
        takeover,
        appended: Vec::new(),
    };
    apply_managed(&mut doc, &desired.effective, &mut Vec::new(), &mut walk)?;
    apply_seed(&mut doc, &desired.seed);
    let declared: BTreeMap<&str, &Handler> = desired
        .handlers
        .iter()
        .map(|h| (h.id.as_str(), h))
        .collect();
    let mut touched = BTreeSet::new();
    for o in owned {
        let kept = declared
            .get(o.id.as_str())
            .is_some_and(|h| h.event == o.key && h.sha256 == o.sha256);
        if !kept {
            remove_owned(&mut doc, &o.key, &o.sha256, &mut touched)?;
        }
    }
    tidy(&mut doc, &touched);
    let mut entries: Vec<OwnedEntry> = Vec::new();
    for h in &desired.handlers {
        let hooks = match doc
            .entry("hooks")
            .or_insert_with(|| Json::Object(Map::new()))
        {
            Json::Object(m) => m,
            _ => {
                return Err(conflict(
                    "hooks in the live file is not an object. Fix: correct it by hand; the manager does not rewrite a foreign shape",
                ));
            }
        };
        let (added, exact) = match event_array(hooks, &h.event)? {
            Some(arr) => {
                // An exact copy is the manager's to own. An equivalent hook (another spelling of
                // the same command) is the user's: it is not duplicated and not recorded, so a
                // retire can never remove a group that holds foreign hooks too.
                let exact = arr.iter().any(|e| sha_of(e) == h.sha256);
                let added = !exact && !hook_present(arr, &h.entry, desired.home.as_deref());
                if added {
                    arr.push(h.entry.clone());
                }
                (added, exact)
            }
            None => {
                hooks.insert(h.event.clone(), Json::Array(vec![h.entry.clone()]));
                (true, false)
            }
        };
        // The manager owns an entry it added now or recorded as its own before, and, at an
        // approved take-over, an identical entry already there. Otherwise an identical entry
        // that is not recorded belongs to the user: a later retire must never delete it.
        let was_owned = owned
            .iter()
            .any(|o| o.id == h.id && o.key == h.event && o.sha256 == h.sha256);
        // One record per (event, entry hash): handlers with the same content share one live copy,
        // and a repeated record would let a retire remove a user's identical copy as well.
        let recorded = entries
            .iter()
            .any(|e| e.key == h.event && e.sha256 == h.sha256);
        if (added || exact && (was_owned || takeover)) && !recorded {
            entries.push(OwnedEntry {
                id: h.id.clone(),
                key: h.event.clone(),
                sha256: h.sha256.clone(),
            });
        }
    }
    let rendered_leaves = leaves_of(&desired.rendered_map());
    if rendered_leaves.len() + entries.len() + walk.appended.len() > MAX_RECORDED {
        return Err(Refusal::new(
            Code::LimitExceeded,
            format!(
                "the merge would record more than {MAX_RECORDED} managed leaves, hooks and appended items in the ledger. Fix: declare fewer keys, or shrink the fragments"
            ),
        ));
    }
    let (bytes, changed) = output(live, before, doc);
    Ok(Merged {
        bytes,
        rendered_view: desired.rendered_view(),
        live_leaves: leaves_of(&live_map),
        rendered_leaves,
        entries,
        appended: walk.appended,
        changed,
    })
}

/// How a character of a shell word was written. Only text the shell would expand can be a home
/// spelling: `~` when bare, `$HOME` and `${HOME}` when bare or in double quotes. A character in
/// single quotes, or escaped with a backslash, is literal text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lead {
    Bare,
    Double,
    Literal,
}

/// One character with how it was written and the quote span it sits in. A span changes at every
/// quote that opens or closes, so `"$"HOME` and `$HO"ME"` are not one token.
type Ch = (char, Lead, u32);

/// Splits a shell command into words the way a POSIX shell reads quotes (single, double and
/// backslash), without any expansion. `None` when a quote is not closed.
fn shell_words(s: &str) -> Option<Vec<Vec<Ch>>> {
    let (mut words, mut cur, mut open, mut span) = (Vec::new(), Vec::<Ch>::new(), false, 0u32);
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                open = true;
                span += 1;
                loop {
                    match it.next()? {
                        '\'' => break,
                        x => cur.push((x, Lead::Literal, span)),
                    }
                }
                span += 1;
            }
            '"' => {
                open = true;
                span += 1;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => {
                            let n = it.next()?;
                            if matches!(n, '\\' | '"' | '$' | '`') {
                                cur.push((n, Lead::Literal, span));
                            } else {
                                cur.push(('\\', Lead::Literal, span));
                                cur.push((n, Lead::Double, span));
                            }
                        }
                        x => cur.push((x, Lead::Double, span)),
                    }
                }
                span += 1;
            }
            '\\' => {
                open = true;
                span += 1;
                cur.push((it.next()?, Lead::Literal, span));
                span += 1;
            }
            // POSIX sh splits on blank and newline only, so a no-break space stays in its word.
            ' ' | '\t' | '\n' => {
                if open {
                    words.push(std::mem::take(&mut cur));
                    open = false;
                }
            }
            c => {
                open = true;
                cur.push((c, Lead::Bare, span));
            }
        }
    }
    if open {
        words.push(cur);
    }
    Some(words)
}

/// One word with its home spelling made canonical. The leading `~`, `$HOME` or `${HOME}` becomes
/// the home marker only when the shell would expand it: the whole token is written as one unit,
/// bare (or double-quoted for the variables), and a `~` is followed by a bare `/` or the end. A
/// partly quoted token (`"$"HOME`, `$HO"ME"`, `~"/x"`) is literal text. The absolute home path is
/// literal text in any quoting, so it always matches.
fn canon_word(w: &[Ch], home: Option<&str>) -> String {
    let text: String = w.iter().map(|c| c.0).collect();
    let unit = |n: usize, lead: Lead| {
        w.len() >= n
            && w[..n]
                .iter()
                .all(|c| c.1 == lead && c.2 == w.first().map_or(0, |f| f.2))
    };
    let at_end = |n: usize, bare_slash: bool| match w.get(n) {
        None => true,
        Some(c) => c.0 == '/' && (!bare_slash || c.1 == Lead::Bare),
    };
    let marker = |n: usize| {
        let rest: String = w[n..].iter().map(|c| c.0).collect();
        format!("\u{1}HOME{rest}")
    };
    if unit(1, Lead::Bare) && w[0].0 == '~' && at_end(1, true) {
        return marker(1);
    }
    for (name, n) in [("$HOME", 5), ("${HOME}", 7)] {
        for lead in [Lead::Bare, Lead::Double] {
            if text.starts_with(name) && unit(n, lead) && at_end(n, false) {
                return marker(n);
            }
        }
    }
    if let Some(h) = home
        && let Some(rest) = text.strip_prefix(h)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("\u{1}HOME{rest}");
    }
    text
}

/// The identity of a hook command: its words, read as a shell reads them, with an expandable home
/// spelling made canonical. Quoting that does not change the words, and `~/`, `$HOME/`,
/// `${HOME}/` and the absolute home path at the start of a word, do not change it. A `~` or
/// `$HOME` that is single-quoted, escaped or only partly quoted is literal text and does. A
/// command with an unclosed quote is its own identity.
fn command_identity(cmd: &str, home: Option<&str>) -> String {
    match shell_words(cmd) {
        Some(w) => w
            .iter()
            .map(|x| canon_word(x, home))
            .collect::<Vec<_>>()
            .join("\0"),
        None => format!("raw\0{cmd}"),
    }
}

/// Whether `arr` (the groups of one event) already runs the declared hook: a group with the same
/// matcher that holds a `command` hook of the same identity. The timeout does not matter, and
/// neither does what else the group holds, so a group the nix merge wrote with two hooks counts.
fn hook_present(arr: &[Json], entry: &Json, home: Option<&str>) -> bool {
    let Some(want) = entry["hooks"][0]["command"]
        .as_str()
        .map(|c| command_identity(c, home))
    else {
        return false;
    };
    arr.iter().any(|g| {
        g.get("matcher") == entry.get("matcher")
            && g["hooks"].as_array().is_some_and(|hs| {
                hs.iter().any(|h| {
                    h["type"] == "command"
                        && h["command"]
                            .as_str()
                            .is_some_and(|c| command_identity(c, home) == want)
                })
            })
    })
}

fn pointer_get<'a>(m: &'a Map<String, Json>, pointer: &str) -> Option<&'a Json> {
    let tokens = pointer_tokens(pointer);
    let (last, parents) = tokens.split_last()?;
    let mut cur = m;
    for t in parents {
        cur = cur.get(t)?.as_object()?;
    }
    cur.get(last)
}

/// Removes only the handler entries and appended items the ledger recorded as owned. Managed
/// values, seeded values and every foreign key, hook and array item stay. Returns the new bytes
/// and whether they change the file (the live bytes when nothing was removed).
pub fn retire(
    live: &[u8],
    owned: &[OwnedEntry],
    appended: &[AppendedItem],
) -> Result<(Vec<u8>, bool), Refusal> {
    let before = parse_live(Some(live))?;
    let mut doc = before.clone();
    let mut touched = BTreeSet::new();
    for o in owned {
        remove_owned(&mut doc, &o.key, &o.sha256, &mut touched)?;
    }
    tidy(&mut doc, &touched);
    for a in appended {
        remove_item(&mut doc, &pointer_tokens(&a.pointer), &a.sha256);
    }
    Ok(output(Some(live), before, doc))
}

/// The text for a lost or unusable salt while the ledger records a salted hash. It is a hard
/// state error: no command recovers it.
pub const SALT_LOST: &str = "state/salt is missing or unusable, but the ledger records a salted hash of managed fields. This is a hard state error, so the foreign-edit check cannot run. Fix: restore state/salt from a copy (32 bytes, mode 0600). If no copy exists, edit state/ledger.json by hand and delete the `coowned` object (the key under resources[].owned_outputs[]) of each co-owned output; the next apply then needs --allow-adopt for that file";

/// Whether a managed field was changed outside the manager since the last write. The live
/// managed view must differ from the salted hash the ledger holds, and must not already equal
/// the declared values (then nothing is overwritten). Plan and apply both decide through this.
pub fn foreign_edit(salt: &[u8], recorded: &CoOwned, m: &Merged) -> bool {
    // Leaf by leaf, over the leaves both the ledger and the module name now. A leaf the module
    // dropped, renamed or added is not an edit: the declared key set changed, not the file. The
    // whole-view hash is not consulted here, because it covers the old key set. A ledger written
    // before DFM-48 has no leaf hashes, so its first write has nothing to compare and records them.
    recorded.leaves.iter().any(|(path, hash)| {
        let Some(declared) = m.rendered_leaves.get(path) else {
            return false;
        };
        match m.live_leaves.get(path) {
            None => true,
            Some(live) => live != declared && leaf_hash(salt, path, live) != *hash,
        }
    })
}

/// The shared conflict check for plan and apply. `live_present` is false when the file is
/// absent: there is nothing to compare, and the merge creates the file and records a new salted
/// hash. A recorded hash needs a usable salt. Messages carry no live content.
pub fn check_foreign(
    live_present: bool,
    recorded: Option<&CoOwned>,
    salt: Option<&[u8]>,
    m: &Merged,
) -> Result<(), Refusal> {
    let Some(c) = recorded else {
        return Ok(());
    };
    let Some(salt) = salt else {
        return Err(Refusal::new(Code::StateInvalid, SALT_LOST));
    };
    if live_present && foreign_edit(salt, c, m) {
        return Err(conflict(
            "a managed field of the live settings file was changed outside the manager since the last apply; nothing was changed. Fix: restore the declared value in the file, or set the same value in the module so the manager adopts your edit",
        ));
    }
    Ok(())
}

/// What a merge would do to the live file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Write the file, or record ownership of one that is already correct.
    Write,
    /// The result equals the live document as JSON and the ledger records the same hash and
    /// entries.
    Current,
}

/// Merges and checks in one step: the one function plan-time facts use. Apply calls [`merge`]
/// through its generator and then [`check_foreign`], the same check.
pub fn classify(
    live: Option<&[u8]>,
    desired: &Desired,
    recorded: Option<&CoOwned>,
    salt: Option<&[u8]>,
) -> Result<(Merged, Outcome), Refusal> {
    let owned = recorded.map(|c| c.entries.as_slice()).unwrap_or_default();
    let appended = recorded.map(|c| c.appended.as_slice()).unwrap_or_default();
    let merged = merge_with(live, desired, owned, appended, false)?;
    check_foreign(live.is_some(), recorded, salt, &merged)?;
    let current = live.is_some()
        && !merged.changed
        && recorded.zip(salt).is_some_and(|(c, salt)| {
            salted(salt, &merged.rendered_view) == c.managed_hash
                && c.leaves == leaf_hashes(salt, &merged)
                && c.entries == merged.entries
                && c.appended == merged.appended
        });
    Ok((
        merged,
        if current {
            Outcome::Current
        } else {
            Outcome::Write
        },
    ))
}

/// The salted hash of a managed view. The salt is a per-machine secret in `state/salt`. The
/// result may be stored in `ledger.json` and nowhere else (docs/state.md).
pub fn salted(salt: &[u8], view: &[u8]) -> String {
    let mut buf = Vec::with_capacity(salt.len() + view.len() + 32);
    buf.extend_from_slice(salt);
    buf.extend_from_slice(b"\0dfm-managed-fields/v1\0");
    buf.extend_from_slice(view);
    sha256_hex(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(src: &str) -> Desired {
        Desired::from_value(&toml::from_str::<Toml>(src).unwrap()).unwrap()
    }

    const BASE: &str = r#"
[managed]
outputStyle = "STE"
[seed]
model = "example-model"
[handlers.audit]
event = "PreToolUse"
type = "command"
argv = ["audit", "--quiet", "two words"]
"#;

    fn json(b: &[u8]) -> Json {
        serde_json::from_slice(b).unwrap()
    }

    #[test]
    fn foreign_keys_and_hooks_survive_and_managed_and_seed_follow_the_rules() {
        let live = br#"{"model":"mine","outputStyle":"other","secretToken":"planted-token","hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"foreign"}]}],"Stop":[]}}"#;
        let m = merge(Some(live), &desired(BASE), &[]).unwrap();
        let out = json(&m.bytes);
        assert_eq!(out["outputStyle"], "STE", "managed overrides");
        assert_eq!(out["model"], "mine", "seed only when absent");
        assert_eq!(out["secretToken"], "planted-token");
        assert_eq!(out["hooks"]["Stop"], serde_json::json!([]));
        let pre = out["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "foreign");
        assert_eq!(pre[1]["hooks"][0]["command"], "audit --quiet 'two words'");
        assert!(m.changed);
        assert_eq!(m.entries.len(), 1);
    }

    #[test]
    fn an_absent_file_is_an_empty_object_and_seed_is_written() {
        let m = merge(None, &desired(BASE), &[]).unwrap();
        let out = json(&m.bytes);
        assert_eq!(out["model"], "example-model");
        assert!(m.bytes.ends_with(b"\n"));
        assert!(
            String::from_utf8(m.bytes)
                .unwrap()
                .contains("\n  \"model\"")
        );
    }

    #[test]
    fn an_integer_outside_64_bits_blocks_the_merge_and_a_string_with_digits_does_not() {
        for wide in ["12345678901234567890123", "-9223372036854775809"] {
            let live = format!(r#"{{"mine":{wide}}}"#);
            let e = merge(Some(live.as_bytes()), &desired(BASE), &[]).unwrap_err();
            assert_eq!(e.code, Code::Conflict, "{wide}");
            assert!(e.detail.contains("64-bit"), "{}", e.detail);
        }
        let ok = br#"{"a":18446744073709551615,"b":-9223372036854775808,"c":1.5e300,"d":"12345678901234567890123 \" 99999999999999999999999"}"#;
        assert!(merge(Some(ok), &desired(BASE), &[]).is_ok());
    }

    #[test]
    fn a_second_merge_of_its_own_output_changes_nothing() {
        let first = merge(None, &desired(BASE), &[]).unwrap();
        let second = merge(Some(&first.bytes), &desired(BASE), &first.entries).unwrap();
        assert!(!second.changed);
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn an_explicit_false_disable_is_written_as_declared() {
        let d = desired("[managed]\nenableAll = false\n");
        let m = merge(Some(br#"{"enableAll":true}"#), &d, &[]).unwrap();
        assert_eq!(json(&m.bytes)["enableAll"], false);
        assert_eq!(m.live_leaves["/enableAll"], b"true");
        assert_eq!(m.rendered_view, br#"{"enableAll":false}"#);
    }

    #[test]
    fn retire_removes_only_owned_entries_and_leaves_values_and_foreign_hooks() {
        let live =
            br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"foreign"}]}]}}"#;
        let m = merge(Some(live), &desired(BASE), &[]).unwrap();
        let (bytes, changed) = retire(&m.bytes, &m.entries, &m.appended).unwrap();
        assert!(changed);
        let out = json(&bytes);
        assert_eq!(out["outputStyle"], "STE", "managed values stay");
        assert_eq!(out["model"], "example-model", "seeded values stay");
        let pre = out["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], "foreign");
    }

    #[test]
    fn retire_drops_the_event_and_hooks_it_emptied() {
        let m = merge(None, &desired(BASE), &[]).unwrap();
        let (bytes, _) = retire(&m.bytes, &m.entries, &m.appended).unwrap();
        assert!(json(&bytes).get("hooks").is_none());
    }

    #[test]
    fn an_identical_entry_in_place_is_owned_only_at_an_approved_take_over() {
        let first = merge(None, &desired(BASE), &[]).unwrap();
        let plain = merge(Some(&first.bytes), &desired(BASE), &[]).unwrap();
        assert!(plain.entries.is_empty(), "the user's copy stays the user's");
        let taken = merge_with(Some(&first.bytes), &desired(BASE), &[], &[], true).unwrap();
        assert_eq!(taken.entries, first.entries);
    }

    #[test]
    fn handlers_with_the_same_content_share_one_record_and_retire_keeps_a_user_copy() {
        let two = format!(
            "{BASE}[handlers.audit2]\nevent = \"PreToolUse\"\ntype = \"command\"\nargv = [\"audit\", \"--quiet\", \"two words\"]\n"
        );
        let first = merge(None, &desired(BASE), &[]).unwrap();
        let taken = merge_with(Some(&first.bytes), &desired(&two), &[], &[], true).unwrap();
        assert_eq!(taken.entries.len(), 1, "{:?}", taken.entries);
        // The user later adds an identical second copy: retire removes one element only.
        let mut live = json(&taken.bytes);
        let copy = live["hooks"]["PreToolUse"][0].clone();
        live["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(copy);
        let (bytes, _) = retire(&serde_json::to_vec(&live).unwrap(), &taken.entries, &[]).unwrap();
        assert_eq!(
            json(&bytes)["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_handler_removed_from_the_declaration_is_retired_by_the_next_merge() {
        let first = merge(None, &desired(BASE), &[]).unwrap();
        let none = desired("[managed]\noutputStyle = \"STE\"\n[handlers]\n");
        let second = merge(Some(&first.bytes), &none, &first.entries).unwrap();
        assert!(json(&second.bytes).get("hooks").is_none());
        assert!(second.entries.is_empty());
    }

    #[test]
    fn a_changed_handler_replaces_the_old_entry_and_keeps_foreign_ones() {
        let first = merge(
            Some(
                br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"foreign"}]}]}}"#,
            ),
            &desired(BASE),
            &[],
        )
        .unwrap();
        let changed = BASE.replace("--quiet", "--loud");
        let second = merge(Some(&first.bytes), &desired(&changed), &first.entries).unwrap();
        let out = json(&second.bytes);
        let pre = out["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "foreign");
        assert_eq!(pre[1]["hooks"][0]["command"], "audit --loud 'two words'");
    }

    #[test]
    fn malformed_json_is_refused_without_echoing_any_content() {
        for live in [
            &b"{\"token\": \"planted-token\" oops"[..],
            b"[1,2]",
            b"",
            b"null",
        ] {
            let e = merge(Some(live), &desired(BASE), &[]).unwrap_err();
            assert_eq!(e.code, Code::Conflict);
            assert!(!e.detail.contains("planted-token"), "{}", e.detail);
        }
    }

    #[test]
    fn a_foreign_shape_under_hooks_is_refused_not_rewritten() {
        let e = merge(Some(br#"{"hooks":[]}"#), &desired(BASE), &[]).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        let e = merge(Some(br#"{"hooks":{"PreToolUse":{}}}"#), &desired(BASE), &[]).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
    }

    #[test]
    fn unsupported_declarations_are_explicit_refusals_with_fix_text() {
        let bad =
            |src: &str| Desired::from_value(&toml::from_str::<Toml>(src).unwrap()).unwrap_err();
        let e = bad("[managed]\n[handlers.x]\nevent=\"Stop\"\ntype=\"prompt\"\nargv=[\"a\"]\n");
        assert!(e.detail.contains("Fix:"), "{}", e.detail);
        assert!(bad("[managed]\nhooks = {}\n").detail.contains("hooks"));
        assert!(
            bad("[managed]\nwhen = 1979-05-27\n")
                .detail
                .contains("date")
        );
        assert!(bad("[managed]\nx = nan\n").detail.contains("finite"));
    }

    #[test]
    fn a_home_reference_stays_live_and_everything_else_is_quoted_literally() {
        assert_eq!(shell_word("${HOME}/hooks/x y"), "\"${HOME}/hooks/x y\"");
        assert_eq!(shell_word("${HOME}/a$b\"c"), "\"${HOME}/a\\$b\\\"c\"");
        assert_eq!(shell_word("${HOMEX}/a"), "'${HOMEX}/a'");
        assert_eq!(shell_word("$HOME/a"), "'$HOME/a'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word(""), "''");
    }

    #[test]
    fn output_keys_are_sorted_whatever_the_order_in_the_live_file() {
        let live =
            br#"{"zeta":1,"alpha":{"y":1,"b":2},"model":"m","hooks":{"Stop":[],"PreToolUse":[]}}"#;
        let m = merge(Some(live), &desired(BASE), &[]).unwrap();
        let t = String::from_utf8(m.bytes).unwrap();
        let at = |k: &str| t.find(&format!("\n  \"{k}\"")).unwrap();
        assert!(
            at("alpha") < at("hooks")
                && at("hooks") < at("model")
                && at("model") < at("outputStyle")
        );
        assert!(at("outputStyle") < at("zeta"));
        assert!(
            t.find("\"b\"").unwrap() < t.find("\"y\"").unwrap(),
            "nested keys too"
        );
        assert!(t.find("\"PreToolUse\"").unwrap() < t.find("\"Stop\"").unwrap());
    }

    /// `v` as JSON text with every object's keys in reverse order and no spaces, the way an
    /// application with its own key order might write it.
    fn reversed(v: &Json) -> String {
        match v {
            Json::Object(m) => {
                let items: Vec<_> = m
                    .iter()
                    .rev()
                    .map(|(k, v)| format!("{}:{}", Json::String(k.clone()), reversed(v)))
                    .collect();
                format!("{{{}}}", items.join(","))
            }
            Json::Array(a) => {
                let items: Vec<_> = a.iter().map(reversed).collect();
                format!("[{}]", items.join(","))
            }
            other => other.to_string(),
        }
    }

    #[test]
    fn a_live_file_equal_as_json_keeps_its_bytes_and_is_current() {
        let d = desired(BASE);
        let salt = [9u8; 32];
        let first = merge(None, &d, &[]).unwrap();
        let rec = CoOwned {
            managed_hash: salted(&salt, &first.rendered_view),
            entries: first.entries.clone(),
            appended: first.appended.clone(),
            leaves: leaf_hashes(&salt, &first),
        };
        let mut doc = json(&first.bytes);
        doc["foreign"] = Json::from("kept");
        let live = reversed(&doc).into_bytes();
        let (m, o) = classify(Some(&live), &d, Some(&rec), Some(&salt)).unwrap();
        assert_eq!((o, m.changed), (Outcome::Current, false));
        assert_eq!(
            m.bytes, live,
            "the live bytes stay, key order and spacing included"
        );
        // A missing managed value is a change: the output is normalized again.
        doc.as_object_mut().unwrap().remove("outputStyle");
        let m = merge(Some(reversed(&doc).as_bytes()), &d, &first.entries).unwrap();
        assert!(m.changed);
        assert!(
            String::from_utf8(m.bytes)
                .unwrap()
                .starts_with("{\n  \"foreign\": \"kept\",\n")
        );
        // `1` and `1.0` are one value: a whole number the application respells is no change.
        let float = desired("[managed]\nratio = 1.0\n");
        let spelled = br#"{"ratio":1}"#;
        let m = merge(Some(spelled), &float, &[]).unwrap();
        assert_eq!((m.changed, m.bytes.as_slice()), (false, &spelled[..]));
        // A retire that removes nothing keeps the live bytes too.
        assert_eq!(retire(&live, &[], &[]).unwrap(), (live.clone(), false));
        let (bytes, changed) = retire(&live, &first.entries, &first.appended).unwrap();
        assert!(changed);
        let out = json(&bytes);
        assert!(out.get("hooks").is_none(), "the owned hook goes: {out}");
        assert_eq!(
            (&out["foreign"], &out["outputStyle"]),
            (&Json::from("kept"), &Json::from("STE"))
        );
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .starts_with("{\n  \"foreign\": \"kept\",\n"),
            "a changed result is normalized"
        );
        // A duplicate key keeps its last value. With no change both copies stay in the bytes; a
        // change writes one.
        let plain = desired("[managed]\noutputStyle = \"STE\"\n");
        let dup = br#"{"outputStyle":"other","outputStyle":"STE"}"#;
        let m = merge(Some(dup), &plain, &[]).unwrap();
        assert_eq!((m.changed, m.bytes.as_slice()), (false, &dup[..]));
        let dup = br#"{"outputStyle":"STE","outputStyle":"other"}"#;
        let m = merge(Some(dup), &plain, &[]).unwrap();
        assert!(m.changed);
        assert_eq!(m.bytes, b"{\n  \"outputStyle\": \"STE\"\n}\n");
    }

    #[test]
    fn a_word_that_starts_with_equals_is_quoted() {
        assert_eq!(shell_word("=cmd"), "'=cmd'");
        assert_eq!(shell_word("FOO=bar"), "'FOO=bar'");
        assert_eq!(shell_word("_x1=y"), "'_x1=y'");
        assert_eq!(shell_word("%1"), "'%1'");
        assert_eq!(shell_word("--flag=1"), "--flag=1", "not an assignment name");
        assert_eq!(shell_word("a%b"), "a%b");
    }

    #[test]
    fn an_identical_hook_that_was_already_there_is_not_recorded_as_owned() {
        let live = br#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"audit --quiet 'two words'","type":"command"}]}]}}"#;
        let first = merge(Some(live), &desired(BASE), &[]).unwrap();
        assert!(first.entries.is_empty(), "{:?}", first.entries);
        let (bytes, _) = retire(&first.bytes, &first.entries, &first.appended).unwrap();
        assert_eq!(
            json(&bytes)["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // Once the manager added an entry, it stays owned on later merges.
        let added = merge(None, &desired(BASE), &[]).unwrap();
        assert_eq!(added.entries.len(), 1);
        let again = merge(Some(&added.bytes), &desired(BASE), &added.entries).unwrap();
        assert_eq!(again.entries, added.entries);
    }

    #[test]
    fn the_conflict_check_is_shared_and_skips_an_absent_file() {
        let d = desired(BASE);
        let salt = [9u8; 32];
        let first = merge(None, &d, &[]).unwrap();
        let rec = CoOwned {
            managed_hash: salted(&salt, &first.rendered_view),
            entries: first.entries.clone(),
            appended: first.appended.clone(),
            leaves: leaf_hashes(&salt, &first),
        };
        // Unchanged live file: current.
        let (_, o) = classify(Some(&first.bytes), &d, Some(&rec), Some(&salt)).unwrap();
        assert_eq!(o, Outcome::Current);
        // A hand edit of a managed field is a conflict, and a salt-less check is a hard error.
        let edited = br#"{"outputStyle":"mine"}"#;
        let e = classify(Some(edited), &d, Some(&rec), Some(&salt)).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert!(!e.detail.contains("mine"));
        let e = classify(Some(&first.bytes), &d, Some(&rec), None).unwrap_err();
        assert_eq!(e.code, Code::StateInvalid);
        // An absent file has nothing to compare: it is created again.
        let (m, o) = classify(None, &d, Some(&rec), Some(&salt)).unwrap();
        assert_eq!((o, m.changed), (Outcome::Write, true));
        // A live value already equal to the declared one is no conflict.
        let stale = CoOwned {
            managed_hash: "0".repeat(64),
            ..rec
        };
        assert!(classify(Some(&first.bytes), &d, Some(&stale), Some(&salt)).is_ok());
    }

    #[test]
    fn managed_sha256_covers_the_declared_values_only() {
        let a = desired(BASE);
        let b = desired(&BASE.replace("example-model", "another"));
        assert_eq!(
            a.managed_sha256(),
            b.managed_sha256(),
            "seed is not managed"
        );
        let c = desired(&BASE.replace("STE", "plain"));
        assert_ne!(a.managed_sha256(), c.managed_sha256());
    }

    #[test]
    fn the_salted_hash_depends_on_the_salt() {
        assert_ne!(salted(b"a", b"{}"), salted(b"b", b"{}"));
        assert_eq!(salted(b"a", b"{}"), salted(b"a", b"{}"));
    }

    const NESTED: &str = r#"
append = ["/sandbox/filesystem/allowWrite"]
[managed.env]
MANAGED = "1"
[managed.enabledPlugins]
"a@m" = true
[managed.sandbox.filesystem]
allowWrite = ["/m/one", "/m/two", "/m/one"]
[seed]
model = "opusplan"
[seed.permissions]
defaultMode = "auto"
"#;

    #[test]
    fn objects_merge_recursively_and_foreign_leaves_survive_at_any_depth() {
        let live = br#"{"env":{"MINE":"x","MANAGED":"0"},"enabledPlugins":{"user@m":true,"a@m":false},"permissions":{"allow":["Bash"]}}"#;
        let m = merge(Some(live), &desired(NESTED), &[]).unwrap();
        let out = json(&m.bytes);
        assert_eq!(out["env"], serde_json::json!({"MINE":"x","MANAGED":"1"}));
        assert_eq!(
            out["enabledPlugins"],
            serde_json::json!({"user@m":true,"a@m":true}),
            "a plugin the user enabled survives, a managed leaf wins"
        );
        assert_eq!(out["permissions"]["allow"], serde_json::json!(["Bash"]));
        assert_eq!(out["permissions"]["defaultMode"], "auto", "seed at depth");
        assert_eq!(out["model"], "opusplan");
    }

    #[test]
    fn seeds_never_overwrite_a_live_value_at_any_depth() {
        let live = br#"{"model":"haiku","permissions":{"defaultMode":"plan"}}"#;
        let out = json(&merge(Some(live), &desired(NESTED), &[]).unwrap().bytes);
        assert_eq!(out["model"], "haiku");
        assert_eq!(out["permissions"]["defaultMode"], "plan");
    }

    #[test]
    fn an_append_array_adds_unique_items_once_and_keeps_the_users() {
        let live = br#"{"sandbox":{"filesystem":{"allowWrite":["/user","/m/two"]}}}"#;
        let d = desired(NESTED);
        let m = merge(Some(live), &d, &[]).unwrap();
        let aw = json(&m.bytes)["sandbox"]["filesystem"]["allowWrite"].clone();
        assert_eq!(aw, serde_json::json!(["/user", "/m/two", "/m/one"]));
        // Only the item the manager added is recorded. `/m/two` was the user's.
        assert_eq!(m.appended.len(), 1);
        let again = merge_with(Some(&m.bytes), &d, &[], &m.appended, false).unwrap();
        assert!(!again.changed, "a second merge changes nothing");
        assert_eq!(again.appended, m.appended);
    }

    #[test]
    fn an_append_array_replaces_nothing_and_creates_the_path_when_absent() {
        let m = merge(None, &desired(NESTED), &[]).unwrap();
        assert_eq!(
            json(&m.bytes)["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/m/one", "/m/two"])
        );
        assert_eq!(m.appended.len(), 2);
    }

    #[test]
    fn a_dropped_appended_item_is_removed_and_a_user_item_is_not() {
        let d1 = desired(NESTED);
        let live = br#"{"sandbox":{"filesystem":{"allowWrite":["/user"]}}}"#;
        let m1 = merge(Some(live), &d1, &[]).unwrap();
        let d2 = desired(&NESTED.replace(r#""/m/one", "/m/two", "/m/one""#, r#""/m/two""#));
        let m2 = merge_with(Some(&m1.bytes), &d2, &[], &m1.appended, false).unwrap();
        assert_eq!(
            json(&m2.bytes)["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/user", "/m/two"])
        );
        assert_eq!(m2.appended.len(), 1);
    }

    #[test]
    fn deselecting_removes_appended_items_and_the_paths_they_emptied() {
        let m = merge(None, &desired(NESTED), &[]).unwrap();
        let (bytes, changed) = retire(&m.bytes, &m.entries, &m.appended).unwrap();
        assert!(changed);
        let out = json(&bytes);
        assert!(out.get("sandbox").is_none(), "the emptied path is dropped");
        assert_eq!(out["env"]["MANAGED"], "1", "managed values stay");
        // A user item in the same array stays, and so does the array.
        let live = br#"{"sandbox":{"filesystem":{"allowWrite":["/user"]}}}"#;
        let m = merge(Some(live), &desired(NESTED), &[]).unwrap();
        let (bytes, _) = retire(&m.bytes, &m.entries, &m.appended).unwrap();
        assert_eq!(
            json(&bytes)["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/user"])
        );
    }

    #[test]
    fn the_managed_view_ignores_foreign_leaves_and_append_items() {
        let d = desired(NESTED);
        let m = merge(None, &d, &[]).unwrap();
        let salt = [3u8; 32];
        let rec = CoOwned {
            managed_hash: salted(&salt, &m.rendered_view),
            entries: m.entries.clone(),
            appended: m.appended.clone(),
            leaves: leaf_hashes(&salt, &m),
        };
        // A plugin the user adds, a user sandbox path and a deleted appended item are no edit.
        let live = br#"{"env":{"MANAGED":"1","MINE":"x"},"enabledPlugins":{"a@m":true,"user@m":true},"sandbox":{"filesystem":{"allowWrite":["/user"]}}}"#;
        let (merged, _) = classify(Some(live), &d, Some(&rec), Some(&salt)).unwrap();
        // The view holds the managed leaves and nothing the user added.
        assert_eq!(
            merged.live_leaves.keys().collect::<Vec<_>>(),
            ["/enabledPlugins/a@m", "/env/MANAGED"]
        );
        assert_eq!(
            merged.appended.len(),
            2,
            "a deleted appended item comes back"
        );
        // A change of a managed leaf is an edit.
        let edited = br#"{"env":{"MANAGED":"2"},"enabledPlugins":{"a@m":true}}"#;
        let e = classify(Some(edited), &d, Some(&rec), Some(&salt)).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert!(!e.detail.contains('2'));
    }

    #[test]
    fn a_managed_object_over_a_live_scalar_is_refused_without_content() {
        let live = br#"{"env":"secret-text"}"#;
        let e = merge(Some(live), &desired(NESTED), &[]).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert!(!e.detail.contains("secret-text"));
        let live = br#"{"sandbox":{"filesystem":{"allowWrite":"secret-text"}}}"#;
        let e = merge(Some(live), &desired(NESTED), &[]).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert!(!e.detail.contains("secret-text"));
    }

    #[test]
    fn append_pointers_are_validated_and_fragments_need_both_fields() {
        for bad in [
            "append = [\"sandbox\"]\n[managed]\n",
            "append = [\"/a//b\"]\n[managed]\n",
            "append = [\"/a/~2\"]\n[managed]\n",
            "append = \"/a\"\n[managed]\n",
            "fragments_dir = \".claude/settings.d\"\n[managed]\n",
        ] {
            let v = toml::from_str::<Toml>(bad).unwrap();
            assert_eq!(
                Desired::from_value(&v).unwrap_err().code,
                Code::ManifestInvalid,
                "{bad}"
            );
        }
        // An append pointer must name an array under managed. A mistyped one is refused, because
        // it would turn append into replace and lose the user's items.
        for bad in [
            "append = [\"/a/b\"]\n[managed.a]\nb = 1\n",
            "append = [\"/sandbox/filesystem/allowwrite\"]\n[managed.sandbox.filesystem]\nallowWrite = [\"x\"]\n",
            "append = [\"/a/missing\"]\n[managed.a]\nb = [1]\n",
            "append = [\"/hooks/Stop\"]\n[managed.a]\nb = [1]\n",
        ] {
            let v = toml::from_str::<Toml>(bad).unwrap();
            let e = Desired::from_value(&v).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{bad}");
        }
        desired("append = [\"/a/b\"]\n[managed.a]\nb = [1]\n");
    }

    #[test]
    fn fragments_layer_in_name_order_over_the_declared_values() {
        let d = desired(&format!(
            "fragments_dir = \".claude/settings.d\"\npattern = \"*.json\"\n{NESTED}"
        ));
        assert_eq!(d.fragment_spec(), Some((".claude/settings.d", "*.json")));
        let layered = d
            .with_fragments([
                (
                    "10-a.json",
                    br#"{"env":{"MANAGED":"frag","NEW":"n"},"outputStyle":"X"}"#.as_slice(),
                ),
                ("20-b.json", br#"{"env":{"NEW":"later"}}"#.as_slice()),
            ])
            .unwrap();
        let out = json(&merge(None, &layered, &[]).unwrap().bytes);
        assert_eq!(
            out["env"],
            serde_json::json!({"MANAGED":"frag","NEW":"later"})
        );
        assert_eq!(out["outputStyle"], "X");
        // The declared digest ignores fragments. The names cover the layered view.
        assert_eq!(layered.managed_sha256(), d.managed_sha256());
        assert!(layered.setting_names().contains(&"outputStyle".to_string()));
        assert!(!d.setting_names().contains(&"outputStyle".to_string()));
        // The salted view covers the fragments.
        assert_ne!(layered.rendered_view(), d.rendered_view());
    }

    #[test]
    fn a_bad_fragment_is_refused_by_name_without_content() {
        let d = desired(NESTED);
        for (bytes, why) in [
            (b"{not json planted-token".as_slice(), "not valid JSON"),
            (b"[1]".as_slice(), "not an object"),
            (br#"{"hooks":{}}"#.as_slice(), "sets hooks"),
            (br#"{"":1}"#.as_slice(), "empty or has a control"),
            (br#"{"a\nb":1}"#.as_slice(), "empty or has a control"),
            (
                br#"{"env":{"x\u0001":1}}"#.as_slice(),
                "empty or has a control",
            ),
        ] {
            let e = d.with_fragments([("02-bad.json", bytes)]).unwrap_err();
            assert_eq!(e.code, Code::Conflict);
            assert!(
                e.detail.contains("02-bad.json") && e.detail.contains(why),
                "{}",
                e.detail
            );
            assert!(!e.detail.contains("planted-token"));
        }
    }

    #[test]
    fn a_handler_may_carry_a_matcher_and_a_timeout() {
        let d = desired(
            "[managed]\n[handlers.guard]\nevent = \"PreToolUse\"\ntype = \"command\"\nargv = [\"g\"]\nmatcher = \"Bash\"\ntimeout = 20\n",
        );
        let out = json(&merge(None, &d, &[]).unwrap().bytes);
        let g = &out["hooks"]["PreToolUse"][0];
        assert_eq!(g["matcher"], "Bash");
        assert_eq!(g["hooks"][0]["timeout"], 20);
        for bad in ["matcher = 3", "timeout = 0", "timeout = \"x\""] {
            let v = toml::from_str::<Toml>(&format!(
                "[managed]\n[handlers.g]\nevent = \"E\"\ntype = \"command\"\nargv = [\"g\"]\n{bad}\n"
            ))
            .unwrap();
            assert!(Desired::from_value(&v).is_err(), "{bad}");
        }
    }

    #[test]
    fn command_identity_ignores_home_spelling_and_quoting() {
        let home = Some("/home/me");
        let id = |c: &str| command_identity(c, home);
        let want = id("~/.claude/hooks/x.sh");
        for same in [
            "$HOME/.claude/hooks/x.sh",
            "${HOME}/.claude/hooks/x.sh",
            "\"${HOME}/.claude/hooks/x.sh\"",
            "\"$HOME/.claude/hooks/x.sh\"",
            "/home/me/.claude/hooks/x.sh",
            "'/home/me/.claude/hooks/x.sh'",
            "  ~/.claude/hooks/x.sh  ",
        ] {
            assert_eq!(id(same), want, "{same}");
        }
        // A single-quoted or escaped `~` or `$HOME` is literal text, not the home directory.
        for literal in [
            "'~/.claude/hooks/x.sh'",
            "'${HOME}/.claude/hooks/x.sh'",
            "\\$HOME/.claude/hooks/x.sh",
            "\"\\${HOME}/.claude/hooks/x.sh\"",
            "\"~/.claude/hooks/x.sh\"",
            "\\~/.claude/hooks/x.sh",
            // A token that is only partly quoted is not one expandable unit.
            "\"$\"HOME/.claude/hooks/x.sh",
            "~\"/.claude/hooks/x.sh\"",
            "$HO\"ME\"/.claude/hooks/x.sh",
            "$H\"\"OME/.claude/hooks/x.sh",
            "\"$\"\"HOME\"/.claude/hooks/x.sh",
        ] {
            assert_ne!(id(literal), want, "{literal}");
        }
        for other in [
            "~/.claude/hooks/y.sh",
            "/home/mex/.claude/hooks/x.sh",
            "bash ~/.claude/hooks/x.sh",
            "/opt/x/.claude/hooks/x.sh",
        ] {
            assert_ne!(id(other), want, "{other}");
        }
        assert_ne!(id("a 'b c'"), id("a b c"), "word boundaries count");
        assert_eq!(
            id("a 'b c"),
            "raw\0a 'b c",
            "an unclosed quote is its own identity"
        );
    }

    #[test]
    fn an_equivalent_hook_is_not_duplicated_and_not_owned_even_at_take_over() {
        let d = desired(
            "[managed]\n[handlers.g]\nevent = \"E\"\ntype = \"command\"\nargv = [\"${HOME}/h/x.sh\"]\n",
        )
        .with_home("/home/me");
        let live = br#"{"hooks":{"E":[{"hooks":[{"type":"command","command":"/home/me/h/x.sh"},{"type":"command","command":"foreign"}]}]}}"#;
        for takeover in [false, true] {
            let m = merge_with(Some(live), &d, &[], &[], takeover).unwrap();
            assert_eq!(json(&m.bytes)["hooks"]["E"].as_array().unwrap().len(), 1);
            assert!(m.entries.is_empty(), "takeover={takeover}");
        }
    }

    #[test]
    fn dropping_a_managed_key_is_no_edit_but_a_hand_edit_of_a_kept_key_still_is() {
        let salt = [5u8; 32];
        let wide = desired("[managed]\na = 1\nb = 2\n");
        let first = merge(None, &wide, &[]).unwrap();
        let rec = CoOwned {
            managed_hash: salted(&salt, &first.rendered_view),
            entries: vec![],
            appended: vec![],
            leaves: leaf_hashes(&salt, &first),
        };
        let narrow = desired("[managed]\na = 1\n");
        // The module dropped `b`: the live file is as written, so the write is allowed.
        assert!(classify(Some(&first.bytes), &narrow, Some(&rec), Some(&salt)).is_ok());
        // A hand edit of the kept key is still a conflict, with or without the dropped key.
        let edited = br#"{"a":9,"b":2}"#;
        let e = classify(Some(edited), &narrow, Some(&rec), Some(&salt)).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert!(!e.detail.contains('9'));
        // A key the module added is not compared: there is nothing recorded for it.
        let wider = desired("[managed]\na = 1\nb = 2\nc = 3\n");
        assert!(classify(Some(&first.bytes), &wider, Some(&rec), Some(&salt)).is_ok());
        // A removed kept key is an edit.
        let e = classify(Some(br#"{"b":2}"#), &narrow, Some(&rec), Some(&salt)).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
    }

    #[test]
    fn a_declared_value_over_a_foreign_table_is_refused_without_content() {
        let live = br#"{"a":{"secret":"planted-token"}}"#;
        let e = merge(Some(live), &desired("[managed]\na = 1\n"), &[]).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert!(!e.detail.contains("planted-token"));
        let e = merge(Some(live), &desired("[managed]\na = [1]\n"), &[]).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
    }

    #[test]
    fn a_seed_fills_a_live_null() {
        let d = desired("[managed]\n[seed]\nmodel = \"m\"\n[seed.p]\nq = 1\n");
        let out = json(
            &merge(Some(br#"{"model":null,"p":{"q":null}}"#), &d, &[])
                .unwrap()
                .bytes,
        );
        assert_eq!(
            (out["model"].clone(), out["p"]["q"].clone()),
            ("m".into(), 1.into())
        );
    }

    #[test]
    fn a_fragment_array_at_an_append_pointer_adds_unique_items() {
        let d = desired(NESTED);
        let layered = d
            .with_fragments([(
                "10.json",
                br#"{"sandbox":{"filesystem":{"allowWrite":["/m/two","/frag/new"]}}}"#.as_slice(),
            )])
            .unwrap();
        let m = merge(None, &layered, &[]).unwrap();
        assert_eq!(
            json(&m.bytes)["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/m/one", "/m/two", "/frag/new"])
        );
        // Another array in a fragment still replaces.
        let layered = d
            .with_fragments([
                ("10.json", br#"{"extra":[1]}"#.as_slice()),
                ("20.json", br#"{"extra":[2]}"#.as_slice()),
            ])
            .unwrap();
        assert_eq!(
            json(&merge(None, &layered, &[]).unwrap().bytes)["extra"],
            serde_json::json!([2])
        );
    }

    #[test]
    fn handler_matcher_and_timeout_are_validated() {
        let with = |extra: &str| {
            toml::from_str::<Toml>(&format!(
                "[managed]\n[handlers.g]\nevent = \"E\"\ntype = \"command\"\nargv = [\"g\"]\n{extra}\n"
            ))
            .unwrap()
        };
        for ok in [
            "matcher = \"\"",
            "timeout = 3600",
            "timeout = 1",
            "matcher = \"Bash|Write\"",
        ] {
            assert!(Desired::from_value(&with(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "timeout = 3601",
            "timeout = -5",
            "matcher = \"a\\nb\"",
            "matcher = true",
        ] {
            let e = Desired::from_value(&with(bad)).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{bad}");
        }
        let out = json(
            &merge(
                None,
                &Desired::from_value(&with("matcher = \"\"")).unwrap(),
                &[],
            )
            .unwrap()
            .bytes,
        );
        assert_eq!(out["hooks"]["E"][0]["matcher"], "");
    }

    #[test]
    fn a_merge_that_would_record_too_many_leaves_is_refused() {
        let d = desired(NESTED);
        let mut big = String::from("{");
        for i in 0..=MAX_RECORDED {
            big.push_str(&format!("\"k{i}\":{i},"));
        }
        big.push_str("\"z\":0}");
        let layered = d.with_fragments([("big.json", big.as_bytes())]).unwrap();
        let e = merge(None, &layered, &[]).unwrap_err();
        assert_eq!(e.code, Code::LimitExceeded);
        assert!(merge(None, &d, &[]).is_ok());
    }

    #[test]
    fn an_owned_record_removes_one_element_and_an_identical_user_copy_stays() {
        let d = desired(BASE);
        let first = merge(None, &d, &[]).unwrap();
        // The user adds an identical copy of the manager's hook.
        let mut doc = json(&first.bytes);
        let copy = doc["hooks"]["PreToolUse"][0].clone();
        doc["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(copy);
        let live = serde_json::to_vec(&doc).unwrap();
        // The handler leaves the module: one element goes.
        let gone = desired("[managed]\noutputStyle = \"STE\"\n");
        let m = merge(Some(&live), &gone, &first.entries).unwrap();
        assert_eq!(
            json(&m.bytes)["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // A retire removes one element too.
        let (bytes, _) = retire(&live, &first.entries, &[]).unwrap();
        assert_eq!(
            json(&bytes)["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // The same holds for an appended array item.
        let n = desired(NESTED);
        let a = merge(
            Some(br#"{"sandbox":{"filesystem":{"allowWrite":["/m/one","/m/one"]}}}"#),
            &n,
            &[],
        )
        .unwrap();
        let rec = vec![AppendedItem {
            pointer: "/sandbox/filesystem/allowWrite".into(),
            sha256: sha_of(&Json::from("/m/one")),
        }];
        let (bytes, _) = retire(&serde_json::to_vec(&json(&a.bytes)).unwrap(), &[], &rec).unwrap();
        assert_eq!(
            json(&bytes)["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/m/one", "/m/two"]),
            "one copy of /m/one is removed and the other stays"
        );
    }

    #[test]
    fn a_handler_id_that_the_ledger_would_refuse_is_refused_when_desired_is_built() {
        for id in ["\"\"", "\"a\\nb\"", "\"a\\u0001\""] {
            let v = toml::from_str::<Toml>(&format!(
                "[managed]\n[handlers.{id}]\nevent = \"E\"\ntype = \"command\"\nargv = [\"g\"]\n"
            ))
            .unwrap();
            let e = Desired::from_value(&v).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{id}");
        }
    }

    #[test]
    fn renaming_a_managed_key_in_one_update_is_no_conflict() {
        let salt = [8u8; 32];
        let old = desired("[managed]\nkeep = 1\nold = 2\n");
        let first = merge(None, &old, &[]).unwrap();
        let rec = CoOwned {
            managed_hash: salted(&salt, &first.rendered_view),
            entries: vec![],
            appended: vec![],
            leaves: leaf_hashes(&salt, &first),
        };
        // `old` is removed and `new` is added in one source update. The live file is untouched.
        let renamed = desired("[managed]\nkeep = 1\nnew = 2\n");
        let (m, o) = classify(Some(&first.bytes), &renamed, Some(&rec), Some(&salt)).unwrap();
        assert_eq!(o, Outcome::Write);
        let out = json(&m.bytes);
        assert_eq!(
            (out["new"].clone(), out["old"].clone()),
            (2.into(), 2.into())
        );
        // A hand edit of the kept key in the same state is still a conflict.
        let e = classify(
            Some(br#"{"keep":9,"old":2}"#),
            &renamed,
            Some(&rec),
            Some(&salt),
        )
        .unwrap_err();
        assert_eq!(e.code, Code::Conflict);
    }

    #[test]
    fn a_number_spelled_as_a_float_is_the_same_leaf_as_the_integer() {
        let salt = [9u8; 32];
        let d = desired("[managed]\nretries = 1\nbig = 1e20\n");
        let first = merge(None, &d, &[]).unwrap();
        let rec = CoOwned {
            managed_hash: salted(&salt, &first.rendered_view),
            entries: vec![],
            appended: vec![],
            leaves: leaf_hashes(&salt, &first),
        };
        // The same value spelled `1.0` is no foreign edit and no change: the file stays as it
        // is. A real change still is an edit.
        let (_, o) = classify(
            Some(br#"{"retries":1.0,"big":1e20}"#),
            &d,
            Some(&rec),
            Some(&salt),
        )
        .unwrap();
        assert_eq!(o, Outcome::Current);
        let e = classify(
            Some(br#"{"retries":1.5,"big":1e20}"#),
            &d,
            Some(&rec),
            Some(&salt),
        )
        .unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert_eq!(canonical(&json(b"[1.0, 2.5, -0.0]")), b"[1,2.5,0]");
    }

    #[test]
    fn a_managed_key_the_ledger_cannot_spell_is_refused_at_any_depth() {
        for src in [
            "[managed]\n\"\" = 1\n",
            "[managed]\n\"a\\nb\" = 1\n",
            "[managed.outer]\n\"\" = 1\n",
            "[managed.outer.inner]\n\"x\\u0001\" = 1\n",
        ] {
            let e = Desired::from_value(&toml::from_str::<Toml>(src).unwrap()).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{src}");
        }
    }

    #[test]
    fn a_no_break_space_stays_inside_a_shell_word() {
        let split = |c: &str| shell_words(c).unwrap().len();
        assert_eq!(split("a\u{a0}b"), 1);
        assert_eq!(split("a b\tc\nd"), 4);
        assert_ne!(
            command_identity("run a\u{a0}b", None),
            command_identity("run a b", None)
        );
    }
}
