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

pub use build::{Context, Owned, build, followups, requests};
pub use stale::{Stale, stale};

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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Missing,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub node: Node,
    /// The spelling on disk when it differs from the requested one only by case. The target
    /// filesystem folds case, so the two names are one entry.
    pub alias: Option<String>,
}

/// The names in one directory. `truncated` means the bound was reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub names: Vec<String>,
    pub truncated: bool,
}

/// Every local fact the plan rests on, keyed by HOME-relative path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observations {
    pub seen: BTreeMap<String, Seen>,
    pub listings: BTreeMap<String, Listing>,
}

/// One path to observe. The caller observes every proper prefix of `path` too, stopping at the
/// first one that is not a directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Request {
    pub path: String,
    /// Record the content hash of a regular file at `path`.
    pub hash: bool,
    /// List the entries when `path` is a directory.
    pub list: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binary {
    pub version: String,
    pub sha256: String,
}

/// A source tree the plan was made from: `kind = "path"` identity (section 9): the canonical
/// absolute root plus the tree digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Nothing to do, or out of scope.
    None,
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
pub struct Input {
    /// Path of the source file in the source tree.
    pub path: String,
    pub content_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fragment {
    pub name: String,
    /// `selected:<resource>` or `foreign`.
    pub origin: String,
}

/// A fact that must still hold at apply time: `subject` names it, `state` is its fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Precondition {
    pub subject: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub resource: String,
    pub module: String,
    pub adapter: String,
    pub class: Class,
    pub action: Action,
    pub status: Status,
    pub path: Option<String>,
    pub mode: Option<u32>,
    /// Always `user`: a HOME adapter never escalates (section 8).
    pub privilege: String,
    /// What the operation does, without content or secret values.
    pub effect: String,
    pub ownership: Ownership,
    pub inputs: Vec<Input>,
    pub fragments: Vec<Fragment>,
    /// Operation ids that must finish first.
    pub after: Vec<String>,
    pub preconditions: Vec<Precondition>,
    pub postcondition: String,
}

/// Why a plan cannot be approved: the source, resource and field, the conflicting claim or
/// failed precondition, and one correction. It never carries a secret value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn digest(&self) -> String {
        // Serializing owned data to a JSON value cannot fail for these types.
        sha256_hex(&canonical(self).expect("a plan serializes"))
    }

    pub fn saved(&self) -> Saved {
        Saved {
            plan_digest: self.digest(),
            plan: self.clone(),
            isolated: None,
        }
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
        if saved.plan.digest() != saved.plan_digest {
            return Err(bad(
                "plan_digest does not match the plan content; the file was edited".into(),
            ));
        }
        Ok(saved)
    }
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
    fn every_code_round_trips_through_its_name() {
        for c in Code::ALL {
            assert_eq!(Code::parse(c.as_str()), Some(*c));
        }
        assert_eq!(Code::parse("E_NOPE"), None);
    }

    #[test]
    fn a_saved_plan_round_trips_and_detects_an_edit() {
        let p = empty();
        let bytes = p.saved().to_json().unwrap();
        let back = Saved::parse(&bytes).unwrap();
        assert_eq!(back.plan, p);
        assert_eq!(back.plan_digest, p.digest());
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
    fn the_digest_changes_with_any_field() {
        let a = empty();
        let mut b = a.clone();
        b.machine_id = "n".into();
        assert_ne!(a.digest(), b.digest());
        let mut c = a.clone();
        c.observations.seen.insert(
            "x".into(),
            Seen {
                node: Node::Missing,
                alias: None,
            },
        );
        assert_ne!(a.digest(), c.digest());
    }
}
