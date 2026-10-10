//! Versioned machine-written state documents (docs/architecture.md sections 4 and 5, docs/state.md).
//!
//! Pure data: parsing takes already-read bytes. Every document has an integer `schema` that is
//! checked before any other field, and unknown keys are refused.

use crate::binding::Binding;
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
pub fn canonical<T: Serialize>(doc: &T) -> Vec<u8> {
    // `Value` keeps objects in a sorted map (serde_json without `preserve_order`).
    let v = serde_json::to_value(doc).expect("plain data always serializes");
    serde_json::to_vec(&v).expect("a JSON value always serializes")
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
    pub started_at: String,
    pub finished_at: String,
    pub before: Option<Fingerprint>,
    pub after: Option<Fingerprint>,
    pub owned_outputs: Vec<String>,
    pub error: Option<String>,
}

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
    pub commit: String,
    pub tree: String,
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
        let mut l = canonical(self);
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

/// Text for a receipt, journal or log: known secret values become `[redacted]`, control
/// characters become visible escapes, and the length is bounded.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut t = text.to_string();
    // Longest first: replacing a short secret first could leave the rest of a longer one visible.
    let mut ordered: Vec<&&str> = secrets.iter().filter(|s| !s.is_empty()).collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in ordered {
        t = t.replace(*s, "[redacted]");
    }
    let mut out = String::new();
    for c in t.chars() {
        let piece: String = if c.is_control() {
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
            started_at: "2026-10-10T00:00:00Z".into(),
            finished_at: "2026-10-10T00:00:01Z".into(),
            before: None,
            after: None,
            owned_outputs: vec!["/h/.gitconfig".into()],
            error: None,
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
        let bytes = canonical(&doc);
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
        let back = Receipt::parse(&canonical(&ok)).unwrap();
        assert_eq!(back, ok);
        assert_eq!(back.result.counts.values().sum::<u32>(), 2);
        assert_eq!(back.file_name().unwrap(), "20261010T000000Z-run-1.json");

        let mut skew = ok.clone();
        skew.result.counts.insert(Status::Failed, 1);
        assert!(Receipt::parse(&canonical(&skew)).is_err());

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
        assert_eq!(TrialRecord::parse(&canonical(&t)).unwrap(), t);
        let mut bad = t;
        bad.trial_id = "Bad Id".into();
        assert!(bad.validate().is_err());
    }
}
