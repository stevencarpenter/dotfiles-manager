//! `trial --script`: an install script is fetched through the `curl` look-alike, cached,
//! previewed and run under the real `sandbox-exec` (DFM-81). No real network access.

mod common;

use common::providers::{
    FAKE_BREW, FAKE_CURL, FAKE_GETCONF, FAKE_GITLEAKS, FAKE_MISE, FAKE_SANDBOX_EXEC,
};
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
        Fx::build(false)
    }

    /// The source tree at `~/.dotfiles`, as on a machine: a write to it passes the HOME allow,
    /// so only the checkout deny can stop it.
    fn in_home() -> Fx {
        Fx::build(true)
    }

    fn build(root_in_home: bool) -> Fx {
        let src = TempDir::new("script-src");
        let home = TempDir::new("script-home");
        let fx = Fx {
            root: if root_in_home {
                home.path().join(".dotfiles")
            } else {
                src.path().join("root")
            },
            home,
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
            ("gitleaks", FAKE_GITLEAKS),
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
            .env("DFM_LOGIN_SHELL", "/opt/test/login-shell")
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
    assert!(
        env.contains(&("SHELL", "/opt/test/login-shell")),
        "{env_line}"
    );
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
    let scripts = fx.scripts();
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
    // The script runs from a private copy `run-<trial id>-<pid>`, not from the cache file.
    let pid = commands
        .get(1)
        .and_then(|c| c.strip_prefix(&format!("/bin/sh {}/run-trial-boat-", scripts.display())))
        .and_then(|c| c.strip_suffix(" --quiet"));
    assert!(
        commands.len() == 2
            && commands[0] == "/usr/bin/true"
            && pid.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
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
    assert_eq!(run_files(&fx), Vec::<String>::new());
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

/// Creates `n` small files `~/.local/bin/f-<i>`, each with a 64-hex digest in the window.
fn many_files(fx: &Fx, n: usize) {
    let bin = fx.h(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    for i in 0..n {
        fs::write(bin.join(format!("f-{i:05}")), "x").unwrap();
    }
}

/// How many files make a `before` observation of about `bytes`: one entry is the path plus
/// about 116 bytes of keys, kind and digest.
fn files_for(fx: &Fx, bytes: usize) -> usize {
    bytes / (fx.h(".local/bin/f-00000").display().to_string().len() + 116)
}

#[test]
fn a_window_over_the_record_budget_is_shrunk_and_the_outcome_kept() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // The script creates 2 x 5 000 files with 240-byte names in two watched directories. Each
    // created entry is about 440 bytes, so the window alone is about 4.4 MB: far over the 3 MiB
    // budget and the 4 MiB write cap, while `before` stays small. The tail cut must run.
    let name = "f".repeat(240);
    fx.script(format!(
        "{}mkdir -p \"$HOME/bin\"\ni=0\nwhile [ $i -lt 5000 ]; do\n  : > \"$HOME/.local/bin/{name}-$i\"\n  : > \"$HOME/bin/{name}-$i\"\n  i=$((i + 1))\ndone\n",
        BOAT.trim_end_matches("echo installed boat\n")
    ));
    let o = fx.approved("boat");
    let out = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{out}", stderr(&o));
    let note = out
        .lines()
        .find(|l| l.starts_with("note: the window was shrunk to fit the record: "))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        note.ends_with(" entries dropped; the exit and the binary are recorded"),
        "{note}"
    );
    let file = fx.state().join("trials/trial-boat.json");
    let bytes = fs::metadata(&file).unwrap().len();
    assert!(bytes <= 4 << 20, "{bytes}");
    let rec = fx.record("trial-boat");
    let s = &rec["script"];
    assert_eq!(s["exit"], serde_json::json!({"code": 0, "signal": null}));
    assert!(s["binary"]["sha256"].is_string(), "{}", s["binary"]);
    let window = s["window"].as_array().unwrap();
    assert!(
        !window.is_empty() && window.len() < 10_000,
        "{}",
        window.len()
    );
    let unknown = rec["unknown"].as_array().unwrap();
    assert!(
        unknown.iter().any(|u| u == "window-truncated"),
        "{unknown:?}"
    );
}

#[test]
fn a_before_observation_over_half_the_budget_refuses_before_any_write() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    many_files(&fx, files_for(&fx, 1_800_000));
    fx.script(BOAT);
    let o = fx.approved("boat");
    assert_eq!(code(&o), 3, "{}", text(&o.stdout));
    let err = stderr(&o);
    assert!(err.contains("E_LIMIT_EXCEEDED"), "{err}");
    assert!(
        err.contains("the watch set is too large to record"),
        "{err}"
    );
    assert!(!fx.state().join("trials/trial-boat.json").exists());
    assert_eq!(run_files(&fx), Vec::<String>::new());
    assert!(!fx.h(".ascii").exists(), "the script must not run");
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
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!fx.state().join("trials/trial-boat.json").exists());
}

/// Starts the approved run parked at the failpoint `point` and waits until it is there. The
/// run continues when the returned gate file exists.
fn parked(fx: &Fx, point: &str, digest: &str) -> (std::process::Child, PathBuf) {
    let gate = fx.fake.path().join("go");
    let child = fx
        .command(&["trial", "boat", "--script", URL, "--yes-plan", digest])
        .env("DFM_FAILPOINT", format!("{point}:wait-{}", gate.display()))
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
    (child, gate)
}

fn run_files(fx: &Fx) -> Vec<String> {
    fs::read_dir(fx.scripts())
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("run-"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_preview_that_replaces_the_cache_after_the_record_does_not_change_what_runs() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    let approved = "#!/bin/sh\n: > \"$HOME/approved\"\n";
    fx.script(approved);
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let (child, gate) = parked(&fx, "trial-script-after-record", &digest_of(&o));
    // A concurrent preview takes no lock and may rewrite the URL-keyed cache file.
    fs::write(fx.cached(), "#!/bin/sh\n: > \"$HOME/replaced\"\n").unwrap();
    fs::write(&gate, "").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(fx.h("approved").exists());
    assert!(!fx.h("replaced").exists());
    let rec = fx.record("trial-boat");
    assert_eq!(rec["script"]["sha256"], sha(approved.as_bytes()).as_str());
    assert_eq!(run_files(&fx), Vec::<String>::new());
}

#[test]
fn a_tool_that_appears_on_path_before_the_lock_refuses_the_run() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script("#!/bin/sh\nexit 0\n");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let (child, gate) = parked(&fx, "trial-before-lock", &digest_of(&o));
    // Another installer puts the tool on PATH between the first check and the lock.
    let p = fx.h(".local/bin/boat");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&gate, "").unwrap();
    let o = child.wait_with_output().unwrap();
    let err = stderr(&o);
    assert_eq!(code(&o), 3, "{err}\n{}", text(&o.stdout));
    assert!(err.contains("E_CONFLICT"), "{err}");
    assert!(
        err.contains(&format!(
            "an executable named boat is already on PATH: {}",
            p.display()
        )),
        "{err}"
    );
    assert!(!fx.state().join("trials/trial-boat.json").exists());
    assert!(
        !fx.journal().iter().any(|e| e["op"] == "trial-script"),
        "{:?}",
        fx.journal()
    );
    assert_eq!(run_files(&fx), Vec::<String>::new());
}

#[test]
fn the_script_runs_in_a_private_directory_under_tmpdir_that_is_removed_after() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // An installer that downloads and unpacks into `.` (`curl -LO; tar xzf`).
    fx.script(
        "#!/bin/sh\necho x > ./rel && mkdir work && { echo \"cwd=$PWD\"; cat ./rel; [ -d work ] && echo work; } > \"$HOME/result\"\n",
    );
    let o = fx.approved("boat");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let result = fs::read_to_string(fx.h("result")).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    let tmp = fs::canonicalize(fx.tmpdir()).unwrap();
    let cwd = lines[0].strip_prefix("cwd=").unwrap();
    assert!(
        cwd.starts_with(&format!("{}/dfm-trial-trial-boat-", tmp.display())),
        "{result}"
    );
    assert_eq!(lines[1..], ["x", "work"], "{result}");
    assert!(!Path::new(cwd).exists(), "{cwd}");
}

/// Ends the leftover sandboxed sleep of a killed run, also when the test fails.
struct KillMarker(String);

impl Drop for KillMarker {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill")
            .args(["-KILL", "-f", &self.0])
            .status();
    }
}

#[test]
fn a_killed_run_leaves_the_record_without_a_window_and_an_open_intent() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // The sleep runs through a link under this test's HOME, so `pgrep -f` finds only its own.
    let marker = fx.h("marker-sleep").display().to_string();
    fx.script(
        "#!/bin/sh\nln -s /bin/sleep \"$HOME/marker-sleep\"\n: > \"$HOME/started\"\n\"$HOME/marker-sleep\" 30\n",
    );
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // Output goes nowhere: the orphaned sleep would hold a pipe open.
    let mut child = fx
        .command(&[
            "trial",
            "boat",
            "--script",
            URL,
            "--yes-plan",
            &digest_of(&o),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _sleep = KillMarker(marker.clone());
    let started = Instant::now();
    while !fx.h("started").exists() || pgrep(&["-f", &marker]).is_empty() {
        assert!(child.try_wait().unwrap().is_none(), "ended before the run");
        assert!(started.elapsed() < Duration::from_secs(60), "never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let rec = fx.record("trial-boat");
    let s = &rec["script"];
    assert!(!s["before"].as_array().unwrap().is_empty(), "{rec:#}");
    assert!(s["window"].is_null() && s["exit"].is_null(), "{rec:#}");
    // Without a window, what the run changed is unknown: adopt refuses the record.
    let o = fx.run(&["adopt", "trial-boat", "--module", "extra", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let e = stderr(&o);
    assert!(
        e.contains("E_CONFLICT") && e.contains("has no window"),
        "{e}"
    );
    let entries: Vec<serde_json::Value> = fx
        .journal()
        .into_iter()
        .filter(|e| e["op"] == "trial-script")
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["phase"], "intent");
    let id = entries[0]["op_id"].as_str().unwrap().to_string();
    let o = fx.run(&["state", "check"]);
    let all = format!("{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(code(&o), 1, "{all}");
    assert!(all.contains("E_OP_INTERRUPTED"), "{all}");
    assert!(all.contains("trial-script") && all.contains(&id), "{all}");
    // The intent has no path, so recover cannot settle it.
    let o = fx.run(&["state", "recover"]);
    let all = format!("{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(code(&o), 3, "{all}");
    assert!(
        all.contains(&format!("open {id} (trial-script of trial-boat)")),
        "{all}"
    );
    let o = fx.run(&["state", "resolve", &id, "--as", "failed"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.run(&["state", "check"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
}

#[test]
fn a_ledger_change_between_the_preview_and_the_run_makes_the_digest_stale() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.script("#!/bin/sh\nexit 0\n");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // The four new ledger targets change only the sandbox profile, which the digest covers.
    fx.apply();
    let o = fx.trial_script("boat", &["--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
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
fn doctor_shows_the_script_trial_and_purge_refuses_it() {
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
}

/// Runs `body` as an approved script trial and returns `~/result`. The trial must succeed: the
/// script's own exit is 0 and the tracked-file guard finds nothing, which would be a conflict.
fn sandboxed(fx: &Fx, body: &str) -> String {
    fx.script(format!("#!/bin/sh\n{body}"));
    let o = fx.approved("boat");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.record("trial-boat")["script"]["exit"]["code"], 0);
    fs::read_to_string(fx.h("result")).unwrap_or_default()
}

#[test]
fn a_write_through_a_dfm_link_is_denied_and_the_repo_file_is_unchanged() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::in_home();
    fx.bind_with_links();
    // `zshrc` is a link target; `bundle.toml` and a new file are only inside the checkout.
    let zshrc = fx.root.join("base/modules/extra/files/zshrc");
    let bundle = fx.root.join("base/bundle.toml");
    let before = (fs::read(&zshrc).unwrap(), fs::read(&bundle).unwrap());
    let result = sandboxed(
        &fx,
        "echo x >> ~/.zshrc; echo link=$? >> ~/result\n\
         echo x >> ~/.dotfiles/base/bundle.toml; echo tracked=$? >> ~/result\n\
         echo x > ~/.dotfiles/planted; echo new=$? >> ~/result\n",
    );
    assert_eq!(result, "link=1\ntracked=1\nnew=1\n");
    assert_eq!(
        (fs::read(&zshrc).unwrap(), fs::read(&bundle).unwrap()),
        before
    );
    assert!(!fx.root.join("planted").exists());
}

#[test]
fn replacing_or_removing_a_dfm_link_is_denied() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::in_home();
    fx.bind_with_links();
    let links = [".zshrc", ".ssh/config", ".config/git/config"];
    let targets: Vec<PathBuf> = links
        .iter()
        .map(|l| fs::read_link(fx.h(l)).unwrap())
        .collect();
    let result = sandboxed(
        &fx,
        "mv ~/.zshrc ~/.zshrc.bak; echo mv=$? >> ~/result\n\
         unlink ~/.ssh/config; echo unlink=$? >> ~/result\n\
         ln -sf /etc/hosts ~/.config/git/config; echo ln=$? >> ~/result\n",
    );
    assert_eq!(result, "mv=1\nunlink=1\nln=1\n");
    for (l, t) in links.iter().zip(&targets) {
        assert_eq!(&fs::read_link(fx.h(l)).unwrap(), t, "{l}");
    }
    assert!(!fx.h(".zshrc.bak").exists());
}

#[test]
fn sudo_su_and_osascript_are_denied() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // 126 is the shell's code for an exec the sandbox refused; outside it, sudo -n and su exit 1
    // and osascript 0.
    let result = sandboxed(
        &fx,
        "/usr/bin/sudo -n true; echo sudo=$? >> ~/result\n\
         /usr/bin/su root -c true < /dev/null; echo su=$? >> ~/result\n\
         /usr/bin/osascript -e return; echo osascript=$? >> ~/result\n",
    );
    assert_eq!(result, "sudo=126\nsu=126\nosascript=126\n");
}

#[test]
fn a_write_outside_home_is_denied() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // Both are writable by the test user outside the sandbox, so only the sandbox stops them.
    let name = fx.fake.path().file_name().unwrap().to_str().unwrap();
    let var_tmp = PathBuf::from(format!("/private/var/tmp/{name}"));
    let user_dir = fx.fake.path().join("outside");
    // The profile allows `/private/tmp`, so a test temp dir there cannot show the deny.
    let d3 = !user_dir.starts_with("/private/tmp");
    if !d3 {
        common::skip("the test temp dir is under /private/tmp; the d3 write is allowed there");
    }
    let mut body = format!(
        "echo x > /usr/local/bin/{name} || echo d1 >> ~/result\n\
         echo x > {} || echo d2 >> ~/result\n",
        var_tmp.display()
    );
    if d3 {
        body.push_str(&format!(
            "echo x > {} || echo d3 >> ~/result\n",
            user_dir.display()
        ));
    }
    body.push_str("echo x > \"$TMPDIR/inside\" && echo t >> ~/result\n");
    let result = sandboxed(&fx, &body);
    assert_eq!(result, if d3 { "d1\nd2\nd3\nt\n" } else { "d1\nd2\nt\n" });
    let mut denied = vec![PathBuf::from("/usr/local/bin").join(name), var_tmp];
    if d3 {
        denied.push(user_dir);
    }
    for p in denied {
        assert!(!p.exists(), "{}", p.display());
    }
    assert_eq!(
        fs::read_to_string(fx.tmpdir().join("inside")).unwrap(),
        "x\n"
    );
}

#[test]
fn the_binding_and_the_lock_are_protected() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    let binding = fx.h(".config/dotfiles-manager/machine.toml");
    let lock = fx.h(".local/state/dotfiles-manager.lock");
    let bytes = fs::read(&binding).unwrap();
    assert!(lock.exists());
    let result = sandboxed(
        &fx,
        "echo x > ~/.config/dotfiles-manager/machine.toml || echo d1 >> ~/result\n\
         unlink ~/.local/state/dotfiles-manager.lock || echo d2 >> ~/result\n\
         echo x > ~/.local/state/dotfiles-manager/state/planted || echo d3 >> ~/result\n",
    );
    assert_eq!(result, "d1\nd2\nd3\n");
    assert_eq!(fs::read(&binding).unwrap(), bytes);
    assert!(lock.exists());
    assert!(!fx.state().join("planted").exists());
}

#[test]
fn renaming_an_ancestor_of_a_protected_path_is_denied() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::in_home();
    fx.bind_with_links();
    let result = sandboxed(
        &fx,
        "mv ~/.config ~/.config2 || echo d1 >> ~/result\n\
         mv ~/.local ~/.local2 || echo d2 >> ~/result\n\
         mv ~/.dotfiles ~/.dotfiles2 || echo d3 >> ~/result\n",
    );
    assert_eq!(result, "d1\nd2\nd3\n");
    for (kept, gone) in [
        (".config", ".config2"),
        (".local", ".local2"),
        (".dotfiles", ".dotfiles2"),
    ] {
        assert!(fx.h(kept).is_dir() && !fx.h(gone).exists(), "{kept}");
    }
}

#[test]
fn a_hard_link_to_a_protected_file_is_denied() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::in_home();
    fx.bind_with_links();
    let result = sandboxed(
        &fx,
        "ln ~/.dotfiles/base/modules/extra/files/zshrc ~/hl || echo d >> ~/result\n",
    );
    assert_eq!(result, "d\n");
    assert!(fs::symlink_metadata(fx.h("hl")).is_err());
    let source = fx.root.join("base/modules/extra/files/zshrc");
    assert_eq!(
        std::os::unix::fs::MetadataExt::nlink(&fs::metadata(source).unwrap()),
        1
    );
}

#[test]
fn created_directory_bin_file_and_link_are_recorded() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    let tool = "#!/bin/sh\necho boat\n";
    sandboxed(
        &fx,
        &format!(
            "set -e\nmkdir -p ~/.boat ~/.local/bin\nprintf '{}' > ~/.local/bin/boat\n\
             chmod 755 ~/.local/bin/boat\nln -s ~/.local/bin/boat ~/.local/bin/b\n",
            tool.replace('\n', "\\n")
        ),
    );
    let rec = fx.record("trial-boat");
    let dir = window_entry(&rec, &fx.h(".boat"));
    assert_eq!(
        (&dir["change"], &dir["kind"]),
        (&"created".into(), &"dir".into())
    );
    let file = window_entry(&rec, &fx.h(".local/bin/boat"));
    assert_eq!(
        (&file["change"], &file["kind"]),
        (&"created".into(), &"file".into())
    );
    assert_eq!(file["sha256"], sha(tool.as_bytes()).as_str());
    let link = window_entry(&rec, &fx.h(".local/bin/b"));
    assert_eq!(
        (&link["change"], &link["kind"]),
        (&"created".into(), &"link".into())
    );
    assert_eq!(
        link["target"],
        fx.h(".local/bin/boat").display().to_string().as_str()
    );
}

#[test]
fn a_missing_sandbox_exec_refuses_before_the_preview() {
    let fx = Fx::new();
    fs::remove_file(fx.bin().join("sandbox-exec")).unwrap();
    fx.script("#!/bin/sh\nexit 0\n");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let err = stderr(&o);
    assert!(err.contains("E_PLATFORM_UNSUPPORTED"), "{err}");
    assert!(err.contains("sandbox-exec is not available"), "{err}");
    assert!(
        !text(&o.stdout).contains("plan digest: "),
        "{}",
        text(&o.stdout)
    );
    // Nothing is fetched or cached, and no state is written.
    assert!(!fx.log().contains("curl "), "{}", fx.log());
    assert!(!fx.cached().exists() && !fx.cached().with_extension("json").exists());
    assert!(!fx.state().join("trials").join("trial-boat.json").exists());
}

#[test]
fn a_profile_the_sandbox_rejects_refuses_the_preview_and_the_approved_run() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.script("#!/bin/sh\nexit 0\n");
    let o = fx.trial_script("boat", &["--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let digest = digest_of(&o);
    fs::remove_file(fx.cached()).unwrap();
    fs::remove_file(fx.cached().with_extension("json")).unwrap();
    fs::write(fx.fake.path().join("log"), "").unwrap();
    // The fake now fails the dry run as a rejected profile would.
    fx.marker("sandbox-missing", "");
    let not_accepted = |o: &Output| {
        assert_eq!(code(o), 3, "{}\n{}", stderr(o), text(&o.stdout));
        let err = stderr(o);
        assert!(err.contains("E_PLATFORM_UNSUPPORTED"), "{err}");
        assert!(
            err.contains("the sandbox profile was not accepted"),
            "{err}"
        );
        assert!(
            !text(&o.stdout).contains("plan digest: "),
            "{}",
            text(&o.stdout)
        );
    };
    not_accepted(&fx.trial_script("boat", &["--preview"]));
    // The approved run refuses on the sandbox before it looks for the cached script.
    not_accepted(&fx.trial_script("boat", &["--yes-plan", &digest]));
    assert!(!fx.log().contains("curl "), "{}", fx.log());
    assert!(!fx.cached().exists() && !fx.cached().with_extension("json").exists());
    assert!(!fx.state().join("trials/trial-boat.json").exists());
    assert!(!fx.journal().iter().any(|e| e["op"] == "trial-script"));
}

/// `pgrep` with `args`: the matching pids, none when it finds nothing.
fn pgrep(args: &[&str]) -> Vec<String> {
    let o = Command::new("/usr/bin/pgrep").args(args).output().unwrap();
    text(&o.stdout).lines().map(str::to_string).collect()
}

#[test]
fn a_stop_request_during_a_tty_run_records_the_window_and_exits_143() {
    if no_sandbox() {
        return;
    }
    if !Path::new("/usr/bin/script").exists() {
        common::skip("/usr/bin/script missing");
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // The sleep runs through a link under this test's HOME, so `pgrep -f` finds only its own.
    let sleeper = fx.h("marker-sleep");
    let marker = sleeper.display().to_string();
    fx.script(
        "#!/bin/sh\nln -s /bin/sleep \"$HOME/marker-sleep\"\necho started > \"$HOME/result\"\n\"$HOME/marker-sleep\" 30\n",
    );
    let o = fx.trial_script("boat", &["--tty", "--preview"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let d = digest_of(&o);
    // `script` gives dfm a pty for standard input and returns dfm's exit code.
    let base = fx.command(&["trial", "boat", "--script", URL, "--tty", "--yes-plan", &d]);
    let mut child = Command::new("/usr/bin/script")
        .env_clear()
        .envs(base.get_envs().filter_map(|(k, v)| Some((k, v?))))
        .args(["-q", "/dev/null"])
        .arg(base.get_program())
        .args(base.get_args())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !fx.h("result").exists() || pgrep(&["-f", &marker]).is_empty() {
        if child.try_wait().unwrap().is_some() {
            let o = child.wait_with_output().unwrap();
            let all = format!("{}\n{}", stderr(&o), text(&o.stdout));
            if all.contains("openpty") {
                common::skip("no pty");
                return;
            }
            panic!("ended before the script slept:\n{all}");
        }
        assert!(started.elapsed() < Duration::from_secs(60), "never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let dfm = pgrep(&["-P", &child.id().to_string()]);
    assert_eq!(dfm.len(), 1, "{dfm:?}");
    let k = Command::new("/bin/kill")
        .args(["-TERM", &dfm[0]])
        .status()
        .unwrap();
    assert!(k.success());
    let stopped = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if stopped.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            panic!("dfm did not exit within 15 s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 143, "{}\n{}", stderr(&o), text(&o.stdout));
    let rec = fx.record("trial-boat");
    assert!(rec["script"]["window"].is_array(), "{rec:#}");
    assert_eq!(rec["script"]["exit"]["signal"], "stopped", "{rec:#}");
    let phases: Vec<String> = fx
        .journal()
        .iter()
        .filter(|e| e["op"] == "trial-script")
        .map(|e| e["phase"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(phases, ["intent", "failed"]);
    assert_eq!(pgrep(&["-f", &marker]), Vec::<String>::new());
}

// ---- adopt of a script trial and the script/v1 install (DFM-83) -----------------------------

/// BOAT, and it records the `--env` value it saw, so the install shows the declared environment.
const BOAT_ENV: &str = r#"#!/bin/sh
set -e
mkdir -p "$HOME/.ascii/bin" "$HOME/.config/ascii" "$HOME/.local/bin"
printf '#!/bin/sh\necho boat\n' > "$HOME/.ascii/bin/boat"
chmod 755 "$HOME/.ascii/bin/boat"
echo x > "$HOME/.config/ascii/config"
printf '%s' "$BOAT_NO_UPDATE" > "$HOME/.ascii/env"
ln -s "$HOME/.ascii/bin/boat" "$HOME/.local/bin/boat"
"#;

/// A tool with no configuration: only the binary and its link.
const BARE: &str = r#"#!/bin/sh
set -e
mkdir -p "$HOME/.bare/bin" "$HOME/.local/bin"
printf '#!/bin/sh\necho boat\n' > "$HOME/.bare/bin/boat"
chmod 755 "$HOME/.bare/bin/boat"
ln -s "$HOME/.bare/bin/boat" "$HOME/.local/bin/boat"
"#;

const EXTRA_MANIFEST: &str = "base/modules/extra/module.toml";

impl Fx {
    /// An approved script trial of boat with `extra` trial arguments; it must succeed.
    fn script_trial(&self, body: &str, extra: &[&str]) {
        self.script(body);
        let o = self.trial_script("boat", &[extra, &["--preview"]].concat());
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        let d = digest_of(&o);
        let o = self.trial_script("boat", &[extra, &["--yes-plan", &d]].concat());
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    }

    /// Previews the adoption of trial-boat into `extra`, then runs it with the digest.
    fn adopt(&self) -> (Output, Output) {
        let p = self.run(&["adopt", "trial-boat", "--module", "extra", "--preview"]);
        assert_eq!(code(&p), 0, "{}\n{}", stderr(&p), text(&p.stdout));
        let o = self.run(&[
            "adopt",
            "trial-boat",
            "--module",
            "extra",
            "--yes-plan",
            &digest_of(&p),
        ]);
        (p, o)
    }

    fn manifest(&self) -> String {
        fs::read_to_string(self.root.join(EXTRA_MANIFEST)).unwrap()
    }

    /// Removes the tool as a new machine lacks it; the adopted configuration link stays.
    fn remove_tool(&self) {
        let _ = fs::remove_dir_all(self.h(".ascii"));
        let _ = fs::remove_dir_all(self.h(".bare"));
        let _ = fs::remove_file(self.h(".local/bin/boat"));
    }

    fn grant(&self) -> Output {
        self.run(&["sync", "--trust-origin", &format!("script:{URL}")])
    }

    fn packages(&self, extra: &[&str]) -> Output {
        self.run(&[&["packages"], extra].concat())
    }

    /// Every entry of the script cache directory: an install leaves none behind.
    fn cache_entries(&self) -> Vec<String> {
        fs::read_dir(self.scripts())
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn clear_log(&self) {
        fs::write(self.fake.path().join("log"), "").unwrap();
    }

    fn curls(&self) -> usize {
        self.log()
            .lines()
            .filter(|l| l.starts_with("curl "))
            .count()
    }
}

#[test]
fn adopt_promotes_a_created_config_entry_and_writes_the_declaration() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BOAT_ENV, &["--arg", "--quiet", "--env", "BOAT_NO_UPDATE=1"]);
    let (p, o) = fx.adopt();
    let preview = text(&p.stdout);
    assert!(
        preview.contains(&format!(
            "declaration: boat adapter=script/v1 url={URL} binary=~/.local/bin/boat"
        )),
        "{preview}"
    );
    assert!(
        preview.contains("+1 fs-link/v1 resources, +1 script/v1 declaration"),
        "{preview}"
    );
    assert!(
        preview.contains("brewfile: none (a script tool has no Brewfile line"),
        "{preview}"
    );
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // The created `~/.config/ascii` is promoted; `~/.ascii` and the bin link are not.
    let copy = fx.root.join("base/modules/extra/home/.config/ascii/config");
    assert_eq!(fs::read_to_string(&copy).unwrap(), "x\n");
    assert!(!fx.root.join("base/modules/extra/home/.ascii").exists());
    assert!(!fx.root.join("base/modules/extra/home/.local").exists());
    assert!(
        fs::symlink_metadata(fx.h(".config/ascii/config"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let m = fx.manifest();
    let decl = m.split("\n[[resources]]\n").last().unwrap();
    assert_eq!(
        decl,
        format!(
            "id = \"boat\"\nkind = \"package\"\nadapter = \"script/v1\"\n[resources.value]\nurl = \"{URL}\"\neffective_url = \"{URL}\"\nshell = \"sh\"\nargs = [\"--quiet\"]\nbinary = \".local/bin/boat\"\n[resources.value.env]\nBOAT_NO_UPDATE = \"1\"\n"
        ),
        "{m}"
    );
    assert_eq!(fx.record("trial-boat")["adopted"]["module"], "extra");
    // The adopted composition plans, and the present binary is current: nothing is fetched.
    fs::write(fx.fake.path().join("log"), "").unwrap();
    let o = fx.packages(&["--preview"]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{t}", stderr(&o));
    assert!(t.contains("current extra.boat@package"), "{t}");
    assert!(
        t.contains("boat is installed by script/v1 (~/.local/bin/boat)"),
        "{t}"
    );
    let log = fx.log();
    assert!(
        !log.contains("curl") && !log.contains("sandbox-exec"),
        "{log}"
    );
}

#[test]
fn a_script_trial_without_a_config_entry_is_adopted_as_the_declaration_only() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let before = fx.manifest();
    let (p, o) = fx.adopt();
    let preview = text(&p.stdout);
    assert!(
        preview.contains("+0 fs-link/v1 resources, +1 script/v1 declaration"),
        "{preview}"
    );
    assert!(
        preview.contains("takeover: none (no configuration file)"),
        "{preview}"
    );
    assert!(!preview.contains("candidate:"), "{preview}");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let after = fx.manifest();
    let added = after.strip_prefix(before.as_str()).unwrap();
    assert!(added.contains("adapter = \"script/v1\""), "{added}");
    assert!(!added.contains("fs-link/v1"), "{added}");
    assert!(!added.contains("[resources.value.env]"), "{added}");
    assert!(!fx.root.join("base/modules/extra/home").exists());
    assert_eq!(fx.record("trial-boat")["adopted"]["module"], "extra");
}

#[test]
fn a_script_trial_whose_tool_is_not_on_path_is_refused() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial("#!/bin/sh\nmkdir -p \"$HOME/.config/ascii\"\n", &[]);
    assert!(fx.record("trial-boat")["script"]["binary"].is_null());
    let o = fx.run(&["adopt", "trial-boat", "--module", "extra", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let e = stderr(&o);
    assert!(
        e.contains("E_CONFLICT") && e.contains("the tool is not on PATH"),
        "{e}"
    );
}

#[test]
fn the_secret_scan_covers_the_declaration() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &["--env", "BOAT_TOKEN=FAKE_SECRET"]);
    let before = fx.manifest();
    let o = fx.run(&["adopt", "trial-boat", "--module", "extra", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let e = stderr(&o);
    assert!(
        e.contains("gitleaks found a possible secret in the script/v1 declaration boat"),
        "{e}"
    );
    assert!(!e.contains("FAKE_SECRET"), "{e}");
    assert_eq!(fx.manifest(), before);
}

#[test]
fn packages_install_a_missing_declared_tool_only_with_the_grant() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BOAT_ENV, &["--env", "BOAT_NO_UPDATE=1"]);
    let (_, o) = fx.adopt();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    fx.remove_tool();
    fs::write(fx.fake.path().join("log"), "").unwrap();
    // No grant: blocked with the command, and nothing is fetched or run.
    let o = fx.packages(&["--preview"]);
    let t = format!("{}{}", text(&o.stdout), stderr(&o));
    assert_eq!(code(&o), 3, "{t}");
    assert!(t.contains("E_SOURCE_UNTRUSTED"), "{t}");
    assert!(
        t.contains(&format!("dfm sync --trust-origin 'script:{URL}'")),
        "{t}"
    );
    assert!(!fx.log().contains("curl"), "{}", fx.log());
    // The grant goes to the third list.
    let o = fx.grant();
    assert!(
        fx.state().join("grants.json").exists(),
        "{}\n{}",
        stderr(&o),
        text(&o.stdout)
    );
    let g: serde_json::Value =
        serde_json::from_slice(&fs::read(fx.state().join("grants.json")).unwrap()).unwrap();
    assert_eq!(
        g["scripts"][0]["script"],
        format!("script:{URL}").as_str(),
        "{g:#}"
    );
    assert!(g["packages"].is_null(), "{g:#}");
    // With the grant the preview plans the install offline: no fetch, no cache entry.
    fx.clear_log();
    let o = fx.packages(&["--preview"]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{t}", stderr(&o));
    assert!(t.contains("planned extra.boat@package"), "{t}");
    assert_eq!(fx.curls(), 0, "{}", fx.log());
    assert_eq!(fx.cache_entries(), Vec::<String>::new());
    // The approved run fetches once under the lock and runs the script under the sandbox,
    // without a terminal. The adopted `~/.config/ascii/config` is now a dfm link, which the
    // sandbox denies: a script that writes it fails its row and the repo copy stays as adopted.
    // The failed install fetches nothing more and leaves no cache entry.
    let o = fx.packages(&["--yes-plan", &digest_of(&o)]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 1, "{}\n{t}", stderr(&o));
    assert!(t.contains("failed extra.boat@package"), "{t}");
    assert_eq!(fx.curls(), 1, "{}", fx.log());
    assert_eq!(fx.cache_entries(), Vec::<String>::new());
    assert!(
        stderr(&o).contains("Operation not permitted"),
        "{}",
        stderr(&o)
    );
    let copy = fx.root.join("base/modules/extra/home/.config/ascii/config");
    assert_eq!(fs::read_to_string(&copy).unwrap(), "x\n");
    fx.remove_tool();
    fx.script(BOAT_ENV.replace("echo x > \"$HOME/.config/ascii/config\"\n", ""));
    let o = fx.packages(&["--preview"]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{t}", stderr(&o));
    assert!(t.contains("planned extra.boat@package"), "{t}");
    fs::write(fx.fake.path().join("log"), "").unwrap();
    let o = fx.packages(&["--yes-plan", &digest_of(&o)]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{t}", stderr(&o));
    assert!(t.contains("updated extra.boat@package"), "{t}");
    assert!(fx.h(".local/bin/boat").exists());
    assert_eq!(fs::read_to_string(fx.h(".ascii/env")).unwrap(), "1");
    let log = fx.log();
    assert!(
        log.contains(&format!("/bin/sh {}/run-sync-", fx.scripts().display())),
        "{log}"
    );
    assert!(log.contains("(deny process-exec"), "{log}");
    assert_eq!(fx.curls(), 1, "{log}");
    assert_eq!(fx.cache_entries(), Vec::<String>::new());
    // The next run finds the binary: current, and the script does not run again.
    fs::write(fx.fake.path().join("log"), "").unwrap();
    let o = fx.packages(&["--preview"]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 0, "{}\n{t}", stderr(&o));
    assert!(t.contains("current extra.boat@package"), "{t}");
    let log = fx.log();
    assert!(
        !log.contains("curl") && !log.contains("sandbox-exec"),
        "{log}"
    );
}

#[test]
fn a_redirect_to_another_url_blocks_and_a_prompt_fails_the_install() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let (_, o) = fx.adopt();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    fx.remove_tool();
    fx.grant();
    fx.marker("curl-effective", "https://mirror.invalid/install");
    // The preview is offline: it cannot see the redirect, and it fetches and writes nothing.
    fx.clear_log();
    let o = fx.packages(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.curls(), 0, "{}", fx.log());
    assert_eq!(fx.cache_entries(), Vec::<String>::new());
    // The fetch under the lock ends elsewhere: the row is blocked and names both URLs.
    let o = fx.packages(&["--yes-plan", &digest_of(&o)]);
    let t = format!("{}{}", text(&o.stdout), stderr(&o));
    assert_ne!(code(&o), 0, "{t}");
    assert!(t.contains("blocked extra.boat@package"), "{t}");
    assert!(
        t.contains(&format!(
            "the script {URL} now ends at https://mirror.invalid/install, not at the declared effective URL {URL}"
        )),
        "{t}"
    );
    assert_eq!(fx.curls(), 1, "{}", fx.log());
    assert!(!fx.log().contains("run-sync-"), "{}", fx.log());
    assert_eq!(fx.cache_entries(), Vec::<String>::new());
    // The declared URL again, but the script now asks a question: the install fails.
    fs::remove_file(fx.fake.path().join("curl-effective")).unwrap();
    let rest = BARE.strip_prefix("#!/bin/sh\nset -e\n").unwrap();
    fx.script(format!("#!/bin/sh\nset -e\nread answer < /dev/tty\n{rest}"));
    let o = fx.packages(&["--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    fx.clear_log();
    let o = fx.packages(&["--yes-plan", &digest_of(&o)]);
    let t = text(&o.stdout);
    assert_eq!(code(&o), 1, "{}\n{t}", stderr(&o));
    assert!(t.contains("failed extra.boat@package"), "{t}");
    assert!(!fx.h(".local/bin/boat").exists());
    assert_eq!(fx.curls(), 1, "{}", fx.log());
    assert_eq!(fx.cache_entries(), Vec::<String>::new());
}

#[test]
fn doctor_names_the_grant_a_declared_missing_tool_needs() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let (_, o) = fx.adopt();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    fx.remove_tool();
    // Doctor probes only from an existing private provider directory; a packages run makes it.
    let _ = fx.packages(&["--preview"]);
    fx.clear_log();
    let row = |fx: &Fx| {
        let o = fx.run(&["doctor", "--json"]);
        let v: serde_json::Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("{e}\n{}\n{}", text(&o.stdout), stderr(&o)));
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| {
                r["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("extra.boat"))
            })
            .unwrap_or_else(|| panic!("no boat row in {v:#}"))
            .clone()
    };
    let r = row(&fx);
    assert_eq!(r["code"], "E_SOURCE_UNTRUSTED", "{r:#}");
    assert!(
        r["fix"]
            .as_str()
            .unwrap()
            .contains(&format!("dfm sync --trust-origin 'script:{URL}'")),
        "{r:#}"
    );
    fx.grant();
    let r = row(&fx);
    assert_eq!(r["code"], "D_PLAN_PENDING", "{r:#}");
    assert_eq!(fx.curls(), 0, "{}", fx.log());
}
