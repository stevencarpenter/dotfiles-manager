//! The `claude-settings/v1` merge (docs/adapters.md, "claude-settings/v1"). Pure: it takes the
//! live bytes, the declared value and the handler entries the ledger records as owned, and
//! returns the new bytes. It opens no file.
//!
//! This code lives in `dfm-model` because `serde_json` is allowed there only (docs/architecture.md
//! section 1). The adapter glue (registry entry, `Generator` impl) is in the CLI crate.
//!
//! The live document is the base. The declared `managed` keys are set, the declared `seed` keys
//! are set only when absent, and the declared handlers are added to `hooks.<event>` as owned
//! entries. Every other key and every foreign hook entry is carried over by value. The output is
//! re-serialized with sorted keys and a 2-space indent, so formatting is normalized.
//!
//! Nothing in an error message repeats a byte of the live file: a parse error names a category,
//! a line and a column only.

use crate::digest::sha256_hex;
use crate::refusal::{Code, Refusal};
use crate::state::{CoOwned, OwnedEntry};
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
    managed: Map<String, Json>,
    seed: Map<String, Json>,
    handlers: Vec<Handler>,
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
    // A leading `=` is expanded by zsh (`=cmd` is the path of cmd), so such a word is quoted.
    let plain = !a.is_empty()
        && !a.starts_with('=')
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
    let entry = serde_json::json!({ "hooks": [ { "command": command, "type": "command" } ] });
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
        Ok(Desired {
            managed,
            seed,
            handlers,
        })
    }

    /// SHA-256 of the canonical JSON of the declared managed values. The module declares them,
    /// so the digest is not secret and may appear in the plan.
    pub fn managed_sha256(&self) -> String {
        sha256_hex(&canonical(&Json::Object(self.managed.clone())))
    }

    /// The number of declared handlers.
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }

    /// The top-level setting names the manager sets or seeds, sorted, never values.
    pub fn setting_names(&self) -> Vec<String> {
        let mut names: BTreeSet<String> = self.managed.keys().cloned().collect();
        names.extend(self.seed.keys().cloned());
        names.into_iter().collect()
    }

    fn rendered_view(&self) -> Vec<u8> {
        canonical(&Json::Object(self.managed.clone()))
    }
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

fn view_of(doc: &Map<String, Json>, keys: &Map<String, Json>) -> Vec<u8> {
    let picked: Map<String, Json> = keys
        .keys()
        .filter_map(|k| doc.get(k).map(|v| (k.clone(), v.clone())))
        .collect();
    canonical(&Json::Object(picked))
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
    let mut doc = parse_live(live)?;
    let live_view = view_of(&doc, &desired.managed);
    for (k, v) in &desired.managed {
        doc.insert(k.clone(), v.clone());
    }
    for (k, v) in &desired.seed {
        doc.entry(k.clone()).or_insert_with(|| v.clone());
    }
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
        // The manager owns an entry it added now or recorded as its own before. An identical
        // entry that was already there and is not recorded belongs to the user: a later retire
        // must never delete it.
        let was_owned = owned
            .iter()
            .any(|o| o.id == h.id && o.key == h.event && o.sha256 == h.sha256);
        if !present || was_owned {
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
        changed,
    })
}

/// Removes only the handler entries the ledger recorded as owned. Managed values, seeded values
/// and every foreign key and hook stay. Returns the new bytes and whether they differ.
pub fn retire(live: &[u8], owned: &[OwnedEntry]) -> Result<(Vec<u8>, bool), Refusal> {
    let mut doc = parse_live(Some(live))?;
    let mut touched = BTreeSet::new();
    for o in owned {
        remove_owned(&mut doc, &o.key, &o.sha256, &mut touched)?;
    }
    tidy(&mut doc, &touched);
    let bytes = serialize(doc);
    let changed = live != bytes.as_slice();
    Ok((bytes, changed))
}

/// Checks that the live document parses, and returns its managed view, without building a merge.
pub fn live_view(live: Option<&[u8]>, desired: &Desired) -> Result<Vec<u8>, Refusal> {
    Ok(view_of(&parse_live(live)?, &desired.managed))
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
    let merged = merge(live, desired, owned)?;
    check_foreign(live.is_some(), recorded, salt, &merged)?;
    let current = live.is_some()
        && !merged.changed
        && recorded.zip(salt).is_some_and(|(c, salt)| {
            salted(salt, &merged.rendered_view) == c.managed_hash && c.entries == merged.entries
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
        let (bytes, changed) = retire(&m.bytes, &m.entries).unwrap();
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
        let (bytes, _) = retire(&m.bytes, &m.entries).unwrap();
        assert!(json(&bytes).get("hooks").is_none());
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
        assert_eq!(shell_word("a=b"), "a=b");
    }

    #[test]
    fn an_identical_hook_that_was_already_there_is_not_recorded_as_owned() {
        let live = br#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"audit --quiet 'two words'","type":"command"}]}]}}"#;
        let first = merge(Some(live), &desired(BASE), &[]).unwrap();
        assert!(first.entries.is_empty(), "{:?}", first.entries);
        let (bytes, _) = retire(&first.bytes, &first.entries).unwrap();
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
}
