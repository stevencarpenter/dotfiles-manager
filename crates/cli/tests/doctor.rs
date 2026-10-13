//! `doctor` and `status` through the isolated binary: row statuses and codes, independent rows,
//! table and JSON agreement, narrow output, the exit rule and non-mutation (DFM-35).
//!
//! The fixture (tests/common) is a synthetic link-only source tree. No test touches the real
//! HOME. Every run passes `--home <temp dir>` and an empty environment.

mod common;

use common::{BIN, Fx, PROFILE, code, fingerprint, is_root, text};
use dfm_model::doctor::DoctorCode;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Applies the plan, so every link is owned.
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

impl Fx {
    fn state(&self) -> PathBuf {
        self.h(".local/state/dotfiles-manager/state")
    }

    fn doctor(&self, extra: &[&str]) -> std::process::Output {
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

fn at<'a>(rows: &'a [Value], subject: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["subject"].as_str().unwrap().ends_with(subject))
        .unwrap_or_else(|| panic!("no row for {subject} in {rows:#?}"))
}

fn status<'a>(rows: &'a [Value], subject: &str) -> (&'a str, &'a str) {
    let r = at(rows, subject);
    (r["status"].as_str().unwrap(), r["code"].as_str().unwrap())
}

// ---- healthy state ------------------------------------------------------------------------

#[test]
fn an_applied_machine_is_healthy_and_every_owned_output_is_ok() {
    let fx = applied();
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
    let fx = applied();
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
    let fx = applied();
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
    let fx = applied();
    fs::remove_file(fx.h(".zshrc")).unwrap();
    symlink("/etc/hosts", fx.h(".zshrc")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(status(&rows, ".zshrc"), ("ERROR", "E_CONFLICT"));
}

#[test]
fn a_missing_output_is_a_warning_with_its_fix() {
    let fx = applied();
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
    let fx = applied();
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
    let fx = applied();
    fx.put("profiles/t-01.toml", &PROFILE.replace("\"extra\"", ""));
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    assert_eq!(status(&rows, ".zshrc"), ("WARN", "D_PLAN_REMOVAL"));
    assert_eq!(status(&rows, ".ssh/config").0, "OK");
}

#[test]
fn claims_under_missing_share_and_cache_directories_are_not_errors() {
    let fx = Fx::new();
    for (m, from, to) in [
        ("core", ".ssh/config", ".local/share/firstmate/config"),
        ("extra", ".zshrc", ".cache/firstmate/zshrc"),
    ] {
        let rel = format!("base/modules/{m}/module.toml");
        let t = fs::read_to_string(fx.root.join(&rel)).unwrap();
        fx.put(&rel, &t.replacen(from, to, 1));
    }
    for d in [".local/share", ".cache"] {
        assert!(!fx.h(d).exists(), "{d}");
    }
    let (_, rows) = fx.rows();
    assert!(rows.iter().all(|r| r["status"] != "ERROR"), "{rows:#?}");
}

#[test]
fn a_linked_parent_of_an_owned_output_is_unsafe() {
    let fx = applied();
    let elsewhere = fx._src.path().join("moved-tmux");
    fs::rename(fx.h(".config/tmux"), &elsewhere).unwrap();
    symlink(&elsewhere, fx.h(".config/tmux")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    let r = at(&rows, ".config/tmux/tmux.conf");
    assert_eq!(r["status"], "ERROR");
    assert_eq!(r["code"], "E_PARENT_UNSAFE");
    assert_eq!(status(&rows, ".ssh/config").0, "OK", "other rows stay");
}

#[test]
fn without_a_plan_a_linked_parent_of_an_owned_output_is_still_found() {
    let fx = applied();
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
        common::skip("running as root: directory modes do not apply");
        return;
    }
    let fx = applied();
    let dir = fx.h(".config/git");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let (c, rows) = fx.rows();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(c, 1, "{rows:#?}");
    let r = at(&rows, ".config/git/config");
    assert_eq!(r["status"], "UNKNOWN", "{r:#?}");
    assert_eq!(r["code"], "E_OBSERVATION_UNKNOWN");
    assert_eq!(r["required"], true);
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
    let fx = applied();
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
    let fx = applied();
    fs::write(fx.state().join("journal.jsonl"), "garbage\n").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(status(&rows, "journal.jsonl").0, "ERROR");
    assert_eq!(status(&rows, "ledger.json").0, "OK");
    assert_eq!(status(&rows, ".zshrc").0, "OK");
}

#[test]
fn a_missing_profile_keeps_the_state_and_owned_rows_and_labels_them_partial() {
    let fx = applied();
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
    let fx = applied();
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
    let fx = applied();
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
    let fx = Fx::unbound();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(
        rows.iter().find(|r| r["kind"] == "binding").unwrap()["code"],
        "E_BINDING_INVALID"
    );
    let state = rows.iter().find(|r| r["kind"] == "state").unwrap();
    assert_eq!(
        (state["status"].as_str(), state["code"].as_str()),
        (Some("N/A"), Some("D_STATE_ABSENT"))
    );
    let profile = rows.iter().find(|r| r["kind"] == "profile").unwrap();
    assert_eq!(
        (profile["status"].as_str(), profile["code"].as_str()),
        (Some("UNKNOWN"), Some("D_DEPENDS_UNKNOWN"))
    );
}

// ---- not applicable -----------------------------------------------------------------------

#[test]
fn sensitive_resources_are_not_applicable_and_read_no_content() {
    let fx = personal();
    let (_, rows) = fx.rows();
    let na = |c: &str| rows.iter().filter(|r| r["code"] == c).collect::<Vec<_>>();
    assert!(!na("D_NA_SENSITIVE").is_empty(), "{rows:#?}");
    assert!(na("D_NA_SENSITIVE").iter().all(|r| r["status"] == "N/A"));
    assert!(na("D_NA_GENERATED").is_empty(), "{rows:#?}");
    // Tools and applications are not probed: UNKNOWN, and not required.
    let probe = na("D_PROBING_NOT_BUILT");
    assert!(!probe.is_empty());
    assert!(
        probe
            .iter()
            .all(|r| r["status"] == "UNKNOWN" && r["required"] == false)
    );
}

#[test]
fn a_generated_output_reaches_the_pending_and_missing_rows() {
    let fx = personal();
    let (_, rows) = fx.rows();
    assert_eq!(
        status(&rows, ".codex/AGENTS.md"),
        ("WARN", "D_PLAN_PENDING")
    );
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let digest = dfm_model::plan::Saved::parse(&o.stdout)
        .unwrap()
        .plan_digest;
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
    assert_eq!(code(&o), 0, "{}\n{}", text(&o.stderr), text(&o.stdout));
    fs::remove_file(fx.h(".codex/AGENTS.md")).unwrap();
    let (_, rows) = fx.rows();
    assert_eq!(
        status(&rows, ".codex/AGENTS.md"),
        ("WARN", "D_OUTPUT_MISSING")
    );
}

// ---- formats ------------------------------------------------------------------------------

/// `(status, code, kind, subject)` of each row, read from the table. Wrapping drops the spaces
/// at the breaks, so subjects are compared without spaces.
fn table_rows(table: &str) -> Vec<(String, String, String, String)> {
    let known = ["OK", "WARN", "ERROR", "UNKNOWN", "N/A"];
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    let mut in_subject = false;
    for l in table.lines() {
        let mut w = l.split_whitespace();
        let first = w.next();
        if !l.starts_with(' ') && first.is_some_and(|s| known.contains(&s)) {
            let s = first.unwrap().to_string();
            rows.push((
                s,
                w.next().unwrap().into(),
                w.next().unwrap().into(),
                String::new(),
            ));
            in_subject = false;
        } else if let Some(rest) = l.strip_prefix("  subject: ") {
            in_subject = true;
            rows.last_mut().unwrap().3.push_str(&rest.replace(' ', ""));
        } else if l.starts_with("  detail: ") || l.starts_with("  fix: ") || !l.starts_with(' ') {
            in_subject = false;
        } else if in_subject {
            rows.last_mut().unwrap().3.push_str(&l.replace(' ', ""));
        }
    }
    rows
}

#[test]
fn table_status_and_json_agree_on_every_row_for_doctor_and_status() {
    let fx = applied();
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
                r["subject"].as_str().unwrap().replace(' ', ""),
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
    let fx = applied();
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
    let fx = applied();
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
fn untrusted_text_from_a_file_name_is_escaped_in_both_formats() {
    let fx = applied();
    // A receipt file name is untrusted data. It reaches the subject of a WARN row.
    let name = "bad\u{1b}[31mname\u{202e}.json";
    fs::write(fx.state().join("receipts").join(name), "{}").unwrap();
    for extra in [&[][..], &["--json"][..]] {
        let o = fx.doctor(extra);
        let out = text(&o.stdout);
        assert!(
            !out.contains('\u{1b}') && !out.contains('\u{202e}'),
            "{out:?}"
        );
        assert!(
            out.contains("bad\\u{1b}[31mname\\u{202e}.json") || out.contains("bad\\\\u{1b}"),
            "{out}"
        );
    }
    let (_, rows) = fx.rows();
    assert!(
        rows.iter()
            .any(|r| r["subject"].as_str().unwrap().contains("\\u{1b}")),
        "{rows:#?}"
    );
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
    let fx = applied();
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
        7,
        "no new state file: ledger, journal, state.json, salt, 3 dirs"
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

// ---- sensitive paths are never read ---------------------------------------------------------

fn old_atime(p: &Path) -> SystemTime {
    let old = UNIX_EPOCH + Duration::from_secs(946_684_800);
    let f = fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_times(fs::FileTimes::new().set_accessed(old)).unwrap();
    old
}

fn atime(p: &Path) -> SystemTime {
    fs::metadata(p).unwrap().accessed().unwrap()
}

/// The personal example profile, bound in an isolated HOME. Its `.claude/settings.json` is a
/// sensitive resource.
fn personal() -> Fx {
    let src = common::TempDir::new("doctor-ex-src");
    let root = src.path().join("examples");
    common::copy_tree(&common::repo_root().join("examples"), &root);
    let fx = Fx {
        home: common::TempDir::new("doctor-ex-home"),
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

/// Adds a ledger entry with the given class, by editing `ledger.json` as the store wrote it.
fn own(fx: &Fx, resource: &str, class: &str, rel: &str) {
    let f = fx.state().join("ledger.json");
    let mut l: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    let fingerprint = if class == "normal" {
        Value::String("ab".repeat(32))
    } else {
        Value::Null
    };
    l["resources"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "resource": resource, "adapter": "fs-link/v1", "class": class, "op_id": "x-1",
            "owned_outputs": [{"path": fx.h(rel), "mode": 420, "fingerprint": fingerprint}],
        }));
    fs::write(&f, serde_json::to_vec(&l).unwrap()).unwrap();
}

#[test]
fn a_path_a_sensitive_resource_now_claims_is_not_read_even_if_the_ledger_has_a_normal_entry() {
    let fx = personal();
    let target = fx.h(".claude/settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "{\"token\":\"hunter2-secret\"}").unwrap();
    own(&fx, "old-owner", "normal", ".claude/settings.json");
    let before = fingerprint(fx.home.path());
    let old = old_atime(&target);
    for extra in [&["--json"][..], &[][..]] {
        let o = fx.doctor(extra);
        assert!(!text(&o.stdout).contains("hunter2"), "a content leaked");
    }
    assert_eq!(atime(&target), old, "doctor read the sensitive file");
    assert_eq!(fingerprint(fx.home.path()), before);
    let (_, rows) = fx.rows();
    let r = at(&rows, ".claude/settings.json");
    // The planner's blocker (it fails closed on a sensitive destination without a ledger entry
    // of this resource) decides the row. It is never OK, and nothing was hashed.
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("ERROR"), Some("E_CONFLICT")),
        "{r:#?}"
    );
}

#[test]
fn a_case_variant_of_a_sensitive_ledger_entry_is_not_read() {
    let fx = personal();
    // The ledger records the sensitive class for `.CLAUDE/settings.json`. The file on disk is at
    // the lower-case spelling of the same path. (A ledger that lists both spellings is
    // `E_STATE_INVALID`, so only the sensitive one is recorded.)
    let target = fx.h(".claude/settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "secret-bytes").unwrap();
    own(&fx, "vault-owner", "sensitive", ".CLAUDE/settings.json");
    let old = old_atime(&target);
    // Without a profile there is no plan, so only the ledger decides what is sensitive.
    fs::remove_file(fx.root.join("profiles/personal-dev-01.toml")).unwrap();
    let (_, rows) = fx.rows();
    assert_eq!(
        atime(&target),
        old,
        "doctor read a case alias of a sensitive entry"
    );
    assert_eq!(at(&rows, ".CLAUDE/settings.json")["status"], "N/A");
}

#[test]
fn a_sensitive_destination_with_no_read_permission_is_not_applicable_not_an_error() {
    if is_root() {
        common::skip("running as root: file modes do not apply");
        return;
    }
    let fx = personal();
    let target = fx.h(".claude/settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "x").unwrap();
    let old = old_atime(&target);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
    let (_, rows) = fx.rows();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(atime(&target), old);
    assert!(
        rows.iter()
            .all(|r| r["status"] != "OK"
                || !r["subject"].as_str().unwrap().contains("settings.json"))
    );
}

// ---- documented codes -----------------------------------------------------------------------

fn code_of(rows: &[Value], kind: &str) -> (String, String) {
    let r = rows
        .iter()
        .find(|r| r["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} row"));
    (
        r["status"].as_str().unwrap().into(),
        r["code"].as_str().unwrap().into(),
    )
}

fn row_code<'a>(rows: &'a [Value], subject: &str) -> &'a str {
    at(rows, subject)["code"].as_str().unwrap()
}

#[test]
fn an_exact_unowned_link_is_a_pending_warning_that_names_the_adopt_flag() {
    let fx = Fx::new();
    let link = fx.root.join("base/modules/extra/files/zshrc");
    symlink(&link, fx.h(".zshrc")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    // DFM-45: the planner adopts it, so doctor shows a pending plan row, not D_UNRECORDED.
    assert_eq!(status(&rows, ".zshrc"), ("WARN", "D_PLAN_PENDING"));
    let r = at(&rows, ".zshrc");
    assert!(r["detail"].as_str().unwrap().contains("adopt"), "{r}");
    assert!(r["fix"].as_str().unwrap().contains("--allow-adopt"), "{r}");
}

#[test]
fn a_stray_state_entry_is_a_partial_warning() {
    let fx = applied();
    fs::write(fx.state().join("receipts").join(".half.tmp"), "x").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    let r = rows
        .iter()
        .find(|r| r["code"] == "D_STATE_PARTIAL")
        .unwrap();
    assert_eq!(
        (r["status"].as_str(), r["partial"].as_bool()),
        (Some("WARN"), Some(true))
    );
}

#[test]
fn a_staged_copy_of_an_older_format_beside_an_owned_output_is_a_warning_and_is_kept() {
    let fx = applied();
    let (_, rows) = fx.rows();
    let owned = rows
        .iter()
        .find(|r| r["kind"] == "output" && r["status"] == "OK")
        .unwrap()["subject"]
        .as_str()
        .unwrap()
        .to_string();
    let (dir, name) = owned.rsplit_once('/').unwrap_or(("", &owned));
    let parent = if dir.is_empty() { fx.h("") } else { fx.h(dir) };
    let decoy = parent.join(format!(".{name}.1.2.3.tmp"));
    let current = parent.join(format!(".{name}.0123456789abcdef.tmp"));
    fs::write(&decoy, "mine\n").unwrap();
    fs::write(&current, "tagged\n").unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "{rows:#?}");
    let staged: Vec<&Value> = rows
        .iter()
        .filter(|r| r["code"] == "D_STAGED_COPY")
        .collect();
    assert_eq!(staged.len(), 1, "{rows:#?}");
    assert_eq!(staged[0]["status"], "WARN");
    assert!(
        staged[0]["subject"]
            .as_str()
            .unwrap()
            .ends_with(&format!(".{name}.1.2.3.tmp"))
    );
    assert_eq!(
        staged[0]["fix"],
        "A staged copy from an interrupted run; check it, then remove it by hand."
    );
    assert_eq!(
        fs::read_to_string(&decoy).unwrap(),
        "mine\n",
        "doctor deletes nothing"
    );
    assert_eq!(fs::read_to_string(&current).unwrap(), "tagged\n");
}

#[test]
fn a_cut_directory_listing_gets_its_own_partial_warning() {
    let fx = applied();
    let (_, rows) = fx.rows();
    let owned = rows
        .iter()
        .find(|r| r["kind"] == "output" && r["status"] == "OK")
        .unwrap()["subject"]
        .as_str()
        .unwrap()
        .to_string();
    let (dir, name) = owned.rsplit_once('/').unwrap_or(("", &owned));
    let parent = if dir.is_empty() { fx.h("") } else { fx.h(dir) };
    fs::write(parent.join(format!(".{name}.7.8.9.tmp")), "x").unwrap();
    let o = fx.run_env(
        &[("DFM_STRAY_SCAN_MAX", "1")],
        &["doctor", "--root", fx.root_arg(), "--json"],
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let cut: Vec<&Value> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| {
            r["code"] == "D_STAGED_COPY" && r["detail"].as_str().unwrap().contains("was cut")
        })
        .collect();
    let mine: Vec<&&Value> = cut.iter().filter(|r| r["subject"] == dir).collect();
    assert_eq!(mine.len(), 1, "{v:#}");
    let cut = mine;
    assert_eq!(
        (cut[0]["status"].as_str(), cut[0]["partial"].as_bool()),
        (Some("WARN"), Some(true))
    );
    assert_eq!(code(&o), 0, "a WARN row does not change the exit");
}

#[test]
fn no_receipt_is_not_applicable_and_a_failed_last_run_is_a_warning() {
    let fx = Fx::new();
    let (_, rows) = fx.rows();
    assert_eq!(
        code_of(&rows, "receipt"),
        ("N/A".into(), "D_NO_RECEIPT".into())
    );
    let fx = applied();
    let dir = fx.state().join("receipts");
    let f = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let mut r: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    r["result"]["outcome"] = "failed".into();
    r["result"]["exit_code"] = 1.into();
    fs::write(&f, serde_json::to_vec(&r).unwrap()).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 0, "WARN alone exits 0: {rows:#?}");
    assert_eq!(
        code_of(&rows, "receipt"),
        ("WARN".into(), "D_LAST_RUN".into())
    );
}

#[test]
fn a_missing_profile_makes_the_plan_row_unknown_and_dependent() {
    let fx = applied();
    fs::remove_file(fx.root.join("profiles/t-01.toml")).unwrap();
    let (_, rows) = fx.rows();
    assert_eq!(
        code_of(&rows, "plan"),
        ("UNKNOWN".into(), "D_DEPENDS_UNKNOWN".into())
    );
}

#[test]
fn state_loss_and_state_without_a_binding_are_state_invalid() {
    let fx = applied();
    fs::remove_dir_all(fx.h(".local/state/dotfiles-manager")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(
        code_of(&rows, "state"),
        ("ERROR".into(), "E_STATE_INVALID".into())
    );
    assert!(
        at(&rows, "state directory")["detail"]
            .as_str()
            .unwrap()
            .contains("state loss")
    );

    let fx = applied();
    fs::remove_file(fx.h(".config/dotfiles-manager/machine.toml")).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(code_of(&rows, "binding").1, "E_BINDING_INVALID");
    assert_eq!(
        code_of(&rows, "state"),
        ("ERROR".into(), "E_STATE_INVALID".into())
    );
    assert!(
        at(&rows, "state directory")["detail"]
            .as_str()
            .unwrap()
            .contains("no readable binding")
    );
}

#[test]
fn a_redirected_state_directory_is_storage_invalid() {
    let fx = applied();
    let dir = fx.h(".local/state/dotfiles-manager");
    let moved = fx.h(".local/state/moved");
    fs::rename(&dir, &moved).unwrap();
    symlink(&moved, &dir).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(
        code_of(&rows, "state"),
        ("ERROR".into(), "E_STORAGE_INVALID".into())
    );
}

#[test]
fn state_written_for_another_identity_is_an_identity_mismatch() {
    let fx = applied();
    let f = fx.state().join("state.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    v["binding"]["machine_id"] = "another-machine".into();
    fs::write(&f, serde_json::to_vec(&v).unwrap()).unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(row_code(&rows, "state.json"), "E_IDENTITY_MISMATCH");
    assert_eq!(status(&rows, "state.json").0, "ERROR");
}

#[test]
fn a_ledger_path_outside_home_is_refused_and_the_other_rows_stay() {
    let fx = applied();
    let f = fx.state().join("ledger.json");
    let l = fs::read_to_string(&f).unwrap();
    // The scope check refuses a path outside HOME when the ledger is read, so the whole ledger
    // is one ERROR row here. Every other source still has its rows.
    fs::write(
        &f,
        l.replace(fx.home.path().to_str().unwrap(), "/elsewhere"),
    )
    .unwrap();
    let (c, rows) = fx.rows();
    assert_eq!(c, 1);
    assert_eq!(status(&rows, "ledger.json").0, "ERROR");
    assert_eq!(status(&rows, "journal.jsonl").0, "OK");
    assert_eq!(code_of(&rows, "profile").0, "OK");
}

// ---- the lock row ---------------------------------------------------------------------------

fn lock_of(fx: &Fx) -> Value {
    let (_, rows) = fx.rows();
    rows.into_iter().find(|r| r["kind"] == "lock").unwrap()
}

#[test]
fn the_lock_row_handles_every_kind_of_lock_file() {
    let fx = applied();
    let lock = fx.h(".local/state/dotfiles-manager.lock");
    // A note left by the last holder.
    let r = lock_of(&fx);
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("OK"), Some("D_LOCK_NOTE")),
        "{r:#?}"
    );
    // No file.
    fs::remove_file(&lock).unwrap();
    let r = lock_of(&fx);
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("OK"), Some("D_OK"))
    );
    assert!(!lock.exists());
    // An empty file has no note.
    fs::write(&lock, "").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    let r = lock_of(&fx);
    assert_eq!(r["status"], "OK");
    assert!(r["detail"].as_str().unwrap().contains("no holder note"));
    fs::remove_file(&lock).unwrap();
    // A symlink is refused, not followed.
    symlink("/etc/hosts", &lock).unwrap();
    let r = lock_of(&fx);
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("WARN"), Some("E_STORAGE_INVALID")),
        "{r:#?}"
    );
    fs::remove_file(&lock).unwrap();
    // A directory.
    fs::create_dir(&lock).unwrap();
    let r = lock_of(&fx);
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("WARN"), Some("E_STORAGE_INVALID")),
        "{r:#?}"
    );
    fs::remove_dir(&lock).unwrap();
    // A FIFO must neither block doctor nor be read.
    let made = Command::new("mkfifo")
        .arg(&lock)
        .status()
        .is_ok_and(|s| s.success());
    assert!(made, "mkfifo failed");
    let r = lock_of(&fx);
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("WARN"), Some("E_STORAGE_INVALID")),
        "{r:#?}"
    );
    fs::remove_file(&lock).unwrap();
    // A file the user cannot read.
    if is_root() {
        common::skip("running as root: the unreadable lock file case");
        return;
    }
    fs::write(&lock, "pid 1 since x\n").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o000)).unwrap();
    let r = lock_of(&fx);
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("WARN"), Some("E_STORAGE_INVALID")),
        "{r:#?}"
    );
}

#[test]
fn the_lock_row_warns_on_a_mode_or_link_count_that_the_writer_lock_refuses() {
    let fx = applied();
    let lock = fx.h(".local/state/dotfiles-manager.lock");
    let digest = {
        let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
        dfm_model::plan::Saved::parse(&o.stdout)
            .unwrap()
            .plan_digest
    };
    let warns = |detail: &str| {
        let r = lock_of(&fx);
        assert_eq!(
            (r["status"].as_str(), r["code"].as_str()),
            (Some("WARN"), Some("E_STORAGE_INVALID")),
            "{r:#?}"
        );
        assert!(r["detail"].as_str().unwrap().contains(detail), "{r:#?}");
    };
    // A mode with group bits: doctor warns and the writer lock refuses.
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    warns("mode is 644");
    let before = fingerprint(fx.home.path());
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_STORAGE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(
        fingerprint(fx.home.path()),
        before,
        "doctor and the refusal must not write"
    );
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    // A second hard link.
    let second = fx.h(".local/state/second-name");
    fs::hard_link(&lock, &second).unwrap();
    warns("hard link");
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_STORAGE_INVALID") && text(&o.stderr).contains("hard link"),
        "{}",
        text(&o.stderr)
    );
    fs::remove_file(&second).unwrap();
    // A mode with owner bits only is refused too: the writer lock opens the file read-write
    // before it can tighten it, so 0400, 0200 and 0000 fail at the open.
    // Doctor reads the file, so it names the mode only where it can open the file.
    for (mode, detail) in [
        (0o400, "mode is 400"),
        (0o200, "cannot open"),
        (0o000, "cannot open"),
    ] {
        fs::set_permissions(&lock, fs::Permissions::from_mode(mode)).unwrap();
        warns(detail);
        let before = fingerprint(fx.home.path());
        let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
        assert_eq!(code(&o), 3, "{mode:o}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains("E_STORAGE_INVALID"),
            "{mode:o}: {}",
            text(&o.stderr)
        );
        assert_eq!(fingerprint(fx.home.path()), before, "{mode:o}");
    }
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(lock_of(&fx)["status"], "OK");
}

// ---- docs -----------------------------------------------------------------------------------

/// The `D_*` codes in the first column of the code table in docs/doctor.md.
fn documented_codes() -> Vec<String> {
    let doc = fs::read_to_string(common::repo_root().join("docs/doctor.md")).unwrap();
    let table = doc.split("\n## Codes\n").nth(1).expect("a Codes section");
    let table = table.split("\n## ").next().unwrap();
    table
        .lines()
        .filter_map(|l| l.strip_prefix("| `"))
        .filter_map(|l| l.split_once('`'))
        .map(|(c, _)| c.to_string())
        .filter(|c| c.starts_with("D_"))
        .collect()
}

#[test]
fn the_documented_codes_are_exactly_the_registered_codes() {
    let mut docs = documented_codes();
    let mut registered: Vec<String> = DoctorCode::ALL
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();
    docs.sort();
    registered.sort();
    assert_eq!(docs, registered);
    let doc = fs::read_to_string(common::repo_root().join("docs/doctor.md")).unwrap();
    for c in DoctorCode::ALL {
        assert!(
            doc.contains(c.description()),
            "the description of {} is not in the docs",
            c.as_str()
        );
    }
}

#[test]
fn a_ledger_entry_owned_by_a_selected_sensitive_resource_is_not_read() {
    let fx = personal();
    let target = fx.h(".zz-other");
    fs::write(&target, "secret-bytes").unwrap();
    // The owner is a selected sensitive resource that no longer claims this path.
    own(&fx, "agents.claude-settings", "normal", ".zz-other");
    let before = fingerprint(fx.home.path());
    let old = old_atime(&target);
    let (_, rows) = fx.rows();
    assert_eq!(atime(&target), old, "doctor read the file");
    assert_eq!(fingerprint(fx.home.path()), before);
    let r = at(&rows, ".zz-other");
    assert_eq!(
        (r["status"].as_str(), r["code"].as_str()),
        (Some("N/A"), Some("D_NA_SENSITIVE")),
        "{r:#?}"
    );
}

// ---- unusable HOME (DFM-57 C1) --------------------------------------------------------------

fn bad_home(args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env_clear()
        .args(["--home", "/a b"])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn doctor_and_status_report_an_unusable_home_as_rows_and_exit_1() {
    let fx = Fx::new();
    for cmd in ["doctor", "status"] {
        let o = bad_home(&[cmd, "--root", fx.root_arg(), "--json"]);
        assert_eq!(code(&o), 1, "{cmd}: {}", text(&o.stderr));
        assert!(text(&o.stderr).contains("E_DIAGNOSED"), "{cmd}");
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        let rows = v["rows"].as_array().unwrap();
        let first = &rows[0];
        assert_eq!(
            (first["kind"].as_str(), first["status"].as_str()),
            (Some("binding"), Some("ERROR"))
        );
        assert_eq!(first["code"], "E_HOME_MISMATCH");
        // HOME mismatch has more than one cause, so the fix names the whole rule.
        let fix = first["fix"].as_str().unwrap();
        assert!(fix.contains("password-database home equals HOME"), "{fix}");
        let t = bad_home(&[cmd, "--root", fx.root_arg()]);
        assert_eq!(code(&t), 1, "{cmd}");
        assert!(
            text(&t.stdout).contains("ERROR   E_HOME_MISMATCH binding"),
            "{cmd}"
        );
    }
}

#[test]
fn a_usage_error_wins_over_an_unusable_home() {
    let o = bad_home(&["doctor"]);
    assert_eq!(code(&o), 2, "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("--root is required"));
    assert!(o.stdout.is_empty());
}

#[test]
fn an_unusable_root_is_a_row_and_exit_1_whether_or_not_home_is_usable() {
    let fx = Fx::new();
    let o = fx.run(&["doctor", "--root", "/nonexistent-dfm-root"]);
    assert_eq!(code(&o), 1, "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("ERROR   E_REPORT_UNAVAILABLE profile"));
    let o = bad_home(&["doctor", "--root", "/nonexistent-dfm-root"]);
    assert_eq!(code(&o), 1, "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("E_DIAGNOSED"));
    assert!(text(&o.stdout).contains("ERROR   E_HOME_MISMATCH binding"));
}

#[test]
fn a_path_handed_to_a_new_owner_is_pending_not_gone() {
    let fx = personal();
    own(&fx, "old-owner", "normal", ".codex/AGENTS.md");
    let (_, rows) = fx.rows();
    assert_eq!(
        status(&rows, ".codex/AGENTS.md"),
        ("WARN", "D_PLAN_PENDING")
    );
    let detail = at(&rows, ".codex/AGENTS.md")["detail"].as_str().unwrap();
    assert!(
        detail.contains("owned by old-owner") && detail.contains("takes the path over"),
        "{detail}"
    );
}
