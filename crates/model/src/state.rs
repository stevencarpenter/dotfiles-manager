//! Versioned machine-written state documents (docs/architecture.md sections 4 and 5, docs/state.md).
//!
//! Pure data: parsing takes already-read bytes. Every document has an integer `schema` that is
//! checked before any other field, and unknown keys are refused.

use crate::binding::Binding;
use crate::fold::fold;
use crate::{Code, Refusal};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};

pub const SCHEMA: i64 = 1;
/// Largest JSON state file (section 5, parser and read limits).
pub const MAX_BYTES: usize = 16 << 20;
/// Longest journal line. A longer line is a journal error, not a skipped line.
pub const MAX_JOURNAL_LINE: usize = 64 << 10;
const MAX_DEPTH: usize = 32;
/// Provider text kept in a receipt or journal entry (section 8, secrets item 3).
pub const MAX_ERROR_BYTES: usize = 4096;
/// Largest ledger, receipt or trial document the manager writes. The read cap is larger, so a
/// document the manager wrote is always readable and a hand-made huge one is still refused.
pub const MAX_WRITE_BYTES: usize = 4 << 20;
/// Largest total size of the journal. An append that would pass it is refused before writing.
pub const MAX_JOURNAL_BYTES: u64 = 8 << 20;

fn bad(d: impl Into<String>) -> Refusal {
    Refusal::new(Code::StateInvalid, d)
}

/// Valid id for operations, trials and resources: lowercase ASCII, 1 to 64 characters.
pub fn valid_id(id: &str) -> bool {
    crate::binding::valid_machine_id(id)
}

fn valid_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// An absolute path with no `.` or `..` component, no empty component and no control character.
pub fn valid_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() > 1
        && !p.ends_with('/')
        && !p.chars().any(char::is_control)
        && p[1..]
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

fn depth(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        serde_json::Value::Object(o) => 1 + o.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Strict parse. Order: size, JSON, depth, schema, then the typed fields.
pub fn parse<T: DeserializeOwned>(kind: &str, bytes: &[u8]) -> Result<T, Refusal> {
    if bytes.len() > MAX_BYTES {
        return Err(Refusal::new(
            Code::LimitExceeded,
            format!("{kind} exceeds 16 MiB"),
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| bad(format!("{kind} is not valid JSON: {e}")))?;
    if depth(&v) > MAX_DEPTH {
        return Err(bad(format!("{kind} nests deeper than {MAX_DEPTH}")));
    }
    match v.get("schema").and_then(serde_json::Value::as_i64) {
        Some(SCHEMA) => {}
        Some(n) if n > SCHEMA => {
            return Err(Refusal::new(
                Code::StateNewer,
                format!("{kind} schema {n} is newer than this binary supports (supported: 1)"),
            ));
        }
        Some(n) => {
            return Err(Refusal::new(
                Code::SchemaUnsupported,
                format!("{kind} schema {n} is not supported (supported: 1)"),
            ));
        }
        None => return Err(bad(format!("{kind} has no integer schema"))),
    }
    serde_json::from_value(v).map_err(|e| bad(format!("{kind}: {e}")))
}

/// Canonical form: UTF-8, sorted keys, no insignificant whitespace.
pub fn canonical<T: Serialize>(doc: &T) -> Result<Vec<u8>, Refusal> {
    // `Value` keeps objects in a sorted map (serde_json without `preserve_order`).
    serde_json::to_value(doc)
        .and_then(|v| serde_json::to_vec(&v))
        .map_err(|e| bad(format!("cannot serialize: {e}")))
}

// ---- identity ---------------------------------------------------------------------------------

/// The binding identity a record was written under. Identity is profile plus machine id;
/// the binding digest is audit evidence and is not compared, because an authored binding edit
/// (for example its origins) changes the digest without changing the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stamp {
    pub profile: String,
    pub machine_id: String,
    pub binding_digest: String,
}

impl Stamp {
    pub fn of(b: &Binding) -> Stamp {
        Stamp {
            profile: b.profile().into(),
            machine_id: b.machine_id().into(),
            binding_digest: b.digest(),
        }
    }

    pub fn matches(&self, b: &Binding) -> bool {
        self.profile == b.profile() && self.machine_id == b.machine_id()
    }

    pub fn same_identity(&self, other: &Stamp) -> bool {
        self.profile == other.profile && self.machine_id == other.machine_id
    }

    fn validate(&self) -> Result<(), Refusal> {
        Binding::new(&self.profile, &self.machine_id).map_err(|r| bad(r.detail))?;
        if !valid_sha256(&self.binding_digest) {
            return Err(bad("binding_digest is not a SHA-256"));
        }
        Ok(())
    }
}

/// `state/state.json`: records which identity owns this state directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateDoc {
    pub schema: i64,
    pub binding: Stamp,
    pub created_unix: u64,
}

impl StateDoc {
    pub fn new(binding: Stamp, created_unix: u64) -> StateDoc {
        StateDoc {
            schema: SCHEMA,
            binding,
            created_unix,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<StateDoc, Refusal> {
        let d: StateDoc = parse("state.json", bytes)?;
        d.binding.validate()?;
        Ok(d)
    }
}

// ---- ledger -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Normal,
    Sensitive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedOutput {
    pub path: String,
    pub mode: u32,
    /// SHA-256 of the content the manager wrote. Absent for sensitive resources.
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerResource {
    pub resource: String,
    pub adapter: String,
    pub class: Class,
    /// The operation that last wrote these outputs.
    pub op_id: String,
    pub owned_outputs: Vec<OwnedOutput>,
}

/// `state/ledger.json`: the only source of delete authority (section 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    pub schema: i64,
    pub binding: Stamp,
    pub resources: Vec<LedgerResource>,
}

impl Ledger {
    pub fn empty(binding: Stamp) -> Ledger {
        Ledger {
            schema: SCHEMA,
            binding,
            resources: Vec::new(),
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Ledger, Refusal> {
        let l: Ledger = parse("ledger.json", bytes)?;
        l.validate()?;
        Ok(l)
    }

    /// Exact ownership: unique resources, unique output paths across all resources (two owners
    /// for one path is a conflict), normalized absolute paths, and no content hash for a
    /// sensitive resource.
    pub fn validate(&self) -> Result<(), Refusal> {
        self.binding.validate()?;
        let mut resources = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for r in &self.resources {
            if !valid_id(&r.op_id) || r.resource.is_empty() || r.adapter.is_empty() {
                return Err(bad(format!(
                    "ledger resource {:?} is malformed",
                    r.resource
                )));
            }
            if !resources.insert(&r.resource) {
                return Err(bad(format!("ledger lists {} twice", r.resource)));
            }
            for o in &r.owned_outputs {
                if !valid_path(&o.path) {
                    return Err(bad(format!("ledger path {:?} is not normalized", o.path)));
                }
                if !paths.insert(&o.path) {
                    return Err(bad(format!("ledger gives {} two owners", o.path)));
                }
                match (r.class, &o.fingerprint) {
                    (Class::Normal, Some(f)) if valid_sha256(f) => {}
                    (Class::Sensitive, None) => {}
                    _ => {
                        return Err(bad(format!(
                            "ledger fingerprint for {} does not fit its class",
                            o.path
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Delete authority for one exact output. It needs a ledger entry for this resource and path
    /// whose fingerprint equals the live one. A mismatch is a conflict and authorizes nothing,
    /// and so does a sensitive resource (its salted comparison is not implemented).
    pub fn authorizes_delete(&self, resource: &str, path: &str, live_sha256: &str) -> bool {
        self.resources
            .iter()
            .filter(|r| r.resource == resource && r.class == Class::Normal)
            .flat_map(|r| &r.owned_outputs)
            .any(|o| o.path == path && o.fingerprint.as_deref() == Some(live_sha256))
    }

    /// Replaces or adds one resource entry. Refuses an output another resource already owns.
    pub fn record(&mut self, entry: LedgerResource) -> Result<(), Refusal> {
        let mut next = self.clone();
        next.resources.retain(|r| r.resource != entry.resource);
        next.resources.push(entry);
        next.resources.sort_by(|a, b| a.resource.cmp(&b.resource));
        next.validate()?;
        *self = next;
        Ok(())
    }
}

// ---- receipts ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Updated,
    Current,
    Held,
    Pending,
    Failed,
    Blocked,
    BlockedPending,
    NotStarted,
    NotInScope,
    Unknown,
}

impl Status {
    pub const ALL: [Status; 10] = [
        Status::Updated,
        Status::Current,
        Status::Held,
        Status::Pending,
        Status::Failed,
        Status::Blocked,
        Status::BlockedPending,
        Status::NotStarted,
        Status::NotInScope,
        Status::Unknown,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fingerprint {
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub id: String,
    pub resource: String,
    pub adapter: String,
    /// Sensitivity class. A sensitive operation carries no `before` or `after`.
    pub class: Class,
    pub status: Status,
    /// Absent only for an operation that never ran (`not_started`, `not_in_scope`).
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub before: Option<Fingerprint>,
    pub after: Option<Fingerprint>,
    pub owned_outputs: Vec<String>,
    pub error: Option<String>,
    /// What happened to the consumer of a published file, recorded apart from the publication
    /// (architecture section 8: a correct file and a reloaded consumer are separate
    /// postconditions). One of [`RELOADS`]. Absent when the operation published nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload: Option<String>,
}

/// The consumer reload states the manager can record. Only `not_attempted` exists until a
/// `Reloader` does (architecture section 11 names `requested`, `reloaded`, `unsupported` and
/// `failed` for it). A state nothing can write is refused, so a receipt cannot claim one.
pub const RELOADS: [&str; 1] = ["not_attempted"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryId {
    pub version: String,
    pub commit: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRef {
    pub id: String,
    pub binding_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub kind: String,
    pub plan_digest: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceId {
    pub id: String,
    /// Absent for a local path source, which has no commit. The `tree` digest identifies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub tree: String,
}

/// One approved disruptive operation or adoption (architecture section 5, `approvals[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisruptiveApproval {
    /// The `id` of the receipt operation that ran with this approval.
    pub operation_id: String,
    /// `allow_disruptive` or `allow_adopt` (DFM-45). The contract also names `interactive`,
    /// which needs a prompt that does not exist yet.
    pub how: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    pub outcome: String,
    pub exit_code: u8,
    pub counts: BTreeMap<Status, u32>,
}

/// `state/receipts/<utc-timestamp>-<op-id>.json`: audit evidence, immutable, never authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema: i64,
    pub op_id: String,
    pub command: String,
    pub started_at: String,
    pub finished_at: String,
    pub binary: BinaryId,
    pub profile: ProfileRef,
    pub machine_id: String,
    pub plan_digest: Option<String>,
    pub approval: Option<Approval>,
    pub sources: Vec<SourceId>,
    /// One entry per disruptive operation or adoption that ran with an approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<DisruptiveApproval>,
    pub operations: Vec<OperationRecord>,
    pub result: RunResult,
}

/// One count per status; their sum is the number of participants.
pub fn count_statuses(ops: &[OperationRecord]) -> BTreeMap<Status, u32> {
    let mut counts: BTreeMap<Status, u32> = Status::ALL.iter().map(|s| (*s, 0)).collect();
    for o in ops {
        *counts.entry(o.status).or_insert(0) += 1;
    }
    counts
}

impl Receipt {
    pub fn parse(bytes: &[u8]) -> Result<Receipt, Refusal> {
        let r: Receipt = parse("receipt", bytes)?;
        r.validate()?;
        Ok(r)
    }

    pub fn stamp(&self) -> Stamp {
        Stamp {
            profile: self.profile.id.clone(),
            machine_id: self.machine_id.clone(),
            binding_digest: self.profile.binding_digest.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), Refusal> {
        if !valid_id(&self.op_id) || self.command.is_empty() {
            return Err(bad("receipt op_id or command is malformed"));
        }
        self.stamp().validate()?;
        for t in [&self.started_at, &self.finished_at] {
            if parse_rfc3339(t).is_none() {
                return Err(bad(format!(
                    "receipt time {t:?} is not YYYY-MM-DDTHH:MM:SSZ"
                )));
            }
        }
        if self.result.counts != count_statuses(&self.operations) {
            return Err(bad("receipt counts do not match its operations"));
        }
        self.validate_fields()?;
        for o in &self.operations {
            if o.class == Class::Sensitive && (o.before.is_some() || o.after.is_some()) {
                return Err(bad(format!(
                    "sensitive operation {} carries a fingerprint",
                    o.id
                )));
            }
            for f in [&o.before, &o.after].into_iter().flatten() {
                if !valid_sha256(&f.fingerprint) {
                    return Err(bad(format!("operation {} fingerprint is malformed", o.id)));
                }
            }
            if o.owned_outputs.iter().any(|p| !valid_path(p)) {
                return Err(bad(format!("operation {} output path is malformed", o.id)));
            }
            if o.error.as_ref().is_some_and(|e| e.len() > MAX_ERROR_BYTES) {
                return Err(bad(format!("operation {} error is not bounded", o.id)));
            }
            if o.reload.as_deref().is_some_and(|r| !RELOADS.contains(&r)) {
                return Err(bad(format!("operation {} reload state is unknown", o.id)));
            }
        }
        Ok(())
    }

    /// The fields that carry no free text: each must have its exact shape, so nothing can be
    /// hidden in them.
    fn validate_fields(&self) -> Result<(), Refusal> {
        let plain =
            |t: &str| !t.is_empty() && t.len() <= 64 && t.chars().all(|c| c.is_ascii_graphic());
        let hex_id =
            |t: &str| matches!(t.len(), 40 | 64) && t.bytes().all(|c| c.is_ascii_hexdigit());
        if !plain(&self.binary.version) || !plain(&self.binary.commit) {
            return Err(bad("receipt binary version or commit is malformed"));
        }
        if !valid_sha256(&self.binary.sha256) {
            return Err(bad("receipt binary sha256 is malformed"));
        }
        if self
            .plan_digest
            .as_deref()
            .is_some_and(|d| !valid_sha256(d))
        {
            return Err(bad("receipt plan_digest is malformed"));
        }
        if let Some(a) = &self.approval
            && (!matches!(a.kind.as_str(), "interactive" | "yes_plan")
                || !valid_sha256(&a.plan_digest)
                || parse_rfc3339(&a.at).is_none())
        {
            return Err(bad("receipt approval is malformed"));
        }
        for src in &self.sources {
            let tree_ok = src.tree.strip_prefix("sha256:").is_some_and(valid_sha256);
            let commit_ok = src.commit.as_deref().is_none_or(hex_id);
            if !valid_id(&src.id) || !commit_ok || !tree_ok {
                return Err(bad("receipt source identity is malformed"));
            }
        }
        for a in &self.approvals {
            let op = self.operations.iter().find(|o| o.id == a.operation_id);
            // The receipt keeps no action, so the approval must fit the shape of its operation.
            // A removal publishes and owns nothing. An adoption that ran owns its link and has
            // no `before` and no `reload`. A failed or blocked operation has neither shape, so
            // only its `how` is checked.
            let fits = op.is_some_and(|o| match a.how.as_str() {
                // A removal that ran records the fingerprint it removed as `before`.
                "allow_disruptive" => {
                    o.owned_outputs.is_empty()
                        && o.after.is_none()
                        && (o.status != Status::Updated || o.before.is_some())
                }
                "allow_adopt" => {
                    o.reload.is_none()
                        && o.before.is_none()
                        && (o.status != Status::Updated
                            || (o.after.is_some() && !o.owned_outputs.is_empty()))
                }
                _ => false,
            });
            if !fits || parse_rfc3339(&a.at).is_none() {
                return Err(bad("receipt approvals[] entry is malformed"));
            }
        }
        let exit_name = match self.result.exit_code {
            0 => "ok",
            1 => "failed",
            2 => "usage",
            3 => "blocked",
            4 => "declined",
            5 => "pending",
            130 => "interrupted",
            143 => "terminated",
            _ => return Err(bad("receipt exit_code is not a contract code")),
        };
        if self.result.outcome != exit_name {
            return Err(bad("receipt outcome does not match its exit_code"));
        }
        for o in &self.operations {
            let named =
                |t: &str| !t.is_empty() && t.len() <= 256 && !t.chars().any(char::is_control);
            if !valid_id(&o.id) || !named(&o.resource) || !named(&o.adapter) {
                return Err(bad(format!("operation {} has a malformed name", o.id)));
            }
            let never_ran = matches!(o.status, Status::NotStarted | Status::NotInScope);
            for t in [&o.started_at, &o.finished_at] {
                let ok = match t {
                    Some(t) => parse_rfc3339(t).is_some(),
                    None => never_ran,
                };
                if !ok {
                    return Err(bad(format!(
                        "operation {} time is missing or malformed",
                        o.id
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn file_name(&self) -> Option<String> {
        Some(format!(
            "{}-{}.json",
            compact_utc(parse_rfc3339(&self.started_at)?),
            self.op_id
        ))
    }
}

// ---- journal ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Intent,
    Done,
    Failed,
    Blocked,
    Pending,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryOutput {
    pub path: String,
    /// Whether the path existed before the operation. A pre-existing path is never claimed.
    pub existed_before: bool,
    /// Expected content SHA-256. Absent for a sensitive operation.
    pub expected_sha256: Option<String>,
}

/// What a later run needs to decide whether an interrupted operation finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub outputs: Vec<RecoveryOutput>,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub schema: i64,
    pub op_id: String,
    pub op: String,
    pub phase: Phase,
    pub at: String,
    pub resource: String,
    pub adapter: String,
    pub class: Class,
    pub recovery: Option<Recovery>,
    pub token: Option<String>,
    pub reason: Option<String>,
}

impl JournalEntry {
    pub fn validate(&self) -> Result<(), Refusal> {
        if self.schema != SCHEMA {
            return Err(bad("journal entry schema"));
        }
        if !valid_id(&self.op_id) || !valid_id(&self.op) || parse_rfc3339(&self.at).is_none() {
            return Err(bad("journal entry ids or time are malformed"));
        }
        if self.phase == Phase::Intent && self.recovery.is_none() {
            return Err(bad("an intent entry needs recovery metadata"));
        }
        for o in self.recovery.iter().flat_map(|r| &r.outputs) {
            let hash_ok = match (self.class, &o.expected_sha256) {
                (Class::Sensitive, h) => h.is_none(),
                (Class::Normal, h) => h.as_deref().is_none_or(valid_sha256),
            };
            if !valid_path(&o.path) || !hash_ok {
                return Err(bad(format!("recovery output {:?} is malformed", o.path)));
            }
        }
        if self
            .reason
            .as_ref()
            .is_some_and(|r| r.len() > MAX_ERROR_BYTES)
        {
            return Err(bad("journal reason is not bounded"));
        }
        Ok(())
    }

    /// One line of `journal.jsonl`, with the trailing newline.
    pub fn to_line(&self) -> Result<Vec<u8>, Refusal> {
        self.validate()?;
        let mut l = canonical(self)?;
        l.push(b'\n');
        if l.len() > MAX_JOURNAL_LINE {
            return Err(Refusal::new(
                Code::LimitExceeded,
                "journal entry exceeds 64 KiB",
            ));
        }
        Ok(l)
    }
}

/// Parses the whole journal. A bad, oversize or unterminated (torn) line is an error naming the
/// line; nothing is skipped.
pub fn parse_journal(bytes: &[u8]) -> Result<Vec<JournalEntry>, Refusal> {
    if bytes.len() > MAX_BYTES {
        return Err(Refusal::new(Code::LimitExceeded, "journal exceeds 16 MiB"));
    }
    let mut out = Vec::new();
    let mut rest = bytes;
    let mut n = 0;
    while !rest.is_empty() {
        n += 1;
        let Some(end) = rest.iter().position(|b| *b == b'\n') else {
            return Err(bad(format!("journal line {n} is not terminated")));
        };
        let (line, tail) = rest.split_at(end);
        rest = &tail[1..];
        if line.len() + 1 > MAX_JOURNAL_LINE {
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!("journal line {n} exceeds 64 KiB"),
            ));
        }
        let e: JournalEntry = parse("journal entry", line)
            .map_err(|r| Refusal::new(r.code, format!("line {n}: {}", r.detail)))?;
        e.validate()
            .map_err(|r| Refusal::new(r.code, format!("line {n}: {}", r.detail)))?;
        out.push(e);
    }
    Ok(out)
}

/// Intent entries with no later terminal entry for the same operation: interruption records.
pub fn unfinished(entries: &[JournalEntry]) -> Vec<&JournalEntry> {
    entries
        .iter()
        .enumerate()
        .filter(|(i, e)| {
            e.phase == Phase::Intent
                && !entries[i + 1..]
                    .iter()
                    .any(|t| t.phase != Phase::Intent && t.op_id == e.op_id && t.op == e.op)
        })
        .map(|(_, e)| e)
        .collect()
}

// ---- trials -----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselinePath {
    pub path: String,
    pub existed: bool,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub tool_preexisting: bool,
    pub paths: Vec<BaselinePath>,
}

/// `state/trials/<id>.json`: an active trial and its recoverable baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialRecord {
    pub schema: i64,
    pub binding: Stamp,
    pub trial_id: String,
    pub tool: String,
    pub created_unix: u64,
    pub baseline: Baseline,
    pub owned_outputs: Vec<String>,
}

impl TrialRecord {
    pub fn parse(bytes: &[u8]) -> Result<TrialRecord, Refusal> {
        let t: TrialRecord = parse("trial", bytes)?;
        t.validate()?;
        Ok(t)
    }

    pub fn validate(&self) -> Result<(), Refusal> {
        self.binding.validate()?;
        if !valid_id(&self.trial_id) || !valid_id(&self.tool) {
            return Err(bad("trial id or tool is malformed"));
        }
        let paths_ok = self
            .baseline
            .paths
            .iter()
            .all(|p| valid_path(&p.path) && p.sha256.as_deref().is_none_or(valid_sha256))
            && self.owned_outputs.iter().all(|p| valid_path(p));
        if paths_ok {
            Ok(())
        } else {
            Err(bad("trial paths are malformed"))
        }
    }
}

// ---- redaction and time -----------------------------------------------------------------------

fn hidden(c: char) -> bool {
    // Bidirectional overrides and isolates, zero-width characters and the byte order mark can
    // hide or reorder text on a terminal.
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// True when any known secret value occurs in `text`.
pub fn leaks(text: &str, secrets: &[&str]) -> bool {
    secrets.iter().any(|s| !s.is_empty() && text.contains(s))
}

/// Text for a receipt, journal or log: every occurrence of a known secret value becomes
/// `[redacted]` (overlapping and adjacent occurrences are merged, so no part of a longer match
/// survives), control, bidirectional and zero-width characters become visible escapes, and the
/// length is bounded.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut ranges: Vec<(usize, usize)> = secrets
        .iter()
        .filter(|s| !s.is_empty())
        // Every start position, so a match that overlaps itself ("aaaa" in "xaaaaaax") is found.
        .flat_map(|s| {
            text.char_indices()
                .filter(move |(i, _)| text[*i..].starts_with(s))
                .map(move |(i, _)| (i, i + s.len()))
        })
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut t = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in merged {
        t.push_str(&text[at..start]);
        t.push_str("[redacted]");
        at = end;
    }
    t.push_str(&text[at..]);
    let mut out = String::new();
    for c in t.chars() {
        let piece: String = if c.is_control() || hidden(c) {
            c.escape_default().collect()
        } else {
            c.to_string()
        };
        if out.len() + piece.len() > MAX_ERROR_BYTES {
            break;
        }
        out.push_str(&piece);
    }
    out
}

fn secret_in_path() -> Refusal {
    // The value is not repeated: this text is printed.
    bad("a path or identifier contains a known secret; it is refused, not redacted")
}

impl Receipt {
    /// Redacts descriptive text with `secrets` in one pass. A path or identifier that contains
    /// a secret cannot be redacted without breaking its use, so it is refused.
    pub fn scrub(&mut self, secrets: &[&str]) -> Result<(), Refusal> {
        if leaks(&self.op_id, secrets) {
            return Err(secret_in_path());
        }
        for o in &self.operations {
            let ids = [&o.id, &o.resource, &o.adapter];
            if ids.iter().any(|t| leaks(t, secrets))
                || o.owned_outputs.iter().any(|p| leaks(p, secrets))
            {
                return Err(secret_in_path());
            }
        }
        let red = |t: &mut String| *t = redact(t, secrets);
        red(&mut self.command);
        red(&mut self.binary.version);
        red(&mut self.binary.commit);
        if let Some(a) = &mut self.approval {
            red(&mut a.kind);
        }
        for o in &mut self.operations {
            if let Some(e) = &mut o.error {
                red(e);
            }
        }
        Ok(())
    }
}

impl JournalEntry {
    /// Redacts descriptive text with `secrets`. Identifiers and recovery paths that contain a
    /// secret are refused.
    pub fn scrub(&mut self, secrets: &[&str]) -> Result<(), Refusal> {
        let paths = self
            .recovery
            .iter()
            .flat_map(|r| &r.outputs)
            .map(|o| &o.path);
        let ids = [&self.op_id, &self.op, &self.resource, &self.adapter];
        if ids.iter().any(|t| leaks(t, secrets)) || paths.into_iter().any(|p| leaks(p, secrets)) {
            return Err(secret_in_path());
        }
        for t in [&mut self.token, &mut self.reason].into_iter().flatten() {
            *t = redact(t, secrets);
        }
        Ok(())
    }
}

impl Ledger {
    /// A ledger key or path that contains a known secret is refused.
    pub fn check_secrets(&self, secrets: &[&str]) -> Result<(), Refusal> {
        let bad_text = self.resources.iter().any(|r| {
            leaks(&r.resource, secrets)
                || leaks(&r.adapter, secrets)
                || r.owned_outputs.iter().any(|o| leaks(&o.path, secrets))
        });
        if bad_text {
            Err(secret_in_path())
        } else {
            Ok(())
        }
    }
}

impl TrialRecord {
    /// A trial id, tool or path that contains a known secret is refused.
    pub fn check_secrets(&self, secrets: &[&str]) -> Result<(), Refusal> {
        let paths = self
            .baseline
            .paths
            .iter()
            .map(|p| &p.path)
            .chain(&self.owned_outputs);
        let hit = [&self.trial_id, &self.tool]
            .iter()
            .any(|t| leaks(t, secrets))
            || paths.into_iter().any(|p| leaks(p, secrets));
        if hit { Err(secret_in_path()) } else { Ok(()) }
    }
}

// ---- scope ------------------------------------------------------------------------------------

/// Where a claim may point: strictly inside HOME, and neither inside, equal to, nor an ancestor
/// of a reserved manager path (state, config, lock, data, cache). Comparison is case-folded,
/// because APFS is case-insensitive by default, so it also holds when the reserved directory
/// does not exist yet. Unicode normalization differences are not folded (a known limit).
#[derive(Debug, Clone)]
pub struct Scope {
    home: String,
    reserved: Vec<String>,
}

impl Scope {
    pub fn new(home: &str, reserved: &[String]) -> Scope {
        Scope {
            home: fold(home),
            reserved: reserved.iter().map(|r| fold(r)).collect(),
        }
    }

    pub fn admits(&self, path: &str) -> bool {
        if !valid_path(path) {
            return false;
        }
        let p = fold(path);
        let inside = p
            .strip_prefix(&self.home)
            .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1);
        inside
            && !self.reserved.iter().any(|r| {
                p == *r || p.starts_with(&format!("{r}/")) || r.starts_with(&format!("{p}/"))
            })
    }

    fn check<'a>(&self, paths: impl IntoIterator<Item = &'a String>) -> Result<(), Refusal> {
        match paths.into_iter().find(|p| !self.admits(p)) {
            None => Ok(()),
            Some(p) => Err(bad(format!(
                "{p} is outside HOME or overlaps the manager's own paths"
            ))),
        }
    }
}

impl Ledger {
    /// Containment of every owned output. Applied on read as well as on write.
    pub fn validate_within(&self, scope: &Scope) -> Result<(), Refusal> {
        scope.check(
            self.resources
                .iter()
                .flat_map(|r| &r.owned_outputs)
                .map(|o| &o.path),
        )
    }
}

impl Receipt {
    pub fn validate_within(&self, scope: &Scope) -> Result<(), Refusal> {
        scope.check(self.operations.iter().flat_map(|o| &o.owned_outputs))
    }
}

impl TrialRecord {
    pub fn validate_within(&self, scope: &Scope) -> Result<(), Refusal> {
        scope.check(
            self.baseline
                .paths
                .iter()
                .map(|p| &p.path)
                .chain(&self.owned_outputs),
        )
    }
}

impl JournalEntry {
    pub fn validate_within(&self, scope: &Scope) -> Result<(), Refusal> {
        scope.check(
            self.recovery
                .iter()
                .flat_map(|r| &r.outputs)
                .map(|o| &o.path),
        )
    }
}

// ---- time -------------------------------------------------------------------------------------

/// Days since 1970-01-01 to a civil date (proleptic Gregorian).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn rfc3339(unix: u64) -> String {
    let (days, secs) = ((unix / 86_400) as i64, unix % 86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// `20261010T120000Z`, used in receipt file names.
pub fn compact_utc(unix: u64) -> String {
    rfc3339(unix).replace(['-', ':'], "")
}

/// Parses exactly `YYYY-MM-DDTHH:MM:SSZ`.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let shape = b.len() == 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z';
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    if !shape {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    // Reject dates that do not round-trip (for example February 30).
    let t = u64::try_from(t).ok()?;
    (rfc3339(t) == s).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> Stamp {
        Stamp::of(&Binding::new("/p/profile.toml", "m5").unwrap())
    }

    fn op(status: Status, class: Class) -> OperationRecord {
        OperationRecord {
            id: "op1".into(),
            resource: "core.git".into(),
            adapter: "fs-link/v1".into(),
            class,
            status,
            started_at: Some("2026-10-10T00:00:00Z".into()),
            finished_at: Some("2026-10-10T00:00:01Z".into()),
            before: None,
            after: None,
            owned_outputs: vec!["/h/.gitconfig".into()],
            error: None,
            reload: None,
        }
    }

    pub(crate) fn receipt(ops: Vec<OperationRecord>) -> Receipt {
        let s = stamp();
        Receipt {
            schema: SCHEMA,
            op_id: "run-1".into(),
            command: "sync".into(),
            started_at: "2026-10-10T00:00:00Z".into(),
            finished_at: "2026-10-10T00:00:02Z".into(),
            binary: BinaryId {
                version: "0.1.0".into(),
                commit: "unknown".into(),
                sha256: "0".repeat(64),
            },
            profile: ProfileRef {
                id: s.profile.clone(),
                binding_digest: s.binding_digest.clone(),
            },
            machine_id: s.machine_id,
            plan_digest: None,
            approval: None,
            sources: vec![],
            approvals: vec![],
            result: RunResult {
                outcome: "ok".into(),
                exit_code: 0,
                counts: count_statuses(&ops),
            },
            operations: ops,
        }
    }

    #[test]
    fn time_round_trips_and_rejects_impossible_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_791_590_400), "2026-10-10T00:00:00Z");
        assert_eq!(parse_rfc3339("2026-10-10T00:00:00Z"), Some(1_791_590_400));
        assert_eq!(parse_rfc3339("2026-02-30T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-10-10 00:00:00Z"), None);
        assert_eq!(compact_utc(1_791_590_400), "20261010T000000Z");
    }

    #[test]
    fn schema_is_checked_before_any_other_field() {
        let code = |s: &str| StateDoc::parse(s.as_bytes()).unwrap_err().code;
        assert_eq!(code("{\"schema\":2,\"junk\":1}"), Code::StateNewer);
        assert_eq!(code("{\"schema\":0}"), Code::SchemaUnsupported);
        assert_eq!(code("{\"junk\":1}"), Code::StateInvalid);
        assert_eq!(code("not json"), Code::StateInvalid);
        assert_eq!(code("{\"schema\":1,\"junk\":1}"), Code::StateInvalid);
        let deep = format!(
            "{{\"schema\":1,\"x\":{}1{}}}",
            "[".repeat(40),
            "]".repeat(40)
        );
        assert_eq!(code(&deep), Code::StateInvalid);
        let big = vec![b' '; MAX_BYTES + 1];
        assert_eq!(StateDoc::parse(&big).unwrap_err().code, Code::LimitExceeded);
    }

    #[test]
    fn documents_round_trip_in_canonical_form() {
        let doc = StateDoc::new(stamp(), 7);
        let bytes = canonical(&doc).unwrap();
        assert_eq!(StateDoc::parse(&bytes).unwrap(), doc);
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains(' ') && !text.contains('\n'));
        assert!(text.find("\"binding\"").unwrap() < text.find("\"created_unix\"").unwrap());
        assert!(text.find("\"created_unix\"").unwrap() < text.find("\"schema\"").unwrap());
    }

    #[test]
    fn ledger_ownership_is_exact_and_unique() {
        let entry = |res: &str, path: &str| LedgerResource {
            resource: res.into(),
            adapter: "fs-link/v1".into(),
            class: Class::Normal,
            op_id: "op1".into(),
            owned_outputs: vec![OwnedOutput {
                path: path.into(),
                mode: 0o600,
                fingerprint: Some("a".repeat(64)),
            }],
        };
        let mut l = Ledger::empty(stamp());
        l.record(entry("a", "/h/x")).unwrap();
        assert_eq!(
            l.record(entry("b", "/h/x")).unwrap_err().code,
            Code::StateInvalid
        );
        assert!(l.authorizes_delete("a", "/h/x", &"a".repeat(64)));
        assert!(
            !l.authorizes_delete("a", "/h/x", &"b".repeat(64)),
            "conflict"
        );
        assert!(
            !l.authorizes_delete("b", "/h/x", &"a".repeat(64)),
            "wrong owner"
        );
        assert!(
            !l.authorizes_delete("a", "/h/y", &"a".repeat(64)),
            "not owned"
        );
        for bad in ["rel", "/h/../x", "/h//x", "/h/x/", "/", "/h/\nx"] {
            assert!(!valid_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_sensitive_resource_never_stores_a_hash() {
        let mut l = Ledger::empty(stamp());
        let e = LedgerResource {
            resource: "s".into(),
            adapter: "op-template/v1".into(),
            class: Class::Sensitive,
            op_id: "op1".into(),
            owned_outputs: vec![OwnedOutput {
                path: "/h/.secret".into(),
                mode: 0o600,
                fingerprint: Some("a".repeat(64)),
            }],
        };
        assert!(l.record(e.clone()).is_err());
        let mut e = e;
        e.owned_outputs[0].fingerprint = None;
        l.record(e).unwrap();
        assert!(!l.authorizes_delete("s", "/h/.secret", &"a".repeat(64)));
    }

    #[test]
    fn receipt_counts_and_sensitive_fingerprints_are_enforced() {
        let ok = receipt(vec![
            op(Status::Updated, Class::Normal),
            op(Status::Current, Class::Normal),
        ]);
        let back = Receipt::parse(&canonical(&ok).unwrap()).unwrap();
        assert_eq!(back, ok);
        assert_eq!(back.result.counts.values().sum::<u32>(), 2);
        assert_eq!(back.file_name().unwrap(), "20261010T000000Z-run-1.json");

        let mut skew = ok.clone();
        skew.result.counts.insert(Status::Failed, 1);
        assert!(Receipt::parse(&canonical(&skew).unwrap()).is_err());

        let mut s = op(Status::Updated, Class::Sensitive);
        s.after = Some(Fingerprint {
            fingerprint: "a".repeat(64),
        });
        assert!(receipt(vec![s]).validate().is_err());
    }

    #[test]
    fn journal_torn_oversize_and_unfinished_entries() {
        let intent = |op: &str| JournalEntry {
            schema: SCHEMA,
            op_id: "run-1".into(),
            op: op.into(),
            phase: Phase::Intent,
            at: "2026-10-10T00:00:00Z".into(),
            resource: "r".into(),
            adapter: "a".into(),
            class: Class::Normal,
            recovery: Some(Recovery {
                outputs: vec![],
                snapshot: None,
            }),
            token: None,
            reason: None,
        };
        let done = |op: &str| JournalEntry {
            phase: Phase::Done,
            recovery: None,
            ..intent(op)
        };
        let mut bytes = Vec::new();
        for e in [intent("a"), intent("b"), done("a")] {
            bytes.extend(e.to_line().unwrap());
        }
        let entries = parse_journal(&bytes).unwrap();
        let open = unfinished(&entries);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].op, "b");

        let torn = &bytes[..bytes.len() - 1];
        assert_eq!(parse_journal(torn).unwrap_err().code, Code::StateInvalid);
        let mut long = intent("a");
        long.recovery = None;
        assert!(long.to_line().is_err(), "intent without recovery metadata");
        let mut huge = intent("a");
        huge.reason = Some("x".into());
        let line = vec![b'x'; MAX_JOURNAL_LINE];
        assert_eq!(
            parse_journal(&[line, b"\n".to_vec()].concat())
                .unwrap_err()
                .code,
            Code::LimitExceeded
        );
    }

    #[test]
    fn redaction_removes_secrets_escapes_controls_and_bounds_length() {
        let out = redact("token=hunter2\x1b[31m ok", &["hunter2"]);
        assert_eq!(out, "token=[redacted]\\u{1b}[31m ok");
        assert!(!out.contains("hunter2"));
        assert!(redact(&"x".repeat(10_000), &[]).len() <= MAX_ERROR_BYTES);
        // Overlapping matches merge, so no tail of the longer one survives.
        assert_eq!(redact("abcdef", &["abcd", "cdef"]), "[redacted]");
        assert_eq!(redact("abcdefgh", &["abcd", "efgh"]), "[redacted]");
        // Bidirectional and zero-width characters become visible.
        assert_eq!(redact("a\u{202E}b\u{200B}c", &[]), "a\\u{202e}b\\u{200b}c");
        // A match that overlaps itself is found at every start.
        assert_eq!(redact("ababab", &["abab"]), "[redacted]");
        assert_eq!(redact("xaaaaaax", &["aaaa"]), "x[redacted]x");
        // A secret that contains another secret is redacted whole, whatever the list order.
        for list in [["abc", "abcdef"], ["abcdef", "abc"]] {
            assert_eq!(redact("x abcdef y", &list), "x [redacted] y");
        }
    }

    #[test]
    fn trial_records_validate_identity_and_paths() {
        let t = TrialRecord {
            schema: SCHEMA,
            binding: stamp(),
            trial_id: "t1".into(),
            tool: "atuin".into(),
            created_unix: 1,
            baseline: Baseline {
                tool_preexisting: false,
                paths: vec![BaselinePath {
                    path: "/h/.config/a".into(),
                    existed: false,
                    sha256: None,
                }],
            },
            owned_outputs: vec![],
        };
        assert_eq!(TrialRecord::parse(&canonical(&t).unwrap()).unwrap(), t);
        let mut bad = t;
        bad.trial_id = "Bad Id".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn scope_is_case_folded_strict_and_refuses_ancestors() {
        let reserved = [
            "/H/.local/state/dotfiles-manager".to_string(),
            "/H/.config/dotfiles-manager".to_string(),
        ];
        let sc = Scope::new("/H", &reserved);
        for ok in [
            "/H/.gitconfig",
            "/h/.config/git/config",
            "/H/.local/share/x",
        ] {
            assert!(sc.admits(ok), "{ok}");
        }
        for no in [
            "/H",
            "/etc/hosts",
            "/Hx/y",
            "/H/.local/state/dotfiles-manager/state/state.json",
            "/h/.LOCAL/STATE/Dotfiles-Manager/state/ledger.json",
            "/H/.local/state",
            "/H/.config",
            "/H/.local",
            "/H/.config/dotfiles-manager/machine.toml",
        ] {
            assert!(!sc.admits(no), "{no}");
        }
    }

    #[test]
    fn scrub_redacts_text_and_refuses_secret_paths_without_echoing_them() {
        let mut r = receipt(vec![op(Status::Updated, Class::Normal)]);
        r.command = "sync hunter2".into();
        r.operations[0].error = Some("bad hunter2".into());
        r.scrub(&["hunter2"]).unwrap();
        assert_eq!(r.command, "sync [redacted]");
        assert_eq!(r.operations[0].error.as_deref(), Some("bad [redacted]"));
        r.operations[0].owned_outputs = vec!["/h/tok-hunter2".into()];
        let e = r.scrub(&["hunter2"]).unwrap_err();
        assert!(!e.detail.contains("hunter2"));
    }

    #[test]
    fn an_operation_that_never_ran_has_no_times_and_one_that_ran_must() {
        let mut o = op(Status::NotStarted, Class::Normal);
        o.started_at = None;
        o.finished_at = None;
        assert!(receipt(vec![o.clone()]).validate().is_ok());
        o.status = Status::NotInScope;
        assert!(receipt(vec![o.clone()]).validate().is_ok());
        for ran in [
            Status::Updated,
            Status::Failed,
            Status::Pending,
            Status::Unknown,
        ] {
            o.status = ran;
            assert!(receipt(vec![o.clone()]).validate().is_err(), "{ran:?}");
        }
    }

    #[test]
    fn the_fold_catches_unicode_aliases_even_when_nothing_exists() {
        let sc = Scope::new("/H", &["/H/.local/share/dotfiles-manager".to_string()]);
        assert!(!sc.admits("/H/.local/\u{17f}hare/dotfiles-manager/new"));
        assert!(!sc.admits("/H/.local/share/DOTFILE\u{17f}-manager/x"));
        assert!(sc.admits("/H/.local/share/other"));
    }

    #[test]
    fn a_reload_state_must_come_from_the_fixed_list_and_round_trips() {
        for name in RELOADS {
            let mut o = op(Status::Updated, Class::Normal);
            o.reload = Some(name.into());
            let r = receipt(vec![o]);
            r.validate().unwrap();
            let again = Receipt::parse(&canonical(&r).unwrap()).unwrap();
            assert_eq!(again.operations[0].reload.as_deref(), Some(name));
        }
        let mut o = op(Status::Updated, Class::Normal);
        o.reload = Some("done".into());
        assert_eq!(
            receipt(vec![o]).validate().unwrap_err().code,
            Code::StateInvalid
        );
        // An absent field is not written, so receipts without it keep their bytes.
        let plain = canonical(&receipt(vec![op(Status::Updated, Class::Normal)])).unwrap();
        assert!(!String::from_utf8(plain).unwrap().contains("reload"));
    }

    #[test]
    fn a_path_source_and_a_disruptive_approval_validate_and_round_trip() {
        let mut r = receipt(vec![op(Status::Updated, Class::Normal)]);
        r.sources = vec![SourceId {
            id: "path-0123456789ab".into(),
            commit: None,
            tree: format!("sha256:{}", "b".repeat(64)),
        }];
        // A removal: it owns and publishes nothing, and `before` is the fingerprint it removed.
        r.operations[0].owned_outputs.clear();
        r.operations[0].before = Some(Fingerprint {
            fingerprint: "a".repeat(64),
        });
        r.approvals = vec![DisruptiveApproval {
            operation_id: "op1".into(),
            how: "allow_disruptive".into(),
            at: "2026-10-10T00:00:00Z".into(),
        }];
        r.validate().unwrap();
        let again = Receipt::parse(&canonical(&r).unwrap()).unwrap();
        assert_eq!(again, r);
    }

    #[test]
    fn an_approval_must_fit_the_operation_it_names() {
        let adopted = || {
            let mut r = receipt(vec![op(Status::Updated, Class::Normal)]);
            r.operations[0].after = Some(Fingerprint {
                fingerprint: "a".repeat(64),
            });
            r.approvals = vec![DisruptiveApproval {
                operation_id: "op1".into(),
                how: "allow_adopt".into(),
                at: "2026-10-10T00:00:00Z".into(),
            }];
            r
        };
        adopted().validate().unwrap();
        // The same operation cannot carry the disruptive approval: it owns an output.
        let mut r = adopted();
        r.approvals[0].how = "allow_disruptive".into();
        assert!(r.validate().is_err());
        // A removal (nothing owned, a `before`) cannot carry an adoption approval.
        let mut r = adopted();
        r.operations[0].owned_outputs.clear();
        r.operations[0].after = None;
        r.operations[0].before = Some(Fingerprint {
            fingerprint: "a".repeat(64),
        });
        assert!(r.validate().is_err());
        r.approvals[0].how = "allow_disruptive".into();
        r.validate().unwrap();
        // An updated operation with no `before` (a directory made, say) is not a removal.
        let mut r = adopted();
        r.operations[0].owned_outputs.clear();
        r.operations[0].after = None;
        r.approvals[0].how = "allow_disruptive".into();
        assert!(r.validate().is_err());
        // A published link (it has a reload state) is not an adoption.
        let mut r = adopted();
        r.operations[0].reload = Some("not_attempted".into());
        assert!(r.validate().is_err());
    }

    #[test]
    fn receipt_fields_without_free_text_must_have_their_exact_shape() {
        let good = receipt(vec![op(Status::Updated, Class::Normal)]);
        assert!(good.validate().is_ok());
        let mut cases: Vec<(&str, Receipt)> = Vec::new();
        let mut r = good.clone();
        r.result.outcome = "all fine, token=abc".into();
        cases.push(("outcome", r));
        let mut r = good.clone();
        r.binary.sha256 = "not a hash".into();
        cases.push(("sha", r));
        let mut r = good.clone();
        r.plan_digest = Some("secret".into());
        cases.push(("plan", r));
        let mut r = good.clone();
        r.approval = Some(Approval {
            kind: "hunter2".into(),
            plan_digest: "a".repeat(64),
            at: "2026-10-10T00:00:00Z".into(),
        });
        cases.push(("approval", r));
        let mut r = good.clone();
        r.sources = vec![SourceId {
            id: "s".into(),
            commit: Some("zz".into()),
            tree: "x".into(),
        }];
        cases.push(("source", r));
        let mut r = good.clone();
        r.approvals = vec![DisruptiveApproval {
            operation_id: "no-such-op".into(),
            how: "allow_disruptive".into(),
            at: "2026-10-10T00:00:00Z".into(),
        }];
        cases.push(("approval for an unknown operation", r));
        let mut r = good.clone();
        r.approvals = vec![DisruptiveApproval {
            operation_id: "op1".into(),
            how: "interactive".into(),
            at: "2026-10-10T00:00:00Z".into(),
        }];
        cases.push(("approval kind with no prompt behind it", r));
        let mut r = good.clone();
        r.operations[0].started_at = Some("yesterday".into());
        cases.push(("time", r));
        let mut r = good.clone();
        r.operations[0].resource = "line\nbreak".into();
        cases.push(("name", r));
        for (what, r) in cases {
            assert!(r.validate().is_err(), "{what}");
        }
    }
}
