//! Apply at the engine level with hand-built plans: generated-file publication, file removal,
//! rechecks and races (DFM-18). The CLI tests cover links through the real binary. No adapter
//! exists yet, so a test generator supplies file content here.

mod common;

use common::*;
use dfm_engine::{
    Action as Inject, Dir, Fail, Generator, NoGenerator, Run, Store, apply, observe, owned_outputs,
    preflight,
};
use dfm_model::digest::sha256_hex;
use dfm_model::plan::{
    Action, Binary, Observations, Operation, Ownership, Plan, Precondition, Request, Source,
    Status as PlanStatus, ledger_state, node_state,
};
use dfm_model::state::{Class, LedgerResource, OwnedOutput, Phase, Status};
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
    let fail = Fail::none();
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let r = run(s, g, allow, &fail, &home);
    preflight(&r, p).unwrap();
    apply(&r, p).unwrap()
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

fn own(s: &Setup, rel: &str, content: &str) {
    let mut l = s.store.read_ledger().unwrap();
    l.record(LedgerResource {
        resource: "core.x".into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        op_id: "seed-1".into(),
        owned_outputs: vec![OwnedOutput {
            path: s.h.0.join(rel).to_str().unwrap().into(),
            mode: 0o600,
            fingerprint: Some(sha256_hex(content.as_bytes())),
        }],
    })
    .unwrap();
    s.store.write_ledger(&l).unwrap();
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
    let gate =
        s.h.0
            .parent()
            .unwrap()
            .join(format!("gate-{}-a", std::process::id()));
    let _ = fs::remove_file(&gate);
    let fail = Fail::at("file-after-tmp", Inject::Wait(gate.clone()));
    let target = s.h.0.join("a");
    let reached = gate.with_extension("reached");
    let t = std::thread::spawn({
        let (gate, reached) = (gate.clone(), reached.clone());
        move || {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !reached.exists() && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            fs::write(&target, "foreign\n").unwrap();
            fs::write(&gate, "").unwrap();
        }
    });
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let g = gen_for("x@a", "mine\n");
    let allow = none();
    let r = run(&s, &g, &allow, &fail, &home);
    preflight(&r, &plan(vec![o.clone()])).unwrap();
    let rep = apply(&r, &plan(vec![o])).unwrap();
    t.join().unwrap();
    let _ = fs::remove_file(&reached);
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
fn a_failure_before_the_commit_leaves_no_file_and_no_staged_name() {
    for point in ["file-before-write", "file-after-tmp"] {
        let s = setup();
        let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
        let fail = Fail::at(point, Inject::Error);
        let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
        let g = gen_for("x@a", "x");
        let allow = none();
        let r = run(&s, &g, &allow, &fail, &home);
        let p = plan(vec![o]);
        preflight(&r, &p).unwrap();
        let rep = apply(&r, &p).unwrap();
        assert_eq!(rep.operations[0].status, Status::Failed, "{point}");
        assert!(!s.h.0.join("a").exists(), "{point}");
        assert!(leftovers(&s.h.0).is_empty(), "{point}");
        assert!(
            s.store.read_ledger().unwrap().resources.is_empty(),
            "{point}"
        );
    }
}

#[test]
fn an_owned_file_that_still_matches_is_removed_and_its_ledger_entry_goes() {
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    fs::write(s.h.0.join("sibling"), "not mine\n").unwrap();
    own(&s, "a", "mine\n");
    let mut o = op(&s.h, &s.store, "remove@core.x@a", 1, Action::Remove, "a");
    o.id = "remove@core.x@a".into();
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
    let gate =
        s.h.0
            .parent()
            .unwrap()
            .join(format!("gate-{}-rm", std::process::id()));
    let _ = fs::remove_file(&gate);
    let fail = Fail::at("apply-before-effect", Inject::Wait(gate.clone()));
    let reached = gate.with_extension("reached");
    let home_path = s.h.0.clone();
    let t = std::thread::spawn({
        let (gate, reached) = (gate.clone(), reached.clone());
        move || {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !reached.exists() && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            fs::remove_file(home_path.join("a")).unwrap();
            symlink(home_path.join("victim"), home_path.join("a")).unwrap();
            fs::write(&gate, "").unwrap();
        }
    });
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let allow = BTreeSet::from(["remove@core.x@a".to_string()]);
    let r = run(&s, &NoGenerator, &allow, &fail, &home);
    let p = plan(vec![o]);
    preflight(&r, &p).unwrap();
    let rep = apply(&r, &p).unwrap();
    t.join().unwrap();
    let _ = fs::remove_file(&reached);
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
fn a_directory_is_never_removed_and_nothing_is_deleted_recursively() {
    let s = setup();
    fs::create_dir_all(s.h.0.join("d/inner")).unwrap();
    fs::write(s.h.0.join("d/inner/x"), "x").unwrap();
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let e = home.remove_entry("d").unwrap_err();
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
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let (allow, fail) = (none(), Fail::none());
    let r = run(&s, &NoGenerator, &allow, &fail, &home);
    assert_eq!(
        preflight(&r, &p).unwrap_err().code,
        Code::GeneratorUnavailable
    );
    // A sensitive output is refused even when a generator offers content.
    let mut sens = p.clone();
    sens.operations[0].class = Class::Sensitive;
    let g = gen_for("x@a", "secret");
    let r = run(&s, &g, &allow, &fail, &home);
    assert_eq!(
        preflight(&r, &sens).unwrap_err().code,
        Code::GeneratorUnavailable
    );
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
    let fail = Fail::at("file-before-write", Inject::Error);
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let g = Gen(BTreeMap::from([
        ("x@d/a".to_string(), b"1".to_vec()),
        ("x@d/b".to_string(), b"2".to_vec()),
        ("x@c".to_string(), b"3".to_vec()),
    ]));
    let allow = none();
    let r = run(&s, &g, &allow, &fail, &home);
    let p = plan(vec![a, b, c]);
    preflight(&r, &p).unwrap();
    let rep = apply(&r, &p).unwrap();
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
