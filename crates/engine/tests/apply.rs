//! Apply at the engine level with hand-built plans: generated-file publication, file removal,
//! rechecks and races (DFM-18). The CLI tests cover links through the real binary. No adapter
//! exists yet, so a test generator supplies file content here.

mod common;

use common::*;
use dfm_engine::{
    Action as Inject, Dir, Fail, Generator, NoGenerator, Run, Store, apply, observe, owned_outputs,
};
use dfm_model::digest::sha256_hex;
use dfm_model::plan::{
    Action, Binary, Observations, Operation, Ownership, Plan, Precondition, Request, Source,
    Status as PlanStatus, ledger_state, link_fingerprint, node_state,
};
use dfm_model::state::{Class, JournalEntry, LedgerResource, OwnedOutput, Phase, Status};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

struct Gen(BTreeMap<String, Vec<u8>>);

impl Generator for Gen {
    fn supports(&self, op: &Operation) -> bool {
        self.0.contains_key(&op.id)
    }
    fn generate(&self, op: &Operation) -> Result<Vec<u8>, Refusal> {
        self.0
            .get(&op.id)
            .cloned()
            .ok_or_else(|| Refusal::new(Code::GeneratorUnavailable, "none"))
    }
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
        after: vec![],
        preconditions: pre,
        postcondition: String::new(),
    }
}

fn plan(ops: Vec<Operation>) -> Plan {
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
            root: "/src".into(),
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
    apply(&r, p).unwrap()
}

fn refused(s: &Setup, p: &Plan, g: &dyn Generator, allow: &BTreeSet<String>) -> Refusal {
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let fail = Fail::none();
    let r = run(s, g, allow, &fail, &home);
    apply(&r, p).unwrap_err()
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
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::CreateLink, "l");
    o.link = Some("/second/files/x".into());
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
