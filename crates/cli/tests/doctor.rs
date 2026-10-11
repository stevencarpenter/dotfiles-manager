//! `doctor` and `status` through the isolated binary: row statuses and codes, independent rows,
//! table and JSON agreement, narrow output, the exit rule and non-mutation (DFM-35).
//!
//! The fixture mirrors tests/apply.rs (a synthetic link-only source tree). No test touches the
//! real HOME. Every run passes `--home <temp dir>` and an empty environment.

mod common;

use common::TempDir;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

const PROFILE: &str = "schema = 1\nid = \"t-01\"\nbundle = \"../base/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [\"extra\"]\noverrides = []\n";

impl Fx {
    /// A bound machine whose profile selects the `extra` module. Nothing is applied yet.
    fn new() -> Fx {
        let fx = Fx {
            home: TempDir::new("doctor-home"),
            _src: TempDir::new("doctor-src"),
            root: PathBuf::new(),
        };
        let root = fx._src.path().join("root");
        let fx = Fx { root, ..fx };
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
        fx.put("profiles/t-01.toml", PROFILE);
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

    /// As [`Fx::new`], and every link applied.
    fn applied() -> Fx {
        let fx = Fx::new();
        let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
        let digest = dfm_model::plan::Saved::parse(&o.stdout)
            .unwrap()
            .plan_digest;
        let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
        assert_eq!(code(&o), 0, "{}\n{}", text(&o.stderr), text(&o.stdout));
        fx
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        Command::new(BIN)
            .env_clear()
            .envs(env.iter().copied())
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }

    fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn state(&self) -> PathBuf {
        self.h(".local/state/dotfiles-manager/state")
    }

    fn doctor(&self, extra: &[&str]) -> Output {
        let mut a = vec!["doctor", "--root", self.root_arg()];
        a.extend_from_slice(extra);
        self.run(&a)
    }

    /// The rows of `doctor --json`, and the exit code.
    fn rows(&self) -> (i32, Vec<Value>) {
        let o = self.doctor(&["--json"]);
        let v: Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("{e}\n{}\n{}", text(&o.stdout), text(&o.stderr)));
        assert_eq!(v["schema"], 1);
        (code(&o), v["rows"].as_array().unwrap().clone())
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

/// The row whose subject ends with `subject`.
fn at<'a>(rows: &'a [Value], subject: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["subject"].as_str().unwrap().ends_with(subject))
        .unwrap_or_else(|| panic!("no row for {subject} in {rows:#?}"))
}

fn status<'a>(rows: &'a [Value], subject: &str) -> (&'a str, &'a str) {
    let r = at(rows, subject);
    (r["status"].as_str().unwrap(), r["code"].as_str().unwrap())
}

/// Names, kinds, modes, modification times and bytes of everything below `root`, links unread.
/// The lock file is included: doctor must not create or touch it.
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

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| text(&o.stdout).trim() == "0")
}

// ---- healthy state ------------------------------------------------------------------------

#[test]
fn an_applied_machine_is_healthy_and_every_owned_output_is_ok() {
    let fx = Fx::applied();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    for p in [
        ".zshrc",
        ".ssh/config",
        ".config/git/config",
        ".config/tmux/tmux.conf",
    ] {
        assert_eq!(status(&rows, p), ("OK", "D_OK"), "{p}");
        assert!(
            at(&rows, p)["detail"]
                .as_str()
                .unwrap()
                .contains("owned by")
        );
    }
    assert_eq!(status(&rows, "ledger.json").0, "OK");
    assert_eq!(status(&rows, "journal.jsonl").0, "OK");
    assert_eq!(rows.iter().filter(|r| r["kind"] == "module").count(), 2);
    let r = fx.doctor(&[]);
    assert_eq!(code(&r), 0, "{}", text(&r.stderr));
    assert!(text(&r.stdout).contains("summary: result=ok"));
}

#[test]
fn a_foreign_file_that_the_plan_does_not_claim_is_not_a_finding() {
    let fx = Fx::applied();
    fs::write(fx.h(".config/git/local-override"), "[user]\nname = x\n").unwrap();
    fs::write(fx.h(".zshrc.local"), "export B=2\n").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    assert!(rows.iter().all(|r| {
        let s = r["subject"].as_str().unwrap();
        !s.contains("local-override") && !s.contains(".zshrc.local")
    }));
}

#[test]
fn a_fresh_bind_lists_the_planned_outputs_as_pending_warnings_and_exits_0() {
    let fx = Fx::new();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    assert_eq!(status(&rows, ".zshrc"), ("WARN", "D_PLAN_PENDING"));
    let r = fx.doctor(&[]);
    assert!(text(&r.stdout).contains("WARN    D_PLAN_PENDING output"));
    assert_eq!(code(&r), 0);
}

// ---- drift --------------------------------------------------------------------------------

#[test]
fn a_replaced_link_is_a_conflict_and_exits_1() {
    let fx = Fx::applied();
    fs::remove_file(fx.h(".zshrc")).unwrap();
    fs::write(fx.h(".zshrc"), "export EDITED=1\n").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    let r = at(&rows, ".zshrc");
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("ERROR"), Some("E_CONFLICT"))
    );
    assert!(!r["fix"].as_str().unwrap().is_empty());
    assert_eq!(status(&rows, ".ssh/config").0, "OK", "other rows stay");
}

#[test]
fn a_retargeted_link_is_a_conflict() {
    let fx = Fx::applied();
    fs::remove_file(fx.h(".zshrc")).unwrap();
    symlink("/etc/hosts", fx.h(".zshrc")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(status(&rows, ".zshrc"), ("ERROR", "E_CONFLICT"));
}

#[test]
fn a_missing_output_is_a_warning_with_its_fix() {
    let fx = Fx::applied();
    fs::remove_file(fx.h(".zshrc")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "WARN alone exits 0");
    assert_eq!(status(&rows, ".zshrc"), ("WARN", "D_OUTPUT_MISSING"));
    assert!(
        at(&rows, ".zshrc")["fix"]
            .as_str()
            .unwrap()
            .contains("apply")
    );
}

#[test]
fn a_changed_source_makes_the_owned_link_pending() {
    let fx = Fx::applied();
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &m.replace("files/zshrc", "files/zshrc2"),
    );
    fx.put("base/modules/extra/files/zshrc2", "export C=3\n");
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    assert_eq!(status(&rows, ".zshrc"), ("WARN", "D_PLAN_PENDING"));
}

#[test]
fn deselecting_a_module_shows_its_owned_outputs_as_removals() {
    let fx = Fx::applied();
    fx.put("profiles/t-01.toml", &PROFILE.replace("\"extra\"", ""));
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    assert_eq!(status(&rows, ".zshrc"), ("WARN", "D_PLAN_REMOVAL"));
    assert_eq!(status(&rows, ".ssh/config").0, "OK");
}

#[test]
fn a_linked_parent_of_an_owned_output_is_unsafe() {
    let fx = Fx::applied();
    let elsewhere = fx._src.path().join("moved-tmux");
    fs::rename(fx.h(".config/tmux"), &elsewhere).unwrap();
    symlink(&elsewhere, fx.h(".config/tmux")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    let r = at(&rows, ".config/tmux/tmux.conf");
    assert_eq!(r["status"], "ERROR");
    // The planner judges a link that leaves HOME to be a reserved target.
    assert_eq!(r["code"], "E_TARGET_RESERVED");
    assert_eq!(status(&rows, ".ssh/config").0, "OK", "other rows stay");
}

#[test]
fn without_a_plan_a_linked_parent_of_an_owned_output_is_still_found() {
    let fx = Fx::applied();
    fs::remove_file(fx.root.join("profiles/t-01.toml")).unwrap();
    let real = fx.h(".config/tmux-real");
    fs::rename(fx.h(".config/tmux"), &real).unwrap();
    symlink(&real, fx.h(".config/tmux")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    let r = at(&rows, ".config/tmux/tmux.conf");
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("ERROR"), Some("E_PARENT_UNSAFE"))
    );
    assert_eq!(r["partial"], true);
}

#[test]
fn an_unreadable_parent_is_an_unknown_required_row_and_exits_1() {
    if is_root() {
        return;
    }
    let fx = Fx::applied();
    let dir = fx.h(".config/git");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let (c, rows) = fx.rows();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(c, 1, "{rows:#?}");
    let r = at(&rows, ".config/git/config");
    assert!(
        matches!(r["status"].as_str(), Some("UNKNOWN" | "ERROR")),
        "{r:#?}"
    );
    assert_ne!(r["status"], "OK");
}

// ---- interrupted work ---------------------------------------------------------------------

#[test]
fn an_interrupted_intent_is_an_error_row_with_a_recovery_hint() {
    let fx = Fx::new();
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let digest = dfm_model::plan::Saved::parse(&o.stdout)
        .unwrap()
        .plan_digest;
    let o = fx.run_env(
        &[("DFM_FAILPOINT", "apply-before-effect:abort")],
        &["apply", "--root", fx.root_arg(), "--yes-plan", &digest],
    );
    assert!(!o.status.success());
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    let r = rows
        .iter()
        .find(|r| r["kind"] == "intent")
        .expect("intent row");
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("ERROR"), Some("E_OP_INTERRUPTED"))
    );
    assert!(r["fix"].as_str().unwrap().contains("apply"));
    assert!(r["detail"].as_str().unwrap().contains("no outcome"));
    assert_eq!(
        status(&rows, "journal.jsonl").0,
        "OK",
        "the journal itself parses"
    );
}

// ---- independent rows ---------------------------------------------------------------------

#[test]
fn a_ledger_parse_failure_keeps_the_other_rows() {
    let fx = Fx::applied();
    fs::write(fx.state().join("ledger.json"), "{ not json").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(status(&rows, "ledger.json").0, "ERROR");
    assert_eq!(status(&rows, "state.json").0, "OK");
    assert_eq!(status(&rows, "journal.jsonl").0, "OK");
    assert_eq!(
        rows.iter().find(|r| r["kind"] == "binding").unwrap()["status"],
        "OK"
    );
    assert_eq!(
        rows.iter().find(|r| r["kind"] == "profile").unwrap()["status"],
        "OK"
    );
    assert!(
        rows.iter()
            .any(|r| r["kind"] == "receipt" && r["status"] == "OK")
    );
}

#[test]
fn an_unreadable_journal_keeps_the_ledger_and_output_rows() {
    let fx = Fx::applied();
    fs::write(fx.state().join("journal.jsonl"), "garbage\n").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(status(&rows, "journal.jsonl").0, "ERROR");
    assert_eq!(status(&rows, "ledger.json").0, "OK");
    assert_eq!(status(&rows, ".zshrc").0, "OK");
}

#[test]
fn a_missing_profile_keeps_the_state_and_owned_rows_and_labels_them_partial() {
    let fx = Fx::applied();
    fs::remove_file(fx.root.join("profiles/t-01.toml")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1, "{rows:#?}");
    let p = rows.iter().find(|r| r["kind"] == "profile").unwrap();
    assert_eq!(p["status"], "ERROR");
    assert!(
        rows.iter()
            .any(|r| r["kind"] == "plan" && r["status"] == "UNKNOWN")
    );
    assert_eq!(status(&rows, "ledger.json").0, "OK");
    let z = at(&rows, ".zshrc");
    assert_eq!(z["status"], "OK");
    assert_eq!(z["partial"], true, "drift was not assessed without a plan");
}

#[test]
fn a_broken_module_manifest_is_one_error_row() {
    let fx = Fx::applied();
    fx.put("base/modules/extra/module.toml", "schema = 1\nid = ");
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(
        rows.iter().find(|r| r["kind"] == "profile").unwrap()["status"],
        "ERROR"
    );
    assert_eq!(status(&rows, "ledger.json").0, "OK");
}

#[test]
fn an_unreadable_receipt_is_a_partial_warning_not_an_error() {
    let fx = Fx::applied();
    let dir = fx.state().join("receipts");
    let name = fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    fs::write(dir.join(&name), "{}").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    let r = rows
        .iter()
        .find(|r| r["status"] == "WARN" && r["partial"] == true)
        .unwrap();
    assert!(r["subject"].as_str().unwrap().starts_with("receipts/"));
}

#[test]
fn an_unbound_machine_reports_the_binding_and_exits_1() {
    let fx = Fx {
        home: TempDir::new("doctor-unbound"),
        _src: TempDir::new("doctor-unbound-src"),
        root: PathBuf::new(),
    };
    let root = fx._src.path().to_path_buf();
    let fx = Fx { root, ..fx };
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(
        rows.iter().find(|r| r["kind"] == "binding").unwrap()["code"],
        "E_BINDING_INVALID"
    );
    assert!(
        rows.iter()
            .any(|r| r["kind"] == "state" && r["status"] == "N/A")
    );
}

// ---- not applicable -----------------------------------------------------------------------

#[test]
fn sensitive_and_generated_resources_are_not_applicable_and_read_no_content() {
    let home = TempDir::new("doctor-ex-home");
    let src = TempDir::new("doctor-ex-src");
    let root = src.path().join("examples");
    common::copy_tree(&common::repo_root().join("examples"), &root);
    let fx = Fx {
        home,
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
    let (_, rows) = fx.rows();
    let na = |c: &str| rows.iter().filter(|r| r["code"] == c).collect::<Vec<_>>();
    assert!(!na("D_NA_SENSITIVE").is_empty(), "{rows:#?}");
    assert!(!na("D_NA_GENERATED").is_empty(), "{rows:#?}");
    assert!(na("D_NA_SENSITIVE").iter().all(|r| r["status"] == "N/A"));
    assert!(
        na("D_NA_GENERATED")
            .iter()
            .all(|r| r["status"] == "N/A" && r["detail"].as_str().unwrap().contains("no generator"))
    );
    // Tools and applications are not probed: UNKNOWN, and not required.
    let probe = na("D_PROBING_NOT_BUILT");
    assert!(!probe.is_empty());
    assert!(
        probe
            .iter()
            .all(|r| r["status"] == "UNKNOWN" && r["required"] == false)
    );
}

// ---- formats ------------------------------------------------------------------------------

/// `(status, code, kind)` of each row, read from the table.
fn table_rows(table: &str) -> Vec<(String, String, String)> {
    let known = ["OK", "WARN", "ERROR", "UNKNOWN", "N/A"];
    table
        .lines()
        .filter(|l| !l.starts_with(' '))
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let s = w.next()?;
            known
                .contains(&s)
                .then(|| (s.into(), w.next().unwrap().into(), w.next().unwrap().into()))
        })
        .collect()
}

#[test]
fn table_status_and_json_agree_on_every_row_for_doctor_and_status() {
    let fx = Fx::applied();
    fs::remove_file(fx.h(".zshrc")).unwrap();
    fs::remove_file(fx.h(".ssh/config")).unwrap();
    fs::write(fx.h(".ssh/config"), "x").unwrap();
    let (_, rows) = fx.rows();
    let want: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r["status"].as_str().unwrap().to_string(),
                r["code"].as_str().unwrap().to_string(),
                r["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(want.iter().any(|w| w.0 == "ERROR") && want.iter().any(|w| w.0 == "WARN"));
    for cmd in ["doctor", "status"] {
        let o = fx.run(&[cmd, "--root", fx.root_arg()]);
        assert_eq!(code(&o), 1);
        assert_eq!(table_rows(&text(&o.stdout)), want, "{cmd}");
        let j = fx.run(&[cmd, "--root", fx.root_arg(), "--json"]);
        let v: Value = serde_json::from_slice(&j.stdout).unwrap();
        assert_eq!(v["command"], cmd);
        assert_eq!(v["result"], "error");
        assert_eq!(v["rows"].as_array().unwrap(), &rows.to_vec());
    }
    // status prints no detail or fix lines.
    let s = text(&fx.run(&["status", "--root", fx.root_arg()]).stdout);
    assert!(!s.contains("detail:") && !s.contains("fix:"));
}

#[test]
fn json_is_one_document_without_ansi_and_exit_follows_the_rows() {
    let fx = Fx::applied();
    let o = fx.doctor(&["--json"]);
    let out = text(&o.stdout);
    assert_eq!(out.lines().count(), 1);
    assert!(!out.contains('\u{1b}'));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(v["isolated"].as_str().unwrap().contains("home="));
    assert_eq!(v["result"], "ok");
    for r in v["rows"].as_array().unwrap() {
        for k in [
            "kind", "code", "status", "subject", "detail", "fix", "required", "partial",
        ] {
            assert!(r.get(k).is_some(), "{r}");
        }
    }
}

#[test]
fn narrow_output_wraps_every_line_and_json_keeps_the_full_path() {
    let fx = Fx::applied();
    let deep = ".config/some/very/long/directory/name/for/narrow/terminals/tool.conf";
    fx.put("base/modules/extra/files/deep", "x\n");
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &format!("{m}\n[[resources]]\nid = \"deep\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"{deep}\"\n[resources.value]\nsource = \"files/deep\"\n"),
    );
    let o = fx.run_env(&[("DFM_WIDTH", "40")], &["doctor", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let t = text(&o.stdout);
    let header_free: Vec<&str> = t.lines().filter(|l| !l.starts_with("isolated:")).collect();
    assert!(header_free.iter().all(|l| l.chars().count() <= 40), "{t}");
    assert!(!t.contains(deep), "the long path is wrapped");
    let (_, rows) = fx.rows();
    assert!(
        rows.iter().any(|r| r["subject"] == deep),
        "--json has the full path"
    );
}

#[test]
fn untrusted_text_is_escaped_in_both_formats() {
    let fx = Fx::applied();
    fs::remove_file(fx.h(".zshrc")).unwrap();
    fs::write(fx.h(".zshrc"), "x").unwrap();
    // A resource whose id carries a terminal escape is refused by the resolver, and the refusal
    // text reaches a row. Neither format may carry the raw escape.
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &m.replace("id = \"tmux\"", "id = \"tm\\u001b[31mux\""),
    );
    for extra in [&[][..], &["--json"][..]] {
        let o = fx.doctor(extra);
        let out = text(&o.stdout);
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert_eq!(code(&o), 1);
    }
}

// ---- usage --------------------------------------------------------------------------------

#[test]
fn bad_arguments_are_usage_errors() {
    let fx = Fx::new();
    for cmd in ["doctor", "status"] {
        for args in [
            vec![cmd],
            vec![cmd, "--root"],
            vec![cmd, "--root", fx.root_arg(), "--nope"],
            vec![cmd, "--root", fx.root_arg(), "--json", "--json"],
            vec![cmd, "--root", fx.root_arg(), "--against", "x"],
        ] {
            let o = fx.run(&args);
            assert_eq!(code(&o), 2, "{args:?}: {}", text(&o.stderr));
            assert!(text(&o.stdout).is_empty(), "{args:?}");
        }
    }
}

// ---- non-mutation -------------------------------------------------------------------------

#[test]
fn doctor_status_and_json_change_nothing_and_never_create_the_lock_file() {
    let fx = Fx::applied();
    // Break several things so every row kind is exercised, then remove the lock file bind left.
    fs::remove_file(fx.h(".zshrc")).unwrap();
    fs::remove_file(fx.h(".ssh/config")).unwrap();
    fs::write(fx.h(".ssh/config"), "x").unwrap();
    fs::write(fx.state().join("receipts").join("stray.json"), "{}").unwrap();
    let lock = fx.h(".local/state/dotfiles-manager.lock");
    fs::remove_file(&lock).unwrap();
    let before = fingerprint(fx.home.path());
    let src_before = fingerprint(&fx.root);
    for args in [
        vec!["doctor", "--root", fx.root_arg()],
        vec!["doctor", "--root", fx.root_arg(), "--json"],
        vec!["status", "--root", fx.root_arg()],
        vec!["status", "--root", fx.root_arg(), "--json"],
    ] {
        let o = fx.run(&args);
        assert_eq!(code(&o), 1, "{args:?}: {}", text(&o.stderr));
        assert_eq!(fingerprint(fx.home.path()), before, "{args:?}");
        assert!(!lock.exists(), "{args:?} created the lock file");
    }
    assert_eq!(fingerprint(&fx.root), src_before);
    let names = |d: &Path| {
        let mut v: Vec<_> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names(&fx.state()).len(),
        6,
        "no new state file: ledger, journal, state.json, 3 dirs"
    );
}

#[test]
fn doctor_answers_while_another_command_holds_the_lock_and_leaves_it_alone() {
    use std::process::Stdio;
    let fx = Fx::new();
    let digest = {
        let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
        dfm_model::plan::Saved::parse(&o.stdout)
            .unwrap()
            .plan_digest
    };
    // A parked apply holds the writer lock until the gate file appears.
    let gate = fx._src.path().join("gate");
    let reached = gate.with_extension("reached");
    let child = Command::new(BIN)
        .env_clear()
        .env(
            "DFM_FAILPOINT",
            format!("apply-before-effect:wait-{}", gate.display()),
        )
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", &digest])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !reached.exists() {
        assert!(std::time::Instant::now() < end, "apply never parked");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let blocked = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
    let before = fingerprint(fx.home.path());
    let o = fx.doctor(&["--json"]);
    let after = fingerprint(fx.home.path());
    fs::write(&gate, "").unwrap();
    let _ = child.wait_with_output().unwrap();
    assert!(
        text(&blocked.stderr).contains("E_LOCKED"),
        "the lock was not held"
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(
        rows.iter().find(|r| r["kind"] == "lock").unwrap()["code"],
        "D_LOCK_NOTE"
    );
    // The parked run wrote its intent, so doctor reports the interruption record it can see.
    assert!(rows.iter().any(|r| r["kind"] == "intent"));
    assert_eq!(before, after, "doctor changed HOME while the lock was held");
}

// ---- docs ---------------------------------------------------------------------------------

#[test]
fn every_doctor_code_is_documented() {
    let doc = fs::read_to_string(common::repo_root().join("docs/doctor.md")).unwrap();
    for (c, _) in dfm_model::doctor::CODES {
        assert!(doc.contains(c), "docs/doctor.md does not mention {c}");
    }
}
