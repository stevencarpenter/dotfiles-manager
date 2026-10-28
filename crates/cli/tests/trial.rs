//! `trial`: a tool is tried without a module, through the isolated binary and the `brew` and
//! `mise` look-alikes (DFM-32, docs/trial.md). No real provider runs.

mod common;

use common::providers::{FAKE_BREW, FAKE_MISE};
use common::{TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");
const BREW: &str = "homebrew/v1";

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
    fake: TempDir,
}

fn stderr(o: &Output) -> String {
    text(&o.stderr)
}

fn digest_of(o: &Output) -> String {
    text(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no digest in\n{}\n{}", text(&o.stdout), stderr(o)))
}

impl Fx {
    fn new() -> Fx {
        let fx = Fx {
            home: TempDir::new("trial-home"),
            _src: TempDir::new("trial-src"),
            root: PathBuf::new(),
            fake: TempDir::new("trial-providers"),
        };
        let root = fx._src.path().join("root");
        let fx = Fx { root, ..fx };
        fx.put("base/bundle.toml", common::BUNDLE);
        fx.put("base/modules/core/module.toml", common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/extra/module.toml", common::EXTRA);
        fx.put("base/modules/extra/files/tmux.conf", "set -g mouse on\n");
        fx.put("base/modules/extra/files/zshrc", "export A=1\n");
        fx.put("profiles/t-01.toml", common::PROFILE);
        fs::create_dir_all(fx.bin()).unwrap();
        for (name, body) in [("brew", FAKE_BREW), ("mise", FAKE_MISE)] {
            let p = fx.bin().join(name);
            fs::write(&p, body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let p = fx.root.join("profiles/t-01.toml");
        let o = fx.run(&[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-test",
            "--root",
            fx.root.to_str().unwrap(),
        ]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        fx
    }

    fn bin(&self) -> PathBuf {
        self.fake.path().join("bin")
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// The installed brew packages, one `<name> <version>` line each, as Cellar kegs.
    fn brew_installed(&self, lines: &str) {
        let cellar = self.fake.path().join("Cellar");
        fs::create_dir_all(&cellar).unwrap();
        for l in lines.lines() {
            let mut p = l.split_whitespace();
            let keg = cellar.join(p.next().unwrap()).join(p.next().unwrap());
            fs::create_dir_all(&keg).unwrap();
            fs::write(
                keg.join("INSTALL_RECEIPT.json"),
                r#"{"source":{"tap":"homebrew/core"}}"#,
            )
            .unwrap();
        }
    }

    fn marker(&self, name: &str, content: &str) {
        fs::write(self.fake.path().join(name), content).unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    fn cmd(&self, env: &[(&str, &str)], args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
            .env("USER", "leak-check")
            .envs(env.iter().copied())
            .arg("--home")
            .arg(self.home.path())
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(&[], args).output().unwrap()
    }

    fn trial(&self, args: &[&str]) -> Output {
        let mut v = vec!["trial"];
        v.extend_from_slice(args);
        self.run(&v)
    }

    fn state(&self) -> PathBuf {
        self.home.path().join(".local/state/dotfiles-manager/state")
    }

    fn entries(&self, sub: &str) -> Vec<PathBuf> {
        let Ok(dir) = fs::read_dir(self.state().join(sub)) else {
            return Vec::new();
        };
        let mut v: Vec<PathBuf> = dir.map(|e| e.unwrap().path()).collect();
        v.sort();
        v
    }

    fn json(&self, path: &PathBuf) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn journal(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.state().join("journal.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn config(&self) -> String {
        self.home.path().join(".config/atuin").display().to_string()
    }
}

#[test]
fn a_preview_shows_the_owner_the_baseline_and_the_limits_and_writes_nothing() {
    let fx = Fx::new();
    fx.marker("deps-atuin", "libsodium\n");
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("trial: trial-atuin tool=atuin"), "{t}");
    assert!(t.contains("provider: homebrew/v1 package=atuin"), "{t}");
    assert!(
        t.contains("action: install atuin with homebrew/v1 (adds dependencies: libsodium)"),
        "{t}"
    );
    assert!(
        t.contains(&format!("baseline: {} absent", fx.config())),
        "{t}"
    );
    assert!(t.contains("cleanup scope: the install above"), "{t}");
    assert!(
        t.contains("unknown: services paths-outside-baseline provider-caches"),
        "{t}"
    );
    assert!(t.contains("plan digest: "), "{t}");
    // The probe ran read-only with the provider variables, and nothing was written.
    let log = fx.log();
    assert!(log.contains("brew --cellar | auto=1 analytics=1"), "{log}");
    assert!(!log.contains("brew install"), "{log}");
    assert!(fx.entries("trials").is_empty());
    assert!(fx.entries("receipts").is_empty());
    assert!(fx.journal().is_empty());
}

#[test]
fn a_new_tool_is_installed_and_the_trial_record_survives_the_process() {
    let fx = Fx::new();
    fx.marker("deps-atuin", "libsodium\n");
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    let digest = digest_of(&o);
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("  updated trial-atuin trial-"), "{t}");
    assert!(t.contains("trial: state/trials/trial-atuin.json"), "{t}");
    assert!(t.contains("receipt: "), "{t}");
    assert!(t.ends_with("result: ok\n"), "{t}");
    assert!(
        fx.log().contains(
            "brew install homebrew/core/atuin | auto=1 analytics=1 hints=1 cleanup=1 upgrade=1 dependents=1 user=none"
        ),
        "{}",
        fx.log()
    );
    // The record: baseline before the install, the install attributed by its journal id.
    let trials = fx.entries("trials");
    assert_eq!(trials.len(), 1, "{trials:?}");
    let r = fx.json(&trials[0]);
    assert_eq!(r["schema"], 2);
    assert_eq!(r["trial_id"], "trial-atuin");
    assert_eq!(r["tool"], "atuin");
    assert_eq!(r["binding"]["machine_id"], "m5-test");
    assert_eq!(r["baseline"]["tool_preexisting"], false);
    assert_eq!(r["baseline"]["paths"][0]["path"], fx.config());
    assert_eq!(r["baseline"]["paths"][0]["existed"], false);
    assert!(r["baseline"].get("owners").is_none(), "{r}");
    assert_eq!(r["package"]["adapter"], BREW);
    assert_eq!(r["package"]["name"], "atuin");
    assert_eq!(
        r["package"]["install"]["dependencies"],
        serde_json::json!(["libsodium"])
    );
    let op = r["package"]["install"]["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(op.starts_with("trial-"), "{op}");
    assert_eq!(
        r["unknown"],
        serde_json::json!(["services", "paths-outside-baseline", "provider-caches"])
    );
    // The journal and the receipt carry the same id.
    let j = fx.journal();
    let phases: Vec<&str> = j.iter().map(|e| e["phase"].as_str().unwrap()).collect();
    assert_eq!(phases, ["intent", "done"], "{j:?}");
    assert!(
        j.iter()
            .all(|e| e["op_id"] == op && e["resource"] == "trial-atuin")
    );
    let receipts = fx.entries("receipts");
    assert_eq!(receipts.len(), 1);
    let rc = fx.json(&receipts[0]);
    assert_eq!(rc["command"], "trial");
    assert_eq!(rc["op_id"], op);
    assert_eq!(rc["result"]["outcome"], "ok");
    assert_eq!(rc["plan_digest"], digest);
    assert_eq!(rc["operations"][0]["status"], "updated");
    // The state is visible after the process ended: state check and status count it.
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("trials: 1 (0 unreadable)"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.run(&["status", "--root", fx.root.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // The active trial has its own row.
    let t = text(&o.stdout);
    assert!(t.contains("OK      D_TRIAL trial"), "{t}");
    assert!(t.contains("trials/trial-atuin"), "{t}");
    // A second trial of the same tool is refused before any write.
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    let d2 = digest_of(&o);
    assert_ne!(
        d2, digest,
        "the tool is installed now, so the decision changed"
    );
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &d2]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("E_CONFLICT trial trial-atuin already exists"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fx.entries("receipts").len(), 1);
}

#[test]
fn a_pre_existing_tool_is_recorded_with_its_owner_and_nothing_is_installed() {
    let fx = Fx::new();
    fx.brew_installed("atuin 18.24.1\n");
    let cfg = fx.home.path().join(".config/atuin");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("config.toml"), "sync = true\n").unwrap();
    let o = fx.trial(&[
        "atuin",
        "--config",
        ".config/atuin/config.toml",
        "--preview",
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("action: nothing to install; atuin pre-exists, owned by homebrew/v1 (18.24.1)"),
        "{t}"
    );
    assert!(
        t.contains(&format!(
            "baseline: {} file sha256:",
            cfg.join("config.toml").display()
        )),
        "{t}"
    );
    let o = fx.trial(&[
        "atuin",
        "--config",
        ".config/atuin/config.toml",
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("  current trial-atuin trial-"),
        "{}",
        text(&o.stdout)
    );
    assert!(!fx.log().contains(" install "), "{}", fx.log());
    let r = fx.json(&fx.entries("trials")[0]);
    assert_eq!(r["baseline"]["tool_preexisting"], true);
    assert_eq!(
        r["baseline"]["owners"],
        serde_json::json!(["homebrew/v1 (18.24.1)"])
    );
    assert_eq!(r["baseline"]["paths"][0]["existed"], true);
    assert!(r["baseline"]["paths"][0]["sha256"].is_string(), "{r}");
    assert!(r["package"].get("install").is_none(), "{r}");
    assert!(
        fx.journal().is_empty(),
        "a pre-existing tool journals no install"
    );
    let rc = fx.json(&fx.entries("receipts")[0]);
    assert_eq!(rc["operations"][0]["status"], "current");
    // The configuration file was observed, not changed.
    assert_eq!(
        fs::read_to_string(cfg.join("config.toml")).unwrap(),
        "sync = true\n"
    );
}

#[test]
fn a_tool_with_another_owner_pre_exists_and_is_not_replaced() {
    let fx = Fx::new();
    let other = fx.home.path().join(".local/bin");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("atuin"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(other.join("atuin"), fs::Permissions::from_mode(0o755)).unwrap();
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("action: nothing to install; atuin pre-exists, owned by"),
        "{t}"
    );
    assert!(t.contains(".local/bin/atuin"), "{t}");
    assert!(t.contains("provider: none (no provider lists it)"), "{t}");
    // The record names no provider: none owns anything here.
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let r = fx.json(&fx.entries("trials")[0]);
    assert_eq!(r["baseline"]["tool_preexisting"], true);
    assert!(r.get("package").is_none(), "{r}");
    assert_eq!(
        fx.json(&fx.entries("receipts")[0])["operations"][0]["adapter"],
        "other-owner"
    );
}

#[test]
fn a_record_planted_between_the_check_and_the_lock_is_a_conflict_not_replaced() {
    let fx = Fx::new();
    // A pre-existing tool, so the second run changes nothing the locked re-probe would see.
    fx.brew_installed("atuin 18.24.1\n");
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    let digest = digest_of(&o);
    // The first run parks after its pre-lock check; a second run completes meanwhile.
    let gate = fx.fake.path().join("go");
    let parked = fx
        .cmd(
            &[(
                "DFM_FAILPOINT",
                &format!("trial-before-lock:wait-{}", gate.display()),
            )],
            &["trial", "atuin", "--provider", BREW, "--yes-plan", &digest],
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reached = gate.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < end,
            "the failpoint was not reached"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let first = fx.json(&fx.entries("trials")[0]);
    fs::write(&gate, "").unwrap();
    let o = parked.wait_with_output().unwrap();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_CONFLICT trial trial-atuin already exists"),
        "{}",
        stderr(&o)
    );
    // The first record is untouched, and only the run that wrote it left a receipt.
    assert_eq!(fx.json(&fx.entries("trials")[0]), first);
    assert_eq!(fx.entries("trials").len(), 1);
    assert_eq!(fx.entries("receipts").len(), 1);
}

#[test]
fn a_baseline_that_cannot_be_read_blocks_before_any_write() {
    let fx = Fx::new();
    let link = fx.home.path().join("atuin-link");
    std::os::unix::fs::symlink("/nonexistent", &link).unwrap();
    let o = fx.trial(&[
        "atuin",
        "--provider",
        BREW,
        "--config",
        "atuin-link",
        "--preview",
    ]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_STATE_INVALID baseline failure"),
        "{}",
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("is a link or a special file"),
        "{}",
        stderr(&o)
    );
    // A candidate outside HOME is refused too, before the providers run.
    let o = fx.trial(&[
        "atuin",
        "--provider",
        BREW,
        "--config",
        "/etc/atuin",
        "--preview",
    ]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_STATE_INVALID"), "{}", stderr(&o));
    assert!(fx.entries("trials").is_empty());
    assert!(fx.entries("receipts").is_empty());
    assert!(fx.journal().is_empty());
    assert!(!fx.log().contains(" install "), "{}", fx.log());
}

#[test]
fn without_an_approval_nothing_is_written_and_a_stale_digest_is_refused() {
    let fx = Fx::new();
    let o = fx.trial(&["atuin", "--provider", BREW]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_APPROVAL_REQUIRED nothing was changed"),
        "{}",
        stderr(&o)
    );
    assert!(
        text(&o.stdout).contains("plan digest: "),
        "the decision is printed for review"
    );
    let wrong = "0".repeat(64);
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &wrong]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    // A digest approved for one baseline does not cover another: the candidate changed.
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    let digest = digest_of(&o);
    fs::create_dir_all(fx.home.path().join(".config/atuin")).unwrap();
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!fx.log().contains(" install "), "{}", fx.log());
    assert!(fx.entries("trials").is_empty());
    assert!(fx.entries("receipts").is_empty());
    assert!(fx.journal().is_empty());
}

#[test]
fn an_interrupted_install_leaves_the_baseline_and_an_open_intent() {
    let fx = Fx::new();
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    let digest = digest_of(&o);
    let o = fx
        .cmd(
            &[("DFM_FAILPOINT", "trial-after-intent:abort")],
            &["trial", "atuin", "--provider", BREW, "--yes-plan", &digest],
        )
        .output()
        .unwrap();
    assert!(!o.status.success(), "{}", text(&o.stdout));
    assert!(!fx.log().contains(" install "), "{}", fx.log());
    // The baseline survived; the install intent has no outcome.
    let r = fx.json(&fx.entries("trials")[0]);
    assert_eq!(r["baseline"]["paths"][0]["existed"], false);
    let op = r["package"]["install"]["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    let j = fx.journal();
    assert_eq!(j.len(), 1, "{j:?}");
    assert_eq!(j[0]["phase"], "intent");
    assert_eq!(j[0]["op_id"], op);
    assert!(
        fx.entries("receipts").is_empty(),
        "no receipt is written for a crash"
    );
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("E_OP_INTERRUPTED"), "{t}");
    assert!(t.contains("trials: 1 (0 unreadable)"), "{t}");
    // The same trial is refused until it is purged; the operator settles the journal by hand.
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    // recover settles nothing (the intent has no path) and leaves the entry unknown; the
    // operator then resolves it as failed.
    let o = fx.run(&["state", "recover"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["state", "resolve", &op, "--as", "failed"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
}

#[test]
fn a_failed_install_is_journaled_and_the_receipt_says_failed() {
    let fx = Fx::new();
    fx.marker("fail", "");
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    let digest = digest_of(&o);
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest]);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("  failed trial-atuin trial-") && t.contains("exited 1"),
        "{t}"
    );
    assert!(t.ends_with("result: failed\n"), "{t}");
    assert!(!t.contains("PLANTED-STDERR") && !stderr(&o).contains("PLANTED-STDERR"));
    let phases: Vec<String> = fx
        .journal()
        .iter()
        .map(|e| e["phase"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(phases, ["intent", "failed"]);
    let rc = fx.json(&fx.entries("receipts")[0]);
    assert_eq!(rc["result"]["outcome"], "failed");
    assert_eq!(rc["result"]["exit_code"], 1);
    assert_eq!(rc["operations"][0]["status"], "failed");
    assert_eq!(
        fx.entries("trials").len(),
        1,
        "the baseline is kept for the operator"
    );
}

#[test]
fn an_ambiguous_provider_is_asked_for_and_a_single_capable_one_is_chosen() {
    let fx = Fx::new();
    // Both look-alikes can install atuin: the choice is the owner's.
    let o = fx.trial(&["atuin", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let e = stderr(&o);
    assert!(e.contains("E_APPROVAL_REQUIRED"), "{e}");
    assert!(
        e.contains("homebrew/v1, mise/v1") && e.contains("--provider"),
        "{e}"
    );
    // brew does not know duckdb as official, so mise is the one capable provider.
    fx.marker("official-brew", "atuin\n");
    let o = fx.trial(&["duckdb", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("action: install duckdb with mise/v1"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.trial(&["duckdb", "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        fx.log()
            .contains("mise install duckdb@latest | yes=1 auto=false noconfig=1"),
        "{}",
        fx.log()
    );
    // A name no provider can install blocks with the probe's own code.
    let o = fx.trial(&["zzz-unknown", "--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("E_PROBE_FAILED")
            && stderr(&o).contains("no provider can install it here"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_tool_name_that_cannot_be_a_trial_id_is_a_usage_error() {
    let fx = Fx::new();
    for name in ["npm:prettier", "npm:@scope/pkg"] {
        let o = fx.trial(&[name, "--preview"]);
        assert_eq!(code(&o), 2, "{}", stderr(&o));
        assert!(stderr(&o).contains("E_USAGE"), "{}", stderr(&o));
    }
    let o = fx.trial(&["atuin", "--id", "Bad Id", "--preview"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(!fx.log().contains("brew"), "no provider ran: {}", fx.log());
}
