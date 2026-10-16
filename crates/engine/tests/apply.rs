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
    Action, Binary, Blocker, Fragment, Input, Observations, Operation, Ownership, Plan,
    Precondition, Request, Source, Status as PlanStatus, file_fingerprint, ledger_state,
    link_fingerprint, node_state,
};
use dfm_model::state::{Class, JournalEntry, LedgerResource, OwnedOutput, Phase, Status};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

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
        schema: dfm_model::state::SCHEMA,
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
    own_fp(s, rel, &file_fingerprint(content.as_bytes()));
}

/// Waits (at most 20 s) until the parked run reports that it reached its failpoint. Otherwise it
/// releases the gate, so the run does not wait for its own 30 s timeout, and fails the test.
fn wait_reached(reached: &Path, gate: &Path, point: &str) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !reached.exists() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if !reached.exists() {
        let _ = fs::write(gate, "");
        panic!("the failpoint {point} was not reached");
    }
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
    // The thread id keeps the gate of one test apart from that of another test in this process
    // that parks at the same point.
    let thread: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let gate =
        s.h.0
            .parent()
            .unwrap()
            .join(format!("gate-{}-{thread}-{point}", std::process::id()));
    let _ = fs::remove_file(&gate);
    let reached = gate.with_extension("reached");
    let _ = fs::remove_file(&reached);
    let fail = Fail::at(point, Inject::Wait(gate.clone()));
    let t = std::thread::spawn({
        let (gate, reached, point) = (gate.clone(), reached.clone(), point.to_string());
        move || {
            wait_reached(&reached, &gate, &point);
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
    // The staged name was renamed, not linked: the published file has one name.
    assert_eq!(fs::metadata(&p).unwrap().nlink(), 1);
    assert!(leftovers(&s.h.0.join(".config")).is_empty());
    let l = s.store.read_ledger().unwrap();
    assert_eq!(
        l.resources[0].owned_outputs[0].fingerprint.as_deref(),
        Some(file_fingerprint(b"a=1\n").as_str())
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
        Some(file_fingerprint(b"new\n").as_str())
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
fn a_link_planted_in_excluded_content_after_the_containment_walk_is_published_unseen() {
    // The walk of an excluded directory source ends before the link is published, and the
    // publish never reads the source again. The same user can change the tree in between. This
    // pins that accepted window (docs/apply.md): the walk is not a lock on the tree.
    let s = setup();
    let root = real_src(&s);
    let dir = Path::new(&root).join("dir");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("inner"), "i\n").unwrap();
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::CreateLink, "l");
    o.link = Some(format!("{root}/dir"));
    o.inputs = vec![Input {
        path: "dir".into(),
        content_id: None,
    }];
    o.excluded = Some(1);
    let planted = dir.join("esc");
    let rep = parked(
        &s,
        &plan_in(&root, vec![o]),
        &NoGenerator,
        &none(),
        "apply-after-prereq",
        {
            let planted = planted.clone();
            move || symlink("../../../etc/passwd", planted).unwrap()
        },
    );
    assert_eq!(
        rep.operations[0].status,
        Status::Updated,
        "{:?}",
        rep.stopped
    );
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new(&root).join("dir")
    );
    assert!(
        fs::symlink_metadata(&planted).is_ok(),
        "the entry planted after the walk was not seen"
    );
    // The next walk sees it: the same source, with the entry already there, is refused before
    // any link is published.
    let s = setup();
    let root = real_src(&s);
    let dir = Path::new(&root).join("dir");
    fs::create_dir_all(&dir).unwrap();
    symlink("../../../etc/passwd", dir.join("esc")).unwrap();
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::CreateLink, "l");
    o.link = Some(format!("{root}/dir"));
    o.inputs = vec![Input {
        path: "dir".into(),
        content_id: None,
    }];
    o.excluded = Some(1);
    let rep = go(&s, &plan_in(&root, vec![o]), &NoGenerator, &none());
    assert_eq!(
        rep.stopped.as_ref().unwrap().code,
        Code::SourceEscape,
        "{:?}",
        rep.operations[0].status
    );
    assert!(fs::symlink_metadata(s.h.0.join("l")).is_err());
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
        Some(file_fingerprint(b"old\n").as_str()),
        "the ledger still describes what the manager wrote"
    );
}

#[test]
fn a_file_swapped_for_an_identical_one_after_the_content_check_is_not_replaced() {
    // The content check passes for the swapped-in file too. Only the identity of the inode that
    // was hashed, compared at the owner check, sees that the name now refers to another file.
    let s = setup();
    fs::write(s.h.0.join("a"), "old\n").unwrap();
    own(&s, "a", "old\n");
    let o = op(&s.h, &s.store, "x@a", 1, Action::UpdateFile, "a");
    let home_path = s.h.0.clone();
    let rep = parked(
        &s,
        &plan(vec![o]),
        &gen_for("x@a", "new\n"),
        &none(),
        "file-after-hash",
        move || {
            fs::write(home_path.join("a.swap"), "old\n").unwrap();
            fs::rename(home_path.join("a.swap"), home_path.join("a")).unwrap();
        },
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    assert_eq!(rep.stopped.as_ref().unwrap().code, Code::Conflict);
    assert!(
        rep.stopped
            .as_ref()
            .unwrap()
            .detail
            .contains("replaced while applying")
    );
    assert_eq!(fs::read_to_string(s.h.0.join("a")).unwrap(), "old\n");
    assert!(leftovers(&s.h.0).is_empty());
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
            sys(&s.h).uid,
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
        sys(&s.h).uid,
        &Fail::none(),
    )
    .unwrap();
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new("/src/new")
    );
}

#[test]
fn replacing_a_link_checks_its_owner_where_it_is_replaced() {
    let s = setup();
    symlink("/src/old", s.h.0.join("l")).unwrap();
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    // The ledger fingerprint matches, so only the owner differs.
    let e = home
        .replace_link(
            "l",
            "/src/new",
            &link_fingerprint("/src/old"),
            sys(&s.h).uid + 1,
            &Fail::none(),
        )
        .unwrap_err();
    assert_eq!(e.code, Code::Conflict);
    assert!(e.detail.contains("owned by another user"), "{}", e.detail);
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new("/src/old")
    );
    assert!(leftovers(&s.h.0).is_empty());
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
    for action in [
        Action::CreateLink,
        Action::ReplaceLink,
        Action::Remove,
        Action::CreateDir,
    ] {
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

/// Writes a journal of `done` lines, then `tail`, whose size leaves room for one operation
/// (4096 bytes) but not for the entries that settling `tail` adds. Returns the bytes written.
fn near_cap_journal(s: &Setup, tail: &[u8]) -> Vec<u8> {
    let e = JournalEntry {
        phase: Phase::Done,
        recovery: None,
        ..intent("create-link", &s.h.0.join("z").to_string_lossy())
    };
    let line = e.to_line().unwrap();
    let room = dfm_model::state::MAX_JOURNAL_BYTES as usize - 4096 - 1024 - tail.len();
    let mut bytes = line.repeat(room / line.len());
    bytes.extend_from_slice(tail);
    let path =
        s.h.0
            .join(".local/state/dotfiles-manager/state/journal.jsonl");
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    bytes
}

#[test]
fn the_journal_gate_reserves_room_for_settling_earlier_work() {
    let open = |phase| {
        let mut e = intent("create-other", "/z/other");
        e.phase = phase;
        if phase == Phase::Unknown {
            e.recovery = None;
            e.reason = Some("unconfirmed: x".into());
        }
        e.to_line().unwrap()
    };
    // An `unknown` entry settling can close has its intent record before it.
    let paired = [open(Phase::Intent), open(Phase::Unknown)].concat();
    let torn = b"{\"schema\":2,\"op_id\":\"tor".to_vec();
    let cases: [(&str, Vec<u8>); 3] = [
        ("an open intent", open(Phase::Intent)),
        ("an open unknown entry with its intent", paired),
        ("a torn final line", torn),
    ];
    for (what, tail) in cases {
        let s = setup();
        let before = near_cap_journal(&s, &tail);
        let path =
            s.h.0
                .join(".local/state/dotfiles-manager/state/journal.jsonl");
        let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
        let err = refused(&s, &plan(vec![o]), &gen_for("x@a", "x"), &none());
        assert_eq!(err.code, Code::LimitExceeded, "{what}: {err:?}");
        assert!(
            err.detail.contains("earlier work"),
            "{what}: {}",
            err.detail
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "{what}: nothing was appended"
        );
        assert!(!s.h.0.join("a").exists(), "{what}");
    }
    // The same size with nothing to settle still applies.
    let s = setup();
    near_cap_journal(&s, b"");
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let rep = go(&s, &plan(vec![o]), &gen_for("x@a", "x"), &none());
    assert_eq!(rep.operations[0].status, Status::Updated);
    assert!(s.h.0.join("a").exists());
    // An `unknown` entry with no intent record is never closed, so it reserves nothing.
    let s = setup();
    let mut stray = intent("create-other", "/z/other");
    stray.phase = Phase::Unknown;
    stray.resource = "other.y".into();
    stray.recovery = None;
    stray.reason = Some("unconfirmed: x".into());
    near_cap_journal(&s, &stray.to_line().unwrap());
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let rep = go(&s, &plan(vec![o]), &gen_for("x@a", "x"), &none());
    assert_eq!(rep.operations[0].status, Status::Updated);
    assert!(s.h.0.join("a").exists());
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
    let store = open_write_after_drop(&h, Fail::at("journal-before-write", Inject::Error));
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
    let store = open_write_after_drop(&h, fail);
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
            wait_reached(&reached, &gate, "journal-before-write");
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

// ---- leaf ownership (DFM-58) ------------------------------------------------------------

/// The planner blocks a leaf of another user, but the prerequisites compare no owner. A leaf that
/// is swapped in after the plan must still be refused. A real foreign owner needs root, so the
/// run's uid is shifted: `peek` then reports `own: false` while mode, content and target match.
#[test]
fn a_leaf_owned_by_another_user_is_blocked_before_any_journal_entry() {
    type Prep = fn(&Setup) -> Operation;
    let cases: [(&str, Prep); 4] = [
        ("replace_link", |s| {
            symlink("/src/old", s.h.0.join("l")).unwrap();
            own_fp(s, "l", &link_fingerprint("/src/old"));
            let mut o = op(&s.h, &s.store, "x@l", 1, Action::ReplaceLink, "l");
            o.link = Some("/src/new".into());
            o
        }),
        ("update_file", |s| {
            fs::write(s.h.0.join("l"), "old\n").unwrap();
            own(s, "l", "old\n");
            op(&s.h, &s.store, "x@l", 1, Action::UpdateFile, "l")
        }),
        ("remove", |s| {
            fs::write(s.h.0.join("l"), "old\n").unwrap();
            own(s, "l", "old\n");
            op(&s.h, &s.store, "x@l", 1, Action::Remove, "l")
        }),
        ("adopt", adopt_op),
    ];
    for (what, prep) in cases {
        for shifted in [true, false] {
            let s = setup();
            let o = prep(&s);
            let p = fill(&plan(vec![o]), &gen_for("x@l", "new\n"));
            let ledger = s.store.read_ledger().unwrap();
            let journal = s.store.read_journal().unwrap().len();
            let before = fs::symlink_metadata(s.h.0.join("l")).unwrap().ino();
            let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
            let g = gen_for("x@l", "new\n");
            let id = p.operations[0].id.clone();
            let (allow, adopt) = match what {
                "remove" => (ids(&[&id]), none()),
                "adopt" => (none(), ids(&[&id])),
                _ => (none(), none()),
            };
            let nofail = Fail::none();
            let mut r = run(&s, &g, &allow, &nofail, &home);
            r.allow_adopt = &adopt;
            if shifted {
                r.sys.uid += 1;
            }
            let rep = apply(&r, &p).unwrap();
            let st = rep.operations[0].status;
            if !shifted {
                // The control: with the real owner the same plan applies.
                assert_eq!(st, Status::Updated, "{what}: {:?}", rep.stopped);
                continue;
            }
            assert_eq!(st, Status::Blocked, "{what}: {:?}", rep.stopped);
            let stopped = rep
                .stopped
                .as_ref()
                .unwrap_or_else(|| panic!("{what}: no stop"));
            assert_eq!(
                stopped.code,
                Code::PrivilegeUnsupported,
                "{what}: {stopped:?}"
            );
            assert_eq!(
                fs::symlink_metadata(s.h.0.join("l")).unwrap().ino(),
                before,
                "{what}: the leaf is untouched"
            );
            assert_eq!(
                s.store.read_journal().unwrap().len(),
                journal,
                "{what}: no journal entry"
            );
            assert_eq!(
                s.store.read_ledger().unwrap(),
                ledger,
                "{what}: the ledger is unchanged"
            );
        }
    }
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
                wait_reached(&reached, &gate, point);
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
            sha256: Some(file_fingerprint(b)),
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
        Some(file_fingerprint(b"good\n").as_str())
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
            schema: dfm_model::state::SCHEMA,
            op_id: format!("{}-1", jid(1)),
            op: "create-link".into(),
            phase: Phase::Intent,
            at: "2026-10-10T00:00:00.000000000Z".into(),
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
    assert!(owned_paths(&s).contains(&(abs, Some(file_fingerprint(b"new\n")))));
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
        schema: dfm_model::state::SCHEMA,
        op_id: id.into(),
        op: op.into(),
        phase,
        at: "2026-10-10T00:00:00.000000000Z".into(),
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
        schema: dfm_model::state::SCHEMA,
        op_id: "empty-1".into(),
        op: "create-link".into(),
        phase,
        at: "2026-10-10T00:00:00.000000000Z".into(),
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
fn a_source_root_that_cannot_be_opened_is_a_failure_not_a_stale_source() {
    let s = setup();
    let (p, root) = link_plan(&s, "root-gone", false);
    fs::remove_dir_all(&root).unwrap();
    let r = go(&s, &p, &NoGenerator, &none());
    assert_eq!(r.operations[0].status, Status::Failed, "{:?}", r.stopped);
    nothing_published(&s);
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
            rendered_view: b"{}".to_vec(),
            live_leaves: BTreeMap::new(),
            rendered_leaves: BTreeMap::new(),
            entries: vec![],
            appended: vec![],
            changed: false,
        })
    }
}

/// A merge of `settings.json`, planned from the live machine. `existing` writes the live file
/// first (mode 0600), so the plan finds it.
fn merge_plan_of(s: &Setup, action: Action, existing: bool) -> Plan {
    let live = s.h.0.join("settings.json");
    if existing {
        fs::write(&live, "{\"a\":1}\n").unwrap();
        fs::set_permissions(&live, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut o = op(&s.h, &s.store, "x@s", 1, action, "settings.json");
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

/// An `update_file` merge of `settings.json`, planned from the live machine.
fn merge_plan(s: &Setup) -> Plan {
    merge_plan_of(s, Action::UpdateFile, true)
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
        eprintln!("SKIPPED: case-sensitive volume");
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

// ---- review fixes (DFM-57): refusals leave HOME and the store alone ---------------------------

/// Every entry below HOME with its bytes (a link with its target), the state store included.
fn tree(s: &Setup) -> BTreeMap<PathBuf, Vec<u8>> {
    files_below(&s.h.0)
        .into_iter()
        .map(|p| {
            let m = fs::symlink_metadata(&p).unwrap();
            let bytes = if m.file_type().is_symlink() {
                fs::read_link(&p)
                    .unwrap()
                    .into_os_string()
                    .into_encoded_bytes()
            } else if m.is_file() {
                fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            (p, bytes)
        })
        .collect()
}

/// The whole plan is refused with `want` before any write: HOME, the journal and the ledger are
/// byte for byte what they were.
fn assert_refused_untouched(s: &Setup, p: &Plan, g: &dyn Generator, want: Code) -> Refusal {
    let before = tree(s);
    let e = refused(s, p, g, &none());
    assert_eq!(e.code, want, "{e}");
    assert_eq!(tree(s), before, "HOME or the state store changed");
    assert!(s.store.read_journal().unwrap().is_empty());
    e
}

fn phases(s: &Setup) -> Vec<Phase> {
    s.store
        .read_journal()
        .unwrap()
        .iter()
        .map(|e| e.phase)
        .collect()
}

#[test]
fn a_path_that_is_not_a_normalized_home_relative_path_is_refused_before_any_write() {
    for bad in ["", ".", "a/../b", "a/\u{7}b", "/abs", "a//b"] {
        let s = setup();
        let mut o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
        o.path = Some(bad.into());
        let g = gen_for("x@a", "x");
        assert_refused_untouched(&s, &plan(vec![o]), &g, Code::PlanInvalid);
    }
}

#[test]
fn a_plan_with_blockers_is_refused_with_the_first_blockers_code() {
    let s = setup();
    let o = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let mut p = plan(vec![o]);
    p.blockers = vec![Blocker {
        code: Code::TargetCollision,
        resource: "core.x".into(),
        source: "/p".into(),
        field: None,
        path: Some("a".into()),
        detail: "collides".into(),
        correction: "rename it".into(),
    }];
    let e = assert_refused_untouched(&s, &p, &gen_for("x@a", "x"), Code::TargetCollision);
    assert!(e.detail.contains("collides"), "{e}");
}

#[test]
fn a_duplicate_operation_id_is_refused_before_any_write() {
    let s = setup();
    let a = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let b = op(&s.h, &s.store, "x@a", 2, Action::CreateFile, "b");
    let e = assert_refused_untouched(
        &s,
        &plan(vec![a, b]),
        &gen_for("x@a", "x"),
        Code::PlanInvalid,
    );
    assert!(e.detail.contains("appears twice"), "{e}");
}

#[test]
fn an_after_edge_to_a_later_operation_is_refused_before_any_write() {
    let s = setup();
    let mut a = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    let b = op(&s.h, &s.store, "x@b", 2, Action::CreateFile, "b");
    a.after = vec![b.id.clone()];
    let g = Gen(BTreeMap::from([
        ("x@a".to_string(), b"a".to_vec()),
        ("x@b".to_string(), b"b".to_vec()),
    ]));
    let e = assert_refused_untouched(&s, &plan(vec![a, b]), &g, Code::PlanInvalid);
    assert!(e.detail.contains("does not come earlier"), "{e}");
}

#[test]
fn an_invalid_journal_id_is_refused_before_any_write() {
    let s = setup();
    let mut a = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    a.journal_id = "not a journal id".into();
    let e = assert_refused_untouched(&s, &plan(vec![a]), &gen_for("x@a", "x"), Code::PlanInvalid);
    assert!(e.detail.contains("journal id"), "{e}");
}

#[test]
fn a_blocked_operation_with_no_listed_blocker_is_refused_before_any_write() {
    let s = setup();
    let mut a = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
    a.status = PlanStatus::Blocked;
    let e = assert_refused_untouched(&s, &plan(vec![a]), &gen_for("x@a", "x"), Code::PlanInvalid);
    assert!(e.detail.contains("lists no blocker"), "{e}");
}

#[test]
fn a_retire_that_carries_a_managed_fingerprint_is_refused_before_any_write() {
    let s = setup();
    let mut a = op(&s.h, &s.store, "x@a", 1, Action::Retire, "a");
    a.class = Class::Sensitive;
    a.managed_sha256 = Some("a".repeat(64));
    let e = assert_refused_untouched(&s, &plan(vec![a]), &MergeOnly, Code::PlanInvalid);
    assert!(e.detail.contains("carries a managed fingerprint"), "{e}");
}

#[test]
fn a_retire_of_an_output_that_is_not_a_merge_is_refused_before_any_write() {
    let s = setup();
    let a = op(&s.h, &s.store, "x@a", 1, Action::Retire, "a");
    let e = assert_refused_untouched(&s, &plan(vec![a]), &NoGenerator, Code::PlanInvalid);
    assert!(e.detail.contains("not a co-owned merge"), "{e}");
}

#[test]
fn a_planned_operation_with_no_action_is_refused_before_any_write() {
    let s = setup();
    let a = op(&s.h, &s.store, "x@a", 1, Action::None, "a");
    let e = assert_refused_untouched(&s, &plan(vec![a]), &NoGenerator, Code::PlanInvalid);
    assert!(e.detail.contains("no action"), "{e}");
}

#[test]
fn a_sensitive_directory_is_refused_by_the_gates_not_by_the_executor() {
    let s = setup();
    let mut d = op(&s.h, &s.store, "x@d", 1, Action::CreateDir, "d");
    d.class = Class::Sensitive;
    for g in [&NoGenerator as &dyn Generator, &MergeOnly] {
        assert_refused_untouched(&s, &plan(vec![d.clone()]), g, Code::SensitiveUnsupported);
    }
    assert!(!s.h.0.join("d").exists());
}

#[test]
fn a_leaf_with_no_room_for_the_staged_name_is_refused_before_any_write() {
    // A staged name adds up to 39 bytes, and a name holds 255. 216 fits and is really published.
    let s = setup();
    let fits = "a".repeat(216);
    let o = op(&s.h, &s.store, "x@fits", 1, Action::CreateFile, &fits);
    let r = go(&s, &plan(vec![o]), &gen_for("x@fits", "x"), &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    assert!(s.h.0.join(&fits).is_file());
    assert!(leftovers(&s.h.0).is_empty());
    // 217 does not, for every action that stages. The refusal names the limit.
    let long = "b".repeat(217);
    let s = setup();
    let o = op(&s.h, &s.store, "x@long", 1, Action::CreateFile, &long);
    let e = assert_refused_untouched(
        &s,
        &plan(vec![o]),
        &gen_for("x@long", "x"),
        Code::LimitExceeded,
    );
    assert!(e.detail.contains("217 bytes"), "{e}");
    let mut l = op(&s.h, &s.store, "x@link", 2, Action::ReplaceLink, &long);
    l.link = Some("/src/x".into());
    let g = gates(&plan(vec![l]), &NoGenerator);
    assert_eq!(g.refusal.unwrap().code, Code::LimitExceeded);
    // A link that is created is not staged, so 217 bytes still fit. A name over the limit of a
    // name is refused for every action, also one that stages nothing, before any write.
    let mut c = op(&s.h, &s.store, "x@create", 3, Action::CreateLink, &long);
    c.link = Some("/src/x".into());
    assert!(gates(&plan(vec![c]), &NoGenerator).refusal.is_none());
    let over = "c".repeat(256);
    for action in [
        Action::CreateLink,
        Action::CreateDir,
        Action::Remove,
        Action::Adopt,
    ] {
        let s = setup();
        let link =
            matches!(action, Action::CreateLink | Action::Adopt).then(|| "/src/x".to_string());
        let mut o = op(&s.h, &s.store, "x@over", 4, action, "short");
        o.path = Some(over.clone());
        o.link = link.clone();
        let e = assert_refused_untouched(&s, &plan(vec![o]), &NoGenerator, Code::LimitExceeded);
        assert!(e.detail.contains("256 bytes"), "{action:?}: {e}");
        // The limit itself is a name the file system takes.
        let mut ok = op(&s.h, &s.store, "x@max", 5, action, "short");
        ok.path = Some("d".repeat(255));
        ok.link = link;
        let g = gates(&plan(vec![ok]), &NoGenerator);
        assert_ne!(
            g.refusal.map(|r| r.code),
            Some(Code::LimitExceeded),
            "{action:?}"
        );
    }
}

#[test]
fn an_immutable_file_is_a_permission_failure_not_a_directory() {
    let s = setup();
    let a = s.h.0.join("a");
    fs::write(&a, "mine\n").unwrap();
    own(&s, "a", "mine\n");
    let id = "remove@core.x@a";
    let o = op(&s.h, &s.store, id, 1, Action::Remove, "a");
    let chflags = |f: &str| {
        std::process::Command::new("chflags")
            .args([f, a.to_str().unwrap()])
            .status()
            .is_ok_and(|s| s.success())
    };
    // Clears the flag on every exit, also a panic in `go`, so the HOME can be deleted.
    struct Clear<F: Fn(&str) -> bool>(F);
    impl<F: Fn(&str) -> bool> Drop for Clear<F> {
        fn drop(&mut self) {
            (self.0)("nouchg");
        }
    }
    let clear = Clear(chflags);
    // chflags ships with macOS, the only supported platform, so a failure here is a failure of
    // the test and not a reason to pass without running it.
    assert!(
        (clear.0)("uchg"),
        "chflags uchg failed: the check cannot run"
    );
    let allow = BTreeSet::from([id.to_string()]);
    let rep = go(&s, &plan(vec![o]), &NoGenerator, &allow);
    assert_eq!(
        rep.operations[0].status,
        Status::Failed,
        "{:?}",
        rep.stopped
    );
    let e = rep.stopped.unwrap();
    assert_eq!(e.code, Code::IoFailed);
    assert!(e.detail.contains("immutable flag"), "{e}");
    assert!(!e.detail.contains("is a directory"), "{e}");
    assert_eq!(fs::read_to_string(&a).unwrap(), "mine\n");
    assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);
}

// ---- a stop request before the intent entry (A2) --------------------------------------------

/// Asks for a stop (SIGTERM or SIGINT) when the executor calls it, as a signal that arrives
/// during generation would. The first call is the plan fill in `go_with`, which asks for none.
struct Stopping {
    calls: AtomicU32,
    term: bool,
}

impl Generator for Stopping {
    fn supports(&self, op: &Operation) -> bool {
        op.action == Action::CreateFile
    }
    fn generate(&self, _: &Operation, _: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= 1 {
            dfm_engine::signal::simulate(self.term);
        }
        Ok(b"x".to_vec())
    }
}

#[test]
fn a_signal_during_generation_ends_the_operation_not_started_with_no_intent_entry() {
    for (term, code, exit) in [
        (true, Code::Terminated, 143),
        (false, Code::Interrupted, 130),
    ] {
        // The simulated stop belongs to its thread, so each case gets its own.
        std::thread::spawn(move || {
            let s = setup();
            let a = op(&s.h, &s.store, "x@a", 1, Action::CreateFile, "a");
            let b = op(&s.h, &s.store, "x@b", 2, Action::CreateDir, "b");
            let g = Stopping {
                calls: AtomicU32::new(0),
                term,
            };
            let before = tree(&s);
            let rep = go(&s, &plan(vec![a, b]), &g, &none());
            let st: Vec<_> = rep.operations.iter().map(|o| o.status).collect();
            assert_eq!(st, [Status::NotStarted, Status::NotStarted], "{term}");
            let stop = rep.stopped.unwrap();
            assert_eq!((stop.code, stop.exit().code()), (code, exit));
            assert_eq!(tree(&s), before, "nothing was written, not even an intent");
            assert!(s.store.read_journal().unwrap().is_empty());
        })
        .join()
        .unwrap();
    }
}

/// A merge that asks for a stop while it is decided.
struct StoppingMerge;

impl Generator for StoppingMerge {
    fn supports(&self, _: &Operation) -> bool {
        false
    }
    fn generate(&self, _: &Operation, _: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        Err(Refusal::new(Code::GeneratorUnavailable, "none"))
    }
    fn merges(&self, _: &Operation) -> bool {
        true
    }
    fn merge(&self, op: &Operation, i: &GeneratorInputs) -> Result<Merged, Refusal> {
        dfm_engine::signal::simulate(true);
        Unchanged.merge(op, i)
    }
}

#[test]
fn a_signal_during_a_merge_decision_ends_it_not_started_with_no_intent_entry() {
    let s = setup();
    let p = merge_plan(&s);
    let before = tree(&s);
    let rep = go(&s, &p, &StoppingMerge, &none());
    assert_eq!(rep.operations[0].status, Status::NotStarted);
    assert_eq!(rep.stopped.unwrap().exit().code(), 143);
    assert_eq!(tree(&s), before);
    assert!(s.store.read_journal().unwrap().is_empty());
}

#[test]
fn a_signal_during_a_merge_decision_does_not_create_the_missing_salt() {
    // A store from an older build has no salt. A merge creates it only after its last stop
    // check, so an operation that ends as not started leaves the state store as it was.
    let s = setup();
    let salt = s.h.0.join(".local/state/dotfiles-manager/state/salt");
    fs::remove_file(&salt).unwrap();
    let p = merge_plan(&s);
    let before = tree(&s);
    let rep = go(&s, &p, &StoppingMerge, &none());
    assert_eq!(rep.operations[0].status, Status::NotStarted);
    assert_eq!(rep.stopped.unwrap().exit().code(), 143);
    assert!(!salt.exists(), "the salt was created before the stop check");
    assert_eq!(tree(&s), before);
    assert!(s.store.read_journal().unwrap().is_empty());
}

#[test]
fn a_signal_before_a_stale_entry_is_pruned_keeps_the_entry_and_writes_no_intent() {
    // The simulated stop belongs to its thread, so the run gets its own.
    std::thread::spawn(|| {
        let s = setup();
        own(&s, "gone", "x");
        let p = absent_candidate(&s);
        let before = tree(&s);
        let rep = go_with(
            &s,
            &p,
            &NoGenerator,
            &none(),
            &Fail::at("prune-before-intent", Inject::Stop),
        );
        assert_eq!(rep.operations[0].status, Status::Current);
        assert_eq!(rep.stopped.unwrap().exit().code(), 143);
        assert_eq!(tree(&s), before, "no intent, no ledger change");
        assert!(s.store.read_journal().unwrap().is_empty());
        assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);
    })
    .join()
    .unwrap();
}

// ---- a publish whose read-back differs (T1) ------------------------------------------------

#[test]
fn a_publish_whose_read_back_differs_fails_and_is_never_recorded_as_owned() {
    type Mutate = fn(&Path);
    let cases: [(&str, Action, Mutate); 6] = [
        ("content", Action::CreateFile, |p| {
            fs::write(p, "tampered\n").unwrap()
        }),
        ("mode", Action::CreateFile, |p| {
            fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap()
        }),
        ("file to link", Action::CreateFile, |p| {
            fs::remove_file(p).unwrap();
            symlink("/elsewhere", p).unwrap();
        }),
        ("link retargeted", Action::CreateLink, |p| {
            fs::remove_file(p).unwrap();
            symlink("/src/other", p).unwrap();
        }),
        ("link to file", Action::CreateLink, |p| {
            fs::remove_file(p).unwrap();
            fs::write(p, "x").unwrap();
        }),
        ("dir to file", Action::CreateDir, |p| {
            fs::remove_dir(p).unwrap();
            fs::write(p, "x").unwrap();
        }),
    ];
    for (tag, action, mutate) in cases {
        let s = setup();
        let mut o = op(&s.h, &s.store, "x@file", 1, action, "t");
        o.link = (action == Action::CreateLink).then(|| "/src/x".to_string());
        let target = s.h.0.join("t");
        let rep = parked(
            &s,
            &plan(vec![o]),
            &gens(),
            &none(),
            "apply-after-effect",
            move || mutate(&target),
        );
        assert_eq!(rep.operations[0].status, Status::Failed, "{tag}");
        let e = rep.stopped.unwrap();
        assert_eq!(e.code, Code::IoFailed, "{tag}: {e}");
        assert!(e.detail.contains("postcondition failed"), "{tag}: {e}");
        assert!(
            s.store.read_ledger().unwrap().resources.is_empty(),
            "{tag}: nothing is owned with the planned fingerprint"
        );
        assert_eq!(phases(&s), [Phase::Intent, Phase::Failed], "{tag}: no done");
    }
}

// ---- the final delete authorization (T2) ---------------------------------------------------

#[test]
fn a_ledger_rewritten_before_the_unlink_keeps_the_file_and_fails_the_removal() {
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    own(&s, "a", "mine\n");
    let id = "remove@core.x@a";
    let o = op(&s.h, &s.store, id, 1, Action::Remove, "a");
    let ledger =
        s.h.0
            .join(".local/state/dotfiles-manager/state/ledger.json");
    let mut other = s.store.read_ledger().unwrap();
    other
        .record(LedgerResource {
            resource: "core.x".into(),
            adapter: "git-config/v1".into(),
            class: Class::Normal,
            op_id: "seed-2".into(),
            owned_outputs: vec![OwnedOutput {
                path: s.h.0.join("a").to_str().unwrap().into(),
                mode: 0o600,
                fingerprint: Some("b".repeat(64)),
                coowned: None,
            }],
        })
        .unwrap();
    let bytes = dfm_model::state::canonical(&other).unwrap();
    let allow = BTreeSet::from([id.to_string()]);
    // Every earlier check passed. The ledger changes while the removal is about to run.
    let rep = parked(
        &s,
        &plan(vec![o]),
        &NoGenerator,
        &allow,
        "apply-before-effect",
        move || fs::write(&ledger, bytes).unwrap(),
    );
    assert_eq!(
        rep.operations[0].status,
        Status::Failed,
        "{:?}",
        rep.stopped
    );
    let e = rep.stopped.unwrap();
    assert_eq!(e.code, Code::Conflict);
    assert!(
        e.detail.contains("no longer matches the fingerprint"),
        "{e}"
    );
    assert_eq!(fs::read_to_string(s.h.0.join("a")).unwrap(), "mine\n");
    assert_eq!(phases(&s), [Phase::Intent, Phase::Failed]);
    assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);
}

// ---- an outcome that cannot be settled after a good effect (T3, T10, T14) ------------------

/// A plan for a new file or a new link, and what to check for the effect.
fn new_output(s: &Setup, link: bool) -> Plan {
    if link {
        let mut l = op(&s.h, &s.store, "x@file", 1, Action::CreateLink, "t");
        l.link = Some("/src/x".into());
        plan(vec![l])
    } else {
        plan(vec![op(
            &s.h,
            &s.store,
            "x@file",
            1,
            Action::CreateFile,
            "t",
        )])
    }
}

/// The same store without its failpoint, as a later run opens it.
fn reopened(s: Setup) -> Setup {
    let Setup { h, store } = s;
    drop(store);
    let store = open(&h);
    Setup { h, store }
}

#[test]
fn an_outcome_that_cannot_be_settled_after_a_good_effect_blocks_the_path_in_the_next_run() {
    // (tag, store failpoint, run failpoint, status, ledger entry written)
    type Case = (
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
        Status,
        bool,
    );
    let cases: [Case; 3] = [
        (
            "done hook",
            None,
            Some("apply-after-done"),
            Status::Failed,
            false,
        ),
        (
            "ledger write",
            Some("ledger-before-write"),
            None,
            Status::Failed,
            false,
        ),
        (
            "ledger rename",
            Some("ledger-after-rename"),
            None,
            Status::Unknown,
            true,
        ),
    ];
    for link in [false, true] {
        for (tag, store_fp, run_fp, status, owned) in cases {
            let s = match store_fp {
                Some(fp) => rearmed(|_| {}, Fail::at(fp, Inject::Error)),
                None => setup(),
            };
            let p = new_output(&s, link);
            let fail = run_fp.map_or_else(Fail::none, |fp| Fail::at(fp, Inject::Error));
            let rep = go_with(&s, &p, &gens(), &none(), &fail);
            let what = format!("{tag}, link={link}");
            assert_eq!(rep.operations[0].status, status, "{what}");
            let e = rep.stopped.clone().unwrap();
            assert_eq!(e.exit(), dfm_model::Exit::Failed, "{what}: {e}");
            // The effect is in place and the journal holds the intent and the done entry.
            assert!(fs::symlink_metadata(s.h.0.join("t")).is_ok(), "{what}");
            assert_eq!(phases(&s), [Phase::Intent, Phase::Done], "{what}");
            let ledger = s.store.read_ledger().unwrap();
            assert_eq!(!ledger.resources.is_empty(), owned, "{what}");
            // The next run does not take the path over: the plan is stale for it.
            let s = reopened(s);
            let before = tree(&s);
            let again = go(&s, &p, &gens(), &none());
            assert_eq!(again.operations[0].status, Status::Blocked, "{what}");
            assert_eq!(again.stopped.unwrap().code, Code::PlanStale, "{what}");
            assert_eq!(tree(&s), before, "{what}");
            // A plan made from the machine as it is now still does not take the path over:
            // nothing records an owner for it, so the create is a conflict.
            let fresh = new_output(&s, link);
            let again = go(&s, &fresh, &gens(), &none());
            assert_eq!(again.operations[0].status, Status::Blocked, "{what}");
            assert_eq!(again.stopped.unwrap().code, Code::Conflict, "{what}");
            assert_eq!(tree(&s), before, "{what}");
            assert_eq!(s.store.read_ledger().unwrap().resources.len() == 1, owned);
        }
    }
}

#[test]
fn an_update_or_replace_whose_ledger_write_is_lost_gets_the_new_fingerprint_from_recovery() {
    // The new content is live and the ledger still holds the old fingerprint. Without a repair
    // the planner reports the path as modified and blocks it for good.
    for link in [false, true] {
        for point in ["ledger-before-write", "ledger-after-rename"] {
            let s = rearmed(
                |s| {
                    if link {
                        symlink("/src/old", s.h.0.join("t")).unwrap();
                        own_fp(s, "t", &link_fingerprint("/src/old"));
                    } else {
                        fs::write(s.h.0.join("t"), "old\n").unwrap();
                        own(s, "t", "old\n");
                    }
                },
                Fail::at(point, Inject::Error),
            );
            let what = format!("{point}, link={link}");
            // A link must resolve to a regular file to be owned.
            let target = format!("{}/x", real_src(&s));
            let p = if link {
                let mut l = op(&s.h, &s.store, "x@file", 1, Action::ReplaceLink, "t");
                l.link = Some(target.clone());
                plan_in(&real_src(&s), vec![l])
            } else {
                plan(vec![op(
                    &s.h,
                    &s.store,
                    "x@file",
                    1,
                    Action::UpdateFile,
                    "t",
                )])
            };
            let rep = go(&s, &p, &gens(), &none());
            assert!(rep.stopped.is_some(), "{what}");
            let new_fp = if link {
                link_fingerprint(&target)
            } else {
                file_fingerprint(b"new\n")
            };
            let s = reopened(s);
            recover_now(&s);
            let ledger = s.store.read_ledger().unwrap();
            assert_eq!(
                ledger.resources[0].owned_outputs[0].fingerprint.as_deref(),
                Some(new_fp.as_str()),
                "{what}"
            );
            assert!(unfinished_none(&s), "{what}");
        }
    }
}

fn unfinished_none(s: &Setup) -> bool {
    dfm_model::state::unfinished(&s.store.read_journal().unwrap()).is_empty()
}

#[test]
fn a_merge_whose_outcome_cannot_be_settled_is_reported_and_the_next_run_records_it() {
    // (tag, store failpoint, run failpoint, status, ledger entry written)
    type Case = (
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
        Status,
        bool,
    );
    let cases: [Case; 3] = [
        (
            "done hook",
            None,
            Some("apply-after-done"),
            Status::Failed,
            false,
        ),
        (
            "ledger write",
            Some("ledger-before-write"),
            None,
            Status::Failed,
            false,
        ),
        (
            "ledger rename",
            Some("ledger-after-rename"),
            None,
            Status::Unknown,
            true,
        ),
    ];
    for (tag, store_fp, run_fp, status, owned) in cases {
        let s = match store_fp {
            Some(fp) => rearmed(|_| {}, Fail::at(fp, Inject::Error)),
            None => setup(),
        };
        let p = merge_plan(&s);
        let fail = run_fp.map_or_else(Fail::none, |fp| Fail::at(fp, Inject::Error));
        let rep = go_with(&s, &p, &Unchanged, &none(), &fail);
        assert_eq!(rep.operations[0].status, status, "{tag}: {:?}", rep.stopped);
        let e = rep.stopped.clone().unwrap();
        assert_eq!(e.exit(), dfm_model::Exit::Failed, "{tag}: {e}");
        // The file is in place, and the journal holds the intent and the done entry.
        assert!(s.h.0.join("settings.json").exists(), "{tag}");
        assert_eq!(phases(&s), [Phase::Intent, Phase::Done], "{tag}");
        assert_eq!(
            !s.store.read_ledger().unwrap().resources.is_empty(),
            owned,
            "{tag}"
        );
        // A merge is decided from the bytes read under the lock, so a run that has no record of
        // the file merges again and records it, and does not rewrite the file. A plan that
        // expects no record, made before the ledger got one, is stale.
        let s = reopened(s);
        let live = fs::read(s.h.0.join("settings.json")).unwrap();
        let again = go(&s, &merge_plan(&s), &Unchanged, &none());
        let want = if owned {
            Status::Blocked
        } else {
            Status::Updated
        };
        assert_eq!(again.operations[0].status, want, "{tag}");
        assert_eq!(
            fs::read(s.h.0.join("settings.json")).unwrap(),
            live,
            "{tag}"
        );
        assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1, "{tag}");
    }
}

#[test]
fn a_journal_line_that_is_kept_but_unconfirmed_makes_the_operation_unknown() {
    // The second journal write is the outcome entry: the line stays, its sync is unconfirmed.
    let s = rearmed(
        |_| {},
        Fail::at_nth("journal-after-write", Inject::Error, 2),
    );
    let o = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    let rep = go(&s, &with_dependent(&s, o), &gens(), &none());
    let st: Vec<_> = rep.operations.iter().map(|o| o.status).collect();
    assert_eq!(st, [Status::Unknown, Status::Blocked]);
    assert_eq!(rep.stopped.unwrap().code, Code::IoUnconfirmed);
    assert_eq!(phases(&s), [Phase::Intent, Phase::Done], "the line is kept");
    assert!(s.store.mutated());
    assert_eq!(fs::read_to_string(s.h.0.join("f")).unwrap(), "new\n");
    assert!(s.store.read_ledger().unwrap().resources.is_empty());
    assert!(!s.h.0.join("dep").exists());
    // The first write is the intent. Its line is kept and no effect ran.
    let s = rearmed(
        |_| {},
        Fail::at_nth("journal-after-write", Inject::Error, 1),
    );
    let o = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    let rep = go(&s, &plan(vec![o]), &gens(), &none());
    assert_eq!(rep.operations[0].status, Status::Failed);
    assert_eq!(rep.stopped.unwrap().code, Code::IoUnconfirmed);
    assert_eq!(phases(&s), [Phase::Intent]);
    assert!(!s.h.0.join("f").exists());
}

#[test]
fn a_staged_link_is_removed_when_the_rename_after_it_fails() {
    let s = setup();
    symlink("/src/old", s.h.0.join("l")).unwrap();
    own_fp(&s, "l", &link_fingerprint("/src/old"));
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::ReplaceLink, "l");
    o.link = Some("/src/new".into());
    let rep = go_with(
        &s,
        &plan(vec![o]),
        &gens(),
        &none(),
        &Fail::at("link-after-tmp", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    assert!(leftovers(&s.h.0).is_empty(), "{:?}", leftovers(&s.h.0));
    assert!(
        fs::read_dir(&s.h.0).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".stage")),
        "no staged link remains"
    );
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new("/src/old")
    );
    assert_eq!(phases(&s), [Phase::Intent, Phase::Failed]);
}

#[test]
fn an_outcome_that_cannot_be_journaled_is_reported_and_the_run_stops() {
    // An unconfirmed effect whose `unknown` entry cannot be written: the operation is unknown.
    let s = rearmed(
        |_| {},
        Fail::at_nth("journal-before-write", Inject::Error, 2),
    );
    let o = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    let rep = go_with(
        &s,
        &with_dependent(&s, o),
        &gens(),
        &none(),
        &Fail::at("file-after-rename", Inject::Error),
    );
    let st: Vec<_> = rep.operations.iter().map(|o| o.status).collect();
    assert_eq!(st, [Status::Unknown, Status::Blocked]);
    let why = rep.operations[0].error.clone().unwrap();
    assert!(why.contains("the outcome could not be journaled"), "{why}");
    assert_eq!(rep.stopped.unwrap().code, Code::IoUnconfirmed);
    assert_eq!(phases(&s), [Phase::Intent], "only the intent is on disk");
    assert!(!s.h.0.join("dep").exists());
    // The next run settles the intent from the live file but records no ownership (the resource
    // owns nothing yet), so the file is not taken over and the create is blocked.
    let s = reopened(s);
    let o = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    let before = tree(&s);
    let again = go(&s, &plan(vec![o]), &gens(), &none());
    assert_eq!(again.operations[0].status, Status::Blocked);
    assert_eq!(again.stopped.unwrap().code, Code::Conflict);
    assert_eq!(fs::read_to_string(s.h.0.join("f")).unwrap(), "new\n");
    assert!(s.store.read_ledger().unwrap().resources.is_empty());
    assert!(
        tree(&s)
            .into_iter()
            .all(|(p, b)| p.ends_with("journal.jsonl") || before.get(&p) == Some(&b)),
        "only the journal may grow"
    );
    // A failed effect whose `failed` entry cannot be written: the operation is failed.
    let s = rearmed(
        |_| {},
        Fail::at_nth("journal-before-write", Inject::Error, 2),
    );
    let o = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    let rep = go_with(
        &s,
        &plan(vec![o]),
        &gens(),
        &none(),
        &Fail::at("apply-before-effect", Inject::Error),
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    let why = rep.operations[0].error.clone().unwrap();
    assert!(why.contains("the outcome could not be journaled"), "{why}");
    assert_eq!(phases(&s), [Phase::Intent]);
    assert!(!s.h.0.join("f").exists());
}

// ---- merge read-state refusals (T13) -------------------------------------------------------

#[test]
fn a_merge_target_that_changed_since_the_plan_is_refused_before_the_intent_entry() {
    type Mutate = fn(&Path);
    // (tag, action, the live file exists in the plan, change at the read, status, code, text)
    type Case = (
        &'static str,
        Action,
        bool,
        Mutate,
        Status,
        Code,
        &'static str,
    );
    let cases: [Case; 6] = [
        (
            "gone",
            Action::UpdateFile,
            true,
            |p| fs::remove_file(p).unwrap(),
            Status::Blocked,
            Code::Conflict,
            "is gone since the plan",
        ),
        (
            "appeared",
            Action::CreateFile,
            false,
            |p| {
                fs::write(p, "{}\n").unwrap();
                fs::set_permissions(p, fs::Permissions::from_mode(0o600)).unwrap();
            },
            Status::Blocked,
            Code::Conflict,
            "appeared before it could be created",
        ),
        (
            "open mode",
            Action::UpdateFile,
            true,
            |p| fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap(),
            Status::Blocked,
            Code::Conflict,
            "open to group or others",
        ),
        (
            "link",
            Action::UpdateFile,
            true,
            |p| {
                let real = p.with_file_name("real.json");
                fs::rename(p, &real).unwrap();
                symlink(&real, p).unwrap();
            },
            Status::Blocked,
            Code::Conflict,
            "is a link",
        ),
        (
            "hard link",
            Action::UpdateFile,
            true,
            |p| fs::hard_link(p, p.with_file_name("second.json")).unwrap(),
            Status::Blocked,
            Code::Conflict,
            "more than one hard link",
        ),
        (
            "over 16 MiB",
            Action::UpdateFile,
            true,
            |p| {
                let len = usize::try_from(dfm_model::plan::MAX_HASHED_BYTES).unwrap() + 1;
                fs::write(p, vec![b'x'; len]).unwrap()
            },
            Status::Failed,
            Code::LimitExceeded,
            "exceeds 16 MiB",
        ),
    ];
    for (tag, action, existing, mutate, status, code, text) in cases {
        let s = setup();
        let p = merge_plan_of(&s, action, existing);
        let live = s.h.0.join("settings.json");
        let rep = parked(
            &s,
            &p,
            &Unchanged,
            &none(),
            "merge-before-read",
            move || mutate(&live),
        );
        assert_eq!(rep.operations[0].status, status, "{tag}");
        let e = rep.stopped.unwrap();
        assert_eq!(e.code, code, "{tag}: {e}");
        assert!(e.detail.contains(text), "{tag}: {e}");
        assert!(
            s.store.read_journal().unwrap().is_empty(),
            "{tag}: no intent"
        );
        assert!(s.store.read_ledger().unwrap().resources.is_empty(), "{tag}");
        assert!(leftovers(&s.h.0).is_empty(), "{tag}: nothing was staged");
    }
}

// ---- races the primitives refuse (T16) -----------------------------------------------------

#[test]
fn a_leaf_that_changes_just_before_a_primitive_acts_is_refused_and_left_alone() {
    // A directory that appears before `mkdirat`.
    let s = setup();
    let o = op(&s.h, &s.store, "x@d", 1, Action::CreateDir, "d");
    let d = s.h.0.join("d");
    let rep = parked(
        &s,
        &plan(vec![o]),
        &gens(),
        &none(),
        "apply-before-effect",
        move || fs::create_dir(&d).unwrap(),
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    let e = rep.stopped.unwrap();
    assert_eq!(e.code, Code::Conflict);
    assert!(e.detail.contains("appeared while applying"), "{e}");
    assert!(s.h.0.join("d").is_dir());
    assert!(s.store.read_ledger().unwrap().resources.is_empty());

    // A link replaced by a regular file before `replace_link` reads it.
    let s = setup();
    symlink("/src/old", s.h.0.join("l")).unwrap();
    own_fp(&s, "l", &link_fingerprint("/src/old"));
    let mut o = op(&s.h, &s.store, "x@l", 1, Action::ReplaceLink, "l");
    o.link = Some("/src/new".into());
    let l = s.h.0.join("l");
    let rep = parked(
        &s,
        &plan(vec![o]),
        &gens(),
        &none(),
        "apply-before-effect",
        move || {
            fs::remove_file(&l).unwrap();
            fs::write(&l, "mine\n").unwrap();
        },
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    let e = rep.stopped.unwrap();
    assert_eq!(e.code, Code::Conflict);
    assert!(e.detail.contains("is no longer a link"), "{e}");
    assert_eq!(fs::read_to_string(s.h.0.join("l")).unwrap(), "mine\n");
    assert!(leftovers(&s.h.0).is_empty());

    // An owned file that vanishes between the identity check and `unlinkat`.
    let s = setup();
    fs::write(s.h.0.join("a"), "mine\n").unwrap();
    own(&s, "a", "mine\n");
    let id = "remove@core.x@a";
    let o = op(&s.h, &s.store, id, 1, Action::Remove, "a");
    let a = s.h.0.join("a");
    let allow = BTreeSet::from([id.to_string()]);
    let rep = parked(
        &s,
        &plan(vec![o]),
        &NoGenerator,
        &allow,
        "remove-before-unlink",
        move || fs::remove_file(&a).unwrap(),
    );
    assert_eq!(rep.operations[0].status, Status::Failed);
    let e = rep.stopped.unwrap();
    assert_eq!(e.code, Code::Conflict);
    assert!(e.detail.contains("vanished while applying"), "{e}");
    assert_eq!(phases(&s), [Phase::Intent, Phase::Failed]);
    assert_eq!(s.store.read_ledger().unwrap().resources.len(), 1);

    // A mode above 0777 is never published.
    let s = setup();
    let mut o = op(&s.h, &s.store, "x@file", 1, Action::CreateFile, "f");
    o.mode = Some(0o4755);
    let rep = go(&s, &plan(vec![o]), &gens(), &none());
    assert_eq!(rep.operations[0].status, Status::Failed);
    let e = rep.stopped.unwrap();
    assert_eq!(e.code, Code::Conflict);
    assert!(e.detail.contains("above 0777"), "{e}");
    assert!(!s.h.0.join("f").exists());
    assert!(leftovers(&s.h.0).is_empty());
}

// ---- a merge that crashed with only its intent (A1t) ---------------------------------------

#[test]
fn a_merge_that_crashed_with_only_its_intent_blocks_its_path_until_resolved_failed() {
    const CHILD: &str = "DFM_APPLY_CRASH_HOME";
    if let Some(dir) = std::env::var_os(CHILD) {
        // The child: writes the intent, then ends at once with no terminal entry, as a crash does.
        let h = Home(dir.into());
        let store = open(&h);
        let s = Setup { h, store };
        let p = merge_plan(&s);
        go_with(
            &s,
            &p,
            &Unchanged,
            &none(),
            &Fail::at("apply-before-effect", Inject::Abort),
        );
        std::process::exit(9);
    }
    let Setup { h, store } = setup();
    drop(store);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_merge_that_crashed_with_only_its_intent_blocks_its_path_until_resolved_failed",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &h.0)
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(6), "the child must abort: {status:?}");
    let s = Setup { store: open(&h), h };
    assert_eq!(phases(&s), [Phase::Intent], "only the intent was written");
    let live = fs::read(s.h.0.join("settings.json")).unwrap();
    // Run 1 settles the intent: a sensitive file has no expected hash, so it stays unknown and
    // blocks the path. Run 2 meets the same block. Neither changes the file.
    for run in 1..=2 {
        let rep = go(&s, &merge_plan(&s), &Unchanged, &none());
        assert_eq!(rep.operations[0].status, Status::Blocked, "run {run}");
        assert_eq!(rep.stopped.unwrap().code, Code::OpInterrupted, "run {run}");
        assert_eq!(
            fs::read(s.h.0.join("settings.json")).unwrap(),
            live,
            "run {run}"
        );
        assert!(
            s.store.read_ledger().unwrap().resources.is_empty(),
            "run {run}"
        );
    }
    // Only `state resolve --as failed` clears it.
    let id = s.store.read_journal().unwrap()[0].op_id.clone();
    let sy = sys(&s.h);
    let home = Dir::open_home(&sy, &s.h.0).unwrap();
    dfm_engine::resolve(&s.store, (&sy, &home, s.h.0.as_path()), &id, false).unwrap();
    let rep = go(&s, &merge_plan(&s), &Unchanged, &none());
    assert_eq!(
        rep.operations[0].status,
        Status::Updated,
        "{:?}",
        rep.stopped
    );
}

#[test]
fn a_recovery_whose_ledger_write_fails_leaves_the_intent_open_and_the_next_one_repairs_it() {
    // The apply leaves an open intent: the new content is live, the ledger holds the old
    // fingerprint.
    let s = rearmed(
        |s| {
            fs::write(s.h.0.join("t"), "old\n").unwrap();
            own(s, "t", "old\n");
        },
        Fail::at("ledger-before-write", Inject::Error),
    );
    let p = plan(vec![op(
        &s.h,
        &s.store,
        "x@file",
        1,
        Action::UpdateFile,
        "t",
    )]);
    go(&s, &p, &gens(), &none());
    assert_eq!(phases(&s), [Phase::Intent]);
    let new_fp = file_fingerprint(b"new\n");
    let fingerprint = |s: &Setup| {
        s.store.read_ledger().unwrap().resources[0].owned_outputs[0]
            .fingerprint
            .clone()
    };
    // The first recovery cannot write the ledger. It fails and leaves the intent open.
    let Setup { h, store } = s;
    drop(store);
    let store = open_write_after_drop(&h, Fail::at("ledger-before-write", Inject::Error));
    let s = Setup { h, store };
    let home = Dir::open_home(&sys(&s.h), &s.h.0).unwrap();
    let err = dfm_engine::recover(&s.store, (&sys(&s.h), &home, s.h.0.as_path())).unwrap_err();
    assert_eq!(err.code, Code::IoFailed, "{err}");
    assert_eq!(phases(&s), [Phase::Intent], "the intent stays open");
    assert_ne!(fingerprint(&s).as_deref(), Some(new_fp.as_str()));
    // The next recovery repairs the fingerprint and closes the intent.
    let s = reopened(s);
    let settled = recover_now(&s);
    assert!(settled.open.is_empty(), "{:?}", settled.open);
    assert_eq!(phases(&s), [Phase::Intent, Phase::Done]);
    assert_eq!(fingerprint(&s).as_deref(), Some(new_fp.as_str()));
}

#[test]
fn a_crash_between_stage_and_rename_leaves_no_staged_copy_and_settles_not_done() {
    const CHILD: &str = "DFM_APPLY_STAGE_CRASH_HOME";
    let seed = |s: &Setup| {
        fs::write(s.h.0.join("f"), "old\n").unwrap();
        own(s, "f", "old\n");
    };
    if let Some(dir) = std::env::var_os(CHILD) {
        // The child: stages the new content, then ends at once, as a crash does.
        let h = Home(dir.into());
        let store = open(&h);
        let s = Setup { h, store };
        let p = plan(vec![op(
            &s.h,
            &s.store,
            "x@file",
            1,
            Action::UpdateFile,
            "f",
        )]);
        go_with(
            &s,
            &p,
            &gens(),
            &none(),
            &Fail::at("file-after-tmp", Inject::Abort),
        );
        std::process::exit(9);
    }
    let Setup { h, store } = setup();
    let s = Setup { h, store };
    seed(&s);
    let Setup { h, store } = s;
    drop(store);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_crash_between_stage_and_rename_leaves_no_staged_copy_and_settles_not_done",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &h.0)
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(6), "the child must abort: {status:?}");
    let s = Setup { store: open(&h), h };
    assert_eq!(leftovers(&s.h.0).len(), 1, "the crash left the staged copy");
    assert_eq!(phases(&s), [Phase::Intent]);
    // A file of the user that only looks like a staged name is not touched.
    fs::write(s.h.0.join(".f.2024.10.12.tmp"), "mine\n").unwrap();
    let settled = recover_now(&s);
    // The live file still holds the ledger content: the effect did not happen.
    assert!(settled.open.is_empty(), "{:?}", settled.open);
    assert!(settled.warnings.is_empty(), "{:?}", settled.warnings);
    assert_eq!(phases(&s), [Phase::Intent, Phase::Failed]);
    assert_eq!(
        leftovers(&s.h.0),
        [".f.2024.10.12.tmp"],
        "only the decoy is left"
    );
    assert_eq!(
        fs::read_to_string(s.h.0.join(".f.2024.10.12.tmp")).unwrap(),
        "mine\n"
    );
    assert_eq!(fs::read_to_string(s.h.0.join("f")).unwrap(), "old\n");
}

#[test]
fn a_leftover_stage_of_a_repeated_op_id_does_not_block_the_next_publish() {
    // The journal was archived by hand, so the op id restarts. An old stage under the tag of the
    // bare op id must not make the next publish fail with EEXIST.
    let s = setup();
    let o = op(
        &s.h,
        &s.store,
        "x@app.conf",
        1,
        Action::CreateFile,
        "app.conf",
    );
    let old = {
        let mut hex = dfm_model::digest::sha256_hex(format!("{}-1", o.journal_id).as_bytes());
        hex.truncate(16);
        hex
    };
    let mine = s.h.0.join(format!(".app.conf.{old}.tmp"));
    fs::write(&mine, "old\n").unwrap();
    let r = go(&s, &plan(vec![o]), &gen_for("x@app.conf", "a=1\n"), &none());
    assert_eq!(r.operations[0].status, Status::Updated, "{:?}", r.stopped);
    assert_eq!(fs::read_to_string(s.h.0.join("app.conf")).unwrap(), "a=1\n");
    assert_eq!(fs::read_to_string(&mine).unwrap(), "old\n", "left alone");
}

#[test]
fn a_crash_between_staging_a_link_and_the_rename_leaves_no_staged_link_and_settles_not_done() {
    const CHILD: &str = "DFM_APPLY_LINK_STAGE_CRASH_HOME";
    const NAME: &str =
        "a_crash_between_staging_a_link_and_the_rename_leaves_no_staged_link_and_settles_not_done";
    if let Some(dir) = std::env::var_os(CHILD) {
        // The child: stages the new link, then ends at once, as a crash does.
        let h = Home(dir.into());
        let store = open(&h);
        let s = Setup { h, store };
        let mut l = op(&s.h, &s.store, "x@file", 1, Action::ReplaceLink, "l");
        l.link = Some(format!("{}/x", real_src(&s)));
        go_with(
            &s,
            &plan_in(&real_src(&s), vec![l]),
            &gens(),
            &none(),
            &Fail::at("link-after-tmp", Inject::Abort),
        );
        std::process::exit(9);
    }
    let Setup { h, store } = setup();
    let s = Setup { h, store };
    symlink("/src/old", s.h.0.join("l")).unwrap();
    own_fp(&s, "l", &link_fingerprint("/src/old"));
    let Setup { h, store } = s;
    drop(store);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, &h.0)
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(6), "the child must abort: {status:?}");
    let s = Setup { store: open(&h), h };
    assert_eq!(leftovers(&s.h.0).len(), 1, "the crash left the staged link");
    assert_eq!(phases(&s), [Phase::Intent]);
    let settled = recover_now(&s);
    assert!(settled.open.is_empty(), "{:?}", settled.open);
    assert_eq!(phases(&s), [Phase::Intent, Phase::Failed]);
    assert!(leftovers(&s.h.0).is_empty(), "the staged link is removed");
    // The live link is still the old one: the effect did not happen.
    assert_eq!(
        fs::read_link(s.h.0.join("l")).unwrap(),
        Path::new("/src/old")
    );
}

#[test]
fn a_flag_and_a_ledger_refusal_carry_raw_text_that_the_command_escapes_once() {
    // The command escapes the whole refusal once when it prints it. A flag or a path escaped
    // here as well would print each backslash as four characters, and the pasted id would differ.
    assert_eq!(
        dfm_engine::flag(dfm_engine::Approval::Disruptive, r"remove@core.x@a\b'c"),
        r"--allow-disruptive 'remove@core.x@a\b'\''c'"
    );
    let s = setup();
    let mut l = s.store.read_ledger().unwrap();
    l.record(LedgerResource {
        resource: "core.x".into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        op_id: "seed-1".into(),
        owned_outputs: vec![OwnedOutput {
            path: r"/elsewhere/a\b".into(),
            mode: 0o600,
            fingerprint: Some(sha256_hex(b"x")),
            coowned: None,
        }],
    })
    .unwrap();
    let e = owned_outputs(&l, &s.h.0).unwrap_err();
    assert_eq!(e.code, Code::StateInvalid);
    assert!(
        e.detail.contains(r"/elsewhere/a\b is outside HOME"),
        "{}",
        e.detail
    );
}

#[test]
fn a_removal_of_a_file_with_a_backslash_names_its_real_id_in_the_flag() {
    let s = setup();
    fs::write(s.h.0.join(r"a\b"), "x").unwrap();
    own(&s, r"a\b", "x");
    let o = op(
        &s.h,
        &s.store,
        r"remove@core.x@a\b",
        1,
        Action::Remove,
        r"a\b",
    );
    let e = refused(&s, &plan(vec![o]), &NoGenerator, &none());
    assert_eq!(e.code, Code::DisruptiveUnapproved);
    assert!(
        e.detail.contains(r"--allow-disruptive 'remove@core.x@a\b'"),
        "{}",
        e.detail
    );
    assert!(s.h.0.join(r"a\b").exists());
}

#[test]
fn an_effect_failure_that_cannot_be_journaled_keeps_its_path_unescaped_in_the_refusal() {
    // The removal fails after its intent, and then the journal rejects the failure note. The
    // refusal carries the raw detail; the command escapes it once when it prints it.
    if Home::new().0.metadata().unwrap().uid() == 0 {
        eprintln!("SKIPPED: runs as root, which can write a read-only journal");
        return;
    }
    let s = setup();
    fs::write(s.h.0.join(r"a\b"), "mine\n").unwrap();
    own(&s, r"a\b", "mine\n");
    let o = op(
        &s.h,
        &s.store,
        r"remove@core.x@a\b",
        1,
        Action::Remove,
        r"a\b",
    );
    let home = s.h.0.clone();
    let journal = home.join(".local/state/dotfiles-manager/state/journal.jsonl");
    let allow = BTreeSet::from([r"remove@core.x@a\b".to_string()]);
    let rep = parked(
        &s,
        &plan(vec![o]),
        &NoGenerator,
        &allow,
        "apply-before-unlink",
        move || {
            fs::write(home.join("swap"), "mine\n").unwrap();
            fs::rename(home.join("swap"), home.join(r"a\b")).unwrap();
            fs::set_permissions(&journal, fs::Permissions::from_mode(0o400)).unwrap();
        },
    );
    let stopped = rep.stopped.expect("the run stopped");
    assert!(
        stopped.detail.contains("could not be journaled"),
        "{}",
        stopped.detail
    );
    assert!(
        stopped.detail.contains(r"a\b was replaced"),
        "{}",
        stopped.detail
    );
    assert!(!stopped.detail.contains(r"a\\b"), "{}", stopped.detail);
}
