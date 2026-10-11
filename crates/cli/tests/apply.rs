//! `apply` through the isolated binary against synthetic, link-only source trees: publication,
//! refusal before mutation, removal, races, failure accounting and reruns (DFM-18).
//!
//! No test touches the real HOME. Every run passes `--home <temp dir>` and an empty environment.

mod common;

use common::TempDir;
use dfm_model::plan::Saved;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
}

const CORE: &str = r#"schema = 1
id = "core"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "git"
kind = "link"
adapter = "fs-link/v1"
target = ".config/git/config"
[resources.value]
source = "files/gitconfig"

[[resources]]
id = "ssh"
kind = "link"
adapter = "fs-link/v1"
target = ".ssh/config"
[resources.value]
source = "files/ssh_config"
"#;

const EXTRA: &str = r#"schema = 1
id = "extra"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "tmux"
kind = "link"
adapter = "fs-link/v1"
target = ".config/tmux/tmux.conf"
[resources.value]
source = "files/tmux.conf"

[[resources]]
id = "zsh"
kind = "link"
adapter = "fs-link/v1"
target = ".zshrc"
[resources.value]
source = "files/zshrc"
"#;

const BUNDLE: &str = r#"schema = 1
id = "t-base"
core = "modules/core/module.toml"

[modules]
extra = "modules/extra/module.toml"
"#;

fn profile(modules: &str) -> String {
    format!(
        "schema = 1\nid = \"t-01\"\nbundle = \"../base/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [{modules}]\noverrides = []\n"
    )
}

impl Fx {
    /// A bound machine whose profile selects the `extra` module.
    fn new() -> Fx {
        Fx::with_root_in_home(false)
    }

    /// With `in_home`, the source tree sits at `~/.dotfiles` (the usual layout).
    fn with_root_in_home(in_home: bool) -> Fx {
        let home = TempDir::new("apply-home");
        let src = TempDir::new("apply-src");
        let root = if in_home {
            home.path().join(".dotfiles")
        } else {
            src.path().join("root")
        };
        let fx = Fx {
            home,
            _src: src,
            root,
        };
        fx.put("base/bundle.toml", BUNDLE);
        fx.put("base/modules/core/module.toml", CORE);
        fx.put("base/modules/extra/module.toml", EXTRA);
        for (m, f, c) in [
            ("core", "gitconfig", "[user]\n"),
            ("core", "ssh_config", "Host *\n"),
            ("extra", "tmux.conf", "set -g mouse on\n"),
            ("extra", "zshrc", "export A=1\n"),
        ] {
            fx.put(&format!("base/modules/{m}/files/{f}"), c);
        }
        fx.put("profiles/t-01.toml", &profile("\"extra\""));
        let p = fx.root.join("profiles/t-01.toml");
        let o = fx.run(&[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-test",
        ]);
        assert_eq!(code(&o), 0, "{}", text(&o.stderr));
        fx
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
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

    fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    /// The digest of the plan for this machine now.
    fn digest(&self) -> String {
        let o = self.run(&["plan", "--root", self.root_arg(), "--json"]);
        Saved::parse(&o.stdout)
            .unwrap_or_else(|e| panic!("{e}\n{}", text(&o.stdout)))
            .plan_digest
    }

    fn saved(&self) -> Saved {
        let o = self.run(&["plan", "--root", self.root_arg(), "--json"]);
        Saved::parse(&o.stdout).unwrap()
    }

    fn apply(&self, extra: &[&str]) -> Output {
        let d = self.digest();
        self.apply_with(&d, extra)
    }

    fn apply_with(&self, digest: &str, extra: &[&str]) -> Output {
        let mut a = vec!["apply", "--root", self.root_arg(), "--yes-plan", digest];
        a.extend_from_slice(extra);
        self.run(&a)
    }

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn state(&self) -> PathBuf {
        self.h(".local/state/dotfiles-manager/state")
    }

    fn ledger(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.state().join("ledger.json")).unwrap()).unwrap()
    }

    fn journal(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.state().join("journal.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn receipts(&self) -> Vec<serde_json::Value> {
        let mut v: Vec<_> = fs::read_dir(self.state().join("receipts"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        v.sort();
        let mut all: Vec<serde_json::Value> = v
            .iter()
            .map(|p| serde_json::from_slice(&fs::read(p).unwrap()).unwrap())
            .collect();
        // Oldest first. Two runs in the same second sort by file name, which has no meaning, so
        // the run that published more comes first (the earlier run of these tests).
        let updated = |r: &serde_json::Value| {
            r["operations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|o| o["status"] == "updated")
                .count()
        };
        all.sort_by(|a, b| {
            a["started_at"]
                .as_str()
                .cmp(&b["started_at"].as_str())
                .then(updated(b).cmp(&updated(a)))
        });
        all
    }

    fn owned_paths(&self) -> Vec<String> {
        let l = self.ledger();
        let mut v: Vec<String> = l["resources"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["owned_outputs"].as_array().unwrap().iter())
            .map(|o| {
                let p = o["path"].as_str().unwrap();
                p.strip_prefix(self.home.path().to_str().unwrap())
                    .unwrap()
                    .trim_start_matches('/')
                    .to_string()
            })
            .collect();
        v.sort();
        v
    }

    /// Drops the zsh link, so the first operation is a directory with a dependent link.
    fn without_zsh(&self) {
        let m = fs::read_to_string(self.root.join("base/modules/extra/module.toml")).unwrap();
        self.put(
            "base/modules/extra/module.toml",
            &m[..m.find("[[resources]]\nid = \"zsh\"").unwrap()],
        );
    }

    fn lock_note(&self) -> Vec<u8> {
        fs::read(self.h(".local/state/dotfiles-manager.lock")).unwrap()
    }

    fn set_profile(&self, modules: &str) {
        self.put("profiles/t-01.toml", &profile(modules));
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn stderr(o: &Output) -> String {
    text(&o.stderr)
}

/// Names, kinds, modes, modification times and bytes of everything below `root`, links unread.
/// The lock file is left out: every mutating command writes its holder note.
fn fingerprint(root: &Path) -> String {
    fingerprint_skipping(root, None)
}

/// As [`fingerprint`], without the entry `skip` (a path below `root`).
fn fingerprint_skipping(root: &Path, skip: Option<&Path>) -> String {
    fn go(dir: &Path, h: &mut Sha256, skip: Option<&Path>) {
        let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        v.sort_by_key(|e| e.file_name());
        for e in v {
            if e.file_name() == "dotfiles-manager.lock" || Some(e.path().as_path()) == skip {
                continue;
            }
            let m = fs::symlink_metadata(e.path()).unwrap();
            h.update(e.path().to_str().unwrap().as_bytes());
            h.update(
                format!("{:?} {:o}", m.modified().unwrap(), m.permissions().mode()).as_bytes(),
            );
            if m.is_dir() {
                go(&e.path(), h, skip);
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
    go(root, &mut h, skip);
    format!("{:x}", h.finalize())
}

fn is_link_to(p: &Path, target: &Path) -> bool {
    fs::read_link(p).is_ok_and(|t| t == target)
}

// ---- approval ---------------------------------------------------------------------------

#[test]
fn apply_without_an_approved_digest_prints_the_plan_and_writes_nothing() {
    let fx = Fx::new();
    let before = fingerprint(fx.home.path());
    let o = fx.run(&["apply", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
    assert!(text(&o.stdout).contains("plan digest: "));
    assert!(text(&o.stdout).contains("--yes-plan") || stderr(&o).contains("--yes-plan"));
    assert_eq!(fingerprint(fx.home.path()), before);
    assert!(!fx.h(".zshrc").exists() && !fx.h(".ssh").exists());
}

#[test]
fn preview_prints_the_plan_digest_and_changes_nothing() {
    let fx = Fx::new();
    let before = fingerprint(fx.home.path());
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(text(&o.stdout).contains(&format!("plan digest: {}", fx.digest())));
    assert_eq!(fingerprint(fx.home.path()), before);
}

#[test]
fn bad_flags_are_usage_errors() {
    let fx = Fx::new();
    let d = fx.digest();
    for args in [
        vec![
            "apply",
            "--root",
            fx.root_arg(),
            "--preview",
            "--yes-plan",
            &d,
        ],
        vec!["apply", "--root", fx.root_arg(), "--yes-plan", "abc"],
        vec![
            "apply",
            "--root",
            fx.root_arg(),
            "--yes-plan",
            &d,
            "--yes-plan",
            &d,
        ],
        vec!["apply", "--yes-plan", &d],
        vec!["apply", "--root", fx.root_arg(), "--force"],
    ] {
        let o = fx.run(&args);
        assert_eq!(code(&o), 2, "{args:?}: {}", stderr(&o));
    }
    // An allowance for an operation the plan does not contain is refused as well, before the
    // lock is taken and before the state store is opened for writing.
    let note = fx.lock_note();
    let before = fingerprint(fx.home.path());
    let o = fx.apply_with(&d, &["--allow-disruptive", "remove@nothing@here"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(
        text(&o.stdout).contains("result: usage"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.lock_note(), note, "the lock was not taken");
    assert_eq!(fingerprint(fx.home.path()), before);
}

#[test]
fn a_changed_machine_makes_the_approved_plan_stale_and_writes_nothing() {
    let fx = Fx::new();
    let approved = fx.digest();
    // The machine changes after the review: a parent directory appears.
    fs::create_dir_all(fx.h(".ssh")).unwrap();
    let before = fingerprint(fx.home.path());
    let o = fx.apply_with(&approved, &[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(
        fingerprint(fx.home.path()),
        before,
        "a stale refusal writes nothing"
    );
    assert!(fx.receipts().is_empty() && fx.journal().is_empty());
}

#[test]
fn a_changed_source_makes_the_approved_plan_stale() {
    let fx = Fx::new();
    let approved = fx.digest();
    fx.put(
        "base/modules/core/files/gitconfig",
        "[user]\nname = edited\n",
    );
    let o = fx.apply_with(&approved, &[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"));
    assert!(!fx.h(".config/git/config").exists());
}

// ---- publication ------------------------------------------------------------------------

#[test]
fn apply_publishes_links_creates_parents_and_records_ownership_and_a_receipt() {
    let fx = Fx::new();
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(
        out.contains("counts: updated=") && out.contains("failed=0"),
        "{out}"
    );
    for (rel, src) in [
        (".config/git/config", "base/modules/core/files/gitconfig"),
        (".ssh/config", "base/modules/core/files/ssh_config"),
        (
            ".config/tmux/tmux.conf",
            "base/modules/extra/files/tmux.conf",
        ),
        (".zshrc", "base/modules/extra/files/zshrc"),
    ] {
        assert!(is_link_to(&fx.h(rel), &fx.root.join(src)), "{rel}");
    }
    // Directories the manager made are private, and never recorded as owned.
    for d in [".config", ".config/git", ".ssh"] {
        let m = fs::metadata(fx.h(d)).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o700, "{d}");
    }
    assert_eq!(
        fx.owned_paths(),
        [
            ".config/git/config",
            ".config/tmux/tmux.conf",
            ".ssh/config",
            ".zshrc"
        ]
    );
    // Every effect was journaled first: an intent, then its outcome, with distinct ids.
    let j = fx.journal();
    let intents = j.iter().filter(|e| e["phase"] == "intent").count();
    let dones = j.iter().filter(|e| e["phase"] == "done").count();
    assert_eq!((intents, dones), (7, 7), "{j:#?}");
    let ids: std::collections::BTreeSet<_> = j
        .iter()
        .filter(|e| e["phase"] == "intent")
        .map(|e| e["op_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 7);
    // The receipt records publication, and separately that no reload was attempted.
    let rs = fx.receipts();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0]["result"]["outcome"], "ok");
    assert_eq!(rs[0]["approval"]["kind"], "yes_plan");
    let links: Vec<_> = rs[0]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["status"] == "updated" && o["reload"].is_string())
        .collect();
    assert_eq!(links.len(), 4);
    assert!(links.iter().all(|o| o["reload"] == "not_attempted"));
    assert!(out.contains("reload: not attempted"), "{out}");
}

#[test]
fn publication_success_does_not_imply_consumer_reload() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    let rs = fx.receipts();
    for op in rs[0]["operations"].as_array().unwrap() {
        if op["status"] == "updated" {
            // A reload field is present only on published outputs, and it never claims one.
            let r = op["reload"].as_str();
            assert!(matches!(r, None | Some("not_attempted")), "{op}");
            assert_ne!(r, Some("reloaded"));
        }
    }
}

#[test]
fn a_second_apply_of_an_applied_plan_changes_nothing_and_exits_0() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
    assert_eq!(
        fingerprint(fx.home.path()),
        before,
        "a rerun writes no state"
    );
    assert_eq!(
        fx.receipts().len(),
        1,
        "a run that changed nothing writes no receipt"
    );
}

#[test]
fn recursive_dotfiles_layout_links_into_a_source_tree_inside_home() {
    // The usual layout: the source tree is ~/.dotfiles and every link points back into it.
    let fx = Fx::with_root_in_home(true);
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(is_link_to(
        &fx.h(".zshrc"),
        &fx.h(".dotfiles/base/modules/extra/files/zshrc")
    ));
    assert_eq!(fs::read_to_string(fx.h(".zshrc")).unwrap(), "export A=1\n");
    // The source tree is untouched, and the manager never claims or removes it.
    assert!(fx.h(".dotfiles/base/modules/extra/files/zshrc").is_file());
    assert!(fx.owned_paths().iter().all(|p| !p.starts_with(".dotfiles")));
}

#[test]
fn a_parent_that_links_into_the_source_tree_is_refused_and_the_source_is_untouched() {
    // ~/.ssh is a link into ~/.dotfiles: writing ~/.ssh/config would write into the source
    // tree. The claim must be refused with nothing written.
    let fx = Fx::with_root_in_home(true);
    fs::create_dir_all(fx.h(".dotfiles/config")).unwrap();
    symlink(fx.h(".dotfiles/config"), fx.h(".ssh")).unwrap();
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PARENT_UNSAFE"), "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
    assert_eq!(fs::read_dir(fx.h(".dotfiles/config")).unwrap().count(), 0);
}

#[test]
fn dangling_and_looping_parent_links_are_refused_with_nothing_written() {
    for setup in ["dangling", "loop"] {
        let fx = Fx::new();
        fs::create_dir_all(fx.h(".config")).unwrap();
        match setup {
            "dangling" => symlink(fx.h("nowhere"), fx.h(".config/git")).unwrap(),
            _ => symlink(fx.h(".config/git"), fx.h(".config/git")).unwrap(),
        }
        let before = fingerprint(fx.home.path());
        let o = fx.apply(&[]);
        assert_eq!(code(&o), 3, "{setup}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("E_PARENT_UNSAFE"),
            "{setup}: {}",
            stderr(&o)
        );
        assert_eq!(fingerprint(fx.home.path()), before, "{setup}");
        assert!(!fx.h("nowhere").exists());
    }
}

#[test]
fn directory_and_file_transitions_are_refused_and_preserved() {
    // A directory where the link goes, and a file where a directory is needed.
    let fx = Fx::new();
    fs::create_dir_all(fx.h(".zshrc/inner")).unwrap();
    fs::write(fx.h(".ssh"), "i am a file\n").unwrap();
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
    assert!(fx.h(".zshrc/inner").is_dir());
    assert_eq!(fs::read_to_string(fx.h(".ssh")).unwrap(), "i am a file\n");
}

#[test]
fn a_foreign_file_or_link_at_a_target_is_preserved() {
    let fx = Fx::new();
    fs::create_dir_all(fx.h(".config/git")).unwrap();
    fs::write(fx.h(".config/git/config"), "[user]\nname = me\n").unwrap();
    fs::write(fx.h(".zshrc"), "mine\n").unwrap();
    symlink("/somewhere/else", fx.h(".config/tmux")).ok();
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT") || stderr(&o).contains("E_PARENT_UNSAFE"));
    assert_eq!(fingerprint(fx.home.path()), before);
}

#[test]
fn an_owned_link_the_user_retargeted_is_preserved_not_replaced() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fs::remove_file(fx.h(".zshrc")).unwrap();
    symlink("/my/own/zshrc", fx.h(".zshrc")).unwrap();
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
    assert!(is_link_to(&fx.h(".zshrc"), Path::new("/my/own/zshrc")));
}

#[test]
fn a_source_that_moves_replaces_only_the_link_the_manager_owns() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    // The module now points the resource at another source file.
    fx.put("base/modules/extra/files/zshrc2", "export B=2\n");
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fs::write(
        fx.root.join("base/modules/extra/module.toml"),
        m.replace("files/zshrc\"", "files/zshrc2\""),
    )
    .unwrap();
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(is_link_to(
        &fx.h(".zshrc"),
        &fx.root.join("base/modules/extra/files/zshrc2")
    ));
    // The old source and the other links are untouched.
    assert!(fx.root.join("base/modules/extra/files/zshrc").is_file());
    assert!(fx.h(".ssh/config").is_symlink());
    let rs = fx.receipts();
    assert_eq!(rs.len(), 2);
    assert_eq!(
        rs[1]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["status"] == "updated")
            .count(),
        1
    );
}

#[test]
fn configuration_changes_never_require_rebuilding_the_manager() {
    // The user requirement: editing configuration must not need a manager rebuild or a change
    // to the manager pin. The same binary plans and applies both versions of the source.
    let fx = Fx::new();
    let bin_before = fs::read(BIN).unwrap();
    let p1 = fx.saved();
    assert_eq!(code(&fx.apply(&[])), 0);
    // Edit configuration content: the tree digest changes, the old approval is stale.
    fx.put("base/modules/extra/files/zshrc", "export A=changed\n");
    let stale = fx.apply_with(&p1.plan_digest, &[]);
    assert_eq!(code(&stale), 3, "{}", stderr(&stale));
    // Add a new resource: a new module file, no new manager.
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &format!(
            "{m}\n[[resources]]\nid = \"vim\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".vimrc\"\n[resources.value]\nsource = \"files/vimrc\"\n"
        ),
    );
    fx.put("base/modules/extra/files/vimrc", "set number\n");
    let p2 = fx.saved();
    assert_ne!(p1.plan_digest, p2.plan_digest);
    assert_eq!(
        p1.plan.binary, p2.plan.binary,
        "the manager binary did not change"
    );
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fs::read_to_string(fx.h(".vimrc")).unwrap(), "set number\n");
    assert_eq!(
        fs::read_to_string(fx.h(".zshrc")).unwrap(),
        "export A=changed\n"
    );
    assert_eq!(fs::read(BIN).unwrap(), bin_before, "no rebuild happened");
    assert!(
        !fx.root.join("dotfiles-manager.lock").exists(),
        "no pin was written"
    );
}

// ---- removal ----------------------------------------------------------------------------

#[test]
fn deselecting_a_module_removes_only_its_owned_links_and_nothing_else() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    // Things that are not the manager's: a sibling in an owned directory, and runtime state.
    fs::write(fx.h(".config/tmux/plugins.txt"), "tpm\n").unwrap();
    fs::create_dir_all(fx.h(".local/share/app")).unwrap();
    fs::write(fx.h(".local/share/app/state.db"), "data").unwrap();
    fx.set_profile("");
    let d = fx.digest();
    let plan = fx.saved();
    let removals: Vec<String> = plan
        .plan
        .operations
        .iter()
        .filter(|o| o.id.starts_with("remove@"))
        .map(|o| o.id.clone())
        .collect();
    assert_eq!(removals.len(), 2, "{removals:?}");
    // Without an allowance for each removal, nothing is deleted.
    let before = fingerprint(fx.home.path());
    let o = fx.apply_with(&d, &[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("E_DISRUPTIVE_UNAPPROVED"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fingerprint(fx.home.path()), before);
    // One allowance is not enough either.
    let o = fx.apply_with(&d, &["--allow-disruptive", &removals[0]]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
    // With every removal approved, exactly the two owned links go.
    let mut args = Vec::new();
    for r in &removals {
        args.push("--allow-disruptive");
        args.push(r.as_str());
    }
    let o = fx.apply_with(&d, &args);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(!fx.h(".zshrc").exists() && fs::symlink_metadata(fx.h(".zshrc")).is_err());
    assert!(fs::symlink_metadata(fx.h(".config/tmux/tmux.conf")).is_err());
    assert_eq!(fx.owned_paths(), [".config/git/config", ".ssh/config"]);
    // Siblings, directories, other links, runtime state and the sources stay.
    assert!(fx.h(".config/tmux").is_dir());
    assert_eq!(
        fs::read_to_string(fx.h(".config/tmux/plugins.txt")).unwrap(),
        "tpm\n"
    );
    assert!(fx.h(".local/share/app/state.db").is_file());
    assert!(fx.h(".ssh/config").is_symlink() && fx.h(".config/git/config").is_symlink());
    assert!(fx.root.join("base/modules/extra/files/zshrc").is_file());
}

#[test]
fn removal_is_refused_when_the_owned_link_was_edited() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fs::remove_file(fx.h(".zshrc")).unwrap();
    symlink("/my/own/zshrc", fx.h(".zshrc")).unwrap();
    fx.set_profile("");
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
    assert!(is_link_to(&fx.h(".zshrc"), Path::new("/my/own/zshrc")));
}

#[test]
fn removal_never_follows_a_link_swapped_in_for_the_owned_output() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fx.set_profile("");
    let d = fx.digest();
    let plan = fx.saved();
    let removals: Vec<String> = plan
        .plan
        .operations
        .iter()
        .filter(|o| o.id.starts_with("remove@"))
        .map(|o| o.id.clone())
        .collect();
    // After the review, the owned link is replaced by a link to a precious file.
    fs::write(fx.h("precious"), "keep me\n").unwrap();
    fs::remove_file(fx.h(".zshrc")).unwrap();
    symlink(fx.h("precious"), fx.h(".zshrc")).unwrap();
    let mut args = Vec::new();
    for r in &removals {
        args.push("--allow-disruptive");
        args.push(r.as_str());
    }
    let o = fx.apply_with(&d, &args);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(fs::read_to_string(fx.h("precious")).unwrap(), "keep me\n");
    assert!(is_link_to(&fx.h(".zshrc"), &fx.h("precious")));
}

#[test]
fn an_owned_link_swapped_while_removal_waits_is_never_followed_or_deleted() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fx.set_profile("");
    fs::write(fx.h("precious"), "keep me\n").unwrap();
    let plan = fx.saved();
    let removals: Vec<String> = plan
        .plan
        .operations
        .iter()
        .filter(|o| o.id.starts_with("remove@"))
        .map(|o| o.id.clone())
        .collect();
    let digest = plan.plan_digest.clone();
    let gate = fx
        .home
        .path()
        .parent()
        .unwrap()
        .join(format!("rm-gate-{}", std::process::id()));
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .env(
            "DFM_FAILPOINT",
            format!("apply-before-effect:wait-{}", gate.display()),
        )
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
    for r in &removals {
        cmd.args(["--allow-disruptive", r]);
    }
    let child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let reached = gate.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !reached.exists() {
        assert!(std::time::Instant::now() < end);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for t in [".zshrc", ".config/tmux/tmux.conf"] {
        fs::remove_file(fx.h(t)).unwrap();
        symlink(fx.h("precious"), fx.h(t)).unwrap();
    }
    fs::write(&gate, "").unwrap();
    let o = child.wait_with_output().unwrap();
    let _ = fs::remove_file(&gate);
    let _ = fs::remove_file(&reached);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fs::read_to_string(fx.h("precious")).unwrap(), "keep me\n");
    assert!(is_link_to(&fx.h(".zshrc"), &fx.h("precious")));
    assert!(is_link_to(
        &fx.h(".config/tmux/tmux.conf"),
        &fx.h("precious")
    ));
    assert_eq!(
        fx.owned_paths().len(),
        4,
        "a refused removal keeps the ledger entry"
    );
}

#[test]
fn deselecting_everything_still_removes_only_proven_outputs_one_approval_each() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    // Make one owned link foreign by retargeting it. Deselect everything.
    fs::remove_file(fx.h(".ssh/config")).unwrap();
    symlink("/mine", fx.h(".ssh/config")).unwrap();
    fx.set_profile("");
    // The profile still selects core, so nothing is deselected: nothing to remove.
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(is_link_to(&fx.h(".ssh/config"), Path::new("/mine")));
    assert_eq!(fx.owned_paths().len(), 4, "the ledger keeps what it proves");
}

// ---- races and failures -----------------------------------------------------------------

/// Starts `apply` parked at a failpoint, runs `during` while it waits, then releases it.
fn parked_apply(fx: &Fx, point: &str, during: impl FnOnce(&Fx)) -> Output {
    parked_apply_with(fx, point, &[], during)
}

/// As [`parked_apply`], with more `apply` arguments.
fn parked_apply_with(fx: &Fx, point: &str, extra: &[&str], during: impl FnOnce(&Fx)) -> Output {
    parked_apply_pid(fx, point, extra, |fx, _pid| during(fx))
}

/// As [`parked_apply_with`], and `during` also gets the process id of the parked `apply`.
fn parked_apply_pid(fx: &Fx, point: &str, extra: &[&str], during: impl FnOnce(&Fx, u32)) -> Output {
    let gate = fx.home.path().parent().unwrap().join(format!(
        "gate-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let gate = PathBuf::from(gate.to_str().unwrap().replace(['(', ')'], ""));
    let _ = fs::remove_file(&gate);
    let _ = fs::remove_file(gate.with_extension("reached"));
    let digest = fx.digest();
    let child = Command::new(BIN)
        .env_clear()
        .env("DFM_FAILPOINT", format!("{point}:wait-{}", gate.display()))
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let reached = gate.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < end,
            "apply never reached {point}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    during(fx, child.id());
    fs::write(&gate, "").unwrap();
    let out = child.wait_with_output().unwrap();
    let _ = fs::remove_file(&gate);
    let _ = fs::remove_file(&reached);
    out
}

#[test]
fn a_target_swapped_for_a_foreign_file_after_the_last_check_is_never_overwritten() {
    let fx = Fx::new();
    fx.without_zsh();
    // Every parent exists, so the first operation is the creation of the git link.
    fs::create_dir_all(fx.h(".config/git")).unwrap();
    fs::create_dir_all(fx.h(".config/tmux")).unwrap();
    fs::create_dir_all(fx.h(".ssh")).unwrap();
    let o = parked_apply(&fx, "apply-before-effect", |fx| {
        for t in [
            ".config/git/config",
            ".config/tmux/tmux.conf",
            ".ssh/config",
        ] {
            fs::write(fx.h(t), "foreign\n").unwrap();
        }
    });
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    // The parked operation is the git link, and it is the one that failed.
    assert!(out.contains("failed core.git@.config/git/config"), "{out}");
    assert!(
        out.contains("counts: updated=0 current=0 failed=1 blocked=0 unknown=0 not_started=2"),
        "{out}"
    );
    assert!(out.contains("result: failed"), "{out}");
    for t in [
        ".config/git/config",
        ".config/tmux/tmux.conf",
        ".ssh/config",
    ] {
        assert_eq!(fs::read_to_string(fx.h(t)).unwrap(), "foreign\n", "{t}");
    }
    assert!(
        fx.owned_paths().is_empty(),
        "a failed publication is not owned"
    );
    let rs = fx.receipts();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0]["result"]["outcome"], "failed");
    let counts = &rs[0]["result"]["counts"];
    assert_eq!(
        (
            counts["failed"].as_u64(),
            counts["updated"].as_u64(),
            counts["not_started"].as_u64()
        ),
        (Some(1), Some(0), Some(2))
    );
}

#[test]
fn a_parent_swapped_for_a_link_after_the_check_receives_no_write() {
    let fx = Fx::new();
    fx.without_zsh();
    fs::create_dir_all(fx.h("victim")).unwrap();
    // The first operation creates ~/.config/git. While it waits (the parent ~/.config is
    // already open), ~/.config is swapped for a link to a directory outside the plan.
    let o = parked_apply(&fx, "apply-before-effect", |fx| {
        fs::rename(fx.h(".config"), fx.h(".config-moved")).unwrap();
        symlink(fx.h("victim"), fx.h(".config")).unwrap();
    });
    assert_eq!(
        fs::read_dir(fx.h("victim")).unwrap().count(),
        0,
        "nothing was written through the link"
    );
    // The effect landed in the directory that was verified, which now has another name.
    assert!(fx.h(".config-moved/git").is_dir());
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains("updated mkdir@.config/git"), "{out}");
    // The next operation saw the changed topology and was blocked, the rest did not start.
    assert!(out.contains("blocked core.git@.config/git/config"), "{out}");
    assert!(
        out.contains("counts: updated=1 current=0 failed=0 blocked=1 unknown=0 not_started=4"),
        "{out}"
    );
}

#[test]
fn a_required_failure_stops_mutation_blocks_dependents_and_counts_the_rest() {
    let fx = Fx::new();
    fx.without_zsh();
    let digest = fx.digest();
    let o = Command::new(BIN)
        .env_clear()
        .env("DFM_FAILPOINT", "apply-before-effect:error")
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
        .output()
        .unwrap();
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    // Exactly one failure (the first directory), exactly one dependent, four not started.
    assert!(out.contains("failed mkdir@.config/git"), "{out}");
    assert!(
        out.contains("counts: updated=0 current=0 failed=1 blocked=1 unknown=0 not_started=4"),
        "{out}"
    );
    assert!(out.contains("result: failed"), "{out}");
    // The original code is kept. There is no umbrella code.
    assert!(stderr(&o).contains("E_IO_FAILED"), "{}", stderr(&o));
    assert!(!stderr(&o).contains("E_APPLY_FAILED"));
    let r = &fx.receipts()[0];
    assert_eq!(r["result"]["outcome"], "failed");
    assert_eq!(r["result"]["exit_code"], 1);
    // The journal shows the intent and its failure. The receipt id of the failed operation is
    // the journal attempt id. Nothing was recorded as owned.
    let j = fx.journal();
    assert_eq!(j.iter().filter(|e| e["phase"] == "intent").count(), 1);
    assert_eq!(j.iter().filter(|e| e["phase"] == "failed").count(), 1);
    let failed_op = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["status"] == "failed")
        .unwrap();
    assert_eq!(failed_op["id"], j[0]["op_id"]);
    assert!(fx.owned_paths().is_empty());
}

#[test]
fn a_failure_before_any_write_prints_failed_and_exits_1() {
    // The journal cannot take the first intent entry: the run fails before it writes anything.
    // That is a failure, not a block, so the printed result and the exit say so.
    let fx = Fx::new();
    let digest = fx.digest();
    let o = Command::new(BIN)
        .env_clear()
        .env("DFM_FAILPOINT", "journal-before-write:error")
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
        .output()
        .unwrap();
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("result: failed"),
        "{}",
        text(&o.stdout)
    );
    assert!(stderr(&o).contains("E_IO_FAILED"), "{}", stderr(&o));
    assert!(
        fx.receipts().is_empty(),
        "nothing was written, so there is no receipt"
    );
    assert!(fx.owned_paths().is_empty());
}

#[test]
fn an_unwritable_directory_is_a_failure_and_never_data_loss() {
    if running_as_root() {
        return;
    }
    let fx = Fx::new();
    fx.without_zsh();
    fs::create_dir_all(fx.h(".config/git")).unwrap();
    fs::create_dir_all(fx.h(".config/tmux")).unwrap();
    fs::create_dir_all(fx.h(".ssh")).unwrap();
    fs::write(fx.h("keep.txt"), "keep\n").unwrap();
    // The first operation is the git link. Its directory loses write permission while it waits.
    let o = parked_apply(&fx, "apply-before-effect", |fx| {
        fs::set_permissions(fx.h(".config/git"), fs::Permissions::from_mode(0o500)).unwrap();
    });
    fs::set_permissions(fx.h(".config/git"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains("failed core.git@.config/git/config"), "{out}");
    assert!(
        out.contains("counts: updated=0 current=0 failed=1 blocked=0 unknown=0 not_started=2"),
        "{out}"
    );
    assert_eq!(fs::read_to_string(fx.h("keep.txt")).unwrap(), "keep\n");
    assert!(fx.owned_paths().is_empty());
    assert!(fs::symlink_metadata(fx.h(".config/git/config")).is_err());
}

#[test]
fn an_unwritable_directory_known_at_plan_time_blocks_before_any_write() {
    if running_as_root() {
        return;
    }
    let fx = Fx::new();
    fs::create_dir_all(fx.h(".ssh")).unwrap();
    fs::set_permissions(fx.h(".ssh"), fs::Permissions::from_mode(0o500)).unwrap();
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    let after = fingerprint(fx.home.path());
    fs::set_permissions(fx.h(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("E_PRIVILEGE_UNSUPPORTED"),
        "{}",
        stderr(&o)
    );
    assert!(
        text(&o.stdout).contains("result: blocked"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(after, before);
}

fn running_as_root() -> bool {
    // A root user ignores directory modes, so these tests would prove nothing.
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| text(&o.stdout).trim() == "0")
}

#[test]
fn a_restrictive_umask_does_not_wedge_the_directories_apply_creates() {
    // With umask 0500, mkdir(0700) would give mode 0200: the process could not open the
    // directory it just made. Apply states the mode, so the run completes with 0700.
    let fx = Fx::new();
    let digest = fx.digest();
    let o = Command::new("/bin/sh")
        .env_clear()
        .args(["-c", "umask 0500 && exec \"$0\" \"$@\""])
        .arg(BIN)
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    for d in [".config/git", ".config/tmux", ".ssh"] {
        let m = fs::metadata(fx.h(d)).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o700, "{d}");
    }
    assert!(fx.h(".config/git/config").is_symlink());
}

#[test]
fn a_crash_after_a_link_effect_leaves_an_unowned_link_that_a_later_run_adopts() {
    // The first operation is the zsh link. The process dies right after the link exists and
    // before the journal says done, so the ledger has no entry for it (docs/state.md rule 5).
    let fx = Fx::new();
    let digest = fx.digest();
    let o = Command::new(BIN)
        .env_clear()
        .env("DFM_FAILPOINT", "apply-after-effect:abort")
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
        .output()
        .unwrap();
    assert!(!o.status.success());
    let zsh = fx.root.join("base/modules/extra/files/zshrc");
    assert!(is_link_to(&fx.h(".zshrc"), &zsh));
    assert!(
        fx.owned_paths().is_empty(),
        "the ledger has no entry for it"
    );
    let j = fx.journal();
    assert_eq!((j.len(), j[0]["phase"].as_str()), (1, Some("intent")));
    // The link is not owned, so it is only an adoption candidate: without the flag nothing
    // runs, not even the settling of the interruption.
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_ADOPT_UNAPPROVED"), "{}", stderr(&o));
    assert!(fx.owned_paths().is_empty());
    assert_eq!(fx.journal().len(), 1);
    // A run that deselects the module settles the interruption as done and removes nothing: no
    // ledger entry says the manager owns the link, so not even an allowance could delete it.
    fx.set_profile("");
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.journal();
    let first = j[0]["op_id"].as_str().unwrap();
    assert!(
        j.iter()
            .any(|e| e["op_id"] == first && e["phase"] == "done"),
        "the interrupted entry is settled as done: {j:#?}"
    );
    assert!(is_link_to(&fx.h(".zshrc"), &zsh));
    assert!(!fx.owned_paths().contains(&".zshrc".to_string()));
    // Selecting the module again plans the leftover link as an adoption. With the flag the link
    // is recorded and the rest of the plan runs.
    fx.set_profile("\"extra\"");
    let o = fx.apply(&["--allow-adopt", "extra.zsh@.zshrc"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(fx.owned_paths().contains(&".zshrc".to_string()));
    assert!(is_link_to(&fx.h(".zshrc"), &zsh));
}

#[test]
fn a_crash_between_intent_and_effect_is_settled_and_its_journal_id_is_not_reused() {
    let fx = Fx::new();
    let digest = fx.digest();
    let o = Command::new(BIN)
        .env_clear()
        .env("DFM_FAILPOINT", "apply-before-effect:abort")
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
        .output()
        .unwrap();
    assert!(!o.status.success());
    let j = fx.journal();
    assert_eq!(j.len(), 1, "{j:#?}");
    assert_eq!(j[0]["phase"], "intent");
    let first_id = j[0]["op_id"].as_str().unwrap().to_string();
    assert!(first_id.ends_with("-1"), "{first_id}");
    // The rerun settles the interruption, then runs everything with fresh ids.
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.journal();
    let by_id = |id: &str, phase: &str| {
        j.iter()
            .filter(|e| e["op_id"] == id && e["phase"] == phase)
            .count()
    };
    assert_eq!(
        by_id(&first_id, "intent"),
        1,
        "the interrupted id is not reused"
    );
    assert_eq!(
        by_id(&first_id, "failed"),
        1,
        "the absent effect is settled as failed"
    );
    let second = first_id.replace("-1", "-2");
    assert_eq!(by_id(&second, "intent"), 1);
    assert_eq!(by_id(&second, "done"), 1);
    assert_eq!(fx.owned_paths().len(), 4);
}

// ---- hardening ----------------------------------------------------------------------------

/// Moves `~/.config/tmux` outside HOME and leaves a link to it, as a stow-style setup does.
fn stow_style_tmux(fx: &Fx) -> PathBuf {
    let outside = fx
        .home
        .path()
        .parent()
        .unwrap()
        .join(format!("stow-{}", std::process::id()));
    fs::create_dir_all(&outside).unwrap();
    fs::rename(fx.h(".config/tmux"), outside.join("tmux")).unwrap();
    symlink(outside.join("tmux"), fx.h(".config/tmux")).unwrap();
    outside
}

#[test]
fn a_linked_parent_of_one_owned_output_blocks_only_that_entry() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    let outside = stow_style_tmux(&fx);
    let before = fingerprint(fx.home.path());

    // Still selected: plan and apply do not fail with E_STATE_INVALID. The one operation on
    // the unsafe entry is blocked, and every other operation stays current.
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let s = Saved::parse(&o.stdout).unwrap_or_else(|e| panic!("{e}\n{}", text(&o.stdout)));
    assert!(!stderr(&o).contains("E_STATE_INVALID"), "{}", stderr(&o));
    assert_eq!(s.plan.blockers.len(), 1, "{:?}", s.plan.blockers);
    assert_eq!(s.plan.blockers[0].resource, "extra.tmux");
    for op in &s.plan.operations {
        if op.resource != "extra.tmux" {
            assert_eq!(op.status, dfm_model::plan::Status::Current, "{}", op.id);
        }
    }
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(!stderr(&o).contains("E_STATE_INVALID"), "{}", stderr(&o));

    // Deselected: the removal on the unsafe entry is blocked as an unsafe parent. The other
    // removal (zsh) is still planned, and nothing is read or deleted through the link.
    fx.set_profile("");
    let s = fx.saved();
    assert_eq!(s.plan.blockers.len(), 1, "{:?}", s.plan.blockers);
    assert_eq!(s.plan.blockers[0].code.as_str(), "E_PARENT_UNSAFE");
    assert_eq!(s.plan.blockers[0].resource, "extra.tmux");
    let zsh = s
        .plan
        .operations
        .iter()
        .find(|o| o.id.starts_with("remove@extra.zsh"))
        .unwrap();
    assert_eq!(zsh.status, dfm_model::plan::Status::Planned);
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PARENT_UNSAFE"), "{}", stderr(&o));
    assert!(
        outside.join("tmux/tmux.conf").is_symlink(),
        "the link target was not touched"
    );
    assert!(
        fs::symlink_metadata(fx.h(".zshrc")).is_ok(),
        "nothing was removed"
    );
    assert_eq!(fingerprint(fx.home.path()), before, "nothing was written");
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn ledger_writes_still_work_when_another_entry_has_a_linked_parent() {
    // The tmux resource now claims another path, so its old entry is no longer claimed and not a
    // removal candidate (the resource is still selected). The old entry sits behind a link that
    // leaves HOME. A new link is published and the ledger is rewritten around the old entry.
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    let outside = stow_style_tmux(&fx);
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &m.replace(
            "target = \".config/tmux/tmux.conf\"",
            "target = \".tmux.conf\"",
        ),
    );
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(is_link_to(
        &fx.h(".tmux.conf"),
        &fx.root.join("base/modules/extra/files/tmux.conf")
    ));
    let owned = fx.owned_paths();
    assert!(owned.contains(&".tmux.conf".to_string()), "{owned:?}");
    assert!(
        owned.contains(&".config/tmux/tmux.conf".to_string()),
        "the old entry is kept: {owned:?}"
    );
    assert!(
        outside.join("tmux/tmux.conf").is_symlink(),
        "nothing was touched behind the link"
    );
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn preview_refuses_unapproved_removals_with_pasteable_flags_and_accepts_them_when_given() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fx.set_profile("");
    let ids = [
        "remove@extra.tmux@.config/tmux/tmux.conf",
        "remove@extra.zsh@.zshrc",
    ];
    let before = fingerprint(fx.home.path());
    // Without the flags the preview refuses exactly as apply would, and writes nothing.
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("E_DISRUPTIVE_UNAPPROVED"),
        "{}",
        stderr(&o)
    );
    let out = text(&o.stdout);
    for id in ids {
        assert!(out.contains(&format!("--allow-disruptive '{id}'")), "{out}");
    }
    assert!(out.contains("result: blocked"), "{out}");
    assert_eq!(fingerprint(fx.home.path()), before);
    // The flags, pasted back, make the preview exit 0 and the run apply exactly those removals.
    let mut flags = vec!["apply", "--root", fx.root_arg(), "--preview"];
    for id in ids {
        flags.push("--allow-disruptive");
        flags.push(id);
    }
    let o = fx.run(&flags);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
    let o = fx.apply_with(
        &fx.digest(),
        &["--allow-disruptive", ids[0], "--allow-disruptive", ids[1]],
    );
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // The refusal of a real run lists every id too.
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fx.set_profile("");
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stdout).contains("--allow-disruptive 'remove@extra.zsh@.zshrc'"),
        "{}",
        text(&o.stdout)
    );
    assert!(text(&o.stdout).contains("result: blocked"));
}

#[test]
fn a_full_journal_refuses_a_preview_and_a_run_before_any_write() {
    let fx = Fx::new();
    let e = dfm_model::state::JournalEntry {
        schema: 1,
        op_id: "p1-1".into(),
        op: "remove".into(),
        phase: dfm_model::state::Phase::Done,
        at: "2026-10-10T00:00:00Z".into(),
        resource: "r".into(),
        adapter: "fs-link/v1".into(),
        class: dfm_model::state::Class::Normal,
        recovery: None,
        token: None,
        reason: None,
    };
    let line = e.to_line().unwrap();
    let count = (dfm_model::state::MAX_JOURNAL_BYTES as usize - 2048) / line.len();
    let path = fx.state().join("journal.jsonl");
    fs::write(&path, line.repeat(count)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fingerprint(fx.home.path());
    for extra in [vec!["--preview"], vec![]] {
        let mut a = vec!["apply", "--root", fx.root_arg()];
        let d = fx.digest();
        if extra.is_empty() {
            a.extend(["--yes-plan", d.as_str()]);
        } else {
            a.extend(extra.iter().copied());
        }
        let o = fx.run(&a);
        assert_eq!(code(&o), 3, "{}", stderr(&o));
        assert!(stderr(&o).contains("E_LIMIT_EXCEEDED"), "{}", stderr(&o));
        assert!(stderr(&o).contains("DFM-19"), "{}", stderr(&o));
    }
    assert_eq!(fingerprint(fx.home.path()), before);
    assert!(!fx.h(".zshrc").exists());
}

#[test]
fn a_self_looping_parent_of_one_owned_output_blocks_only_that_entry() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    // ~/.config/tmux becomes a link to itself: resolving its children fails with a loop error.
    fs::remove_file(fx.h(".config/tmux/tmux.conf")).unwrap();
    fs::remove_dir(fx.h(".config/tmux")).unwrap();
    symlink(fx.h(".config/tmux"), fx.h(".config/tmux")).unwrap();
    let before = fingerprint(fx.home.path());
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let s = Saved::parse(&o.stdout).unwrap_or_else(|e| panic!("{e}\n{}", text(&o.stdout)));
    assert!(
        !stderr(&o).contains("E_STATE_INVALID") && !stderr(&o).contains("E_IO_FAILED"),
        "{}",
        stderr(&o)
    );
    assert_eq!(s.plan.blockers.len(), 1, "{:?}", s.plan.blockers);
    assert_eq!(s.plan.blockers[0].resource, "extra.tmux");
    for op in &s.plan.operations {
        if op.resource != "extra.tmux" {
            assert_eq!(op.status, dfm_model::plan::Status::Current, "{}", op.id);
        }
    }
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PARENT_UNSAFE"), "{}", stderr(&o));
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before, "nothing was written");
}

#[test]
fn the_receipt_records_each_disruptive_approval_and_the_path_source() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fx.set_profile("");
    let d = fx.digest();
    let o = fx.apply_with(
        &d,
        &[
            "--allow-disruptive",
            "remove@extra.tmux@.config/tmux/tmux.conf",
            "--allow-disruptive",
            "remove@extra.zsh@.zshrc",
        ],
    );
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let r = &fx.receipts()[1];
    let approvals = r["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 2);
    let op_ids: Vec<_> = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].clone())
        .collect();
    for a in approvals {
        assert_eq!(a["how"], "allow_disruptive");
        assert!(op_ids.contains(&a["operation_id"]), "{a}");
    }
    let src = &r["sources"][0];
    assert!(src.get("kind").is_none());
    assert!(src.get("commit").is_none());
    assert!(src["tree"].as_str().unwrap().starts_with("sha256:"));
    // The receipt a first apply writes has no approvals.
    assert!(fx.receipts()[0].get("approvals").is_none());
}

// ---- adoption (DFM-45) ------------------------------------------------------------------

impl Fx {
    /// A stow-style machine: every link the profile plans already exists, with real parents.
    fn stow(&self) {
        for (dir, target, src) in [
            (".config/git", ".config/git/config", "core/files/gitconfig"),
            (".ssh", ".ssh/config", "core/files/ssh_config"),
            (
                ".config/tmux",
                ".config/tmux/tmux.conf",
                "extra/files/tmux.conf",
            ),
            ("", ".zshrc", "extra/files/zshrc"),
        ] {
            fs::create_dir_all(self.h(dir)).unwrap();
            symlink(self.root.join("base/modules").join(src), self.h(target)).unwrap();
        }
    }

    fn adopt_ids(&self) -> Vec<String> {
        self.saved()
            .plan
            .operations
            .iter()
            .filter(|o| o.action == dfm_model::plan::Action::Adopt)
            .map(|o| o.id.clone())
            .collect()
    }
}

fn adopt_args(ids: &[String]) -> Vec<&str> {
    ids.iter()
        .flat_map(|i| ["--allow-adopt", i.as_str()])
        .collect()
}

/// Everything in HOME except the manager's own state directory tree.
fn home_outside_state(fx: &Fx) -> String {
    fingerprint_skipping(fx.home.path(), Some(&fx.h(".local")))
}

#[test]
fn a_stow_style_machine_is_adopted_without_writing_to_home() {
    let fx = Fx::new();
    fx.stow();
    let ids = fx.adopt_ids();
    assert_eq!(ids.len(), 4, "{ids:?}");
    let before = home_outside_state(&fx);
    let o = fx.apply(&adopt_args(&ids));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(
        out.contains("counts: updated=4 current=0 failed=0 blocked=0"),
        "{out}"
    );
    assert!(!out.contains("reload:"), "nothing was published: {out}");
    // Whole-HOME fingerprint: names, kinds, modes, mtimes, bytes and link text are unchanged.
    assert_eq!(home_outside_state(&fx), before);
    // The ledger owns every link, with the link fingerprint and the link mode.
    assert_eq!(fx.owned_paths().len(), 4);
    let zsh = fx.root.join("base/modules/extra/files/zshrc");
    let want = dfm_model::plan::link_fingerprint(zsh.to_str().unwrap());
    let owned: Vec<serde_json::Value> = fx.ledger()["resources"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| r["owned_outputs"].as_array().unwrap().clone())
        .collect();
    let z = owned
        .iter()
        .find(|o| o["path"].as_str().unwrap().ends_with("/.zshrc"))
        .unwrap();
    assert_eq!(z["fingerprint"], want.as_str());
    // The link fingerprint is not the hash of a file with the same bytes.
    assert_ne!(
        want,
        format!("{:x}", Sha256::digest(zsh.to_str().unwrap().as_bytes()))
    );
    assert_eq!(z["mode"], 0o777);
    // The receipt records each approval and no reload state.
    let r = fx.receipts();
    assert_eq!(r.len(), 1);
    let approvals = r[0]["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 4);
    assert!(approvals.iter().all(|a| a["how"] == "allow_adopt"));
    for op in r[0]["operations"].as_array().unwrap() {
        assert_eq!(op["status"], "updated");
        assert!(op["reload"].is_null(), "{op}");
        assert!(op["before"].is_null(), "{op}");
        assert!(op["after"]["fingerprint"].is_string());
        assert!(
            approvals.iter().any(|a| a["operation_id"] == op["id"]),
            "{op}"
        );
    }
    // The journal has an intent and a done entry per adoption.
    let j = fx.journal();
    assert_eq!(j.len(), 8, "{j:#?}");
    assert!(j.iter().all(|e| e["op"] == "adopt"));
    for e in j.iter().filter(|e| e["phase"] == "intent") {
        let out = &e["recovery"]["outputs"][0];
        assert_eq!(out["existed_before"], true);
        assert!(out["expected_sha256"].is_string());
    }
    // A rerun has nothing to do and writes no receipt.
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("counts: updated=0 current=4"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.receipts().len(), 1);
    assert_eq!(home_outside_state(&fx), before);
}

#[test]
fn a_later_deselect_removes_an_adopted_link_only_with_allow_disruptive() {
    let fx = Fx::new();
    fx.stow();
    let ids = fx.adopt_ids();
    assert_eq!(code(&fx.apply(&adopt_args(&ids))), 0);
    fx.set_profile("");
    let plan = fx.saved();
    let removals: Vec<String> = plan
        .plan
        .operations
        .iter()
        .filter(|o| o.id.starts_with("remove@"))
        .map(|o| o.id.clone())
        .collect();
    assert_eq!(removals.len(), 2, "{removals:?}");
    let o = fx.apply_with(&plan.plan_digest, &[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("E_DISRUPTIVE_UNAPPROVED")
            || text(&o.stdout).contains("--allow-disruptive")
    );
    assert!(fs::symlink_metadata(fx.h(".zshrc")).is_ok());
    let mut args = Vec::new();
    for r in &removals {
        args.push("--allow-disruptive");
        args.push(r.as_str());
    }
    let o = fx.apply_with(&plan.plan_digest, &args);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(fs::symlink_metadata(fx.h(".zshrc")).is_err());
    assert!(fs::symlink_metadata(fx.h(".config/tmux/tmux.conf")).is_err());
    assert!(fs::symlink_metadata(fx.h(".ssh/config")).is_ok());
}

#[test]
fn an_adoption_without_its_flag_is_refused_before_any_write_and_a_preview_lists_the_flag() {
    let fx = Fx::new();
    fx.stow();
    let ids = fx.adopt_ids();
    let before = fingerprint(fx.home.path());
    let note = fx.lock_note();
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_ADOPT_UNAPPROVED"), "{}", stderr(&o));
    assert_eq!(fx.lock_note(), note, "refused before the lock");
    assert_eq!(fingerprint(fx.home.path()), before);
    // A preview prints every missing flag, ready to paste, and exits 3.
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_ADOPT_UNAPPROVED"));
    let out = text(&o.stdout);
    for id in &ids {
        assert!(out.contains(&format!("--allow-adopt '{id}'")), "{out}");
    }
    assert_eq!(fx.lock_note(), note);
    // With every flag the preview passes.
    let mut a = vec!["apply", "--root", fx.root_arg(), "--preview"];
    a.extend(adopt_args(&ids));
    let o = fx.run(&a);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.lock_note(), note);
    assert_eq!(fingerprint(fx.home.path()), before);
    // An id that names no adoption is a usage error, also before the lock.
    let o = fx.apply(&["--allow-adopt", "nope@x"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("names no adoption"), "{}", stderr(&o));
    assert_eq!(fx.lock_note(), note);
    // A removal id is not an adoption id.
    let o = fx.apply(&["--allow-adopt", "remove@x@y"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    // The same holds across kinds: a wrong disruptive id wins over a missing adoption.
    let o = fx.apply(&["--allow-disruptive", ids[0].as_str()]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("names no removal"), "{}", stderr(&o));
}

#[test]
fn a_link_with_other_text_and_a_regular_file_stay_blocked() {
    let fx = Fx::new();
    let note = fx.lock_note();
    fs::create_dir_all(fx.h(".config/git")).unwrap();
    symlink("/elsewhere", fx.h(".config/git/config")).unwrap();
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(is_link_to(
        &fx.h(".config/git/config"),
        Path::new("/elsewhere")
    ));
    fs::remove_file(fx.h(".config/git/config")).unwrap();
    // A file with the very content of the source is still not the link.
    fs::write(fx.h(".config/git/config"), "[user]\n").unwrap();
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    let e = stderr(&o);
    assert!(e.contains("E_CONFLICT"), "{e}");
    assert!(e.contains("is a regular file, not the link"), "{e}");
    assert!(e.contains("move the file aside"), "{e}");
    assert_eq!(fs::read(fx.h(".config/git/config")).unwrap(), b"[user]\n");
    assert!(fx.owned_paths().is_empty());
    assert_eq!(fx.lock_note(), note);
}

#[test]
fn a_leaf_swapped_after_the_plan_is_not_adopted_and_no_ledger_entry_is_written() {
    for point in ["apply-before-effect", "apply-after-effect"] {
        let fx = Fx::new();
        // Only the zsh link exists, so it is the one adoption and the first operation.
        symlink(
            fx.root.join("base/modules/extra/files/zshrc"),
            fx.h(".zshrc"),
        )
        .unwrap();
        let ids = fx.adopt_ids();
        assert_eq!(ids, ["extra.zsh@.zshrc"]);
        let o = parked_apply_with(&fx, point, &["--allow-adopt", &ids[0]], |fx| {
            fs::remove_file(fx.h(".zshrc")).unwrap();
            symlink("/mine", fx.h(".zshrc")).unwrap();
        });
        assert_eq!(code(&o), 1, "{point}: {}\n{}", stderr(&o), text(&o.stdout));
        let out = text(&o.stdout);
        assert!(out.contains("failed extra.zsh@.zshrc"), "{point}: {out}");
        assert!(out.contains("E_CONFLICT"), "{point}: {out}");
        assert!(is_link_to(&fx.h(".zshrc"), Path::new("/mine")), "{point}");
        assert!(fx.owned_paths().is_empty(), "{point}: no ledger entry");
        let j = fx.journal();
        assert_eq!(
            j.iter()
                .map(|e| e["phase"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["intent", "failed"],
            "{point}"
        );
        // The links the swap did not touch were not created: the run stopped at the failure.
        assert!(fs::symlink_metadata(fx.h(".ssh")).is_err(), "{point}");
        // The attempt reached its intent, so the receipt records exactly its one approval.
        let approvals = fx.receipts()[0]["approvals"].as_array().unwrap().clone();
        assert_eq!(approvals.len(), 1, "{point}");
        assert_eq!(approvals[0]["how"], "allow_adopt", "{point}");
    }
}

#[test]
fn a_crash_during_an_adoption_is_settled_and_the_rerun_records_the_link() {
    // `apply-after-effect` leaves an intent entry that the reconcile probe settles as done.
    // `apply-after-done` leaves a done entry and no ledger entry.
    for point in ["apply-after-effect", "apply-after-done"] {
        let fx = Fx::new();
        let zsh = fx.root.join("base/modules/extra/files/zshrc");
        symlink(&zsh, fx.h(".zshrc")).unwrap();
        let digest = fx.digest();
        let o = Command::new(BIN)
            .env_clear()
            .env("DFM_FAILPOINT", format!("{point}:abort"))
            .arg("--home")
            .arg(fx.home.path())
            .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
            .args(["--allow-adopt", "extra.zsh@.zshrc"])
            .output()
            .unwrap();
        assert!(!o.status.success(), "{point}");
        assert!(fx.owned_paths().is_empty(), "{point}: nothing recorded yet");
        let first: Vec<_> = fx.journal();
        let id = first[0]["op_id"].as_str().unwrap().to_string();
        let o = fx.apply(&["--allow-adopt", "extra.zsh@.zshrc"]);
        assert_eq!(code(&o), 0, "{point}: {}\n{}", stderr(&o), text(&o.stdout));
        let j = fx.journal();
        assert!(
            j.iter()
                .any(|e| e["op_id"] == id.as_str() && e["phase"] == "done"),
            "{point}: the interrupted attempt ends done: {j:#?}"
        );
        assert!(
            j.iter()
                .all(|e| e["op"] != "adopt" || e["phase"] != "failed")
        );
        assert!(fx.owned_paths().contains(&".zshrc".to_string()), "{point}");
        assert!(is_link_to(&fx.h(".zshrc"), &zsh), "{point}");
    }
}

#[test]
fn an_adopted_link_replaced_by_a_file_with_the_link_text_as_bytes_is_not_removed() {
    let fx = Fx::new();
    fx.stow();
    let ids = fx.adopt_ids();
    assert_eq!(code(&fx.apply(&adopt_args(&ids))), 0);
    // A regular file whose bytes are exactly the link text: a plain SHA-256 of it would equal
    // the old link fingerprint.
    let zsh = fx.root.join("base/modules/extra/files/zshrc");
    fs::remove_file(fx.h(".zshrc")).unwrap();
    fs::write(fx.h(".zshrc"), zsh.to_str().unwrap()).unwrap();
    fx.set_profile("");
    let plan = fx.saved();
    let removal = "remove@extra.zsh@.zshrc";
    let args = ["--allow-disruptive", removal];
    // The plan itself refuses: the file is not what the ledger recorded.
    let o = fx.apply_with(&plan.plan_digest, &args);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(
        fs::read(fx.h(".zshrc")).unwrap(),
        zsh.to_str().unwrap().as_bytes()
    );
    assert!(fx.owned_paths().contains(&".zshrc".to_string()));
}

// ---- signals (DFM-47) -------------------------------------------------------------------

/// Sends `signal` (`-INT` or `-TERM`) to `pid`. The test crate has no `unsafe`, so it uses kill(1).
fn send(signal: &str, pid: u32) {
    let st = Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .status()
        .unwrap();
    assert!(st.success(), "kill {signal} {pid} failed");
}

/// Parks `apply` before the effect of its first operation, signals it (twice when `twice`), and
/// checks everything the contract promises: the parked operation finished, the rest did not
/// start, the receipt and the exit code name the signal.
fn signal_during_apply(signal: &str, twice: bool, exit: i32, name: &str) {
    let fx = Fx::new();
    fx.without_zsh();
    // Every parent exists, so the first operation is the creation of the git link.
    fs::create_dir_all(fx.h(".config/git")).unwrap();
    fs::create_dir_all(fx.h(".config/tmux")).unwrap();
    fs::create_dir_all(fx.h(".ssh")).unwrap();
    let o = parked_apply_pid(&fx, "apply-before-effect", &[], |_, pid| {
        send(signal, pid);
        if twice {
            send(signal, pid);
        }
    });
    let out = text(&o.stdout);
    assert_eq!(code(&o), exit, "{}\n{out}", stderr(&o));
    // Not killed by the signal: the process chose its exit.
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&o.status),
        None
    );
    assert!(out.contains("updated core.git@.config/git/config"), "{out}");
    assert!(
        out.contains("counts: updated=1 current=0 failed=0 blocked=0 unknown=0 not_started=2"),
        "{out}"
    );
    assert!(out.contains(&format!("result: {name}")), "{out}");
    assert!(is_link_to(
        &fx.h(".config/git/config"),
        &fx.root.join("base/modules/core/files/gitconfig")
    ));
    assert!(!fx.h(".config/tmux/tmux.conf").exists() && !fx.h(".ssh/config").exists());
    // Journal: one intent and its done entry. Nothing for the two operations that did not start.
    let j = fx.journal();
    let phases: Vec<_> = j.iter().map(|e| e["phase"].as_str().unwrap()).collect();
    assert_eq!(phases, ["intent", "done"], "{j:#?}");
    assert_eq!(fx.owned_paths(), [".config/git/config"]);
    let rs = fx.receipts();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0]["result"]["outcome"], name);
    assert_eq!(rs[0]["result"]["exit_code"], exit);
    let counts = &rs[0]["result"]["counts"];
    assert_eq!(
        (counts["updated"].as_u64(), counts["not_started"].as_u64()),
        (Some(1), Some(2))
    );
}

#[test]
fn sigint_finishes_the_running_operation_writes_the_receipt_and_exits_130() {
    signal_during_apply("-INT", false, 130, "interrupted");
}

#[test]
fn sigterm_finishes_the_running_operation_writes_the_receipt_and_exits_143() {
    signal_during_apply("-TERM", false, 143, "terminated");
}

#[test]
fn a_second_signal_during_shutdown_changes_nothing() {
    signal_during_apply("-INT", true, 130, "interrupted");
}

#[test]
fn the_first_signal_keeps_its_code_when_another_kind_follows() {
    let fx = Fx::new();
    fx.without_zsh();
    fs::create_dir_all(fx.h(".config/git")).unwrap();
    fs::create_dir_all(fx.h(".config/tmux")).unwrap();
    fs::create_dir_all(fx.h(".ssh")).unwrap();
    let o = parked_apply_pid(&fx, "apply-before-effect", &[], |_, pid| {
        send("-TERM", pid);
        send("-INT", pid);
    });
    assert_eq!(code(&o), 143, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.receipts()[0]["result"]["outcome"], "terminated");
}

/// A fixture whose three operations are all creations in existing parents.
fn signal_fixture() -> Fx {
    let fx = Fx::new();
    fx.without_zsh();
    for d in [".config/git", ".config/tmux", ".ssh"] {
        fs::create_dir_all(fx.h(d)).unwrap();
    }
    fx
}

fn journal_phases(fx: &Fx) -> Vec<String> {
    fs::read_to_string(fx.state().join("journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["phase"].as_str().unwrap().to_string()
        })
        .collect()
}

#[test]
fn a_signal_before_the_first_check_changes_nothing_writes_no_receipt_and_exits_130() {
    let fx = signal_fixture();
    let o = parked_apply_pid(&fx, "apply-after-install", &[], |_, pid| send("-INT", pid));
    let out = text(&o.stdout);
    assert_eq!(code(&o), 130, "{}\n{out}", stderr(&o));
    assert!(out.contains("result: interrupted"), "{out}");
    assert!(
        out.contains("counts: updated=0 current=0 failed=0 blocked=0 unknown=0 not_started=3"),
        "{out}"
    );
    assert!(journal_phases(&fx).is_empty());
    assert!(fx.receipts().is_empty());
    assert!(fx.owned_paths().is_empty());
    assert!(!fx.h(".config/git/config").exists());
}

#[test]
fn a_signal_during_a_removal_lets_it_finish_and_leaves_the_other_removal_alone() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply(&[])), 0);
    fx.set_profile("");
    let removals: Vec<String> = fx
        .saved()
        .plan
        .operations
        .iter()
        .filter(|o| o.id.starts_with("remove@"))
        .map(|o| o.id.clone())
        .collect();
    assert_eq!(removals.len(), 2, "{removals:?}");
    let mut extra = Vec::new();
    for r in &removals {
        extra.push("--allow-disruptive");
        extra.push(r.as_str());
    }
    let before = fx.journal().len();
    let o = parked_apply_pid(&fx, "apply-before-unlink", &extra, |_, pid| {
        send("-TERM", pid)
    });
    let out = text(&o.stdout);
    assert_eq!(code(&o), 143, "{}\n{out}", stderr(&o));
    assert!(out.contains("result: terminated"), "{out}");
    // Exactly one removal ran: its intent and done entries, and one ledger entry is gone.
    assert_eq!(journal_phases(&fx).len() - before, 2, "{:#?}", fx.journal());
    assert_eq!(fx.owned_paths().len(), 3);
    let receipts = fx.receipts();
    let counts = &receipts.last().unwrap()["result"]["counts"];
    assert_eq!(counts["updated"], 1);
    assert_eq!(counts["not_started"], 1);
}

#[test]
fn a_signal_after_an_earlier_failure_still_exits_130_and_the_receipt_lists_the_failure() {
    let fx = signal_fixture();
    let o = parked_apply_pid(&fx, "apply-before-effect", &[], |fx, pid| {
        fs::write(fx.h(".config/git/config"), "foreign\n").unwrap();
        send("-INT", pid);
    });
    let out = text(&o.stdout);
    assert_eq!(code(&o), 130, "{}\n{out}", stderr(&o));
    assert!(out.contains("failed core.git@.config/git/config"), "{out}");
    assert!(out.contains("result: interrupted"), "{out}");
    let rs = fx.receipts();
    assert_eq!(rs[0]["result"]["outcome"], "interrupted");
    assert_eq!(rs[0]["result"]["counts"]["failed"], 1);
    assert_eq!(rs[0]["result"]["counts"]["not_started"], 2);
}

#[test]
fn a_signal_during_the_receipt_write_still_decides_the_exit() {
    for point in [
        "receipt-before-write",
        "receipt-after-tmp",
        "receipt-after-rename",
    ] {
        let fx = signal_fixture();
        let o = parked_apply_pid(&fx, point, &[], |_, pid| send("-INT", pid));
        let out = text(&o.stdout);
        assert_eq!(code(&o), 130, "{point}: {}\n{out}", stderr(&o));
        assert!(out.contains("result: interrupted"), "{point}: {out}");
        assert!(
            out.contains("signal: E_INTERRUPTED arrived after"),
            "{point}: {out}"
        );
        // The receipt was written before the signal could be added to it, and says so.
        assert_eq!(fx.receipts()[0]["result"]["outcome"], "ok", "{point}");
    }
}

#[test]
fn a_signal_wins_the_exit_when_the_receipt_cannot_be_written() {
    let fx = signal_fixture();
    let receipts = fx.state().join("receipts");
    let o = parked_apply_pid(&fx, "apply-before-effect", &[], |_, pid| {
        fs::set_permissions(&receipts, fs::Permissions::from_mode(0o500)).unwrap();
        send("-INT", pid);
    });
    fs::set_permissions(&receipts, fs::Permissions::from_mode(0o700)).unwrap();
    let out = text(&o.stdout);
    assert_eq!(code(&o), 130, "{}\n{out}", stderr(&o));
    assert!(out.contains("receipt: not written"), "{out}");
    assert!(out.contains("result: interrupted"), "{out}");
    assert!(fx.receipts().is_empty());
}
