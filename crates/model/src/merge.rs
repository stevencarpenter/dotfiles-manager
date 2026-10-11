//! The `claude-settings/v1` merge (docs/adapters.md, "claude-settings/v1"). Pure: it takes the
//! live bytes, the declared value and the handler entries the ledger records as owned, and
//! returns the new bytes. It opens no file.
//!
//! This code lives in `dfm-model` because `serde_json` is allowed there only (docs/architecture.md
//! section 1). The adapter glue (registry entry, `Generator` impl) is in the CLI crate.
//!
//! The live document is the base. The declared `managed` values merge into it: an object merges
//! recursively (managed leaves win, foreign leaves survive), an array replaces, except at a
//! pointer listed in `append`, where the unique declared items are appended and the manager
//! records them as its own. Fragments (`settings.d`) merge over the declared values first. The
//! declared `seed` values are set only where the live file has nothing, at any depth, and the
//! declared handlers are added to `hooks.<event>` as owned entries. Every other key and every
//! foreign hook entry is carried over by value. The output is re-serialized with sorted keys and
//! a 2-space indent, so formatting is normalized.
//!
//! Nothing in an error message repeats a byte of the live file: a parse error names a category,
//! a line and a column only.

use crate::digest::sha256_hex;
use crate::refusal::{Code, Refusal};
use crate::state::{AppendedItem, CoOwned, OwnedEntry};
use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet};
use toml::Value as Toml;

/// The adapter id this module implements.
pub const ADAPTER: &str = "claude-settings/v1";
/// The mode of the published file. A live file open to group or others is refused.
pub const MODE: u32 = 0o600;

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
}

/// The result of a merge. `Debug` prints lengths only: the bytes can hold credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Merged {
    /// The new file content.
    pub bytes: Vec<u8>,
    /// Canonical JSON of the managed keys as the live file holds them (absent keys are left
    /// out). Only a salted hash of it may be stored.
    pub live_view: Vec<u8>,
    /// Canonical JSON of the managed keys as declared.
    pub rendered_view: Vec<u8>,
    /// The handler entries the manager owns after this merge. The ledger records them.
    pub entries: Vec<OwnedEntry>,
    /// The array items the manager owns after this merge (`append`). The ledger records them.
    pub appended: Vec<AppendedItem>,
    /// Whether `bytes` differ from the live bytes.
    pub changed: bool,
}

impl std::fmt::Debug for Merged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Merged")
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field("live_view", &format_args!("{} bytes", self.live_view.len()))
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

fn canonical(v: &Json) -> Vec<u8> {
    // `serde_json::Map` is a sorted map here (no `preserve_order`), so this is canonical.
    serde_json::to_vec(v).expect("a serde_json::Value always serializes: its keys are strings")
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
            "{at}.type is `{kind}`; claude-settings/v1 builds only `command` hooks. Fix: use type = \"command\", or add the hook by hand (the manager cannot reverse any other hook type safely)"
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
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid(format!("{at}.timeout must be a positive integer")))?;
        hook["timeout"] = Json::from(secs);
    }
    let mut entry = serde_json::json!({ "hooks": [hook] });
    if let Some(v) = t.get("matcher") {
        let m = v
            .as_str()
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .ok_or_else(|| invalid(format!("{at}.matcher must be a non-empty string")))?;
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

impl Desired {
    /// Converts the resolved resource value. `managed` is required. `hooks` is reserved for
    /// handlers, so a managed or seed key named `hooks` is refused.
    pub fn from_value(value: &Toml) -> Result<Desired, Refusal> {
        let root = value
            .as_table()
            .ok_or_else(|| invalid("the claude-settings value must be a table"))?;
        let section = |name: &str, required: bool| -> Result<Map<String, Json>, Refusal> {
            match root.get(name) {
                Some(Toml::Table(t)) => table(t, name),
                Some(_) => Err(invalid(format!("{name} must be a table"))),
                None if required => Err(invalid(format!("{name} is required"))),
                None => Ok(Map::new()),
            }
        };
        let (managed, seed) = (section("managed", true)?, section("seed", false)?);
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
        })
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
            overlay(&mut out.effective, &obj);
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

    fn rendered_view(&self) -> Vec<u8> {
        canonical(&Json::Object(view_of(
            &self.effective,
            &self.effective,
            "",
            &self.append,
        )))
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

/// Deep merge of fragment objects: objects merge, everything else replaces.
fn overlay(dst: &mut Map<String, Json>, src: &Map<String, Json>) {
    for (k, v) in src {
        match (dst.get_mut(k), v) {
            (Some(Json::Object(d)), Json::Object(s)) => overlay(d, s),
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

fn escape_token(t: &str) -> String {
    t.replace('~', "~0").replace('/', "~1")
}

/// A JSON pointer that names at least one key, in its canonical spelling. `None` when the text
/// is not a pointer (RFC 6901) or names the whole document.
fn normal_pointer(p: &str) -> Option<String> {
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
            .map(|t| format!("/{}", escape_token(t)))
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
        let here = format!("{at}/{}", escape_token(k));
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

fn serialize(doc: Map<String, Json>) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(&Json::Object(doc))
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
    let before = arr.len();
    arr.retain(|e| sha_of(e) != sha256);
    if arr.len() != before {
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
            Some(Json::Array(a)) => {
                let before = a.len();
                a.retain(|e| sha_of(e) != sha256);
                a.is_empty() && a.len() != before
            }
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
            .map(|t| format!("/{}", escape_token(t)))
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
            (None, _) => {
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
    let mut doc = parse_live(live)?;
    let live_view = canonical(&Json::Object(view_of(
        &doc,
        &desired.effective,
        "",
        &desired.append,
    )));
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
    let mut entries = Vec::new();
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
        let present = match event_array(hooks, &h.event)? {
            Some(arr) => {
                let found = arr.iter().any(|e| sha_of(e) == h.sha256);
                if !found {
                    arr.push(h.entry.clone());
                }
                found
            }
            None => {
                hooks.insert(h.event.clone(), Json::Array(vec![h.entry.clone()]));
                false
            }
        };
        // The manager owns an entry it added now or recorded as its own before, and, at an
        // approved take-over, an identical entry already there. Otherwise an identical entry
        // that is not recorded belongs to the user: a later retire must never delete it.
        let was_owned = owned
            .iter()
            .any(|o| o.id == h.id && o.key == h.event && o.sha256 == h.sha256);
        if !present || was_owned || takeover {
            entries.push(OwnedEntry {
                id: h.id.clone(),
                key: h.event.clone(),
                sha256: h.sha256.clone(),
            });
        }
    }
    let bytes = serialize(doc);
    let changed = live != Some(bytes.as_slice());
    Ok(Merged {
        bytes,
        live_view,
        rendered_view: desired.rendered_view(),
        entries,
        appended: walk.appended,
        changed,
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
/// and whether they differ.
pub fn retire(
    live: &[u8],
    owned: &[OwnedEntry],
    appended: &[AppendedItem],
) -> Result<(Vec<u8>, bool), Refusal> {
    let mut doc = parse_live(Some(live))?;
    let mut touched = BTreeSet::new();
    for o in owned {
        remove_owned(&mut doc, &o.key, &o.sha256, &mut touched)?;
    }
    tidy(&mut doc, &touched);
    for a in appended {
        remove_item(&mut doc, &pointer_tokens(&a.pointer), &a.sha256);
    }
    let bytes = serialize(doc);
    let changed = live != bytes.as_slice();
    Ok((bytes, changed))
}

/// Checks that the live document parses, and returns its managed view, without building a merge.
pub fn live_view(live: Option<&[u8]>, desired: &Desired) -> Result<Vec<u8>, Refusal> {
    Ok(canonical(&Json::Object(view_of(
        &parse_live(live)?,
        &desired.effective,
        "",
        &desired.append,
    ))))
}

/// The text for a lost or unusable salt while the ledger records a salted hash. It is a hard
/// state error: no command recovers it.
pub const SALT_LOST: &str = "state/salt is missing or unusable, but the ledger records a salted hash of managed fields. This is a hard state error, so the foreign-edit check cannot run. Fix: restore state/salt from a copy (32 bytes, mode 0600). If no copy exists, edit state/ledger.json by hand and delete the `coowned` object (the key under resources[].owned_outputs[]) of each co-owned output; the next apply then needs --allow-adopt for that file";

/// Whether a managed field was changed outside the manager since the last write. The live
/// managed view must differ from the salted hash the ledger holds, and must not already equal
/// the declared values (then nothing is overwritten). Plan and apply both decide through this.
pub fn foreign_edit(salt: &[u8], recorded: &CoOwned, m: &Merged) -> bool {
    salted(salt, &m.live_view) != recorded.managed_hash && m.live_view != m.rendered_view
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
    /// The result equals the live bytes and the ledger records the same hash and entries.
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
/// result may be stored in `ledger.json` and nowhere else (docs/architecture.md section 8).
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
        assert_eq!(m.live_view, br#"{"enableAll":true}"#);
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
        };
        // A plugin the user adds, a user sandbox path and a deleted appended item are no edit.
        let live = br#"{"env":{"MANAGED":"1","MINE":"x"},"enabledPlugins":{"a@m":true,"user@m":true},"sandbox":{"filesystem":{"allowWrite":["/user"]}}}"#;
        let (merged, _) = classify(Some(live), &d, Some(&rec), Some(&salt)).unwrap();
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
        let d = desired("append = [\"/a/b\"]\n[managed.a]\nb = 1\n");
        assert!(merge(None, &d, &[]).is_err_and(|e| e.code == Code::ManifestInvalid));
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
}
