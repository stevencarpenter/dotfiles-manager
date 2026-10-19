//! `plan` in an isolated HOME against copies of the fixtures: claims, collisions, topology,
//! ownership, edges, determinism, stale-plan detection and non-mutation (DFM-16).

mod common;

use common::{TempDir, copy_tree, repo_root};
use dfm_model::plan::{Action, Ownership, Saved, Status};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");

struct Env {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
}

impl Env {
    fn new(profile: &str) -> Env {
        let home = TempDir::new("plan-home");
        let src = TempDir::new("plan-src");
        let root = src.path().join("examples");
        copy_tree(&repo_root().join("examples"), &root);
        let e = Env {
            home,
            _src: src,
            root,
        };
        let p = e.root.join("profiles").join(profile);
        let o = e.run(&[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-personal",
        ]);
        assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
        e
    }

    fn personal() -> Env {
        Env::new("personal-dev-01.toml")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .env_clear()
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn plan(&self, extra: &[&str]) -> Output {
        let mut a = vec!["plan", "--root", self.root.to_str().unwrap()];
        a.extend_from_slice(extra);
        self.run(&a)
    }

    fn saved(&self) -> (Output, Saved) {
        let o = self.plan(&["--json"]);
        let s = Saved::parse(&o.stdout).unwrap_or_else(|e| panic!("{e}\n{}", text(&o.stdout)));
        (o, s)
    }

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn src(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn mkdirs(&self, rel: &str) {
        fs::create_dir_all(self.h(rel)).unwrap();
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn op<'a>(s: &'a Saved, id: &str) -> &'a dfm_model::plan::Operation {
    s.plan
        .operations
        .iter()
        .find(|o| o.id == id)
        .unwrap_or_else(|| panic!("no operation {id}"))
}

/// Names, kinds, modes, modification times and bytes of everything below `root`, links unread.
fn fingerprint(root: &Path) -> String {
    fn go(dir: &Path, h: &mut Sha256) {
        let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        v.sort_by_key(|e| e.file_name());
        for e in v {
            let m = fs::symlink_metadata(e.path()).unwrap();
            h.update(e.path().to_str().unwrap().as_bytes());
            h.update(
                format!("{:?} {:o}", m.modified().unwrap(), m.permissions().mode()).as_bytes(),
            );
            if m.is_dir() {
                go(&e.path(), h);
            } else if m.is_file() {
                h.update(fs::read(e.path()).unwrap());
            } else if m.file_type().is_symlink() {
                h.update(
                    fs::read_link(e.path())
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .as_bytes(),
                );
            }
        }
    }
    let mut h = Sha256::new();
    go(root, &mut h);
    format!("{:x}", h.finalize())
}

fn case_insensitive(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe");
    fs::write(&probe, b"x").unwrap();
    let folded = dir.join("caseprobe").exists();
    fs::remove_file(&probe).unwrap();
    folded
}

// ---- C01: claims ------------------------------------------------------------------------

#[test]
fn claims_are_enumerated_for_files_directories_and_distribution_leaves() {
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    fs::write(e.h(".codex/AGENTS.d/10-local.md"), "local\n").unwrap();
    fs::write(e.h(".codex/AGENTS.d/notes.txt"), "not a fragment\n").unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    // An exact path claim.
    let git = op(&s, "core.git@.config/git/config");
    assert_eq!(git.action, Action::CreateLink);
    assert!(git.effect.contains("-> ") && git.effect.contains("core/files/gitconfig"));
    // Distribution leaves: one per enabled entry under each root.
    for root in [".claude/skills", ".pi/agent/skills"] {
        let leaf = op(&s, &format!("agents.skills@{root}/sample-audit"));
        assert_eq!(leaf.action, Action::CreateLink);
        assert!(leaf.effect.contains("sample-audit"));
    }
    // A directory claim: only matching entries are fragments, and the foreign one is recorded.
    let asm = op(&s, "agents.codex-instructions@.codex/AGENTS.md");
    let names: Vec<_> = asm
        .fragments
        .iter()
        .map(|f| (&f.name[..], &f.origin[..]))
        .collect();
    assert_eq!(names, [("10-local.md", "foreign")]);
    assert!(s.plan.observations.listings.contains_key(".codex/AGENTS.d"));
    // Out of scope work is listed, never planned.
    let secret = op(&s, "personal-dev.example-secret@scope");
    assert_eq!(secret.status, Status::NotInScope);
    assert!(secret.mode.is_some() && secret.path.is_some());
    assert_eq!(
        op(&s, "personal-dev.atuin@scope").status,
        Status::NotInScope
    );
}

#[test]
fn a_case_alias_of_an_existing_entry_is_a_collision() {
    let e = Env::personal();
    if !case_insensitive(e.home.path()) {
        eprintln!("SKIPPED: this volume is case sensitive, so no case alias can exist");
        return;
    }
    e.mkdirs(".config/GIT");
    let o = e.plan(&[]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert!(
        err.contains("E_TARGET_COLLISION") && err.contains("`GIT`"),
        "{err}"
    );
}

// ---- C01/C07: parent links, directory and file transitions -------------------------------

/// Plans with `--json` and checks the first blocker exactly, then checks that the text run
/// ends with that blocker as its one error line and exit 3.
fn expect_blocked(e: &Env, code_name: &str, resource: &str, path: &str, detail: &str) -> Saved {
    let (o, s) = e.saved();
    assert_eq!(code(&o), 3, "{}", text(&o.stdout));
    let b = s.plan.blockers.first().expect("a blocker");
    assert_eq!(
        (
            b.code.as_str(),
            b.resource.as_str(),
            b.path.as_deref(),
            b.detail.as_str()
        ),
        (code_name, resource, Some(path), detail)
    );
    assert!(!b.correction.is_empty() && !b.source.is_empty());
    let t = e.plan(&[]);
    assert_eq!(code(&t), 3);
    assert_eq!(text(&t.stderr), format!("error: {}\n", b.refusal()));
    s
}

#[test]
fn dangling_recursive_and_redirecting_parent_links_are_rejected() {
    // Dangling.
    let e = Env::personal();
    e.mkdirs(".config");
    symlink("/nonexistent/target", e.h(".config/git")).unwrap();
    expect_blocked(
        &e,
        "E_PARENT_UNSAFE",
        "core.git",
        ".config/git/config",
        "parent .config/git is a dangling link (-> /nonexistent/target)",
    );

    // Recursive.
    let e = Env::personal();
    e.mkdirs(".config");
    symlink("git2", e.h(".config/git")).unwrap();
    symlink("git", e.h(".config/git2")).unwrap();
    expect_blocked(
        &e,
        "E_PARENT_UNSAFE",
        "core.git",
        ".config/git/config",
        "parent .config/git is a recursive link (-> git2)",
    );

    // A stale link that still resolves: it redirects the write out of the claim.
    let e = Env::personal();
    e.mkdirs(".config");
    e.mkdirs("elsewhere");
    symlink(e.h("elsewhere"), e.h(".config/git")).unwrap();
    let before = fingerprint(&e.h("elsewhere"));
    expect_blocked(
        &e,
        "E_PARENT_UNSAFE",
        "core.git",
        ".config/git/config",
        &format!(
            "parent .config/git is a link to a directory, so a write would land outside the claim (-> {})",
            e.h("elsewhere").display()
        ),
    );
    assert_eq!(fingerprint(&e.h("elsewhere")), before);

    // A link to a file where a directory is needed.
    let e = Env::personal();
    fs::write(e.h("a-file"), "x").unwrap();
    symlink("a-file", e.h(".ssh")).unwrap();
    expect_blocked(
        &e,
        "E_PARENT_UNSAFE",
        "core.ssh",
        ".ssh/config",
        "parent .ssh is a link where a directory is needed (-> a-file)",
    );
}

#[test]
fn directory_and_file_transitions_are_conflicts_and_nothing_is_replaced() {
    // A directory where the link goes.
    let e = Env::personal();
    e.mkdirs(".ssh/config");
    expect_blocked(
        &e,
        "E_CONFLICT",
        "core.ssh",
        ".ssh/config",
        ".ssh/config is a directory where a link is needed",
    );
    assert!(e.h(".ssh/config").is_dir());

    // A file where a parent directory goes.
    let e = Env::personal();
    fs::write(e.h(".ssh"), "x").unwrap();
    expect_blocked(
        &e,
        "E_CONFLICT",
        "core.ssh",
        ".ssh/config",
        ".ssh is a file where a directory is needed",
    );
    assert!(e.h(".ssh").is_file());

    // A file where a fragment directory goes.
    let e = Env::personal();
    e.mkdirs(".codex");
    fs::write(e.h(".codex/AGENTS.d"), "x").unwrap();
    expect_blocked(
        &e,
        "E_CONFLICT",
        "agents.codex-instructions",
        ".codex/AGENTS.d",
        ".codex/AGENTS.d is not a directory",
    );

    // A foreign link at the leaf, and a foreign file at the leaf.
    let e = Env::personal();
    e.mkdirs(".ssh");
    symlink("/somewhere/else", e.h(".ssh/config")).unwrap();
    expect_blocked(
        &e,
        "E_CONFLICT",
        "core.ssh",
        ".ssh/config",
        ".ssh/config exists and no ledger entry says this resource owns it",
    );
    assert_eq!(
        fs::read_link(e.h(".ssh/config")).unwrap(),
        Path::new("/somewhere/else")
    );
}

#[test]
fn a_directory_that_does_not_exist_yet_is_planned_in_dependency_order() {
    let e = Env::personal();
    let (_, s) = e.saved();
    let ssh = op(&s, "core.ssh@.ssh/config");
    assert_eq!(ssh.after, ["mkdir@.ssh"]);
    let deep = op(&s, "agents.skills@.pi/agent/skills/sample-audit");
    assert_eq!(deep.after, ["mkdir@.pi/agent/skills"]);
    assert_eq!(op(&s, "mkdir@.pi/agent/skills").after, ["mkdir@.pi/agent"]);
    let pos = |id: &str| s.plan.operations.iter().position(|o| o.id == id).unwrap();
    assert!(
        pos("mkdir@.pi") < pos("mkdir@.pi/agent")
            && pos("mkdir@.pi/agent") < pos("mkdir@.pi/agent/skills")
    );
}

// ---- C06: ownership fails closed --------------------------------------------------------

fn record(e: &Env, resource: &str, output: &Path, content: &str) {
    let o = e.run(&[
        "__record-op",
        "--resource",
        resource,
        "--output",
        output.to_str().unwrap(),
        "--content",
        content,
    ]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
}

#[test]
fn ownership_comes_from_the_ledger_only() {
    let base = ".config/external-overlays/git/base.inc";
    // Unowned content is never overwritten (C09: a representative source and ownership conflict).
    let e = Env::personal();
    e.mkdirs(".config/external-overlays/git");
    fs::write(e.h(base), "user content\n").unwrap();
    let s = expect_blocked(
        &e,
        "E_CONFLICT",
        "core.git-defaults",
        base,
        &format!("{base} exists and no ledger entry says this resource owns it"),
    );
    assert!(s.plan.blockers[0].source.ends_with("core/module.toml"));
    assert_eq!(fs::read_to_string(e.h(base)).unwrap(), "user content\n");

    // An output the ledger records, still unchanged, may be updated.
    let e = Env::personal();
    e.mkdirs(".config/external-overlays/git");
    record(&e, "core.git-defaults", &e.h(base), "managed\n");
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let git = op(&s, &format!("core.git-defaults@{base}"));
    assert_eq!(
        (git.action, git.ownership),
        (Action::UpdateFile, Ownership::Owned)
    );

    // After a user edit the fingerprint no longer matches: a conflict, not a replacement.
    fs::write(e.h(base), "edited by the user\n").unwrap();
    expect_blocked(
        &e,
        "E_CONFLICT",
        "core.git-defaults",
        base,
        &format!("{base} was changed after the manager wrote it"),
    );

    // An output another resource owns is foreign.
    let e = Env::personal();
    e.mkdirs(".config/external-overlays/git");
    record(&e, "core.git", &e.h(base), "managed\n");
    expect_blocked(
        &e,
        "E_CONFLICT",
        "core.git-defaults",
        base,
        &format!("{base} exists and no ledger entry says this resource owns it"),
    );
}

#[test]
fn a_sensitive_destination_that_exists_is_merged_in_place_and_leaks_nothing() {
    let secret = "sk-PLANTED-0123456789";
    let content = format!("{{\"token\":\"{secret}\"}}");
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    // The merge adapter owns this path (docs/adapters.md): an existing file is read, never
    // printed, and merged in place. With no ledger entry the manager may write. A ledger entry
    // of another resource blocks. A file open to group or others blocks with fix text.
    for (mode, other_owner, code_want, words) in [
        (0o600, false, 0, ""),
        (0o644, false, 3, "chmod 600"),
        (0o600, true, 3, "recorded in the ledger as an output of"),
    ] {
        let e = Env::personal();
        e.mkdirs(".claude");
        let out = e.h(".claude/settings.json");
        if !other_owner {
            fs::write(&out, &content).unwrap();
        }
        if other_owner {
            let o = e.run(&[
                "__record-op",
                "--resource",
                "gone.res",
                "--output",
                out.to_str().unwrap(),
                "--content",
                &content,
            ]);
            assert_eq!(code(&o), 0, "{}", text(&o.stderr));
        }
        fs::set_permissions(&out, fs::Permissions::from_mode(mode)).unwrap();
        for o in [e.plan(&[]), e.plan(&["--json"])] {
            assert_eq!(code(&o), code_want, "{}", text(&o.stderr));
            let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
            assert!(all.contains(words), "{all}");
            assert!(
                !all.contains(secret) && !all.contains(&digest),
                "secret leaked: {all}"
            );
        }
        assert_eq!(fs::read_to_string(&out).unwrap(), content);
    }
}

#[test]
fn state_is_required_because_ownership_would_be_unknown() {
    let e = Env::personal();
    fs::remove_dir_all(e.h(".local/state/dotfiles-manager/state")).unwrap();
    let o = e.plan(&[]);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_STATE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    // An unbound machine is refused before anything is read.
    let fresh = TempDir::new("plan-unbound");
    let o = Command::new(BIN)
        .env_clear()
        .arg("--home")
        .arg(fresh.path())
        .args(["plan", "--root", e.root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(code(&o), 3);
    assert!(text(&o.stderr).contains("E_BINDING_INVALID"));
}

#[test]
fn a_directory_the_user_cannot_write_is_a_privilege_failure() {
    if is_root() {
        eprintln!("SKIPPED: running as root");
        return;
    }
    let e = Env::personal();
    e.mkdirs(".ssh");
    let _restore = Restore(e.h(".ssh"), 0o700);
    fs::set_permissions(e.h(".ssh"), fs::Permissions::from_mode(0o500)).unwrap();
    let s = e.saved().1;
    let b = &s.plan.blockers[0];
    assert_eq!(b.code.as_str(), "E_PRIVILEGE_UNSUPPORTED");
    assert_eq!(
        b.detail,
        ".ssh is not writable by the current user; a HOME adapter never escalates"
    );
    assert_eq!(
        (b.resource.as_str(), b.path.as_deref()),
        ("core.ssh", Some(".ssh/config"))
    );
}

// ---- C02/C03: edges, cycles, no module barrier ------------------------------------------

#[test]
fn a_fragment_producer_precedes_its_consumer_and_modules_are_not_barriers() {
    let e = Env::new("work-01.toml");
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let consumer = op(&s, "agents.codex-instructions@.codex/AGENTS.md");
    assert!(
        consumer
            .after
            .contains(&"work.instructions@.codex/AGENTS.d/40-work.md".to_string()),
        "{:?}",
        consumer.after
    );
    assert!(
        consumer
            .fragments
            .iter()
            .any(|f| f.name == "40-work.md" && f.origin == "selected:work.instructions")
    );
    let pos = |id: &str| s.plan.operations.iter().position(|o| o.id == id).unwrap();
    assert!(pos("work.instructions@.codex/AGENTS.d/40-work.md") < pos(&consumer.id));
    // One edge crosses from module work to module agents. No other operation waits for a module.
    for o in &s.plan.operations {
        for a in &o.after {
            let dep = op(&s, a);
            let same = dep.module == o.module;
            let allowed =
                a.starts_with("mkdir@") || a == "work.instructions@.codex/AGENTS.d/40-work.md";
            assert!(same || allowed, "{} waits for {a}", o.id);
        }
    }
    let ssh = op(&s, "core.ssh@.ssh/config");
    assert_eq!(ssh.after, ["mkdir@.ssh"]);
}

const LOOP: &str = r#"
[[resources]]
id = "loop-a"
kind = "assemble"
adapter = "text-fragments/v1"
target = "loopb/a.md"
[resources.value]
body = "files/AGENTS.md"
fragments_dir = "loopa"
pattern = "*.md"
separator = "\n"

[[resources]]
id = "loop-b"
kind = "assemble"
adapter = "text-fragments/v1"
target = "loopa/b.md"
[resources.value]
body = "files/AGENTS.md"
fragments_dir = "loopb"
pattern = "*.md"
separator = "\n"
"#;

#[test]
fn two_assemblers_that_feed_each_other_are_a_cycle() {
    let e = Env::personal();
    let m = e.src("base/modules/agents/module.toml");
    let mut t = fs::read_to_string(&m).unwrap();
    t.push_str(LOOP);
    fs::write(&m, t).unwrap();
    // The resolver accepts the pair (docs/resolver.md, "Fragment injection"); the planner must not.
    expect_blocked(
        &e,
        "E_PLAN_CYCLE",
        "agents.loop-a",
        "loopb/a.md",
        "operations form a cycle: agents.loop-a@loopb/a.md -> agents.loop-b@loopa/b.md -> agents.loop-a@loopb/a.md. Each output is a fragment of the next one",
    );
    let o = e.plan(&[]);
    let out = text(&o.stdout);
    assert!(
        out.contains("blocked agents.loop-a@") && out.contains("blocked agents.loop-b@"),
        "{out}"
    );
}

// ---- C05: determinism --------------------------------------------------------------------

#[test]
fn identical_inputs_and_observations_give_the_same_plan() {
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    for n in ["30-c.md", "10-a.md", "20-b.md"] {
        fs::write(e.h(&format!(".codex/AGENTS.d/{n}")), n).unwrap();
    }
    let a = e.plan(&["--json"]);
    let b = e.plan(&["--json"]);
    assert_eq!(a.stdout, b.stdout);
    let (sa, sb) = (
        Saved::parse(&a.stdout).unwrap(),
        Saved::parse(&b.stdout).unwrap(),
    );
    assert_eq!(sa.plan_digest, sb.plan_digest);
    let t1 = e.plan(&[]);
    let t2 = e.plan(&[]);
    assert_eq!(t1.stdout, t2.stdout);
    assert!(text(&t1.stdout).contains(&format!("plan digest: {}", sa.plan_digest)));
}

#[test]
fn the_plan_records_identities_ownership_privilege_and_conditions() {
    let e = Env::personal();
    let (_, s) = e.saved();
    let p = &s.plan;
    assert_eq!(p.binary.sha256.len(), 64);
    assert_eq!(p.sources[0].kind, "path");
    assert!(p.sources[0].tree.starts_with("sha256:"));
    assert_eq!(p.model_digest.len(), 64);
    assert!(!p.observations.seen.is_empty());
    let git = op(&s, "core.git@.config/git/config");
    assert_eq!(git.privilege, "user");
    assert_eq!(git.ownership, Ownership::Absent);
    assert!(
        git.preconditions
            .iter()
            .any(|c| c.subject == "path:.config/git/config" && c.state == "missing")
    );
    assert!(
        git.preconditions
            .iter()
            .any(|c| c.subject.starts_with("source:"))
    );
    assert!(!git.postcondition.is_empty() && !git.effect.is_empty());
    // A sensitive output shows destination and mode only, and no content hash.
    let settings = op(&s, "agents.claude-settings@.claude/settings.json");
    assert!(
        settings
            .effect
            .contains("sensitive: destination and mode only")
    );
}

// ---- DFM-45: adoption of an exact existing link --------------------------------------------

/// The link text `core.ssh` would publish, taken from a plan made before the leaf exists.
fn ssh_link(e: &Env) -> String {
    let (_, s) = e.saved();
    op(&s, "core.ssh@.ssh/config").link.clone().unwrap()
}

#[test]
fn an_exact_unowned_link_is_planned_as_an_adoption_with_a_leaf_and_ledger_precondition() {
    let e = Env::personal();
    let (_, before) = e.saved();
    let link = ssh_link(&e);
    e.mkdirs(".ssh");
    symlink(&link, e.h(".ssh/config")).unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stdout));
    let a = op(&s, "core.ssh@.ssh/config");
    assert_eq!((a.action, a.status), (Action::Adopt, Status::Planned));
    assert_eq!(a.ownership, Ownership::UnrecordedCurrent);
    assert_eq!(a.link.as_deref(), Some(link.as_str()));
    assert!(a.after.is_empty(), "{:?}", a.after);
    let pre = |subject: &str| {
        a.preconditions
            .iter()
            .find(|p| p.subject == subject)
            .map(|p| p.state.clone())
    };
    assert_eq!(pre("path:.ssh/config"), Some(format!("link:{link}:file")));
    assert_eq!(pre("ledger:.ssh/config").as_deref(), Some("none"));
    // The new action is part of the plan digest and the saved form accepts it.
    assert_ne!(s.plan_digest, before.plan_digest);
    let bytes = s.to_json().unwrap();
    let back = Saved::parse(&bytes).unwrap();
    assert_eq!(back.plan, s.plan);
    assert!(text(&bytes).contains("\"action\":\"adopt\""));
    // The human form says what happens.
    let t = text(&e.plan(&[]).stdout);
    assert!(
        t.contains("adopt .ssh/config: record the existing link"),
        "{t}"
    );
}

#[test]
fn a_change_of_an_adoption_leaf_makes_the_saved_plan_stale() {
    let e = Env::personal();
    let link = ssh_link(&e);
    e.mkdirs(".ssh");
    symlink(&link, e.h(".ssh/config")).unwrap();
    let (f, _) = save(&e);
    fs::remove_file(e.h(".ssh/config")).unwrap();
    symlink("/somewhere/else", e.h(".ssh/config")).unwrap();
    let o = against(&e, &f);
    assert_eq!(code(&o), 3, "{}{}", text(&o.stdout), text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_PLAN_STALE"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn a_regular_file_at_a_link_target_is_a_conflict_that_names_the_file_and_the_fix() {
    let e = Env::personal();
    let link = ssh_link(&e);
    e.mkdirs(".ssh");
    // The content does not matter: a file is not the link the resource creates.
    let source = fs::read(e.src("base/modules/core/files/ssh_config")).unwrap();
    fs::write(e.h(".ssh/config"), source).unwrap();
    let s = expect_blocked(
        &e,
        "E_CONFLICT",
        "core.ssh",
        ".ssh/config",
        &format!(
            ".ssh/config is a regular file, not the link -> {} this resource would create; the manager adopts only an exact existing link",
            link
        ),
    );
    assert_eq!(
        s.plan.blockers[0].correction,
        "move the file aside, or replace it with that link by hand"
    );
    assert!(op(&s, "core.ssh@.ssh/config").action != Action::Adopt);
}

#[test]
fn a_blocker_detail_is_escaped_once_where_it_is_printed() {
    // The link text holds a backslash and a real newline. Escaped once, that prints as `\\` and
    // `\n`. Escaped twice, the backslash would print as four characters.
    let e = Env::personal();
    e.mkdirs(".config");
    symlink("/nonexistent/a\\b\nc", e.h(".config/git")).unwrap();
    let once = "(-> /nonexistent/a\\\\b\\nc)";
    let o = e.plan(&[]);
    assert_eq!(code(&o), 3);
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert!(out.contains(once), "{out}");
    assert!(err.contains(once), "{err}");
    assert!(!out.contains("\\\\\\"), "{out}");
    assert!(!err.contains("\\\\\\"), "{err}");
}

// ---- C06/C07: stale plans ----------------------------------------------------------------

fn save(e: &Env) -> (PathBuf, Saved) {
    let (_, s) = e.saved();
    let f = e.h("saved-plan.json");
    fs::write(&f, serde_json::to_vec(&s).unwrap()).unwrap();
    (f, s)
}

fn against(e: &Env, f: &Path) -> Output {
    e.plan(&["--against", f.to_str().unwrap()])
}

/// Exit 3, one stale line `stale <kind>: <subject> was ...` and the error line for the first
/// difference, which must be the most specific one.
fn expect_stale(o: &Output, kind: &str, subject: &str) {
    assert_eq!(code(o), 3, "{}{}", text(&o.stdout), text(&o.stderr));
    let out = text(&o.stdout);
    let prefix = format!("stale {kind}: {subject} was ");
    assert!(
        out.lines().any(|l| l.starts_with(&prefix)),
        "{prefix}\n{out}"
    );
    let first = out.lines().find(|l| l.starts_with("stale ")).unwrap();
    assert!(first.starts_with(&prefix), "most specific first: {first}");
    let err = text(&o.stderr);
    assert!(
        err.starts_with(&format!(
            "error: E_PLAN_STALE {kind} changed: {subject} was "
        )),
        "{err}"
    );
}

#[test]
fn an_unchanged_world_matches_the_saved_plan() {
    let e = Env::personal();
    let (f, s) = save(&e);
    let o = against(&e, &f);
    assert_eq!(code(&o), 0, "{}{}", text(&o.stdout), text(&o.stderr));
    assert!(text(&o.stdout).contains("plan current"));
    assert!(text(&o.stdout).contains(&s.plan_digest));
}

#[test]
fn a_source_edit_after_planning_invalidates_the_plan() {
    let e = Env::personal();
    let (f, _) = save(&e);
    fs::write(
        e.src("base/modules/core/files/gitconfig"),
        "[user]\n name = changed\n",
    )
    .unwrap();
    let o = against(&e, &f);
    expect_stale(&o, "source input", "core.git@.config/git/config (core.git)");
    assert!(text(&o.stdout).contains("stale source: "));
}

#[test]
fn a_stale_refusal_is_escaped_once_where_it_is_printed() {
    // A new fragment name holds a backslash and shows in the stale values. The stale line and the
    // error line each print it once escaped (two backslashes), never four.
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    fs::write(e.h(".codex/AGENTS.d/10-a.md"), "a").unwrap();
    let (f, _) = save(&e);
    fs::write(e.h(".codex/AGENTS.d/20-b\\c.md"), "b").unwrap();
    let o = against(&e, &f);
    assert_eq!(code(&o), 3, "{}{}", text(&o.stdout), text(&o.stderr));
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert!(err.starts_with("error: E_PLAN_STALE "), "{err}");
    for all in [&out, &err] {
        assert!(all.contains("20-b\\\\c.md"), "{all}");
        assert!(!all.contains("b\\\\\\"), "{all}");
    }
}

#[test]
fn a_new_foreign_fragment_or_a_removed_one_changes_the_input_set() {
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    fs::write(e.h(".codex/AGENTS.d/10-a.md"), "a").unwrap();
    let (f, _) = save(&e);
    fs::write(e.h(".codex/AGENTS.d/20-b.md"), "b").unwrap();
    expect_stale(&against(&e, &f), "observation", "fragments:.codex/AGENTS.d");
    fs::remove_file(e.h(".codex/AGENTS.d/20-b.md")).unwrap();
    assert_eq!(code(&against(&e, &f)), 0);
    fs::remove_file(e.h(".codex/AGENTS.d/10-a.md")).unwrap();
    expect_stale(&against(&e, &f), "observation", "fragments:.codex/AGENTS.d");
    // A changed fragment is a changed input too.
    fs::write(e.h(".codex/AGENTS.d/10-a.md"), "changed").unwrap();
    expect_stale(&against(&e, &f), "observation", "fragments:.codex/AGENTS.d");
}

#[test]
fn a_target_that_appears_or_changes_type_invalidates_the_plan() {
    let e = Env::personal();
    let (f, _) = save(&e);
    e.mkdirs(".ssh");
    expect_stale(&against(&e, &f), "observation", "path:.ssh");

    // Topology: a directory replaced by a link after planning.
    let e = Env::personal();
    e.mkdirs(".ssh");
    let (f, _) = save(&e);
    fs::remove_dir(e.h(".ssh")).unwrap();
    symlink("/elsewhere", e.h(".ssh")).unwrap();
    let o = against(&e, &f);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_PLAN_STALE"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn changed_ownership_invalidates_the_plan() {
    let base = ".config/external-overlays/git/base.inc";
    let e = Env::personal();
    e.mkdirs(".config/external-overlays/git");
    record(&e, "core.git-defaults", &e.h(base), "managed\n");
    let (f, _) = save(&e);
    // The user edits the owned file after the plan was made.
    fs::write(e.h(base), "edited\n").unwrap();
    let o = against(&e, &f);
    assert_eq!(code(&o), 3);
    let err = text(&o.stderr);
    assert!(err.contains("E_PLAN_STALE"), "{err}");
    assert!(
        text(&o.stdout).contains("stale ownership: core.git-defaults@"),
        "{}",
        text(&o.stdout)
    );
}

#[test]
fn a_saved_plan_that_was_edited_or_is_unreadable_is_refused() {
    let e = Env::personal();
    let (f, s) = save(&e);

    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    v["plan"]["operations"][0]["effect"] = "something else".into();
    fs::write(&f, serde_json::to_vec(&v).unwrap()).unwrap();
    let o = against(&e, &f);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_PLAN_INVALID"),
        "{}",
        text(&o.stderr)
    );

    let mut v = serde_json::to_value(&s).unwrap();
    v["plan"]["schema"] = 2.into();
    fs::write(&f, serde_json::to_vec(&v).unwrap()).unwrap();
    let o = against(&e, &f);
    assert_eq!(code(&o), 3);
    assert!(text(&o.stderr).contains("E_SCHEMA_UNSUPPORTED"));

    fs::write(&f, "not json").unwrap();
    assert!(text(&against(&e, &f).stderr).contains("E_PLAN_INVALID"));

    let missing = e.h("none.json");
    let o = against(&e, &missing);
    assert_eq!(code(&o), 2);
    assert!(text(&o.stderr).contains("E_REPORT_UNAVAILABLE"));
}

// ---- C07: no writes, no processes, no network --------------------------------------------

#[test]
fn plan_changes_nothing_and_starts_no_provider() {
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    fs::write(e.h(".codex/AGENTS.d/10-a.md"), "a").unwrap();
    // Provider look-alikes on PATH record any start.
    let bin = TempDir::new("plan-path");
    let marker = bin.path().join("started");
    for tool in [
        "op",
        "brew",
        "git",
        "mise",
        "curl",
        "ssh",
        "security",
        "launchctl",
        "sudo",
    ] {
        let p = bin.path().join(tool);
        fs::write(
            &p,
            format!("#!/bin/sh\necho {tool} >> '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (home_before, src_before) = (fingerprint(e.home.path()), fingerprint(&e.root));
    for extra in [&[][..], &["--json"][..]] {
        let mut a = vec!["--home", e.home.path().to_str().unwrap(), "plan", "--root"];
        a.push(e.root.to_str().unwrap());
        a.extend_from_slice(extra);
        let o = Command::new(BIN)
            .env_clear()
            .env("PATH", bin.path())
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .args(a)
            .output()
            .unwrap();
        assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    }
    assert!(
        !marker.exists(),
        "a provider was started: {:?}",
        fs::read_to_string(&marker)
    );
    assert_eq!(fingerprint(e.home.path()), home_before, "HOME changed");
    assert_eq!(fingerprint(&e.root), src_before, "the sources changed");
}

#[test]
fn the_planner_code_has_no_process_network_or_write_calls() {
    let root = repo_root();
    let files = [
        "crates/model/src/plan/mod.rs",
        "crates/model/src/plan/build.rs",
        "crates/model/src/plan/graph.rs",
        "crates/model/src/plan/stale.rs",
        "crates/engine/src/observe.rs",
        "crates/cli/src/plan_cmd.rs",
    ];
    let banned = [
        // The runner and the package probes start processes; they belong to the commands that
        // own a scope (docs/packages.md), never to the plan builder.
        "Runner",
        "runner(",
        "package_facts",
        "std::net",
        "TcpStream",
        "UdpSocket",
        "process::Command",
        "Command::new",
        "std::fs::write",
        "fs::write",
        "create_dir",
        "remove_file",
        "remove_dir",
        "rename(",
        "symlink(",
        "OpenOptions::new().write",
        ".create(true)",
        "mkdirat",
        "unlinkat",
        "renameat",
        "symlinkat",
    ];
    for f in files {
        let src = fs::read_to_string(root.join(f)).unwrap();
        // Skip the test module and doc text; only code lines count.
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for b in banned {
            assert!(!code.contains(b), "{f} contains {b}");
        }
    }
}

// ---- exit mapping (section 10) ----------------------------------------------------------

#[test]
fn plan_follows_the_exit_table_for_resolver_and_report_failures() {
    // A malformed manifest is a gate failure before mutation: exit 3 (doctor maps it to 1).
    let e = Env::personal();
    fs::write(
        e.src("base/modules/core/module.toml"),
        "schema = 1\nnot valid toml [\n",
    )
    .unwrap();
    let o = e.plan(&[]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("E_"), "{}", text(&o.stderr));

    // A source that cannot be read means no report can be produced: exit 2.
    let o = e.run(&["plan", "--root", "/nonexistent-dfm-root"]);
    assert_eq!(code(&o), 2);
    assert!(text(&o.stderr).contains("E_REPORT_UNAVAILABLE"));

    // Usage errors.
    for args in [
        &["plan"][..],
        &["plan", "--root"][..],
        &["plan", "--root", "x", "--root", "y"][..],
        &["plan", "--root", "x", "--bogus"][..],
        &["plan", "--root", "x", "--json", "--json"][..],
    ] {
        assert_eq!(code(&e.run(args)), 2, "{args:?}");
    }
}

// ---- C08: a rejected plan explains itself -------------------------------------------------

#[test]
fn a_blocker_names_source_resource_field_claim_and_one_fix() {
    let e = Env::personal();
    e.mkdirs(".ssh");
    fs::write(e.h(".ssh/config"), "Host *\n").unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 3);
    let b = &s.plan.blockers[0];
    assert_eq!(b.code.as_str(), "E_CONFLICT");
    assert_eq!(b.resource, "core.ssh");
    assert!(
        b.source.ends_with("modules/core/module.toml"),
        "{}",
        b.source
    );
    assert_eq!(b.field.as_deref(), Some("target"));
    assert_eq!(b.path.as_deref(), Some(".ssh/config"));
    assert!(b.detail.contains(".ssh/config") && !b.correction.is_empty());
    assert_eq!(op(&s, "core.ssh@.ssh/config").status, Status::Blocked);
    // The first blocker is what the error line and the exit code report.
    let err = text(&o.stderr);
    assert!(
        err.starts_with("error: E_CONFLICT core.ssh (.ssh/config, target)"),
        "{err}"
    );
}

#[test]
fn untrusted_text_in_a_blocker_is_escaped() {
    let e = Env::personal();
    e.mkdirs(".config");
    symlink("/x\u{202e}y", e.h(".config/git")).unwrap();
    let o = e.plan(&[]);
    assert_eq!(code(&o), 3);
    let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
    assert!(
        !all.contains('\u{202e}') && all.contains("\\u{202e}"),
        "{all}"
    );
}

// ---- the library with synthetic observations (facts a test cannot create as this user) -----

mod synthetic {
    use super::*;
    use dfm_cli::builtin_catalog;
    use dfm_model::Code;
    use dfm_model::plan::{
        Binary, Context, Node, Observations, Owned, Plan, Seen, Source, build, followups, requests,
    };
    use dfm_model::resolve::{SourceSet, resolve};
    use std::collections::BTreeMap;

    fn load(dir: &Path, rel: &str, out: &mut SourceSet) {
        let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        v.sort_by_key(|e| e.file_name());
        for e in v {
            let name = e.file_name().into_string().unwrap();
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let ty = e.file_type().unwrap();
            if ty.is_symlink() {
                out.insert_link(path, fs::read_link(e.path()).unwrap().to_str().unwrap());
            } else if ty.is_dir() {
                load(&e.path(), &path, out);
            } else {
                out.insert_file(path, fs::read(e.path()).unwrap());
            }
        }
    }

    fn observed() -> (dfm_model::resolve::Resolved, Observations) {
        observed_with(|_| {})
    }

    /// Like `observed`, after `prep` has made real entries in the temporary HOME.
    fn observed_with(prep: impl FnOnce(&Path)) -> (dfm_model::resolve::Resolved, Observations) {
        let mut set = SourceSet::new();
        load(&repo_root().join("examples"), "", &mut set);
        let resolved = resolve(&set, "profiles/personal-dev-01.toml", &builtin_catalog()).unwrap();
        let home = TempDir::new("synthetic-home");
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        prep(home.path());
        let sys = dfm_engine::Sys {
            uid: dfm_platform_macos::current_uid().unwrap(),
        };
        let dir = dfm_engine::Dir::open_home(&sys, home.path()).unwrap();
        let mut obs = dfm_engine::observe(
            &sys,
            &dir,
            &requests(&resolved, &[], dfm_model::plan::Scope::Config),
        )
        .unwrap();
        let more = dfm_engine::observe(
            &sys,
            &dir,
            &followups(&resolved, &obs, &[], dfm_model::plan::Scope::Config),
        )
        .unwrap();
        dfm_engine::merge(&mut obs, more);
        (resolved, obs)
    }

    fn plan(resolved: &dfm_model::resolve::Resolved, obs: &Observations) -> Plan {
        plan_with(resolved, obs, vec![], BTreeMap::new())
    }

    fn plan_with(
        resolved: &dfm_model::resolve::Resolved,
        obs: &Observations,
        ledger: Vec<Owned>,
        reserved: BTreeMap<String, String>,
    ) -> Plan {
        plan_full(resolved, obs, ledger, reserved, BTreeMap::new())
    }

    fn plan_full(
        resolved: &dfm_model::resolve::Resolved,
        obs: &Observations,
        ledger: Vec<Owned>,
        reserved: BTreeMap<String, String>,
        merge: BTreeMap<String, dfm_model::plan::MergeFact>,
    ) -> Plan {
        let cx = Context {
            scope: dfm_model::plan::Scope::Config,
            secret: std::collections::BTreeMap::new(),
            package: std::collections::BTreeMap::new(),
            service: std::collections::BTreeMap::new(),
            restart: std::collections::BTreeSet::new(),
            binary: Binary {
                version: "0".into(),
                sha256: "0".repeat(64),
            },
            profile: "p",
            machine_id: "m",
            sources: vec![Source {
                kind: "path".into(),
                root: "/r".into(),
                tree: "sha256:0".into(),
            }],
            root: "/r",
            ledger,
            reserved,
            unchecked: BTreeMap::new(),
            merge,
        };
        build(resolved, &cx, obs).unwrap()
    }

    fn first(p: &Plan, id: &str) -> Code {
        p.blockers
            .iter()
            .find(|b| b.resource == id)
            .unwrap_or_else(|| panic!("no blocker for {id}: {:?}", p.blockers))
            .code
    }

    fn put(obs: &mut Observations, path: &str, node: Node, alias: Option<&str>) {
        obs.seen.insert(
            path.into(),
            Seen {
                node,
                alias: alias.map(String::from),
            },
        );
    }

    #[test]
    fn the_same_inputs_build_equal_plans() {
        let (r, obs) = observed();
        let (a, b) = (plan(&r, &obs), plan(&r, &obs));
        assert_eq!(a, b);
        assert_eq!(a.digest().unwrap(), b.digest().unwrap());
        assert!(a.blockers.is_empty());
        // The digest depends on the observations.
        let mut other = obs.clone();
        put(
            &mut other,
            ".ssh",
            Node::Dir {
                mode: 0o700,
                own: true,
            },
            None,
        );
        assert_ne!(plan(&r, &other).digest().unwrap(), a.digest().unwrap());
    }

    #[test]
    fn a_parent_owned_by_another_user_needs_privilege_that_a_home_adapter_never_has() {
        let (r, mut obs) = observed();
        put(
            &mut obs,
            ".ssh",
            Node::Dir {
                mode: 0o755,
                own: false,
            },
            None,
        );
        let p = plan(&r, &obs);
        assert_eq!(first(&p, "core.ssh"), Code::PrivilegeUnsupported);
        let leaf = Node::File {
            mode: 0o644,
            own: false,
            size: 1,
            sha256: None,
        };
        put(
            &mut obs,
            ".ssh",
            Node::Dir {
                mode: 0o755,
                own: true,
            },
            None,
        );
        put(&mut obs, ".ssh/config", leaf, None);
        assert_eq!(
            first(&plan(&r, &obs), "core.ssh"),
            Code::PrivilegeUnsupported
        );
    }

    #[test]
    fn a_leaf_alias_is_a_collision_even_when_the_ledger_owns_the_entry() {
        let (r, mut obs) = observed();
        put(
            &mut obs,
            ".ssh",
            Node::Dir {
                mode: 0o700,
                own: true,
            },
            None,
        );
        let target = "/src/ssh_config";
        let link = Node::Link {
            target: target.into(),
            to: dfm_model::plan::LinkTo::File,
            own: true,
        };
        put(&mut obs, ".ssh/config", link, Some("CONFIG"));
        let ledger = vec![Owned {
            resource: "core.ssh".into(),
            adapter: "fs-link/v1".into(),
            class: dfm_model::state::Class::Normal,
            path: ".ssh/config".into(),
            fingerprint: Some(dfm_model::plan::link_fingerprint(target)),
            coowned_entries: None,
        }];
        let p = plan_with(&r, &obs, ledger, BTreeMap::new());
        let b = p
            .blockers
            .iter()
            .find(|b| b.resource == "core.ssh")
            .unwrap();
        assert_eq!(b.code, Code::TargetCollision);
        assert_eq!(
            b.detail,
            ".ssh/config differs from the existing entry `CONFIG` only by case; the filesystem treats them as one"
        );
        let op = p
            .operations
            .iter()
            .find(|o| o.id == "core.ssh@.ssh/config")
            .unwrap();
        assert_eq!(op.action, dfm_model::plan::Action::None);
    }

    fn dir(own: bool) -> Node {
        Node::Dir { mode: 0o755, own }
    }

    fn file(own: bool, mode: u32) -> Node {
        Node::File {
            mode,
            own,
            size: 1,
            sha256: Some("0".repeat(64)),
        }
    }

    fn owned(resource: &str, path: &str, fingerprint: Option<String>) -> Owned {
        Owned {
            resource: resource.into(),
            adapter: "x/v1".into(),
            class: dfm_model::state::Class::Normal,
            path: path.into(),
            fingerprint,
            coowned_entries: None,
        }
    }

    /// The blocker of `resource` whose `field` is `field`, with its code and detail.
    fn blocker_at<'a>(p: &'a Plan, resource: &str, field: &str) -> (Code, &'a str) {
        let b = p
            .blockers
            .iter()
            .find(|b| b.resource == resource && b.field.as_deref() == Some(field))
            .unwrap_or_else(|| panic!("no {field} blocker for {resource}: {:?}", p.blockers));
        (b.code, b.detail.as_str())
    }

    fn action_of(p: &Plan, id: &str) -> (dfm_model::plan::Action, Status) {
        let o = p.operations.iter().find(|o| o.id == id).unwrap();
        (o.action, o.status)
    }

    const GD: &str = ".config/external-overlays/git/base.inc";

    /// Parents of the render `core.git-defaults` exist, so only its leaf decides.
    fn with_git_defaults_parents(obs: &mut Observations) {
        for d in [
            ".config",
            ".config/external-overlays",
            ".config/external-overlays/git",
        ] {
            put(obs, d, dir(true), None);
        }
    }

    #[test]
    fn a_file_whose_bytes_spell_a_link_fingerprint_is_not_the_owned_link() {
        let (r, obs) = observed();
        let want = plan(&r, &obs)
            .operations
            .iter()
            .find(|o| o.id == "core.ssh@.ssh/config")
            .and_then(|o| o.link.clone())
            .unwrap();
        // A regular file whose content is `link\0<target>` hashes like the link would, without
        // the file domain prefix.
        let crafted = format!("link\0{want}");
        let (r, obs) = observed_with(|home| {
            fs::create_dir(home.join(".ssh")).unwrap();
            fs::write(home.join(".ssh/config"), crafted.as_bytes()).unwrap();
        });
        let ledger = vec![owned(
            "core.ssh",
            ".ssh/config",
            Some(dfm_model::plan::link_fingerprint(&want)),
        )];
        let p = plan_with(&r, &obs, ledger, BTreeMap::new());
        assert_eq!(first(&p, "core.ssh"), Code::Conflict);
        let op = p
            .operations
            .iter()
            .find(|o| o.id == "core.ssh@.ssh/config")
            .unwrap();
        assert_eq!(
            (op.action, op.ownership),
            (Action::None, Ownership::Modified)
        );
        // The observed hash of the file is its file fingerprint, never the plain hash.
        let Some(Node::File { sha256, .. }) = obs.seen.get(".ssh/config").map(|s| &s.node) else {
            panic!("no file observed");
        };
        assert_eq!(
            sha256.as_deref(),
            Some(dfm_model::plan::file_fingerprint(crafted.as_bytes()).as_str())
        );
    }

    #[test]
    fn a_link_the_ledger_owns_is_not_updated_as_a_file_when_the_resource_became_a_render() {
        let (r, mut obs) = observed();
        with_git_defaults_parents(&mut obs);
        let target = "/src/old";
        let link = Node::Link {
            target: target.into(),
            to: dfm_model::plan::LinkTo::File,
            own: true,
        };
        put(&mut obs, GD, link, None);
        let ledger = vec![owned(
            "core.git-defaults",
            GD,
            Some(dfm_model::plan::link_fingerprint(target)),
        )];
        let p = plan_with(&r, &obs, ledger, BTreeMap::new());
        assert_eq!(first(&p, "core.git-defaults"), Code::Conflict);
        assert!(p.blockers.iter().any(|b| {
            b.detail
                .contains("is a link where this resource now generates a file")
        }));
        assert_eq!(
            action_of(&p, &format!("core.git-defaults@{GD}")),
            (Action::None, Status::Blocked)
        );
        assert!(p.operations.iter().all(|o| o.action != Action::UpdateFile));
    }

    #[test]
    fn a_file_the_ledger_owns_is_not_replaced_by_a_link_when_the_resource_became_a_link() {
        let (r, mut obs) = observed();
        put(&mut obs, ".ssh", dir(true), None);
        // The ledger holds the fingerprint of this very file, so ownership reads Owned.
        put(&mut obs, ".ssh/config", file(true, 0o600), None);
        let ledger = vec![owned("core.ssh", ".ssh/config", Some("0".repeat(64)))];
        let p = plan_with(&r, &obs, ledger, BTreeMap::new());
        assert_eq!(first(&p, "core.ssh"), Code::Conflict);
        assert!(p.blockers.iter().any(|b| {
            b.detail
                .contains("is a file where this resource now creates a link")
        }));
        assert_eq!(
            action_of(&p, "core.ssh@.ssh/config"),
            (Action::None, Status::Blocked)
        );
        assert!(p.operations.iter().all(|o| o.action != Action::ReplaceLink));
    }

    #[test]
    fn an_owner_that_cannot_be_verified_blocks_a_link_replace_and_a_file_update() {
        // No fingerprint is recorded for the entry, so ownership is unverifiable.
        let (r, mut obs) = observed();
        put(&mut obs, ".ssh", dir(true), None);
        let other = Node::Link {
            target: "/src/other".into(),
            to: dfm_model::plan::LinkTo::File,
            own: true,
        };
        put(&mut obs, ".ssh/config", other, None);
        let p = plan_with(
            &r,
            &obs,
            vec![owned("core.ssh", ".ssh/config", None)],
            BTreeMap::new(),
        );
        assert_eq!(first(&p, "core.ssh"), Code::Conflict);
        assert!(
            p.blockers
                .iter()
                .any(|b| b.detail.ends_with("has an owner the manager cannot verify"))
        );
        assert_eq!(
            action_of(&p, "core.ssh@.ssh/config"),
            (Action::None, Status::Blocked)
        );

        let (r, mut obs) = observed();
        with_git_defaults_parents(&mut obs);
        put(&mut obs, GD, file(true, 0o600), None);
        let p = plan_with(
            &r,
            &obs,
            vec![owned("core.git-defaults", GD, None)],
            BTreeMap::new(),
        );
        assert_eq!(first(&p, "core.git-defaults"), Code::Conflict);
        assert!(
            p.blockers
                .iter()
                .any(|b| b.detail.contains("its ownership cannot be verified"))
        );
        assert!(p.operations.iter().all(|o| o.action != Action::UpdateFile));
    }

    #[test]
    fn two_ledger_entries_on_one_path_never_make_a_removal() {
        // The ledger check refuses this shape, but the planner takes its ledger from the caller.
        // The entry the planner looks up by path belongs to another resource, so the removal is
        // refused as not owned and nothing is removed.
        let (r, mut obs) = observed();
        put(&mut obs, "old", dir(true), None);
        put(&mut obs, "old/out", file(true, 0o600), None);
        let ledger = vec![
            owned("gone.a", "old/out", Some("1".repeat(64))),
            owned("gone.b", "old/out", Some("1".repeat(64))),
        ];
        let p = plan_with(&r, &obs, ledger, BTreeMap::new());
        let b = p
            .blockers
            .iter()
            .find(|b| b.resource == "gone.b")
            .expect("a blocker for the second entry");
        assert_eq!(b.code, Code::Conflict);
        assert_eq!(
            b.detail,
            "old/out is no longer selected but is not owned by this resource according to the ledger"
        );
        assert_eq!(
            action_of(&p, "remove@gone.b@old/out"),
            (Action::None, Status::Blocked)
        );
    }

    #[test]
    fn a_fragment_directory_that_is_unsafe_or_unreadable_blocks_only_its_consumer() {
        const DIR: &str = ".codex/AGENTS.d";
        let cases: [(&str, Node, Option<&str>, Code, &str); 4] = [
            (
                "an alias",
                dir(true),
                Some("agents.d"),
                Code::TargetCollision,
                ".codex/AGENTS.d differs from the existing entry `agents.d` only by case; the filesystem treats them as one",
            ),
            (
                "a link",
                Node::Link {
                    target: "/elsewhere".into(),
                    to: dfm_model::plan::LinkTo::Dir,
                    own: true,
                },
                None,
                Code::ParentUnsafe,
                "parent .codex/AGENTS.d is a link to a directory, so a write would land outside the claim (-> /elsewhere)",
            ),
            (
                "another user's directory",
                dir(false),
                None,
                Code::PrivilegeUnsupported,
                ".codex/AGENTS.d is owned by another user; a HOME adapter never escalates",
            ),
            (
                "an unknown fact",
                Node::Unknown {
                    reason: "EIO".into(),
                },
                None,
                Code::ObservationUnknown,
                "",
            ),
        ];
        for (what, node, alias, want, detail) in cases {
            let (r, mut obs) = observed();
            put(&mut obs, ".codex", dir(true), None);
            put(&mut obs, DIR, node, alias);
            let p = plan(&r, &obs);
            let (code, got) = blocker_at(&p, "agents.codex-instructions", "/fragments_dir");
            assert_eq!(code, want, "{what}: {got}");
            if !detail.is_empty() {
                assert_eq!(got, detail, "{what}");
            }
            // Other resources stay plannable.
            assert!(
                p.blockers
                    .iter()
                    .all(|b| b.resource == "agents.codex-instructions"),
                "{what}"
            );
        }
    }

    #[test]
    fn a_foreign_fragment_owned_by_another_user_blocks_its_consumer() {
        let (r, mut obs) = observed();
        put(&mut obs, ".codex", dir(true), None);
        put(&mut obs, ".codex/AGENTS.d", dir(true), None);
        put(
            &mut obs,
            ".codex/AGENTS.d/10-a.md",
            file(false, 0o644),
            None,
        );
        obs.listings.insert(
            ".codex/AGENTS.d".into(),
            dfm_model::plan::Listing {
                names: vec!["10-a.md".into()],
                incomplete: None,
            },
        );
        let p = plan(&r, &obs);
        let (code, detail) = blocker_at(&p, "agents.codex-instructions", "/fragments_dir");
        assert_eq!(code, Code::PrivilegeUnsupported);
        assert_eq!(
            detail,
            "fragment .codex/AGENTS.d/10-a.md is owned by another user"
        );
    }

    #[test]
    fn a_merge_target_that_is_not_a_plain_user_file_is_refused() {
        use dfm_model::plan::{MergeFact, MergeVerdict};
        const T: &str = ".claude/settings.json";
        let link = Node::Link {
            target: "/elsewhere".into(),
            to: dfm_model::plan::LinkTo::File,
            own: true,
        };
        let fact = |verdict| MergeFact {
            managed_sha256: "0".repeat(64),
            sets: vec![],
            handlers: 0,
            fragments: vec![],
            verdict,
        };
        let cases: Vec<(&str, Node, Option<MergeFact>, Code, &str)> = vec![
            (
                "a directory",
                dir(true),
                None,
                Code::Conflict,
                ".claude/settings.json is a directory where a settings file is needed",
            ),
            (
                "a link",
                link,
                None,
                Code::Conflict,
                ".claude/settings.json is a link or special file; the manager merges only a regular file",
            ),
            (
                "another user's file",
                file(false, 0o600),
                None,
                Code::PrivilegeUnsupported,
                ".claude/settings.json is owned by another user; a HOME adapter never escalates",
            ),
            (
                "an unknown fact",
                Node::Unknown {
                    reason: "EIO".into(),
                },
                None,
                Code::ObservationUnknown,
                "",
            ),
            (
                "a file with no merge fact",
                file(true, 0o600),
                None,
                Code::Conflict,
                ".claude/settings.json exists and its merge state was not read",
            ),
            (
                "a blocked verdict",
                file(true, 0o600),
                Some(fact(MergeVerdict::Blocked {
                    code: Code::Conflict,
                    detail: "the file is not a JSON object".into(),
                    correction: "fix it".into(),
                })),
                Code::Conflict,
                "the file is not a JSON object",
            ),
        ];
        for (what, node, fact, want, detail) in cases {
            let (r, mut obs) = observed();
            put(&mut obs, ".claude", dir(true), None);
            put(&mut obs, T, node, None);
            let merge: BTreeMap<_, _> = fact
                .into_iter()
                .map(|f| ("agents.claude-settings".to_string(), f))
                .collect();
            let p = plan_full(&r, &obs, vec![], BTreeMap::new(), merge);
            let (code, got) = blocker_at(&p, "agents.claude-settings", "target");
            assert_eq!(code, want, "{what}: {got}");
            if !detail.is_empty() {
                assert_eq!(got, detail, "{what}");
            }
            assert_eq!(
                action_of(&p, &format!("agents.claude-settings@{T}")),
                (Action::None, Status::Blocked),
                "{what}"
            );
        }
    }

    #[test]
    fn a_merge_over_a_present_file_owned_by_another_resource_is_blocked_by_the_planner() {
        use dfm_model::plan::{MergeFact, MergeVerdict};
        const T: &str = ".claude/settings.json";
        let (r, mut obs) = observed();
        put(&mut obs, ".claude", dir(true), None);
        put(&mut obs, T, file(true, 0o600), None);
        let merge = BTreeMap::from([(
            "agents.claude-settings".to_string(),
            MergeFact {
                managed_sha256: "0".repeat(64),
                sets: vec![],
                handlers: 0,
                fragments: vec![],
                verdict: MergeVerdict::Write,
            },
        )]);
        let ledger = vec![owned("gone.res", T, None)];
        let p = plan_full(&r, &obs, ledger, BTreeMap::new(), merge);
        let (code, got) = blocker_at(&p, "agents.claude-settings", "target");
        assert_eq!(code, Code::Conflict);
        assert_eq!(
            got,
            ".claude/settings.json is recorded in the ledger as an output of gone.res"
        );
        assert_eq!(
            action_of(&p, &format!("agents.claude-settings@{T}")),
            (Action::None, Status::Blocked)
        );
    }

    #[test]
    fn an_incomplete_listing_says_why() {
        use dfm_model::plan::{Incomplete, Listing};
        for (why, text) in [
            (
                Incomplete::TooMany,
                ".codex/AGENTS.d has too many entries to list",
            ),
            (Incomplete::Failed, ".codex/AGENTS.d could not be listed"),
            (
                Incomplete::NonUtf8,
                ".codex/AGENTS.d holds an entry whose name is not UTF-8",
            ),
        ] {
            let (r, mut obs) = observed();
            obs.listings.insert(
                ".codex/AGENTS.d".into(),
                Listing {
                    names: vec![],
                    incomplete: Some(why),
                },
            );
            let p = plan(&r, &obs);
            let b = p
                .blockers
                .iter()
                .find(|b| b.resource == "agents.codex-instructions")
                .unwrap();
            assert_eq!(
                (b.code, b.detail.as_str()),
                (Code::ObservationUnknown, text)
            );
        }
    }

    #[test]
    fn stale_names_binary_and_model_changes_once_per_subject() {
        use dfm_model::plan::{StaleKind, stale};
        let (r, obs) = observed();
        let old = plan(&r, &obs);
        let mut new = old.clone();
        new.binary.version = "9".into();
        new.binary.sha256 = "1".repeat(64);
        new.model_digest = "2".repeat(64);
        let kinds: Vec<_> = stale(&old, &new)
            .unwrap()
            .into_iter()
            .map(|s| (s.what, s.subject))
            .collect();
        assert_eq!(
            kinds,
            [
                (StaleKind::Model, "resolved model".to_string()),
                (StaleKind::Binary, "sha256".to_string()),
                (StaleKind::Binary, "version".to_string()),
            ]
        );
        // One observation changes a path that the observation map and several preconditions name.
        let mut obs2 = obs.clone();
        put(
            &mut obs2,
            ".ssh",
            Node::Dir {
                mode: 0o700,
                own: true,
            },
            None,
        );
        let new = plan(&r, &obs2);
        let ssh: Vec<_> = stale(&old, &new)
            .unwrap()
            .into_iter()
            .filter(|s| s.subject == "path:.ssh")
            .collect();
        assert_eq!(ssh.len(), 1, "{ssh:?}");
        assert!(stale(&old, &old).unwrap().is_empty());
    }

    #[test]
    fn the_observation_diff_hides_the_size_of_an_unhashed_fragment_of_a_sensitive_merge() {
        use dfm_model::plan::{Fragment, stale};
        let (r, obs) = observed();
        let mut old = plan(&r, &obs);
        let op = &mut old.operations[0];
        op.class = dfm_model::state::Class::Sensitive;
        op.fragment_dir = Some(".claude/frag".into());
        op.fragments = vec![Fragment {
            name: "x.json".into(),
            origin: "foreign".into(),
            sha256: None,
        }];
        let sized = |size| Node::File {
            mode: 0o600,
            own: true,
            size,
            sha256: None,
        };
        let mut new = old.clone();
        put(
            &mut old.observations,
            ".claude/frag/x.json",
            sized(120),
            None,
        );
        put(
            &mut new.observations,
            ".claude/frag/x.json",
            sized(131),
            None,
        );
        // Only the hidden size changed: no observation line prints it, and the digest entry
        // still reports that the plan differs.
        let diff = stale(&old, &new).unwrap();
        assert!(
            diff.iter()
                .all(|s| !s.was.contains("size:") && !s.now.contains("size:")),
            "{diff:?}"
        );
        assert!(diff.iter().any(|s| s.subject == "digest"), "{diff:?}");
    }

    #[test]
    fn a_case_alias_and_an_unknown_fact_fail_closed() {
        let (r, mut obs) = observed();
        put(
            &mut obs,
            ".config",
            Node::Dir {
                mode: 0o755,
                own: true,
            },
            Some("Config"),
        );
        let p = plan(&r, &obs);
        assert_eq!(first(&p, "core.git"), Code::TargetCollision);
        assert!(p.blockers.iter().any(|b| b.detail.contains("`Config`")));

        let (r, mut obs) = observed();
        put(
            &mut obs,
            ".ssh",
            Node::Unknown {
                reason: "EIO".into(),
            },
            None,
        );
        assert_eq!(first(&plan(&r, &obs), "core.ssh"), Code::ObservationUnknown);

        let (r, mut obs) = observed();
        obs.seen.remove(".ssh");
        assert_eq!(first(&plan(&r, &obs), "core.ssh"), Code::ObservationUnknown);
    }
}

// ---- fail-closed observation, reserved paths, names the planner cannot compare ------------

fn blocked(e: &Env) -> Saved {
    let (o, s) = e.saved();
    assert_eq!(code(&o), 3, "{}{}", text(&o.stdout), text(&o.stderr));
    s
}

fn first(s: &Saved) -> (&str, &str, &str) {
    let b = &s.plan.blockers[0];
    (b.code.as_str(), b.resource.as_str(), b.detail.as_str())
}

/// Restores a mode when dropped, so a failed assertion cannot leave an undeletable tree.
struct Restore(PathBuf, u32);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
    }
}

/// Permission tests mean nothing for root, which ignores modes.
fn is_root() -> bool {
    dfm_platform_macos::current_uid() == Some(0)
}

fn unreadable(e: &Env, rel: &str, mode: u32, f: impl FnOnce(&Env)) {
    let _restore = Restore(e.h(rel), mode);
    fs::set_permissions(e.h(rel), fs::Permissions::from_mode(0o000)).unwrap();
    f(e);
}

#[test]
fn an_unreadable_file_or_directory_is_an_unknown_fact_not_a_crash() {
    if is_root() {
        eprintln!("SKIPPED: running as root");
        return;
    }
    // A claimed leaf that cannot be read.
    let e = Env::personal();
    e.mkdirs(".ssh");
    fs::write(e.h(".ssh/config"), "Host *\n").unwrap();
    unreadable(&e, ".ssh/config", 0o600, |e| {
        let s = blocked(e);
        assert_eq!(
            first(&s),
            (
                "E_OBSERVATION_UNKNOWN",
                "core.ssh",
                ".ssh/config could not be observed: file cannot be read; check its permissions"
            )
        );
    });

    // A parent directory that cannot be opened.
    let e = Env::personal();
    e.mkdirs(".ssh");
    unreadable(&e, ".ssh", 0o700, |e| {
        let s = blocked(e);
        assert_eq!(
            first(&s),
            (
                "E_OBSERVATION_UNKNOWN",
                "core.ssh",
                ".ssh could not be observed: directory cannot be opened; check its permissions"
            )
        );
    });

    // A fragment that cannot be read blocks the resource that would assemble it.
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    fs::write(e.h(".codex/AGENTS.d/10-a.md"), "a").unwrap();
    unreadable(&e, ".codex/AGENTS.d/10-a.md", 0o644, |e| {
        let s = blocked(e);
        assert_eq!(
            first(&s),
            (
                "E_OBSERVATION_UNKNOWN",
                "agents.codex-instructions",
                "fragment .codex/AGENTS.d/10-a.md could not be read or is too large to hash"
            )
        );
        assert_eq!(
            op(&s, "agents.codex-instructions@.codex/AGENTS.md").status,
            Status::Blocked
        );
    });

    // A fragment directory that cannot be listed.
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    unreadable(&e, ".codex/AGENTS.d", 0o755, |e| {
        let s = blocked(e);
        assert_eq!(
            first(&s),
            (
                "E_OBSERVATION_UNKNOWN",
                "agents.codex-instructions",
                ".codex/AGENTS.d could not be listed"
            )
        );
    });
}

#[test]
fn a_foreign_fragment_that_is_not_a_plain_readable_file_blocks_the_consumer() {
    let consumer = "agents.codex-instructions";
    let dir = ".codex/AGENTS.d";
    type Case = (&'static str, fn(&Env), &'static str, &'static str);
    let cases: [Case; 4] = [
        (
            "a link",
            |e| symlink("/etc/hosts", e.h(".codex/AGENTS.d/10-a.md")).unwrap(),
            "E_CONFLICT",
            "fragment .codex/AGENTS.d/10-a.md is a link; its content is not what the plan observed",
        ),
        (
            "a fifo",
            |e| {
                let st = Command::new("mkfifo")
                    .arg(e.h(".codex/AGENTS.d/10-a.md"))
                    .status()
                    .unwrap();
                assert!(st.success());
            },
            "E_CONFLICT",
            "fragment .codex/AGENTS.d/10-a.md is not a regular file",
        ),
        (
            "a file over the hash bound",
            |e| {
                let f = fs::File::create(e.h(".codex/AGENTS.d/10-a.md")).unwrap();
                f.set_len((16 << 20) + 1).unwrap();
            },
            "E_OBSERVATION_UNKNOWN",
            "fragment .codex/AGENTS.d/10-a.md could not be read or is too large to hash",
        ),
        (
            "a directory",
            |e| e.mkdirs(".codex/AGENTS.d/10-a.md"),
            "E_CONFLICT",
            "fragment .codex/AGENTS.d/10-a.md is not a regular file",
        ),
    ];
    for (name, make, code_name, detail) in cases {
        let e = Env::personal();
        e.mkdirs(dir);
        make(&e);
        let s = blocked(&e);
        assert_eq!(first(&s), (code_name, consumer, detail), "{name}");
        let op = op(&s, "agents.codex-instructions@.codex/AGENTS.md");
        assert_eq!(op.status, Status::Blocked, "{name}");
    }
}

#[test]
fn a_sensitive_output_under_a_fragment_pattern_is_never_hashed() {
    // An assembly reads `.claude/*.json`. The merge target `.claude/settings.json` matches the
    // pattern, so the merge is a producer of one fragment. A merged file has no content hash by
    // design, so the assembly blocks, and the live token and its hash appear nowhere.
    let e = Env::personal();
    let m = e.src("base/modules/agents/module.toml");
    let mut t = fs::read_to_string(&m).unwrap();
    // The example's skills live under `.claude/skills`, which a `.claude` fragment directory
    // would collide with. They are not part of this case.
    t.truncate(t.find("[[resources]]\nid = \"skills\"").unwrap());
    t.push_str(
        r#"
[[resources]]
id = "claude-index"
kind = "assemble"
adapter = "text-fragments/v1"
target = ".codex/INDEX.md"
[resources.value]
body = "files/AGENTS.md"
fragments_dir = ".claude"
pattern = "*.json"
separator = "\n"
"#,
    );
    fs::write(&m, t).unwrap();
    e.mkdirs(".claude");
    let content = "{\"token\":\"sk-FRAGMENT-SECRET\"}";
    let live = e.h(".claude/settings.json");
    fs::write(&live, content).unwrap();
    fs::set_permissions(&live, fs::Permissions::from_mode(0o600)).unwrap();
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    let o = e.plan(&["--json"]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
    assert!(
        !all.contains(&digest) && !all.contains("sk-FRAGMENT-SECRET"),
        "{all}"
    );
    let s =
        Saved::parse(&o.stdout).unwrap_or_else(|er| panic!("{er}\nstderr: {}", text(&o.stderr)));
    let asm = op(&s, "agents.claude-index@.codex/INDEX.md");
    assert_eq!(asm.status, Status::Blocked);
    assert!(
        s.plan
            .blockers
            .iter()
            .any(|b| b.resource == "agents.claude-index" && b.detail.contains("has no content")),
        "{:?}",
        s.plan.blockers
    );
    let seen = &s.plan.observations.seen[".claude/settings.json"];
    let dfm_model::plan::Node::File { sha256, .. } = &seen.node else {
        panic!("{:?}", seen.node);
    };
    assert_eq!(sha256, &None, "a sensitive path is never hashed");
}

fn retarget(e: &Env, from: &str, to: &str) {
    let m = e.src("base/modules/core/module.toml");
    let t = fs::read_to_string(&m).unwrap();
    assert!(t.contains(from), "{from}");
    fs::write(&m, t.replacen(from, to, 1)).unwrap();
}

#[test]
fn claims_on_the_manager_s_own_paths_are_reserved() {
    let cases = [
        ".local/state/dotfiles-manager/state/ledger.json",
        ".local/state/dotfiles-manager/x",
        ".local/state/dotfiles-manager.lock",
        ".config/dotfiles-manager/machine.toml",
        ".local/share/dotfiles-manager/toolchains/x",
        ".cache/dotfiles-manager/build",
        // An ancestor of a reserved path.
        ".local/state",
        // Case and Unicode aliases of a reserved path.
        ".Local/State/Dotfiles-Manager/x",
        ".local/\u{17f}tate/dotfiles-manager/x",
    ];
    for target in cases {
        let e = Env::personal();
        retarget(
            &e,
            "target = \".ssh/config\"",
            &format!("target = \"{target}\""),
        );
        let s = blocked(&e);
        let b = &s.plan.blockers[0];
        assert_eq!(b.code.as_str(), "E_TARGET_RESERVED", "{target}: {b:?}");
        assert_eq!(
            (b.resource.as_str(), b.path.as_deref()),
            ("core.ssh", Some(target))
        );
        assert!(
            b.detail
                .starts_with(&format!("{target} overlaps a path the manager owns: "))
        );
    }
}

#[test]
fn a_symlink_into_the_manager_s_state_is_reserved_by_identity() {
    let e = Env::personal();
    // `.alias` is a real directory name here, so the text check passes and identity decides.
    e.mkdirs(".local/state");
    retarget(
        &e,
        "target = \".ssh/config\"",
        "target = \"viaparent/state/dotfiles-manager/state/ledger.json\"",
    );
    symlink(e.h(".local"), e.h("viaparent")).unwrap();
    let s = blocked(&e);
    assert_eq!(s.plan.blockers[0].code.as_str(), "E_TARGET_RESERVED");
}

#[test]
fn non_ascii_names_are_refused_because_normalization_is_not_supported() {
    for name in ["caf\u{e9}", "cafe\u{301}"] {
        let e = Env::personal();
        let target = format!(".config/{name}/x");
        retarget(
            &e,
            "target = \".ssh/config\"",
            &format!("target = \"{target}\""),
        );
        let s = blocked(&e);
        assert_eq!(
            first(&s),
            (
                "E_TARGET_INVALID",
                "core.ssh",
                &format!(
                    "{target} contains non-ASCII characters; Unicode normalization is not supported yet, so two spellings could be one entry"
                )[..]
            )
        );
    }
    // A foreign fragment with a non-ASCII name.
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    fs::write(e.h(".codex/AGENTS.d/10-caf\u{e9}.md"), "a").unwrap();
    let s = blocked(&e);
    let (c, r, d) = first(&s);
    assert_eq!((c, r), ("E_TARGET_INVALID", "agents.codex-instructions"));
    assert!(
        d.contains("non-ASCII") && d.contains("not supported yet"),
        "{d}"
    );
}

#[test]
fn a_leaf_alias_is_a_collision_even_when_the_ledger_owns_the_entry() {
    let e = Env::personal();
    if !case_insensitive(e.home.path()) {
        eprintln!("SKIPPED: this volume is case sensitive; the synthetic test covers the logic");
        return;
    }
    e.mkdirs(".ssh");
    fs::write(e.h(".ssh/CONFIG"), "Host *\n").unwrap();
    expect_blocked(
        &e,
        "E_TARGET_COLLISION",
        "core.ssh",
        ".ssh/config",
        ".ssh/config differs from the existing entry `CONFIG` only by case; the filesystem treats them as one",
    );
}

#[test]
fn two_claims_whose_missing_parents_differ_only_by_case_are_a_collision() {
    // `.aa` and `.AA` are missing, so no listing can show the alias. On a case-insensitive
    // volume the two mkdir operations would name one directory.
    let e = Env::personal();
    retarget(
        &e,
        "target = \".ssh/config\"",
        "target = \".AA/ssh-config\"",
    );
    retarget(&e, ".config/git/config", ".aa/git-config");
    let before = fingerprint(e.home.path());
    let s = expect_blocked(
        &e,
        "E_TARGET_COLLISION",
        "core.ssh",
        ".AA/ssh-config",
        "the missing parent .AA differs from the directory `.aa` another claim creates only by case; the filesystem treats them as one",
    );
    assert!(!s.plan.operations.iter().any(|o| o.id == "mkdir@.AA"));
    assert_eq!(fingerprint(e.home.path()), before);
}

#[test]
fn an_unsafe_merge_target_or_fragment_directory_is_refused_and_changes_nothing() {
    // A directory and a link where the merge file goes.
    let e = Env::personal();
    e.mkdirs(".claude/settings.json");
    let before = fingerprint(e.home.path());
    expect_blocked(
        &e,
        "E_CONFLICT",
        "agents.claude-settings",
        ".claude/settings.json",
        ".claude/settings.json is a directory where a settings file is needed",
    );
    assert_eq!(fingerprint(e.home.path()), before);

    let e = Env::personal();
    e.mkdirs(".claude");
    symlink("/somewhere/else", e.h(".claude/settings.json")).unwrap();
    let before = fingerprint(e.home.path());
    expect_blocked(
        &e,
        "E_CONFLICT",
        "agents.claude-settings",
        ".claude/settings.json",
        ".claude/settings.json is a link or special file; the manager merges only a regular file",
    );
    assert_eq!(fingerprint(e.home.path()), before);

    // A merge file that is not JSON: the caller's verdict is Blocked.
    let e = Env::personal();
    e.mkdirs(".claude");
    fs::write(e.h(".claude/settings.json"), "not json").unwrap();
    fs::set_permissions(
        e.h(".claude/settings.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let before = fingerprint(e.home.path());
    let s = blocked(&e);
    let b = s
        .plan
        .blockers
        .iter()
        .find(|b| b.resource == "agents.claude-settings")
        .expect("the merge is blocked");
    assert_eq!(
        (b.code.as_str(), b.path.as_deref()),
        ("E_CONFLICT", Some(".claude/settings.json"))
    );
    assert!(
        b.detail
            .starts_with("the live settings file is not valid JSON ("),
        "{}",
        b.detail
    );
    // The detail names the parse position and never echoes the content.
    assert!(!b.detail.contains("not json"), "{}", b.detail);
    assert!(
        s.plan
            .operations
            .iter()
            .all(|o| !(o.resource == "agents.claude-settings" && o.action == Action::UpdateFile))
    );
    assert_eq!(fingerprint(e.home.path()), before);

    // A fragment directory that is a link.
    let e = Env::personal();
    e.mkdirs(".codex");
    let outside = TempDir::new("plan-fragments");
    symlink(outside.path(), e.h(".codex/AGENTS.d")).unwrap();
    let before = fingerprint(e.home.path());
    let s = blocked(&e);
    let b = s
        .plan
        .blockers
        .iter()
        .find(|b| b.field.as_deref() == Some("/fragments_dir"))
        .expect("the fragment directory is refused");
    assert_eq!(b.code.as_str(), "E_PARENT_UNSAFE");
    assert_eq!(fingerprint(e.home.path()), before);
}

// ---- the saved plan: JSON stays one document, the comparison is complete -----------------

#[test]
fn json_against_a_stale_plan_prints_one_json_document() {
    let e = Env::personal();
    let (f, _) = save(&e);
    fs::write(
        e.src("base/modules/core/files/gitconfig"),
        "[user]\n name = x\n",
    )
    .unwrap();
    let o = e.plan(&["--json", "--against", f.to_str().unwrap()]);
    assert_eq!(code(&o), 3);
    let s = Saved::parse(&o.stdout).unwrap_or_else(|er| panic!("{er}\n{}", text(&o.stdout)));
    assert_eq!(s.stale[0].what, dfm_model::plan::StaleKind::SourceInput);
    assert!(text(&o.stderr).starts_with("error: E_PLAN_STALE source input changed: "));
    // An unchanged world: exit 0, no stale entries, still one document.
    let e = Env::personal();
    let (f, _) = save(&e);
    let o = e.plan(&["--json", "--against", f.to_str().unwrap()]);
    assert_eq!(code(&o), 0);
    assert!(Saved::parse(&o.stdout).unwrap().stale.is_empty());
}

fn rewrite(f: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(f).unwrap()).unwrap();
    edit(&mut v);
    // Recompute the digest the way a forger would, so the file is self-consistent.
    let plan: dfm_model::plan::Plan = serde_json::from_value(v["plan"].clone()).unwrap();
    v["plan_digest"] = plan.digest().unwrap().into();
    fs::write(f, serde_json::to_vec(&v).unwrap()).unwrap();
}

#[test]
fn a_self_consistent_forged_plan_is_still_stale() {
    for (what, edit) in [
        (
            "effect",
            (|v: &mut serde_json::Value| v["plan"]["operations"][0]["effect"] = "other".into())
                as fn(&mut serde_json::Value),
        ),
        ("scope", |v| v["plan"]["scope"] = "other".into()),
        ("path", |v| {
            v["plan"]["operations"][1]["path"] = ".elsewhere".into()
        }),
    ] {
        let e = Env::personal();
        let (f, _) = save(&e);
        rewrite(&f, edit);
        let o = against(&e, &f);
        assert_eq!(code(&o), 3, "{what}");
        let out = text(&o.stdout);
        assert!(out.contains("stale plan: digest was "), "{what}: {out}");
        assert!(
            text(&o.stderr).starts_with("error: E_PLAN_STALE plan changed: digest was "),
            "{what}"
        );
    }
}

#[test]
fn the_stale_list_says_how_many_lines_it_omits() {
    let e = Env::personal();
    let (f, _) = save(&e);
    rewrite(&f, |v| {
        for i in 0..30 {
            v["plan"]["observations"]["seen"][format!("extra/{i:02}")] =
                serde_json::json!({"node": {"kind": "missing"}, "alias": null});
        }
    });
    let o = against(&e, &f);
    assert_eq!(code(&o), 3);
    let out = text(&o.stdout);
    assert_eq!(
        out.lines()
            .filter(|l| l.starts_with("stale observation: "))
            .count(),
        20
    );
    assert!(
        out.contains("stale: 10 more differences not shown (30 in total)"),
        "{out}"
    );
}

// ---- for apply (DFM-18): ids, link targets, exported fingerprints, removals ----------------

#[test]
fn operations_carry_what_apply_needs_without_parsing_prose() {
    let e = Env::personal();
    let (_, s) = e.saved();
    let mut ids = std::collections::BTreeSet::new();
    for o in &s.plan.operations {
        assert!(
            dfm_model::state::valid_id(&o.journal_id),
            "{}",
            o.journal_id
        );
        assert!(ids.insert(o.journal_id.clone()), "duplicate journal id");
    }
    let git = op(&s, "core.git@.config/git/config");
    assert!(
        git.link
            .as_deref()
            .unwrap()
            .ends_with("modules/core/files/gitconfig")
    );
    assert!(
        git.link
            .as_deref()
            .unwrap()
            .starts_with(e.root.to_str().unwrap())
    );
    assert_eq!(
        op(
            &s,
            "core.git-defaults@.config/external-overlays/git/base.inc"
        )
        .link,
        None
    );
    assert_eq!(
        op(&s, "personal-dev.example-secret@scope").mode,
        Some(0o600)
    );
    // The journal id does not change with the observations, and does change with the model.
    e.mkdirs(".ssh");
    let (_, again) = e.saved();
    assert_eq!(
        op(&again, "core.git@.config/git/config").journal_id,
        git.journal_id
    );
    fs::write(e.src("base/modules/core/files/gitconfig"), "x").unwrap();
    let (_, changed) = e.saved();
    assert_ne!(
        op(&changed, "core.git@.config/git/config").journal_id,
        git.journal_id
    );
    // Preconditions use the exported builder, so apply can recompute them.
    let ssh = op(&again, "core.ssh@.ssh/config");
    let seen = &again.plan.observations.seen[".ssh"];
    assert!(
        ssh.preconditions.iter().any(
            |c| c.subject == "path:.ssh" && c.state == dfm_model::plan::node_state(seen, false)
        )
    );
}

#[test]
fn an_owned_output_that_nothing_selects_any_more_is_planned_for_removal() {
    let e = Env::personal();
    let gone = e.h("gone.txt");
    record(&e, "gone.res", &gone, "managed\n");
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let r = op(&s, "remove@gone.res@gone.txt");
    assert_eq!(
        (r.action, r.status, r.ownership),
        (Action::Remove, Status::Planned, Ownership::Owned)
    );
    assert!(
        r.preconditions
            .iter()
            .any(|c| c.subject == "path:gone.txt" && c.state.contains("sha256:"))
    );
    assert!(
        r.preconditions
            .iter()
            .any(|c| c.subject == "ledger:gone.txt")
    );
    assert_eq!(fs::read_to_string(&gone).unwrap(), "managed\n");

    // Modified by the user: a conflict, never a removal.
    fs::write(&gone, "edited\n").unwrap();
    let s = blocked(&e);
    assert_eq!(
        first(&s),
        (
            "E_CONFLICT",
            "gone.res",
            "gone.txt is no longer selected but was changed after the manager wrote it"
        )
    );
    assert_eq!(s.plan.blockers[0].source, "state/ledger.json");
    assert_eq!(op(&s, "remove@gone.res@gone.txt").action, Action::None);

    // Already gone: nothing to remove.
    fs::remove_file(&gone).unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0);
    let r = op(&s, "remove@gone.res@gone.txt");
    // The action is Remove so apply can drop the stale ledger entry (docs/apply.md).
    assert_eq!((r.action, r.status), (Action::Remove, Status::Current));

    // A directory is never removed.
    fs::create_dir(&gone).unwrap();
    assert_eq!(first(&blocked(&e)).0, "E_CONFLICT");
}

mod removal {
    use super::*;
    use dfm_cli::builtin_catalog;
    use dfm_model::plan::{
        Binary, Context, LinkTo, Node, Observations, Owned, Seen, Source, build, link_fingerprint,
        requests,
    };
    use dfm_model::resolve::{SourceSet, resolve};
    use dfm_model::state::Class;
    use std::collections::BTreeMap;

    fn go(ledger: Vec<Owned>, node: Node) -> dfm_model::plan::Plan {
        let mut set = SourceSet::new();
        fn load(dir: &Path, rel: &str, out: &mut SourceSet) {
            for e in fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().into_string().unwrap();
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                if e.file_type().unwrap().is_dir() {
                    load(&e.path(), &path, out);
                } else {
                    out.insert_file(path, fs::read(e.path()).unwrap());
                }
            }
        }
        load(&repo_root().join("examples"), "", &mut set);
        let resolved = resolve(&set, "profiles/personal-dev-01.toml", &builtin_catalog()).unwrap();
        let mut obs = Observations::default();
        let missing = Seen {
            node: Node::Missing {},
            alias: None,
        };
        for r in requests(&resolved, &ledger, dfm_model::plan::Scope::Config) {
            for i in 0..r.path.split('/').count() {
                let p = r.path.split('/').take(i + 1).collect::<Vec<_>>().join("/");
                obs.seen.entry(p).or_insert_with(|| missing.clone());
            }
        }
        obs.seen.insert(
            "old".into(),
            Seen {
                node: Node::Dir {
                    mode: 0o755,
                    own: true,
                },
                alias: None,
            },
        );
        obs.seen
            .insert("old/link".into(), Seen { node, alias: None });
        let cx = Context {
            scope: dfm_model::plan::Scope::Config,
            secret: std::collections::BTreeMap::new(),
            package: std::collections::BTreeMap::new(),
            service: std::collections::BTreeMap::new(),
            restart: std::collections::BTreeSet::new(),
            binary: Binary {
                version: "0".into(),
                sha256: "0".repeat(64),
            },
            profile: "p",
            machine_id: "m",
            sources: vec![Source {
                kind: "path".into(),
                root: "/r".into(),
                tree: "sha256:0".into(),
            }],
            root: "/r",
            ledger,
            reserved: BTreeMap::new(),
            unchecked: BTreeMap::new(),
            merge: BTreeMap::new(),
        };
        build(&resolved, &cx, &obs).unwrap()
    }

    fn owned(class: Class, fp: Option<String>) -> Vec<Owned> {
        vec![Owned {
            resource: "gone.res".into(),
            adapter: "fs-link/v1".into(),
            class,
            path: "old/link".into(),
            fingerprint: fp,
            coowned_entries: None,
        }]
    }

    fn link(target: &str) -> Node {
        Node::Link {
            target: target.into(),
            to: LinkTo::File,
            own: true,
        }
    }

    #[test]
    fn a_deselected_owned_link_is_removed_and_a_changed_or_unverifiable_one_is_a_conflict() {
        let fp = Some(link_fingerprint("/src/x"));
        let p = go(owned(Class::Normal, fp.clone()), link("/src/x"));
        let r = p
            .operations
            .iter()
            .find(|o| o.id == "remove@gone.res@old/link")
            .unwrap();
        assert_eq!((r.action, r.status), (Action::Remove, Status::Planned));
        assert_eq!(r.ownership, Ownership::Owned);
        assert!(p.blockers.is_empty(), "{:?}", p.blockers);

        // The link now points elsewhere: the fingerprint no longer matches.
        let p = go(owned(Class::Normal, fp), link("/src/y"));
        assert_eq!(p.blockers[0].code.as_str(), "E_CONFLICT");
        assert_eq!(
            p.blockers[0].detail,
            "old/link is no longer selected but was changed after the manager wrote it"
        );

        // A link entry written before the fingerprint prefix holds the plain hash. Deselected,
        // it is a conflict and never a removal.
        let plain = Some(dfm_model::digest::sha256_hex(b"/src/x"));
        let p = go(owned(Class::Normal, plain), link("/src/x"));
        assert_eq!(p.blockers[0].code.as_str(), "E_CONFLICT");
        assert!(p.operations.iter().all(|o| o.action != Action::Remove));

        // A sensitive entry has no comparable fingerprint.
        let p = go(owned(Class::Sensitive, None), link("/src/x"));
        assert_eq!(
            p.blockers[0].detail,
            "old/link is no longer selected but has an owner the manager cannot verify"
        );

        // The ledger resource is still selected and still claims the path: not a removal.
        let p = go(
            vec![Owned {
                resource: "core.ssh".into(),
                adapter: "fs-link/v1".into(),
                class: Class::Normal,
                path: ".ssh/config".into(),
                fingerprint: Some(link_fingerprint("/x")),
                coowned_entries: None,
            }],
            link("/src/x"),
        );
        assert!(p.operations.iter().all(|o| !o.id.starts_with("remove@")));
    }
}

// ---- final round: hashes, removal order, unchecked claims ---------------------------------

#[test]
fn a_ledger_entry_on_a_sensitive_claim_leaks_no_hash_and_is_not_a_removal() {
    let e = Env::personal();
    e.mkdirs(".claude");
    let out = e.h(".claude/settings.json");
    // Another, deselected, normal resource owns the file in the ledger (its fingerprint is the
    // content hash), and a selected sensitive resource now claims it.
    record(&e, "gone.res", &out, "SECRETTOKEN123");
    let digest = format!("{:x}", Sha256::digest(b"SECRETTOKEN123"));
    for extra in [&["--json"][..], &[][..]] {
        let o = e.plan(extra);
        let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
        assert_eq!(code(&o), 3, "{all}");
        assert!(
            !all.contains(&digest) && !all.contains("SECRETTOKEN123"),
            "{all}"
        );
    }
    let s = e.saved().1;
    match &s.plan.observations.seen[".claude/settings.json"].node {
        dfm_model::plan::Node::File { sha256, .. } => assert_eq!(sha256, &None),
        n => panic!("{n:?}"),
    }
    assert!(
        s.plan
            .operations
            .iter()
            .all(|o| !o.id.starts_with("remove@"))
    );
    let leaf = op(&s, "agents.claude-settings@.claude/settings.json");
    assert!(leaf
        .preconditions
        .iter()
        .any(|c| c.subject == "ledger:.claude/settings.json" && c.state == "gone.res:sensitive"));
}

#[test]
fn a_fragment_being_removed_is_not_an_input_and_its_consumer_runs_after_the_removal() {
    let e = Env::personal();
    e.mkdirs(".codex/AGENTS.d");
    let gone = e.h(".codex/AGENTS.d/30-gone.md");
    record(&e, "gone.res", &gone, "old fragment\n");
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let consumer = op(&s, "agents.codex-instructions@.codex/AGENTS.md");
    let rm = op(&s, "remove@gone.res@.codex/AGENTS.d/30-gone.md");
    assert_eq!((rm.action, rm.status), (Action::Remove, Status::Planned));
    // The consumer waits for the removal, never the other way round, so there is no cycle.
    assert!(consumer.after.contains(&rm.id), "{:?}", consumer.after);
    assert!(rm.after.is_empty(), "{:?}", rm.after);
    // Deselected content is not an input.
    assert!(consumer.fragments.is_empty(), "{:?}", consumer.fragments);
    assert!(
        consumer
            .preconditions
            .iter()
            .any(|c| c.state == "selected=[] foreign=[] removing=[30-gone.md]")
    );
    let pos = |id: &str| s.plan.operations.iter().position(|o| o.id == id).unwrap();
    assert!(pos(&rm.id) < pos(&consumer.id));
    assert!(s.plan.blockers.is_empty());

    // A removal that is blocked (the file was edited) is not a producer and no edge is added:
    // the edited file is an ordinary foreign fragment.
    fs::write(&gone, "edited\n").unwrap();
    let s = e.saved().1;
    let consumer = op(&s, "agents.codex-instructions@.codex/AGENTS.md");
    assert!(consumer.after.iter().all(|a| !a.starts_with("remove@")));
    assert_eq!(consumer.fragments[0].origin, "foreign");
    assert_eq!(
        op(&s, "remove@gone.res@.codex/AGENTS.d/30-gone.md").action,
        Action::None
    );
}

#[test]
fn outputs_of_a_resource_that_is_still_selected_are_never_removed() {
    let e = Env::personal();
    // `personal-dev.atuin` is selected and claims nothing.
    record(&e, "personal-dev.atuin", &e.h("kept.txt"), "x");
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    assert!(
        s.plan
            .operations
            .iter()
            .all(|o| !o.id.starts_with("remove@"))
    );
}

#[test]
fn a_removal_candidate_with_a_non_ascii_path_is_refused() {
    let e = Env::personal();
    let out = e.h("caf\u{e9}.txt");
    record(&e, "gone.res", &out, "x");
    let s = blocked(&e);
    let b = &s.plan.blockers[0];
    assert_eq!(
        (b.code.as_str(), b.resource.as_str()),
        ("E_TARGET_INVALID", "gone.res")
    );
    assert!(
        b.detail
            .starts_with("caf\u{e9}.txt contains non-ASCII characters"),
        "{}",
        b.detail
    );
    assert_eq!(fs::read_to_string(&out).unwrap(), "x");
}

#[test]
fn a_claim_that_cannot_be_checked_against_the_manager_s_paths_is_an_unknown_fact() {
    if is_root() {
        eprintln!("SKIPPED: running as root");
        return;
    }
    let e = Env::personal();
    e.mkdirs(".ssh");
    fs::write(e.h(".ssh/config"), "x").unwrap();
    // Readable but not searchable: the leaf cannot be examined.
    let _restore = Restore(e.h(".ssh"), 0o700);
    fs::set_permissions(e.h(".ssh"), fs::Permissions::from_mode(0o600)).unwrap();
    let s = blocked(&e);
    let b = &s.plan.blockers[0];
    assert_eq!(
        (b.code.as_str(), b.resource.as_str()),
        ("E_OBSERVATION_UNKNOWN", "core.ssh")
    );
    assert!(
        b.detail
            .starts_with(".ssh/config could not be checked against the manager's own paths: "),
        "{}",
        b.detail
    );
}

mod hardening {
    use super::*;
    use dfm_cli::builtin_catalog;
    use dfm_model::plan::{
        Binary, Context, LinkTo, Node, Observations, Owned, Seen, Source, build, node_state, stale,
    };
    use dfm_model::resolve::{SourceSet, resolve};
    use dfm_model::state::Class;
    use std::collections::BTreeMap;

    fn resolved() -> dfm_model::resolve::Resolved {
        let mut set = SourceSet::new();
        fn load(dir: &Path, rel: &str, out: &mut SourceSet) {
            for e in fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().into_string().unwrap();
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                if e.file_type().unwrap().is_dir() {
                    load(&e.path(), &path, out);
                } else {
                    out.insert_file(path, fs::read(e.path()).unwrap());
                }
            }
        }
        load(&repo_root().join("examples"), "", &mut set);
        resolve(&set, "profiles/personal-dev-01.toml", &builtin_catalog()).unwrap()
    }

    fn all_missing(r: &dfm_model::resolve::Resolved, ledger: &[Owned]) -> Observations {
        let mut obs = Observations::default();
        for q in dfm_model::plan::requests(r, ledger, dfm_model::plan::Scope::Config) {
            for i in 0..q.path.split('/').count() {
                let p = q.path.split('/').take(i + 1).collect::<Vec<_>>().join("/");
                obs.seen.entry(p).or_insert(Seen {
                    node: Node::Missing {},
                    alias: None,
                });
            }
        }
        obs
    }

    fn run(
        r: &dfm_model::resolve::Resolved,
        obs: &Observations,
        ledger: Vec<Owned>,
    ) -> dfm_model::plan::Plan {
        let cx = Context {
            scope: dfm_model::plan::Scope::Config,
            secret: std::collections::BTreeMap::new(),
            package: std::collections::BTreeMap::new(),
            service: std::collections::BTreeMap::new(),
            restart: std::collections::BTreeSet::new(),
            binary: Binary {
                version: "0".into(),
                sha256: "0".repeat(64),
            },
            profile: "p",
            machine_id: "m",
            sources: vec![Source {
                kind: "path".into(),
                root: "/r".into(),
                tree: "sha256:0".into(),
            }],
            root: "/r",
            ledger,
            reserved: BTreeMap::new(),
            unchecked: BTreeMap::new(),
            merge: BTreeMap::new(),
        };
        build(r, &cx, obs).unwrap()
    }

    #[test]
    fn a_mode_outside_the_permission_bits_is_refused_not_dropped() {
        for bad in [0o4755_i64, 0o2755, 0o1777, -1, 0o1000] {
            let mut r = resolved();
            let git = r.resources.get_mut("core.git").unwrap();
            git.value
                .as_table_mut()
                .unwrap()
                .insert("mode".into(), toml::Value::Integer(bad));
            let p = run(&r, &all_missing(&r, &[]), vec![]);
            let b = p
                .blockers
                .iter()
                .find(|b| b.resource == "core.git")
                .expect("a blocker");
            assert_eq!(b.code.as_str(), "E_MANIFEST_INVALID", "{bad:o}");
            assert_eq!(b.field.as_deref(), Some("/mode"));
        }
        let mut r = resolved();
        r.resources
            .get_mut("core.git")
            .unwrap()
            .value
            .as_table_mut()
            .unwrap()
            .insert("mode".into(), toml::Value::Integer(0o644));
        let p = run(&r, &all_missing(&r, &[]), vec![]);
        assert_eq!(
            p.operations
                .iter()
                .find(|o| o.id == "core.git@.config/git/config")
                .unwrap()
                .mode,
            Some(0o644)
        );
    }

    #[test]
    fn a_removal_candidate_owned_by_another_user_needs_privilege() {
        let r = resolved();
        let ledger = vec![Owned {
            resource: "gone.res".into(),
            adapter: "fs-link/v1".into(),
            class: Class::Normal,
            path: "old/file".into(),
            fingerprint: Some("0".repeat(64)),
            coowned_entries: None,
        }];
        let mut obs = all_missing(&r, &ledger);
        obs.seen.insert(
            "old".into(),
            Seen {
                node: Node::Dir {
                    mode: 0o755,
                    own: true,
                },
                alias: None,
            },
        );
        obs.seen.insert(
            "old/file".into(),
            Seen {
                node: Node::File {
                    mode: 0o644,
                    own: false,
                    size: 1,
                    sha256: Some("0".repeat(64)),
                },
                alias: None,
            },
        );
        let p = run(&r, &obs, ledger);
        let b = p
            .blockers
            .iter()
            .find(|b| b.resource == "gone.res")
            .unwrap();
        assert_eq!(b.code.as_str(), "E_PRIVILEGE_UNSUPPORTED");
    }

    #[test]
    fn a_case_only_retarget_is_a_different_link_and_makes_the_plan_stale() {
        let link = |t: &str| Seen {
            node: Node::Link {
                target: t.into(),
                to: LinkTo::File,
                own: true,
            },
            alias: None,
        };
        assert_ne!(
            node_state(&link("/a/Config"), false),
            node_state(&link("/a/config"), false)
        );
        assert_eq!(node_state(&link("/a/Config"), false), "link:/a/Config:file");
        let r = resolved();
        let mk = |t: &str| {
            let mut obs = all_missing(&r, &[]);
            obs.seen.insert(
                ".ssh".into(),
                Seen {
                    node: Node::Dir {
                        mode: 0o700,
                        own: true,
                    },
                    alias: None,
                },
            );
            obs.seen.insert(".ssh/config".into(), link(t));
            run(&r, &obs, vec![])
        };
        let (a, b) = (mk("/a/Config"), mk("/a/config"));
        let diffs = stale(&a, &b).unwrap();
        let d = diffs
            .iter()
            .find(|d| d.subject == "path:.ssh/config")
            .expect("stale path");
        assert_eq!(
            (d.was.as_str(), d.now.as_str()),
            ("link:/a/Config:file", "link:/a/config:file")
        );
        assert_eq!(
            diffs
                .iter()
                .filter(|d| d.subject == "path:.ssh/config")
                .count(),
            1
        );
    }

    #[test]
    fn a_node_with_an_unknown_key_is_refused() {
        let ok: Result<Node, _> = serde_json::from_value(serde_json::json!({"kind": "missing"}));
        assert!(ok.is_ok());
        let bad: Result<Node, _> =
            serde_json::from_value(serde_json::json!({"kind": "missing", "extra": 1}));
        assert!(bad.is_err());
        let bad: Result<Node, _> = serde_json::from_value(
            serde_json::json!({"kind": "dir", "mode": 493, "own": true, "extra": 1}),
        );
        assert!(bad.is_err());
    }
}

#[test]
fn a_path_the_ledger_records_as_sensitive_is_never_hashed_whoever_claims_it_now() {
    let e = Env::personal();
    e.mkdirs(".ssh");
    let out = e.h(".ssh/config");
    // The ledger says a sensitive resource wrote this file. A normal resource claims it now.
    let o = e.run(&[
        "__record-op",
        "--resource",
        "gone.res",
        "--output",
        out.to_str().unwrap(),
        "--content",
        "SECRETX-CONTENT",
        "--sensitive",
        "yes",
    ]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let digest = format!("{:x}", Sha256::digest(b"SECRETX-CONTENT"));
    let o = e.plan(&["--json"]);
    let all = text(&o.stdout);
    assert!(
        !all.contains(&digest) && !all.contains("SECRETX-CONTENT"),
        "{all}"
    );
    let s = Saved::parse(&o.stdout).unwrap();
    match &s.plan.observations.seen[".ssh/config"].node {
        dfm_model::plan::Node::File { sha256, .. } => assert_eq!(sha256, &None),
        n => panic!("{n:?}"),
    }
}

mod ledger_sensitivity {
    use super::*;
    use dfm_cli::builtin_catalog;
    use dfm_model::plan::{Node, Observations, Owned, Seen, followups, requests};
    use dfm_model::resolve::{SourceSet, resolve};
    use dfm_model::state::Class;

    fn resolved() -> dfm_model::resolve::Resolved {
        let mut set = SourceSet::new();
        fn load(dir: &Path, rel: &str, out: &mut SourceSet) {
            for e in fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().into_string().unwrap();
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                if e.file_type().unwrap().is_dir() {
                    load(&e.path(), &path, out);
                } else {
                    out.insert_file(path, fs::read(e.path()).unwrap());
                }
            }
        }
        load(&repo_root().join("examples"), "", &mut set);
        resolve(&set, "profiles/personal-dev-01.toml", &builtin_catalog()).unwrap()
    }

    fn entry(path: &str) -> Owned {
        Owned {
            resource: "gone.res".into(),
            adapter: "x/v1".into(),
            class: Class::Sensitive,
            path: path.into(),
            fingerprint: None,
            coowned_entries: None,
        }
    }

    #[test]
    fn requests_never_hash_a_path_the_ledger_records_as_sensitive() {
        let r = resolved();
        let plain = requests(&r, &[], dfm_model::plan::Scope::Config);
        assert!(plain.iter().any(|q| q.path == ".ssh/config" && q.hash));
        let with = requests(&r, &[entry(".ssh/config")], dfm_model::plan::Scope::Config);
        assert!(with.iter().any(|q| q.path == ".ssh/config" && !q.hash));
        // A spelling that differs only by case is the same path.
        let with = requests(&r, &[entry(".SSH/Config")], dfm_model::plan::Scope::Config);
        assert!(with.iter().any(|q| q.path == ".ssh/config" && !q.hash));
    }

    #[test]
    fn followups_skip_a_fragment_the_ledger_records_as_sensitive_in_another_case() {
        let r = resolved();
        let mut obs = Observations::default();
        obs.listings.insert(
            ".codex/AGENTS.d".into(),
            dfm_model::plan::Listing {
                names: vec!["50-s.md".into(), "60-t.md".into()],
                incomplete: None,
            },
        );
        let _ = Seen {
            node: Node::Missing {},
            alias: None,
        };
        let paths = |l: &[Owned]| {
            followups(&r, &obs, l, dfm_model::plan::Scope::Config)
                .into_iter()
                .map(|q| q.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(&[]),
            [".codex/AGENTS.d/50-s.md", ".codex/AGENTS.d/60-t.md"]
        );
        assert_eq!(
            paths(&[entry(".codex/AGENTS.d/50-S.md")]),
            [".codex/AGENTS.d/60-t.md"]
        );
    }

    #[test]
    fn missing_serializes_to_exactly_its_kind() {
        assert_eq!(
            serde_json::to_string(&Node::Missing {}).unwrap(),
            r#"{"kind":"missing"}"#
        );
    }

    #[test]
    fn an_out_of_scope_resource_with_a_bad_mode_still_raises_a_blocker() {
        use dfm_model::plan::{Binary, Context, Source, build};
        let mut r = resolved();
        r.resources
            .get_mut("personal-dev.example-secret")
            .unwrap()
            .value
            .as_table_mut()
            .unwrap()
            .insert("mode".into(), toml::Value::Integer(0o4755));
        let cx = Context {
            scope: dfm_model::plan::Scope::Config,
            secret: std::collections::BTreeMap::new(),
            package: std::collections::BTreeMap::new(),
            service: std::collections::BTreeMap::new(),
            restart: std::collections::BTreeSet::new(),
            binary: Binary {
                version: "0".into(),
                sha256: "0".repeat(64),
            },
            profile: "p",
            machine_id: "m",
            sources: vec![Source {
                kind: "path".into(),
                root: "/r".into(),
                tree: "sha256:0".into(),
            }],
            root: "/r",
            ledger: vec![],
            reserved: Default::default(),
            merge: Default::default(),
            unchecked: Default::default(),
        };
        let mut obs = Observations::default();
        for q in requests(&r, &[], dfm_model::plan::Scope::Config) {
            for i in 0..q.path.split('/').count() {
                let p = q.path.split('/').take(i + 1).collect::<Vec<_>>().join("/");
                obs.seen.entry(p).or_insert(Seen {
                    node: Node::Missing {},
                    alias: None,
                });
            }
        }
        let p = build(&r, &cx, &obs).unwrap();
        let b = p
            .blockers
            .iter()
            .find(|b| b.resource == "personal-dev.example-secret")
            .unwrap();
        assert_eq!(
            (b.code.as_str(), b.field.as_deref()),
            ("E_MANIFEST_INVALID", Some("/mode"))
        );
    }
}

#[test]
fn an_exact_link_that_another_resource_owns_is_not_adopted_and_says_why() {
    let e = Env::personal();
    let link = ssh_link(&e);
    e.mkdirs(".ssh");
    // Another (deselected) resource has a ledger entry for the path. The link is the one
    // `core.ssh` would create, but the ledger already has an entry, so nothing is adopted.
    record(&e, "other.res", &e.h(".ssh/config"), "owned\n");
    fs::remove_file(e.h(".ssh/config")).unwrap();
    symlink(&link, e.h(".ssh/config")).unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}{}", text(&o.stdout), text(&o.stderr));
    let a = op(&s, "core.ssh@.ssh/config");
    assert_eq!((a.action, a.status), (Action::None, Status::Current));
    assert_eq!(a.ownership, Ownership::UnrecordedCurrent);
    assert!(
        a.effect
            .ends_with("already holds; not adopted: the ledger already has an entry for this path"),
        "{}",
        a.effect
    );
}

#[test]
fn claims_under_missing_share_config_and_cache_directories_plan_clean_with_mkdirs() {
    let e = Env::personal();
    for d in [".local/share", ".config/firstmate", ".cache"] {
        assert!(!e.h(d).exists(), "{d}");
    }
    retarget(
        &e,
        "target = \".ssh/config\"",
        "target = \".local/share/firstmate/config\"",
    );
    let m = e.src("base/modules/core/module.toml");
    let t = fs::read_to_string(&m).unwrap();
    fs::write(
        &m,
        t.replacen(".config/git/config", ".cache/firstmate/git", 1),
    )
    .unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}{}", text(&o.stdout), text(&o.stderr));
    assert!(s.plan.blockers.is_empty(), "{:?}", s.plan.blockers);
    for d in [
        ".local/share",
        ".local/share/firstmate",
        ".cache",
        ".cache/firstmate",
    ] {
        assert_eq!(
            op(&s, &format!("mkdir@{d}")).action,
            Action::CreateDir,
            "{d}"
        );
    }
}

#[test]
fn a_link_parent_that_leaves_home_is_a_parent_refusal_not_an_overlap() {
    let e = Env::personal();
    let outside = TempDir::new("plan-outside");
    // `.local/share` exists but is a link out of HOME: it is the first existing prefix.
    e.mkdirs(".local");
    symlink(outside.path(), e.h(".local/share")).unwrap();
    retarget(
        &e,
        "target = \".ssh/config\"",
        "target = \".local/share/firstmate/config\"",
    );
    let (o, s) = e.saved();
    assert_eq!(code(&o), 3, "{}{}", text(&o.stdout), text(&o.stderr));
    let b = &s.plan.blockers[0];
    assert_eq!(b.code.as_str(), "E_PARENT_UNSAFE", "{b:?}");
}

#[test]
fn a_merge_over_a_stale_owner_retires_it_or_blocks_with_a_fix_that_works() {
    let e = Env::personal();
    e.mkdirs(".claude");
    let out = e.h(".claude/settings.json");
    record(&e, "gone.res", &out, "{}");
    // The old owner's file is present: blocked, and the fix text is "move the file aside".
    let o = e.plan(&[]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("move the file aside"),
        "{}",
        text(&o.stderr)
    );
    // Following the text: the next plan retires the stale entry before the merge, and apply
    // writes the file with the new owner only.
    fs::rename(&out, e.h(".claude/settings.old")).unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let pos = |pre: &str| {
        s.plan
            .operations
            .iter()
            .position(|o| o.id.starts_with(pre))
            .unwrap()
    };
    assert!(pos("remove@gone.res@") < pos("agents.claude-settings@"));
    let d = s.plan_digest;
    let o = e.run(&[
        "apply",
        "--root",
        e.root.to_str().unwrap(),
        "--yes-plan",
        &d,
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", text(&o.stdout), text(&o.stderr));
    assert!(out.is_file());
    let ledger =
        fs::read_to_string(e.h(".local/state/dotfiles-manager/state/ledger.json")).unwrap();
    assert!(!ledger.contains("gone.res"), "{ledger}");
}

#[test]
fn a_merge_retires_a_missing_output_of_a_selected_owner_that_moved_its_claim() {
    let e = Env::personal();
    e.mkdirs(".claude");
    let out = e.h(".claude/settings.json");
    // A still selected resource once owned this path and now claims another one. The file is gone.
    record(&e, "core.git-defaults", &out, "{}");
    fs::remove_file(&out).unwrap();
    let (o, s) = e.saved();
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let pos = |pre: &str| {
        s.plan
            .operations
            .iter()
            .position(|o| o.id.starts_with(pre))
            .unwrap()
    };
    assert!(pos("remove@core.git-defaults@.claude/settings.json") < pos("agents.claude-settings@"));
    let o = e.run(&[
        "apply",
        "--root",
        e.root.to_str().unwrap(),
        "--yes-plan",
        &s.plan_digest,
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", text(&o.stdout), text(&o.stderr));
    assert!(out.is_file());
}
