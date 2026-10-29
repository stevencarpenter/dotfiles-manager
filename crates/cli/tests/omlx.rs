//! `omlx-model-settings/v1` merge and the `omlx-app/v1` service through the isolated binary
//! (DFM-21 C08, docs/adapters.md, docs/lifecycle.md).
//!
//! Every run passes `--home <temp dir>` and an empty environment, plus `DFM_PROVIDER_PATH`
//! pointing at a directory with `open`, `ps` and `kill` look-alikes. The app bundle is a
//! directory below HOME (the isolated build never looks in `/Applications`). The app's control
//! socket is served by a thread of the test. HOME is a short directory below `/tmp`, because a Unix socket path
//! holds at most 104 bytes on macOS and the system temporary directory is too deep for it.

mod common;

use common::{BIN, TempDir, code, text};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TOKEN: &str = "PLANTED-OMLX-TOKEN-51c2";
const MODEL: &str = "Qwen3.6-35B-A3B-Abliterated-Heretic-MLX-4bit";
const OP: &str = "llm.omlx@service";

const LLM: &str = r#"schema = 1
id = "llm"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "omlx-models"
kind = "merge"
adapter = "omlx-model-settings/v1"
target = ".omlx/model_settings.json"
[resources.value.managed.models."Qwen3.6-35B-A3B-Abliterated-Heretic-MLX-4bit"]
enable_thinking = false
forced_ct_kwargs = ["enable_thinking"]
trust_remote_code = false

[[resources]]
id = "omlx"
kind = "service"
adapter = "omlx-app/v1"
[resources.value]
config = [".omlx/model_settings.json"]
"#;

/// `open -gj -a <bundle>`: logs, reports the server running under pid 1001 and marks the app
/// launched, as the app does once it started its server.
const FAKE_OPEN: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
here=$(dirname "$0")
printf 'open %s\n' "$*" >> "$here/../log"
echo running > "$here/../state"
echo 1001 > "$here/../pid"
: > "$here/../launched"
exit 0
"#;

/// `ps -o uid=,comm= -p <pid>`: the uid in `owner` (default: the current user) and the name in
/// `comm` (default: `omlx-server`); exits 1 with the `gone` marker, as for a pid with no process.
const FAKE_PS: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
here=$(dirname "$0")
printf 'ps %s\n' "$*" >> "$here/../log"
[ -e "$here/../gone" ] && exit 1
printf '%5s %s\n' "$(cat "$here/../owner" 2>/dev/null || id -u)" "$(cat "$here/../comm" 2>/dev/null || echo omlx-server)"
"#;

/// `kill -TERM <pid>`: logs, fails with the `fail` marker, and otherwise does what the app does
/// when its server exits: the socket then reports `next-state` (default `running`) under
/// `next-pid` (default 2002).
const FAKE_KILL: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
here=$(dirname "$0")
printf 'kill %s\n' "$*" >> "$here/../log"
[ -e "$here/../fail" ] && { echo "PLANTED-STDERR" >&2; exit 1; }
cat "$here/../next-state" 2>/dev/null > "$here/../state" || echo running > "$here/../state"
cat "$here/../next-pid" 2>/dev/null > "$here/../pid" || echo 2002 > "$here/../pid"
exit 0
"#;

/// The app's file as oMLX writes it: another model and keys the module does not declare.
const LIVE: &str = r#"{
    "version": 1,
    "models": {
        "nomicai-modernbert-embed-base-8bit": { "force_sampling": false, "turboquant_kv_bits": 4.0 },
        "Qwen3.6-35B-A3B-Abliterated-Heretic-MLX-4bit": { "temperature": 0.0, "enable_thinking": true, "model_type_override": "llm" }
    }
}
"#;

/// HOME below `/tmp`, removed on drop.
struct ShortHome(PathBuf);

impl ShortHome {
    fn new() -> ShortHome {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("dfm-o-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        ShortHome(p)
    }
}

impl Drop for ShortHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fx {
    home: ShortHome,
    _src: TempDir,
    root: PathBuf,
    fake: TempDir,
}

impl Fx {
    fn new() -> Fx {
        let src = TempDir::new("omlx-src");
        let fx = Fx {
            home: ShortHome::new(),
            root: src.path().join("root"),
            _src: src,
            fake: TempDir::new("omlx-providers"),
        };
        fx.put(
            "base/bundle.toml",
            "schema = 1\nid = \"t-base\"\ncore = \"modules/core/module.toml\"\n\n[modules]\nllm = \"modules/llm/module.toml\"\n",
        );
        fx.put("base/modules/core/module.toml", common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/llm/module.toml", LLM);
        fx.put(
            "profiles/t-01.toml",
            "schema = 1\nid = \"t-01\"\nbundle = \"../base/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [\"llm\"]\noverrides = []\n",
        );
        let bin = fx.fake.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for (name, body) in [("open", FAKE_OPEN), ("ps", FAKE_PS), ("kill", FAKE_KILL)] {
            fs::write(bin.join(name), body).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::create_dir_all(fx.bundle()).unwrap();
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

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .env_clear()
            .env("DFM_PROVIDER_PATH", self.fake.path().join("bin"))
            .env("DFM_SERVICE_TIMEOUT_SECS", "2")
            .arg("--home")
            .arg(&self.home.0)
            .args(args)
            .output()
            .unwrap()
    }

    fn bundle(&self) -> PathBuf {
        self.home.0.join("Applications/oMLX.app")
    }

    fn mark(&self, name: &str, content: &str) {
        fs::write(self.fake.path().join(name), content).unwrap();
    }

    fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    fn settings(&self) -> PathBuf {
        self.home.0.join(".omlx/model_settings.json")
    }

    /// Writes the live file as the app does: a new file renamed over the old one, mode 0644.
    fn live(&self, content: &str, mode: u32) {
        let p = self.settings();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, content).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).unwrap();
        fs::rename(&tmp, &p).unwrap();
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.settings()).unwrap()).unwrap()
    }

    fn mode(&self) -> u32 {
        fs::metadata(self.settings()).unwrap().permissions().mode() & 0o777
    }

    fn digest(&self, args: &[&str]) -> (String, Vec<String>) {
        let mut a = args.to_vec();
        a.extend(["--root", self.root_arg(), "--json"]);
        let o = self.run(&a);
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

    /// Applies the configuration, approving each take-over by id.
    fn apply(&self) -> Output {
        let (d, adopt) = self.digest(&["plan"]);
        let mut a = vec!["apply", "--root", self.root_arg(), "--yes-plan", &d];
        for id in &adopt {
            a.extend(["--allow-adopt", id]);
        }
        self.run(&a)
    }

    fn preview(&self, flags: &[&str]) -> Output {
        let mut a = vec!["services", "--root", self.root_arg(), "--preview"];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    fn services(&self, flags: &[&str]) -> Output {
        let o = self.preview(flags);
        let d = text(&o.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
            .unwrap_or_else(|| panic!("no digest\n{}\n{}", text(&o.stdout), text(&o.stderr)));
        let mut a = vec!["services", "--root", self.root_arg(), "--yes-plan", &d];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    /// Serves the control socket from a thread: each request line gets the state in the `state`
    /// file and the server pid in the `pid` file (1001 at first). With `after_launch`, the socket appears only once the look-alike launched the app.
    fn app(&self, state: &str, after_launch: bool) {
        fs::write(self.fake.path().join("state"), state).unwrap();
        fs::write(self.fake.path().join("pid"), "1001").unwrap();
        let sock = self
            .home
            .0
            .join("Library/Application Support/oMLX/control.sock");
        fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let (fake, home, at) = (
            self.fake.path().to_path_buf(),
            self.home.0.clone(),
            sock.clone(),
        );
        std::thread::spawn(move || {
            if after_launch {
                while !fake.join("launched").exists() {
                    if !home.exists() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            let listener = UnixListener::bind(&at).unwrap();
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { return };
                let mut line = String::new();
                let _ = BufReader::new(&c).read_line(&mut line);
                assert_eq!(line, "{\"command\":\"status\"}\n");
                let st = fs::read_to_string(fake.join("state")).unwrap_or_default();
                let pid = fs::read_to_string(fake.join("pid")).unwrap_or_default();
                let pid = match pid.trim() {
                    "" => "null".to_string(),
                    p => p.to_string(),
                };
                let _ = writeln!(
                    c,
                    "{{\"status\":\"ok\",\"pid\":{pid},\"port\":8000,\"host\":\"127.0.0.1\",\"state\":\"{}\",\"ok\":true}}",
                    st.trim()
                );
            }
        });
        if !after_launch {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !sock.exists() {
                assert!(std::time::Instant::now() < end, "the socket never appeared");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

fn ok(o: &Output) {
    assert_eq!(code(o), 0, "{}\n{}", text(&o.stderr), text(&o.stdout));
}

fn qwen(j: &serde_json::Value) -> &serde_json::Value {
    &j["models"][MODEL]
}

#[test]
fn foreign_models_and_keys_survive_and_the_file_keeps_mode_644() {
    let fx = Fx::new();
    fx.live(LIVE, 0o644);
    let before: serde_json::Value = serde_json::from_str(LIVE).unwrap();
    ok(&fx.apply());
    let j = fx.json();
    assert_eq!(qwen(&j)["enable_thinking"], false);
    assert_eq!(
        qwen(&j)["forced_ct_kwargs"],
        serde_json::json!(["enable_thinking"])
    );
    assert_eq!(qwen(&j)["trust_remote_code"], false);
    assert_eq!(qwen(&j)["temperature"], 0.0);
    assert_eq!(qwen(&j)["model_type_override"], "llm");
    let embed = "nomicai-modernbert-embed-base-8bit";
    assert_eq!(j["models"][embed], before["models"][embed]);
    assert_eq!(j["version"], 1);
    assert_eq!(fx.mode(), 0o644);
    // An absent file is created with the app's mode, holding the declared leaves only.
    let fresh = Fx::new();
    ok(&fresh.apply());
    assert_eq!(fresh.mode(), 0o644);
    assert_eq!(
        fresh.json(),
        serde_json::json!({ "models": { MODEL: {
            "enable_thinking": false, "forced_ct_kwargs": ["enable_thinking"], "trust_remote_code": false
        }}})
    );
}

#[test]
fn a_rewrite_by_the_app_in_another_key_order_is_current() {
    let fx = Fx::new();
    fx.live(LIVE, 0o644);
    ok(&fx.apply());
    // oMLX rewrites the whole file from memory: its own key order and indent, mode 0644.
    let j = fx.json();
    let mut reordered = String::from("{\n    \"version\": 1,\n    \"models\": {\n");
    let models: Vec<_> = j["models"].as_object().unwrap().iter().rev().collect();
    for (i, (k, v)) in models.iter().enumerate() {
        let mut keys: Vec<_> = v.as_object().unwrap().iter().collect();
        keys.reverse();
        let body: Vec<String> = keys.iter().map(|(k, v)| format!("\"{k}\": {v}")).collect();
        let comma = if i + 1 < models.len() { "," } else { "" };
        reordered.push_str(&format!(
            "        \"{k}\": {{ {} }}{comma}\n",
            body.join(", ")
        ));
    }
    reordered.push_str("    }\n}\n");
    fx.live(&reordered, 0o644);
    assert_eq!(fx.json(), j);
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    ok(&o);
    assert!(
        text(&o.stdout).contains("already holds the merge result"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.apply();
    ok(&o);
    assert!(text(&o.stdout).contains("updated=0"), "{}", text(&o.stdout));
    assert_eq!(fs::read_to_string(fx.settings()).unwrap(), reordered);
    assert_eq!(fx.mode(), 0o644);
}

#[test]
fn malformed_json_blocks_and_the_last_good_file_stays() {
    let fx = Fx::new();
    fx.live(LIVE, 0o644);
    ok(&fx.apply());
    let ledger = fx
        .home
        .0
        .join(".local/state/dotfiles-manager/state/ledger.json");
    let before = fs::read(&ledger).unwrap();
    fx.live("{\"version\": 1, \"models\": {", 0o644);
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("not valid JSON"),
        "{}",
        text(&o.stderr)
    );
    let (d, _) = fx.digest(&["plan"]);
    let a = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &d]);
    assert_eq!(code(&a), 3, "{}", text(&a.stderr));
    assert_eq!(
        fs::read_to_string(fx.settings()).unwrap(),
        "{\"version\": 1, \"models\": {"
    );
    assert_eq!(fs::read(&ledger).unwrap(), before);
}

#[test]
fn a_file_writable_by_others_blocks_and_any_other_target_is_refused() {
    let fx = Fx::new();
    fx.live(LIVE, 0o666);
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("chmod 644"), "{}", text(&o.stderr));
    assert_eq!(fs::read_to_string(fx.settings()).unwrap(), LIVE);
    fx.put(
        "base/modules/llm/module.toml",
        &LLM.replace(
            "target = \".omlx/model_settings.json\"",
            "target = \".omlx/settings.json\"",
        ),
    );
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    let e = text(&o.stderr);
    assert_eq!(code(&o), 3, "{e}");
    assert!(
        e.contains("E_MANIFEST_INVALID") && e.contains(".omlx/model_settings.json"),
        "{e}"
    );
}

/// `.omlx/settings.json` holds the server's credentials. No run reads, changes or shows it.
#[test]
fn the_server_settings_file_never_reaches_any_output_or_state_file() {
    let fx = Fx::new();
    let secret = fx.home.0.join(".omlx/settings.json");
    fs::create_dir_all(secret.parent().unwrap()).unwrap();
    let body = format!("{{\"auth\": {{\"api_key\": \"{TOKEN}\"}}}}\n");
    fs::write(&secret, &body).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    fx.live(LIVE, 0o644);
    fx.app("running", false);
    let mut outputs = vec![fx.apply()];
    for args in [
        &["plan", "--root", fx.root_arg(), "--json"][..],
        &["services", "--root", fx.root_arg(), "--preview"][..],
        &["doctor", "--root", fx.root_arg()][..],
    ] {
        outputs.push(fx.run(args));
    }
    for o in &outputs[..3] {
        ok(o);
    }
    for o in &outputs {
        assert!(!text(&o.stdout).contains(TOKEN) && !text(&o.stderr).contains(TOKEN));
    }
    let state = fx.home.0.join(".local/state/dotfiles-manager");
    let mut stack = vec![state];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(b) = fs::read(&p) {
                assert!(!text(&b).contains(TOKEN), "{}", p.display());
            }
        }
    }
    assert_eq!(fs::read_to_string(&secret).unwrap(), body);
    assert_eq!(
        fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn an_app_that_is_not_running_is_started_and_a_running_one_restarts_only_on_request() {
    let fx = Fx::new();
    fx.live(LIVE, 0o644);
    ok(&fx.apply());
    // No socket: the app is not running. The start is planned and not disruptive.
    fx.app("running", true);
    let o = fx.preview(&[]);
    ok(&o);
    assert!(
        text(&o.stdout).contains("the app is not running"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.services(&[]);
    ok(&o);
    assert!(
        text(&o.stdout).contains(&format!("updated {OP} reload=reloaded")),
        "{}",
        text(&o.stdout)
    );
    let opened = format!("open -gj -a {}\n", fx.bundle().display());
    assert_eq!(fx.log(), opened);
    // Running: current, and nothing is restarted without the request.
    let o = fx.services(&[]);
    ok(&o);
    assert!(
        text(&o.stdout).contains(&format!("current {OP}")),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.log(), opened);
    // A restart needs the request and its approval.
    let o = fx.preview(&["--restart", "llm.omlx"]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_DISRUPTIVE_UNAPPROVED"),
        "{}",
        text(&o.stderr)
    );
    let o = fx.services(&["--restart", "llm.omlx", "--allow-disruptive", OP]);
    ok(&o);
    assert!(
        text(&o.stdout).contains(&format!("updated {OP} reload=reloaded")),
        "{}",
        text(&o.stdout)
    );
    // The restart signals exactly the pid the socket named, after ps showed it is this user's
    // omlx-server, and waits for the server under its new pid.
    assert_eq!(
        fx.log(),
        format!("{opened}ps -o uid=,comm= -p 1001\nkill -TERM 1001\n")
    );
}

#[test]
fn a_server_in_error_is_stale_and_a_failed_launcher_is_never_a_success() {
    let fx = Fx::new();
    fx.live(LIVE, 0o644);
    ok(&fx.apply());
    fx.app("error", false);
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("--restart"), "{}", text(&o.stderr));
    assert!(fx.log().is_empty(), "{}", fx.log());
    fs::write(fx.fake.path().join("fail"), "").unwrap();
    let o = fx.services(&["--restart", "llm.omlx", "--allow-disruptive", OP]);
    assert_eq!(code(&o), 1, "{}\n{}", text(&o.stderr), text(&o.stdout));
    assert!(
        !text(&o.stderr).contains("PLANTED-STDERR"),
        "{}",
        text(&o.stderr)
    );
    // Without the bundle the app is not installed: a blocker, and nothing runs.
    fs::remove_dir(fx.bundle()).unwrap();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("not installed"),
        "{}",
        text(&o.stderr)
    );
}

/// A running server, its settings applied, ready for `--restart`.
fn restartable() -> Fx {
    let fx = Fx::new();
    fx.live(LIVE, 0o644);
    ok(&fx.apply());
    fx.app("running", false);
    fx
}

fn restart(fx: &Fx) -> Output {
    fx.services(&["--restart", "llm.omlx", "--allow-disruptive", OP])
}

fn failed_with(o: &Output, detail: &str) {
    let all = format!("{}\n{}", text(&o.stderr), text(&o.stdout));
    assert_eq!(code(o), 1, "{all}");
    assert!(all.contains(detail), "{all}");
}

#[test]
fn a_server_that_never_runs_again_after_the_restart_fails_with_the_last_state() {
    let fx = restartable();
    fx.mark("next-state", "starting");
    let o = restart(&fx);
    failed_with(&o, "last state: starting");
    assert!(fx.log().ends_with("kill -TERM 1001\n"), "{}", fx.log());
    // A server still running under the pid that was signalled is not a restarted one.
    let fx = restartable();
    fx.mark("next-pid", "1001");
    let o = restart(&fx);
    failed_with(&o, "last state: running under the old server pid 1001");
    // A running reply that names no pid is no evidence of a new server.
    let fx = restartable();
    fx.mark("next-pid", "");
    let o = restart(&fx);
    failed_with(&o, "last state: running without a server pid");
}

#[test]
fn a_stopped_server_under_a_running_app_blocks_with_the_menu_fix() {
    let fx = Fx::new();
    fx.live(LIVE, 0o644);
    ok(&fx.apply());
    fx.app("stopped", false);
    let o = fx.services(&[]);
    let all = format!("{}\n{}", text(&o.stderr), text(&o.stdout));
    assert!(all.contains("start the server from the oMLX menu"), "{all}");
    assert!(!fx.log().contains("open "), "{}", fx.log());
}

#[test]
fn a_pid_that_is_not_this_users_omlx_server_is_never_signalled() {
    for (marker, content, detail) in [
        ("owner", "0", "is not an omlx-server of this user"),
        (
            "comm",
            "/usr/bin/python3",
            "is not an omlx-server of this user",
        ),
        ("gone", "", "ps finds no process 1001"),
    ] {
        let fx = restartable();
        fx.mark(marker, content);
        let o = restart(&fx);
        failed_with(&o, detail);
        assert!(
            text(&o.stderr).contains("nothing was signalled"),
            "{}",
            text(&o.stderr)
        );
        assert!(!fx.log().contains("kill"), "{marker}: {}", fx.log());
    }
    // A status that names no pid: nothing to check, nothing is signalled.
    let fx = restartable();
    fx.mark("pid", "");
    let o = restart(&fx);
    failed_with(&o, "named no server pid");
    assert!(
        !fx.log().contains("ps ") && !fx.log().contains("kill"),
        "{}",
        fx.log()
    );
}
