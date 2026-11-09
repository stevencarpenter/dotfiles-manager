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
    // The test HOME is on APFS, so the record carries the inode fence and is schema 6.
    assert_eq!(rec["schema"], 6, "{rec:#}");
    let s = &rec["script"];
    assert!(s["fence"]["ino"].as_u64().unwrap() > 0, "{rec:#}");
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
fn doctor_shows_the_script_trial() {
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
    assert_eq!(code(&o), 3, "{t}");
    assert!(t.contains("blocked extra.boat@package"), "{t}");
    // Blocked before the intent: the journal never shows the install as started.
    let intents: Vec<serde_json::Value> = fx
        .journal()
        .into_iter()
        .filter(|e| e["resource"] == "extra.boat" && e["phase"] == "intent")
        .collect();
    assert!(intents.is_empty(), "{intents:?}");
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
    // This install started, so its intent is journaled under the same resource id.
    assert!(
        fx.journal()
            .iter()
            .any(|e| e["resource"] == "extra.boat" && e["phase"] == "intent"),
        "{:?}",
        fx.journal()
    );
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

// ---- doctor rows for script-installed binaries (DFM-84) -------------------------------------

/// The binary BARE installs, as bytes.
const BARE_BINARY: &[u8] = b"#!/bin/sh\necho boat\n";

impl Fx {
    /// The `D_SCRIPT_BINARY` rows of `doctor --json` run with `path` as PATH.
    fn binary_rows(&self, path: &str) -> Vec<serde_json::Value> {
        let o = self
            .command(&["doctor", "--json"])
            .env("PATH", path)
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("{e}\n{}\n{}", text(&o.stdout), stderr(&o)));
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["code"] == "D_SCRIPT_BINARY")
            .cloned()
            .collect()
    }

    /// The one `D_SCRIPT_BINARY` row with the fixture's PATH.
    fn binary_row(&self) -> serde_json::Value {
        let rows = self.binary_rows(&self.path_var());
        assert_eq!(rows.len(), 1, "{rows:#?}");
        rows[0].clone()
    }

    /// An executable file at `rel` below HOME with `body`.
    fn tool_file(&self, rel: &str, body: &[u8]) -> PathBuf {
        let p = self.h(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

fn detail(r: &serde_json::Value) -> &str {
    r["detail"].as_str().unwrap()
}

#[test]
fn doctor_hashes_the_file_the_binary_link_points_to_now() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let link = fx.h(".local/bin/boat");
    let old_real = fs::canonicalize(fx.h(".bare/bin/boat")).unwrap();
    let old = sha(BARE_BINARY);
    let r = fx.binary_row();
    assert_eq!(r["status"], "OK", "{r:#}");
    assert_eq!(r["kind"], "binary", "{r:#}");
    assert_eq!(r["subject"], link.to_str().unwrap(), "{r:#}");
    assert!(detail(&r).starts_with("script trial trial-boat"), "{r:#}");
    assert!(
        detail(&r).contains(&format!("binary unchanged since trial (sha256 {old})")),
        "{r:#}"
    );
    // An updater points the link to a new file and leaves the old file in place. The new file
    // leaves a marker when it runs: doctor must never run it.
    let body = b"#!/bin/sh\ntouch \"$HOME/ran\"\n";
    let new = fx.tool_file(".bare/versions/2/boat", body);
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&new, &link).unwrap();
    let new_real = fs::canonicalize(&new).unwrap();
    let r = fx.binary_row();
    assert_eq!(r["status"], "OK", "{r:#}");
    assert!(
        detail(&r).contains(&format!(
            "binary changed since trial: sha256 {old} -> {}; real path {} -> {}",
            sha(body),
            old_real.display(),
            new_real.display()
        )),
        "{r:#}"
    );
    assert_eq!(fs::read(&old_real).unwrap(), BARE_BINARY);
    assert!(!fx.h("ran").exists());
}

#[test]
fn a_missing_link_target_or_binary_path_is_warn() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    fs::remove_file(fx.h(".bare/bin/boat")).unwrap();
    let r = fx.binary_row();
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert!(
        detail(&r).contains("the link target of the binary path is missing"),
        "{r:#}"
    );
    fs::remove_file(fx.h(".local/bin/boat")).unwrap();
    let r = fx.binary_row();
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert!(detail(&r).contains("the binary path is missing"), "{r:#}");
}

#[test]
fn a_second_copy_on_path_is_warn_and_names_each_owner() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let brew = fx.tool_file("opt/homebrew/bin/boat", b"#!/bin/sh\necho brew\n");
    let mise = fx.tool_file(".local/share/mise/shims/boat", b"#!/bin/sh\necho mise\n");
    let other = fx.tool_file("other/bin/boat", b"#!/bin/sh\necho other\n");
    // A second link to the trial's file is the same file, not a second copy.
    let alias = fx.h("alias/boat");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(fx.h(".bare/bin/boat"), &alias).unwrap();
    let dir = |p: &Path| p.parent().unwrap().display().to_string();
    let path = [
        dir(&alias),
        dir(&brew),
        dir(&mise),
        dir(&other),
        "relative/bin".into(),
        String::new(),
        fx.path_var(),
    ]
    .join(":");
    let rows = fx.binary_rows(&path);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let r = &rows[0];
    assert_eq!(r["status"], "WARN", "{r:#}");
    let d = detail(r);
    assert!(
        d.contains("PATH holds 4 different files named boat:"),
        "{r:#}"
    );
    for (p, owner) in [
        (&alias, "script trial trial-boat"),
        (&brew, "homebrew/v1"),
        (&mise, "mise/v1"),
        (&other, "unknown"),
        (&fx.h(".local/bin/boat"), "script trial trial-boat"),
    ] {
        assert!(
            d.contains(&format!("{} ({owner})", p.display())),
            "{p:?}\n{r:#}"
        );
    }
    // Without the other copies there is no finding.
    assert_eq!(fx.binary_row()["status"], "OK");
}

#[test]
fn an_adopted_declaration_gets_a_binary_row_against_its_trial() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let (_, o) = fx.adopt();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let r = fx.binary_row();
    assert_eq!(r["status"], "OK", "{r:#}");
    assert_eq!(r["subject"], fx.h(".local/bin/boat").to_str().unwrap());
    assert!(
        detail(&r).starts_with("script/v1 declaration extra.boat"),
        "{r:#}"
    );
    assert!(
        detail(&r).contains(&format!(
            "binary unchanged since trial (sha256 {})",
            sha(BARE_BINARY)
        )),
        "{r:#}"
    );
    fx.remove_tool();
    let r = fx.binary_row();
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert!(detail(&r).contains("the binary path is missing"), "{r:#}");
}

#[test]
fn a_script_trial_that_recorded_no_binary_is_warn() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial("#!/bin/sh\nmkdir -p \"$HOME/.config/ascii\"\n", &[]);
    let r = fx.binary_row();
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert_eq!(r["subject"], "trials/trial-boat.json", "{r:#}");
    assert!(detail(&r).contains("no binary was recorded"), "{r:#}");
}

#[test]
fn a_binary_linked_to_a_sensitive_output_is_warn_and_not_hashed() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    // A file the ledger records with the sensitive class, owned by another resource.
    let secret = fx.tool_file(".vault/token", b"hunter2-secret");
    let f = fx.state().join("ledger.json");
    let mut l: serde_json::Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    l["resources"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "resource": "vault", "adapter": "fs-link/v1", "class": "sensitive", "op_id": "x-1",
            "owned_outputs": [{"path": secret, "mode": 420, "fingerprint": null}],
        }));
    fs::write(&f, serde_json::to_vec(&l).unwrap()).unwrap();
    let link = fx.h(".local/bin/boat");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let rows = fx.binary_rows(&fx.path_var());
    let json = serde_json::to_string(&rows).unwrap();
    assert!(!json.contains(&sha(b"hunter2-secret")), "{rows:#?}");
    let r = rows
        .iter()
        .find(|r| r["subject"] == link.to_str().unwrap())
        .unwrap();
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert!(
        detail(r).contains("the binary resolves to a sensitive managed output"),
        "{r:#}"
    );
    assert!(!detail(r).contains("sha256"), "{r:#}");
}

#[test]
fn a_binary_without_the_owner_execute_bit_is_warn() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let real = fs::canonicalize(fx.h(".bare/bin/boat")).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
    let r = fx.binary_row();
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert!(detail(&r).contains("not executable by its owner"), "{r:#}");
    assert!(r["fix"].as_str().unwrap().contains("chmod u+x"), "{r:#}");
}

#[test]
fn a_binary_hard_linked_to_a_sensitive_output_is_not_hashed() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let secret = fx.tool_file(".vault/token", b"hunter2-secret");
    let f = fx.state().join("ledger.json");
    let mut l: serde_json::Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    l["resources"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "resource": "vault", "adapter": "fs-link/v1", "class": "sensitive", "op_id": "x-1",
            "owned_outputs": [{"path": secret, "mode": 420, "fingerprint": null}],
        }));
    fs::write(&f, serde_json::to_vec(&l).unwrap()).unwrap();
    // A second name for the same inode: no path comparison can see it.
    let link = fx.h(".local/bin/boat");
    fs::remove_file(&link).unwrap();
    fs::hard_link(&secret, &link).unwrap();
    let rows = fx.binary_rows(&fx.path_var());
    let json = serde_json::to_string(&rows).unwrap();
    assert!(!json.contains(&sha(b"hunter2-secret")), "{rows:#?}");
    let r = rows
        .iter()
        .find(|r| r["subject"] == link.to_str().unwrap())
        .unwrap();
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert!(
        detail(r).contains("the binary resolves to a sensitive managed output"),
        "{r:#}"
    );
}

#[test]
fn an_unreadable_ledger_hashes_no_binary_and_says_so() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let r = fx.binary_row();
    assert!(detail(&r).contains("sha256"), "{r:#}");
    fs::write(fx.state().join("ledger.json"), b"not json").unwrap();
    let rows = fx.binary_rows(&fx.path_var());
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let r = &rows[0];
    assert_eq!(r["status"], "OK", "{r:#}");
    assert!(detail(r).contains("the ledger cannot be read"), "{r:#}");
    assert!(!detail(r).contains("sha256"), "{r:#}");
}

#[test]
fn an_unresolved_profile_hashes_no_binary_and_says_so() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    assert!(detail(&fx.binary_row()).contains("sha256"));
    fs::write(fx.root.join("profiles/t-01.toml"), b"not [toml").unwrap();
    let rows = fx.binary_rows(&fx.path_var());
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let r = &rows[0];
    assert_eq!(r["status"], "OK", "{r:#}");
    assert!(detail(r).contains("the profile is not resolved"), "{r:#}");
    assert!(!detail(r).contains("sha256"), "{r:#}");
}

#[test]
fn hard_links_to_one_file_on_path_are_not_a_second_copy() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fx.script_trial(BARE, &[]);
    let first = fx.tool_file("hl1/boat", b"#!/bin/sh\necho hl\n");
    let second = fx.h("hl2/boat");
    fs::create_dir_all(second.parent().unwrap()).unwrap();
    fs::hard_link(&first, &second).unwrap();
    let dir = |p: &Path| p.parent().unwrap().display().to_string();
    // Both entries share one inode: one file, no WARN.
    let rows = fx.binary_rows(&[dir(&first), dir(&second)].join(":"));
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["status"], "OK", "{rows:#?}");
    assert!(!detail(&rows[0]).contains("PATH holds"), "{rows:#?}");
}

#[test]
fn a_binary_over_the_hash_cap_is_present_with_no_digest() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // A sparse executable just over 16 MiB: the trial records no binary it cannot hash.
    fx.script_trial(
        &BARE.replace(
            "chmod 755",
            "dd if=/dev/null of=\"$HOME/.bare/bin/boat\" bs=1 seek=16777217 2>/dev/null\nchmod 755",
        ),
        &[],
    );
    let r = fx.binary_row();
    assert_eq!(r["status"], "OK", "{r:#}");
    assert_eq!(
        r["subject"],
        fx.h(".local/bin/boat").to_str().unwrap(),
        "{r:#}"
    );
    assert!(detail(&r).contains("no digest was recorded"), "{r:#}");
    assert!(detail(&r).contains("larger than 16 MiB"), "{r:#}");
    assert!(!detail(&r).contains("sha256"), "{r:#}");
    // The second-copy check still runs.
    fx.tool_file(".other/boat", b"#!/bin/sh\necho other\n");
    let rows = fx.binary_rows(&format!("{}:{}", fx.path_var(), fx.h(".other").display()));
    let r = &rows[0];
    assert_eq!(r["status"], "WARN", "{r:#}");
    assert!(
        detail(r).contains("PATH holds 2 different files named boat"),
        "{r:#}"
    );
}

// ---- purge of a script trial (DFM-82) ----

/// Previews the purge of `trial-boat`, then runs it with the preview's digest. Returns both.
fn purged(fx: &Fx) -> (Output, Output) {
    let preview = fx.run(&["purge", "trial-boat", "--preview"]);
    assert_eq!(
        code(&preview),
        0,
        "{}\n{}",
        stderr(&preview),
        text(&preview.stdout)
    );
    assert_removed_never_kept(&text(&preview.stdout));
    let run = fx.run(&["purge", "trial-boat", "--yes-plan", &digest_of(&preview)]);
    (preview, run)
}

/// Fails when a purge preview names one path (case fold) both as `action: remove` and as
/// `keep:`, `kept:` or `retained:`.
fn assert_removed_never_kept(shown: &str) {
    let path = |l: &str, p: &str| {
        l.strip_prefix(p)
            .and_then(|r| r.split_whitespace().next())
            .map(str::to_lowercase)
    };
    let removed: Vec<String> = shown
        .lines()
        .filter_map(|l| path(l, "action: remove "))
        .collect();
    for l in shown.lines() {
        let kept = path(l, "keep: remove ")
            .or_else(|| path(l, "kept: "))
            .or_else(|| path(l, "retained: "));
        if let Some(k) = kept {
            assert!(!removed.contains(&k), "{k} is removed and kept\n{shown}");
        }
    }
}

fn gone(p: &Path) -> bool {
    fs::symlink_metadata(p).is_err()
}

#[test]
fn purge_removes_a_created_directory_bin_file_and_link_and_retains_the_rest() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    fs::create_dir_all(fx.h("Library/LaunchAgents")).unwrap();
    fs::write(fx.h(".profile"), "export A=1\n").unwrap();
    fs::write(fx.h("real.txt"), "keep me\n").unwrap();
    let before = sha(b"export A=1\n");
    sandboxed(
        &fx,
        "set -e\nmkdir -p ~/.boat/sub\necho x > ~/.boat/sub/f\n\
         printf '#!/bin/sh\\n' > ~/.local/bin/boat\nchmod 755 ~/.local/bin/boat\n\
         ln -s ~/.local/bin/boat ~/.local/bin/b\nln -s ~/real.txt ~/.local/bin/r\n\
         echo 'export B=1' >> ~/.profile\n\
         echo plist > ~/Library/LaunchAgents/dev.boat.plist\n",
    );
    // As `shrink_window` leaves it: the modified entry without its own `before`. The old hash
    // then comes from the record's `before` observation.
    let rec_path = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    let profile_path = fx.h(".profile").display().to_string();
    for e in rec["script"]["window"].as_array_mut().unwrap() {
        if e["path"] == profile_path.as_str() {
            e["before"] = serde_json::Value::Null;
        }
    }
    fs::write(&rec_path, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let profile = fx.h(".profile").display().to_string();
    let plist = fx.h("Library/LaunchAgents/dev.boat.plist");
    for want in [
        format!(
            "action: remove {} directory (2 entries",
            fx.h(".boat").display()
        ),
        format!(
            "action: remove {} file sha256:",
            fx.h(".local/bin/boat").display()
        ),
        format!(
            "action: remove {} link -> {}",
            fx.h(".local/bin/b").display(),
            fx.h(".local/bin/boat").display()
        ),
        format!(
            "retained: {profile} modified during the trial (sha256 {before} -> {}); edit {profile} by hand",
            sha(b"export A=1\nexport B=1\n")
        ),
        format!(
            "retained: {} created during the trial; unload it with launchctl bootout gui/",
            plist.display()
        ),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    let out = text(&run.stdout);
    assert_eq!(code(&run), 0, "{}\n{out}", stderr(&run));
    for p in [".boat", ".local/bin/boat", ".local/bin/b", ".local/bin/r"] {
        assert!(gone(&fx.h(p)), "{p}\n{out}");
    }
    assert!(fx.h(".local/bin").is_dir());
    // The link was unlinked, never followed: the file it named is intact.
    assert_eq!(fs::read_to_string(fx.h("real.txt")).unwrap(), "keep me\n");
    assert_eq!(
        fs::read_to_string(fx.h(".profile")).unwrap(),
        "export A=1\nexport B=1\n"
    );
    assert!(plist.is_file());
    assert!(
        out.contains("trial: state/trials/trial-boat.json removed"),
        "{out}"
    );
    assert!(!fx.state().join("trials/trial-boat.json").exists());
    let removes = fx
        .journal()
        .iter()
        .filter(|e| e["op"] == "remove" && e["phase"] == "done")
        .count();
    assert_eq!(removes, 4, "{out}");
    assert!(fx.receipts().iter().any(|r| r["command"] == "purge"));
}

#[test]
fn a_link_repointed_into_a_created_directory_is_removed_and_one_pointed_elsewhere_is_kept() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    fs::create_dir_all(fx.h(".local/share")).unwrap();
    sandboxed(
        &fx,
        "set -e\nv=~/.local/share/claude/versions\nmkdir -p $v/1\nprintf '#!/bin/sh\\n' > $v/1/claude\n\
         ln -s $v/1/claude ~/.local/bin/claude\nln -s $v/1/claude ~/.local/bin/other\n",
    );
    // An update after the trial: a new version inside the created directory, through a
    // relative link, and the owner's own repointing of the other link.
    fs::create_dir_all(fx.h(".local/share/claude/versions/2")).unwrap();
    fs::remove_file(fx.h(".local/bin/claude")).unwrap();
    std::os::unix::fs::symlink(
        "../share/claude/versions/2/claude",
        fx.h(".local/bin/claude"),
    )
    .unwrap();
    fs::remove_file(fx.h(".local/bin/other")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", fx.h(".local/bin/other")).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let kept = format!(
        "kept: {} -> /usr/bin/true (repointed since the trial)",
        fx.h(".local/bin/other").display()
    );
    assert!(shown.contains(&kept), "{shown}");
    let out = text(&run.stdout);
    assert_eq!(code(&run), 0, "{}\n{out}", stderr(&run));
    assert!(gone(&fx.h(".local/bin/claude")), "{out}");
    assert!(gone(&fx.h(".local/share/claude")), "{out}");
    assert_eq!(
        fs::read_link(fx.h(".local/bin/other")).unwrap(),
        Path::new("/usr/bin/true")
    );
    assert!(!fx.state().join("trials/trial-boat.json").exists());
}

#[test]
fn a_created_directory_of_a_truncated_window_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/share")).unwrap();
    sandboxed(
        &fx,
        "set -e\nmkdir ~/.local/share/c\nln -s /usr/bin/false ~/.local/share/c/l\n",
    );
    // As `shrink_window` leaves it: the link inside the directory dropped, the token added.
    let (dir, link) = (fx.h(".local/share/c"), fx.h(".local/share/c/l"));
    let p = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    window_entry(&rec, &dir);
    rec["script"]["window"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["path"] != link.display().to_string().as_str());
    rec["unknown"]
        .as_array_mut()
        .unwrap()
        .push("window-truncated".into());
    fs::write(&p, serde_json::to_vec(&rec).unwrap()).unwrap();
    // The owner repoints the link the window no longer lists.
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", &link).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want = format!(
        "retained: {} (the window was truncated; its contents were not all recorded)",
        dir.display()
    );
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(
        !shown
            .lines()
            .any(|l| l.contains("action: remove") && l.contains(&dir.display().to_string())),
        "{shown}"
    );
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("/usr/bin/true"));
}

#[test]
fn a_baseline_directory_of_a_truncated_window_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    let bin = fx.h("bin");
    let bs = bin.display().to_string();
    fx.script_trial(
        "#!/bin/sh\nset -e\nmkdir ~/bin\necho x > ~/bin/mine\n",
        &["--config", &bs],
    );
    // As `shrink_window` may leave it: no window entry under `~/bin`, the token added. Only the
    // baseline item names the directory.
    let p = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    rec["script"]["window"]
        .as_array_mut()
        .unwrap()
        .retain(|e| !e["path"].as_str().unwrap().starts_with(bs.as_str()));
    rec["unknown"]
        .as_array_mut()
        .unwrap()
        .push("window-truncated".into());
    fs::write(&p, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want =
        format!("retained: {bs} (the window was truncated; its contents were not all recorded)");
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_to_string(bin.join("mine")).unwrap(), "x\n");
}

#[test]
fn a_link_into_a_created_directory_before_could_not_read_is_kept() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    fs::create_dir_all(fx.h(".local/share")).unwrap();
    sandboxed(
        &fx,
        "set -e\nmkdir ~/.local/share/c\nln -s /usr/bin/false ~/.local/bin/l\n",
    );
    // `~/.local/share` could not be listed before the run, so `c` may have been there all
    // along; the owner points the link into it.
    let (share, dir, link) = (
        fx.h(".local/share"),
        fx.h(".local/share/c"),
        fx.h(".local/bin/l"),
    );
    let p = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    window_entry(&rec, &dir);
    let before = rec["script"]["before"].as_array_mut().unwrap();
    before.retain(|o| o["path"] != share.display().to_string().as_str());
    before.push(
        serde_json::json!({"path": share.display().to_string(), "kind": "unreadable",
        "sha256": null, "target": null}),
    );
    fs::write(&p, serde_json::to_vec(&rec).unwrap()).unwrap();
    let into = dir.join("v");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&into, &link).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want = format!(
        "kept: {} -> {} (repointed since the trial)",
        link.display(),
        into.display()
    );
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_link(&link).unwrap(), into);
    assert!(dir.is_dir());
}

#[test]
fn a_link_into_a_held_created_directory_is_kept() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    // `~/.config/fish` appears in a watched listing and `conf.d` is a watched directory, so the
    // window lists the link inside and `fish` is a created directory candidate.
    sandboxed(
        &fx,
        "set -e\nmkdir -p ~/.config/fish/conf.d\nln -s /usr/bin/false ~/.config/fish/conf.d/l\n\
         ln -s /usr/bin/false ~/.local/bin/m\n",
    );
    // The owner repoints the link inside `fish`, which holds `fish`, and points `m` into it.
    let (dir, inner, link) = (
        fx.h(".config/fish"),
        fx.h(".config/fish/conf.d/l"),
        fx.h(".local/bin/m"),
    );
    fs::remove_file(&inner).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", &inner).unwrap();
    let into = dir.join("v");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&into, &link).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    for want in [
        format!(
            "retained: {} (holds {}, which is kept)",
            dir.display(),
            inner.display()
        ),
        format!(
            "kept: {} -> {} (points into {}, which is kept)",
            link.display(),
            into.display(),
            dir.display()
        ),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_link(&link).unwrap(), into);
    assert_eq!(fs::read_link(&inner).unwrap(), Path::new("/usr/bin/true"));
}

#[test]
fn an_interrupted_record_is_purged_from_its_before_observation() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    fs::write(fx.h(".keep"), "mine\n").unwrap();
    sandboxed(&fx, "set -e\nmkdir ~/.boat\n");
    // The shape a killed run leaves: `before` and no exit, window or binary.
    let p = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    for k in ["exit", "window", "binary"] {
        rec["script"][k] = serde_json::Value::Null;
    }
    fs::write(&p, serde_json::to_vec(&rec).unwrap()).unwrap();
    // Created after the run ended: still a candidate, because the live state is compared.
    fs::write(fx.h(".later"), "x").unwrap();
    // A link to a path outside every created directory: the live target is not a recorded one.
    std::os::unix::fs::symlink(fx.h(".keep"), fx.h(".local/bin/x")).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let note = shown
        .find("note: the script run of this trial did not finish")
        .unwrap_or_else(|| panic!("{shown}"));
    assert!(note < shown.find("plan digest:").unwrap(), "{shown}");
    let kept = format!(
        "kept: {} -> {} (no target recorded)",
        fx.h(".local/bin/x").display(),
        fx.h(".keep").display()
    );
    assert!(shown.contains(&kept), "{shown}");
    for want in [".boat", ".later"] {
        let l = format!("action: remove {}", fx.h(want).display());
        assert!(shown.contains(&l), "{l}\n{shown}");
    }
    let out = text(&run.stdout);
    assert_eq!(code(&run), 0, "{}\n{out}", stderr(&run));
    assert!(gone(&fx.h(".boat")) && gone(&fx.h(".later")), "{out}");
    assert_eq!(fs::read_to_string(fx.h(".keep")).unwrap(), "mine\n");
    assert!(fs::symlink_metadata(fx.h(".local/bin/x")).is_ok());
    assert!(!p.exists());
}

#[test]
fn a_purge_without_approval_or_with_a_stale_digest_changes_nothing() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    sandboxed(
        &fx,
        "set -e\nmkdir ~/.boat\necho x > ~/.boat/f\nln -s ~/.boat/f ~/.local/bin/l\n",
    );
    let o = fx.run(&["purge", "trial-boat"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
    // The digest covers the live tree, so content added after the preview is stale.
    let preview = fx.run(&["purge", "trial-boat", "--preview"]);
    fs::write(fx.h(".boat/g"), "y").unwrap();
    let o = fx.run(&["purge", "trial-boat", "--yes-plan", &digest_of(&preview)]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(fx.h(".boat/f").is_file() && fx.h(".boat/g").is_file());
    // The digest covers a link's target text, so a link repointed after the preview is stale.
    let preview = fx.run(&["purge", "trial-boat", "--preview"]);
    fs::remove_file(fx.h(".local/bin/l")).unwrap();
    std::os::unix::fs::symlink(fx.h(".boat/g"), fx.h(".local/bin/l")).unwrap();
    let o = fx.run(&["purge", "trial-boat", "--yes-plan", &digest_of(&preview)]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(fs::symlink_metadata(fx.h(".local/bin/l")).is_ok());
    assert!(fx.state().join("trials/trial-boat.json").exists());
}

#[test]
fn a_file_changed_after_the_locked_decision_is_not_removed() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    sandboxed(&fx, "set -e\necho x > ~/.boat-rc\n");
    let preview = fx.run(&["purge", "trial-boat", "--preview"]);
    assert_eq!(code(&preview), 0, "{}", stderr(&preview));
    let gate = fx.fake.path().join("go");
    let child = fx
        .command(&["purge", "trial-boat", "--yes-plan", &digest_of(&preview)])
        .env(
            "DFM_FAILPOINT",
            format!("purge-remove:wait-{}", gate.display()),
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
    fs::write(fx.h(".boat-rc"), "edited\n").unwrap();
    fs::write(&gate, "").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_ne!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(fs::read_to_string(fx.h(".boat-rc")).unwrap(), "edited\n");
}

#[test]
fn a_tampered_window_entry_for_a_pre_existing_or_unwatched_path_is_kept() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h("Documents/inner")).unwrap();
    fs::write(fx.h("Documents/f"), "mine\n").unwrap();
    sandboxed(&fx, "exit 0\n");
    // `~/Documents` is in the HOME listing of `before`; `~/Documents/inner` is in no watched
    // place. A record that marks either as created is not believed.
    let p = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    let created = |rel: &str| {
        serde_json::json!({"path": fx.h(rel).display().to_string(), "change": "created",
            "kind": "dir", "sha256": null, "target": null, "before": null})
    };
    rec["script"]["window"] = serde_json::json!([created("Documents"), created("Documents/inner")]);
    fs::write(&p, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    for want in [
        format!(
            "retained: {} is in the observation before the run",
            fx.h("Documents").display()
        ),
        format!(
            "retained: {} is not in a watched place",
            fx.h("Documents/inner").display()
        ),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(fx.h("Documents/inner").is_dir());
    assert_eq!(fs::read_to_string(fx.h("Documents/f")).unwrap(), "mine\n");
}

#[test]
fn a_created_directory_that_holds_a_kept_link_is_retained_and_its_other_entries_removed() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    sandboxed(
        &fx,
        "set -e\nmkdir ~/bin\nprintf '#!/bin/sh\\n' > ~/bin/tool\nln -s ~/bin/tool ~/bin/x\n",
    );
    // The owner repoints the link after the trial, so purge keeps it, and so its directory.
    fs::remove_file(fx.h("bin/x")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", fx.h("bin/x")).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let (bin, x) = (fx.h("bin"), fx.h("bin/x"));
    for want in [
        format!(
            "kept: {} -> /usr/bin/true (repointed since the trial)",
            x.display()
        ),
        format!(
            "retained: {} (holds {}, which is kept)",
            bin.display(),
            x.display()
        ),
        format!("action: remove {} file", fx.h("bin/tool").display()),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(
        !shown.contains(&format!("action: remove {} directory", bin.display())),
        "{shown}"
    );
    let out = text(&run.stdout);
    assert_eq!(code(&run), 0, "{}\n{out}", stderr(&run));
    assert!(gone(&fx.h("bin/tool")), "{out}");
    assert_eq!(fs::read_link(&x).unwrap(), Path::new("/usr/bin/true"));
    assert!(!fx.state().join("trials/trial-boat.json").exists());
}

#[test]
fn a_created_directory_that_holds_a_kept_baseline_path_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // `~/bin/x` is a declared path: a baseline item, kept because it is a link.
    let x = fx.h("bin/x");
    let xs = x.display().to_string();
    fx.script_trial(
        "#!/bin/sh\nset -e\nmkdir ~/bin\nln -s /usr/bin/true ~/bin/x\n",
        &["--config", &xs],
    );
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let bin = fx.h("bin");
    for want in [
        format!("keep: remove {xs} link"),
        format!("retained: {} (holds {xs}, which is kept)", bin.display()),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_link(&x).unwrap(), Path::new("/usr/bin/true"));
}

#[test]
fn a_baseline_directory_a_kept_link_points_into_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    // `~/.config/boat` is a declared path, so it is a baseline item and no window candidate:
    // it cannot vouch for the link.
    let cfg = fx.h(".config/boat");
    let cs = cfg.display().to_string();
    fx.script_trial(
        "#!/bin/sh\nset -e\nmkdir -p ~/.config/boat\nln -s /usr/bin/true ~/.local/bin/boat\n",
        &["--config", &cs],
    );
    // After the trial the link is repointed into the baseline directory.
    let link = fx.h(".local/bin/boat");
    fs::write(cfg.join("boat"), "#!/bin/sh\n").unwrap();
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("../../.config/boat/boat", &link).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let ls = link.display().to_string();
    for want in [
        format!("kept: {ls} -> ../../.config/boat/boat (repointed since the trial)"),
        format!("retained: {cs} (the kept link {ls} points into it)"),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_to_string(cfg.join("boat")).unwrap(), "#!/bin/sh\n");
    assert!(fs::metadata(&link).is_ok(), "the link must not dangle");
}

#[test]
fn a_baseline_path_created_as_a_file_and_now_a_directory_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".config")).unwrap();
    // `~/.config/boat` is both a declared path and a created window entry.
    let boat = fx.h(".config/boat");
    let bs = boat.display().to_string();
    fx.script_trial(
        "#!/bin/sh\nset -e\necho x > ~/.config/boat\n",
        &["--config", &bs],
    );
    fs::remove_file(&boat).unwrap();
    fs::create_dir(&boat).unwrap();
    fs::write(boat.join("mine"), "user data").unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want =
        format!("retained: {bs} created as a file during the trial and now a dir; not removed");
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_to_string(boat.join("mine")).unwrap(), "user data");
}

#[test]
fn a_created_entry_in_a_place_before_could_not_read_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    sandboxed(&fx, "set -e\necho x > ~/.local/bin/old\nmkdir ~/.ghost\n");
    // As `observe` records it when `~/.local/bin` cannot be listed: a marker in `before`, so
    // `old` (there all along) looks created once the directory is readable. A created marker
    // is never a candidate either.
    let p = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    let (bin, old, ghost) = (fx.h(".local/bin"), fx.h(".local/bin/old"), fx.h(".ghost"));
    let before = rec["script"]["before"].as_array_mut().unwrap();
    before.retain(|o| o["path"] != bin.display().to_string().as_str());
    before.push(
        serde_json::json!({"path": bin.display().to_string(), "kind": "unreadable",
        "sha256": null, "target": null}),
    );
    for e in rec["script"]["window"].as_array_mut().unwrap() {
        if e["path"] == ghost.display().to_string().as_str() {
            e["kind"] = "unreadable".into();
        }
    }
    fs::write(&p, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    for want in [
        format!(
            "retained: {} is in {}, which could not be read before the run",
            old.display(),
            bin.display()
        ),
        format!(
            "retained: {} could not be read after the run; never removed",
            ghost.display()
        ),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(old.is_file() && ghost.is_dir());
}

#[test]
fn a_file_another_active_trial_changed_is_kept_and_one_it_only_observed_is_not() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    sandboxed(&fx, "set -e\necho a > ~/.local/bin/shared\n");
    // A second trial modifies the file the first one created.
    fx.script("#!/bin/sh\necho b >> ~/.local/bin/shared\n");
    let o = fx.approved("ship");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let shared = fx.h(".local/bin/shared");
    let name = shared.display().to_string();
    let ship = fx.state().join("trials/trial-ship.json");
    let full = fx.record("trial-ship");
    assert_eq!(window_entry(&full, &shared)["change"], "modified");
    let without = |rec: &mut serde_json::Value, key: &str| {
        rec["script"][key]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["path"] != name.as_str());
    };
    // The window entry alone keeps the file. Without it, the trial started after boat and
    // only observed the file in `before`, which does not block the purge. A run that did not
    // finish has no window, so what it changed is unknown: no window candidate is removed,
    // even one absent from its `before` (it may have created the file again).
    let mut window_only = full.clone();
    without(&mut window_only, "before");
    let mut untouched = full.clone();
    without(&mut untouched, "window");
    let mut interrupted = full.clone();
    for k in ["exit", "window", "binary"] {
        interrupted["script"][k] = serde_json::Value::Null;
    }
    let mut interrupted_unseen = interrupted.clone();
    without(&mut interrupted_unseen, "before");
    // A truncated window may have dropped the change, so it is incomplete too.
    let mut truncated = untouched.clone();
    truncated["unknown"]
        .as_array_mut()
        .unwrap()
        .push("window-truncated".into());
    // A window path above the file keeps it too: here the trial turned `~/.local/bin` from a
    // file into a directory (a record edit; the file is the same).
    let bin = fx.h(".local/bin").display().to_string();
    let mut above = untouched.clone();
    let mut e = window_entry(&full, &shared).clone();
    e["path"] = bin.as_str().into();
    e["change"] = "kind-changed".into();
    e["kind"] = "dir".into();
    e["sha256"] = serde_json::Value::Null;
    e["before"]["path"] = bin.as_str().into();
    let w = above["script"]["window"].as_array_mut().unwrap();
    w.push(e);
    w.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let incomplete = format!(
        "retained: {name} (script trial trial-ship is incomplete; what it changed is not recorded)"
    );
    for (case, rec, kept) in [
        ("window", window_only, Some(&name)),
        ("untouched", untouched, None),
        // Its `before` holds the file, which keeps it by that rule first (here and when
        // truncated); without it, the incomplete rule keeps it.
        ("interrupted", interrupted, Some(&name)),
        ("interrupted, unseen", interrupted_unseen, Some(&incomplete)),
        ("truncated", truncated, Some(&name)),
        ("above", above, Some(&bin)),
    ] {
        fs::write(&ship, serde_json::to_vec(&rec).unwrap()).unwrap();
        let o = fx.run(&["purge", "trial-boat", "--preview"]);
        let shown = text(&o.stdout);
        assert_eq!(code(&o), 0, "{case}: {}\n{shown}", stderr(&o));
        assert_removed_never_kept(&shown);
        match kept {
            Some(line) if *line == incomplete => {
                assert!(shown.contains(line), "{case}: {line}\n{shown}");
                assert!(!shown.contains("action: remove"), "{case}: {shown}");
            }
            Some(p) => {
                let want = format!("keep: remove {name} file sha256:");
                assert!(shown.contains(&want), "{case}: {want}\n{shown}");
                let why = format!("trial trial-ship observed {p}");
                assert!(shown.contains(&why), "{case}: {why}\n{shown}");
            }
            None => {
                let want = format!("action: remove {name} file sha256:");
                assert!(shown.contains(&want), "{case}: {want}\n{shown}");
                assert!(
                    !shown.contains("trial trial-ship observed"),
                    "{case}: {shown}"
                );
            }
        }
    }
    fs::write(&ship, serde_json::to_vec(&full).unwrap()).unwrap();
    let (_, run) = purged(&fx);
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_to_string(&shared).unwrap(), "a\nb\n");
    assert!(!fx.state().join("trials/trial-boat.json").exists());
    assert!(ship.exists());
}

#[test]
fn a_file_renamed_only_in_case_during_the_trial_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h("bin")).unwrap();
    fs::write(fx.h("bin/Keep"), "mine\n").unwrap();
    // Only a case-insensitive volume (the macOS default) shows the defect.
    if !fx.h("bin/keep").exists() {
        return;
    }
    sandboxed(&fx, "set -e\nmv ~/bin/Keep ~/bin/keep\n");
    let (keep, upper) = (fx.h("bin/keep"), fx.h("bin/Keep"));
    let rec = fx.record("trial-boat");
    assert_eq!(window_entry(&rec, &keep)["change"], "created");
    assert_eq!(window_entry(&rec, &upper)["change"], "removed");
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want = format!(
        "retained: {} is in the observation before the run",
        keep.display()
    );
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_to_string(&keep).unwrap(), "mine\n");
}

#[test]
fn a_baseline_path_is_retained_while_another_script_trial_is_incomplete() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    let rc = fx.h(".boatrc");
    let rs = rc.display().to_string();
    fx.script_trial("#!/bin/sh\necho a > ~/.boatrc\n", &["--config", &rs]);
    // A second trial that did not finish (no window) and whose `before` does not hold the
    // file: it may have created the file again after its `before`.
    fx.script("#!/bin/sh\nexit 0\n");
    let o = fx.approved("ship");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let ship = fx.state().join("trials/trial-ship.json");
    let mut rec = fx.record("trial-ship");
    for k in ["exit", "window", "binary"] {
        rec["script"][k] = serde_json::Value::Null;
    }
    rec["script"]["before"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["path"] != rs.as_str());
    fs::write(&ship, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want = format!(
        "retained: {rs} (script trial trial-ship is incomplete; what it changed is not recorded)"
    );
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(rc.is_file());
}

#[test]
fn a_run_that_ended_without_a_window_removes_no_window_candidate() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    sandboxed(&fx, "set -e\nmkdir ~/.boat\necho x > ~/.boat/f\n");
    // As a failed after-observation leaves it: the exit is recorded, the window is not.
    let rec_path = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    assert!(!rec["script"]["exit"].is_null());
    rec["script"]["window"] = serde_json::Value::Null;
    fs::write(&rec_path, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    assert!(
        shown.contains("the run ended but its after-observation was not recorded"),
        "{shown}"
    );
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(fx.h(".boat/f").is_file());
}

#[test]
fn a_created_link_whose_target_is_not_utf8_is_kept() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    sandboxed(&fx, "ln -s \"$(printf 'x\\377')\" ~/.local/bin/odd\n");
    let link = fx.h(".local/bin/odd");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    assert!(
        shown.contains("its target is not UTF-8, so it cannot be compared"),
        "{shown}"
    );
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(fs::symlink_metadata(&link).is_ok());
}

#[test]
fn a_launch_agent_given_as_config_is_retained_with_the_bootout_hint() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h("Library/LaunchAgents")).unwrap();
    let plist = fx.h("Library/LaunchAgents/dev.boat.plist");
    let ps = plist.display().to_string();
    fx.script_trial(
        "#!/bin/sh\necho plist > ~/Library/LaunchAgents/dev.boat.plist\n",
        &["--config", &ps],
    );
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want =
        format!("retained: {ps} created during the trial; unload it with launchctl bootout gui/");
    assert!(shown.contains(&want), "{want}\n{shown}");
    // The path in the command is one shell word.
    let quoted = format!(" '{ps}', then remove it by hand");
    assert!(shown.contains(&quoted), "{quoted}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(plist.is_file());
}

#[test]
fn a_file_or_directory_moved_in_from_an_unwatched_place_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    fs::create_dir_all(fx.h(".config")).unwrap();
    // The owner's data, in a place the window does not watch, before the trial.
    fs::create_dir_all(fx.h("Documents/proj/src")).unwrap();
    fs::create_dir_all(fx.h("Documents/proj2")).unwrap();
    fs::write(fx.h("Documents/keep.txt"), "mine\n").unwrap();
    fs::write(fx.h("Documents/proj/src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(fx.h("Documents/proj2/notes"), "notes\n").unwrap();
    sandboxed(
        &fx,
        "set -e\nmv ~/Documents/keep.txt ~/.local/bin/keep.txt\n\
         mv ~/Documents/proj ~/.config/proj\n\
         mkdir ~/.config/newtool\nmv ~/Documents/proj2 ~/.config/newtool/proj2\n\
         mkdir ~/.boat\necho x > ~/.boat/f\n",
    );
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let moved = "existed before the trial (inode older than the run); it may have been moved in";
    for want in [
        format!(
            "retained: {} (it {moved})",
            fx.h(".local/bin/keep.txt").display()
        ),
        format!("retained: {} (it {moved})", fx.h(".config/proj").display()),
        format!(
            "retained: {} (it holds {}, which {moved})",
            fx.h(".config/newtool").display(),
            fx.h(".config/newtool/proj2").display()
        ),
        format!("action: remove {} directory", fx.h(".boat").display()),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    let out = text(&run.stdout);
    assert_eq!(code(&run), 0, "{}\n{out}", stderr(&run));
    assert!(gone(&fx.h(".boat")), "{out}");
    assert_eq!(
        fs::read_to_string(fx.h(".local/bin/keep.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        fs::read_to_string(fx.h(".config/proj/src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert_eq!(
        fs::read_to_string(fx.h(".config/newtool/proj2/notes")).unwrap(),
        "notes\n"
    );
}

#[test]
fn a_record_without_an_inode_fence_retains_every_window_candidate() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    sandboxed(
        &fx,
        "set -e\nmkdir ~/.boat\necho x > ~/.boat/f\nprintf '#!/bin/sh\\n' > ~/.local/bin/boat\n\
         ln -s ~/.local/bin/boat ~/.local/bin/b\n",
    );
    // A record written before the fence, or on a volume that is not APFS: schema 5, no fence.
    let p = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    assert_eq!(rec["schema"], 6, "{rec:#}");
    assert!(
        rec["script"]["fence"]["ino"].as_u64().unwrap() > 0,
        "{rec:#}"
    );
    rec["script"].as_object_mut().unwrap().remove("fence");
    rec["schema"] = 5.into();
    fs::write(&p, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    for rel in [".boat", ".local/bin/boat", ".local/bin/b"] {
        let want = format!(
            "retained: {} (the record has no inode fence)",
            fx.h(rel).display()
        );
        assert!(shown.contains(&want), "{want}\n{shown}");
        assert!(fs::symlink_metadata(fx.h(rel)).is_ok(), "{rel}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(fx.h(".boat/f").is_file());
}

#[test]
fn an_inexact_window_name_keeps_every_window_candidate() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    // A link whose name holds a control character: the window records it as kind `other`, and
    // purge cannot read it back to see where it points, a directory or a file.
    sandboxed(
        &fx,
        "set -e\nmkdir ~/.boat\necho x > ~/.boat/f\n\
         printf '#!/bin/sh\\n' > ~/.local/bin/boat\n\
         ln -s ~/.local/bin/boat \"$HOME/.local/bin/b$(printf '\\001')\"\n",
    );
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let why = "a window entry has a name that cannot be shown exactly; nothing the trial created is removed";
    // The directory is already held by the tree rule; the file by the rule for any candidate.
    for want in [
        format!(
            "retained: {} (a window entry has a name that cannot be shown exactly; links in it cannot be followed)",
            fx.h(".boat").display()
        ),
        format!("retained: {} ({why})", fx.h(".local/bin/boat").display()),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_to_string(fx.h(".boat/f")).unwrap(), "x\n");
    assert!(fx.h(".local/bin/boat").is_file());
}

#[test]
fn a_baseline_data_path_moved_in_from_elsewhere_is_kept() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    // `~/Library/Caches/boat` is a data candidate the window does not watch.
    fs::create_dir_all(fx.h("Documents/old")).unwrap();
    fs::write(fx.h("Documents/old/keep.txt"), "mine\n").unwrap();
    fs::create_dir_all(fx.h("Library/Caches")).unwrap();
    sandboxed(&fx, "set -e\nmv ~/Documents/old ~/Library/Caches/boat\n");
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let cache = fx.h("Library/Caches/boat").display().to_string();
    assert!(
        shown.contains(&format!("keep: remove {cache}"))
            && shown.contains("existed before the trial"),
        "{shown}"
    );
    assert!(
        !shown.contains(&format!("action: remove {cache}")),
        "{shown}"
    );
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(
        fs::read_to_string(fx.h("Library/Caches/boat/keep.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn an_inexact_link_to_a_baseline_file_keeps_that_file() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    let rc = fx.h(".boatrc");
    let rs = rc.display().to_string();
    // `~/.boatrc` is a baseline path; the link to it has a control character in its name.
    fx.script_trial(
        "#!/bin/sh\nset -e\necho a > ~/.boatrc\nln -s ~/.boatrc \"$HOME/.local/bin/b$(printf '\\001')\"\n",
        &["--config", &rs],
    );
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want = format!(
        "retained: {rs} (a window entry has a name that cannot be shown exactly; nothing the trial created is removed)"
    );
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(rc.is_file());
}

#[test]
fn a_window_without_an_exit_removes_no_window_candidate() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    sandboxed(&fx, "set -e\nmkdir ~/.boat\necho x > ~/.boat/f\n");
    // As a failed journal append or launch leaves it: the window was observed, no exit.
    let rec_path = fx.state().join("trials/trial-boat.json");
    let mut rec = fx.record("trial-boat");
    assert!(!rec["script"]["window"].is_null());
    rec["script"]["exit"] = serde_json::Value::Null;
    fs::write(&rec_path, serde_json::to_vec(&rec).unwrap()).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    let want = format!(
        "retained: {} (the script never started (no exit was recorded); nothing it created is removed)",
        fx.h(".boat").display()
    );
    assert!(shown.contains(&want), "{want}\n{shown}");
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert!(fx.h(".boat/f").is_file());
}

#[test]
fn a_same_length_rewrite_in_a_created_directory_is_stale() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    sandboxed(&fx, "set -e\nmkdir ~/.boat\necho x > ~/.boat/f\n");
    // After the preview: same path, kind and size, other bytes.
    let preview = fx.run(&["purge", "trial-boat", "--preview"]);
    assert_eq!(code(&preview), 0, "{}", stderr(&preview));
    fs::write(fx.h(".boat/f"), "y\n").unwrap();
    let o = fx.run(&["purge", "trial-boat", "--yes-plan", &digest_of(&preview)]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(fs::read_to_string(fx.h(".boat/f")).unwrap(), "y\n");
    // The same rewrite after the locked decision, just before the removal.
    let preview = fx.run(&["purge", "trial-boat", "--preview"]);
    let gate = fx.fake.path().join("go");
    let child = fx
        .command(&["purge", "trial-boat", "--yes-plan", &digest_of(&preview)])
        .env(
            "DFM_FAILPOINT",
            format!("purge-remove:wait-{}", gate.display()),
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
    fs::write(fx.h(".boat/f"), "z\n").unwrap();
    fs::write(&gate, "").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_ne!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(fs::read_to_string(fx.h(".boat/f")).unwrap(), "z\n");
}

#[test]
fn a_created_file_a_kept_link_was_repointed_at_is_retained() {
    if no_sandbox() {
        return;
    }
    let fx = Fx::new();
    fx.bind_with_links();
    fs::create_dir_all(fx.h(".local/bin")).unwrap();
    sandboxed(
        &fx,
        "set -e\nprintf '#!/bin/sh\\n' > ~/.local/bin/tool\nln -s /usr/bin/true ~/.local/bin/alias\n",
    );
    // The owner repoints the alias at the created tool after the trial: by name, and through
    // a linked parent.
    let (tool, alias) = (fx.h(".local/bin/tool"), fx.h(".local/bin/alias"));
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("tool", &alias).unwrap();
    let (preview, run) = purged(&fx);
    let shown = text(&preview.stdout);
    for want in [
        format!(
            "kept: {} -> tool (repointed since the trial)",
            alias.display()
        ),
        format!(
            "retained: {} (the kept link {} points at it)",
            tool.display(),
            alias.display()
        ),
    ] {
        assert!(shown.contains(&want), "{want}\n{shown}");
    }
    assert!(!shown.contains("action: remove"), "{shown}");
    assert_eq!(code(&run), 0, "{}\n{}", stderr(&run), text(&run.stdout));
    assert_eq!(fs::read_to_string(&tool).unwrap(), "#!/bin/sh\n");
    assert!(fs::metadata(&alias).is_ok(), "the alias must not dangle");
}
