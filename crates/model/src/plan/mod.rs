//! The typed plan (docs/architecture.md sections 6, 9 and 10; docs/planner.md).
//!
//! Pure: [`requests`] says which local facts the planner needs, the caller observes them
//! (read-only), and [`build`] turns the resolved model, the observations, the ownership ledger
//! and the source identities into a [`Plan`]. The same inputs always give the same plan and the
//! same [`Plan::digest`]. [`stale`] compares a saved plan with a fresh one and names what
//! changed.

mod build;
mod graph;
mod stale;

pub use build::{
    Context, MergeFact, MergeVerdict, Owned, build, followups, in_scope, ledger_state,
    matches_pattern, requests, sensitive_paths,
};
pub use stale::{Stale, StaleKind, stale};

use crate::digest::sha256_hex;
use crate::refusal::{Code, Refusal};
use crate::state::{Class, canonical};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: i64 = 1;
/// A saved plan is at most this large (the state document read limit, section 5).
pub const MAX_PLAN_BYTES: usize = 16 << 20;
/// A directory listing or a regular file hash is bounded. A larger one is `Unknown`.
pub const MAX_LISTING: usize = 50_000;
pub const MAX_HASHED_BYTES: u64 = 16 << 20;

/// What was found at one HOME-relative path, without following a link at the path itself.
/// `own` says whether the current user owns it. Raw user ids are not recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    Missing {},
    Dir {
        mode: u32,
        own: bool,
    },
    /// `sha256` is present only when the caller asked for it and the file is a bounded regular
    /// file. It is never recorded for a sensitive path.
    File {
        mode: u32,
        own: bool,
        size: u64,
        sha256: Option<String>,
    },
    Link {
        target: String,
        to: LinkTo,
        own: bool,
    },
    Other {
        mode: u32,
        own: bool,
    },
    /// A required fact that could not be observed. Planning fails closed on it.
    Unknown {
        reason: String,
    },
}

/// What a symlink resolves to when followed once from its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkTo {
    Dir,
    File,
    Other,
    Dangling,
    Loop,
}

impl LinkTo {
    pub fn name(self) -> &'static str {
        match self {
            LinkTo::Dir => "dir",
            LinkTo::File => "file",
            LinkTo::Other => "other",
            LinkTo::Dangling => "dangling",
            LinkTo::Loop => "loop",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seen {
    pub node: Node,
    /// The spelling on disk when it differs from the requested one only by case. The target
    /// filesystem folds case, so the two names are one entry.
    pub alias: Option<String>,
}

/// Why a listing is not the full set of names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Incomplete {
    /// More entries than [`MAX_LISTING`].
    TooMany,
    /// The directory could not be opened or read.
    Failed,
    /// An entry name is not UTF-8.
    NonUtf8,
}

/// The names in one directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    pub names: Vec<String>,
    pub incomplete: Option<Incomplete>,
}

/// Every local fact the plan rests on, keyed by HOME-relative path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observations {
    pub seen: BTreeMap<String, Seen>,
    pub listings: BTreeMap<String, Listing>,
}

/// One path to observe. The caller observes every proper prefix of `path` too, stopping at the
/// first one that is not a directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Request {
    pub path: String,
    /// Record the content hash of a regular file at `path`. Never set for a sensitive path.
    pub hash: bool,
    /// List the entries when `path` is a directory.
    pub list: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binary {
    pub version: String,
    pub sha256: String,
}

/// A source tree the plan was made from: `kind = "path"` identity (section 9): the canonical
/// absolute root plus the tree digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub kind: String,
    pub root: String,
    pub tree: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    CreateDir,
    CreateLink,
    ReplaceLink,
    CreateFile,
    UpdateFile,
    /// Remove an output the ledger proves this manager wrote and nothing selects any more.
    /// Apply (DFM-18) executes it after re-checking the fingerprint precondition.
    Remove,
    /// Record ownership of an existing link that is exactly the link the resource would create.
    /// Apply writes nothing to HOME for it and needs `--allow-adopt <operation-id>`.
    Adopt,
    /// Retire only the handler entries the ledger says the manager added to a co-owned file
    /// whose resource is no longer selected. The file, its managed and seeded values and every
    /// foreign entry stay (docs/adapters.md, `claude-settings/v1`).
    Retire,
    /// Nothing to do, or out of scope.
    None,
}

impl Action {
    /// Whether the operation carries a link target that apply checks against its source root and
    /// fingerprints: it creates, replaces or adopts a link.
    pub fn carries_link(self) -> bool {
        matches!(
            self,
            Action::CreateLink | Action::ReplaceLink | Action::Adopt
        )
    }

    /// Whether the operation puts a link or file into HOME. Adoption records ownership of what
    /// is already there, so it publishes nothing and has no reload state.
    pub fn publishes(self) -> bool {
        matches!(
            self,
            Action::CreateLink | Action::ReplaceLink | Action::CreateFile | Action::UpdateFile
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Planned,
    Current,
    NotInScope,
    Blocked,
}

/// Who owns the output, from the ledger (the only source of delete authority, section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    /// The output does not exist. Nothing is owned.
    Absent,
    /// A ledger entry for this resource and path matches the live output.
    Owned,
    /// The output already is the desired result and the ledger has no entry for it.
    UnrecordedCurrent,
    /// A ledger entry exists and the live output differs from it.
    Modified,
    /// The output exists and the ledger has no entry for it, or another resource owns it.
    Foreign,
    /// Ownership cannot be decided: a sensitive output has no comparable fingerprint yet.
    Unverifiable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Path of the source file in the source tree.
    pub path: String,
    pub content_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fragment {
    pub name: String,
    /// `selected:<resource>` or `foreign`.
    pub origin: String,
    /// SHA-256 of a foreign fragment's content as the planner observed it. Apply reads the
    /// fragment again and compares. A selected fragment has none: its content is the planned
    /// output of its producer (`Operation::output_sha256`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// A fact that must still hold at apply time: `subject` names it, `state` is its fingerprint
/// as [`node_state`] builds it. Apply observes again and compares the strings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Precondition {
    pub subject: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    /// Stable within one plan: `<resource>@<path>`, `mkdir@<path>`, `<resource>@scope` or
    /// `remove@<resource>@<path>`.
    pub id: String,
    /// The journal operation id for this operation (docs/state.md: lowercase letters, digits,
    /// `.`, `_`, `-`). It is derived from the model digest, the sources and `id`, not from the
    /// plan digest, which covers this field.
    pub journal_id: String,
    pub resource: String,
    pub module: String,
    pub adapter: String,
    pub class: Class,
    pub action: Action,
    pub status: Status,
    pub path: Option<String>,
    /// The file mode the resource declares for its output, if it declares one.
    pub mode: Option<u32>,
    /// The absolute link target for a link or distribution leaf. Apply reads this field and
    /// never parses `effect` or `postcondition`, which are prose for people.
    pub link: Option<String>,
    /// Always `user`: a HOME adapter never escalates (section 8).
    pub privilege: String,
    /// What the operation does, without content or secret values.
    pub effect: String,
    pub ownership: Ownership,
    pub inputs: Vec<Input>,
    pub fragments: Vec<Fragment>,
    /// The fragment directory (HOME-relative) an `assemble` operation reads. Apply lists it and
    /// reads each fragment in `fragments` from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragment_dir: Option<String>,
    /// SHA-256 of the content the adapter generates for this operation (render and assemble).
    /// The CLI computes it at plan time from the same inputs apply reads again, and apply
    /// compares the generated bytes with it before the intent entry (docs/adapters.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_sha256: Option<String>,
    /// SHA-256 of the canonical JSON of the managed values a co-owned merge declares. The module
    /// declares them, so the digest is not secret. A sensitive output has no content hash
    /// (`output_sha256` stays unset); this covers the declared set only, never the live file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_sha256: Option<String>,
    /// The first write into an existing co-owned file that the ledger does not record yet. It
    /// needs `--allow-adopt <operation-id>` (docs/adapters.md, "Take over an existing file").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub takeover: bool,
    /// Size in bytes of the generated content. For people: `plan` prints it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    /// The settings a generated file sets, as names only (for `git-config/v1`:
    /// `section.key` or `section.subsection.key`), never values. For people: a reviewer sees a
    /// new SSH-program setting before approving.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<String>,
    /// Excluded entries inside a directory source of this operation (`[sources].exclude`). They
    /// exist on disk, were not read and are not in the source hash, so a reviewer should know the
    /// directory holds more than the plan shows. Absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded: Option<u64>,
    /// Operation ids that must finish first.
    pub after: Vec<String>,
    pub preconditions: Vec<Precondition>,
    pub postcondition: String,
}

/// Why a plan cannot be approved: the source, resource and field, the conflicting claim or
/// failed precondition, and one correction. It never carries a secret value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocker {
    #[serde(with = "code_str")]
    pub code: Code,
    pub resource: String,
    pub source: String,
    pub field: Option<String>,
    pub path: Option<String>,
    pub detail: String,
    pub correction: String,
}

mod code_str {
    use super::Code;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(c: &Code, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(c.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Code, D::Error> {
        let s = String::deserialize(d)?;
        Code::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown code {s}")))
    }
}

impl Blocker {
    /// The refusal a blocked plan ends with: one stable code and the first failing location.
    pub fn refusal(&self) -> Refusal {
        let place = match (&self.path, &self.field) {
            (Some(p), Some(f)) => format!("{} ({p}, {f})", self.resource),
            (Some(p), None) => format!("{} ({p})", self.resource),
            _ => self.resource.clone(),
        };
        Refusal::new(
            self.code,
            crate::display::visible(&format!(
                "{place} in {}: {}. Fix: {}",
                self.source, self.detail, self.correction
            )),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema: i64,
    pub command: String,
    /// `config`: links, rendered and merged files, assembled files and distributions.
    /// Provisioning and secrets are out of scope (DESIGN.md section 7).
    pub scope: String,
    pub binary: Binary,
    pub profile: String,
    pub machine_id: String,
    /// Digest of the resolved model (secret-bearing values excluded).
    pub model_digest: String,
    pub sources: Vec<Source>,
    pub observations: Observations,
    pub operations: Vec<Operation>,
    pub blockers: Vec<Blocker>,
}

impl Plan {
    /// SHA-256 of the canonical JSON of the plan (sorted keys, no insignificant whitespace).
    pub fn digest(&self) -> Result<String, Refusal> {
        // `canonical` reports an impossible serialization failure as E_STATE_INVALID; it is passed on.
        Ok(sha256_hex(&canonical(self)?))
    }

    pub fn saved(&self) -> Result<Saved, Refusal> {
        Ok(Saved {
            plan_digest: self.digest()?,
            plan: self.clone(),
            isolated: None,
            stale: Vec::new(),
        })
    }
}

/// The inspectable form: the plan and its digest, as `plan --json` prints it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    pub plan_digest: String,
    pub plan: Plan,
    /// Set by a `test-isolation` build to show the overrides in force (section 7, item 3). It is
    /// outside the plan, so it is not part of the digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated: Option<String>,
    /// Set by `plan --json --against`: how this plan differs from the saved one. Outside the
    /// plan, so not part of the digest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale: Vec<Stale>,
}

impl Saved {
    pub fn to_json(&self) -> Result<Vec<u8>, Refusal> {
        canonical(self)
    }

    /// Parses a saved plan. A file whose digest does not match its content was edited and is
    /// refused, so a reviewer cannot be shown one plan while another is checked.
    pub fn parse(bytes: &[u8]) -> Result<Saved, Refusal> {
        let bad = |d: String| Refusal::new(Code::PlanInvalid, d);
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(Refusal::new(
                Code::LimitExceeded,
                "the saved plan exceeds 16 MiB",
            ));
        }
        let v: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| bad(format!("not valid JSON: {e}")))?;
        match v
            .pointer("/plan/schema")
            .and_then(serde_json::Value::as_i64)
        {
            Some(SCHEMA) => {}
            Some(n) => {
                return Err(Refusal::new(
                    Code::SchemaUnsupported,
                    format!("plan schema {n} is not supported; this build reads {SCHEMA}"),
                ));
            }
            None => return Err(bad("the saved plan has no integer plan.schema".into())),
        }
        let saved: Saved =
            serde_json::from_value(v).map_err(|e| bad(format!("not a saved plan: {e}")))?;
        if saved.plan.digest()? != saved.plan_digest {
            return Err(bad(
                "plan_digest does not match the plan content; the file was edited".into(),
            ));
        }
        Ok(saved)
    }
}

/// The precondition string for what was seen at one path. Apply observes the path again with
/// the same request and compares this string, so both sides must build it here. A sensitive
/// path carries no hash and no size.
pub fn node_state(seen: &Seen, sensitive: bool) -> String {
    let base = match &seen.node {
        Node::Missing {} => "missing".to_string(),
        Node::Dir { mode, .. } => format!("dir:{mode:o}"),
        Node::File {
            mode, size, sha256, ..
        } => match (sha256, sensitive) {
            (Some(h), false) => format!("file:{mode:o}:sha256:{h}"),
            (None, false) => format!("file:{mode:o}:size:{size}"),
            (_, true) => format!("file:{mode:o}"),
        },
        // The target text is compared exactly: a case-only retarget is a different link.
        Node::Link { target, to, .. } => format!("link:{target}:{}", to.name()),
        Node::Other { mode, .. } => format!("other:{mode:o}"),
        Node::Unknown { reason } => format!("unknown:{reason}"),
    };
    match &seen.alias {
        Some(a) => format!("{base} alias={a}"),
        None => base,
    }
}

/// The ledger fingerprint of a link: SHA-256 of `link\0` and the link target text. DFM-17 left
/// link fingerprints open; the planner and apply both use this function. A file's fingerprint is
/// the SHA-256 of its content. The prefix keeps the two apart: a regular file whose bytes equal
/// a link's target text has a different fingerprint, so it is never taken for that link, not
/// for ownership and not for a deletion (DFM-45). A ledger written before this prefix holds the
/// plain hash for its links. Such an entry no longer matches its live link, so the planner calls
/// it modified and nothing removes it until the link is applied or adopted again.
pub fn link_fingerprint(target: &str) -> String {
    let mut bytes = b"link\0".to_vec();
    bytes.extend_from_slice(target.as_bytes());
    sha256_hex(&bytes)
}

/// The identity of a source tree read from `root` (section 9, "Path-source identity"): the
/// canonical absolute root and the tree digest. Regular files and links only; the loader does
/// not record the executable bit, so a mode-only edit is not detected (docs/planner.md).
pub fn source_identity(root: &str, set: &crate::resolve::SourceSet) -> Source {
    use crate::digest::{EntryKind, TreeEntry, tree_digest};
    use crate::resolve::Entry;
    let entries = set
        .paths()
        .filter_map(|p| {
            let kind = match set.get(p)? {
                Entry::File(b) => EntryKind::File {
                    exec: false,
                    sha256: sha256_hex(b),
                },
                Entry::Link(t) => EntryKind::Symlink { target: t.clone() },
            };
            Some(TreeEntry {
                path: p.to_string(),
                kind,
            })
        })
        .collect();
    Source {
        kind: "path".into(),
        root: root.into(),
        tree: format!("sha256:{}", tree_digest(entries)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Plan {
        Plan {
            schema: SCHEMA,
            command: "plan".into(),
            scope: "config".into(),
            binary: Binary {
                version: "0".into(),
                sha256: "0".repeat(64),
            },
            profile: "/p".into(),
            machine_id: "m".into(),
            model_digest: "0".repeat(64),
            sources: vec![],
            observations: Observations::default(),
            operations: vec![],
            blockers: vec![],
        }
    }

    #[test]
    fn a_saved_plan_round_trips_and_detects_an_edit() {
        let p = empty();
        let bytes = p.saved().unwrap().to_json().unwrap();
        let back = Saved::parse(&bytes).unwrap();
        assert_eq!(back.plan, p);
        assert_eq!(back.plan_digest, p.digest().unwrap());
        let edited = String::from_utf8(bytes)
            .unwrap()
            .replace("\"/p\"", "\"/q\"");
        assert_eq!(
            Saved::parse(edited.as_bytes()).unwrap_err().code,
            Code::PlanInvalid
        );
        assert_eq!(Saved::parse(b"{}").unwrap_err().code, Code::PlanInvalid);
    }

    #[test]
    fn unknown_keys_are_refused_inside_the_plan() {
        let p = empty();
        let mut v = serde_json::to_value(p.saved().unwrap()).unwrap();
        v["plan"]["extra"] = 1.into();
        let e = Saved::parse(&serde_json::to_vec(&v).unwrap()).unwrap_err();
        assert_eq!(e.code, Code::PlanInvalid);
        assert!(e.detail.contains("extra"), "{}", e.detail);
    }

    #[test]
    fn the_digest_changes_with_any_field() {
        let a = empty();
        let mut b = a.clone();
        b.machine_id = "n".into();
        assert_ne!(a.digest().unwrap(), b.digest().unwrap());
        let mut c = a.clone();
        c.observations.seen.insert(
            "x".into(),
            Seen {
                node: Node::Missing {},
                alias: None,
            },
        );
        assert_ne!(a.digest().unwrap(), c.digest().unwrap());
    }

    #[test]
    fn an_output_hash_only_change_prints_a_visible_difference() {
        let op = |hash: &str| Operation {
            id: "r@x".into(),
            journal_id: "j".into(),
            resource: "r".into(),
            module: "m".into(),
            adapter: "git-config/v1".into(),
            class: Class::Normal,
            action: Action::CreateFile,
            status: Status::Planned,
            path: Some("x".into()),
            mode: None,
            link: None,
            privilege: "user".into(),
            effect: String::new(),
            ownership: Ownership::Absent,
            inputs: vec![],
            fragments: vec![],
            fragment_dir: None,
            output_sha256: Some(hash.into()),
            managed_sha256: None,
            takeover: false,
            output_bytes: None,
            excluded: None,
            sets: vec![],
            after: vec![],
            preconditions: vec![],
            postcondition: String::new(),
        };
        let (mut a, mut b) = (empty(), empty());
        a.operations = vec![op(&"a".repeat(64))];
        b.operations = vec![op(&"b".repeat(64))];
        let d = stale(&a, &b).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].what, StaleKind::Operation);
        assert!(d[0].subject.ends_with(" output"), "{:?}", d[0]);
        assert_eq!(d[0].now, "output changed");
        // A hash is never printed.
        assert!(!format!("{d:?}").contains(&"a".repeat(12)), "{d:?}");
        assert!(!format!("{d:?}").contains(&"b".repeat(12)), "{d:?}");

        // A fragment hash change names the fragment and nothing else.
        let frag = |h: &str| Fragment {
            name: "10-a.md".into(),
            origin: "foreign".into(),
            sha256: Some(h.into()),
        };
        let (mut c, mut e) = (a.clone(), a.clone());
        c.operations[0].fragments = vec![frag(&"c".repeat(64))];
        e.operations[0].fragments = vec![frag(&"d".repeat(64))];
        let d = stale(&c, &e).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].now, "fragment 10-a.md changed");
        assert!(!format!("{d:?}").contains(&"c".repeat(12)), "{d:?}");
    }

    #[test]
    fn a_link_fingerprint_is_never_the_hash_of_a_file_with_the_same_bytes() {
        assert_ne!(link_fingerprint("/src/x"), sha256_hex(b"/src/x"));
        assert_eq!(link_fingerprint("/src/x"), link_fingerprint("/src/x"));
    }

    #[test]
    fn node_state_hides_what_a_sensitive_path_must_not_show() {
        let file = |sha: Option<&str>| Seen {
            node: Node::File {
                mode: 0o600,
                own: true,
                size: 9,
                sha256: sha.map(String::from),
            },
            alias: None,
        };
        assert_eq!(node_state(&file(Some("ab")), false), "file:600:sha256:ab");
        assert_eq!(node_state(&file(None), false), "file:600:size:9");
        assert_eq!(node_state(&file(Some("ab")), true), "file:600");
    }
}
