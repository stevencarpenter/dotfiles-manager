//! `trial --script`: an install script is fetched through the `curl` look-alike, cached,
//! previewed and run under the real `sandbox-exec` (DFM-81). No real network access.

mod common;

use common::providers::{FAKE_BREW, FAKE_CURL, FAKE_GETCONF, FAKE_MISE, FAKE_SANDBOX_EXEC};
use common::{TempDir, code, text};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");
const URL: &str = "https://boat.dev/install";
const FIXED_ENV: [&str; 15] = [
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "SHELL",
    "PATH",
    "LANG",
    "LC_ALL",
    "TERM",
    "ZDOTDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_BIN_HOME",
];

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
        .unwrap_or_else(|| panic!("no digest in {}", text(&o.stdout)))
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", stderr(&o));
}

/// The run needs the real sandbox, and root is not confined the same way.
fn no_sandbox() -> bool {
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        common::skip("sandbox-exec missing");
        return true;
    }
    if common::is_root() {
        common::skip("running as root");
        return true;
    }
    false
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

impl Fx {
    fn new() -> Fx {
        let src = TempDir::new("script-src");
        let fx = Fx {
            home: TempDir::new("script-home"),
            root: src.path().join("root"),
            _src: src,
            fake: TempDir::new("script-providers"),
        };
        fx.put("base/bundle.toml", common::BUNDLE);
        fx.put("base/modules/core/module.toml", common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/extra/module.toml", common::EXTRA);
        fx.put("base/modules/extra/files/tmux.conf", "set -g mouse on\n");
        fx.put("base/modules/extra/files/zshrc", "export A=1\n");
        fx.put("profiles/t-01.toml", common::PROFILE);
        // The tracked-file guard reads the lock for the nested checkouts it watches.
        fx.put(
            "dotfiles-manager.lock",
            "[manager]\ntoolchain = \"1.98.1\"\n\n[[sources]]\nid = \"src0\"\nkind = \"git\"\norigin = \"https://example.test/dotfiles\"\nref = \"refs/heads/main\"\npath = \".\"\n",
        );
        fs::create_dir_all(fx.bin()).unwrap();
        fs::create_dir_all(fx.tmpdir()).unwrap();
        for (name, body) in [
            ("brew", FAKE_BREW),
            ("mise", FAKE_MISE),
            ("curl", FAKE_CURL),
            ("sandbox-exec", FAKE_SANDBOX_EXEC),
            ("getconf", FAKE_GETCONF),
            // The tracked-file guard runs git through the provider path. /usr/bin stays off it,
            // so the curl look-alike is not shadowed by the real curl.
            ("git", "#!/bin/sh\nexec /usr/bin/git \"$@\"\n"),
        ] {
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

    /// Inside HOME, so the script environment needs no `getconf` call.
    fn tmpdir(&self) -> PathBuf {
        self.home.path().join("tmp")
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn marker(&self, name: &str, content: impl AsRef<[u8]>) {
        fs::write(self.fake.path().join(name), content).unwrap();
    }

    /// The body the `curl` look-alike serves.
    fn script(&self, body: impl AsRef<[u8]>) {
        self.marker("curl-body", body);
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    fn scripts(&self) -> PathBuf {
        self.home.path().join(".cache/dotfiles-manager/scripts")
    }

    fn cached(&self) -> PathBuf {
        self.scripts().join(sha(URL.as_bytes()))
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
            .env("USER", "leak-check")
            .env("TMPDIR", self.tmpdir())
            .env("SECRET_TOKEN", "do-not-pass")
            .env("PATH", self.path_var())
            .arg("--home")
            .arg(self.home.path())
            .args(args);
        cmd
    }

    fn path_var(&self) -> String {
        format!("{}:/usr/bin:/bin", self.h(".local/bin").display())
    }

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn state(&self) -> PathBuf {
        self.h(".local/state/dotfiles-manager/state")
    }

    fn record(&self, id: &str) -> serde_json::Value {
        let p = self.state().join(format!("trials/{id}.json"));
        serde_json::from_slice(&fs::read(&p).unwrap()).unwrap()
    }

    fn journal(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.state().join("journal.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn receipts(&self) -> Vec<serde_json::Value> {
        fs::read_dir(self.state().join("receipts"))
            .map(|d| {
                d.map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Applies the profile, so the four dfm links point into the source tree, and makes the
    /// source tree a git checkout with everything committed.
    fn bind_with_links(&self) {
        self.apply();
        for rel in [
            ".config/git/config",
            ".ssh/config",
            ".config/tmux/tmux.conf",
            ".zshrc",
        ] {
            let m = fs::symlink_metadata(self.h(rel)).unwrap();
            assert!(m.file_type().is_symlink(), "{rel}");
        }
        git(&self.root, &["init", "-q"]);
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-q", "-m", "init"]);
    }

    fn apply(&self) {
        let o = self.run(&["apply", "--preview"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        let o = self.run(&["apply", "--yes-plan", &digest_of(&o)]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    }

    /// Previews, then runs with the preview's digest.
    fn approved(&self, tool: &str) -> Output {
        let o = self.trial_script(tool, &["--preview"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        self.trial_script(tool, &["--yes-plan", &digest_of(&o)])
    }

    fn trial_script(&self, tool: &str, extra: &[&str]) -> Output {
        let mut v = vec!["trial", tool, "--script", URL];
        v.extend_from_slice(extra);
        self.run(&v)
    }
}

#[test]
fn preview_fetches_once_caches_the_script_and_prints_the_path_and_hints() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    let body = "#!/bin/sh\nread x < /dev/tty\n";
    fx.script(body);
    let o = fx.trial_script("boat", &["--env", "FOO=bar", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    let file = fx.cached();
    assert!(t.contains("trial: trial-boat tool=boat"), "{t}");
    assert!(
        t.contains(&format!(
            "script: {URL} -> {URL} sha256:{} bytes={}",
            sha(body.as_bytes()),
            body.len()
        )),
        "{t}"
    );
    assert!(
        t.contains(&format!("script file: {}", file.display())),
        "{t}"
    );
    assert!(
        t.contains(&format!(
            "run: sandbox-exec /bin/sh {} tty=false",
            file.display()
        )),
        "{t}"
    );
    assert!(
        t.contains("note: the script reads /dev/tty; pass --tty to answer its prompts"),
        "{t}"
    );
    assert!(!t.contains("edits shell startup files"), "{t}");
    assert!(
        t.contains(
            "unknown: services paths-outside-baseline provider-caches script-downloads later-self-updates keychain remote-effects"
        ),
        "{t}"
    );
    assert!(t.contains("plan digest: "), "{t}");

    let env_line = t.lines().find_map(|l| l.strip_prefix("env: ")).unwrap();
    let env: Vec<(&str, &str)> = env_line
        .split(' ')
        .map(|kv| kv.split_once('=').unwrap())
        .collect();
    let home = fx.home.path().display().to_string();
    let tmp = fx.tmpdir().display().to_string();
    assert!(env.contains(&("HOME", home.as_str())), "{env_line}");
    assert!(env.contains(&("TMPDIR", tmp.as_str())), "{env_line}");
    assert!(env.contains(&("FOO", "bar")), "{env_line}");
    for (n, _) in &env {
        assert!(
            FIXED_ENV.contains(n) || *n == "FOO",
            "{n} leaked: {env_line}"
        );
    }
    let names: Vec<&str> = env.iter().map(|(n, _)| *n).collect();
    assert!(names.is_sorted(), "{env_line}");

    // One fetch, with the fixed options, into a temporary file beside the cache.
    let log = fx.log();
    let curls: Vec<&str> = log.lines().filter(|l| l.starts_with("curl ")).collect();
    assert_eq!(curls.len(), 1, "{log}");
    let tmp_prefix = format!("{}/.fetch-", fx.scripts().display());
    assert!(
        curls[0].starts_with(&format!(
            "curl -q --proto =https --proto-redir =https --tlsv1.2 -fsSL --max-filesize 1048576 -o {tmp_prefix}"
        )),
        "{log}"
    );
    assert!(
        curls[0].ends_with(&format!(" -w %{{url_effective}} -- {URL}")),
        "{log}"
    );
    assert!(!log.contains("getconf"), "{log}");

    assert_eq!(fs::read_to_string(&file).unwrap(), body);
    let mode = |p: &PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&fx.scripts()), 0o700);
    let sidecar = file.with_extension("json");
    assert_eq!(mode(&sidecar), 0o600);
    let side: serde_json::Value = serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
    assert_eq!(
        side,
        serde_json::json!({
            "url": URL,
            "effective_url": URL,
            "sha256": sha(body.as_bytes()),
            "bytes": body.len(),
        })
    );
    let left: Vec<_> = fs::read_dir(fx.scripts())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with(".fetch-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_redirect_to_http_is_refused() {
    let fx = Fx::new();
    fx.script("#!/bin/sh\n");
    fx.marker("curl-effective", "http://x/i");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert!(!fx.cached().exists());
    assert_eq!(fs::read_dir(fx.scripts()).unwrap().count(), 0);
}

#[test]
fn a_failed_fetch_is_refused() {
    let fx = Fx::new();
    fx.script("#!/bin/sh\n");
    fx.marker("curl-exit", "22");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert!(!fx.cached().exists());
}

#[test]
fn a_script_over_the_size_cap_is_refused() {
    let fx = Fx::new();
    fx.script(vec![b'a'; 1_048_577]);
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_LIMIT_EXCEEDED"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("1048576-byte script cap"),
        "{}",
        stderr(&o)
    );
    assert!(!fx.cached().exists());
    assert!(!fx.cached().with_extension("json").exists());
    assert_eq!(fs::read_dir(fx.scripts()).unwrap().count(), 0);
}

const BOAT: &str = r#"#!/bin/sh
set -e
mkdir -p "$HOME/.ascii/bin" "$HOME/.config/ascii" "$HOME/.local/bin"
printf '#!/bin/sh\necho boat\n' > "$HOME/.ascii/bin/boat"
chmod 755 "$HOME/.ascii/bin/boat"
echo x > "$HOME/.config/ascii/config"
ln -s "$HOME/.ascii/bin/boat" "$HOME/.local/bin/boat"
echo installed boat
"#;

fn window_entry<'a>(rec: &'a serde_json::Value, path: &Path) -> &'a serde_json::Value {
    let p = path.display().to_string();
    rec["script"]["window"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == p.as_str())
        .unwrap_or_else(|| panic!("no window entry for {p}: {rec:#}"))
}

#[test]
fn a_script_trial_records_before_runs_sandboxed_and_rewrites_the_record() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script(BOAT);
    let o = fx.trial_script("boat", &["--arg", "--quiet", "--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // The log of the approved run alone: its dry run and the script run.
    fs::write(fx.fake.path().join("log"), "").unwrap();
    let cache = fx.cached();
    let o = fx.trial_script("boat", &["--arg", "--quiet", "--yes-plan", &digest_of(&o)]);
    let out = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{out}", stderr(&o));
    // The script's own output is inherited, between the preview and the result lines.
    assert!(out.contains("installed boat"), "{out}");
    let rec = fx.record("trial-boat");
    assert_eq!(rec["schema"], 5, "{rec:#}");
    let s = &rec["script"];
    assert!(!s["before"].as_array().unwrap().is_empty(), "{rec:#}");
    assert_eq!(s["exit"], serde_json::json!({"code": 0, "signal": null}));
    let link = window_entry(&rec, &fx.h(".local/bin/boat"));
    assert_eq!(link["change"], "created");
    assert_eq!(link["kind"], "link");
    assert_eq!(
        link["target"],
        fx.h(".ascii/bin/boat").display().to_string().as_str()
    );
    for dir in [".ascii", ".config/ascii"] {
        let e = window_entry(&rec, &fx.h(dir));
        assert_eq!(
            (&e["change"], &e["kind"]),
            (&"created".into(), &"dir".into())
        );
    }
    assert_eq!(
        s["binary"]["path"],
        fx.h(".local/bin/boat").display().to_string().as_str()
    );
    assert_eq!(
        s["binary"]["real_path"],
        fs::canonicalize(fx.h(".ascii/bin/boat"))
            .unwrap()
            .display()
            .to_string()
            .as_str()
    );
    let ops: Vec<(String, String)> = fx
        .journal()
        .iter()
        .filter(|e| e["op"] == "trial-script")
        .map(|e| {
            assert_eq!(e["adapter"], "script/v1");
            assert_eq!(e["resource"], "trial-boat");
            (
                e["op_id"].as_str().unwrap().to_string(),
                e["phase"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!(ops[0].0, ops[1].0);
    assert_eq!((ops[0].1.as_str(), ops[1].1.as_str()), ("intent", "done"));
    let receipts = fx.receipts();
    let r = receipts
        .iter()
        .find(|r| r["command"] == "trial")
        .expect("a trial receipt");
    assert_eq!(r["operations"][0]["id"], ops[0].0.as_str());
    assert_eq!(r["operations"][0]["status"], "updated");
    assert!(out.contains(&format!("trial-boat {}", ops[0].0)), "{out}");
    assert!(out.contains("trial: state/trials/trial-boat.json"), "{out}");
    assert!(out.contains("result: ok"), "{out}");
    let log = fx.log();
    // The fake logs `sandbox-exec <profile> | <command>`; the profile spans lines, so each
    // invocation starts one line with `sandbox-exec (version 1)`.
    assert_eq!(log.matches("sandbox-exec (version 1)").count(), 2, "{log}");
    let commands: Vec<&str> = log
        .lines()
        .filter_map(|l| l.rsplit_once(" | ").map(|(_, c)| c))
        .collect();
    assert_eq!(
        commands,
        [
            "/usr/bin/true".to_string(),
            format!("/bin/sh {} --quiet", cache.display())
        ],
        "{log}"
    );
    assert!(log.contains("(deny process-exec"), "{log}");
    let state_root = fx.h(".local/state/dotfiles-manager");
    assert!(
        log.contains(&format!("(subpath \"{}\")", state_root.display())),
        "{log}"
    );
    // The run used the cached bytes and then dropped them; the record keeps the digest.
    assert!(!fx.cached().exists() && !fx.cached().with_extension("json").exists());
    assert_eq!(s["sha256"], sha(BOAT.as_bytes()).as_str());
}

#[test]
fn a_nonzero_script_exit_is_exit_1_after_the_record() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script("#!/bin/sh\nexit 9\n");
    let o = fx.approved("boat");
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let rec = fx.record("trial-boat");
    assert_eq!(
        rec["script"]["exit"],
        serde_json::json!({"code": 9, "signal": null})
    );
    assert!(rec["script"]["window"].is_array(), "{rec:#}");
    let phases: Vec<String> = fx
        .journal()
        .iter()
        .filter(|e| e["op"] == "trial-script")
        .map(|e| e["phase"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(phases, ["intent", "failed"]);
    // The note is written by `conclude` after the receipt, so an output error cannot stop the
    // receipt.
    let out = text(&o.stdout);
    let at = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {out}"))
    };
    assert!(
        at("trial: state/trials/trial-boat.json")
            < at("note: the tool boat is not on PATH or cannot be hashed")
    );
    assert!(at("note: the tool boat") < at("receipt: "));
    assert!(
        text(&o.stdout).contains("result: failed"),
        "{}",
        text(&o.stdout)
    );
}

#[test]
fn the_run_never_fetches_again_and_a_changed_or_missing_cache_is_stale() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script("#!/bin/sh\nexit 0\n");
    let curls = || fx.log().lines().filter(|l| l.starts_with("curl ")).count();
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let d = digest_of(&o);
    assert_eq!(curls(), 1);
    let file = fx.cached();
    let mode = fs::metadata(&file).unwrap().permissions();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&file, "#!/bin/sh\nexit 7\n").unwrap();
    fs::set_permissions(&file, mode).unwrap();
    let o = fx.trial_script("boat", &["--yes-plan", &d]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(curls(), 1);
    assert!(!fx.state().join("trials/trial-boat.json").exists());
    fs::remove_file(&file).unwrap();
    let o = fx.trial_script("boat", &["--yes-plan", &d]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(stderr(&o).contains("missing"), "{}", stderr(&o));
    assert_eq!(curls(), 1);
    // A new preview caches the same bytes again, so the same digest now runs.
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(digest_of(&o), d);
    let o = fx.trial_script("boat", &["--yes-plan", &d]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(curls(), 2);
    assert_eq!(fx.record("trial-boat")["script"]["exit"]["code"], 0);
}

#[test]
fn a_pre_existing_tool_refuses_the_trial() {
    let fx = Fx::new();
    let p = fx.h(".local/bin/boat");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    fx.script("#!/bin/sh\n");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("E_CONFLICT"), "{err}");
    assert!(
        err.contains(&format!(
            "an executable named boat is already on PATH: {}",
            p.display()
        )),
        "{err}"
    );
    assert!(!fx.log().contains("curl "), "{}", fx.log());
    assert!(!fx.cached().exists());
}

#[test]
fn the_tracked_file_guard_fails_the_trial_when_a_tracked_file_changed() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // The sandbox stops the script from writing the checkout, so the test changes a tracked
    // file from outside while the script runs: `started` says the run is on, `go` that the
    // change is made. The script then also exits nonzero, which the tracked change outranks.
    fx.script(
        "#!/bin/sh\n: > \"$HOME/started\"\ni=0\nwhile [ ! -e \"$HOME/go\" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done\nexit 4\n",
    );
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let mut child = fx
        .command(&[
            "trial",
            "boat",
            "--script",
            URL,
            "--yes-plan",
            &digest_of(&o),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !fx.h("started").exists() {
        assert!(started.elapsed() < Duration::from_secs(60), "never started");
        if child.try_wait().unwrap().is_some() {
            let o = child.wait_with_output().unwrap();
            panic!(
                "ended before the script ran:\n{}\n{}",
                stderr(&o),
                text(&o.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let tracked = fx.root.join("base/modules/extra/files/tmux.conf");
    let mut f = fs::OpenOptions::new().append(true).open(&tracked).unwrap();
    std::io::Write::write_all(&mut f, b"set -g status off\n").unwrap();
    drop(f);
    fs::write(fx.h("go"), "").unwrap();
    let o = child.wait_with_output().unwrap();
    let all = format!("{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(code(&o), 1, "{all}");
    let err = stderr(&o);
    assert!(err.starts_with("error: E_CONFLICT "), "{all}");
    assert!(err.contains("the script exited 4"), "{all}");
    assert!(
        all.contains(&format!(
            "the script changed the tracked file {}",
            tracked.display()
        )),
        "{all}"
    );
    assert!(
        fs::read_to_string(&tracked)
            .unwrap()
            .ends_with("set -g status off\n")
    );
    let rec = fx.record("trial-boat");
    assert!(rec["script"]["window"].is_array(), "{rec:#}");
    assert_eq!(rec["script"]["exit"]["code"], 4);
}

#[test]
fn a_ledger_change_before_the_lock_makes_the_sandbox_profile_stale() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.script("#!/bin/sh\nexit 0\n");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // The run parks after its pre-lock decision; an apply then adds four ledger targets, which
    // leaves the digest as it is and changes the profile the run would use.
    let gate = fx.fake.path().join("go");
    let parked = fx
        .command(&[
            "trial",
            "boat",
            "--script",
            URL,
            "--yes-plan",
            &digest_of(&o),
        ])
        .env(
            "DFM_FAILPOINT",
            format!("trial-before-lock:wait-{}", gate.display()),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reached = gate.with_extension("reached");
    let started = Instant::now();
    while !reached.exists() {
        assert!(started.elapsed() < Duration::from_secs(30), "not reached");
        std::thread::sleep(Duration::from_millis(10));
    }
    fx.apply();
    fs::write(&gate, "").unwrap();
    let o = parked.wait_with_output().unwrap();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_PLAN_STALE the sandbox profile changed since the preview"),
        "{}",
        stderr(&o)
    );
    assert!(!fx.state().join("trials/trial-boat.json").exists());
}

#[test]
fn a_dev_tty_read_fails_without_tty() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script("#!/bin/sh\nread x < /dev/tty || exit 5\n");
    let o = fx.approved("boat");
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.record("trial-boat")["script"]["exit"]["code"], 5);
}

#[test]
fn doctor_shows_the_script_trial_and_purge_and_adopt_refuse_it() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script(BOAT);
    let o = fx.approved("boat");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["doctor", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("{e}\n{}\n{}", text(&o.stdout), stderr(&o)));
    let row = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["subject"] == "trials/trial-boat.json")
        .unwrap_or_else(|| panic!("no trial row in {v:#}"));
    assert!(
        row["detail"]
            .as_str()
            .unwrap()
            .contains(&format!("boat installed by the trial through {URL}")),
        "{row:#}"
    );
    let o = fx.run(&["purge", "trial-boat", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(stderr(&o).contains("DFM-82"), "{}", stderr(&o));
    let o = fx.run(&["adopt", "trial-boat", "--module", "extra", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(stderr(&o).contains("DFM-83"), "{}", stderr(&o));
}
