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

/// The content token of a sensitive merge's fragments is a salted hash. It decides whether a plan
/// is stale and is never shown: a difference in it reads as a content change.
fn mask_content_tokens(diffs: &mut [Stale]) {
    for d in diffs
        .iter_mut()
        .filter(|d| d.subject.starts_with("fragments:"))
    {
        let head = |s: &str| s.split_once(" content=[").map(|(h, _)| h.to_string());
        if let (Some(w), Some(n)) = (head(&d.was), head(&d.now)) {
            if w == n {
                d.was = format!("{w} (fragment content before)");
                d.now = format!("{n} (fragment content changed)");
            } else {
                d.was = w;
                d.now = n;
            }
        }
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
    let from = out.len();
    keyed(out, StaleKind::Observation, &pre(a), &pre(b));
    mask_content_tokens(&mut out[from..]);
    let listed = |o: &Operation| -> Vec<(String, String)> {
        o.fragments
            .iter()
            .map(|f| (f.name.clone(), f.origin.clone()))
            .collect()
    };
    if (a.action, a.status, &a.after, listed(a), &a.link, &a.mode)
        != (b.action, b.status, &b.after, listed(b), &b.link, &b.mode)
    {
        let show = |o: &Operation| {
            format!(
                "{:?} {:?} after={:?} link={:?} mode={:?} fragments={}",
                o.action,
                o.status,
                o.after,
                o.link,
                o.mode,
                o.fragments.len()
            )
        };
        diff(out, StaleKind::Operation, &subject, &show(a), &show(b));
    }
    // Hashes are never printed. A change reads "changed", and a fragment hash change names the
    // fragment. The subjects differ, so the entries are not merged.
    if a.output_sha256 != b.output_sha256 {
        diff(
            out,
            StaleKind::Operation,
            &format!("{subject} output"),
            "as planned",
            "output changed",
        );
    }
    if a.managed_sha256 != b.managed_sha256 {
        diff(
            out,
            StaleKind::Operation,
            &format!("{subject} managed values"),
            "as planned",
            "managed values changed",
        );
    }
    for f in &a.fragments {
        let moved = b
            .fragments
            .iter()
            .find(|g| g.name == f.name && g.origin == f.origin)
            .is_some_and(|g| g.sha256 != f.sha256);
        if moved {
            diff(
                out,
                StaleKind::Operation,
                &format!("{subject} fragment {}", f.name),
                "as planned",
                &format!("fragment {} changed", f.name),
            );
        }
    }
}
