//! `render` and `assemble` through the isolated binary: plan, apply and rerun (DFM-20).
//!
//! Every run passes `--home <temp dir>` and an empty environment. No test touches the real HOME.

mod common;

use common::{BIN, TempDir, code, copy_tree, fingerprint, repo_root, text};
use dfm_model::plan::Saved;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
}

const BUNDLE: &str = r#"schema = 1
id = "t-base"
core = "modules/core/module.toml"

[modules]
agents = "modules/agents/module.toml"
frag = "modules/frag/module.toml"
"#;

/// Two values for one key (order matters to Git), a plain table and a subsection.
const CORE: &str = r#"schema = 1
id = "core"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "git-defaults"
kind = "render"
adapter = "git-config/v1"
target = ".config/git/defaults.inc"
[resources.value.core]
editor = "nvim"
[resources.value.credential]
helper = ["cache", "store"]
[resources.value.url."ssh://git@example.test/"]
insteadOf = "https://example.test/"
[resources.policy]
defaults = ["/core/editor", "/credential/helper", "/url"]
extensions = []
"#;

const AGENTS: &str = r#"schema = 1
id = "agents"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "instructions"
kind = "assemble"
adapter = "text-fragments/v1"
target = ".codex/AGENTS.md"
[resources.value]
body = "files/AGENTS.md"
fragments_dir = ".codex/AGENTS.d"
pattern = "*.md"
separator = "\n"
"#;

/// A module whose rendered file lands in the fragment directory: a selected producer.
const FRAG: &str = r#"schema = 1
id = "frag"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "git-note"
kind = "render"
adapter = "git-config/v1"
target = ".codex/AGENTS.d/50-git.md"
[resources.value.core]
pager = "less"
[resources.policy]
defaults = ["/core/pager"]
extensions = []

[[resources]]
id = "note-link"
kind = "link"
adapter = "fs-link/v1"
target = ".codex/AGENTS.d/60-link.md"
[resources.value]
source = "files/note.md"
"#;

const BODY: &str = "# Body\n";

fn profile(modules: &str) -> String {
    format!(
        "schema = 1\nid = \"t-01\"\nbundle = \"../base/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [{modules}]\noverrides = []\n"
    )
}

impl Fx {
    fn new(modules: &str) -> Fx {
        let src = TempDir::new("native-src");
        let root = src.path().join("root");
        let fx = Fx {
            home: TempDir::new("native-home"),
            _src: src,
            root,
        };
        fx.put("base/bundle.toml", BUNDLE);
        fx.put("base/modules/core/module.toml", CORE);
        fx.put("base/modules/agents/module.toml", AGENTS);
        fx.put("base/modules/agents/files/AGENTS.md", BODY);
        fx.put("base/modules/frag/module.toml", FRAG);
        fx.put("base/modules/frag/files/note.md", "linked\n");
        fx.put("profiles/t-01.toml", &profile(modules));
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

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn saved(&self) -> Saved {
        let o = self.run(&["plan", "--root", self.root_arg(), "--json"]);
        Saved::parse(&o.stdout).unwrap_or_else(|e| panic!("{e}\n{}", text(&o.stdout)))
    }

    fn apply_with(&self, digest: &str, extra: &[&str]) -> Output {
        let mut a = vec!["apply", "--root", self.root_arg(), "--yes-plan", digest];
        a.extend_from_slice(extra);
        self.run(&a)
    }

    fn apply(&self, extra: &[&str]) -> Output {
        self.apply_with(&self.saved().plan_digest, extra)
    }

    fn receipts(&self) -> usize {
        fs::read_dir(self.h(".local/state/dotfiles-manager/state/receipts"))
            .map_or(0, Iterator::count)
    }

    fn fragments(&self, list: &[(&str, &[u8])]) {
        fs::create_dir_all(self.h(".codex/AGENTS.d")).unwrap();
        for (n, b) in list {
            fs::write(self.h(".codex/AGENTS.d").join(n), b).unwrap();
        }
    }
}

fn stderr(o: &Output) -> String {
    text(&o.stderr)
}

/// Names, modes, modification times and bytes below `root`, without the lock file: every
/// mutating command rewrites its holder note, also when it changes nothing else.
fn without_lock(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in fs::read_dir(root).unwrap() {
        let p = e.unwrap().path();
        if p.file_name().is_some_and(|n| n == "dotfiles-manager.lock") {
            continue;
        }
        let m = fs::symlink_metadata(&p).unwrap();
        out.push(format!("{} {:?}", p.display(), m.modified().unwrap()));
        if m.is_dir() {
            out.extend(without_lock(&p));
        }
    }
    out
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

const DEFAULTS: &str = "[core]\n\teditor = \"nvim\"\n[credential]\n\thelper = \"cache\"\n\thelper = \"store\"\n[url \"ssh://git@example.test/\"]\n\tinsteadOf = \"https://example.test/\"\n";

#[test]
fn render_and_assemble_publish_what_the_plan_hashed_and_a_rerun_changes_nothing() {
    let fx = Fx::new("\"agents\"");
    // A foreign fragment (not produced by any selected resource) and an unmatched file.
    fx.fragments(&[
        ("a-last.md", b"two\n"),
        ("Z-first.md", b"one\n"),
        ("note.txt", b"x"),
    ]);
    let plan = fx.saved();
    let o = fx.apply_with(&plan.plan_digest, &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));

    let defaults = fs::read(fx.h(".config/git/defaults.inc")).unwrap();
    assert_eq!(text(&defaults), DEFAULTS);
    // Byte order: uppercase before lowercase. Body first. The separator joins the parts.
    let agents = fs::read(fx.h(".codex/AGENTS.md")).unwrap();
    assert_eq!(text(&agents), "# Body\n\none\n\ntwo\n");
    // The plan recorded the hash of exactly these bytes before apply wrote them.
    let hashes: Vec<(String, String)> = plan
        .plan
        .operations
        .iter()
        .filter_map(|o| Some((o.path.clone()?, o.output_sha256.clone()?)))
        .collect();
    assert!(hashes.contains(&(".config/git/defaults.inc".into(), sha(&defaults))));
    assert!(hashes.contains(&(".codex/AGENTS.md".into(), sha(&agents))));
    assert_eq!(hashes.len(), 2, "{hashes:?}");

    // Rerun: every operation is current, nothing is written, no receipt is added.
    let receipts = fx.receipts();
    let before = without_lock(fx.home.path());
    let again = fx.saved();
    assert!(
        again
            .plan
            .operations
            .iter()
            .all(|o| o.status != dfm_model::plan::Status::Planned),
        "{:?}",
        again.plan.operations
    );
    let o = fx.apply_with(&again.plan_digest, &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fx.receipts(), receipts);
    assert_eq!(without_lock(fx.home.path()), before);
}

#[test]
fn selected_producers_come_first_and_deselected_ones_leave_in_one_apply() {
    let fx = Fx::new("\"agents\", \"frag\"");
    fx.fragments(&[("10-a.md", b"a\n"), ("90-z.md", b"z\n")]);
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(
        text(&fs::read(fx.h(".codex/AGENTS.md")).unwrap()),
        "# Body\n\na\n\n[core]\n\tpager = \"less\"\n\nlinked\n\nz\n"
    );

    // Deselect the producer. The removal and the new assembly happen in one apply.
    fs::write(fx.root.join("profiles/t-01.toml"), profile("\"agents\"")).unwrap();
    let plan = fx.saved();
    let removals: Vec<String> = plan
        .plan
        .operations
        .iter()
        .filter(|o| o.id.starts_with("remove@"))
        .map(|o| o.id.clone())
        .collect();
    assert_eq!(removals.len(), 2, "{removals:?}");
    let o = fx.apply_with(
        &plan.plan_digest,
        &[
            "--allow-disruptive",
            &removals[0],
            "--allow-disruptive",
            &removals[1],
        ],
    );
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(!fx.h(".codex/AGENTS.d/50-git.md").exists());
    assert!(fs::symlink_metadata(fx.h(".codex/AGENTS.d/60-link.md")).is_err());
    assert_eq!(
        text(&fs::read(fx.h(".codex/AGENTS.md")).unwrap()),
        "# Body\n\na\n\nz\n"
    );
    // Converged: the next plan has nothing to do.
    let next = fx.saved();
    assert!(
        next.plan
            .operations
            .iter()
            .all(|o| o.status != dfm_model::plan::Status::Planned),
        "{:?}",
        next.plan.operations
    );
}

#[test]
fn a_malformed_fragment_blocks_the_plan_and_keeps_the_last_good_output() {
    let fx = Fx::new("\"agents\"");
    fx.fragments(&[("10-a.md", b"a\n")]);
    assert_eq!(code(&fx.apply(&[])), 0);
    let good = fs::read(fx.h(".codex/AGENTS.md")).unwrap();

    type Plant = fn(&Fx);
    let cases: [(&str, Plant); 4] = [
        ("not UTF-8", |fx| {
            fs::write(fx.h(".codex/AGENTS.d/20-bad.md"), [0xff, 0xfe, b'\n']).unwrap();
        }),
        ("oversize", |fx| {
            fs::write(
                fx.h(".codex/AGENTS.d/20-big.md"),
                vec![b'x'; (16 << 20) + 1],
            )
            .unwrap();
        }),
        ("link", |fx| {
            fs::write(fx.h("elsewhere.md"), "outside\n").unwrap();
            symlink(fx.h("elsewhere.md"), fx.h(".codex/AGENTS.d/20-link.md")).unwrap();
        }),
        ("fifo", |fx| {
            let st = Command::new("mkfifo")
                .arg(fx.h(".codex/AGENTS.d/20-fifo.md"))
                .status()
                .unwrap();
            assert!(st.success());
        }),
    ];
    for (what, plant) in cases {
        plant(&fx);
        let before = fingerprint(fx.home.path());
        // The plan itself is blocked, so the digest is that of a blocked plan.
        let o = fx.apply(&[]);
        assert_eq!(code(&o), 3, "{what}: {}\n{}", stderr(&o), text(&o.stdout));
        assert_eq!(
            fingerprint(fx.home.path()),
            before,
            "{what}: nothing written"
        );
        assert_eq!(
            fs::read(fx.h(".codex/AGENTS.md")).unwrap(),
            good,
            "{what}: last good output kept"
        );
        // Remove it again for the next case.
        let d = fx.h(".codex/AGENTS.d");
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.file_name().unwrap() != "10-a.md" {
                fs::remove_file(p).unwrap();
            }
        }
    }
    // The non-UTF-8 case is a generator refusal, not a planner one: it names the fragment.
    fs::write(fx.h(".codex/AGENTS.d/20-bad.md"), [0xff]).unwrap();
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 3);
    assert!(
        stderr(&o).contains("20-bad.md is not UTF-8 text"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_fragment_or_body_changed_between_plan_and_apply_is_stale_and_writes_nothing() {
    let fx = Fx::new("\"agents\"");
    fx.fragments(&[("10-a.md", b"a\n"), ("20-b.md", b"b\n")]);
    assert_eq!(code(&fx.apply(&[])), 0);
    let good = fs::read(fx.h(".codex/AGENTS.md")).unwrap();
    type Change = fn(&Fx);
    let cases: [(&str, Change); 4] = [
        ("fragment disappeared", |fx| {
            fs::remove_file(fx.h(".codex/AGENTS.d/20-b.md")).unwrap();
        }),
        ("fragment edited", |fx| {
            fs::write(fx.h(".codex/AGENTS.d/20-b.md"), "B\n").unwrap();
        }),
        ("fragment appeared", |fx| {
            fs::write(fx.h(".codex/AGENTS.d/30-c.md"), "c\n").unwrap();
        }),
        ("body substituted", |fx| {
            fx.put("base/modules/agents/files/AGENTS.md", "# Other\n");
        }),
    ];
    for (what, change) in cases {
        let approved = fx.saved().plan_digest;
        let before = fingerprint(fx.home.path());
        change(&fx);
        let o = fx.apply_with(&approved, &[]);
        assert_eq!(code(&o), 3, "{what}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("E_PLAN_STALE"),
            "{what}: {}",
            stderr(&o)
        );
        assert_eq!(fs::read(fx.h(".codex/AGENTS.md")).unwrap(), good, "{what}");
        // The manager wrote nothing: HOME differs from before only by the planted change.
        assert!(
            what == "body substituted" || fingerprint(fx.home.path()) != before,
            "{what}"
        );
        // Restore for the next case.
        fs::write(fx.h(".codex/AGENTS.d/20-b.md"), "b\n").unwrap();
        let _ = fs::remove_file(fx.h(".codex/AGENTS.d/30-c.md"));
        fx.put("base/modules/agents/files/AGENTS.md", BODY);
    }
}

#[test]
fn the_example_profile_applies_once_the_sensitive_merge_is_left_out() {
    let fx = example();
    let p = fx.root.join("base/modules/agents/module.toml");
    let m = fs::read_to_string(&p).unwrap();
    let (a, b) = (
        m.find("[[resources]]\nid = \"claude-settings\"").unwrap(),
        m.find("[[resources]]\nid = \"codex-instructions\"")
            .unwrap(),
    );
    fs::write(&p, format!("{}{}", &m[..a], &m[b..])).unwrap();
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(
        text(&fs::read(fx.h(".config/external-overlays/git/base.inc")).unwrap()),
        "[core]\n\teditor = \"nvim\"\n[fetch]\n\tprune = true\n[init]\n\tdefaultBranch = \"main\"\n"
    );
    assert_eq!(
        fs::read(fx.h(".codex/AGENTS.md")).unwrap(),
        fs::read(repo_root().join("examples/base/modules/agents/files/AGENTS.md")).unwrap()
    );
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
}

fn example() -> Fx {
    let src = TempDir::new("native-example");
    let root = src.path().join("examples");
    copy_tree(&repo_root().join("examples"), &root);
    let fx = Fx {
        home: TempDir::new("native-example-home"),
        _src: src,
        root,
    };
    let p = fx.root.join("profiles/personal-dev-01.toml");
    let o = fx.run(&[
        "bind",
        "--profile",
        p.to_str().unwrap(),
        "--machine-id",
        "m5",
    ]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    fx
}

#[test]
fn a_new_setting_name_is_visible_in_the_plan_before_approval_and_its_value_is_not() {
    let fx = Fx::new("\"agents\"");
    let m = fs::read_to_string(fx.root.join("base/modules/core/module.toml")).unwrap();
    fx.put(
        "base/modules/core/module.toml",
        &m.replace(
            "editor = \"nvim\"",
            "editor = \"nvim\"\nsshCommand = \"ssh -i /home/me/.ssh/key\"",
        ),
    );
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = text(&o.stdout);
    assert!(
        out.contains("sets: core.editor, core.sshCommand, credential.helper, url.ssh://git@example.test/.insteadOf"),
        "{out}"
    );
    assert!(out.contains(" bytes"), "{out}");
    assert!(
        !out.contains("/home/me/.ssh/key"),
        "no value in the plan output"
    );
    let op = fx
        .saved()
        .plan
        .operations
        .into_iter()
        .find(|o| o.resource == "core.git-defaults" && o.output_sha256.is_some())
        .unwrap();
    assert!(
        op.sets.contains(&"core.sshCommand".to_string()),
        "{:?}",
        op.sets
    );
    assert!(op.output_bytes.is_some_and(|n| n > 0));
}

#[test]
fn a_consumer_of_a_blocked_producer_reports_the_producers_blocker() {
    let fx = Fx::new("\"agents\", \"frag\"");
    let m = fs::read_to_string(fx.root.join("base/modules/frag/module.toml")).unwrap();
    fx.put(
        "base/modules/frag/module.toml",
        &m.replace("pager = \"less\"", "pager = \"a\\u0001b\""),
    );
    fx.fragments(&[]);
    let blockers = fx.saved().plan.blockers;
    let producer = blockers
        .iter()
        .find(|b| b.resource == "frag.git-note")
        .unwrap();
    let consumer = blockers
        .iter()
        .find(|b| b.resource == "agents.instructions")
        .unwrap();
    assert_eq!(producer.code.as_str(), "E_MANIFEST_INVALID");
    assert_eq!(consumer.code, producer.code, "{consumer:?}");
    assert!(
        consumer.detail.contains("depends on a blocked producer")
            && consumer.detail.contains("control character"),
        "{}",
        consumer.detail
    );
}

#[test]
fn fragments_that_are_each_small_enough_but_too_big_together_block_the_plan() {
    let fx = Fx::new("\"agents\"");
    let big = vec![b'x'; 4 << 20];
    let names = ["0.md", "1.md", "2.md", "3.md", "4.md"];
    let list: Vec<(&str, &[u8])> = names.iter().map(|n| (*n, big.as_slice())).collect();
    fx.fragments(&list);
    let blocker = fx
        .saved()
        .plan
        .blockers
        .into_iter()
        .find(|b| b.resource == "agents.instructions")
        .unwrap();
    assert_eq!(blocker.code.as_str(), "E_LIMIT_EXCEEDED", "{blocker:?}");
    let before = fingerprint(fx.home.path());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
}

#[test]
fn a_hardlinked_source_blocks_the_plan_with_a_source_code_until_the_second_link_goes() {
    let fx = Fx::new("\"agents\"");
    let body = fx.root.join("base/modules/agents/files/AGENTS.md");
    let second = fx.root.join("second-name");
    fs::hard_link(&body, &second).unwrap();
    let before = fingerprint(fx.home.path());
    let blocker = fx
        .saved()
        .plan
        .blockers
        .into_iter()
        .find(|b| b.resource == "agents.instructions")
        .expect("blocked");
    assert_eq!(blocker.code.as_str(), "E_SOURCE_UNTRUSTED", "{blocker:?}");
    assert!(blocker.detail.contains("hardlinked"), "{}", blocker.detail);
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert_eq!(fingerprint(fx.home.path()), before);
    // With one name left, the plan is clean and applies.
    fs::remove_file(&second).unwrap();
    assert!(fx.saved().plan.blockers.is_empty());
    let o = fx.apply(&[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}
