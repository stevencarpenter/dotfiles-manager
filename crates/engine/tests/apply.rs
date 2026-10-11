//! Apply at the engine level with hand-built plans: generated-file publication, file removal,
//! rechecks and races (DFM-18). The CLI tests cover links through the real binary. No adapter
//! exists yet, so a test generator supplies file content here.

mod common;

use common::*;
use dfm_engine::{
    Action as Inject, Dir, Fail, Generator, GeneratorInputs, NoGenerator, Run, Store, apply, gates,
    observe, owned_outputs,
};
use dfm_model::digest::{EntryKind, TreeEntry, sha256_hex, tree_digest};
use dfm_model::merge::Merged;
use dfm_model::plan::{
    Action, Binary, Fragment, Input, Observations, Operation, Ownership, Plan, Precondition,
    Request, Source, Status as PlanStatus, ledger_state, link_fingerprint, node_state,
};
use dfm_model::state::{Class, JournalEntry, LedgerResource, OwnedOutput, Phase, Status};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

static NO_ADOPT: BTreeSet<String> = BTreeSet::new();

struct Gen(BTreeMap<String, Vec<u8>>);

impl Generator for Gen {
    fn supports(&self, op: &Operation) -> bool {
        self.0.contains_key(&op.id)
    }
    fn generate(&self, op: &Operation, _: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        self.0
            .get(&op.id)
            .cloned()
            .ok_or_else(|| Refusal::new(Code::GeneratorUnavailable, "none"))
    }
}

/// The planner records the output hash of a generated file. The test plans carry none, so it is
/// filled in from the generator, as the CLI does at plan time.
fn fill(p: &Plan, g: &dyn Generator) -> Plan {
    let mut p = p.clone();
    for op in &mut p.operations {
        if op.output_sha256.is_none()
            && g.supports(op)
            && let Ok(b) = g.generate(op, &GeneratorInputs::default())
        {
            op.output_sha256 = Some(sha256_hex(&b));
        }
    }
    p
}

fn gen_for(id: &str, content: &str) -> Gen {
    Gen(BTreeMap::from([(
        id.to_string(),
        content.as_bytes().to_vec(),
    )]))
}

fn jid(n: u32) -> String {
    format!("p{n:032x}")
}

/// A planned operation whose preconditions are built from the live machine with the planner's
/// own string builders, as `plan` would.
fn op(h: &Home, store: &Store, id: &str, n: u32, action: Action, rel: &str) -> Operation {
    let sys = sys(h);
    let home = Dir::open_home(&sys, &h.0).unwrap();
    let obs = observe(
        &sys,
        &home,
        &[Request {
            path: rel.into(),
            hash: true,
            list: false,
        }],
    )
    .unwrap();
    let comps: Vec<&str> = rel.split('/').collect();
    let mut pre = Vec::new();
    for i in 0..comps.len() {
        let p = comps[..=i].join("/");
        if let Some(s) = obs.seen.get(&p) {
            pre.push(Precondition {
                subject: format!("path:{p}"),
                state: node_state(s, false),
            });
        } else {
            pre.push(Precondition {
                subject: format!("path:{p}"),
                state: "missing".into(),
            });
        }
    }
    let owned = owned_outputs(&store.read_ledger().unwrap(), &h.0).unwrap();
    let entry = owned.iter().find(|o| o.path == rel);
    pre.push(Precondition {
        subject: format!("ledger:{rel}"),
        state: ledger_state(entry, false),
    });
    Operation {
        id: id.into(),
        journal_id: jid(n),
        resource: "core.x".into(),
        module: "core".into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        action,
        status: PlanStatus::Planned,
        path: Some(rel.into()),
        mode: None,
        link: None,
        privilege: "user".into(),
        effect: String::new(),
        ownership: Ownership::Absent,
        inputs: vec![],
        fragments: vec![],
        fragment_dir: None,
        output_sha256: None,
        managed_sha256: None,
        takeover: false,
        output_bytes: None,
        excluded: None,
        sets: vec![],
        after: vec![],
        preconditions: pre,
        postcondition: String::new(),
    }
}

fn plan(ops: Vec<Operation>) -> Plan {
    plan_in("/src", ops)
}

fn plan_in(root: &str, ops: Vec<Operation>) -> Plan {
    Plan {
        schema: 1,
        command: "plan".into(),
        scope: "config".into(),
        binary: Binary {
            version: "0".into(),
            sha256: "0".repeat(64),
        },
        profile: "/p".into(),
        machine_id: "m5".into(),
        model_digest: "0".repeat(64),
        sources: vec![Source {
            kind: "path".into(),
            root: root.into(),
            tree: "sha256:0".into(),
        }],
        observations: Observations::default(),
        operations: ops,
        blockers: vec![],
    }
}

struct Setup {
    h: Home,
    store: Store,
}

fn setup() -> Setup {
    let h = Home::new();
    let (store, _) = init(&h);
    Setup { h, store }
}

fn run<'a>(
    s: &'a Setup,
    g: &'a dyn Generator,
    allow: &'a BTreeSet<String>,
    fail: &'a Fail,
    home: &'a Dir,
) -> Run<'a> {
    Run {
        store: &s.store,
        sys: sys(&s.h),
        home,
        home_path: &s.h.0,
        generator: g,
        allow,
        allow_adopt: &NO_ADOPT,
        fail,
    }
}

fn go(s: &Setup, p: &Plan, g: &dyn Generator, allow: &BTreeSet<String>) -> dfm_engine::Report {
    go_with(s, p, g, allow, &Fail::none())
}

fn go_with(
    s: &Setup,
    p: &Plan,
    g: &dyn Generator,
    allow: &BTreeSet<String>,
    fail: &Fail,
) -> dfm_engine::Report {
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let r = run(s, g, allow, fail, &home);
    apply(&r, &fill(p, g)).unwrap()
}

fn refused(s: &Setup, p: &Plan, g: &dyn Generator, allow: &BTreeSet<String>) -> Refusal {
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let fail = Fail::none();
    let r = run(s, g, allow, &fail, &home);
    apply(&r, &fill(p, g)).unwrap_err()
}

fn none() -> BTreeSet<String> {
    BTreeSet::new()
}

fn mode_of(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".stage") || n.ends_with(".tmp"))
        .collect()
}

fn own_fp(s: &Setup, rel: &str, fingerprint: &str) {
    let mut l = s.store.read_ledger().unwrap();
    l.record(LedgerResource {
        resource: "core.x".into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        op_id: "seed-1".into(),
        owned_outputs: vec![OwnedOutput {
            path: s.h.0.join(rel).to_str().unwrap().into(),
            mode: 0o600,
            fingerprint: Some(fingerprint.into()),
            coowned: None,
        }],
    })
    .unwrap();
    s.store.write_ledger(&l).unwrap();
}

fn own(s: &Setup, rel: &str, content: &str) {
    own_fp(s, rel, &sha256_hex(content.as_bytes()));
}

/// Runs `apply` parked at the failpoint `point`. `during` runs on another thread once the
/// operation is parked there, and the run continues when it returns.
fn parked(
    s: &Setup,
    p: &Plan,
    g: &dyn Generator,
    allow: &BTreeSet<String>,
    point: &str,
    during: impl FnOnce() + Send + 'static,
) -> dfm_engine::Report {
    let gate =
        s.h.0
            .parent()
            .unwrap()
            .join(format!("gate-{}-{point}", std::process::id()));
    let _ = fs::remove_file(&gate);
    let reached = gate.with_extension("reached");
    let _ = fs::remove_file(&reached);
    let fail = Fail::at(point, Inject::Wait(gate.clone()));
    let t = std::thread::spawn({
        let (gate, reached) = (gate.clone(), reached.clone());
        move || {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !reached.exists() && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            during();
            fs::write(&gate, "").unwrap();
        }
    });
    let rep = go_with(s, p, g, allow, &fail);
    t.join().unwrap();
    let _ = fs::remove_file(&reached);
    let _ = fs::remove_file(&gate);
    rep
}

#[test]
fn a_generated_file_is_staged_in_its_directory_published_with_its_mode_and_owned() {
    let s = setup();
    fs::create_dir_all(s.h.0.join(".config")).unwrap();
    let mut o = op(
        &s.h,
        &s.store,
        "x@.config/app.conf",
        1,
        Action::CreateFile,
        ".config/app.conf",
    );
    o.mode = Some(0o640);
    let r = go(
        &s,
        &plan(vec![o]),
        &gen_for("x@.config/app.conf", "a=1\n"),
        &none(),
    );
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    let p = s.h.0.join(".config/app.conf");
    assert_eq!(fs::read_to_string(&p).unwrap(), "a=1\n");
    assert_eq!(mode_of(&p), 0o640);
    assert!(leftovers(&s.h.0.join(".config")).is_empty());
    let l = s.store.read_ledger().unwrap();
    assert_eq!(
        l.resources[0].owned_outputs[0].fingerprint.as_deref(),
        Some(sha256_hex(b"a=1\n").as_str())
    );
    // Publication is recorded; the reload of its consumer is a separate state.
    assert_eq!(r.operations[0].reload.as_deref(), Some("not_attempted"));
    let j = s.store.read_journal().unwrap();
    assert_eq!(
        j.iter().map(|e| e.phase).collect::<Vec<_>>(),
        [Phase::Intent, Phase::Done]
    );
}

#[test]
fn a_created_directory_is_journaled_under_the_create_dir_name_the_claim_check_knows() {
    let s = setup();
    let d = op(&s.h, &s.store, "x@d", 1, Action::CreateDir, "d");
    let r = go(&s, &plan(vec![d]), &gens(), &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    let j = s.store.read_journal().unwrap();
    assert!(!j.is_empty());
    assert!(j.iter().all(JournalEntry::creates_dir), "{j:?}");
}

#[test]
fn the_default_file_mode_is_0600() {
    let s = setup();
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let r = go(&s, &plan(vec![o]), &gen_for("x@a", "x"), &none());
    assert_eq!(r.operations[0].status, Status::Updated);
    assert_eq!(mode_of(&s.h.0.join("a")), 0o600);
}

#[test]
fn an_owned_file_is_updated_and_its_fingerprint_follows() {
    let s = setup();
    fs::write(s.h.0.join("a"), "old\n").unwrap();
    own(&s, "a", "old\n");
    let o = op(&s.h, &s.store, "x@a", 1, Action::UpdateFile, "a");
    let r = go(&s, &plan(vec![o]), &gen_for("x@a", "new\n"), &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    assert_eq!(fs::read_to_string(s.h.0.join("a")).unwrap(), "new\n");
    let l = s.store.read_ledger().unwrap();
    assert_eq!(
        l.resources[0].owned_outputs[0].fingerprint.as_deref(),
        Some(sha256_hex(b"new\n").as_str())
    );
    assert!(leftovers(&s.h.0).is_empty());
}

#[test]
fn a_file_edited_after_planning_is_preserved_and_blocks_the_update_before_any_journal() {
    let s = setup();
    fs::write(s.h.0.join("a"), "old\n").unwrap();
    own(&s, "a", "old\n");
    let o = op(&s.h, &s.store, "x@a", 1, Action::UpdateFile, "a");
    fs::write(s.h.0.join("a"), "my edit\n").unwrap();
    let r = go(&s, &plan(vec![o]), &gen_for("x@a", "new\n"), &none());
    assert_eq!(r.operations[0].status, Status::Blocked);
    assert_eq!(r.stopped.as_ref().unwrap().code, Code::PlanStale);
    assert_eq!(fs::read_to_string(s.h.0.join("a")).unwrap(), "my edit\n");
    assert!(
        s.store.read_journal().unwrap().is_empty(),
        "no intent for a blocked operation"
    );
}

#[test]
fn a_file_that_appears_between_the_last_check_and_the_publish_is_never_replaced() {
    let s = setup();
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let target = s.h.0.join("a");
    let rep = parked(
        &s,
        &plan(vec![o]),
        &gen_for("x@a", "mine\n"),
        &none(),
        "file-after-tmp",
        move || fs::write(&target, "foreign\n").unwrap(),
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    assert_eq!(rep.stopped.as_ref().unwrap().code, Code::Conflict);
    assert_eq!(fs::read_to_string(s.h.0.join("a")).unwrap(), "foreign\n");
    assert!(leftovers(&s.h.0).is_empty(), "the staged file is removed");
    assert!(s.store.read_ledger().unwrap().resources.is_empty());
    let phases: Vec<_> = s
        .store
        .read_journal()
        .unwrap()
        .iter()
        .map(|e| e.phase)
        .collect();
    assert_eq!(phases, [Phase::Intent, Phase::Failed]);
}

#[test]
fn an_owned_file_edited_after_the_last_check_is_not_replaced_by_the_update() {
    // Publish::Replace compares the live content with the ledger fingerprint just before the
    // rename. The edit lands after every earlier check, while the staged file waits.
    let s = setup();
    fs::write(s.h.0.join("a"), "old\n").unwrap();
    own(&s, "a", "old\n");
    let o = op(&s.h, &s.store, "x@a", 1, Action::UpdateFile, "a");
    let target = s.h.0.join("a");
    let rep = parked(
        &s,
        &plan(vec![o]),
        &gen_for("x@a", "new\n"),
        &none(),
        "file-after-tmp",
        move || fs::write(&target, "my edit\n").unwrap(),
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    assert_eq!(rep.stopped.as_ref().unwrap().code, Code::Conflict);
    assert_eq!(fs::read_to_string(s.h.0.join("a")).unwrap(), "my edit\n");
    assert!(leftovers(&s.h.0).is_empty());
    assert_eq!(
        s.store.read_ledger().unwrap().resources[0].owned_outputs[0]
            .fingerprint
            .as_deref(),
        Some(sha256_hex(b"old\n").as_str()),
        "the ledger still describes what the manager wrote"
    );
}

#[test]
fn a_failure_before_the_commit_leaves_no_file_and_no_staged_name() {
    for point in ["file-before-write", "file-after-tmp"] {
        let s = setup();
        let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
        let rep = go_with(
            &s,
            &plan(vec![o]),
            &gen_for("x@a", "x"),
            &none(),
            &Fail::at(point, Inject::Error),
        );
        assert_eq!(rep.operations[0].status, Status::Failed, "{point}");
        assert!(!s.h.0.join("a").exists(), "{point}");
        assert!(leftovers(&s.h.0).is_empty(), "{point}");
        assert!(
            s.store.read_ledger().unwrap().resources.is_empty(),
            "{point}"
        );
    }
}

/// A dependent operation follows the one under test.
fn with_dependent(s: &Setup, first: Operation) -> Plan {
    let mut dep = op(&s.h, &s.store, "x@dep", 99, Action::CreateFile, "dep");
    dep.after = vec![first.id.clone()];
    plan(vec![first, dep])
}

fn gens() -> Gen {
    Gen(BTreeMap::from([
        ("x@file".to_string(), b"new\n".to_vec()),
        ("x@dep".to_string(), b"dep\n".to_vec()),
    ]))
}

/// The effect is in place and only its durability is unknown: the operation is `unknown`, not
/// `failed`, the journal says so, dependents are blocked, and nothing is recorded as owned.
fn assert_unknown(s: &Setup, rep: &dfm_engine::Report, what: &str) {
    assert_eq!(rep.operations[0].status, Status::Unknown, "{what}");
    assert_eq!(rep.operations[1].status, Status::Blocked, "{what}");
    assert_eq!(
        rep.stopped.as_ref().unwrap().code,
        Code::IoUnconfirmed,
        "{what}"
    );
    let j = s.store.read_journal().unwrap();
    assert_eq!(j.last().unwrap().phase, Phase::Unknown, "{what}");
    assert_eq!(
        rep.operations[0].id, j[0].op_id,
        "{what}: receipt id is the attempt id"
    );
    assert!(!s.h.0.join("dep").exists(), "{what}");
}

#[test]
fn an_unconfirmed_durability_result_is_unknown_for_every_primitive() {
    // Directory.
    let s = setup();
    let d = op(&s.h, &s.store, "x@dir", 1, Action::CreateDir, "d");
    let rep = go_with(
        &s,
        &with_dependent(&s, d),
        &gens(),
        &none(),
        &Fail::at("dir-after-create", Inject::Error),
    );
    assert_unknown(&s, &rep, "dir");
    assert!(s.h.0.join("d").is_dir());

    // New link.
    let s = setup();
    let mut l = op(&s.h, &s.store, "x@file", 1, Action::CreateLink, "l");
    l.link = Some("/src/x".into());
    let rep = go_with(
        &s,
        &with_dependent(&s, l),
        &gens(),
        &none(),
        &Fail::at("link-after-create", Inject::Error),
    );
    assert_unknown(&s, &rep, "link");
    assert!(fs::symlink_metadata(s.h.0.join("l")).is_ok());
    assert!(s.store.read_ledger().unwrap().resources.is_empty());

    // Replaced link.
    let s = setup();
    symlink("/src/old", s.h.0.join("l")).unwrap();
    own_fp(&s, "l", &link_fingerprint("/src/old"));
    let mut l = op(&s.h, &s.store, "x@file", 1, Action::ReplaceLink, "l");
    l.link = Some("/src/new".into());
    let rep = go_with(
        &s,
        &with_dependent(&s, l),
        &gens(),
        &none(),
        &Fail::at("link-after-rename", Inject::Error),
    );
    assert_unknown(&s, &rep, "replace link");
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new("/src/new")
    );

    // New file.
    let s = setup();
    let f = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    let rep = go_with(
        &s,
        &with_dependent(&s, f),
        &gens(),
        &none(),
        &Fail::at("file-after-rename", Inject::Error),
    );
    assert_unknown(&s, &rep, "file");
    assert_eq!(fs::read_to_string(s.h.0.join("f")).unwrap(), "new\n");

    // Replaced file.
    let s = setup();
    fs::write(s.h.0.join("f"), "old\n").unwrap();
    own(&s, "f", "old\n");
    let f = op(&s.h, &s.store, "x@file", 1, Action::UpdateFile, "f");
    let rep = go_with(
        &s,
        &with_dependent(&s, f),
        &gens(),
        &none(),
        &Fail::at("file-after-rename", Inject::Error),
    );
    assert_unknown(&s, &rep, "update");

    // Removed file.
    let s = setup();
    fs::write(s.h.0.join("f"), "old\n").unwrap();
    own(&s, "f", "old\n");
    let id = "remove@core.x@f";
    let r = op(&s.h, &s.store, id, 1, Action::Remove, "f");
    let allow = BTreeSet::from([id.to_string()]);
    let rep = go_with(
        &s,
        &with_dependent(&s, r),
        &gens(),
        &allow,
        &Fail::at("remove-after-unlink", Inject::Error),
    );
    assert_unknown(&s, &rep, "remove");
    assert!(!s.h.0.join("f").exists());
}

#[test]
fn the_receipt_id_of_a_failed_operation_is_its_journal_attempt_id() {
    let s = setup();
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let rep = go_with(
        &s,
        &plan(vec![o]),
        &gen_for("x@a", "x"),
        &none(),
        &Fail::at("apply-before-effect", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    let j = s.store.read_journal().unwrap();
    assert_eq!(rep.operations[0].id, j[0].op_id);
    assert_eq!(rep.operations[0].id, format!("{}-1", jid(1)));
}

#[test]
fn an_owned_file_that_still_matches_is_removed_and_its_ledger_entry_goes() {
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    fs::write(s.h.0.join("sibling"), "not mine\n").unwrap();
    own(&s, "a", "mine\n");
    let o = op(&s.h, &s.store, "remove@core.x@a", 1, Action::Remove, "a");
    let allow = BTreeSet::from(["remove@core.x@a".to_string()]);
    let r = go(&s, &plan(vec![o]), &NoGenerator, &allow);
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    assert!(!s.h.0.join("a").exists());
    assert_eq!(
        fs::read_to_string(s.h.0.join("sibling")).unwrap(),
        "not mine\n"
    );
    assert!(s.store.read_ledger().unwrap().resources.is_empty());
}

#[test]
fn a_removal_without_its_allowance_is_refused_with_every_id_as_a_flag() {
    let s = setup();
    for f in ["a", "b"] {
        fs::write(s.h.0.join(f), "x").unwrap();
    }
    own(&s, "a", "x");
    let o1 = op(&s.h, &s.store, "remove@core.x@a", 1, Action::Remove, "a");
    let mut o2 = op(&s.h, &s.store, "remove@core.x@b", 2, Action::Remove, "b");
    o2.resource = "core.y".into();
    let p = plan(vec![o1, o2]);
    let e = refused(&s, &p, &NoGenerator, &none());
    assert_eq!(e.code, Code::DisruptiveUnapproved);
    assert!(
        e.detail.contains("--allow-disruptive 'remove@core.x@a'"),
        "{}",
        e.detail
    );
    assert!(
        e.detail.contains("--allow-disruptive 'remove@core.x@b'"),
        "{}",
        e.detail
    );
    assert!(s.h.0.join("a").exists() && s.h.0.join("b").exists());
}

#[test]
fn an_owned_file_edited_after_planning_is_not_removed() {
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    own(&s, "a", "mine\n");
    let o = op(&s.h, &s.store, "remove@core.x@a", 1, Action::Remove, "a");
    fs::write(s.h.0.join("a"), "mine, edited\n").unwrap();
    let allow = BTreeSet::from(["remove@core.x@a".to_string()]);
    let r = go(&s, &plan(vec![o]), &NoGenerator, &allow);
    assert_eq!(r.operations[0].status, Status::Blocked);
    assert_eq!(
        fs::read_to_string(s.h.0.join("a")).unwrap(),
        "mine, edited\n"
    );
    assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);
}

#[test]
fn removal_never_follows_a_leaf_link_swapped_in_before_or_during_the_delete() {
    // Swapped after planning: the recorded state no longer matches.
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    fs::write(s.h.0.join("victim"), "keep me\n").unwrap();
    own(&s, "a", "mine\n");
    let o = op(&s.h, &s.store, "remove@core.x@a", 1, Action::Remove, "a");
    fs::remove_file(s.h.0.join("a")).unwrap();
    symlink(s.h.0.join("victim"), s.h.0.join("a")).unwrap();
    let allow = BTreeSet::from(["remove@core.x@a".to_string()]);
    let r = go(&s, &plan(vec![o]), &NoGenerator, &allow);
    assert_eq!(r.operations[0].status, Status::Blocked);
    assert_eq!(
        fs::read_to_string(s.h.0.join("victim")).unwrap(),
        "keep me\n"
    );
    assert!(
        fs::symlink_metadata(s.h.0.join("a"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // Swapped while the delete waits, after every recheck: the delete still refuses.
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    fs::write(s.h.0.join("victim"), "keep me\n").unwrap();
    own(&s, "a", "mine\n");
    let o = op(&s.h, &s.store, "remove@core.x@a", 1, Action::Remove, "a");
    let home_path = s.h.0.clone();
    let allow = BTreeSet::from(["remove@core.x@a".to_string()]);
    let rep = parked(
        &s,
        &plan(vec![o]),
        &NoGenerator,
        &allow,
        "apply-before-effect",
        move || {
            fs::remove_file(home_path.join("a")).unwrap();
            symlink(home_path.join("victim"), home_path.join("a")).unwrap();
        },
    );
    assert_eq!(
        rep.operations[0].status,
        Status::Failed,
        "{:?}",
        rep.stopped
    );
    assert_eq!(
        fs::read_to_string(s.h.0.join("victim")).unwrap(),
        "keep me\n"
    );
    assert!(
        fs::symlink_metadata(s.h.0.join("a"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        s.store.read_ledger().unwrap().resources.len(),
        1,
        "ownership is kept"
    );
}

#[test]
fn a_different_file_with_identical_bytes_swapped_in_before_the_unlink_is_kept() {
    // The hash cannot tell the two files apart. Only the identity check (device and inode)
    // sees that the name no longer refers to the file that was hashed.
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    own(&s, "a", "mine\n");
    let o = op(&s.h, &s.store, "remove@core.x@a", 1, Action::Remove, "a");
    let home_path = s.h.0.clone();
    let allow = BTreeSet::from(["remove@core.x@a".to_string()]);
    let rep = parked(
        &s,
        &plan(vec![o]),
        &NoGenerator,
        &allow,
        "apply-before-unlink",
        move || {
            fs::write(home_path.join("a.new"), "mine\n").unwrap();
            fs::rename(home_path.join("a.new"), home_path.join("a")).unwrap();
        },
    );
    assert_eq!(
        rep.operations[0].status,
        Status::Failed,
        "{:?}",
        rep.stopped
    );
    assert_eq!(rep.stopped.as_ref().unwrap().code, Code::Conflict);
    assert!(
        rep.stopped
            .as_ref()
            .unwrap()
            .detail
            .contains("replaced after it was checked")
    );
    assert_eq!(fs::read_to_string(s.h.0.join("a")).unwrap(), "mine\n");
    assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);
}

#[test]
fn a_replaced_link_is_compared_with_the_ledger_fingerprint_not_the_earlier_peek() {
    let s = setup();
    symlink("/src/other", s.h.0.join("l")).unwrap();
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let e = home
        .replace_link(
            "l",
            "/src/new",
            &link_fingerprint("/src/old"),
            &Fail::none(),
        )
        .unwrap_err();
    assert_eq!(e.code, Code::Conflict);
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new("/src/other")
    );
    assert!(leftovers(&s.h.0).is_empty());
    // And the matching fingerprint replaces it.
    home.replace_link(
        "l",
        "/src/new",
        &link_fingerprint("/src/other"),
        &Fail::none(),
    )
    .unwrap();
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new("/src/new")
    );
}

#[test]
fn a_directory_is_never_removed_and_nothing_is_deleted_recursively() {
    let s = setup();
    fs::create_dir_all(s.h.0.join("d/inner")).unwrap();
    fs::write(s.h.0.join("d/inner/x"), "x").unwrap();
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let e = home.remove_entry("d", &Fail::none()).unwrap_err();
    assert_eq!(e.code, Code::Conflict);
    assert!(s.h.0.join("d/inner/x").is_file());
}

#[test]
fn a_parent_that_is_a_link_or_a_file_is_not_opened() {
    let s = setup();
    fs::create_dir_all(s.h.0.join("real")).unwrap();
    symlink(s.h.0.join("real"), s.h.0.join("link")).unwrap();
    fs::write(s.h.0.join("file"), "x").unwrap();
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    for rel in [
        vec!["link"],
        vec!["file"],
        vec!["missing"],
        vec!["real", "nope"],
    ] {
        let e = home.open_below(&sys(&s.h), &rel).unwrap_err();
        assert_eq!(e.code, Code::ParentUnsafe, "{rel:?}");
    }
    assert!(home.open_below(&sys(&s.h), &["real"]).is_ok());
}

#[test]
fn a_plan_that_needs_content_no_generator_makes_is_refused_before_any_write() {
    let s = setup();
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let p = plan(vec![o]);
    assert_eq!(
        refused(&s, &p, &NoGenerator, &none()).code,
        Code::GeneratorUnavailable
    );
    assert!(s.store.read_journal().unwrap().is_empty() && !s.h.0.join("a").exists());
}

#[test]
fn a_sensitive_output_is_refused_whatever_its_action_and_with_its_own_code() {
    let s = setup();
    let g = Gen(BTreeMap::from([("x@a".to_string(), b"secret".to_vec())]));
    for action in [
        Action::CreateFile,
        Action::CreateLink,
        Action::ReplaceLink,
        Action::UpdateFile,
    ] {
        let mut o = op(&s.h, &s.store, "x@a", 1, action, "a");
        o.class = Class::Sensitive;
        o.link = Some("/src/x".into());
        let e = refused(&s, &plan(vec![o]), &g, &none());
        assert_eq!(e.code, Code::SensitiveUnsupported, "{action:?}");
    }
    assert!(s.store.read_journal().unwrap().is_empty() && !s.h.0.join("a").exists());
}

/// A generator that runs sensitive merges and nothing else.
struct MergeOnly;

impl Generator for MergeOnly {
    fn supports(&self, _: &Operation) -> bool {
        false
    }
    fn generate(&self, _: &Operation, _: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        Err(Refusal::new(Code::GeneratorUnavailable, "none"))
    }
    fn merges(&self, op: &Operation) -> bool {
        matches!(
            op.action,
            Action::CreateFile | Action::UpdateFile | Action::Retire
        )
    }
}

#[test]
fn only_a_merge_the_generator_runs_is_admitted_among_sensitive_operations() {
    let s = setup();
    // Admitted: a merge with its managed fingerprint. The gate passes (nothing runs here).
    let mut m = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    m.class = Class::Sensitive;
    m.managed_sha256 = Some("a".repeat(64));
    assert!(gates(&plan(vec![m.clone()]), &MergeOnly).refusal.is_none());
    // A merge without its fingerprint is a malformed plan, not a sensitive refusal.
    let mut bad = m.clone();
    bad.managed_sha256 = None;
    let r = gates(&plan(vec![bad]), &MergeOnly).refusal.unwrap();
    assert_eq!(r.code, Code::PlanInvalid);
    // Retire carries none.
    let mut retire = m.clone();
    retire.action = Action::Retire;
    retire.managed_sha256 = None;
    assert!(gates(&plan(vec![retire]), &MergeOnly).refusal.is_none());
    // Every other sensitive operation is still refused, and the plan is refused whole.
    for action in [Action::CreateLink, Action::ReplaceLink, Action::Remove] {
        let mut o = op(&s.h, &s.store, "x@b", 2, action, "b");
        o.class = Class::Sensitive;
        o.link = Some("/src/x".into());
        let r = gates(&plan(vec![m.clone(), o]), &MergeOnly)
            .refusal
            .unwrap();
        assert_eq!(r.code, Code::SensitiveUnsupported, "{action:?}");
    }
    // The same operation under a generator that does not merge is refused.
    let r = gates(&plan(vec![m]), &NoGenerator).refusal.unwrap();
    assert_eq!(r.code, Code::SensitiveUnsupported);
}

#[test]
fn generated_content_is_never_copied_into_state_documents() {
    let s = setup();
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let r = go(
        &s,
        &plan(vec![o]),
        &gen_for("x@a", "TOPSECRET-VALUE"),
        &none(),
    );
    assert_eq!(r.operations[0].status, Status::Updated);
    let state = s.h.0.join(".local/state/dotfiles-manager/state");
    for f in files_below(&state) {
        if f.is_file() {
            let b = fs::read(&f).unwrap();
            assert!(
                !String::from_utf8_lossy(&b).contains("TOPSECRET"),
                "{} holds generated content",
                f.display()
            );
        }
    }
}

#[test]
fn dependents_of_a_failed_operation_are_blocked_and_the_rest_are_not_started() {
    let s = setup();
    fs::create_dir_all(s.h.0.join("d")).unwrap();
    let a = op(&s.h, &s.store, "x@d/a", 1, Action::CreateFile, "d/a");
    let mut b = op(&s.h, &s.store, "x@d/b", 2, Action::CreateFile, "d/b");
    b.after = vec!["x@d/a".into()];
    let c = op(&s.h, &s.store, "x@c", 3, Action::CreateFile, "c");
    let g = Gen(BTreeMap::from([
        ("x@d/a".to_string(), b"1".to_vec()),
        ("x@d/b".to_string(), b"2".to_vec()),
        ("x@c".to_string(), b"3".to_vec()),
    ]));
    let rep = go_with(
        &s,
        &plan(vec![a, b, c]),
        &g,
        &none(),
        &Fail::at("file-before-write", Inject::Error),
    );
    let st: Vec<_> = rep.operations.iter().map(|o| o.status).collect();
    assert_eq!(st, [Status::Failed, Status::Blocked, Status::NotStarted]);
    assert!(rep.operations[2].started_at.is_none());
    assert!(!s.h.0.join("c").exists() && !s.h.0.join("d/b").exists());
}

#[test]
fn a_journal_id_is_never_reused_across_runs() {
    let s = setup();
    for run_no in 1..=3u32 {
        // A new file each time keeps the plan valid; the planner id is the same every time.
        let rel = format!("f{run_no}");
        let o = op(&s.h, &s.store, "same-op", 7, Action::CreateFile, &rel);
        let g = gen_for("same-op", "x");
        let r = go(&s, &plan(vec![o]), &g, &none());
        assert_eq!(r.operations[0].status, Status::Updated);
    }
    let ids: Vec<_> = s
        .store
        .read_journal()
        .unwrap()
        .into_iter()
        .filter(|e| e.phase == Phase::Intent)
        .map(|e| e.op_id)
        .collect();
    assert_eq!(
        ids,
        [
            format!("{}-1", jid(7)),
            format!("{}-2", jid(7)),
            format!("{}-3", jid(7))
        ]
    );
}

#[test]
fn a_link_is_checked_against_the_root_of_its_own_source() {
    let s = setup();
    // The second source exists, because a link reads its source before it is published.
    let second = s.h.0.join(".second");
    fs::create_dir_all(second.join("files")).unwrap();
    fs::write(second.join("files/x"), "x").unwrap();
    let second = second.to_str().unwrap().to_string();
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::CreateLink, "l");
    o.link = Some(format!("{second}/files/x"));
    o.inputs = vec![dfm_model::plan::Input {
        path: "files/x".into(),
        content_id: None,
    }];
    let mut p = plan(vec![o]);
    p.sources.push(Source {
        kind: "path".into(),
        root: second,
        tree: "sha256:1".into(),
    });
    // The link lies in the second source, not the first.
    let r = go(&s, &p, &NoGenerator, &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    // A target outside every root is refused, and so is one that names another file than the
    // operation's input.
    for bad in [
        "/elsewhere/files/x",
        "/second/other",
        "/src/../etc/passwd",
        "/second/files/../x",
    ] {
        let s = setup();
        let mut o = op(&s.h, &s.store, "x@l", 1, Action::CreateLink, "l");
        o.link = Some(bad.into());
        o.inputs = vec![dfm_model::plan::Input {
            path: "files/x".into(),
            content_id: None,
        }];
        let mut p = plan(vec![o]);
        p.sources.push(Source {
            kind: "path".into(),
            root: "/second".into(),
            tree: "sha256:1".into(),
        });
        assert_eq!(
            refused(&s, &p, &NoGenerator, &none()).code,
            Code::PlanInvalid,
            "{bad}"
        );
        assert!(fs::symlink_metadata(s.h.0.join("l")).is_err());
    }
}

#[test]
fn an_absent_removal_candidate_drops_its_stale_ledger_entry() {
    let s = setup();
    own(&s, "gone", "x");
    let mut o = op(
        &s.h,
        &s.store,
        "remove@core.x@gone",
        1,
        Action::Remove,
        "gone",
    );
    o.status = PlanStatus::Current;
    let r = go(&s, &plan(vec![o]), &NoGenerator, &none());
    assert_eq!(r.operations[0].status, Status::Current);
    assert!(s.store.read_ledger().unwrap().resources.is_empty());
    let phases: Vec<_> = s
        .store
        .read_journal()
        .unwrap()
        .iter()
        .map(|e| e.phase)
        .collect();
    assert_eq!(phases, [Phase::Intent, Phase::Done]);
}

#[test]
fn a_full_journal_refuses_before_any_write() {
    let s = setup();
    // Fill the journal with terminal entries up to just under the cap.
    let e = JournalEntry {
        phase: Phase::Done,
        recovery: None,
        ..intent("create-link", &s.h.0.join("z").to_string_lossy())
    };
    let line = e.to_line().unwrap();
    let count = (dfm_model::state::MAX_JOURNAL_BYTES as usize - 2048) / line.len();
    let path =
        s.h.0
            .join(".local/state/dotfiles-manager/state/journal.jsonl");
    let mut bytes = Vec::new();
    for _ in 0..count {
        bytes.extend_from_slice(&line);
    }
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let before = fs::read(&path).unwrap();
    let err = refused(&s, &plan(vec![o]), &gen_for("x@a", "x"), &none());
    assert_eq!(err.code, Code::LimitExceeded);
    assert!(err.detail.contains("DFM-19"), "{}", err.detail);
    assert_eq!(fs::read(&path).unwrap(), before, "nothing was appended");
    assert!(!s.h.0.join("a").exists());
}

#[test]
fn a_failure_while_settling_earlier_work_still_returns_a_report() {
    // An earlier run left an intent entry. Writing its outcome fails. The run stops before its
    // first operation, but the caller still gets a report to print and record.
    let h = Home::new();
    let (first, _) = init(&h);
    first
        .append_journal(&intent("create-link", &h.0.join("z").to_string_lossy()))
        .unwrap();
    drop(first);
    let store = Store::open_write(
        sys(&h),
        &h.layout(),
        Fail::at("journal-before-write", Inject::Error),
    )
    .unwrap();
    let s = Setup { h, store };
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let rep = go(&s, &plan(vec![o]), &gen_for("x@a", "x"), &none());
    assert_eq!(rep.stopped.as_ref().unwrap().code, Code::IoFailed);
    assert_eq!(rep.operations[0].status, Status::NotStarted);
    assert!(!s.h.0.join("a").exists());
}

#[test]
fn the_journal_size_is_the_stored_size_not_a_reserialized_sum() {
    // Each line is padded with spaces after its closing brace. The parser accepts it, a
    // re-serialized sum would not see the padding, and the stored size is over the cap.
    let s = setup();
    let e = JournalEntry {
        phase: Phase::Done,
        recovery: None,
        ..intent("create-link", &s.h.0.join("z").to_string_lossy())
    };
    let mut line = e.to_line().unwrap();
    line.pop();
    line.extend_from_slice(&[b' '; 600]);
    line.push(b'\n');
    let count = (dfm_model::state::MAX_JOURNAL_BYTES as usize - 2048) / line.len();
    let path =
        s.h.0
            .join(".local/state/dotfiles-manager/state/journal.jsonl");
    fs::write(&path, line.repeat(count)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(s.store.journal_len().unwrap() > dfm_model::state::MAX_JOURNAL_BYTES - 4096);
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let err = refused(&s, &plan(vec![o]), &gen_for("x@a", "x"), &none());
    assert_eq!(err.code, Code::LimitExceeded);
    assert!(!s.h.0.join("a").exists());
}

/// A store whose own failpoint is armed, after `prepare` ran on a normal one.
fn rearmed(prepare: impl FnOnce(&Setup), fail: Fail) -> Setup {
    let h = Home::new();
    let (store, _) = init(&h);
    let s = Setup { h, store };
    prepare(&s);
    let Setup { h, store } = s;
    drop(store);
    let store = Store::open_write(sys(&h), &h.layout(), fail).unwrap();
    Setup { h, store }
}

fn absent_candidate(s: &Setup) -> Plan {
    let mut o = op(
        &s.h,
        &s.store,
        "remove@core.x@gone",
        1,
        Action::Remove,
        "gone",
    );
    o.status = PlanStatus::Current;
    plan(vec![o])
}

#[test]
fn a_file_that_appears_while_a_stale_entry_is_pruned_keeps_its_ledger_entry() {
    let gate = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("prune-gate-{}", std::process::id()));
    let _ = fs::remove_file(&gate);
    let reached = gate.with_extension("reached");
    let _ = fs::remove_file(&reached);
    let s = rearmed(
        |s| own(s, "gone", "x"),
        Fail::at("journal-before-write", Inject::Wait(gate.clone())),
    );
    let p = absent_candidate(&s);
    let leaf = s.h.0.join("gone");
    let t = std::thread::spawn({
        let (gate, reached) = (gate.clone(), reached.clone());
        move || {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !reached.exists() && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            fs::write(&leaf, "x").unwrap();
            fs::write(&gate, "").unwrap();
        }
    });
    let rep = go(&s, &p, &NoGenerator, &none());
    t.join().unwrap();
    let _ = fs::remove_file(&reached);
    let _ = fs::remove_file(&gate);
    assert_eq!(
        rep.operations[0].status,
        Status::Current,
        "{:?}",
        rep.stopped
    );
    assert_eq!(
        s.store.read_ledger().unwrap().resources.len(),
        1,
        "the entry survives"
    );
    assert!(s.h.0.join("gone").is_file());
}

#[test]
fn a_stale_entry_whose_ledger_write_is_unconfirmed_is_unknown() {
    let s = rearmed(
        |s| own(s, "gone", "x"),
        Fail::at("ledger-after-rename", Inject::Error),
    );
    let p = absent_candidate(&s);
    let rep = go(&s, &p, &NoGenerator, &none());
    assert_eq!(rep.operations[0].status, Status::Unknown);
    assert_eq!(rep.stopped.as_ref().unwrap().code, Code::IoUnconfirmed);
}

// ---- adoption (DFM-45) ------------------------------------------------------------------

fn adopt_op(s: &Setup) -> Operation {
    symlink("/src/x", s.h.0.join("l")).unwrap();
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::Adopt, "l");
    o.link = Some("/src/x".into());
    o
}

fn go_adopt(
    s: &Setup,
    p: &Plan,
    allow_adopt: &BTreeSet<String>,
    fail: &Fail,
) -> dfm_engine::Report {
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let no = none();
    let mut r = run(s, &NoGenerator, &no, fail, &home);
    r.allow_adopt = allow_adopt;
    apply(&r, p).unwrap()
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn an_adoption_records_the_link_and_writes_nothing_to_home() {
    let s = setup();
    let p = plan(vec![adopt_op(&s)]);
    let before = fs::symlink_metadata(s.h.0.join("l"))
        .unwrap()
        .modified()
        .unwrap();
    let r = go_adopt(&s, &p, &ids(&["x@l"]), &Fail::none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    let rec = &r.operations[0];
    assert!(rec.before.is_none());
    assert_eq!(
        rec.after.as_ref().map(|f| f.fingerprint.as_str()),
        Some(link_fingerprint("/src/x").as_str())
    );
    assert_eq!(rec.reload, None, "nothing was published");
    assert_eq!(rec.owned_outputs, [s.h.0.join("l").to_str().unwrap()]);
    // The link is untouched.
    assert_eq!(fs::read_link(s.h.0.join("l")).unwrap(), Path::new("/src/x"));
    assert_eq!(
        fs::symlink_metadata(s.h.0.join("l"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    let l = s.store.read_ledger().unwrap();
    let out = &l.resources[0].owned_outputs[0];
    assert_eq!(
        out.fingerprint.as_deref(),
        Some(link_fingerprint("/src/x").as_str())
    );
    assert_eq!(out.mode, 0o777);
    let j = s.store.read_journal().unwrap();
    assert_eq!(
        j.iter()
            .map(|e| (e.op.as_str(), e.phase))
            .collect::<Vec<_>>(),
        [("adopt", Phase::Intent), ("adopt", Phase::Done)]
    );
    let out = &j[0].recovery.as_ref().unwrap().outputs[0];
    assert!(out.existed_before);
    // Owned now: the same link is no longer an adoption but a plain current output.
    assert!(
        s.store
            .authorizes_delete(
                "core.x",
                s.h.0.join("l").to_str().unwrap(),
                &link_fingerprint("/src/x")
            )
            .unwrap()
    );
}

#[test]
fn an_adoption_needs_its_own_allowance_and_an_unknown_id_is_a_usage_error() {
    let s = setup();
    let p = plan(vec![adopt_op(&s)]);
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let fail = Fail::none();
    let no = none();
    let r = run(&s, &NoGenerator, &no, &fail, &home);
    let e = apply(&r, &p).unwrap_err();
    assert_eq!(e.code, Code::AdoptUnapproved);
    assert!(e.detail.contains("--allow-adopt 'x@l'"), "{}", e.detail);
    assert!(s.store.read_journal().unwrap().is_empty());
    assert!(s.store.read_ledger().unwrap().resources.is_empty());
    // An allow-disruptive id does not approve an adoption: it names no removal.
    let allow = ids(&["x@l"]);
    let r = run(&s, &NoGenerator, &allow, &fail, &home);
    assert_eq!(apply(&r, &p).unwrap_err().code, Code::Usage);
    // An id that names no adoption is usage.
    let mut r = run(&s, &NoGenerator, &no, &fail, &home);
    let bad = ids(&["x@l", "other"]);
    r.allow_adopt = &bad;
    assert_eq!(apply(&r, &p).unwrap_err().code, Code::Usage);
}

#[test]
fn an_adoption_blocks_when_the_leaf_changed_since_the_plan() {
    let s = setup();
    let p = plan(vec![adopt_op(&s)]);
    fs::remove_file(s.h.0.join("l")).unwrap();
    symlink("/src/y", s.h.0.join("l")).unwrap();
    let r = go_adopt(&s, &p, &ids(&["x@l"]), &Fail::none());
    assert_eq!(r.operations[0].status, Status::Blocked);
    assert_eq!(r.stopped.as_ref().unwrap().code, Code::PlanStale);
    assert!(s.store.read_ledger().unwrap().resources.is_empty());
    assert!(s.store.read_journal().unwrap().is_empty());
}

#[test]
fn an_adoption_whose_leaf_is_swapped_after_the_checks_writes_no_ledger_entry() {
    for point in ["apply-before-effect", "apply-after-effect"] {
        let s = setup();
        let p = plan(vec![adopt_op(&s)]);
        let gate =
            s.h.0
                .parent()
                .unwrap()
                .join(format!("adopt-gate-{}-{point}", std::process::id()));
        let _ = fs::remove_file(&gate);
        let reached = gate.with_extension("reached");
        let _ = fs::remove_file(&reached);
        let fail = Fail::at(point, Inject::Wait(gate.clone()));
        let leaf = s.h.0.join("l");
        let t = std::thread::spawn({
            let (gate, reached) = (gate.clone(), reached.clone());
            move || {
                let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while !reached.exists() && std::time::Instant::now() < end {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                fs::remove_file(&leaf).unwrap();
                symlink("/src/y", &leaf).unwrap();
                fs::write(&gate, "").unwrap();
            }
        });
        let r = go_adopt(&s, &p, &ids(&["x@l"]), &fail);
        t.join().unwrap();
        assert_eq!(r.operations[0].status, Status::Failed, "{point}");
        assert_eq!(r.stopped.as_ref().unwrap().code, Code::Conflict, "{point}");
        assert!(
            s.store.read_ledger().unwrap().resources.is_empty(),
            "{point}"
        );
        let j = s.store.read_journal().unwrap();
        assert_eq!(
            j.iter().map(|e| e.phase).collect::<Vec<_>>(),
            [Phase::Intent, Phase::Failed],
            "{point}"
        );
        assert_eq!(fs::read_link(s.h.0.join("l")).unwrap(), Path::new("/src/y"));
        let _ = fs::remove_file(&reached);
        let _ = fs::remove_file(&gate);
    }
}

#[test]
fn an_adoption_target_outside_the_source_root_is_refused() {
    let s = setup();
    symlink("/elsewhere/x", s.h.0.join("l")).unwrap();
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::Adopt, "l");
    o.link = Some("/elsewhere/x".into());
    let e = refused(&s, &plan(vec![o]), &NoGenerator, &none());
    assert_eq!(e.code, Code::PlanInvalid);
}

// ---- the generator seam: verified inputs, planned output hash, last-good output ------------

/// Joins the body and the listed fragments with a newline, as `text-fragments/v1` does. A name
/// ending in `.md` in the live listing that the plan does not list is stale.
struct Joiner;

impl Generator for Joiner {
    fn supports(&self, op: &Operation) -> bool {
        op.adapter == "text-fragments/v1"
    }
    fn generate(&self, _op: &Operation, i: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        if let Some(n) = i
            .listing
            .iter()
            .flatten()
            .find(|n| n.ends_with(".md") && !i.fragments.contains_key(*n))
        {
            return Err(Refusal::new(Code::PlanStale, format!("{n} appeared")));
        }
        let body = i
            .sources
            .get("files/body")
            .ok_or_else(|| Refusal::new(Code::PlanInvalid, "no body"))?;
        let mut parts = vec![body.clone()];
        parts.extend(i.fragments.values().cloned());
        Ok(parts.join(&b'\n'))
    }
}

/// Always fails to generate.
struct Failing;

impl Generator for Failing {
    fn supports(&self, _op: &Operation) -> bool {
        true
    }
    fn generate(&self, _op: &Operation, _i: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        Err(Refusal::new(
            Code::ManifestInvalid,
            "the value is malformed",
        ))
    }
}

struct Asm {
    plan: Plan,
    src: std::path::PathBuf,
}

/// An assemble plan: one `out` file from `files/body` and the foreign fragments in `frags/`.
fn asm(s: &Setup, tag: &str, body: &str, frags: &[(&str, &[u8])]) -> Asm {
    // The source tree sits in the test HOME, so it is removed with it.
    let src = s.h.0.join(format!(".src-{tag}"));
    fs::create_dir_all(src.join("files")).unwrap();
    fs::write(src.join("files/body"), body).unwrap();
    fs::create_dir_all(s.h.0.join("frags")).unwrap();
    let mut fragments = Vec::new();
    let mut want = vec![body.as_bytes().to_vec()];
    let mut sorted: Vec<_> = frags.to_vec();
    sorted.sort();
    for (n, b) in &sorted {
        fs::write(s.h.0.join("frags").join(n), b).unwrap();
        fragments.push(Fragment {
            name: (*n).into(),
            origin: "foreign".into(),
            sha256: Some(sha256_hex(b)),
        });
        want.push(b.to_vec());
    }
    let mut o = op(&s.h, &s.store, "x@out", 1, Action::CreateFile, "out");
    o.adapter = "text-fragments/v1".into();
    o.inputs = vec![Input {
        path: "files/body".into(),
        content_id: Some(format!("sha256:{}", sha256_hex(body.as_bytes()))),
    }];
    o.preconditions.push(Precondition {
        subject: "source:files/body".into(),
        state: format!("sha256:{}", sha256_hex(body.as_bytes())),
    });
    o.fragments = fragments;
    o.fragment_dir = Some("frags".into());
    o.output_sha256 = Some(sha256_hex(&want.join(&b'\n')));
    let mut p = plan(vec![o]);
    p.sources[0].root = src.to_str().unwrap().into();
    Asm { plan: p, src }
}

/// Whether `text` holds a run of 64 hex digits: a SHA-256 printed in full.
fn prints_a_hash(text: &str) -> bool {
    let mut run = 0;
    for c in text.chars() {
        run = if c.is_ascii_hexdigit() { run + 1 } else { 0 };
        if run >= 64 {
            return true;
        }
    }
    false
}

fn blocked_stale(s: &Setup, a: &Asm) {
    let r = go(s, &a.plan, &Joiner, &none());
    assert_eq!(r.operations[0].status, Status::Blocked, "{:?}", r.stopped);
    assert_eq!(r.stopped.as_ref().unwrap().code, Code::PlanStale);
    // The message names the path. It does not print a live hash of someone's file.
    let why = &r.stopped.as_ref().unwrap().detail;
    assert!(!prints_a_hash(why), "{why}");
    assert!(!s.h.0.join("out").exists(), "nothing was written");
    assert!(
        s.store.read_journal().unwrap().is_empty(),
        "no intent entry"
    );
}

#[test]
fn fragments_are_read_through_the_opened_directory_and_joined_in_byte_order() {
    let s = setup();
    let a = asm(
        &s,
        "order",
        "body",
        &[("m.md", b"two"), ("Z.md", b"one"), ("a.md", b"three")],
    );
    let r = go(&s, &a.plan, &Joiner, &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    // Byte order: uppercase before lowercase.
    assert_eq!(
        fs::read_to_string(s.h.0.join("out")).unwrap(),
        "body\none\nthree\ntwo"
    );
}

#[test]
fn a_missing_fragment_directory_with_no_listed_fragment_assembles_the_body_alone() {
    let s = setup();
    let a = asm(&s, "nodir", "body", &[]);
    fs::remove_dir(s.h.0.join("frags")).unwrap();
    let r = go(&s, &a.plan, &Joiner, &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    assert_eq!(fs::read_to_string(s.h.0.join("out")).unwrap(), "body");
}

#[test]
fn a_fragment_that_disappeared_changed_or_was_swapped_is_stale_and_writes_nothing() {
    let frags: &[(&str, &[u8])] = &[("a.md", b"one"), ("b.md", b"two")];
    type Swap = fn(&Setup);
    let swaps: [(&str, Swap); 6] = [
        ("gone", |s| {
            fs::remove_file(s.h.0.join("frags/a.md")).unwrap()
        }),
        ("edited", |s| {
            fs::write(s.h.0.join("frags/a.md"), "ONE").unwrap()
        }),
        ("link", |s| {
            fs::remove_file(s.h.0.join("frags/a.md")).unwrap();
            fs::write(s.h.0.join("elsewhere"), "one").unwrap();
            symlink(s.h.0.join("elsewhere"), s.h.0.join("frags/a.md")).unwrap();
        }),
        ("hardlink", |s| {
            // Same bytes, same hash, but a second name: not the file the plan hashed.
            fs::hard_link(s.h.0.join("frags/a.md"), s.h.0.join("second-name")).unwrap();
        }),
        ("fifo", |s| {
            fs::remove_file(s.h.0.join("frags/a.md")).unwrap();
            let st = std::process::Command::new("mkfifo")
                .arg(s.h.0.join("frags/a.md"))
                .status()
                .unwrap();
            assert!(st.success());
        }),
        ("oversize", |s| {
            fs::write(s.h.0.join("frags/a.md"), vec![b'x'; (16 << 20) + 1]).unwrap();
        }),
    ];
    for (what, swap) in swaps {
        let s = setup();
        let a = asm(&s, what, "body", frags);
        swap(&s);
        blocked_stale(&s, &a);
    }
}

#[test]
fn a_fragment_that_appeared_after_planning_is_stale_but_an_unmatched_name_is_not() {
    let s = setup();
    let a = asm(&s, "appear", "body", &[("a.md", b"one")]);
    fs::write(s.h.0.join("frags/new.md"), "late").unwrap();
    blocked_stale(&s, &a);
    let s = setup();
    let a = asm(&s, "other", "body", &[("a.md", b"one")]);
    fs::write(s.h.0.join("frags/notes.txt"), "late").unwrap();
    let r = go(&s, &a.plan, &Joiner, &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
}

#[test]
fn a_source_replaced_after_planning_is_stale_and_writes_nothing() {
    for what in ["edit", "link", "gone"] {
        let s = setup();
        let a = asm(&s, what, "body", &[]);
        let body = a.src.join("files/body");
        match what {
            "edit" => fs::write(&body, "other body").unwrap(),
            "link" => {
                fs::remove_file(&body).unwrap();
                fs::write(a.src.join("files/real"), "body").unwrap();
                symlink(a.src.join("files/real"), &body).unwrap();
            }
            _ => fs::remove_file(&body).unwrap(),
        }
        blocked_stale(&s, &a);
    }
}

#[test]
fn generated_bytes_that_differ_from_the_planned_hash_are_stale_and_write_nothing() {
    let s = setup();
    let mut a = asm(&s, "hash", "body", &[("a.md", b"one")]);
    a.plan.operations[0].output_sha256 = Some(sha256_hex(b"something else"));
    blocked_stale(&s, &a);
}

#[test]
fn a_plan_without_an_output_hash_for_a_generated_file_is_invalid() {
    let s = setup();
    let mut a = asm(&s, "nohash", "body", &[]);
    a.plan.operations[0].output_sha256 = None;
    let e = refused(&s, &a.plan, &Joiner, &none());
    assert_eq!(e.code, Code::PlanInvalid);
    assert!(!s.h.0.join("out").exists());
}

#[test]
fn a_generation_failure_keeps_the_last_good_output_and_blocks_dependents() {
    let s = setup();
    fs::write(s.h.0.join("out"), "good\n").unwrap();
    own(&s, "out", "good\n");
    let mut o = op(&s.h, &s.store, "x@out", 1, Action::UpdateFile, "out");
    o.output_sha256 = Some(sha256_hex(b"new\n"));
    let mut dep = op(&s.h, &s.store, "x@dep", 2, Action::CreateFile, "dep");
    dep.after = vec![o.id.clone()];
    dep.output_sha256 = Some(sha256_hex(b"dep\n"));
    let r = go(&s, &plan(vec![o, dep]), &Failing, &none());
    assert_eq!(r.operations[0].status, Status::Failed);
    assert_eq!(r.operations[1].status, Status::Blocked);
    assert_eq!(r.stopped.as_ref().unwrap().code, Code::ManifestInvalid);
    assert_eq!(fs::read_to_string(s.h.0.join("out")).unwrap(), "good\n");
    assert!(!s.h.0.join("dep").exists());
    assert!(s.store.read_journal().unwrap().is_empty());
    assert_eq!(leftovers(&s.h.0), Vec::<String>::new());
    // The ledger still names the last good content.
    let l = s.store.read_ledger().unwrap();
    assert_eq!(
        l.resources[0].owned_outputs[0].fingerprint.as_deref(),
        Some(sha256_hex(b"good\n").as_str())
    );
}

#[test]
fn fragments_together_over_the_cap_are_refused_before_they_are_all_read() {
    let s = setup();
    // Five fragments of 4 MiB: each is within the file bound, together they are 20 MiB.
    let big = vec![b'x'; 4 << 20];
    let frags: Vec<(String, &[u8])> = (0..5)
        .map(|i| (format!("{i}.md"), big.as_slice()))
        .collect();
    let refs: Vec<(&str, &[u8])> = frags.iter().map(|(n, b)| (n.as_str(), *b)).collect();
    let a = asm(&s, "cap", "body", &refs);
    let r = go(&s, &a.plan, &Joiner, &none());
    assert_eq!(r.operations[0].status, Status::Failed, "{:?}", r.stopped);
    assert_eq!(r.stopped.as_ref().unwrap().code, Code::LimitExceeded);
    assert!(!s.h.0.join("out").exists());
    assert!(s.store.read_journal().unwrap().is_empty());
}

#[test]
fn a_fragment_directory_with_too_many_entries_is_stale() {
    let s = setup();
    let a = asm(&s, "many", "body", &[("a.md", b"one")]);
    for i in 0..=dfm_model::plan::MAX_LISTING {
        fs::write(s.h.0.join("frags").join(format!("n{i}.txt")), "").unwrap();
    }
    blocked_stale(&s, &a);
}

// ---- recovery across runs (DFM-19) -----------------------------------------------------------

/// A real source file, so a link to it resolves to a regular file. Returns the source root.
fn real_src(s: &Setup) -> String {
    let root = s.h.0.with_extension("src");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("x"), "x\n").unwrap();
    root.to_str().unwrap().to_string()
}

/// A link operation to the real source file `x`.
fn link_op(s: &Setup, n: u32, rel: &str) -> Operation {
    let mut l = op(&s.h, &s.store, "x@link", n, Action::CreateLink, rel);
    l.link = Some(format!("{}/x", real_src(s)));
    l
}

/// Run 1 ends `unknown` for a link `l` (the failpoint fires after the link exists). The
/// resource already has a ledger entry, as automatic ownership requires.
fn unknown_link(s: &Setup) {
    own(s, "seed", "seed\n");
    let l = link_op(s, 1, "l");
    let rep = go_with(
        s,
        &plan_in(&real_src(s), vec![l]),
        &gens(),
        &none(),
        &Fail::at("link-after-create", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Unknown);
}

/// A later run that only creates the unrelated file `g`.
fn later_run(s: &Setup) -> dfm_engine::Report {
    let g = op(&s.h, &s.store, "x@file", 7, Action::CreateFile, "g");
    go(s, &plan(vec![g]), &gens(), &none())
}

fn last_of(s: &Setup, id_prefix: &str) -> JournalEntry {
    let j = s.store.read_journal().unwrap();
    j.into_iter()
        .rev()
        .find(|e| e.op_id.starts_with(id_prefix))
        .unwrap()
}

fn owned_paths(s: &Setup) -> Vec<(String, Option<String>)> {
    s.store
        .read_ledger()
        .unwrap()
        .resources
        .iter()
        .flat_map(|r| r.owned_outputs.iter())
        .filter(|o| !o.path.ends_with("/seed"))
        .map(|o| (o.path.clone(), o.fingerprint.clone()))
        .collect()
}

#[test]
fn an_unknown_link_that_is_now_present_settles_done_in_the_next_run_with_ownership() {
    let s = setup();
    unknown_link(&s);
    assert!(owned_paths(&s).is_empty(), "unknown records no ownership");
    let rep = later_run(&s);
    assert_eq!(rep.operations[0].status, Status::Updated);
    let e = last_of(&s, &jid(1));
    assert_eq!(e.phase, Phase::Done);
    assert_eq!(e.reason.as_deref(), Some("recovered: done"));
    let abs = s.h.0.join("l").to_str().unwrap().to_string();
    assert!(
        owned_paths(&s).contains(&(abs, Some(link_fingerprint(&format!("{}/x", real_src(&s))))))
    );
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new(&format!("{}/x", real_src(&s)))
    );
}

#[test]
fn an_unknown_link_removed_between_runs_settles_failed_and_the_path_is_free() {
    let s = setup();
    unknown_link(&s);
    fs::remove_file(s.h.0.join("l")).unwrap();
    assert_eq!(later_run(&s).operations[0].status, Status::Updated);
    let e = last_of(&s, &jid(1));
    assert_eq!(e.phase, Phase::Failed);
    assert_eq!(e.reason.as_deref(), Some("recovered: not done"));
    assert!(owned_paths(&s).iter().all(|(p, _)| !p.ends_with("/l")));
    // The path is free: a new link there is created.
    let l = link_op(&s, 2, "l");
    let rep = go(&s, &plan_in(&real_src(&s), vec![l]), &gens(), &none());
    assert_eq!(rep.operations[0].status, Status::Updated);
}

#[test]
fn an_unknown_that_the_probe_cannot_decide_blocks_its_path_in_every_run() {
    let s = setup();
    unknown_link(&s);
    // The link is replaced by a directory: the probe cannot say whether the operation is done.
    fs::remove_file(s.h.0.join("l")).unwrap();
    fs::create_dir(s.h.0.join("l")).unwrap();
    let before = fs::read_dir(s.h.0.join("l")).unwrap().count();
    let id = last_of(&s, &jid(1)).op_id;
    for n in 0..2 {
        let g = op(
            &s.h,
            &s.store,
            "x@file",
            7,
            Action::CreateFile,
            &format!("g{n}"),
        );
        let l = link_op(&s, 2, "l");
        let rep = go(&s, &plan_in(&real_src(&s), vec![g, l]), &gens(), &none());
        assert_eq!(rep.operations[1].status, Status::Blocked);
        let r = rep.stopped.unwrap();
        assert_eq!(r.code, Code::OpInterrupted);
        assert!(r.detail.contains(&id), "{}", r.detail);
        assert!(r.detail.contains("state resolve"), "{}", r.detail);
        assert_eq!(last_of(&s, &jid(1)).phase, Phase::Unknown);
        assert_eq!(fs::read_dir(s.h.0.join("l")).unwrap().count(), before);
        assert!(owned_paths(&s).iter().all(|(p, _)| !p.ends_with("/l")));
    }
    // A descendant of the blocked path is blocked too.
    let child = op(&s.h, &s.store, "x@file", 8, Action::CreateFile, "l/child");
    let rep = go(&s, &plan(vec![child]), &gens(), &none());
    assert_eq!(rep.operations[0].status, Status::Blocked);
    assert!(!s.h.0.join("l/child").exists());
}

#[test]
fn an_interrupted_intent_the_probe_cannot_decide_stays_blocked_across_runs() {
    use dfm_model::state::{Recovery, RecoveryOutput};
    let s = setup();
    let abs = s.h.0.join("l").to_str().unwrap().to_string();
    fs::create_dir(s.h.0.join("l")).unwrap();
    s.store
        .append_journal(&JournalEntry {
            schema: 1,
            op_id: format!("{}-1", jid(1)),
            op: "create-link".into(),
            phase: Phase::Intent,
            at: "2026-10-10T00:00:00Z".into(),
            resource: "core.x".into(),
            adapter: "git-config/v1".into(),
            class: Class::Normal,
            recovery: Some(Recovery {
                outputs: vec![RecoveryOutput {
                    path: abs,
                    existed_before: false,
                    expected_sha256: Some(link_fingerprint(&format!("{}/x", real_src(&s)))),
                }],
                snapshot: None,
            }),
            token: None,
            reason: None,
        })
        .unwrap();
    for _ in 0..2 {
        let l = link_op(&s, 2, "l");
        let rep = go(&s, &plan_in(&real_src(&s), vec![l]), &gens(), &none());
        assert_eq!(rep.operations[0].status, Status::Blocked);
        assert_eq!(rep.stopped.unwrap().code, Code::OpInterrupted);
    }
    assert_eq!(last_of(&s, &jid(1)).phase, Phase::Unknown);
    assert_eq!(
        dfm_model::state::unresolved(&s.store.read_journal().unwrap()).len(),
        1
    );
}

#[test]
fn an_unknown_removal_settles_done_when_the_path_is_absent_and_drops_the_ledger_entry() {
    let s = setup();
    fs::write(s.h.0.join("f"), "old\n").unwrap();
    own(&s, "f", "old\n");
    let id = "remove@core.x@f";
    let r = op(&s.h, &s.store, id, 1, Action::Remove, "f");
    let allow = BTreeSet::from([id.to_string()]);
    let rep = go_with(
        &s,
        &plan(vec![r]),
        &gens(),
        &allow,
        &Fail::at("remove-after-unlink", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Unknown);
    assert_eq!(owned_paths(&s).len(), 1, "unknown keeps the ledger entry");
    assert_eq!(later_run(&s).operations[0].status, Status::Updated);
    assert_eq!(last_of(&s, &jid(1)).phase, Phase::Done);
    assert!(owned_paths(&s).iter().all(|(p, _)| !p.ends_with("/f")));
}

#[test]
fn an_unknown_file_update_settles_done_with_the_new_fingerprint_and_mode() {
    let s = setup();
    fs::write(s.h.0.join("f"), "old\n").unwrap();
    own(&s, "f", "old\n");
    let f = op(&s.h, &s.store, "x@file", 1, Action::UpdateFile, "f");
    let rep = go_with(
        &s,
        &plan(vec![f]),
        &gens(),
        &none(),
        &Fail::at("file-after-rename", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Unknown);
    later_run(&s);
    assert_eq!(last_of(&s, &jid(1)).phase, Phase::Done);
    let abs = s.h.0.join("f").to_str().unwrap().to_string();
    assert!(owned_paths(&s).contains(&(abs, Some(sha256_hex(b"new\n")))));
}

#[test]
fn a_file_edited_after_an_unknown_publish_is_not_claimed() {
    let s = setup();
    let f = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    go_with(
        &s,
        &plan(vec![f]),
        &gens(),
        &none(),
        &Fail::at("file-after-rename", Inject::Error),
    );
    fs::write(s.h.0.join("f"), "edited\n").unwrap();
    let g = op(&s.h, &s.store, "x@file", 7, Action::CreateFile, "g");
    let rep = go(&s, &plan(vec![g]), &gens(), &none());
    // The edited file does not match the expectation: the probe cannot decide and nothing is
    // recorded as owned.
    // The effect was seen when the entry was written, and the file differs now: closed as
    // failed, so a later hand-made match is never owned.
    let last = last_of(&s, &jid(1));
    assert_eq!(last.phase, Phase::Failed);
    assert_eq!(
        last.reason.as_deref(),
        Some("recovered: not done; a later match is not owned")
    );
    assert!(owned_paths(&s).iter().all(|(p, _)| !p.ends_with("/f")));
    assert_eq!(rep.operations[0].status, Status::Updated);
}

#[test]
fn resolve_done_records_ownership_only_for_a_matching_path_and_failed_needs_nothing() {
    let s = setup();
    unknown_link(&s);
    let id = last_of(&s, &jid(1)).op_id;
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let sy = sys(&s.h);
    let r = (&sy, &home, s.h.0.as_path());
    // Mismatch: the link is retargeted. `done` is refused with E_CONFLICT, nothing is written.
    fs::remove_file(s.h.0.join("l")).unwrap();
    symlink("/elsewhere", s.h.0.join("l")).unwrap();
    let len = s.store.journal_len().unwrap();
    let e = dfm_engine::resolve(&s.store, r, &id, true).unwrap_err();
    assert_eq!(e.code, Code::Conflict);
    assert!(e.detail.contains("--as failed"), "{}", e.detail);
    assert_eq!(s.store.journal_len().unwrap(), len);
    // An id that names nothing is a usage error.
    let e = dfm_engine::resolve(&s.store, r, "nope-1", true).unwrap_err();
    assert_eq!(e.code, Code::Usage);
    // Match: restore the link.
    fs::remove_file(s.h.0.join("l")).unwrap();
    symlink(format!("{}/x", real_src(&s)), s.h.0.join("l")).unwrap();
    dfm_engine::resolve(&s.store, r, &id, true).unwrap();
    let last = last_of(&s, &jid(1));
    assert_eq!(last.phase, Phase::Done);
    assert_eq!(last.reason.as_deref(), Some("resolved by operator"));
    assert_eq!(owned_paths(&s).len(), 1);
    // It is settled now: resolving again is a usage error.
    assert_eq!(
        dfm_engine::resolve(&s.store, r, &id, false)
            .unwrap_err()
            .code,
        Code::Usage
    );

    // `failed` needs no match and records no ownership.
    let s = setup();
    unknown_link(&s);
    let id = last_of(&s, &jid(1)).op_id;
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let r = (&sy, &home, s.h.0.as_path());
    fs::remove_file(s.h.0.join("l")).unwrap();
    fs::create_dir(s.h.0.join("l")).unwrap();
    dfm_engine::resolve(&s.store, r, &id, false).unwrap();
    assert_eq!(last_of(&s, &jid(1)).phase, Phase::Failed);
    assert!(owned_paths(&s).is_empty());
}

// ---- recovery hardening (DFM-19 fix round) ---------------------------------------------------

/// Writes an intent and an `unknown` entry by hand, as a forged or foreign journal would.
fn forge(
    s: &Setup,
    (id, op, resource): (&str, &str, &str),
    (path, existed_before, expected): (&str, bool, Option<String>),
    reason: &str,
    with_intent: bool,
) {
    use dfm_model::state::{Recovery, RecoveryOutput};
    let entry = |phase, recovery| JournalEntry {
        schema: 1,
        op_id: id.into(),
        op: op.into(),
        phase,
        at: "2026-10-10T00:00:00Z".into(),
        resource: resource.into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        recovery,
        token: None,
        reason: (phase == Phase::Unknown).then(|| reason.to_string()),
    };
    if with_intent {
        let rec = Recovery {
            outputs: vec![RecoveryOutput {
                path: s.h.0.join(path).to_str().unwrap().into(),
                existed_before,
                expected_sha256: expected,
            }],
            snapshot: None,
        };
        s.store
            .append_journal(&entry(Phase::Intent, Some(rec)))
            .unwrap();
    }
    s.store
        .append_journal(&entry(Phase::Unknown, None))
        .unwrap();
}

fn real_link(s: &Setup, rel: &str) -> String {
    let t = format!("{}/x", real_src(s));
    symlink(&t, s.h.0.join(rel)).unwrap();
    link_fingerprint(&t)
}

fn recover_now(s: &Setup) -> dfm_engine::Settled {
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    dfm_engine::recover(&s.store, (&sys(&s.h), &home, s.h.0.as_path())).unwrap()
}

#[test]
fn a_case_only_spelling_of_a_blocked_path_is_still_blocked() {
    let s = setup();
    unknown_link(&s);
    fs::remove_file(s.h.0.join("l")).unwrap();
    fs::create_dir(s.h.0.join("l")).unwrap();
    for rel in ["L", "l/CHILD"] {
        let mut o = op(&s.h, &s.store, "x@file", 8, Action::CreateFile, "g9");
        o.path = Some(rel.into());
        let rep = go(&s, &plan(vec![o]), &gens(), &none());
        assert_eq!(rep.operations[0].status, Status::Blocked, "{rel}");
        assert_eq!(rep.stopped.unwrap().code, Code::OpInterrupted, "{rel}");
    }
}

#[test]
fn an_unknown_with_no_intent_blocks_its_whole_resource_until_resolved_failed() {
    let s = setup();
    forge(
        &s,
        ("orphan-1", "create-link", "core.x"),
        ("l", false, None),
        "unconfirmed: x",
        false,
    );
    let g = op(&s.h, &s.store, "x@file", 7, Action::CreateFile, "g");
    let rep = go(&s, &plan(vec![g]), &gens(), &none());
    assert_eq!(rep.operations[0].status, Status::Blocked);
    let r = rep.stopped.unwrap();
    assert_eq!(r.code, Code::OpInterrupted);
    assert!(
        r.detail.contains("orphan-1") && r.detail.contains("--as failed"),
        "{}",
        r.detail
    );
    assert!(!s.h.0.join("g").exists());
    // Settling cannot decide it, and `done` is refused.
    let st = recover_now(&s);
    assert_eq!(st.open.len(), 1);
    assert!(!st.open[0].has_paths);
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let sy = sys(&s.h);
    let w = (&sy, &home, s.h.0.as_path());
    let e = dfm_engine::resolve(&s.store, w, "orphan-1", true).unwrap_err();
    assert_eq!(e.code, Code::Conflict);
    dfm_engine::resolve(&s.store, w, "orphan-1", false).unwrap();
    let g = op(&s.h, &s.store, "x@file", 7, Action::CreateFile, "g");
    let rep = go(&s, &plan(vec![g]), &gens(), &none());
    assert_eq!(rep.operations[0].status, Status::Updated);
}

#[test]
fn a_forged_or_late_matching_unknown_never_creates_ownership() {
    // Forged: a real link and an intent plus unknown for it, on a resource the ledger and the
    // plan do not know.
    let s = setup();
    let fp = real_link(&s, "l");
    forge(
        &s,
        ("forged-1", "create-link", "other.res"),
        ("l", false, Some(fp.clone())),
        "unconfirmed: x",
        true,
    );
    let st = recover_now(&s);
    assert_eq!(st.closed, [("forged-1".to_string(), Phase::Done)]);
    assert!(owned_paths(&s).is_empty(), "unknown resource");

    // The same on a resource the ledger knows, but the entry was not seen to match when it was
    // written (no `unconfirmed:` mark): the match appeared later.
    let s = setup();
    own(&s, "seed", "seed\n");
    let fp = real_link(&s, "l");
    forge(
        &s,
        ("late-1", "create-link", "core.x"),
        ("l", false, Some(fp.clone())),
        "reconciled: the effect cannot be observed",
        true,
    );
    recover_now(&s);
    assert!(owned_paths(&s).is_empty(), "late match");

    // A create that found the path present before is not owned either.
    let s = setup();
    own(&s, "seed", "seed\n");
    let fp = real_link(&s, "l");
    forge(
        &s,
        ("pre-1", "create-link", "core.x"),
        ("l", true, Some(fp)),
        "unconfirmed: x",
        true,
    );
    recover_now(&s);
    assert!(owned_paths(&s).is_empty(), "existed before");

    // A link to something that is not a regular file is not owned.
    let s = setup();
    own(&s, "seed", "seed\n");
    symlink("/nowhere", s.h.0.join("l")).unwrap();
    forge(
        &s,
        ("dang-1", "create-link", "core.x"),
        ("l", false, Some(link_fingerprint("/nowhere"))),
        "unconfirmed: x",
        true,
    );
    let st = recover_now(&s);
    assert_eq!(st.closed.len(), 1);
    assert!(owned_paths(&s).is_empty(), "dangling link");

    // The legitimate case still records ownership.
    let s = setup();
    unknown_link(&s);
    recover_now(&s);
    assert_eq!(owned_paths(&s).len(), 1);
}

#[test]
fn a_plan_that_names_the_output_lets_settling_own_a_new_resource() {
    let s = setup();
    let root = real_src(&s);
    let l = link_op(&s, 1, "l");
    let rep = go_with(
        &s,
        &plan_in(&root, vec![l]),
        &gens(),
        &none(),
        &Fail::at("link-after-create", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Unknown);
    // The resource has no ledger entry. The plan names the output (an adoption), so the entry
    // is settled and owned first, and the adoption is current.
    let mut a = link_op(&s, 2, "l");
    a.action = Action::Adopt;
    let id = a.id.clone();
    let adopt = BTreeSet::from([id]);
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let (g, fail, nobody) = (gens(), Fail::none(), none());
    let r = Run {
        allow_adopt: &adopt,
        ..run(&s, &g, &nobody, &fail, &home)
    };
    let rep = apply(&r, &plan_in(&root, vec![a])).unwrap();
    assert_eq!(
        rep.operations[0].status,
        Status::Current,
        "{:?}",
        rep.stopped
    );
    assert_eq!(owned_paths(&s).len(), 1);
}

#[test]
fn settle_only_recovery_clears_an_unknown_replace_link_and_reports_what_stays_open() {
    // The replace-link deadlock: the owned link now has the new target, the ledger the old one.
    let s = setup();
    let root = real_src(&s);
    let (old, new) = (format!("{root}/old"), format!("{root}/x"));
    fs::write(format!("{root}/old"), "old\n").unwrap();
    symlink(&old, s.h.0.join("l")).unwrap();
    own_fp(&s, "l", &link_fingerprint(&old));
    let mut l = op(&s.h, &s.store, "x@link", 1, Action::ReplaceLink, "l");
    l.link = Some(new.clone());
    let rep = go_with(
        &s,
        &plan_in(&root, vec![l]),
        &gens(),
        &none(),
        &Fail::at("link-after-rename", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Unknown);
    let st = recover_now(&s);
    assert!(st.open.is_empty());
    assert_eq!(st.closed.len(), 1);
    let abs = s.h.0.join("l").to_str().unwrap().to_string();
    assert_eq!(owned_paths(&s), [(abs, Some(link_fingerprint(&new)))]);
    // Nothing left: a second call settles nothing.
    let st = recover_now(&s);
    assert!(st.open.is_empty() && st.closed.is_empty());

    // One that stays open is reported with its journal id.
    let s = setup();
    unknown_link(&s);
    fs::remove_file(s.h.0.join("l")).unwrap();
    fs::create_dir(s.h.0.join("l")).unwrap();
    let id = last_of(&s, &jid(1)).op_id;
    let st = recover_now(&s);
    assert_eq!(st.open.len(), 1);
    assert_eq!(st.open[0].id, id);
    assert_eq!(st.open[0].paths.len(), 1);
    assert!(st.closed.is_empty());
}

#[test]
fn an_adoption_goes_stale_when_the_ledger_changed_to_another_fingerprint_or_resource() {
    for (resource, target) in [("core.x", "/other"), ("other.res", "")] {
        let s = setup();
        let root = real_src(&s);
        let t = format!("{root}/x");
        symlink(&t, s.h.0.join("l")).unwrap();
        let mut a = link_op(&s, 1, "l");
        a.action = Action::Adopt;
        // The ledger changes after planning.
        let fp = if target.is_empty() {
            link_fingerprint(&t)
        } else {
            link_fingerprint(target)
        };
        let mut led = s.store.read_ledger().unwrap();
        led.record(LedgerResource {
            resource: resource.into(),
            adapter: "git-config/v1".into(),
            class: Class::Normal,
            op_id: "seed-1".into(),
            owned_outputs: vec![OwnedOutput {
                path: s.h.0.join("l").to_str().unwrap().into(),
                mode: 0o777,
                fingerprint: Some(fp),
                coowned: None,
            }],
        })
        .unwrap();
        s.store.write_ledger(&led).unwrap();
        let adopt = BTreeSet::from([a.id.clone()]);
        let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
        let (g, fail, nobody) = (gens(), Fail::none(), none());
        let r = Run {
            allow_adopt: &adopt,
            ..run(&s, &g, &nobody, &fail, &home)
        };
        let rep = apply(&r, &plan_in(&root, vec![a])).unwrap();
        assert_eq!(
            rep.operations[0].status,
            Status::Blocked,
            "{resource} {target}"
        );
        assert_eq!(rep.stopped.unwrap().code, Code::PlanStale);
    }
}

#[test]
fn an_intent_with_no_recorded_path_blocks_its_whole_resource() {
    use dfm_model::state::Recovery;
    let s = setup();
    let entry = |phase, recovery| JournalEntry {
        schema: 1,
        op_id: "empty-1".into(),
        op: "create-link".into(),
        phase,
        at: "2026-10-10T00:00:00Z".into(),
        resource: "core.x".into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        recovery,
        token: None,
        reason: (phase == Phase::Unknown).then(|| "unconfirmed: x".to_string()),
    };
    s.store
        .append_journal(&entry(
            Phase::Intent,
            Some(Recovery {
                outputs: vec![],
                snapshot: None,
            }),
        ))
        .unwrap();
    s.store
        .append_journal(&entry(Phase::Unknown, None))
        .unwrap();
    let g = op(&s.h, &s.store, "x@file", 7, Action::CreateFile, "g");
    let rep = go(&s, &plan(vec![g]), &gens(), &none());
    assert_eq!(rep.operations[0].status, Status::Blocked);
    let r = rep.stopped.unwrap();
    assert_eq!(r.code, Code::OpInterrupted);
    assert!(
        r.detail.contains("empty-1") && r.detail.contains("--as failed"),
        "{}",
        r.detail
    );
    assert!(!recover_now(&s).open[0].has_paths);
}

// ---- a link source is read again before the link is published (PR #8, DFM-55b) --------------

/// A link `l` to a file (`files/x`) or a directory (`files/d`) of a fresh source root, with the
/// `source:` precondition the planner records for it.
fn link_plan(s: &Setup, tag: &str, dir: bool) -> (Plan, std::path::PathBuf) {
    let root = s.h.0.join(format!(".lsrc-{tag}"));
    fs::create_dir_all(root.join("files/d/sub")).unwrap();
    fs::write(root.join("files/x"), "data\n").unwrap();
    fs::write(root.join("files/d/a"), "A").unwrap();
    let (path, id) = if dir {
        let entry = |path: &str, kind| TreeEntry {
            path: path.into(),
            kind,
        };
        let id = tree_digest(vec![
            entry(
                "a",
                EntryKind::File {
                    exec: false,
                    sha256: sha256_hex(b"A"),
                },
            ),
            entry("sub", EntryKind::Dir),
        ]);
        ("files/d", id)
    } else {
        ("files/x", sha256_hex(b"data\n"))
    };
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::CreateLink, "l");
    o.link = Some(format!("{}/{path}", root.display()));
    o.inputs = vec![Input {
        path: path.into(),
        content_id: Some(format!("sha256:{id}")),
    }];
    o.preconditions.push(Precondition {
        subject: format!("source:{path}"),
        state: format!("sha256:{id}"),
    });
    let p = plan_in(root.to_str().unwrap(), vec![o]);
    (p, root)
}

fn nothing_published(s: &Setup) {
    assert!(
        fs::symlink_metadata(s.h.0.join("l")).is_err(),
        "no link was written"
    );
    assert!(
        s.store.read_journal().unwrap().is_empty(),
        "no intent entry"
    );
    assert!(
        s.store.read_ledger().unwrap().resources.is_empty(),
        "no ownership was recorded"
    );
}

#[test]
fn an_unchanged_link_source_is_published_and_owned() {
    for dir in [false, true] {
        let s = setup();
        let (p, _) = link_plan(&s, "ok", dir);
        let r = go(&s, &p, &NoGenerator, &none());
        assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
        assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);
    }
}

#[test]
fn a_link_source_that_changed_after_the_plan_is_stale_and_writes_nothing() {
    type Mutate = fn(&Path);
    let cases: [(&str, bool, Mutate); 8] = [
        ("file-edit", false, |r| {
            fs::write(r.join("files/x"), "other\n").unwrap()
        }),
        ("file-gone", false, |r| {
            fs::remove_file(r.join("files/x")).unwrap()
        }),
        ("file-is-dir", false, |r| {
            fs::remove_file(r.join("files/x")).unwrap();
            fs::create_dir(r.join("files/x")).unwrap();
        }),
        ("file-ancestor-link", false, |r| {
            let out = r.with_extension("out");
            fs::rename(r.join("files"), &out).unwrap();
            symlink(&out, r.join("files")).unwrap();
        }),
        ("dir-entry-added", true, |r| {
            fs::write(r.join("files/d/new"), "N").unwrap()
        }),
        ("dir-empty-subdir-added", true, |r| {
            fs::create_dir(r.join("files/d/sub2")).unwrap()
        }),
        ("dir-gone", true, |r| {
            fs::remove_dir_all(r.join("files/d")).unwrap()
        }),
        ("dir-ancestor-link", true, |r| {
            let out = r.with_extension("out");
            fs::rename(r.join("files"), &out).unwrap();
            symlink(&out, r.join("files")).unwrap();
        }),
    ];
    for (tag, dir, mutate) in cases {
        let s = setup();
        let (p, root) = link_plan(&s, tag, dir);
        mutate(&root);
        let r = go(&s, &p, &NoGenerator, &none());
        assert_eq!(
            r.operations[0].status,
            Status::Blocked,
            "{tag}: {:?}",
            r.stopped
        );
        assert_eq!(r.stopped.as_ref().unwrap().code, Code::PlanStale, "{tag}");
        assert!(!prints_a_hash(&r.stopped.as_ref().unwrap().detail), "{tag}");
        nothing_published(&s);
    }
}

#[test]
fn a_link_source_with_no_recorded_identity_is_still_checked_for_presence() {
    let s = setup();
    let (mut p, root) = link_plan(&s, "unhashed", false);
    p.operations[0].inputs[0].content_id = None;
    p.operations[0]
        .preconditions
        .retain(|c| !c.subject.starts_with("source:"));
    fs::write(root.join("files/x"), "any content\n").unwrap();
    let r = go(&s, &p, &NoGenerator, &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    let s = setup();
    let (mut p, root) = link_plan(&s, "unhashed-gone", false);
    p.operations[0].inputs[0].content_id = None;
    fs::remove_file(root.join("files/x")).unwrap();
    let r = go(&s, &p, &NoGenerator, &none());
    assert_eq!(r.operations[0].status, Status::Blocked);
    nothing_published(&s);
}

// ---- an unchanged merge is read back too (PR #8, DFM-55b) -----------------------------------

/// A merge that leaves the live file as it is: the bytes equal the live ones and `changed` is
/// false, as for a takeover or a metadata-only change.
struct Unchanged;

impl Generator for Unchanged {
    fn supports(&self, _: &Operation) -> bool {
        false
    }
    fn generate(&self, _: &Operation, _: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        Err(Refusal::new(Code::GeneratorUnavailable, "none"))
    }
    fn merges(&self, _: &Operation) -> bool {
        true
    }
    fn merge(&self, _: &Operation, i: &GeneratorInputs) -> Result<Merged, Refusal> {
        Ok(Merged {
            bytes: i.live.clone().unwrap_or_default(),
            live_view: b"{}".to_vec(),
            rendered_view: b"{}".to_vec(),
            live_leaves: BTreeMap::new(),
            rendered_leaves: BTreeMap::new(),
            entries: vec![],
            appended: vec![],
            changed: false,
        })
    }
}

/// An `update_file` merge of `settings.json`, planned from the live machine.
fn merge_plan(s: &Setup) -> Plan {
    let live = s.h.0.join("settings.json");
    fs::write(&live, "{\"a\":1}\n").unwrap();
    fs::set_permissions(&live, fs::Permissions::from_mode(0o600)).unwrap();
    let mut o = op(
        &s.h,
        &s.store,
        "x@s",
        1,
        Action::UpdateFile,
        "settings.json",
    );
    o.class = Class::Sensitive;
    o.managed_sha256 = Some("a".repeat(64));
    let sys = sys(&s.h);
    let home = Dir::open_home(&sys, &s.h.0).unwrap();
    let obs = observe(
        &sys,
        &home,
        &[Request {
            path: "settings.json".into(),
            hash: false,
            list: false,
        }],
    )
    .unwrap();
    for c in &mut o.preconditions {
        if c.subject == "path:settings.json" {
            c.state = node_state(&obs.seen["settings.json"], true);
        } else if c.subject == "ledger:settings.json" {
            c.state = ledger_state(None, true);
        }
    }
    plan(vec![o])
}

#[test]
fn an_unchanged_merge_that_is_left_alone_is_recorded() {
    let s = setup();
    let r = go(&s, &merge_plan(&s), &Unchanged, &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);
}

#[test]
fn an_unchanged_merge_rewritten_or_chmoded_before_the_read_back_is_not_recorded() {
    type Mutate = fn(&Path);
    let cases: [(&str, Mutate); 2] = [
        ("rewrite", |f| fs::write(f, "{\"a\":2}\n").unwrap()),
        ("chmod", |f| {
            fs::set_permissions(f, fs::Permissions::from_mode(0o640)).unwrap()
        }),
    ];
    for (tag, mutate) in cases {
        let s = setup();
        let p = merge_plan(&s);
        let live = s.h.0.join("settings.json");
        let r = parked(
            &s,
            &p,
            &Unchanged,
            &none(),
            "apply-after-effect",
            move || mutate(&live),
        );
        assert_eq!(
            r.operations[0].status,
            Status::Failed,
            "{tag}: {:?}",
            r.stopped
        );
        let last = s.store.read_journal().unwrap().pop().unwrap();
        assert_eq!(last.phase, Phase::Failed, "{tag}");
        assert!(
            s.store.read_ledger().unwrap().resources.is_empty(),
            "{tag}: no ownership from stale bytes"
        );
    }
}

// ---- case-folded ledger paths (PR #8, DFM-55b) ----------------------------------------------

#[test]
fn an_output_planned_with_another_casing_replaces_its_ledger_entry_in_place() {
    let s = setup();
    fs::write(s.h.0.join("Foo"), "old\n").unwrap();
    // The volume must fold case for `foo` to name the file `Foo`.
    if !s.h.0.join("foo").exists() {
        return;
    }
    own(&s, "Foo", "old\n");
    let mut o = op(&s.h, &s.store, "x@foo", 1, Action::UpdateFile, "foo");
    let owned = owned_outputs(&s.store.read_ledger().unwrap(), &s.h.0).unwrap();
    for c in &mut o.preconditions {
        if c.subject == "ledger:foo" {
            c.state = ledger_state(owned.first(), false);
        }
    }
    let r = go(&s, &plan(vec![o]), &gen_for("x@foo", "new\n"), &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    let l = s.store.read_ledger().unwrap();
    let paths: Vec<&str> = l
        .resources
        .iter()
        .flat_map(|r| r.owned_outputs.iter().map(|o| o.path.as_str()))
        .collect();
    assert_eq!(paths, [s.h.0.join("foo").to_str().unwrap()]);
}
