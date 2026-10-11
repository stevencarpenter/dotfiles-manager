//! Health rows for `doctor` and `status` (docs/doctor.md): the row type, the code registry, the
//! exit rule and the two output formats. Pure: the caller gathers the facts.

use crate::display::visible;
use serde::Serialize;

/// The `schema` value of the JSON document.
pub const SCHEMA: i64 = 1;

/// The narrowest width the table renders at.
pub const MIN_WIDTH: usize = 40;

/// Codes that only `doctor` uses. A row may also carry any `E_*` code from [`crate::Code`]. Every
/// code here is described in docs/doctor.md, and a test keeps the two in step.
pub const CODES: &[(&str, &str)] = &[
    ("D_OK", "The check passed."),
    (
        "D_NA_SENSITIVE",
        "A sensitive output. Its content is never read or hashed.",
    ),
    (
        "D_NA_GENERATED",
        "A generated file. No generator exists in this build.",
    ),
    (
        "D_OUTPUT_MISSING",
        "An owned output no longer exists. Apply publishes it again.",
    ),
    (
        "D_PLAN_PENDING",
        "The plan would create or update this output. Nothing owned was edited.",
    ),
    (
        "D_PLAN_REMOVAL",
        "The plan would remove this owned output. Nothing selects it any more.",
    ),
    (
        "D_UNRECORDED",
        "The output already is the wanted result, but the ledger does not record it.",
    ),
    (
        "D_STATE_PARTIAL",
        "A state file that is only audit evidence is unreadable or stray. The inventory is partial.",
    ),
    ("D_LAST_RUN", "The last recorded run did not end with `ok`."),
    ("D_NO_RECEIPT", "No run has been recorded yet."),
    ("D_STATE_ABSENT", "No binding and no state exist yet."),
    (
        "D_LOCK_NOTE",
        "The lock file holds a note from an earlier holder.",
    ),
    (
        "D_DEPENDS_UNKNOWN",
        "A check cannot run because an earlier check failed.",
    ),
    (
        "D_PROBING_NOT_BUILT",
        "Tools, versions and PATH are not probed in this build.",
    ),
];

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
    pub code: String,
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
        code: &str,
        status: Level,
        subject: &str,
        detail: &str,
        fix: &str,
    ) -> Row {
        Row {
            kind,
            code: code.to_string(),
            status,
            subject: visible(subject),
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
        self.rows
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
            out.push_str(&format!("{:<7} {} {}\n", r.status.as_str(), r.code, r.kind));
            wrap_into(&mut out, "subject: ", &r.subject, width);
            if verbose {
                let flags = match (r.required, r.partial) {
                    (_, true) => " (partial inventory)",
                    (false, _) => " (not required)",
                    _ => "",
                };
                wrap_into(&mut out, "detail: ", &format!("{}{flags}", r.detail), width);
                wrap_into(&mut out, "fix: ", &r.fix, width);
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
        let mut w: Vec<char> = word.chars().collect();
        loop {
            let need = usize::from(cur_len > 0) + w.len();
            if cur_len + need <= avail {
                if cur_len > 0 {
                    cur.push(' ');
                }
                cur.extend(w.iter());
                cur_len += need;
                break;
            }
            if cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
                continue;
            }
            let tail = w.split_off(avail);
            lines.push(w.iter().collect());
            w = tail;
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
        let r = Row::new("output", "D_OK", status, "p", "d", "f");
        if required { r } else { r.optional() }
    }

    #[test]
    fn exit_rule() {
        let ok = |rows| !Report::new("doctor", None, rows).failed();
        assert!(ok(vec![
            row(Level::Ok, true),
            row(Level::NotApplicable, true)
        ]));
        assert!(ok(vec![row(Level::Warn, true)]));
        assert!(!ok(vec![row(Level::Error, true)]));
        assert!(!ok(vec![row(Level::Unknown, true)]));
        assert!(ok(vec![row(Level::Unknown, false)]));
    }

    #[test]
    fn codes_are_unique_and_prefixed() {
        let mut names: Vec<_> = CODES.iter().map(|c| c.0).collect();
        assert!(names.iter().all(|n| n.starts_with("D_")));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CODES.len());
    }

    #[test]
    fn wrapping_never_exceeds_the_width_and_loses_nothing() {
        let long = "a/very/long/path/".repeat(12);
        let r = Row::new(
            "output",
            "E_OBSERVATION_UNKNOWN",
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
    fn row_text_is_made_visible() {
        let r = Row::new(
            "output",
            "D_OK",
            Level::Ok,
            "a\u{1b}[31mb",
            "c\nd",
            "e\u{202e}",
        );
        assert!(!r.subject.contains('\u{1b}') && !r.detail.contains('\n'));
        assert!(!r.fix.contains('\u{202e}'));
    }
}
