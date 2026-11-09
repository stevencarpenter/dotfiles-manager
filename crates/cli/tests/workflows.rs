//! The four required workflows (sync, update, try-adopt-purge, doctor) end to end through the
//! packaged binary in one isolated HOME (DFM-36). A public and a private bare remote are reached
//! as `file://` origins; `brew`, `mise`, `gitleaks`, `op` and the native tools are look-alikes.
//! No network, no real provider, vault or launcher.

mod common;

use common::providers::{FAKE_BREW, FAKE_GITLEAKS, FAKE_MISE, FAKE_OP, native_tool};
use common::remote::{Remote, git, lock_for, put};
use common::{TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Synthetic private data: it must never leave the private source and the HOME it renders to.
const MARKER: &str = "PRIVATE-MARKER-7f3a";

/// `extra` with a Homebrew package beside its links.
const EXTRA_PKG: &str = r#"
[[resources]]
id = "atuin"
kind = "package"
adapter = "homebrew/v1"
[resources.value]
name = "atuin"
update_policy = "rolling"
downgrade = "forbid"
"#;

/// The private module: a link to private bytes and a 1Password secret.
const WORK: &str = r#"schema = 1
id = "work"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "profile"
kind = "link"
adapter = "fs-link/v1"
target = ".config/work/profile"
[resources.value]
source = "files/profile"

[[resources]]
id = "token"
kind = "secret"
adapter = "op-template/v1"
target = ".config/work/token.env"
[resources.value]
template = "files/token.env.tpl"
account = "example-account"
mode = 0o600
"#;

/// A public bundle that names the private module in the nested checkout at `base/ext`.
const BUNDLE_WITH_EXT: &str = r#"schema = 1
id = "t-base"
core = "modules/core/module.toml"

[sources]
roots = ["base", "profiles"]
exclude = ["base/ext/.git"]

[modules]
extra = "modules/extra/module.toml"
work = "ext/modules/work/module.toml"
"#;

fn out(o: &Output) -> String {
    text(&o.stdout)
}

fn err(o: &Output) -> String {
    text(&o.stderr)
}

fn digest_of(o: &Output) -> String {
    out(o)
        .lines()
        .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no digest in\n{}\n{}", out(o), err(o)))
}

fn ok(o: &Output) {
    assert_eq!(code(o), 0, "{}\n{}", out(o), err(o));
}

fn exe(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

/// One machine: a HOME, a public and a private remote, the user's checkout of the public one,
/// and the look-alikes.
struct World {
    home: TempDir,
    _work: TempDir,
    fake: TempDir,
    public: Remote,
    private: Remote,
    /// The private commit the public lock pins at `base/ext`.
    pin: String,
    root: PathBuf,
    /// Every output of every command this world ran, for the private-data attestation.
    seen: std::cell::RefCell<String>,
}

impl World {
    /// Both remotes pushed, the public one cloned and bound. No origin is granted.
    fn new() -> World {
        let home = TempDir::new("wf-home");
        let work = TempDir::new("wf-work");
        let fake = TempDir::new("wf-fake");
        let private = Remote::new(work.path(), "private");
        put(&private.seed, "modules/work/module.toml", WORK);
        put(
            &private.seed,
            "modules/work/files/profile",
            &format!("work=1 {MARKER}\n"),
        );
        put(
            &private.seed,
            "modules/work/files/token.env.tpl",
            "TOKEN={{ op://vault/item/field }}\n",
        );
        let pin = private.push("private initial");
        let public = Remote::new(work.path(), "public");
        let seed = &public.seed;
        put(seed, "base/bundle.toml", BUNDLE_WITH_EXT);
        put(seed, "base/modules/core/module.toml", common::CORE);
        put(
            seed,
            "base/modules/extra/module.toml",
            &format!("{}{EXTRA_PKG}", common::EXTRA),
        );
        for (m, f, c) in [
            ("core", "gitconfig", "[user]\n"),
            ("core", "ssh_config", "Host *\n"),
            ("extra", "tmux.conf", "set -g mouse on\n"),
            ("extra", "zshrc", "export A=1\n"),
        ] {
            put(seed, &format!("base/modules/{m}/files/{f}"), c);
        }
        put(
            seed,
            "profiles/t-01.toml",
            &format!(
                "{}updaters = [\"claude\", \"codex\"]\n",
                common::PROFILE.replace("modules = [\"extra\"]", "modules = [\"extra\", \"work\"]")
            ),
        );
        put(seed, "Brewfile", "brew \"git\"\n");
        put(
            seed,
            "dotfiles-manager.lock",
            &lock_for(&[
                (&public.origin(), None, "."),
                (&private.origin(), Some(&pin), "base/ext"),
            ]),
        );
        public.push("public initial");
        git(
            work.path(),
            &["clone", "-q", public.bare.to_str().unwrap(), "root"],
        );
        let root = work.path().join("root");
        let bin = fake.path().join("bin");
        for (name, body) in [
            ("brew", FAKE_BREW),
            ("mise", FAKE_MISE),
            ("gitleaks", FAKE_GITLEAKS),
            ("op", FAKE_OP),
        ] {
            exe(&bin.join(name), body);
        }
        fs::write(fake.path().join("official-brew"), "atuin\ntuicr\n").unwrap();
        fs::write(fake.path().join("signed-in"), "").unwrap();
        let st = fake.path().join("st");
        fs::create_dir_all(&st).unwrap();
        exe(
            &home.path().join(".local/bin/claude"),
            &native_tool("claude", "update", &st),
        );
        exe(
            &home
                .path()
                .join(".codex/packages/standalone/current/bin/codex"),
            &native_tool("codex", "update", &st),
        );
        fs::write(st.join("claude"), "2.1.311 (Claude Code)\n").unwrap();
        fs::write(st.join("next-claude"), "2.1.312 (Claude Code)\n").unwrap();
        fs::write(st.join("codex"), "codex-cli 0.164.0\n").unwrap();
        let w = World {
            home,
            _work: work,
            fake,
            public,
            private,
            pin,
            root,
            seen: std::cell::RefCell::default(),
        };
        let p = w.root.join("profiles/t-01.toml");
        ok(&w.run(&[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-test",
        ]));
        w
    }

    /// [`World::new`] with both origins granted.
    fn granted() -> World {
        let w = World::new();
        w.trust(&w.public.origin());
        w.trust(&w.private.origin());
        w
    }

    fn trust(&self, origin: &str) {
        let o = self.sync(&["--trust-origin", origin]);
        assert!(
            out(&o).contains(&format!("granted: {origin} grant-")),
            "{}\n{}",
            out(&o),
            err(&o)
        );
    }

    fn bin(&self) -> PathBuf {
        self.fake.path().join("bin")
    }

    fn command(&self, env: &[(&str, &str)], args: &[&str]) -> Command {
        let mut c = Command::new(common::BIN);
        c.env_clear()
            .env(
                "DFM_PROVIDER_PATH",
                format!("{}:/usr/bin:/bin", self.bin().display()),
            )
            .env("DFM_SERVICE_TIMEOUT_SECS", "1")
            .env("USER", "leak-check")
            .envs(env.iter().copied())
            .arg("--home")
            .arg(self.home.path())
            .args(args);
        c
    }

    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let o = self.command(env, args).output().unwrap();
        let mut seen = self.seen.borrow_mut();
        seen.push_str(&out(&o));
        seen.push_str(&err(&o));
        o
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }

    /// `dfm sync --root <checkout>` with `flags`: no digest, the daily run.
    fn sync(&self, flags: &[&str]) -> Output {
        let mut a = vec!["sync", "--root", self.root_arg()];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    /// `args` with `--preview`, then with the previewed digest.
    fn approved(&self, args: &[&str]) -> Output {
        let mut p = args.to_vec();
        p.push("--preview");
        let o = self.run(&p);
        ok(&o);
        let d = digest_of(&o);
        let mut a = args.to_vec();
        a.extend_from_slice(&["--yes-plan", &d]);
        self.run(&a)
    }

    fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn state(&self) -> PathBuf {
        self.h(".local/state/dotfiles-manager/state")
    }

    /// What the provider look-alikes logged.
    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    fn ext_head(&self) -> String {
        git(&self.root.join("base/ext"), &["rev-parse", "HEAD"])
    }
}

/// Every regular file below `dir` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return hits;
    };
    for e in rd {
        let p = e.unwrap().path();
        let m = fs::symlink_metadata(&p).unwrap();
        if m.is_dir() {
            hits.extend(files_containing(&p, needle));
        } else if m.is_file()
            && fs::read(&p)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle.as_bytes())
        {
            hits.push(p);
        }
    }
    hits
}

#[test]
fn the_four_workflows_run_together_in_one_isolated_home() {
    let w = World::granted();
    // 1. The first sync publishes both sources and runs every scope and the updaters.
    let o = w.sync(&[]);
    ok(&o);
    let s = out(&o);
    for line in [
        "published: src0 ",
        "published: src1 ",
        "  packages: ok",
        "  secrets: ok",
        "  updated claude 2.1.311 (Claude Code) -> 2.1.312 (Claude Code)",
        "  current codex",
        "  upgrade: updated=1 current=1",
    ] {
        assert!(s.contains(line), "{line:?} missing in\n{s}");
    }
    assert!(s.ends_with("result: ok\n"), "{s}");
    for link in [
        ".zshrc",
        ".config/tmux/tmux.conf",
        ".config/git/config",
        ".ssh/config",
        ".config/work/profile",
    ] {
        assert!(w.h(link).is_symlink(), "{link} is not a link");
    }
    let token = w.h(".config/work/token.env");
    assert_eq!(fs::read_to_string(&token).unwrap(), "TOKEN=planted-value\n");
    assert_eq!(
        fs::metadata(&token).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(w.fake.path().join("Cellar/atuin").is_dir(), "{}", w.log());
    assert_eq!(w.ext_head(), w.pin);

    // 2. An upstream change arrives with a plain sync; the private pin holds.
    put(
        &w.public.seed,
        "base/modules/extra/files/zshrc",
        "export A=2\n",
    );
    let head = w.public.push("change zshrc");
    ok(&w.sync(&[]));
    assert_eq!(git(&w.root, &["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read_to_string(w.h(".zshrc")).unwrap(), "export A=2\n");
    assert_eq!(w.ext_head(), w.pin);

    // 3. Try tuicr, then adopt its configuration into `extra`.
    ok(&w.approved(&["trial", "tuicr", "--provider", "homebrew/v1"]));
    let live = w.h(".config/tuicr/config.toml");
    fs::create_dir_all(live.parent().unwrap()).unwrap();
    fs::write(&live, "theme = \"dark\"\n").unwrap();
    let o = w.approved(&["adopt", "trial-tuicr", "--module", "extra"]);
    ok(&o);
    assert!(out(&o).ends_with("result: ok\n"), "{}", out(&o));
    let repo_file = w
        .root
        .join("base/modules/extra/home/.config/tuicr/config.toml");
    assert_eq!(fs::read_link(&live).unwrap(), repo_file);
    let brewfile = fs::read_to_string(w.root.join("Brewfile")).unwrap();
    assert!(brewfile.ends_with("brew \"tuicr\"\n"), "{brewfile}");

    // 4. Try duckdb with mise, and roll it with `packages --update`.
    ok(&w.approved(&["trial", "duckdb", "--provider", "mise/v1"]));
    let o = w.run(&["packages", "--update", "--preview"]);
    ok(&o);
    assert!(
        out(&o).contains("upgrade duckdb with mise/v1"),
        "{}",
        out(&o)
    );
    let o = w.run(&["packages", "--update", "--yes-plan", &digest_of(&o)]);
    ok(&o);
    assert!(out(&o).contains("counts: updated=1 "), "{}", out(&o));

    // 5. Purge the duckdb trial.
    let o = w.approved(&["purge", "trial-duckdb"]);
    ok(&o);
    assert!(
        out(&o).contains("trial: state/trials/trial-duckdb.json removed"),
        "{}",
        out(&o)
    );
    assert!(
        w.log().contains("mise uninstall --all duckdb"),
        "{}",
        w.log()
    );

    // 6. A sync without the updaters keeps the adopted, uncommitted work.
    ok(&w.sync(&["--no-upgrade"]));
    let manifest = fs::read_to_string(w.root.join("base/modules/extra/module.toml")).unwrap();
    assert!(
        manifest.contains("id = \"config-tuicr-config-toml\""),
        "{manifest}"
    );
    let brewfile = fs::read_to_string(w.root.join("Brewfile")).unwrap();
    assert!(brewfile.ends_with("brew \"tuicr\"\n"), "{brewfile}");

    // 7. Doctor reports the result.
    let o = w.run(&["doctor", "--root", w.root_arg(), "--json"]);
    ok(&o);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    let row = |subject: &str| {
        rows.iter()
            .find(|r| r["subject"].as_str().unwrap().ends_with(subject))
            .unwrap_or_else(|| panic!("no row for {subject} in {rows:#?}"))
    };
    for r in rows {
        let st = r["status"].as_str().unwrap();
        assert!(st != "ERROR" && st != "UNKNOWN", "{r}");
        assert!(st != "WARN", "unexpected warning {r}");
    }
    assert_eq!(row(".config/tuicr/config.toml")["status"], "OK");
    assert_eq!(row(".config/work/profile")["status"], "OK");
    ok(&w.run(&["state", "check"]));

    // 8. The private bytes and the rendered secret never left the private source and HOME.
    let seen = w.seen.borrow().clone();
    for needle in [MARKER, "planted-value"] {
        assert!(!seen.contains(needle), "{needle} in command output");
        assert_eq!(files_containing(&w.state(), needle), Vec::<PathBuf>::new());
    }
    let o = Command::new("/usr/bin/git")
        .env_clear()
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(&w.public.bare)
        .args(["grep", "-q", MARKER, "main"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 1, "{}", err(&o));
}
