//! `trial --script`: an install script is fetched through the `curl` look-alike, cached and
//! previewed (DFM-81). No real network access and no real script run.

mod common;

use common::providers::{FAKE_BREW, FAKE_CURL, FAKE_GETCONF, FAKE_MISE, FAKE_SANDBOX_EXEC};
use common::{TempDir, code, text};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

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
        fs::create_dir_all(fx.bin()).unwrap();
        fs::create_dir_all(fx.tmpdir()).unwrap();
        for (name, body) in [
            ("brew", FAKE_BREW),
            ("mise", FAKE_MISE),
            ("curl", FAKE_CURL),
            ("sandbox-exec", FAKE_SANDBOX_EXEC),
            ("getconf", FAKE_GETCONF),
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
        std::process::Command::new(BIN)
            .env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
            .env("USER", "leak-check")
            .env("TMPDIR", self.tmpdir())
            .env("SECRET_TOKEN", "do-not-pass")
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn trial_script(&self, tool: &str, extra: &[&str]) -> Output {
        let mut v = vec!["trial", tool, "--script", URL];
        v.extend_from_slice(extra);
        self.run(&v)
    }
}

#[test]
fn preview_fetches_once_caches_the_script_and_prints_the_path_and_hints() {
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
            "curl --proto =https --proto-redir =https --tlsv1.2 -fsSL --max-filesize 1048576 -o {tmp_prefix}"
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
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("E_LIMIT_EXCEEDED"), "{}", stderr(&o));
    assert!(!fx.cached().exists());
    assert_eq!(fs::read_dir(fx.scripts()).unwrap().count(), 0);
}
