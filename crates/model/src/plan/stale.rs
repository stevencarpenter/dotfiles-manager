//! Stale-plan detection (docs/architecture.md section 10: "A mismatch exits 3 as stale").
//! A saved plan is compared with a fresh one built from the same command. Every difference is
//! named, so the reviewer sees which source, observation, ownership or topology changed. The
//! comparison is complete: if the digests differ, at least one entry says so.

use super::{Operation, Plan};
use crate::refusal::Refusal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What kind of input changed, most specific first (the derived order is the report order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleKind {
    SourceInput,
    Ownership,
    Observation,
    Operation,
    Source,
    Model,
    Binary,
    Plan,
}

impl StaleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StaleKind::SourceInput => "source input",
            StaleKind::Ownership => "ownership",
            StaleKind::Observation => "observation",
            StaleKind::Operation => "operation",
            StaleKind::Source => "source",
            StaleKind::Model => "model",
            StaleKind::Binary => "binary",
            StaleKind::Plan => "plan",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stale {
    pub what: StaleKind,
    pub subject: String,
    pub was: String,
    pub now: String,
}

fn diff(out: &mut Vec<Stale>, what: StaleKind, subject: &str, was: &str, now: &str) {
    if was != now {
        out.push(Stale {
            what,
            subject: subject.into(),
            was: was.into(),
            now: now.into(),
        });
    }
}

fn keyed(
    out: &mut Vec<Stale>,
    what: StaleKind,
    a: &BTreeMap<String, String>,
    b: &BTreeMap<String, String>,
) {
    for (k, v) in a {
        diff(out, what, k, v, b.get(k).map_or("absent", String::as_str));
    }
    for (k, v) in b.iter().filter(|(k, _)| !a.contains_key(*k)) {
        diff(out, what, k, "absent", v);
    }
}

/// Differences between `old` and `new`, most specific first, one entry per kind and subject.
/// Empty only when the two plans have the same digest.
pub fn stale(old: &Plan, new: &Plan) -> Result<Vec<Stale>, Refusal> {
    let mut out = Vec::new();
    diff(
        &mut out,
        StaleKind::Binary,
        "version",
        &old.binary.version,
        &new.binary.version,
    );
    diff(
        &mut out,
        StaleKind::Binary,
        "sha256",
        &old.binary.sha256,
        &new.binary.sha256,
    );
    diff(
        &mut out,
        StaleKind::Model,
        "profile",
        &old.profile,
        &new.profile,
    );
    diff(
        &mut out,
        StaleKind::Model,
        "machine id",
        &old.machine_id,
        &new.machine_id,
    );
    diff(
        &mut out,
        StaleKind::Model,
        "resolved model",
        &old.model_digest,
        &new.model_digest,
    );
    let tree = |p: &Plan| -> BTreeMap<String, String> {
        p.sources
            .iter()
            .map(|s| (s.root.clone(), s.tree.clone()))
            .collect()
    };
    keyed(&mut out, StaleKind::Source, &tree(old), &tree(new));

    fn ops(p: &Plan) -> BTreeMap<&str, &Operation> {
        p.operations.iter().map(|o| (o.id.as_str(), o)).collect()
    }
    let (a, b) = (ops(old), ops(new));
    for (id, o) in &a {
        match b.get(id) {
            None => diff(&mut out, StaleKind::Operation, id, "present", "gone"),
            Some(n) => op_diff(&mut out, o, n),
        }
    }
    for id in b.keys().filter(|id| !a.contains_key(*id)) {
        diff(&mut out, StaleKind::Operation, id, "absent", "present");
    }

    // Observations that no operation precondition covers still count: a fragment appearing
    // changes the input set even when no precondition names it. The text is the same
    // `node_state` string a precondition shows, so a path reads one way wherever it appears.
    // A change `node_state` does not show (an owner flag) is caught by the digest entry below.
    let obs = |p: &Plan| -> BTreeMap<String, String> {
        let mut m: BTreeMap<String, String> = p
            .observations
            .seen
            .iter()
            .map(|(k, v)| (format!("path:{k}"), super::node_state(v, false)))
            .collect();
        m.extend(p.observations.listings.iter().map(|(k, v)| {
            (
                format!("listing:{k}"),
                format!("{} {:?}", v.names.join(","), v.incomplete),
            )
        }));
        m
    };
    keyed(&mut out, StaleKind::Observation, &obs(old), &obs(new));

    out.sort();
    out.dedup_by(|b, a| a.what == b.what && a.subject == b.subject);

    // Completeness: whatever else differs (scope, blockers, an effect, a mode), the digest says so.
    let (was, now) = (old.digest()?, new.digest()?);
    if out.is_empty() && was != now {
        out.push(Stale {
            what: StaleKind::Plan,
            subject: "digest".into(),
            was,
            now,
        });
    }
    Ok(out)
}

fn op_diff(out: &mut Vec<Stale>, a: &Operation, b: &Operation) {
    let subject = format!("{} ({})", a.id, a.resource);
    let inputs = |o: &Operation| {
        o.inputs
            .iter()
            .map(|i| format!("{}={}", i.path, i.content_id.as_deref().unwrap_or("-")))
            .collect::<Vec<_>>()
            .join(";")
    };
    diff(
        out,
        StaleKind::SourceInput,
        &subject,
        &inputs(a),
        &inputs(b),
    );
    if a.ownership != b.ownership {
        diff(
            out,
            StaleKind::Ownership,
            &subject,
            &format!("{:?}", a.ownership),
            &format!("{:?}", b.ownership),
        );
    }
    let pre = |o: &Operation| -> BTreeMap<String, String> {
        o.preconditions
            .iter()
            .map(|p| (p.subject.clone(), p.state.clone()))
            .collect()
    };
    keyed(out, StaleKind::Observation, &pre(a), &pre(b));
    if (
        a.action,
        a.status,
        &a.after,
        &a.fragments,
        &a.link,
        &a.mode,
        &a.output_sha256,
    ) != (
        b.action,
        b.status,
        &b.after,
        &b.fragments,
        &b.link,
        &b.mode,
        &b.output_sha256,
    ) {
        let show = |o: &Operation| {
            format!(
                "{:?} {:?} after={:?} link={:?} mode={:?} fragments={} fragment_hashes={} output={}",
                o.action,
                o.status,
                o.after,
                o.link,
                o.mode,
                o.fragments.len(),
                o.fragments.iter().filter(|f| f.sha256.is_some()).count(),
                o.output_sha256
                    .as_deref()
                    .map_or("-", |h| h.get(..12).unwrap_or(h))
            )
        };
        diff(out, StaleKind::Operation, &subject, &show(a), &show(b));
    }
}
