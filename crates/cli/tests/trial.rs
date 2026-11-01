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
    assert!(
        t.contains("cleanup scope: dfm purge removes the install above"),
        "{t}"
    );
    assert!(
        t.contains(&format!(
            "data: {} absent",
            fx.home.path().join(".local/share/atuin").display()
        )),
        "{t}"
    );
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
    assert_eq!(r["schema"], 4, "a record with data paths");
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

// ---- `packages --update` rolls an active mise trial (owner decision 2026-10-31) ---------------

const MISE: &str = "mise/v1";
const TRIAL_OP: &str = "trial:trial-duckdb@package";

impl Fx {
    /// `dfm trial duckdb --provider mise/v1`, approved: the look-alike lists `duckdb 2.0.0`.
    fn mise_trial(&self) {
        let o = self.trial(&["duckdb", "--provider", MISE, "--preview"]);
        let o = self.trial(&["duckdb", "--provider", MISE, "--yes-plan", &digest_of(&o)]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    }

    fn update(&self, more: &[&str]) -> Output {
        let mut v = vec!["packages", "--update"];
        v.extend_from_slice(more);
        self.run(&v)
    }

    fn mise_installs(&self) -> usize {
        self.log().matches("mise install duckdb@latest").count()
    }
}

#[test]
fn packages_update_rolls_an_active_mise_trial_as_a_declared_rolling_tool() {
    let fx = Fx::new();
    fx.mise_trial();
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("planned {TRIAL_OP}")), "{t}");
    assert!(
        t.contains(
            "upgrade duckdb with mise/v1 to the newest version the provider resolves at apply time"
        ),
        "{t}"
    );
    assert_eq!(fx.mise_installs(), 1, "the preview installs nothing");
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("updated {TRIAL_OP}")), "{t}");
    assert!(t.contains("counts: updated=1 "), "{t}");
    assert_eq!(fx.mise_installs(), 2, "{}", fx.log());
    // The operation is journaled as a declared tool's upgrade is.
    let phases: Vec<String> = fx
        .journal()
        .iter()
        .filter(|e| e["resource"] == "trial:trial-duckdb" && e["op"] == "upgrade")
        .map(|e| e["phase"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(phases, ["intent", "done"], "{:?}", fx.journal());
}

#[test]
fn packages_update_does_not_roll_an_adopted_trial_from_its_record() {
    let fx = Fx::new();
    fx.mise_trial();
    let path = fx.state().join("trials/trial-duckdb.json");
    let mut t = fx.json(&path);
    // A record with data paths stays schema 4 when adopted.
    t["adopted"] = serde_json::json!({"op_id": "adopt-1-2", "module": "extra", "at_unix_ns": 9});
    fs::write(&path, serde_json::to_vec(&t).unwrap()).unwrap();
    let o = fx.update(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(!t.contains("trial-duckdb"), "{t}");
    assert!(t.contains("nothing to change"), "{t}");
    assert_eq!(fx.mise_installs(), 1, "{}", fx.log());
}

#[test]
fn packages_update_rolls_a_trial_tool_that_a_module_declares_once() {
    let fx = Fx::new();
    fx.put(
        "base/modules/extra/module.toml",
        &format!(
            "{}\n[[resources]]\nid = \"duckdb\"\nkind = \"package\"\nadapter = \"mise/v1\"\n[resources.value]\nname = \"duckdb\"\nupdate_policy = \"rolling\"\n",
            common::EXTRA
        ),
    );
    fx.mise_trial();
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("planned extra.duckdb@package"), "{t}");
    assert!(!t.contains(TRIAL_OP), "{t}");
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("counts: updated=1 "),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.mise_installs(), 2, "{}", fx.log());
}

#[test]
fn packages_update_does_not_reinstall_a_trial_tool_removed_by_hand() {
    let fx = Fx::new();
    fx.mise_trial();
    fx.marker("installed-mise", "");
    let o = fx.update(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("note: trial trial-duckdb: duckdb is not installed by mise/v1; it is not installed again"),
        "{t}"
    );
    assert!(!t.contains(TRIAL_OP), "{t}");
    assert!(t.contains("nothing to change"), "{t}");
    assert_eq!(fx.mise_installs(), 1, "{}", fx.log());
}

#[test]
fn an_unreadable_trial_record_is_a_note_and_does_not_block_the_update() {
    let fx = Fx::new();
    fx.mise_trial();
    fs::write(fx.state().join("trials/t-bad.json"), "not json").unwrap();
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("note: trial record t-bad.json cannot be read"),
        "{t}"
    );
    assert!(t.contains(&format!("planned {TRIAL_OP}")), "{t}");
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("updated {TRIAL_OP}")),
        "{}",
        text(&o.stdout)
    );
}

#[test]
fn packages_update_does_not_roll_a_trial_record_of_another_binding() {
    let fx = Fx::new();
    fx.mise_trial();
    let path = fx.state().join("trials/trial-duckdb.json");
    let mut t = fx.json(&path);
    t["binding"]["profile"] = "/elsewhere/profiles/other.toml".into();
    fs::write(&path, serde_json::to_vec(&t).unwrap()).unwrap();
    let o = fx.update(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("note: trial record trial-duckdb.json was written for /elsewhere/profiles/other.toml as m5-test; its tool is not rolled"),
        "{t}"
    );
    assert!(!t.contains(TRIAL_OP), "{t}");
    assert_eq!(fx.mise_installs(), 1, "{}", fx.log());
}

#[test]
fn packages_update_rolls_a_tool_of_two_trials_once() {
    let fx = Fx::new();
    fx.mise_trial();
    let o = fx.trial(&[
        "duckdb",
        "--id",
        "duckdb-two",
        "--provider",
        MISE,
        "--preview",
    ]);
    let o = fx.trial(&[
        "duckdb",
        "--id",
        "duckdb-two",
        "--provider",
        MISE,
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    // The records are read in file name order, so `duckdb-two` rolls the tool and
    // `trial-duckdb` adds no second row.
    assert!(t.contains("planned trial:duckdb-two@package"), "{t}");
    assert!(!t.contains(TRIAL_OP), "{t}");
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("counts: updated=1 "),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.mise_installs(), 2, "{}", fx.log());
}

#[test]
fn a_trial_id_equal_to_a_manifest_resource_id_still_rolls() {
    let fx = Fx::new();
    let o = fx.trial(&[
        "duckdb",
        "--id",
        "extra.tmux",
        "--provider",
        MISE,
        "--preview",
    ]);
    let o = fx.trial(&[
        "duckdb",
        "--id",
        "extra.tmux",
        "--provider",
        MISE,
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("planned trial:extra.tmux@package"), "{t}");
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("updated trial:extra.tmux@package"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.mise_installs(), 2, "{}", fx.log());
}

impl Fx {
    /// A declared rolling mise tool `jq`, installed: the update a trial must never block.
    fn declare_jq(&self) {
        self.put(
            "base/modules/extra/module.toml",
            &format!(
                "{}\n[[resources]]\nid = \"jq\"\nkind = \"package\"\nadapter = \"mise/v1\"\n[resources.value]\nname = \"jq\"\nupdate_policy = \"rolling\"\n",
                common::EXTRA
            ),
        );
    }

    fn set_trial_package_name(&self, name: &str) {
        let path = self.state().join("trials/trial-duckdb.json");
        let mut t = self.json(&path);
        t["package"]["name"] = name.into();
        fs::write(&path, serde_json::to_vec(&t).unwrap()).unwrap();
    }

    /// `packages --update` previewed and approved: the declared `jq` is updated, the trial has
    /// no row and the note names why.
    fn update_rolls_jq_and_notes(&self, note: &str) {
        let o = self.update(&["--preview"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        let t = text(&o.stdout);
        assert!(t.contains(note), "{t}");
        assert!(!t.contains("trial:trial-duckdb@package"), "{t}");
        let o = self.update(&["--yes-plan", &digest_of(&o)]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        let t = text(&o.stdout);
        assert!(t.contains("updated extra.jq@package"), "{t}");
        assert!(t.contains("counts: updated=1 "), "{t}");
    }
}

#[test]
fn a_trial_tool_with_another_owner_is_a_note_and_blocks_no_other_update() {
    let fx = Fx::new();
    fx.mise_trial();
    fx.declare_jq();
    // duckdb removed by hand, and another copy on the way.
    fx.marker("installed-mise", "jq 1.7\n");
    let other = fx.home.path().join(".local/bin");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("duckdb"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(other.join("duckdb"), fs::Permissions::from_mode(0o755)).unwrap();
    fx.update_rolls_jq_and_notes("note: trial trial-duckdb: duckdb is not rolled (other-owner:");
}

#[test]
fn a_trial_record_with_a_name_the_adapter_refuses_is_a_note_and_blocks_no_other_update() {
    let fx = Fx::new();
    fx.mise_trial();
    fx.declare_jq();
    fx.marker("installed-mise", "duckdb 2.0.0\njq 1.7\n");
    fx.set_trial_package_name("duckdb@lts");
    fx.update_rolls_jq_and_notes("note: trial trial-duckdb: duckdb@lts is not rolled (refused:");
}

#[test]
fn a_trial_tool_the_provider_cannot_list_is_a_note_and_blocks_no_other_update() {
    let fx = Fx::new();
    fx.mise_trial();
    fx.declare_jq();
    fx.marker("installed-mise", "duckdb 2.0.0\njq 1.7\n");
    // The look-alike's `mise ls` exits 1 for this name, as for a removed plugin.
    fx.set_trial_package_name("zzz-unknown");
    fx.update_rolls_jq_and_notes("note: trial trial-duckdb: zzz-unknown is not rolled (unknown:");
}

#[test]
fn a_tool_of_two_trials_rolls_from_the_second_record_when_the_first_drops() {
    let fx = Fx::new();
    fx.mise_trial();
    let o = fx.trial(&[
        "duckdb",
        "--id",
        "duckdb-two",
        "--provider",
        MISE,
        "--preview",
    ]);
    let o = fx.trial(&[
        "duckdb",
        "--id",
        "duckdb-two",
        "--provider",
        MISE,
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // `duckdb-two.json` is read first, and the adapter refuses its name.
    let path = fx.state().join("trials/duckdb-two.json");
    let mut t = fx.json(&path);
    t["package"]["name"] = "duckdb@lts".into();
    fs::write(&path, serde_json::to_vec(&t).unwrap()).unwrap();
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("note: trial duckdb-two: duckdb@lts is not rolled (refused:"),
        "{t}"
    );
    assert!(t.contains(&format!("planned {TRIAL_OP}")), "{t}");
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("counts: updated=1 "),
        "{}",
        text(&o.stdout)
    );
}

#[test]
fn state_check_accepts_a_trial_roll_and_reports_an_interrupted_one_without_blocking_others() {
    let fx = Fx::new();
    fx.mise_trial();
    let o = fx.update(&["--preview"]);
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // The next roll finishes its install, but the manager never records the outcome.
    let o = fx.update(&["--preview"]);
    let crash = fx
        .cmd(
            &[("DFM_FAILPOINT", "apply-after-effect:abort")],
            &["packages", "--update", "--yes-plan", &digest_of(&o)],
        )
        .output()
        .unwrap();
    assert!(!crash.status.success(), "{}", text(&crash.stdout));
    let intent = fx.journal().pop().unwrap();
    assert_eq!(intent["phase"], "intent", "{intent}");
    assert_eq!(intent["resource"], "trial:trial-duckdb", "{intent}");
    let id = intent["op_id"].as_str().unwrap().to_string();
    let o = fx.run(&["state", "check"]);
    assert_ne!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let all = format!("{}{}", text(&o.stdout), stderr(&o));
    assert!(
        all.contains("E_OP_INTERRUPTED") && all.contains(&id),
        "{all}"
    );
    // The trial row is not in a plain packages plan or the configuration plan, so neither is
    // blocked by it. An open intent makes any run ask for the digest, as for a declared tool.
    let o = fx.run(&["packages", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["packages", "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["apply", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // `state recover` cannot observe a package and leaves it unknown; `state resolve` settles it.
    let o = fx.run(&["state", "recover"]);
    assert_ne!(code(&o), 2, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["state", "resolve", &id, "--as", "failed"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
}

#[test]
fn an_interrupted_trial_roll_is_a_note_and_the_declared_tools_still_roll() {
    let fx = Fx::new();
    fx.mise_trial();
    // The trial roll finishes its install, but the manager never records the outcome.
    let o = fx.update(&["--preview"]);
    let crash = fx
        .cmd(
            &[("DFM_FAILPOINT", "apply-after-effect:abort")],
            &["packages", "--update", "--yes-plan", &digest_of(&o)],
        )
        .output()
        .unwrap();
    assert!(!crash.status.success(), "{}", text(&crash.stdout));
    let id = fx.journal().pop().unwrap()["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    fx.declare_jq();
    let listed = fs::read_to_string(fx.fake.path().join("installed-mise")).unwrap();
    fx.marker("installed-mise", &format!("{listed}jq 1.7\n"));
    let note = |id: &str| {
        format!(
            "note: trial trial-duckdb: duckdb is not rolled (interrupted op {id}; run dfm state resolve)"
        )
    };
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&note(&id)), "{t}");
    assert!(!t.contains(TRIAL_OP), "{t}");
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("updated extra.jq@package"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(
        fx.mise_installs(),
        2,
        "the interrupted trial roll ran once only: {}",
        fx.log()
    );
    // That run settled the open intent as `unknown`; it still drops the trial row.
    let open = fx
        .journal()
        .into_iter()
        .rev()
        .find(|e| e["resource"] == "trial:trial-duckdb")
        .unwrap();
    assert_eq!(open["phase"], "unknown", "{open}");
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&note(open["op_id"].as_str().unwrap())), "{t}");
    assert!(!t.contains(TRIAL_OP), "{t}");
}

#[test]
fn an_interrupted_trial_install_is_a_note_and_the_tool_is_not_rolled() {
    let fx = Fx::new();
    let o = fx.trial(&["duckdb", "--provider", MISE, "--preview"]);
    let o = fx
        .cmd(
            &[("DFM_FAILPOINT", "trial-after-intent:abort")],
            &[
                "trial",
                "duckdb",
                "--provider",
                MISE,
                "--yes-plan",
                &digest_of(&o),
            ],
        )
        .output()
        .unwrap();
    assert!(!o.status.success(), "{}", text(&o.stdout));
    let op = fx.journal().pop().unwrap()["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    // The install may have finished before the stop: the provider lists the tool.
    fx.marker("installed-mise", "duckdb 2.0.0\n");
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains(&format!(
            "note: trial trial-duckdb: duckdb is not rolled (interrupted op {op}; run dfm state resolve)"
        )),
        "{t}"
    );
    assert!(!t.contains(TRIAL_OP), "{t}");
    assert_eq!(fx.mise_installs(), 0, "{}", fx.log());
}

impl Fx {
    /// A `work` module whose id sorts after `trial:`, declaring a rolling mise `jq`.
    fn declare_work_jq(&self) {
        self.put(
            "base/modules/work/module.toml",
            "schema = 1\nid = \"work\"\nplatforms = [\"macos\", \"linux\"]\nrequires = []\n\n[[resources]]\nid = \"jq\"\nkind = \"package\"\nadapter = \"mise/v1\"\n[resources.value]\nname = \"jq\"\nupdate_policy = \"rolling\"\n",
        );
        self.put(
            "base/bundle.toml",
            &format!("{}work = \"modules/work/module.toml\"\n", common::BUNDLE),
        );
        self.put(
            "profiles/t-01.toml",
            &common::PROFILE.replace("modules = [\"extra\"]", "modules = [\"extra\", \"work\"]"),
        );
    }
}

/// The package rows of a plan's operation list (the lines with a `[module]` tag), in order.
fn package_rows(t: &str) -> Vec<String> {
    t.lines()
        .filter(|l| l.contains("@package ["))
        .filter_map(|l| l.split_whitespace().find(|w| w.ends_with("@package")))
        .map(str::to_string)
        .collect()
}

#[test]
fn a_failed_trial_roll_runs_after_every_declared_update_and_stops_none() {
    let fx = Fx::new();
    fx.mise_trial();
    fx.declare_work_jq();
    fx.marker("installed-mise", "duckdb 2.0.0\njq 1.7\n");
    fx.marker("mise-fail-duckdb", "");
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // By id alone `trial:` sorts before `work.`; the trial row still comes last.
    assert_eq!(
        package_rows(&text(&o.stdout)),
        ["work.jq@package", TRIAL_OP],
        "{}",
        text(&o.stdout)
    );
    let o = fx.update(&["--yes-plan", &digest_of(&o)]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 1, "{}\n{t}", stderr(&o));
    assert!(t.contains("updated work.jq@package"), "{t}");
    assert!(t.contains(&format!("failed {TRIAL_OP}")), "{t}");
    assert!(!t.contains("not_started work.jq"), "{t}");
    assert!(
        t.contains(" failed=1 ") && t.contains(" not_started=0 "),
        "{t}"
    );
    assert!(t.ends_with("result: failed\n"), "{t}");
}

#[test]
fn a_plan_without_trials_keeps_the_id_order() {
    let fx = Fx::new();
    fx.declare_jq();
    fx.declare_work_jq();
    fx.marker("installed-mise", "jq 1.7\n");
    let o = fx.update(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(
        package_rows(&text(&o.stdout)),
        ["extra.jq@package", "work.jq@package"],
        "{}",
        text(&o.stdout)
    );
}

// ---- purge (DFM-34, docs/purge.md) -------------------------------------------------------------

impl Fx {
    fn purge(&self, args: &[&str]) -> Output {
        let mut v = vec!["purge"];
        v.extend_from_slice(args);
        self.run(&v)
    }

    /// Previews, then purges with the preview's digest.
    fn purge_approved(&self, id: &str) -> Output {
        let o = self.purge(&[id, "--preview"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        self.purge(&[id, "--yes-plan", &digest_of(&o)])
    }

    fn brew_trial(&self) {
        let o = self.trial(&["atuin", "--provider", BREW, "--preview"]);
        let o = self.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest_of(&o)]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    }

    fn home_put(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.home.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p
    }

    fn cellar(&self, name: &str) -> PathBuf {
        self.fake.path().join("Cellar").join(name)
    }
}

#[test]
fn purge_removes_the_install_its_dependencies_and_what_the_tool_created() {
    let fx = Fx::new();
    // A cache that existed before the trial stays.
    let kept = fx.home_put(".cache/atuin/old", "before\n");
    fx.marker("deps-atuin", "libsodium\n");
    fx.brew_trial();
    assert!(fx.cellar("atuin").exists() && fx.cellar("libsodium").exists());
    // What the tool wrote while it was tried.
    fx.home_put(".config/atuin/config.toml", "sync = true\n");
    fx.home_put(".local/share/atuin/history.db", "rows\n");
    fx.home_put(".local/share/atuin/sub/deep", "x\n");
    fx.home_put(".cache/atuin/new", "after\n");
    // An older keg beside the installed one: every version goes.
    fs::create_dir_all(fx.cellar("atuin").join("0.9.0")).unwrap();

    let o = fx.purge(&["atuin", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    let home = fx.home.path().display().to_string();
    assert!(t.contains("purge: trial-atuin tool=atuin"), "{t}");
    assert!(t.contains("action: uninstall homebrew/v1 atuin"), "{t}");
    assert!(
        t.contains("action: uninstall dependency homebrew/v1 libsodium"),
        "{t}"
    );
    assert!(
        t.contains(&format!(
            "action: remove {home}/.config/atuin directory (1 entries"
        )),
        "{t}"
    );
    assert!(
        t.contains(&format!(
            "action: remove {home}/.local/share/atuin directory (3 entries"
        )),
        "{t}"
    );
    assert!(
        t.contains(&format!("keep: remove {home}/.cache/atuin directory")),
        "{t}"
    );
    assert!(t.contains("existed before the trial"), "{t}");
    assert!(
        t.contains(&format!(
            "nothing: remove {home}/.local/state/atuin (already absent)"
        )),
        "{t}"
    );
    assert!(
        t.contains("retained: services paths-outside-baseline provider-caches"),
        "{t}"
    );
    // The preview changed nothing, and no approval changes nothing.
    assert!(!fx.log().contains("uninstall"), "{}", fx.log());
    let o = fx.purge(&["trial-atuin"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
    assert!(fx.cellar("atuin").exists());

    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("trial: state/trials/trial-atuin.json removed"),
        "{t}"
    );
    assert!(t.ends_with("result: ok\n"), "{t}");
    let log = fx.log();
    assert!(
        log.contains("brew uninstall --formula --force atuin |"),
        "{log}"
    );
    assert!(log.contains("uninstall autoremove=1"), "{log}");
    assert!(
        log.contains("brew uninstall --formula --force libsodium |"),
        "{log}"
    );
    assert!(
        !log.contains("brew autoremove") && !log.contains("--zap"),
        "{log}"
    );
    assert!(!fx.cellar("atuin").exists() && !fx.cellar("libsodium").exists());
    assert!(!fx.home.path().join(".config/atuin").exists());
    assert!(!fx.home.path().join(".local/share/atuin").exists());
    assert!(
        fx.home.path().join(".local/share").exists(),
        "the parent stays"
    );
    assert_eq!(fs::read_to_string(&kept).unwrap(), "before\n");
    assert!(fx.home.path().join(".cache/atuin/new").exists());
    assert!(fx.entries("trials").is_empty());
    // Every step is settled in the journal and named by the receipt.
    let purge: Vec<_> = fx
        .journal()
        .into_iter()
        .filter(|e| e["op_id"].as_str().unwrap().starts_with("purge-"))
        .collect();
    let phases: Vec<&str> = purge.iter().map(|e| e["phase"].as_str().unwrap()).collect();
    assert_eq!(
        phases,
        [
            "intent", "done", "intent", "done", "intent", "done", "intent", "done"
        ],
        "{purge:?}"
    );
    let rc = fx
        .entries("receipts")
        .into_iter()
        .map(|p| fx.json(&p))
        .find(|r| r["command"] == "purge")
        .unwrap();
    assert_eq!(rc["result"]["outcome"], "ok");
    assert_eq!(rc["operations"].as_array().unwrap().len(), 4);
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // The tool can be tried again.
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}

#[test]
fn purge_keeps_a_pre_existing_tool_and_a_shared_dependency() {
    let fx = Fx::new();
    fx.brew_installed("atuin 18.24.1\n");
    fx.brew_trial();
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("keep: uninstall homebrew/v1 atuin: existed before the trial")
    );
    assert!(!fx.log().contains("uninstall"), "{}", fx.log());
    assert!(fx.cellar("atuin").exists());
    assert!(fx.entries("trials").is_empty());

    // A dependency another installed formula needs is kept; the purge still succeeds.
    let fx = Fx::new();
    fx.marker("deps-atuin", "libsodium\n");
    fx.marker("needed-libsodium", "other\n");
    fx.brew_trial();
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(
        t.contains("kept: brew uninstall libsodium: an installed formula still needs it"),
        "{t}"
    );
    assert!(!fx.cellar("atuin").exists());
    assert!(fx.cellar("libsodium").exists());
    assert!(fx.entries("trials").is_empty());
}

#[test]
fn purge_keeps_a_package_another_trial_uses() {
    let fx = Fx::new();
    fx.brew_trial();
    // A second trial of the same package under another id.
    let o = fx.trial(&["atuin", "--provider", BREW, "--id", "again", "--preview"]);
    let o = fx.trial(&[
        "atuin",
        "--provider",
        BREW,
        "--id",
        "again",
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("keep: uninstall homebrew/v1 atuin: trial again also uses it"),
        "{}",
        text(&o.stdout)
    );
    assert!(fx.cellar("atuin").exists());
}

#[test]
fn purge_uninstalls_every_version_of_a_mise_trial() {
    let fx = Fx::new();
    fx.mise_trial();
    let o = fx.purge_approved("trial-duckdb");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        fx.log().contains("mise uninstall --all duckdb |"),
        "{}",
        fx.log()
    );
    let db = fs::read_to_string(fx.fake.path().join("installed-mise")).unwrap_or_default();
    assert!(!db.contains("duckdb"), "{db}");
}

#[test]
fn purge_refuses_an_adopted_trial_and_an_unknown_id() {
    let fx = Fx::new();
    fx.mise_trial();
    let path = fx.state().join("trials/trial-duckdb.json");
    let mut t = fx.json(&path);
    // A record with data paths stays schema 4 when adopted.
    t["adopted"] = serde_json::json!({"op_id": "adopt-1-2", "module": "extra", "at_unix_ns": 9});
    fs::write(&path, serde_json::to_vec(&t).unwrap()).unwrap();
    let o = fx.purge(&["trial-duckdb", "--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("E_CONFLICT trial trial-duckdb is adopted"),
        "{}",
        stderr(&o)
    );
    let o = fx.purge(&["nothing", "--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("no trial nothing"), "{}", stderr(&o));
}

#[test]
fn purge_refuses_a_change_after_the_preview_and_never_follows_a_link() {
    let fx = Fx::new();
    fx.brew_trial();
    fx.home_put(".config/atuin/config.toml", "a\n");
    // A link the tool did not make, at a recorded path: never followed, never removed.
    let outside = TempDir::new("purge-outside");
    fs::write(outside.path().join("keep"), "x").unwrap();
    fs::create_dir_all(fx.home.path().join(".local/share")).unwrap();
    std::os::unix::fs::symlink(outside.path(), fx.home.path().join(".local/share/atuin")).unwrap();
    let o = fx.purge(&["trial-atuin", "--preview"]);
    let digest = digest_of(&o);
    assert!(
        text(&o.stdout).contains("a link or a special file is never removed"),
        "{}",
        text(&o.stdout)
    );
    fx.home_put(".config/atuin/added", "b\n");
    let o = fx.purge(&["trial-atuin", "--yes-plan", &digest]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(fx.cellar("atuin").exists());
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(outside.path().join("keep").exists());
    assert!(fs::symlink_metadata(fx.home.path().join(".local/share/atuin")).is_ok());
    assert!(!fx.home.path().join(".config/atuin").exists());
}

#[test]
fn a_failed_uninstall_keeps_the_record_for_a_second_purge() {
    let fx = Fx::new();
    fx.mise_trial();
    // Make mise's uninstall fail once.
    let real = fx.bin().join("mise");
    let body = fs::read_to_string(&real).unwrap();
    fs::write(
        &real,
        body.replacen(
            "  uninstall)\n",
            "  uninstall)\n    [ -e \"$here/../uninstall-fail\" ] && exit 1\n",
            1,
        ),
    )
    .unwrap();
    fx.marker("uninstall-fail", "");
    let o = fx.purge_approved("trial-duckdb");
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains("json kept; run dfm purge trial-duckdb again"));
    assert_eq!(fx.entries("trials").len(), 1);
    fs::remove_file(fx.fake.path().join("uninstall-fail")).unwrap();
    let o = fx.purge_approved("trial-duckdb");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(fx.entries("trials").is_empty());
}

#[test]
fn a_brew_failure_that_is_not_the_dependents_refusal_fails_the_purge() {
    let fx = Fx::new();
    fx.marker("deps-atuin", "libsodium\n");
    fx.brew_trial();
    // brew fails for another reason (its lock, a permission): not a kept dependency.
    let brew = fx.bin().join("brew");
    let body = fs::read_to_string(&brew).unwrap();
    fs::write(
        &brew,
        body.replacen(
            "  uninstall)\n",
            "  uninstall)\n    [ \"$4\" = libsodium ] && { echo 'Error: another brew process is running' >&2; exit 1; }\n",
            1,
        ),
    )
    .unwrap();
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("failed uninstall libsodium"), "{t}");
    assert!(t.contains("json kept"), "{t}");
    assert_eq!(fx.entries("trials").len(), 1);
    assert!(fx.cellar("libsodium").exists());
}

#[test]
fn purge_keeps_a_tree_it_cannot_remove_whole_and_a_path_behind_a_link_parent() {
    let fx = Fx::new();
    fx.brew_trial();
    // A read-only subdirectory: remove_tree could not unlink inside it.
    fx.home_put(".config/atuin/ro/file", "x\n");
    let ro = fx.home.path().join(".config/atuin/ro");
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    // A parent that is a link inside HOME.
    let real = fx.home.path().join("real-share");
    fs::create_dir_all(real.join("atuin")).unwrap();
    fs::write(real.join("atuin/db"), "rows\n").unwrap();
    fs::create_dir_all(fx.home.path().join(".local")).unwrap();
    std::os::unix::fs::symlink(&real, fx.home.path().join(".local/share")).unwrap();
    let o = fx.purge(&["trial-atuin", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("(the owner cannot write)"), "{t}");
    assert!(t.contains("a parent is unsafe"), "{t}");
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(ro.join("file").exists());
    assert!(real.join("atuin/db").exists());
    assert!(!fx.cellar("atuin").exists());
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn purge_keeps_a_path_another_trial_records_and_refuses_beside_an_unreadable_record() {
    let fx = Fx::new();
    fx.brew_trial();
    let o = fx.trial(&[
        "duckdb",
        "--provider",
        BREW,
        "--config",
        ".config/atuin",
        "--preview",
    ]);
    let o = fx.trial(&[
        "duckdb",
        "--provider",
        BREW,
        "--config",
        ".config/atuin",
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    fx.home_put(".config/atuin/config.toml", "x\n");
    let o = fx.purge(&["trial-atuin", "--preview"]);
    assert!(
        text(&o.stdout).contains("trial trial-duckdb also records it"),
        "{}",
        text(&o.stdout)
    );
    // Another record that cannot be read refuses the purge before any change.
    fs::write(fx.state().join("trials/broken.json"), "{").unwrap();
    let o = fx.purge(&["trial-atuin", "--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("trials/broken.json cannot be read"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_trial_skips_a_data_candidate_it_cannot_observe() {
    let fx = Fx::new();
    let outside = TempDir::new("trial-cache-outside");
    std::os::unix::fs::symlink(outside.path(), fx.home.path().join(".cache")).unwrap();
    let o = fx.trial(&["atuin", "--provider", BREW, "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let cache = fx.home.path().join(".cache/atuin").display().to_string();
    assert!(
        text(&o.stdout).contains(&format!("data: {cache} not observed")),
        "{}",
        text(&o.stdout)
    );
    let o = fx.trial(&["atuin", "--provider", BREW, "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let r = fx.json(&fx.entries("trials")[0]);
    assert!(!r.to_string().contains("/.cache/atuin"), "{r}");
}

#[test]
fn an_old_record_without_data_paths_is_purged_with_a_note() {
    let fx = Fx::new();
    fx.brew_trial();
    let path = fx.state().join("trials/trial-atuin.json");
    let mut t = fx.json(&path);
    t["schema"] = 2.into();
    t["baseline"].as_object_mut().unwrap().remove("data");
    fs::write(&path, serde_json::to_vec(&t).unwrap()).unwrap();
    let o = fx.purge(&["trial-atuin", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("note: this trial was recorded before data paths were observed"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
}

#[test]
fn purge_keeps_a_package_another_trial_names_by_its_tap() {
    let fx = Fx::new();
    fx.brew_trial();
    let o = fx.trial(&["atuin", "--provider", BREW, "--id", "again", "--preview"]);
    let o = fx.trial(&[
        "atuin",
        "--provider",
        BREW,
        "--id",
        "again",
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // The other trial spells the same formula with its official tap.
    let again = fx.state().join("trials/again.json");
    let mut r = fx.json(&again);
    r["package"]["name"] = "homebrew/core/atuin".into();
    fs::write(&again, serde_json::to_vec(&r).unwrap()).unwrap();
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("keep: uninstall homebrew/v1 atuin: trial again also uses it"),
        "{}",
        text(&o.stdout)
    );
    assert!(fx.cellar("atuin").exists());
}

#[test]
fn purge_removes_a_recorded_file_inside_a_recorded_directory_once() {
    let fx = Fx::new();
    let args = |tail: &[&str]| {
        let mut v = vec![
            "atuin",
            "--provider",
            BREW,
            "--config",
            ".config/atuin",
            "--config",
            ".config/atuin/config.toml",
        ];
        v.extend_from_slice(tail);
        v.into_iter().map(String::from).collect::<Vec<_>>()
    };
    let run = |a: Vec<String>| fx.trial(&a.iter().map(String::as_str).collect::<Vec<_>>());
    let o = run(args(&["--preview"]));
    let o = run(args(&["--yes-plan", &digest_of(&o)]));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let dir = fx.home_put(".config/atuin/config.toml", "x\n");
    let o = fx.purge_approved("trial-atuin");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(!dir.parent().unwrap().exists());
    assert!(!fx.state().join("trials/trial-atuin.json").exists());
    // Each removal intent names its target, so `state recover` can settle an interrupted one.
    let home = fx.home.path().display().to_string();
    let intents: Vec<_> = fx
        .journal()
        .into_iter()
        .filter(|e| e["op"] == "remove" && e["phase"] == "intent")
        .collect();
    assert_eq!(intents.len(), 1, "{intents:?}");
    assert_eq!(
        intents[0]["recovery"]["outputs"][0]["path"],
        format!("{home}/.config/atuin").as_str()
    );
}

#[test]
fn purge_of_a_pinned_mise_trial_keeps_another_version_of_the_tool() {
    let fx = Fx::new();
    // Version 20 predates the trial; the trial pins 2 (the stand-in installs 2.0.0).
    fx.marker("installed-mise", "duckdb 20.1.0\n");
    let o = fx.trial(&["duckdb@2", "--provider", MISE, "--preview"]);
    let o = fx.trial(&["duckdb@2", "--provider", MISE, "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.purge_approved("trial-duckdb");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        fx.log().contains("mise uninstall duckdb@2.0.0 |"),
        "{}",
        fx.log()
    );
    let db = fs::read_to_string(fx.fake.path().join("installed-mise")).unwrap();
    assert_eq!(db.trim(), "duckdb 20.1.0");
}

#[test]
fn purge_keeps_a_mise_tool_another_trial_pins() {
    let fx = Fx::new();
    fx.mise_trial();
    let o = fx.trial(&[
        "duckdb@2",
        "--provider",
        MISE,
        "--id",
        "pinned",
        "--preview",
    ]);
    let o = fx.trial(&[
        "duckdb@2",
        "--provider",
        MISE,
        "--id",
        "pinned",
        "--yes-plan",
        &digest_of(&o),
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.purge_approved("trial-duckdb");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("trial pinned also uses it"),
        "{}",
        text(&o.stdout)
    );
    assert!(!fx.log().contains("mise uninstall"), "{}", fx.log());
}

#[test]
fn purge_refuses_a_trial_recorded_for_another_machine() {
    let fx = Fx::new();
    fx.brew_trial();
    let path = fx.state().join("trials/trial-atuin.json");
    let mut r = fx.json(&path);
    r["binding"]["machine_id"] = "other-machine".into();
    fs::write(&path, serde_json::to_vec(&r).unwrap()).unwrap();
    let o = fx.purge(&["atuin", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("as other-machine"), "{}", stderr(&o));
    assert!(fx.cellar("atuin").exists());
}
