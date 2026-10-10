//! Stale-plan detection (docs/architecture.md section 10: "A mismatch exits 3 as stale").
//! A saved plan is compared with a fresh one built from the same command. Every difference is
//! named, so the reviewer sees which source, observation, ownership or topology changed.

use super::{Operation, Plan};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stale {
    /// `binary`, `model`, `source`, `operation`, `source input`, `observation` or `ownership`.
    pub what: &'static str,
    pub subject: String,
    pub was: String,
    pub now: String,
}

fn diff(out: &mut Vec<Stale>, what: &'static str, subject: &str, was: &str, now: &str) {
    if was != now {
        out.push(Stale {
            what,
            subject: subject.into(),
            was: was.into(),
            now: now.into(),
        });
    }
}

/// Differences between `old` and `new`, in a fixed order. Empty when the digests agree.
pub fn stale(old: &Plan, new: &Plan) -> Vec<Stale> {
    let mut out = Vec::new();
    diff(
        &mut out,
        "binary",
        "version",
        &old.binary.version,
        &new.binary.version,
    );
    diff(
        &mut out,
        "binary",
        "sha256",
        &old.binary.sha256,
        &new.binary.sha256,
    );
    diff(&mut out, "model", "profile", &old.profile, &new.profile);
    diff(
        &mut out,
        "model",
        "machine id",
        &old.machine_id,
        &new.machine_id,
    );
    diff(
        &mut out,
        "model",
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
    keyed(&mut out, "source", &tree(old), &tree(new));

    fn ops(p: &Plan) -> BTreeMap<&str, &Operation> {
        p.operations.iter().map(|o| (o.id.as_str(), o)).collect()
    }
    let (a, b) = (ops(old), ops(new));
    for (id, o) in &a {
        match b.get(id) {
            None => diff(&mut out, "operation", id, "present", "gone"),
            Some(n) => op_diff(&mut out, o, n),
        }
    }
    for id in b.keys().filter(|id| !a.contains_key(*id)) {
        diff(&mut out, "operation", id, "absent", "present");
    }

    // Observations that no operation precondition covers still count: a fragment appearing
    // changes the input set even when no precondition names it.
    let obs = |p: &Plan| -> BTreeMap<String, String> {
        let mut m: BTreeMap<String, String> = p
            .observations
            .seen
            .iter()
            .map(|(k, v)| (format!("path:{k}"), format!("{v:?}")))
            .collect();
        m.extend(
            p.observations
                .listings
                .iter()
                .map(|(k, v)| (format!("listing:{k}"), v.names.join(","))),
        );
        m
    };
    keyed(&mut out, "observation", &obs(old), &obs(new));
    // The most specific cause first: a source file or an owner before the tree digest that
    // contains it, and a path before the whole model.
    out.sort_by_key(|s| match s.what {
        "source input" => 0,
        "ownership" => 1,
        "observation" => 2,
        "operation" => 3,
        "source" => 4,
        "model" => 5,
        _ => 6,
    });
    out
}

fn keyed(
    out: &mut Vec<Stale>,
    what: &'static str,
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

fn op_diff(out: &mut Vec<Stale>, a: &Operation, b: &Operation) {
    let subject = format!("{} ({})", a.id, a.resource);
    let inputs = |o: &Operation| {
        o.inputs
            .iter()
            .map(|i| format!("{}={}", i.path, i.content_id.as_deref().unwrap_or("-")))
            .collect::<Vec<_>>()
            .join(";")
    };
    diff(out, "source input", &subject, &inputs(a), &inputs(b));
    diff(
        out,
        "ownership",
        &subject,
        &format!("{:?}", a.ownership),
        &format!("{:?}", b.ownership),
    );
    let pre = |o: &Operation| -> BTreeMap<String, String> {
        o.preconditions
            .iter()
            .map(|p| (p.subject.clone(), p.state.clone()))
            .collect()
    };
    let (pa, pb) = (pre(a), pre(b));
    for (k, v) in &pa {
        diff(
            out,
            "observation",
            k,
            v,
            pb.get(k).map_or("absent", String::as_str),
        );
    }
    for (k, v) in pb.iter().filter(|(k, _)| !pa.contains_key(*k)) {
        diff(out, "observation", k, "absent", v);
    }
    diff(
        out,
        "operation",
        &subject,
        &format!(
            "{:?} {:?} {:?} {:?}",
            a.action, a.status, a.after, a.fragments
        ),
        &format!(
            "{:?} {:?} {:?} {:?}",
            b.action, b.status, b.after, b.fragments
        ),
    );
}
