//! `codex-hooks/v1` merge of `~/.codex/hooks.json` through the isolated binary (DFM-21 C01 and
//! C05, docs/adapters.md). The adapter runs the `claude-settings/v1` merge engine with handlers
//! only, so these tests cover what differs: the target, foreign hooks of every shape, and the
//! last-good file after a missing overlay.
//!
//! Every run passes `--home <temp dir>` and an empty environment. No test touches the real HOME.

mod common;

use common::{BIN, TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const AGENTS: &str = r#"schema = 1
id = "agents"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "codex-hooks"
kind = "merge"
adapter = "codex-hooks/v1"
target = ".codex/hooks.json"
[resources.value.handlers.hippo]
event = "SessionStart"
type = "command"
argv = ["${HOME}/.local/share/hippo-brain/shell/session-hook.sh"]
[resources.policy]
extensions = ["/handlers"]

[[resources]]
id = "claude-settings"
kind = "merge"
adapter = "claude-settings/v1"
target = ".claude/settings.json"
[resources.value.managed]
outputStyle = "STE"
[resources.value.handlers.audit]
event = "PreToolUse"
type = "command"
argv = ["audit"]
[resources.policy]
extensions = ["/handlers"]
"#;

/// An overlay module in an extending bundle that adds one handler to each co-owned file.
const WORK: &str = r#"schema = 1
id = "work"
platforms = ["macos", "linux"]
requires = ["agents"]

[[contributions]]
resource = "agents.codex-hooks"
path = "/handlers"
key = "work.session"
reason = "Add the work session hook."
[contributions.value]
event = "SessionStart"
type = "command"
argv = ["work-session"]

[[contributions]]
resource = "agents.claude-settings"
path = "/handlers"
key = "work.session"
reason = "Add the work session hook."
[contributions.value]
event = "SessionStart"
type = "command"
argv = ["work-session"]
"#;

const DECLARED: &str = "\"${HOME}/.local/share/hippo-brain/shell/session-hook.sh\"";

/// Foreign groups in the shapes Codex and its plugins write: extra keys on a command hook
/// (`command_windows`, `commandWindows`, `statusMessage`, `timeout`) and hook types the manager
/// never builds.
const LIVE: &str = r#"{
  "hooks": {
    "SessionStart": [
      { "matcher": "startup|resume", "hooks": [
        { "type": "command", "command": "'/opt/cbm' hook-augment", "command_windows": "& '/opt/cbm' hook-augment", "timeout": 5 } ] }
    ],
    "PostToolUse": [
      { "matcher": "Edit|Write|apply_patch", "hooks": [
        { "type": "command", "command": "check", "commandWindows": "check.cmd", "timeout": 5, "statusMessage": "Checking UI changes" },
        { "type": "prompt", "prompt": "Review this write" },
        { "type": "agent", "prompt": "Validate this write" },
        { "type": "http", "url": "https://localhost/hook" } ] }
    ]
  },
  "futureKey": { "kept": true }
}
"#;

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let (home, src) = (TempDir::new("codex-home"), TempDir::new("codex-src"));
        let root = src.path().join("root");
        let fx = Fx {
            home,
            _src: src,
            root,
        };
        fx.put(
            "base/bundle.toml",
            "schema = 1\nid = \"t-base\"\ncore = \"modules/core/module.toml\"\n\n[modules]\nagents = \"modules/agents/module.toml\"\n",
        );
        fx.put("base/modules/core/module.toml", common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/agents/module.toml", AGENTS);
        fx.put(
            "ext/bundle.toml",
            "schema = 1\nid = \"t-ext\"\nextends = \"../base/bundle.toml\"\n\n[modules]\nwork = \"modules/work/module.toml\"\n",
        );
        fx.put("ext/modules/work/module.toml", WORK);
        fx.set_profile("base", "\"agents\"");
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
        assert_eq!(code(&o), 0, "{}", text(&o.stderr));
        fx
    }

    fn set_profile(&self, bundle: &str, modules: &str) {
        self.put(
            "profiles/t-01.toml",
            &format!(
                "schema = 1\nid = \"t-01\"\nbundle = \"../{bundle}/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [{modules}]\noverrides = []\n"
            ),
        );
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear().arg("--home").arg(self.home.path()).args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    fn hooks(&self) -> PathBuf {
        self.home.path().join(".codex/hooks.json")
    }

    fn live(&self, content: &str) {
        let p = self.hooks();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.hooks()).unwrap()).unwrap()
    }

    fn ledger(&self) -> PathBuf {
        self.home
            .path()
            .join(".local/state/dotfiles-manager/state/ledger.json")
    }

    fn plan(&self) -> Output {
        self.run(&["plan", "--root", self.root_arg()])
    }

    fn digest(&self) -> (String, Vec<String>) {
        let o = self.run(&["plan", "--root", self.root_arg(), "--json"]);
        let v: serde_json::Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("{e}\n{}\n{}", text(&o.stdout), text(&o.stderr)));
        let adopt = v["plan"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["takeover"] == true)
            .map(|o| o["id"].as_str().unwrap().to_string())
            .collect();
        (v["plan_digest"].as_str().unwrap().to_string(), adopt)
    }

    /// Applies the current plan, approving each take-over by id as the refusal says to.
    fn apply(&self) -> Output {
        let (d, adopt) = self.digest();
        let mut a = vec!["apply", "--root", self.root_arg(), "--yes-plan", &d];
        for id in &adopt {
            a.extend(["--allow-adopt", id]);
        }
        self.run(&a)
    }

    fn apply_bare(&self) -> Output {
        let (d, _) = self.digest();
        self.run(&["apply", "--root", self.root_arg(), "--yes-plan", &d])
    }
}

fn ok(o: &Output) {
    assert_eq!(code(o), 0, "{}\n{}", text(&o.stderr), text(&o.stdout));
}

/// The groups of `event` whose hooks run `command`.
fn running(j: &serde_json::Value, event: &str, command: &str) -> usize {
    j["hooks"][event].as_array().map_or(0, |gs| {
        gs.iter()
            .filter(|g| {
                g["hooks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|h| h["command"] == command)
            })
            .count()
    })
}

#[test]
fn foreign_groups_keep_every_key_and_the_declared_entry_is_added_once() {
    let fx = Fx::new();
    fx.live(LIVE);
    let before: serde_json::Value = serde_json::from_str(LIVE).unwrap();
    ok(&fx.apply());
    let j = fx.json();
    assert_eq!(j["futureKey"], before["futureKey"]);
    assert_eq!(j["hooks"]["PostToolUse"], before["hooks"]["PostToolUse"]);
    assert_eq!(
        j["hooks"]["SessionStart"][0], before["hooks"]["SessionStart"][0],
        "the foreign group comes first and keeps command_windows and timeout"
    );
    assert_eq!(running(&j, "SessionStart", DECLARED), 1);
    assert_eq!(
        fs::metadata(fx.hooks()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A second run changes nothing and adds no second copy.
    let bytes = fs::read(fx.hooks()).unwrap();
    ok(&fx.apply());
    assert_eq!(fs::read(fx.hooks()).unwrap(), bytes);
}

#[test]
fn an_existing_file_needs_allow_adopt_and_an_absent_one_is_created() {
    let fx = Fx::new();
    fx.live(LIVE);
    let o = fx.apply_bare();
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_ADOPT_UNAPPROVED"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(fs::read_to_string(fx.hooks()).unwrap(), LIVE);
    ok(&fx.apply());
    let fresh = Fx::new();
    ok(&fresh.apply_bare());
    let j = fresh.json();
    assert_eq!(running(&j, "SessionStart", DECLARED), 1);
    assert_eq!(j.as_object().unwrap().len(), 1, "only hooks: {j}");
}

#[test]
fn deselecting_retires_only_the_owned_entry() {
    let fx = Fx::new();
    fx.live(LIVE);
    ok(&fx.apply());
    assert_eq!(running(&fx.json(), "SessionStart", DECLARED), 1);
    fx.set_profile("base", "");
    let o = fx.plan();
    assert!(
        text(&o.stdout).contains("retire 1 owned handler(s)"),
        "{}",
        text(&o.stdout)
    );
    ok(&fx.apply());
    let live: serde_json::Value = serde_json::from_str(LIVE).unwrap();
    assert_eq!(fx.json(), live, "only the owned entry is gone");
}

/// The live machine spells the hippo hook with the absolute home path. That copy is the user's:
/// the declared hook is not added beside it, nothing is recorded as owned, and a retire leaves it.
#[test]
fn a_copy_in_the_absolute_home_spelling_is_not_duplicated_and_never_retired() {
    let fx = Fx::new();
    let home = fx.home.path().to_str().unwrap();
    let mine = format!("{home}/.local/share/hippo-brain/shell/session-hook.sh");
    let live = serde_json::json!({ "hooks": { "SessionStart": [
        { "hooks": [{ "type": "command", "command": mine }] }
    ]}});
    fx.live(&serde_json::to_string_pretty(&live).unwrap());
    ok(&fx.apply());
    assert_eq!(fx.json(), live);
    fx.set_profile("base", "");
    ok(&fx.apply());
    assert_eq!(fx.json(), live);
}

#[test]
fn malformed_json_blocks_and_the_last_good_file_stays() {
    let fx = Fx::new();
    fx.live(LIVE);
    ok(&fx.apply());
    let ledger = fs::read(fx.ledger()).unwrap();
    fx.live("{\"hooks\": oops");
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("not valid JSON"),
        "{}",
        text(&o.stderr)
    );
    let (d, _) = fx.digest();
    let a = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &d]);
    assert_eq!(code(&a), 3, "{}", text(&a.stderr));
    assert_eq!(fs::read_to_string(fx.hooks()).unwrap(), "{\"hooks\": oops");
    assert_eq!(fs::read(fx.ledger()).unwrap(), ledger);
}

#[test]
fn a_file_replaced_between_the_re_read_and_the_rename_is_refused() {
    let fx = Fx::new();
    fx.live(LIVE);
    ok(&fx.apply());
    fx.put(
        "base/modules/agents/module.toml",
        &AGENTS.replace("session-hook.sh", "session-hook-2.sh"),
    );
    let (d, _) = fx.digest();
    let gate = fx.home.path().parent().unwrap().join(format!(
        "codex-gate-{}",
        fx.home.path().file_name().unwrap().to_str().unwrap()
    ));
    let reached = gate.with_extension("reached");
    let child = fx
        .cmd(&["apply", "--root", fx.root_arg(), "--yes-plan", &d])
        .env(
            "DFM_FAILPOINT",
            format!("file-after-tmp:wait-{}", gate.display()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < end,
            "apply never reached the failpoint"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The application saves by rename while the manager holds its staged file.
    let tmp = fx.home.path().join(".codex/app-save.tmp");
    fs::write(&tmp, "{\"saved\": \"by-codex\"}\n").unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&tmp, fx.hooks()).unwrap();
    fs::write(&gate, "").unwrap();
    let o = child.wait_with_output().unwrap();
    let _ = fs::remove_file(&gate);
    let _ = fs::remove_file(&reached);
    assert_eq!(code(&o), 1, "{}\n{}", text(&o.stderr), text(&o.stdout));
    assert!(
        text(&o.stderr).contains("E_CONFLICT"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(
        fs::read_to_string(fx.hooks()).unwrap(),
        "{\"saved\": \"by-codex\"}\n",
        "the application's file stays"
    );
}

#[test]
fn any_other_target_is_refused_at_resolution() {
    let fx = Fx::new();
    fx.put(
        "base/modules/agents/module.toml",
        &AGENTS.replace("\".codex/hooks.json\"", "\".codex/config.json\""),
    );
    let o = fx.plan();
    assert_ne!(code(&o), 0);
    let e = text(&o.stderr);
    assert!(
        e.contains("E_MANIFEST_INVALID") && e.contains(".codex/hooks.json"),
        "{e}"
    );
}

/// C05: an overlay that contributes handlers and then goes missing blocks the plan. Neither
/// co-owned file nor the ledger changes, so the contributed handlers are not retired by
/// inference from an absent source.
#[test]
fn a_missing_overlay_blocks_and_leaves_both_files_and_the_ledger_as_they_were() {
    let fx = Fx::new();
    fx.set_profile("ext", "\"work\"");
    ok(&fx.apply());
    assert_eq!(running(&fx.json(), "SessionStart", "work-session"), 1);
    let settings = fx.home.path().join(".claude/settings.json");
    let claude: serde_json::Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(running(&claude, "SessionStart", "work-session"), 1);
    let before = [
        fs::read(fx.hooks()).unwrap(),
        fs::read(&settings).unwrap(),
        fs::read(fx.ledger()).unwrap(),
    ];
    fs::remove_dir_all(fx.root.join("ext/modules/work")).unwrap();
    let o = fx.plan();
    assert_eq!(code(&o), 3, "{}\n{}", text(&o.stderr), text(&o.stdout));
    assert!(
        text(&o.stderr).contains("E_SOURCE_MISSING"),
        "{}",
        text(&o.stderr)
    );
    let a = fx.run(&[
        "apply",
        "--root",
        fx.root_arg(),
        "--yes-plan",
        &"0".repeat(64),
    ]);
    assert_eq!(code(&a), 3, "{}\n{}", text(&a.stderr), text(&a.stdout));
    let after = [
        fs::read(fx.hooks()).unwrap(),
        fs::read(&settings).unwrap(),
        fs::read(fx.ledger()).unwrap(),
    ];
    assert!(before == after, "a co-owned file or the ledger changed");
}
