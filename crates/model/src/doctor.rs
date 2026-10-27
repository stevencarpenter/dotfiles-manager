//! Health rows for `doctor` and `status` (docs/doctor.md): the row type, the code registry, the
//! exit rule and the two output formats. Pure: the caller gathers the facts.

use crate::display::{bounded_visible, visible};
use crate::refusal::Code;
use serde::Serialize;

/// The `schema` value of the JSON document.
pub const SCHEMA: i64 = 1;

/// A subject is cut to this many characters. Real paths are far shorter.
pub const MAX_SUBJECT_CHARS: usize = 4096;

/// Row kinds every report must contain.
const REQUIRED_KINDS: [&str; 3] = ["binding", "state", "profile"];

/// The narrowest width the table renders at.
pub const MIN_WIDTH: usize = 40;

/// Codes that only `doctor` uses. A row may also carry any `E_*` code from [`Code`]. Every
/// code is described in docs/doctor.md, and a test keeps the two in step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoctorCode {
    Ok,
    NaSensitive,
    OutputMissing,
    PlanPending,
    PlanRemoval,
    Unrecorded,
    StatePartial,
    LastRun,
    NoReceipt,
    StateAbsent,
    LockNote,
    DependsUnknown,
    ProbingNotBuilt,
    StagedCopy,
    Upgrade,
}

impl DoctorCode {
    pub const ALL: [DoctorCode; 15] = [
        DoctorCode::Ok,
        DoctorCode::NaSensitive,
        DoctorCode::OutputMissing,
        DoctorCode::PlanPending,
        DoctorCode::PlanRemoval,
        DoctorCode::Unrecorded,
        DoctorCode::StatePartial,
        DoctorCode::LastRun,
        DoctorCode::NoReceipt,
        DoctorCode::StateAbsent,
        DoctorCode::LockNote,
        DoctorCode::DependsUnknown,
        DoctorCode::ProbingNotBuilt,
        DoctorCode::StagedCopy,
        DoctorCode::Upgrade,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            DoctorCode::Ok => "D_OK",
            DoctorCode::NaSensitive => "D_NA_SENSITIVE",
            DoctorCode::OutputMissing => "D_OUTPUT_MISSING",
            DoctorCode::PlanPending => "D_PLAN_PENDING",
            DoctorCode::PlanRemoval => "D_PLAN_REMOVAL",
            DoctorCode::Unrecorded => "D_UNRECORDED",
            DoctorCode::StatePartial => "D_STATE_PARTIAL",
            DoctorCode::LastRun => "D_LAST_RUN",
            DoctorCode::NoReceipt => "D_NO_RECEIPT",
            DoctorCode::StateAbsent => "D_STATE_ABSENT",
            DoctorCode::LockNote => "D_LOCK_NOTE",
            DoctorCode::DependsUnknown => "D_DEPENDS_UNKNOWN",
            DoctorCode::ProbingNotBuilt => "D_PROBING_NOT_BUILT",
            DoctorCode::StagedCopy => "D_STAGED_COPY",
            DoctorCode::Upgrade => "D_UPGRADE",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            DoctorCode::Ok => "The check passed.",
            DoctorCode::NaSensitive => "A sensitive output. Its content is never read or hashed.",
            DoctorCode::OutputMissing => {
                "An owned output no longer exists. Apply publishes it again."
            }
            DoctorCode::PlanPending => {
                "The plan would create or update this output. Nothing owned was edited."
            }
            DoctorCode::PlanRemoval => {
                "The plan would remove this owned output. Nothing selects it any more."
            }
            DoctorCode::Unrecorded => {
                "The output already is the wanted result, but the ledger does not record it."
            }
            DoctorCode::StatePartial => {
                "A state file that is only audit evidence is unreadable or stray. The inventory is partial."
            }
            DoctorCode::LastRun => "The last recorded run did not end with `ok`.",
            DoctorCode::NoReceipt => "No run has been recorded yet.",
            DoctorCode::StateAbsent => "No binding and no state exist yet.",
            DoctorCode::LockNote => "The lock file holds a note from an earlier holder.",
            DoctorCode::DependsUnknown => "A check cannot run because an earlier check failed.",
            DoctorCode::ProbingNotBuilt => "Tools, versions and PATH are not probed in this build.",
            DoctorCode::StagedCopy => {
                "A name beside an owned output has the shape of a staged copy of an older build. Nothing removes it."
            }
            DoctorCode::Upgrade => "The last upgrade did not end with `ok`.",
        }
    }
}

/// The code of a row: an existing `E_*` code or a `D_*` code of `doctor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCode {
    Error(Code),
    Doctor(DoctorCode),
}

impl RowCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            RowCode::Error(c) => c.as_str(),
            RowCode::Doctor(c) => c.as_str(),
        }
    }
}

impl From<Code> for RowCode {
    fn from(c: Code) -> Self {
        RowCode::Error(c)
    }
}

impl From<DoctorCode> for RowCode {
    fn from(c: DoctorCode) -> Self {
        RowCode::Doctor(c)
    }
}

impl Serialize for RowCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Level {
    #[serde(rename = "OK")]
    Ok,
    #[serde(rename = "WARN")]
    Warn,
    #[serde(rename = "ERROR")]
    Error,
    #[serde(rename = "UNKNOWN")]
    Unknown,
    #[serde(rename = "N/A")]
    NotApplicable,
}

impl Level {
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "OK",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
            Level::Unknown => "UNKNOWN",
            Level::NotApplicable => "N/A",
        }
    }
}

/// One finding. All text is made visible on construction, so no row can carry a control or
/// bidirectional character.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Row {
    /// What was checked: `binding`, `state`, `lock`, `profile`, `module`, `resource`, `output`,
    /// `intent`, `receipt` or `plan`.
    pub kind: &'static str,
    pub code: RowCode,
    pub status: Level,
    /// The path, file or resource the row is about.
    pub subject: String,
    pub detail: String,
    /// One line, never a secret value.
    pub fix: String,
    /// An UNKNOWN required row makes the exit code 1.
    pub required: bool,
    /// The row comes from an inventory that could not be read in full.
    pub partial: bool,
}

impl Row {
    pub fn new(
        kind: &'static str,
        code: impl Into<RowCode>,
        status: Level,
        subject: &str,
        detail: &str,
        fix: &str,
    ) -> Row {
        Row {
            kind,
            code: code.into(),
            status,
            subject: bounded_visible(subject, MAX_SUBJECT_CHARS),
            detail: visible(detail),
            fix: visible(fix),
            required: true,
            partial: false,
        }
    }

    pub fn optional(mut self) -> Row {
        self.required = false;
        self
    }

    pub fn partial(mut self) -> Row {
        self.partial = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub schema: i64,
    pub command: &'static str,
    /// Set by a `test-isolation` build, as for `plan --json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolated: Option<String>,
    /// `ok`, `warn` or `error`. `error` is exit 1.
    pub result: &'static str,
    pub rows: Vec<Row>,
}

impl Report {
    pub fn new(command: &'static str, isolated: Option<String>, rows: Vec<Row>) -> Report {
        let mut r = Report {
            schema: SCHEMA,
            command,
            isolated,
            result: "ok",
            rows,
        };
        r.result = if r.failed() {
            "error"
        } else if r
            .rows
            .iter()
            .any(|x| matches!(x.status, Level::Warn | Level::Unknown))
        {
            "warn"
        } else {
            "ok"
        };
        r
    }

    /// Exit 1: any ERROR row, or an UNKNOWN row of a required check. A WARN row alone is exit 0.
    pub fn failed(&self) -> bool {
        // A report without the binding, state and profile rows is incomplete, not healthy.
        let missing = REQUIRED_KINDS
            .iter()
            .any(|k| !self.rows.iter().any(|r| r.kind == *k));
        missing
            || self
                .rows
                .iter()
                .any(|r| r.status == Level::Error || (r.status == Level::Unknown && r.required))
    }

    pub fn count(&self, level: Level) -> usize {
        self.rows.iter().filter(|r| r.status == level).count()
    }

    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut b = serde_json::to_vec(self)?;
        b.push(b'\n');
        Ok(b)
    }

    /// The table. Each row starts a line with `STATUS CODE kind`; `subject`, `detail` and `fix`
    /// follow on indented lines, wrapped so that no line is longer than `width` characters.
    /// `verbose` adds `detail` and `fix`.
    pub fn render(&self, width: usize, verbose: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let mut out = String::new();
        for r in &self.rows {
            out.push_str(&format!(
                "{:<7} {} {}\n",
                r.status.as_str(),
                r.code.as_str(),
                r.kind
            ));
            wrap_into(&mut out, "subject: ", &r.subject, width);
            if verbose {
                let flags = match (r.required, r.partial) {
                    (_, true) => " (partial inventory)",
                    (false, _) => " (not required)",
                    _ => "",
                };
                wrap_into(&mut out, "detail: ", &format!("{}{flags}", r.detail), width);
                if !r.fix.is_empty() {
                    wrap_into(&mut out, "fix: ", &r.fix, width);
                }
            }
        }
        out.push_str(&format!("summary: result={}\n", self.result));
        let counts = format!(
            "ok={} warn={} error={} unknown={} n/a={}",
            self.count(Level::Ok),
            self.count(Level::Warn),
            self.count(Level::Error),
            self.count(Level::Unknown),
            self.count(Level::NotApplicable),
        );
        wrap_into(&mut out, "counts: ", &counts, width);
        out
    }
}

/// Writes `label` and `text`, indented by two spaces. Breaks at spaces, and inside a word that is
/// longer than a line.
fn wrap_into(out: &mut String, label: &str, text: &str, width: usize) {
    let indent = 2 + label.chars().count();
    let avail = width.saturating_sub(indent).max(8);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0;
    for word in text.split(' ') {
        let w: Vec<char> = word.chars().collect();
        let need = usize::from(cur_len > 0) + w.len();
        if cur_len + need <= avail {
            if cur_len > 0 {
                cur.push(' ');
            }
            cur.extend(w.iter());
            cur_len += need;
            continue;
        }
        if cur_len > 0 {
            lines.push(std::mem::take(&mut cur));
        }
        // Chunks of `avail` characters: linear in the length of the word.
        let mut chunks = w.chunks(avail).peekable();
        while let Some(c) = chunks.next() {
            if chunks.peek().is_some() {
                lines.push(c.iter().collect());
            } else {
                cur = c.iter().collect();
                cur_len = c.len();
            }
        }
    }
    if cur_len > 0 || lines.is_empty() {
        lines.push(cur);
    }
    for (i, l) in lines.iter().enumerate() {
        let pad = if i == 0 {
            format!("  {label}")
        } else {
            " ".repeat(indent)
        };
        out.push_str(&format!("{pad}{l}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: Level, required: bool) -> Row {
        let r = Row::new("output", DoctorCode::Ok, status, "p", "d", "f");
        if required { r } else { r.optional() }
    }

    /// The three kinds every report needs, so only the rows under test decide the exit.
    fn base() -> Vec<Row> {
        ["binding", "state", "profile"]
            .map(|k| Row::new(k, DoctorCode::Ok, Level::Ok, "s", "d", ""))
            .into()
    }

    fn failed(extra: Vec<Row>) -> bool {
        let mut rows = base();
        rows.extend(extra);
        Report::new("doctor", None, rows).failed()
    }

    #[test]
    fn exit_rule() {
        assert!(!failed(vec![
            row(Level::Ok, true),
            row(Level::NotApplicable, true)
        ]));
        assert!(!failed(vec![row(Level::Warn, true)]));
        assert!(failed(vec![row(Level::Error, true)]));
        assert!(failed(vec![row(Level::Unknown, true)]));
        assert!(!failed(vec![row(Level::Unknown, false)]));
    }

    #[test]
    fn a_report_missing_a_required_kind_is_never_healthy() {
        assert!(Report::new("doctor", None, Vec::new()).failed());
        for skip in ["binding", "state", "profile"] {
            let rows: Vec<Row> = base().into_iter().filter(|r| r.kind != skip).collect();
            assert!(Report::new("doctor", None, rows).failed(), "{skip}");
        }
        assert!(!Report::new("doctor", None, base()).failed());
    }

    #[test]
    fn a_row_is_required_unless_marked_optional() {
        assert!(Row::new("output", DoctorCode::Ok, Level::Unknown, "p", "d", "f").required);
        assert!(!row(Level::Unknown, false).required);
    }

    /// An exhaustive match: a new variant does not compile until it is listed here, and then
    /// `ALL` must list it too.
    fn index(c: DoctorCode) -> usize {
        match c {
            DoctorCode::Ok => 0,
            DoctorCode::NaSensitive => 1,
            DoctorCode::OutputMissing => 2,
            DoctorCode::PlanPending => 3,
            DoctorCode::PlanRemoval => 4,
            DoctorCode::Unrecorded => 5,
            DoctorCode::StatePartial => 6,
            DoctorCode::LastRun => 7,
            DoctorCode::NoReceipt => 8,
            DoctorCode::StateAbsent => 9,
            DoctorCode::LockNote => 10,
            DoctorCode::DependsUnknown => 11,
            DoctorCode::ProbingNotBuilt => 12,
            DoctorCode::StagedCopy => 13,
            DoctorCode::Upgrade => 14,
        }
    }

    #[test]
    fn all_lists_every_variant_once() {
        let mut idx: Vec<usize> = DoctorCode::ALL.iter().map(|c| index(*c)).collect();
        idx.sort_unstable();
        assert_eq!(idx, (0..=14).collect::<Vec<_>>());
    }

    #[test]
    fn codes_are_unique_prefixed_and_described() {
        let mut names: Vec<_> = DoctorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert!(names.iter().all(|n| n.starts_with("D_")));
        assert!(DoctorCode::ALL.iter().all(|c| !c.description().is_empty()));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), DoctorCode::ALL.len());
        assert_eq!(RowCode::from(Code::Conflict).as_str(), "E_CONFLICT");
    }

    #[test]
    fn wrapping_never_exceeds_the_width_and_loses_nothing() {
        let long = "a/very/long/path/".repeat(12);
        let r = Row::new(
            "output",
            Code::ObservationUnknown,
            Level::Error,
            &long,
            "x y z",
            "f",
        );
        let t = Report::new("doctor", None, vec![r]).render(MIN_WIDTH, true);
        assert!(t.lines().all(|l| l.chars().count() <= MIN_WIDTH), "{t}");
        let joined: String = t
            .lines()
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with("detail:"))
            .map(|l| l.trim_start().trim_start_matches("subject: "))
            .collect();
        assert_eq!(joined, long);
    }

    #[test]
    fn wrapping_a_huge_word_is_fast_and_keeps_multibyte_text_intact() {
        let word = "é".repeat(1 << 20);
        let r = Row::new("output", DoctorCode::Ok, Level::Ok, &word, "d", "f");
        let mut out = String::new();
        wrap_into(&mut out, "subject: ", &word, MIN_WIDTH);
        assert!(out.lines().all(|l| l.chars().count() <= MIN_WIDTH));
        // The subject is bounded when the row is made.
        assert_eq!(r.subject.chars().count(), MAX_SUBJECT_CHARS);
        assert_eq!(out.chars().filter(|c| *c == 'é').count(), 1 << 20);
    }

    #[test]
    fn a_subject_is_cut_after_its_escapes_are_expanded_and_never_inside_one() {
        // 5000 ESC characters expand to six characters each. The cut is on the visible text.
        let r = Row::new(
            "output",
            DoctorCode::Ok,
            Level::Ok,
            &"\u{1b}".repeat(5000),
            "d",
            "f",
        );
        assert!(r.subject.chars().count() <= MAX_SUBJECT_CHARS);
        assert_eq!(r.subject.chars().count(), 6 * (MAX_SUBJECT_CHARS / 6));
        assert!(r.subject.starts_with("\\u{1b}\\u{1b}") && r.subject.ends_with("\\u{1b}"));
        assert_eq!(r.subject.matches("\\u{1b}").count(), MAX_SUBJECT_CHARS / 6);
        // Plain text keeps the old bound exactly.
        let plain = Row::new(
            "output",
            DoctorCode::Ok,
            Level::Ok,
            &"a".repeat(5000),
            "d",
            "f",
        );
        assert_eq!(plain.subject.chars().count(), MAX_SUBJECT_CHARS);
    }

    #[test]
    fn row_text_is_made_visible() {
        let r = Row::new(
            "output",
            DoctorCode::Ok,
            Level::Ok,
            "a\u{1b}[31mb",
            "c\nd",
            "e\u{202e}",
        );
        assert!(!r.subject.contains('\u{1b}') && !r.detail.contains('\n'));
        assert!(!r.fix.contains('\u{202e}'));
    }
}
