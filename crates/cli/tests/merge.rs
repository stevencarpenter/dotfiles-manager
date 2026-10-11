//! `claude-settings/v1` merge through the isolated binary (DFM-21, docs/adapters.md).
//!
//! Every run passes `--home <temp dir>` and an empty environment. No test touches the real HOME.

mod common;

use common::{TempDir, copy_tree, repo_root};
use dfm_model::plan::Saved;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");
const TOKEN: &str = "PLANTED-TOKEN-7f3a9c";

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
"#;

const AGENTS: &str = r#"schema = 1
id = "agents"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "claude-settings"
kind = "merge"
adapter = "claude-settings/v1"
target = ".claude/settings.json"
[resources.value.managed]
outputStyle = "STE"
[resources.value.seed]
model = "example-model"
[resources.value.handlers.audit]
event = "PreToolUse"
type = "command"
argv = ["audit", "--quiet"]
[resources.policy]
defaults = ["/managed/outputStyle"]
extensions = ["/handlers"]
"#;

const BUNDLE: &str = r#"schema = 1
id = "t-base"
core = "modules/core/module.toml"

[modules]
agents = "modules/agents/module.toml"
"#;

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
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

fn hex(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}

impl Fx {
    fn new() -> Fx {
        let (home, src) = (TempDir::new("merge-home"), TempDir::new("merge-src"));
        let root = src.path().join("root");
        let fx = Fx {
            home,
            _src: src,
            root,
        };
        fx.put("base/bundle.toml", BUNDLE);
        fx.put("base/modules/core/module.toml", CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/agents/module.toml", AGENTS);
        fx.set_profile("\"agents\"");
        let p = fx.root.join("profiles/t-01.toml");
        let o = fx.run(&[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-test",
        ]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        fx
    }

    fn set_profile(&self, modules: &str) {
        self.put(
            "profiles/t-01.toml",
            &format!(
                "schema = 1\nid = \"t-01\"\nbundle = \"../base/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [{modules}]\noverrides = []\n"
            ),
        );
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

    fn settings(&self) -> PathBuf {
        self.h(".claude/settings.json")
    }

    /// Writes the live settings file with mode 0600.
    fn live(&self, content: &str) {
        let p = self.settings();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn live_json(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.settings()).unwrap()).unwrap()
    }

    fn mode(&self) -> u32 {
        fs::metadata(self.settings()).unwrap().permissions().mode() & 0o777
    }

    fn saved(&self) -> Saved {
        let o = self.run(&["plan", "--root", self.root_arg(), "--json"]);
        Saved::parse(&o.stdout).unwrap_or_else(|e| panic!("{e}\n{}", text(&o.stdout)))
    }

    fn plan(&self) -> Output {
        self.run(&["plan", "--root", self.root_arg()])
    }

    /// Applies the current plan. A take-over of an existing file is approved by id, as a person
    /// would paste the flag the refusal prints.
    fn apply(&self) -> Output {
        let saved = self.saved();
        let ids: Vec<String> = saved
            .plan
            .operations
            .iter()
            .filter(|o| o.takeover)
            .map(|o| o.id.clone())
            .collect();
        let mut a = vec![
            "apply".to_string(),
            "--root".into(),
            self.root_arg().into(),
            "--yes-plan".into(),
            saved.plan_digest,
        ];
        for id in ids {
            a.push("--allow-adopt".into());
            a.push(id);
        }
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        self.run(&a)
    }

    /// Applies with the digest and no allowance.
    fn apply_bare(&self) -> Output {
        let d = self.saved().plan_digest;
        self.apply_with(&d)
    }

    fn apply_with(&self, digest: &str) -> Output {
        self.run(&["apply", "--root", self.root_arg(), "--yes-plan", digest])
    }

    fn state(&self) -> PathBuf {
        self.h(".local/state/dotfiles-manager/state")
    }

    fn ledger(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.state().join("ledger.json")).unwrap()).unwrap()
    }

    fn receipts(&self) -> usize {
        fs::read_dir(self.state().join("receipts")).unwrap().count()
    }

    fn journal_text(&self) -> String {
        fs::read_to_string(self.state().join("journal.jsonl")).unwrap()
    }

    /// The ledger entry of the merge output, if any.
    fn coowned(&self) -> Option<serde_json::Value> {
        self.ledger()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["resource"] == "agents.claude-settings")
            .map(|r| r["owned_outputs"][0].clone())
    }

    fn module(&self, f: impl FnOnce(String) -> String) {
        let p = self.root.join("base/modules/agents/module.toml");
        fs::write(&p, f(fs::read_to_string(&p).unwrap())).unwrap();
    }
}

/// Every regular file below `dir`, recursively.
fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(files_below(&p));
        } else {
            out.push(p);
        }
    }
    out
}

const LIVE: &str = r#"{
  "model": "mine",
  "outputStyle": "other",
  "apiKeyHelper": "PLANTED-TOKEN-7f3a9c",
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "foreign-hook" } ] }
    ],
    "Stop": [ { "hooks": [ { "type": "command", "command": "other" } ] } ]
  }
}
"#;

// ---- merge rules -------------------------------------------------------------------------

#[test]
fn foreign_keys_and_hooks_stay_managed_is_overridden_and_seed_only_fills_a_gap() {
    let fx = Fx::new();
    fx.live(LIVE);
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(
        j["outputStyle"], "STE",
        "managed value overrides the live one"
    );
    assert_eq!(j["model"], "mine", "seed never overwrites");
    assert_eq!(j["apiKeyHelper"], TOKEN, "a foreign key is carried over");
    assert_eq!(j["hooks"]["Stop"][0]["hooks"][0]["command"], "other");
    let pre = j["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0]["hooks"][0]["command"], "foreign-hook");
    assert_eq!(pre[1]["hooks"][0]["command"], "audit --quiet");
    assert_eq!(fx.mode(), 0o600);
    // Formatting is normalized: sorted keys, 2-space indent, a final newline.
    let t = fs::read_to_string(fx.settings()).unwrap();
    assert!(t.starts_with("{\n  \"apiKeyHelper\""), "{t}");
    assert!(t.ends_with("}\n"));
}

#[test]
fn an_absent_file_is_created_with_mode_600_seed_and_handler() {
    let fx = Fx::new();
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(j["outputStyle"], "STE");
    assert_eq!(j["model"], "example-model");
    assert_eq!(
        j["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "audit --quiet"
    );
    assert_eq!(fx.mode(), 0o600);
    let c = fx.coowned().expect("the merge output is in the ledger");
    assert!(c["fingerprint"].is_null(), "no content fingerprint: {c}");
    assert_eq!(c["coowned"]["entries"][0]["id"], "audit");
    assert_eq!(c["coowned"]["managed_hash"].as_str().unwrap().len(), 64);
}

#[test]
fn a_rerun_changes_nothing_and_writes_no_receipt() {
    let fx = Fx::new();
    fx.live(LIVE);
    assert_eq!(code(&fx.apply()), 0);
    let (bytes, receipts, ledger) = (
        fs::read(fx.settings()).unwrap(),
        fx.receipts(),
        fs::read(fx.state().join("ledger.json")).unwrap(),
    );
    let mtime = fs::metadata(fx.settings()).unwrap().modified().unwrap();
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
    assert_eq!(fs::read(fx.settings()).unwrap(), bytes);
    assert_eq!(
        fs::metadata(fx.settings()).unwrap().modified().unwrap(),
        mtime
    );
    assert_eq!(fx.receipts(), receipts);
    assert_eq!(fs::read(fx.state().join("ledger.json")).unwrap(), ledger);
}

#[test]
fn an_explicit_disable_is_written_as_declared() {
    let fx = Fx::new();
    fx.module(|m| {
        m.replace(
            "outputStyle = \"STE\"",
            "outputStyle = \"STE\"\nenableAllProjectMcpServers = false",
        )
    });
    fx.live("{\"enableAllProjectMcpServers\": true}\n");
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(fx.live_json()["enableAllProjectMcpServers"], false);
}

#[test]
fn a_changed_declared_value_is_applied_on_the_next_run_without_a_rebuild() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply()), 0);
    let bin = fs::read(BIN).unwrap();
    fx.module(|m| {
        m.replace("\"STE\"", "\"plain\"")
            .replace("--quiet", "--loud")
    });
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(j["outputStyle"], "plain");
    let pre = j["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "the old owned entry was replaced: {j}");
    assert_eq!(pre[0]["hooks"][0]["command"], "audit --loud");
    assert_eq!(fs::read(BIN).unwrap(), bin);
}

// ---- blocked inputs ----------------------------------------------------------------------

#[test]
fn a_live_file_open_to_group_or_others_blocks_with_fix_text_and_nothing_changes() {
    let fx = Fx::new();
    fx.live(LIVE);
    fs::set_permissions(fx.settings(), fs::Permissions::from_mode(0o644)).unwrap();
    let before = fs::read(fx.settings()).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(stderr(&o).contains("chmod 600"), "{}", stderr(&o));
    assert!(!stderr(&o).contains(TOKEN) && !text(&o.stdout).contains(TOKEN));
    let a = fx.run(&[
        "apply",
        "--root",
        fx.root_arg(),
        "--yes-plan",
        &"0".repeat(64),
    ]);
    assert_eq!(code(&a), 3);
    assert_eq!(fs::read(fx.settings()).unwrap(), before);
    assert_eq!(
        fx.mode(),
        0o644,
        "the manager does not change a mode it did not set"
    );
}

#[test]
fn malformed_json_blocks_with_e_conflict_and_the_last_good_file_stays() {
    let fx = Fx::new();
    fx.live(LIVE);
    assert_eq!(code(&fx.apply()), 0);
    let good = fs::read(fx.settings()).unwrap();
    let (receipts, journal) = (fx.receipts(), fx.journal_text());
    let bad = format!("{{\"apiKeyHelper\": \"{TOKEN}\" oops");
    fx.live(&bad);
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT") && stderr(&o).contains("not valid JSON"));
    assert!(!stderr(&o).contains(TOKEN) && !text(&o.stdout).contains(TOKEN));
    // Apply plans again and refuses. It writes nothing.
    let a = fx.run(&[
        "apply",
        "--root",
        fx.root_arg(),
        "--yes-plan",
        &"0".repeat(64),
    ]);
    assert_eq!(code(&a), 3);
    assert_eq!(fs::read_to_string(fx.settings()).unwrap(), bad);
    assert_eq!(fx.receipts(), receipts);
    assert_eq!(fx.journal_text(), journal);
    // Repair it by hand from the last good file: the next run succeeds.
    fs::write(fx.settings(), &good).unwrap();
    assert_eq!(code(&fx.apply()), 0);
}

#[test]
fn a_symlinked_settings_file_is_not_followed() {
    let fx = Fx::new();
    let elsewhere = fx.h("elsewhere.json");
    fs::write(&elsewhere, "{\"apiKeyHelper\": \"x\"}\n").unwrap();
    fs::create_dir_all(fx.h(".claude")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, fx.settings()).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert_eq!(
        fs::read_to_string(&elsewhere).unwrap(),
        "{\"apiKeyHelper\": \"x\"}\n"
    );
}

// ---- foreign edits of managed fields -----------------------------------------------------

#[test]
fn an_edit_to_a_managed_field_is_a_conflict_at_plan_and_the_fix_is_to_adopt_it() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply()), 0);
    let mut j = fx.live_json();
    j["outputStyle"] = serde_json::json!("hand-edited-secret-style");
    fx.live(&serde_json::to_string_pretty(&j).unwrap());
    let before = fs::read(fx.settings()).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(stderr(&o).contains("Fix:"), "{}", stderr(&o));
    let all = format!("{}{}", stderr(&o), text(&o.stdout));
    assert!(
        !all.contains("hand-edited-secret-style"),
        "no value is shown: {all}"
    );
    let a = fx.run(&[
        "apply",
        "--root",
        fx.root_arg(),
        "--yes-plan",
        &"0".repeat(64),
    ]);
    assert_eq!(code(&a), 3);
    assert_eq!(fs::read(fx.settings()).unwrap(), before);
    // The documented fix: declare the edited value in the module. The manager then adopts it.
    fx.module(|m| m.replace("\"STE\"", "\"hand-edited-secret-style\""));
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.live_json()["outputStyle"], "hand-edited-secret-style");
}

#[test]
fn a_removed_managed_field_is_also_a_foreign_edit() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply()), 0);
    let mut j = fx.live_json();
    j.as_object_mut().unwrap().remove("outputStyle");
    fx.live(&serde_json::to_string_pretty(&j).unwrap());
    // The declared value is not live and differs from what the manager wrote: conflict.
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"));
}

/// Starts `apply` parked at `point`, runs `during` while it waits, then releases it.
fn parked_apply(fx: &Fx, point: &str, digest: &str, during: impl FnOnce(&Fx)) -> Output {
    let gate = fx.home.path().parent().unwrap().join(format!(
        "gate-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let gate = PathBuf::from(gate.to_str().unwrap().replace(['(', ')'], ""));
    let _ = fs::remove_file(&gate);
    let _ = fs::remove_file(gate.with_extension("reached"));
    let child = Command::new(BIN)
        .env_clear()
        .env("DFM_FAILPOINT", format!("{point}:wait-{}", gate.display()))
        .arg("--home")
        .arg(fx.home.path())
        .args(["apply", "--root", fx.root_arg(), "--yes-plan", digest])
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
    during(fx);
    fs::write(&gate, "").unwrap();
    let out = child.wait_with_output().unwrap();
    let _ = fs::remove_file(&gate);
    let _ = fs::remove_file(&reached);
    out
}

/// A machine that has applied once and has a pending declared change, so the next apply writes.
fn pending() -> (Fx, String) {
    let fx = Fx::new();
    fx.live(LIVE);
    assert_eq!(code(&fx.apply()), 0);
    fx.module(|m| m.replace("\"STE\"", "\"next\""));
    let d = fx.saved().plan_digest;
    (fx, d)
}

#[test]
fn an_edit_between_plan_and_apply_is_a_conflict_at_apply() {
    let (fx, d) = pending();
    let o = parked_apply(&fx, "merge-before-read", &d, |fx| {
        let mut j = fx.live_json();
        j["outputStyle"] = serde_json::json!("changed-while-parked");
        fx.live(&serde_json::to_string(&j).unwrap());
    });
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(!stderr(&o).contains("changed-while-parked"));
    assert_eq!(fx.live_json()["outputStyle"], "changed-while-parked");
}

#[test]
fn a_foreign_edit_of_another_key_while_apply_waits_is_merged_not_lost() {
    let (fx, d) = pending();
    let o = parked_apply(&fx, "merge-before-read", &d, |fx| {
        let mut j = fx.live_json();
        j["addedByTheApp"] = serde_json::json!("keep-me");
        fx.live(&serde_json::to_string(&j).unwrap());
    });
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(
        j["addedByTheApp"], "keep-me",
        "the result is made from the re-read file"
    );
    assert_eq!(j["outputStyle"], "next");
}

#[test]
fn a_file_replaced_between_the_re_read_and_the_rename_is_refused() {
    let (fx, d) = pending();
    let o = parked_apply(&fx, "file-after-tmp", &d, |fx| {
        // A new inode takes the name, as an editor that saves by rename would do.
        let tmp = fx.h(".claude/app-save.tmp");
        fs::write(&tmp, "{\"saved\": \"by-the-app\"}\n").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&tmp, fx.settings()).unwrap();
    });
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(
        fs::read_to_string(fx.settings()).unwrap(),
        "{\"saved\": \"by-the-app\"}\n",
        "the replacement stays"
    );
    let stray: Vec<_> = fs::read_dir(fx.h(".claude"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(stray, ["settings.json"], "the staged file was removed");
    // The ledger still holds the earlier state: the failed write recorded nothing.
    assert_eq!(
        fx.coowned().unwrap()["coowned"]["entries"][0]["id"],
        "audit"
    );
}

#[test]
fn a_write_in_place_between_the_re_read_and_the_rename_is_refused() {
    let (fx, d) = pending();
    let o = parked_apply(&fx, "file-after-tmp", &d, |fx| {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(fx.settings())
            .unwrap();
        f.write_all(b"\n").unwrap();
    });
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(
        fx.live_json()["outputStyle"],
        "STE",
        "the live file is as the app left it"
    );
}

// ---- deselection -------------------------------------------------------------------------

#[test]
fn deselecting_the_resource_retires_only_the_owned_handlers() {
    let fx = Fx::new();
    fx.live(LIVE);
    assert_eq!(code(&fx.apply()), 0);
    fx.set_profile("");
    let o = fx.plan();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains("retire 1 owned handler(s)"), "{out}");
    assert!(
        out.contains("the file, its managed and seeded values"),
        "{out}"
    );
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(fx.settings().exists(), "the file is never deleted");
    let j = fx.live_json();
    assert_eq!(j["outputStyle"], "STE", "managed values stay");
    assert_eq!(j["model"], "mine", "seeded values stay");
    assert_eq!(j["apiKeyHelper"], TOKEN);
    let pre = j["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "{j}");
    assert_eq!(pre[0]["hooks"][0]["command"], "foreign-hook");
    assert!(fx.coowned().is_none(), "the resource left the ledger");
    // A rerun has nothing to do.
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
}

#[test]
fn retiring_needs_no_removal_flag_and_never_deletes_a_file_that_only_held_our_entries() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply()), 0);
    fx.set_profile("");
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert!(
        j.get("hooks").is_none(),
        "the emptied hooks object is dropped: {j}"
    );
    assert_eq!(j["outputStyle"], "STE");
}

// ---- redaction and the salted hash -------------------------------------------------------

#[test]
fn nothing_but_the_ledger_holds_the_salted_hash_and_no_output_shows_content() {
    let fx = Fx::new();
    fx.live(LIVE);
    let live_hash = hex(LIVE.as_bytes());
    let mut seen_text = String::new();
    let mut collect = |o: &Output| {
        seen_text.push_str(&text(&o.stdout));
        seen_text.push_str(&stderr(o));
    };
    collect(&fx.plan());
    collect(&fx.run(&["plan", "--root", fx.root_arg(), "--json"]));
    let d = fx.saved().plan_digest;
    collect(&fx.run(&["apply", "--root", fx.root_arg(), "--preview"]));
    collect(&fx.apply_with(&d));
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    collect(&o);
    for args in [
        vec!["doctor", "--root", fx.root_arg()],
        vec!["doctor", "--root", fx.root_arg(), "--json"],
        vec!["status", "--root", fx.root_arg()],
        vec!["plan", "--root", fx.root_arg()],
    ] {
        collect(&fx.run(&args));
    }
    // A refusal on a malformed file prints no content either.
    let live = fs::read_to_string(fx.settings()).unwrap();
    fx.live(&format!("{live} {TOKEN} trailing"));
    collect(&fx.plan());
    fx.live(&live);

    let hash = fx.coowned().unwrap()["coowned"]["managed_hash"]
        .as_str()
        .unwrap()
        .to_string();
    let salt = fs::read(fx.state().join("salt")).unwrap();
    assert_eq!(salt.len(), 32);
    // The raw salt bytes must be absent from every output and every state file but the salt.
    let salt_hex_lower: String = salt.iter().map(|b| format!("{b:02x}")).collect();
    assert!(!seen_text.contains(&salt_hex_lower), "salt hex in output");
    for f in files_below(&fx.state()) {
        if f.file_name().unwrap() != "salt" {
            let b = fs::read(&f).unwrap();
            assert!(
                !b.windows(salt.len()).any(|w| w == salt.as_slice()),
                "{} holds the raw salt",
                f.display()
            );
            assert!(
                !String::from_utf8_lossy(&b).contains(&salt_hex_lower),
                "{} holds the salt in hex",
                f.display()
            );
        }
    }
    for (what, needle) in [
        ("planted token", TOKEN.to_string()),
        ("salted hash", hash.clone()),
        ("salt", hex(&salt)),
        ("hash of the original file", live_hash.clone()),
    ] {
        assert!(
            !seen_text.contains(&needle),
            "{what} appeared in command output"
        );
    }
    for f in files_below(&fx.state()) {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let b = fs::read(&f).unwrap();
        let t = String::from_utf8_lossy(&b);
        assert!(!t.contains(TOKEN), "{name} holds the planted token");
        assert!(
            !t.contains(&live_hash) && !t.contains(&hex(live.as_bytes())),
            "{name} holds a whole-file hash"
        );
        if name != "salt" {
            assert!(!t.contains(&hex(&salt)), "{name} holds the salt");
        }
        if name != "ledger.json" {
            assert!(!t.contains(&hash), "{name} holds the salted hash");
        }
    }
    // The ledger holds it exactly once, and never a plain fingerprint for the file.
    let ledger = fs::read_to_string(fx.state().join("ledger.json")).unwrap();
    assert_eq!(ledger.matches(&hash).count(), 1);
    assert!(ledger.contains("\"fingerprint\":null"));
}

#[test]
fn the_plan_shows_the_declared_digest_and_names_but_no_output_hash_or_value() {
    let fx = Fx::new();
    fx.live(LIVE);
    let s = fx.saved();
    let op = s
        .plan
        .operations
        .iter()
        .find(|o| o.resource == "agents.claude-settings")
        .unwrap();
    assert_eq!(op.class, dfm_model::state::Class::Sensitive);
    assert_eq!(
        op.output_sha256, None,
        "a sensitive output has no content hash"
    );
    assert_eq!(op.mode, Some(0o600));
    assert_eq!(
        op.managed_sha256.as_deref(),
        Some(hex(b"{\"outputStyle\":\"STE\"}").as_str())
    );
    assert_eq!(op.sets, ["model", "outputStyle"]);
    let json = text(&fx.run(&["plan", "--root", fx.root_arg(), "--json"]).stdout);
    assert!(
        !json.contains(TOKEN) && !json.contains("foreign-hook") && !json.contains("example-model")
    );
    let t = text(&fx.plan().stdout);
    assert!(t.contains("managed: sha256:"), "{t}");
    assert!(t.contains("sets: model, outputStyle"), "{t}");
}

#[test]
fn the_salt_is_created_once_private_and_unchanged_and_state_check_accepts_it() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply()), 0);
    let salt = fx.state().join("salt");
    assert_eq!(
        fs::metadata(&salt).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let first = fs::read(&salt).unwrap();
    assert_eq!(first.len(), 32);
    fx.module(|m| m.replace("\"STE\"", "\"two\""));
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(fs::read(&salt).unwrap(), first);
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
}

// ---- the example profile -----------------------------------------------------------------

#[test]
fn the_example_personal_profile_plans_and_applies_unmodified() {
    let src = TempDir::new("merge-example");
    let root = src.path().join("examples");
    copy_tree(&repo_root().join("examples"), &root);
    let fx = Fx {
        home: TempDir::new("merge-example-home"),
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
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = fx.plan();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(j["outputStyle"], "STE");
    assert_eq!(j["model"], "example-model");
    assert!(j.get("hooks").is_none(), "the example declares no handler");
    assert_eq!(fx.mode(), 0o600);
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
}

// ---- doctor ------------------------------------------------------------------------------

fn settings_row(fx: &Fx) -> (i32, serde_json::Value) {
    let o = fx.run(&["doctor", "--root", fx.root_arg(), "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let row = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["subject"] == ".claude/settings.json")
        .cloned()
        .unwrap_or_else(|| panic!("no row: {v}"));
    (code(&o), row)
}

#[test]
fn doctor_reports_the_merge_output_from_the_comparison_and_never_shows_content() {
    let fx = Fx::new();
    fx.live(LIVE);
    assert_eq!(code(&fx.apply()), 0);
    let (c, row) = settings_row(&fx);
    assert_eq!(c, 0, "{row:#}");
    assert_eq!(row["status"], "N/A", "{row:#}");
    let mut j = fx.live_json();
    j["outputStyle"] = serde_json::json!("edited-by-hand");
    fx.live(&serde_json::to_string(&j).unwrap());
    let (c, row) = settings_row(&fx);
    assert_eq!(c, 1);
    assert_eq!(row["status"], "ERROR", "{row:#}");
    assert_eq!(row["code"], "E_CONFLICT");
    let all = row.to_string();
    assert!(
        !all.contains("edited-by-hand") && !all.contains(TOKEN),
        "{all}"
    );
}

// ---- fix round: delete and recreate, take-over approval, salt, target, ownership ----------

#[test]
fn a_deleted_settings_file_is_created_again_and_the_hash_is_recorded_anew() {
    let fx = Fx::new();
    fx.live(LIVE);
    assert_eq!(code(&fx.apply()), 0);
    let old = fx.coowned().unwrap()["coowned"]["managed_hash"].clone();
    fs::remove_file(fx.settings()).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("create .claude/settings.json"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.apply_bare();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(j["outputStyle"], "STE");
    assert_eq!(j["model"], "example-model", "the file is rebuilt from {{}}");
    assert_eq!(fx.mode(), 0o600);
    let c = fx.coowned().unwrap();
    assert_eq!(
        c["coowned"]["managed_hash"], old,
        "same salt and values give the same hash"
    );
    let o = fx.apply_bare();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
}

#[test]
fn an_existing_file_with_no_ledger_entry_needs_allow_adopt_and_an_absent_one_does_not() {
    let fx = Fx::new();
    fx.live(LIVE);
    let before = fs::read(fx.settings()).unwrap();
    let o = fx.apply_bare();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_ADOPT_UNAPPROVED"), "{}", stderr(&o));
    assert!(stderr(&o).contains("--allow-adopt"), "{}", stderr(&o));
    assert_eq!(fs::read(fx.settings()).unwrap(), before);
    assert_eq!(fx.receipts(), 0);
    assert!(fx.coowned().is_none());
    let t = text(&fx.plan().stdout);
    assert!(
        t.contains("take over managed fields of an existing file"),
        "{t}"
    );
    assert!(
        t.contains("hand edits of managed fields are overwritten"),
        "{t}"
    );
    // With the flag it writes, and the receipt records how the approval was given.
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.live_json()["outputStyle"], "STE");
    let r = fs::read_dir(fx.state().join("receipts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let r: serde_json::Value = serde_json::from_slice(&fs::read(r.path()).unwrap()).unwrap();
    assert_eq!(r["approvals"][0]["how"], "allow_adopt");
    // The next run needs no flag: the ledger records the file now.
    fx.module(|m| m.replace("\"STE\"", "\"next\""));
    let o = fx.apply_bare();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // An absent file needs no flag.
    let fresh = Fx::new();
    let o = fresh.apply_bare();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
}

#[test]
fn a_module_that_names_another_target_is_refused_at_resolution() {
    let fx = Fx::new();
    fx.module(|m| {
        m.replace(
            "target = \".claude/settings.json\"",
            "target = \".claude/other.json\"",
        )
    });
    let o = fx.plan();
    assert_ne!(code(&o), 0);
    let e = stderr(&o);
    assert!(
        e.contains("E_MANIFEST_INVALID") && e.contains(".claude/settings.json"),
        "{e}"
    );
}

#[test]
fn an_identical_hook_present_at_take_over_is_owned_by_the_approval_and_retire_removes_it() {
    let fx = Fx::new();
    let same =
        r#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"audit --quiet","type":"command"}]}]}}"#;
    fx.live(same);
    // The first write into an existing file needs --allow-adopt. That approval covers an
    // identical entry already in place: after an interrupted first merge it is the manager's.
    assert_eq!(code(&fx.apply()), 0);
    let c = fx.coowned().unwrap();
    assert_eq!(c["coowned"]["entries"].as_array().unwrap().len(), 1, "{c}");
    assert_eq!(
        fx.live_json()["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fx.set_profile("");
    let o = fx.apply_bare();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        fx.live_json().get("hooks").is_none(),
        "the owned entry is retired: {}",
        fx.live_json()
    );
}

// The salt: missing, short, open to others, or a link. Each is a hard state error for plan and
// apply, with the recovery text, and `state check` reports it.
fn bad_salt(change: impl FnOnce(&Path)) {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply()), 0);
    let salt = fx.state().join("salt");
    change(&salt);
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let e = stderr(&o);
    assert!(e.contains("from a copy"), "{e}");
    assert!(e.contains("coowned"), "names the ledger key: {e}");
    let a = fx.run(&[
        "apply",
        "--root",
        fx.root_arg(),
        "--yes-plan",
        &"0".repeat(64),
    ]);
    assert_eq!(code(&a), 3, "{}", stderr(&a));
    let c = fx.run(&["state", "check"]);
    assert_eq!(code(&c), 1, "{}\n{}", stderr(&c), text(&c.stdout));
    let all = format!("{}{}", stderr(&c), text(&c.stdout));
    assert!(all.contains("salt"), "{all}");
}

#[test]
fn a_missing_salt_with_coowned_entries_is_a_hard_state_error() {
    bad_salt(|p| fs::remove_file(p).unwrap());
}

#[test]
fn a_16_byte_salt_is_refused() {
    bad_salt(|p| fs::write(p, [7u8; 16]).unwrap());
}

#[test]
fn a_salt_open_to_others_is_refused() {
    bad_salt(|p| fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap());
}

#[test]
fn a_symlinked_salt_is_refused() {
    bad_salt(|p| {
        let real = p.with_file_name("salt-real");
        fs::rename(p, &real).unwrap();
        std::os::unix::fs::symlink(&real, p).unwrap();
    });
}

#[test]
fn the_recovery_text_for_an_unusable_salt_works_when_followed_literally() {
    let fx = Fx::new();
    assert_eq!(code(&fx.apply()), 0);
    let salt = fx.state().join("salt");
    fs::set_permissions(&salt, fs::Permissions::from_mode(0o644)).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    let e = stderr(&o);
    assert!(e.contains("move it out of state/"), "{e}");
    assert!(e.contains("The ledger records a salted hash"), "{e}");
    assert!(e.contains("delete the `coowned` object"), "{e}");
    // Step 1: move the bad salt aside. Step 2: delete every coowned object.
    fs::rename(&salt, fx.h("salt.bad")).unwrap();
    let path = fx.state().join("ledger.json");
    let mut l: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for r in l["resources"].as_array_mut().unwrap() {
        for o in r["owned_outputs"].as_array_mut().unwrap() {
            o.as_object_mut().unwrap().remove("coowned");
        }
    }
    fs::write(&path, serde_json::to_vec(&l).unwrap()).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let c = fx.run(&["state", "check"]);
    assert_eq!(code(&c), 0, "{}\n{}", stderr(&c), text(&c.stdout));
    // The file is a take-over again, and the next apply makes a new salt.
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(fs::read(&salt).unwrap().len(), 32);
}

#[test]
fn the_text_without_a_recorded_hash_says_the_next_apply_creates_a_salt() {
    let fx = Fx::new();
    fs::create_dir_all(fx.state()).unwrap();
    fs::write(fx.state().join("salt"), [1u8; 16]).unwrap();
    fs::set_permissions(fx.state().join("salt"), fs::Permissions::from_mode(0o600)).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    let e = stderr(&o);
    assert!(e.contains("The ledger records no salted hash"), "{e}");
    assert!(!e.contains("The ledger records a salted hash"), "{e}");
}

fn retire_with(change: impl FnOnce(&Path)) {
    let fx = Fx::new();
    fx.live(LIVE);
    assert_eq!(code(&fx.apply()), 0);
    change(&fx.state().join("salt"));
    fx.set_profile("");
    let o = fx.plan();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("retire 1 owned handler(s)"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.apply_bare();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let pre = fx.live_json()["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(pre, 1, "only the owned entry is gone");
    assert!(fx.coowned().is_none());
}

#[test]
fn a_retire_needs_no_salt_even_when_it_is_open_to_others() {
    retire_with(|p| fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap());
}

#[test]
fn a_retire_needs_no_salt_even_when_it_is_missing() {
    retire_with(|p| fs::remove_file(p).unwrap());
}

// ---- recovery of an unknown merge outcome (DFM-19) -----------------------------------------

#[test]
fn a_merge_that_ends_unknown_stays_open_is_never_auto_owned_and_clears_only_by_hand() {
    let fx = Fx::new();
    let saved = fx.saved();
    let o = Command::new(BIN)
        .env_clear()
        .env("DFM_FAILPOINT", "file-after-rename:error")
        .arg("--home")
        .arg(fx.home.path())
        .args([
            "apply",
            "--root",
            fx.root_arg(),
            "--yes-plan",
            &saved.plan_digest,
        ])
        .output()
        .unwrap();
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_IO_UNCONFIRMED"), "{}", stderr(&o));
    let id = fx
        .journal_text()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|e| e["phase"] == "unknown")
        .unwrap()["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(fx.coowned().is_none(), "unknown records no ownership");
    assert!(fx.settings().exists(), "the file is in place");

    // Settling cannot decide a sensitive path, so it stays open: recover reports it, exit 3.
    let o = fx.run(&["state", "recover"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_OP_INTERRUPTED") && stderr(&o).contains(&id));
    assert!(
        text(&o.stdout).contains(&format!("open {id}")),
        "{}",
        text(&o.stdout)
    );
    assert!(fx.coowned().is_none());
    // An apply is blocked on the path, names the id and leaves the file as it is.
    let before = fs::read(fx.settings()).unwrap();
    let o = fx.apply();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_OP_INTERRUPTED"), "{}", stderr(&o));
    assert!(stderr(&o).contains(&id), "{}", stderr(&o));
    assert_eq!(hex(&fs::read(fx.settings()).unwrap()), hex(&before));
    // `done` is refused: nothing about a sensitive file can be checked.
    let o = fx.run(&["state", "resolve", &id, "--as", "done"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(fx.coowned().is_none());
    // `failed` clears it. The file is still there with no ledger entry, so the plan takes it
    // over with the usual approval, and the machine is clean afterwards.
    let o = fx.run(&["state", "resolve", &id, "--as", "failed"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(code(&fx.run(&["state", "recover"])), 0);
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let c = fx.coowned().unwrap();
    // The take-over approval covers the manager's own hook that the interrupted run left in
    // the file: it is recorded, so a later retire removes it.
    assert_eq!(c["coowned"]["entries"].as_array().unwrap().len(), 1, "{c}");
    let o = fx.apply_bare();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    fx.set_profile("");
    let o = fx.apply_bare();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(fx.live_json().get("hooks").is_none(), "{}", fx.live_json());
}

// ---- parity with the nix merge (DFM-48) --------------------------------------------------
//
// The fixture is `examples/base/modules/claude-base`: the shape of the base settings file the
// nix merge starts from, with placeholder values. These tests port the cases of
// `test-claude-settings-order.sh` and `test-claude-hooks-preservation.sh` that the manager's
// rules share. Two cases do not carry over: nix sweeps unrecorded hooks by command marker, and
// the manager removes only entries its ledger records (docs/adapters.md, rule 4); nix keeps the
// first-seen key order, and the manager writes sorted keys (rule 7).

const BASE_MODULE: &str = include_str!("../../../examples/base/modules/claude-base/module.toml");
const BASE_ID: &str = "claude-base.claude-settings";

impl Fx {
    /// Selects the `claude-base` fixture instead of `agents`.
    fn base() -> Fx {
        let fx = Fx::new();
        fx.put("base/modules/claude-base/module.toml", BASE_MODULE);
        fx.put(
            "base/bundle.toml",
            &BUNDLE.replace(
                "agents = \"modules/agents/module.toml\"",
                "agents = \"modules/agents/module.toml\"\nclaude-base = \"modules/claude-base/module.toml\"",
            ),
        );
        fx.set_profile("\"claude-base\"");
        fx
    }

    fn base_module(&self, f: impl FnOnce(String) -> String) {
        let p = self.root.join("base/modules/claude-base/module.toml");
        fs::write(&p, f(fs::read_to_string(&p).unwrap())).unwrap();
    }

    fn fragment(&self, name: &str, content: &str) {
        let d = self.h(".claude/settings.d");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), content).unwrap();
    }

    fn coowned_of(&self, id: &str) -> serde_json::Value {
        self.ledger()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["resource"] == id)
            .map(|r| r["owned_outputs"][0].clone())
            .unwrap_or_else(|| panic!("no ledger entry for {id}"))
    }
}

fn commands(j: &serde_json::Value, event: &str) -> Vec<String> {
    j["hooks"][event]
        .as_array()
        .map(|a| {
            a.iter()
                .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
                .filter_map(|h| h["command"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn allow_write(j: &serde_json::Value) -> Vec<String> {
    serde_json::from_value(j["sandbox"]["filesystem"]["allowWrite"].clone()).unwrap_or_default()
}

#[test]
fn the_fixture_creates_every_shape_of_the_base_settings_file() {
    let fx = Fx::base();
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(j["env"]["EXAMPLE_FEATURE_FLAG"], "1");
    assert_eq!(j["permissions"]["defaultMode"], "auto");
    assert_eq!(j["statusLine"]["type"], "command");
    assert_eq!(
        j["enabledPlugins"]["example-experiment@example-marketplace"],
        false
    );
    assert_eq!(j["skillOverrides"]["example-skill"], "off");
    assert_eq!(
        j["extraKnownMarketplaces"]["example-marketplace"]["source"]["repo"],
        "example-org/example-marketplace"
    );
    assert_eq!(j["outputStyle"], "STE");
    assert_eq!(j["verbose"], false);
    assert_eq!(j["model"], "example-model");
    assert_eq!(j["effortLevel"], "high");
    assert_eq!(allow_write(&j).len(), 3);
    let pre = &j["hooks"]["PreToolUse"][0];
    assert_eq!(pre["matcher"], "Bash");
    assert_eq!(
        pre["hooks"][0]["command"],
        "\"${HOME}/.claude/hooks/no-em-dash-commit.sh\""
    );
    assert_eq!(j["hooks"]["SubagentStop"].as_array().unwrap().len(), 2);
    assert_eq!(j["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 20);
    assert_eq!(fx.mode(), 0o600);
    // A rerun changes nothing and writes no receipt.
    let receipts = fx.receipts();
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fx.receipts(), receipts);
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
}

#[test]
fn a_plugin_the_user_enabled_in_claude_code_survives_apply() {
    let fx = Fx::base();
    fx.live(
        r#"{"enabledPlugins":{"foo@elsewhere":true,"example-tools@example-marketplace":false},"env":{"MY_VAR":"1"}}"#,
    );
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(
        j["enabledPlugins"]["foo@elsewhere"], true,
        "foreign leaf survives"
    );
    assert_eq!(
        j["enabledPlugins"]["example-tools@example-marketplace"], true,
        "a managed leaf wins"
    );
    assert_eq!(j["env"]["MY_VAR"], "1");
    assert_eq!(j["env"]["EXAMPLE_MAX_WORKERS"], "64");
    // The user enables another plugin later. That is no foreign edit of a managed field.
    let mut j = fx.live_json();
    j["enabledPlugins"]["later@elsewhere"] = serde_json::json!(true);
    fx.live(&serde_json::to_string(&j).unwrap());
    let o = fx.plan();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.live_json()["enabledPlugins"]["later@elsewhere"], true);
}

#[test]
fn a_user_sandbox_path_survives_and_the_declared_ones_are_appended_once() {
    let fx = Fx::base();
    fx.live(
        r#"{"sandbox":{"filesystem":{"allowWrite":["/user/path","~/projects/example"],"denyRead":["/x"]},"enabled":true}}"#,
    );
    assert_eq!(code(&fx.apply()), 0);
    let aw = allow_write(&fx.live_json());
    assert_eq!(
        aw,
        [
            "/user/path",
            "~/projects/example",
            "~/.cache/example-tool",
            "~/.local/share/example/tools"
        ],
        "user paths stay, an identical path is not duplicated, the rest are appended in order"
    );
    assert_eq!(fx.live_json()["sandbox"]["filesystem"]["denyRead"][0], "/x");
    assert_eq!(fx.live_json()["sandbox"]["enabled"], true);
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(allow_write(&fx.live_json()), aw, "a rerun appends nothing");
    // The file existed, so this was the approved take-over: every declared item already in place
    // is recorded as owned with the two the manager added (docs/adapters.md, rule 4).
    let c = fx.coowned_of(BASE_ID);
    assert_eq!(c["coowned"]["appended"].as_array().unwrap().len(), 3);
    assert_eq!(
        c["coowned"]["appended"][0]["pointer"],
        "/sandbox/filesystem/allowWrite"
    );
}

#[test]
fn a_dropped_declared_path_is_removed_and_a_user_path_is_not() {
    let fx = Fx::base();
    fx.live(r#"{"sandbox":{"filesystem":{"allowWrite":["/user/path"]}}}"#);
    assert_eq!(code(&fx.apply()), 0);
    fx.base_module(|m| m.replace(r#""~/projects/example", "#, ""));
    assert_eq!(code(&fx.apply()), 0);
    let aw = allow_write(&fx.live_json());
    assert_eq!(
        aw,
        [
            "/user/path",
            "~/.cache/example-tool",
            "~/.local/share/example/tools"
        ]
    );
    // The user adds a path of their own, then deselects the module: only ours go.
    let mut j = fx.live_json();
    j["sandbox"]["filesystem"]["allowWrite"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("/user/second"));
    fx.live(&serde_json::to_string(&j).unwrap());
    fx.set_profile("");
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(
        allow_write(&fx.live_json()),
        ["/user/path", "/user/second"],
        "retire removes the appended items and keeps the user's"
    );
}

#[test]
fn a_retired_managed_hook_is_removed_and_a_foreign_hook_stays() {
    let fx = Fx::base();
    fx.live(
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"/opt/other/agent-state.sh"}]}],
        "Stop":[{"hooks":[{"type":"command","command":"/opt/other/stop.sh"}]}]}}"#,
    );
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(commands(&fx.live_json(), "Stop").len(), 2);
    // The module drops the Stop handler: the manager removes its entry and nothing else.
    fx.base_module(|m| {
        let i = m.find("[resources.value.handlers.journal-stop]").unwrap();
        let j = m[i..]
            .find("[resources.value.handlers.journal-subagent-stop]")
            .unwrap();
        format!("{}{}", &m[..i], &m[i + j..])
    });
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(commands(&j, "Stop"), ["/opt/other/stop.sh"]);
    assert_eq!(commands(&j, "SessionStart"), ["/opt/other/agent-state.sh"]);
    assert_eq!(commands(&j, "WorktreeCreate").len(), 1, "the others stay");
}

#[test]
fn foreign_hooks_of_any_type_and_their_group_metadata_survive() {
    let fx = Fx::base();
    fx.live(
        r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[
          {"type":"command","command":"/opt/herdr/pre.sh"},
          {"type":"prompt","prompt":"Review this write"},
          {"type":"agent","prompt":"Validate this write"},
          {"type":"http","url":"https://localhost/hook"},
          {"type":"mcp_tool","server":"guard","tool":"check","input":{"path":"x"}}]}]}}"#,
    );
    assert_eq!(code(&fx.apply()), 0);
    let j = fx.live_json();
    let groups = j["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(
        groups.len(),
        2,
        "the managed group is added beside the foreign one"
    );
    assert_eq!(groups[0]["matcher"], "Write");
    let mut types: Vec<&str> = groups[0]["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["type"].as_str().unwrap())
        .collect();
    types.sort_unstable();
    assert_eq!(types, ["agent", "command", "http", "mcp_tool", "prompt"]);
    let managed: Vec<_> = groups.iter().filter(|g| g["matcher"] == "Bash").collect();
    assert_eq!(managed.len(), 1, "the managed hook is present exactly once");
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(
        fx.live_json()["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_changed_handler_replaces_the_old_entry_and_no_stale_one_remains() {
    let fx = Fx::base();
    assert_eq!(code(&fx.apply()), 0);
    fx.base_module(|m| m.replace("wt-create.sh", "wt-create-v2.sh"));
    assert_eq!(code(&fx.apply()), 0);
    let cmds = commands(&fx.live_json(), "WorktreeCreate");
    assert_eq!(cmds, ["\"${HOME}/.claude/hooks/wt-create-v2.sh\""]);
}

#[test]
fn keys_the_managed_block_never_sets_keep_their_values() {
    let fx = Fx::base();
    fx.live(
        r#"{"$schema":"https://json.schemastore.org/claude-code-settings.json","model":"haiku","theme":"dark","editorMode":"vim","effortLevel":"medium","permissions":{"defaultMode":"acceptEdits"},"teammateMode":"tmux","voiceEnabled":true,"sandbox":{"filesystem":{"allowWrite":["/fixture/user-write"]}}}"#,
    );
    assert_eq!(code(&fx.apply()), 0);
    let j = fx.live_json();
    for (k, v) in [
        ("theme", "dark"),
        ("editorMode", "vim"),
        ("model", "haiku"),
        ("effortLevel", "medium"),
    ] {
        assert_eq!(j[k], v, "in-tool key {k} was changed");
    }
    assert_eq!(j["voiceEnabled"], true);
    assert_eq!(j["teammateMode"], "tmux");
    assert!(allow_write(&j).contains(&"/fixture/user-write".to_string()));
    // `permissions.defaultMode` is managed: the declared value wins over the live one.
    assert_eq!(j["permissions"]["defaultMode"], "auto");
}

#[test]
fn seeds_apply_only_when_absent_and_a_later_in_tool_choice_stays() {
    let fx = Fx::base();
    fx.live(r#"{"model":"mine"}"#);
    assert_eq!(code(&fx.apply()), 0);
    let j = fx.live_json();
    assert_eq!(j["model"], "mine");
    assert_eq!(j["effortLevel"], "high");
    let mut j = fx.live_json();
    j["effortLevel"] = serde_json::json!("low");
    fx.live(&serde_json::to_string(&j).unwrap());
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(fx.live_json()["effortLevel"], "low");
}

#[test]
fn a_fragment_overrides_a_base_value_and_adds_keys() {
    let fx = Fx::base();
    fx.fragment(
        "10-local.json",
        r#"{"outputStyle":"plain","env":{"EXAMPLE_MAX_WORKERS":"8","FROM_FRAGMENT":"1"},"fragmentKey":true}"#,
    );
    fx.fragment("20-later.json", r#"{"env":{"FROM_FRAGMENT":"2"}}"#);
    let plan = fx.plan();
    let out = text(&plan.stdout);
    assert_eq!(code(&plan), 0, "{}\n{out}", stderr(&plan));
    assert!(
        out.contains("fragments: 10-local.json, 20-later.json"),
        "{out}"
    );
    assert!(
        out.contains("env.FROM_FRAGMENT") && out.contains("fragmentKey"),
        "{out}"
    );
    assert!(!out.contains("plain"), "no value is shown");
    assert_eq!(code(&fx.apply()), 0);
    let j = fx.live_json();
    assert_eq!(j["outputStyle"], "plain");
    assert_eq!(j["env"]["EXAMPLE_MAX_WORKERS"], "8");
    assert_eq!(j["env"]["FROM_FRAGMENT"], "2", "the later name wins");
    assert_eq!(j["env"]["EXAMPLE_FEATURE_FLAG"], "1", "base leaves stay");
    assert_eq!(j["fragmentKey"], true);
    // The plan JSON lists each fragment with its hash. The hash is of the fragment file.
    let saved = fx.saved();
    let op = saved
        .plan
        .operations
        .iter()
        .find(|o| o.managed_sha256.is_some())
        .unwrap();
    let names: Vec<&str> = op.fragments.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["10-local.json", "20-later.json"]);
    assert!(
        op.fragments
            .iter()
            .all(|f| f.origin == "foreign" && f.sha256.is_some())
    );
    // The managed fields include the fragments: editing a fragment value is not a conflict,
    // and editing the live value of a fragment key is one.
    assert_eq!(code(&fx.plan()), 0);
    let mut j = fx.live_json();
    j["fragmentKey"] = serde_json::json!(false);
    fx.live(&serde_json::to_string(&j).unwrap());
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
}

#[test]
fn removing_a_fragment_restores_the_base_value_on_the_next_apply() {
    let fx = Fx::base();
    fx.fragment("10-local.json", r#"{"outputStyle":"plain"}"#);
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(fx.live_json()["outputStyle"], "plain");
    fs::remove_file(fx.h(".claude/settings.d/10-local.json")).unwrap();
    // The live value still equals what the manager wrote, so it is no foreign edit: the next
    // apply writes the base value again.
    let o = fx.plan();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(fx.live_json()["outputStyle"], "STE");
}

#[test]
fn a_malformed_fragment_blocks_with_nothing_written_and_no_content_shown() {
    let fx = Fx::base();
    assert_eq!(code(&fx.apply()), 0);
    let before = fs::read(fx.settings()).unwrap();
    let receipts = fx.receipts();
    fx.fragment("01-good.json", r#"{"goodKey":true}"#);
    fx.fragment("02-bad.json", &format!("{{\"k\": \"{TOKEN}\" oops"));
    let o = fx.plan();
    let all = format!("{}{}", text(&o.stdout), stderr(&o));
    assert_eq!(code(&o), 3, "{all}");
    assert!(
        all.contains("E_CONFLICT") && all.contains("02-bad.json"),
        "{all}"
    );
    assert!(!all.contains(TOKEN), "no fragment content is shown");
    let d = fx.saved().plan_digest;
    let o = fx.apply_with(&d);
    let all = format!("{}{}", text(&o.stdout), stderr(&o));
    assert_ne!(code(&o), 0, "{all}");
    assert!(!all.contains(TOKEN));
    assert_eq!(
        fs::read(fx.settings()).unwrap(),
        before,
        "the last good file stays"
    );
    assert_eq!(fx.receipts(), receipts);
    // Fixing the fragment unblocks the next run.
    fx.fragment("02-bad.json", r#"{"fixed":true}"#);
    assert_eq!(code(&fx.apply()), 0);
    let j = fx.live_json();
    assert_eq!(
        (j["goodKey"].clone(), j["fixed"].clone()),
        (true.into(), true.into())
    );
}

#[test]
fn a_fragment_that_is_not_an_object_or_sets_hooks_blocks() {
    for (content, why) in [
        ("[1,2]", "not an object"),
        (r#"{"hooks":{}}"#, "sets hooks"),
    ] {
        let fx = Fx::base();
        fx.fragment("01-x.json", content);
        let o = fx.plan();
        let all = format!("{}{}", text(&o.stdout), stderr(&o));
        assert_eq!(code(&o), 3, "{all}");
        assert!(all.contains("01-x.json") && all.contains(why), "{all}");
        assert!(!fx.settings().exists());
    }
}

#[test]
fn a_fragment_that_changes_between_plan_and_apply_is_stale() {
    let fx = Fx::base();
    fx.fragment("10-a.json", r#"{"fragmentKey":1}"#);
    let d = fx.saved().plan_digest;
    fx.fragment("10-a.json", r#"{"fragmentKey":2}"#);
    let o = fx.apply_with(&d);
    assert_ne!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!fx.settings().exists(), "nothing was written");
    // A fragment that appears after the plan is stale too.
    let d = fx.saved().plan_digest;
    fx.fragment("11-new.json", r#"{"added":true}"#);
    let o = fx.apply_with(&d);
    assert_ne!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!fx.settings().exists());
}

#[test]
fn a_fragment_made_by_a_link_resource_is_layered_like_a_user_fragment() {
    let fx = Fx::base();
    fx.put(
        "base/modules/claude-base/files/20-module.json",
        r#"{"fromModule":"yes","env":{"EXAMPLE_MAX_WORKERS":"2"}}"#,
    );
    fx.base_module(|m| {
        format!(
            "{m}\n[[resources]]\nid = \"fragment\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".claude/settings.d/20-module.json\"\n[resources.value]\nsource = \"files/20-module.json\"\n"
        )
    });
    let o = fx.plan();
    let out = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{out}", stderr(&o));
    assert!(out.contains("fragments: 20-module.json"), "{out}");
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let j = fx.live_json();
    assert_eq!(j["fromModule"], "yes");
    assert_eq!(j["env"]["EXAMPLE_MAX_WORKERS"], "2");
    let receipts = fx.receipts();
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fx.receipts(), receipts, "a rerun changes nothing");
}

#[test]
fn a_managed_object_over_a_foreign_scalar_blocks_and_the_file_is_unchanged() {
    // The nix normalization keeps the existing file when `sandbox.filesystem` has the wrong type.
    for live in [
        r#"{"sandbox":{"filesystem":"invalid"}}"#,
        r#"{"sandbox":{"filesystem":{"allowWrite":"PLANTED-TOKEN-7f3a9c"}}}"#,
        r#"{"env":"PLANTED-TOKEN-7f3a9c"}"#,
    ] {
        let fx = Fx::base();
        fx.live(live);
        let o = fx.plan();
        let all = format!("{}{}", text(&o.stdout), stderr(&o));
        assert_eq!(code(&o), 3, "{live}: {all}");
        assert!(all.contains("E_CONFLICT") && !all.contains(TOKEN), "{all}");
        let d = fx.saved().plan_digest;
        let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &d]);
        assert_ne!(code(&o), 0);
        assert_eq!(fs::read_to_string(fx.settings()).unwrap(), live);
    }
}

#[test]
fn a_hand_edit_of_a_nested_managed_leaf_is_a_conflict_and_a_new_sibling_is_not() {
    let fx = Fx::base();
    assert_eq!(code(&fx.apply()), 0);
    let mut j = fx.live_json();
    j["env"]["MY_NEW_VAR"] = serde_json::json!("1");
    fx.live(&serde_json::to_string(&j).unwrap());
    assert_eq!(code(&fx.plan()), 0, "a sibling the user added is no edit");
    j["env"]["EXAMPLE_FEATURE_FLAG"] = serde_json::json!("0");
    fx.live(&serde_json::to_string(&j).unwrap());
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"));
}

#[test]
fn a_take_over_of_an_existing_file_records_only_the_items_it_added() {
    let fx = Fx::base();
    fx.live(r#"{"sandbox":{"filesystem":{"allowWrite":["~/.cache/example-tool"]}},"foo":1}"#);
    let saved = fx.saved();
    assert!(saved.plan.operations.iter().any(|o| o.takeover));
    // Without the flag the apply is refused.
    let o = fx.apply_bare();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    // With the flag every identical item already in place is recorded as owned (rule 4).
    assert_eq!(code(&fx.apply()), 0);
    let c = fx.coowned_of(BASE_ID);
    assert_eq!(c["coowned"]["appended"].as_array().unwrap().len(), 3);
}

#[test]
fn no_foreign_nested_content_reaches_any_output_or_state_file() {
    let fx = Fx::base();
    fx.live(&format!(
        r#"{{"env":{{"SECRET":"{TOKEN}"}},"enabledPlugins":{{"x@y":true}},"mcpServers":{{"s":{{"headers":{{"Authorization":"Bearer {TOKEN}"}}}}}},"sandbox":{{"filesystem":{{"allowWrite":["/{TOKEN}"]}}}}}}"#
    ));
    fx.fragment("10-a.json", r#"{"fragmentKey":true}"#);
    let mut seen = String::new();
    for args in [
        vec!["plan", "--root", fx.root_arg()],
        vec!["plan", "--root", fx.root_arg(), "--json"],
    ] {
        let o = fx.run(&args);
        seen.push_str(&text(&o.stdout));
        seen.push_str(&stderr(&o));
    }
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    seen.push_str(&text(&o.stdout));
    seen.push_str(&stderr(&o));
    for args in [
        vec!["doctor", "--root", fx.root_arg()],
        vec!["status", "--root", fx.root_arg()],
        vec!["plan", "--root", fx.root_arg()],
    ] {
        let o = fx.run(&args);
        seen.push_str(&text(&o.stdout));
        seen.push_str(&stderr(&o));
    }
    assert!(
        !seen.contains(TOKEN),
        "a foreign value appeared in command output"
    );
    let hash = fx.coowned_of(BASE_ID)["coowned"]["managed_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !seen.contains(&hash),
        "the salted hash appeared in command output"
    );
    for f in files_below(&fx.state()) {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let t = String::from_utf8_lossy(&fs::read(&f).unwrap()).into_owned();
        assert!(!t.contains(TOKEN), "{name} holds foreign content");
        if name != "ledger.json" {
            assert!(!t.contains(&hash), "{name} holds the salted hash");
        }
    }
    assert!(
        fs::read_to_string(fx.state().join("ledger.json"))
            .unwrap()
            .contains(&hash)
    );
}

#[test]
fn hooks_the_nix_merge_wrote_in_another_spelling_are_never_duplicated() {
    let fx = Fx::base();
    // The eighth hook of the owner's set: a SessionStart handler.
    fx.base_module(|m| {
        format!(
            "{m}\n[resources.value.handlers.session-routing]\nevent = \"SessionStart\"\ntype = \"command\"\nargv = [\"${{HOME}}/.claude/hooks/emit-routing-context.sh\"]\n"
        )
    });
    let home = fx.home.path().to_str().unwrap().to_string();
    let h = |c: &str| serde_json::json!({"type": "command", "command": c});
    // As the nix merge writes them: `~/` spelling, the two SubagentStop hooks in one group, and
    // one entry with the absolute home path. WorktreeRemove is missing on purpose.
    let live_hooks = serde_json::json!({
        "PreToolUse": [{"matcher": "Bash", "hooks": [h("~/.claude/hooks/no-em-dash-commit.sh")]}],
        "WorktreeCreate": [{"hooks": [h("~/.claude/hooks/wt-create.sh")]}],
        "Stop": [{"hooks": [h("$HOME/.claude/hooks/agent-journal-stop.sh")]}],
        "SubagentStop": [{"hooks": [
            h("~/.claude/hooks/agent-journal-stop.sh"),
            {"type": "command", "command": "~/.claude/hooks/agent-reap-subagent-stop.sh", "timeout": 20}
        ]}],
        "SessionEnd": [{"hooks": [
            {"type": "command", "command": "'~/.claude/hooks/agent-reap-session-end.sh'", "timeout": 20}
        ]}],
        "SessionStart": [{"hooks": [h(&format!("{home}/.claude/hooks/emit-routing-context.sh"))]}]
    });
    fx.live(&serde_json::json!({ "hooks": live_hooks }).to_string());
    assert_eq!(code(&fx.apply()), 0);
    let j = fx.live_json();
    for ev in [
        "PreToolUse",
        "WorktreeCreate",
        "Stop",
        "SubagentStop",
        "SessionEnd",
        "SessionStart",
    ] {
        assert_eq!(
            j["hooks"][ev], live_hooks[ev],
            "{ev} was changed or duplicated"
        );
    }
    // Only the missing hook was added, and it is the only entry the manager owns.
    assert_eq!(
        commands(&j, "WorktreeRemove"),
        ["\"${HOME}/.claude/hooks/wt-remove.sh\""]
    );
    let owned = fx.coowned_of(BASE_ID)["coowned"]["entries"].clone();
    assert_eq!(owned.as_array().unwrap().len(), 1, "{owned}");
    assert_eq!(owned[0]["id"], "worktree-remove");
    // A rerun changes nothing.
    let receipts = fx.receipts();
    let o = fx.apply();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fx.receipts(), receipts);
    assert_eq!(fx.live_json()["hooks"], j["hooks"]);
    // Deselecting removes the entry the manager added and no other.
    fx.set_profile("");
    assert_eq!(code(&fx.apply()), 0);
    let j = fx.live_json();
    assert!(j["hooks"].get("WorktreeRemove").is_none());
    for ev in [
        "PreToolUse",
        "WorktreeCreate",
        "Stop",
        "SubagentStop",
        "SessionEnd",
        "SessionStart",
    ] {
        assert_eq!(
            j["hooks"][ev], live_hooks[ev],
            "{ev} was touched by the retire"
        );
    }
}

#[test]
fn a_hook_with_another_matcher_is_a_different_hook() {
    let fx = Fx::base();
    fx.live(
        r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"~/.claude/hooks/no-em-dash-commit.sh"}]}]}}"#,
    );
    assert_eq!(code(&fx.apply()), 0);
    assert_eq!(
        fx.live_json()["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
