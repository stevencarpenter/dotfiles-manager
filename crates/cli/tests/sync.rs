//! `sync` through the isolated binary with temporary local git remotes (DFM-30, docs/sync.md).
//!
//! Every run passes `--home <temp dir>` and an empty environment. The remotes are bare
//! repositories in a temporary directory, reached as `file://` origins (the isolated build
//! accepts them). The user's checkout is a clone of the root remote. No network, no real
//! upstream.

mod common;

use common::{TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");

/// The module a nested source supplies, selected by the profile beside `extra`.
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
"#;

/// The bundle of a root that holds a nested checkout at `base/ext`: its `.git` is excluded.
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

fn stderr(o: &Output) -> String {
    text(&o.stderr)
}

fn stdout(o: &Output) -> String {
    text(&o.stdout)
}

fn digest_of(o: &Output) -> String {
    stdout(o)
        .lines()
        .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no digest in\n{}\n{}", stdout(o), stderr(o)))
}

/// Git for building fixtures: a bare environment, no user configuration, a fixed identity.
fn git(dir: &Path, args: &[&str]) -> String {
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
    assert!(
        o.status.success(),
        "git {args:?} in {}:\n{}",
        dir.display(),
        text(&o.stderr)
    );
    text(&o.stdout).trim_end().to_string()
}

fn put(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A bare remote, a seed working copy that pushes to it, and the id of its head.
struct Remote {
    bare: PathBuf,
    seed: PathBuf,
}

impl Remote {
    fn new(work: &Path, name: &str) -> Remote {
        let bare = work.join(name);
        let seed = work.join(format!("{name}-seed"));
        fs::create_dir_all(&bare).unwrap();
        fs::create_dir_all(&seed).unwrap();
        git(&bare, &["init", "-q", "--bare"]);
        git(&seed, &["init", "-q"]);
        git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        Remote { bare, seed }
    }

    fn origin(&self) -> String {
        format!("file://{}", self.bare.display())
    }

    /// Commits everything in the seed copy and pushes `main`. Returns the new head.
    fn push(&self, message: &str) -> String {
        git(&self.seed, &["add", "-A"]);
        git(&self.seed, &["commit", "-q", "-m", message]);
        git(
            &self.seed,
            &["push", "-q", "origin", "HEAD:refs/heads/main"],
        );
        git(&self.seed, &["rev-parse", "HEAD"])
    }

    fn head(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/main"])
    }

    /// Rewrites the head commit without changing its tree and force-pushes it: a new commit
    /// identity over the same content (a new message, since the same second would reproduce
    /// the same commit).
    fn amend(&self) -> String {
        git(
            &self.seed,
            &[
                "commit",
                "-q",
                "--amend",
                "-m",
                "amended: same tree, new commit",
            ],
        );
        git(
            &self.seed,
            &["push", "-q", "-f", "origin", "HEAD:refs/heads/main"],
        );
        git(&self.seed, &["rev-parse", "HEAD"])
    }
}

struct Fx {
    home: TempDir,
    work: TempDir,
    upstream: Remote,
    /// The user's checkout: a clone of `upstream`, the `--root` of every command.
    root: PathBuf,
}

impl Fx {
    /// A root remote with the link-only fixture, cloned, bound and with its origin granted.
    fn new() -> Fx {
        let fx = Fx::ungranted();
        let o = fx.run(&[
            "sync",
            "--root",
            fx.root_arg(),
            "--trust-origin",
            &fx.origin(),
        ]);
        assert_eq!(code(&o), 3, "{}", stderr(&o));
        assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
        fx
    }

    /// As [`Fx::new`] without the grant.
    fn ungranted() -> Fx {
        let home = TempDir::new("sync-home");
        let work = TempDir::new("sync-work");
        let upstream = Remote::new(work.path(), "upstream");
        let seed = &upstream.seed;
        put(seed, "base/bundle.toml", common::BUNDLE);
        put(seed, "base/modules/core/module.toml", common::CORE);
        put(seed, "base/modules/extra/module.toml", common::EXTRA);
        for (m, f, c) in [
            ("core", "gitconfig", "[user]\n"),
            ("core", "ssh_config", "Host *\n"),
            ("extra", "tmux.conf", "set -g mouse on\n"),
            ("extra", "zshrc", "export A=1\n"),
        ] {
            put(seed, &format!("base/modules/{m}/files/{f}"), c);
        }
        put(seed, "profiles/t-01.toml", common::PROFILE);
        put(
            seed,
            "dotfiles-manager.lock",
            &lock_for(&[(&upstream.origin(), None, ".")]),
        );
        upstream.push("initial");
        let root = work.path().join("root");
        git(
            work.path(),
            &["clone", "-q", upstream.bare.to_str().unwrap(), "root"],
        );
        let fx = Fx {
            home,
            work,
            upstream,
            root,
        };
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

    fn origin(&self) -> String {
        self.upstream.origin()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .env_clear()
            .env("DFM_PROVIDER_PATH", "/usr/bin:/bin")
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

    fn preview(&self) -> Output {
        self.run(&["sync", "--root", self.root_arg(), "--preview"])
    }

    fn sync(&self, digest: &str) -> Output {
        self.run(&["sync", "--root", self.root_arg(), "--yes-plan", digest])
    }

    /// `sync` without a digest: the daily run (DFM-76).
    fn plain(&self, flags: &[&str]) -> Output {
        let mut a = vec!["sync", "--root", self.root_arg()];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    /// Preview, then sync with the previewed digest. Returns the sync output.
    fn sync_approved(&self) -> Output {
        let p = self.preview();
        assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
        self.sync(&digest_of(&p))
    }

    fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }

    fn staging(&self) -> PathBuf {
        self.h(".cache/dotfiles-manager/staging")
    }

    /// Staging directories left behind by a run. Every run removes its own.
    fn staging_leftovers(&self) -> Vec<String> {
        match fs::read_dir(self.staging()) {
            Ok(d) => d
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn receipts(&self) -> usize {
        fs::read_dir(self.h(".local/state/dotfiles-manager/state/receipts"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    /// Every receipt, oldest first.
    fn receipt_docs(&self) -> Vec<serde_json::Value> {
        let Ok(dir) = fs::read_dir(self.h(".local/state/dotfiles-manager/state/receipts")) else {
            return Vec::new();
        };
        let mut v: Vec<serde_json::Value> = dir
            .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect();
        v.sort_by_key(|r| r["started_at"].as_str().unwrap().to_string());
        v
    }

    fn grants(&self) -> String {
        fs::read_to_string(self.h(".local/state/dotfiles-manager/state/grants.json"))
            .unwrap_or_default()
    }

    fn journal(&self) -> String {
        fs::read_to_string(self.h(".local/state/dotfiles-manager/state/journal.jsonl"))
            .unwrap_or_default()
    }

    /// Adds a nested remote `ext` with the `work` module, pins it in the lock at its head, and
    /// makes the bundle exclude its `.git`. Returns the remote and the pinned commit.
    fn add_ext(&self) -> (Remote, String) {
        let ext = Remote::new(self.work.path(), "ext");
        put(&ext.seed, "modules/work/module.toml", WORK);
        put(&ext.seed, "modules/work/files/profile", "work=1\n");
        let pin = ext.push("ext initial");
        let seed = &self.upstream.seed;
        put(seed, "base/bundle.toml", BUNDLE_WITH_EXT);
        put(
            seed,
            "profiles/t-01.toml",
            &common::PROFILE.replace("modules = [\"extra\"]", "modules = [\"extra\", \"work\"]"),
        );
        put(
            seed,
            "dotfiles-manager.lock",
            &lock_for(&[
                (&self.origin(), None, "."),
                (&ext.origin(), Some(&pin), "base/ext"),
            ]),
        );
        self.upstream.push("add the ext source");
        (ext, pin)
    }

    fn trust(&self, origin: &str) {
        let o = self.run(&["sync", "--root", self.root_arg(), "--trust-origin", origin]);
        assert!(
            stdout(&o).contains(&format!("granted: {origin} grant-")),
            "{}\n{}",
            stdout(&o),
            stderr(&o)
        );
    }
}

fn lock_for(sources: &[(&str, Option<&str>, &str)]) -> String {
    let mut s = String::from("[manager]\ntoolchain = \"1.98.1\"\n");
    for (i, (origin, commit, path)) in sources.iter().enumerate() {
        s.push_str(&format!(
            "\n[[sources]]\nid = \"src{i}\"\nkind = \"git\"\norigin = \"{origin}\"\nref = \"refs/heads/main\"\npath = \"{path}\"\n"
        ));
        if let Some(c) = commit {
            s.push_str(&format!("commit = \"{c}\"\n"));
        }
    }
    s
}

#[test]
fn an_untrusted_origin_is_refused_before_any_fetch_and_a_grant_is_explicit() {
    let fx = Fx::ungranted();
    // The daily run without a digest refuses the same way and names the command to run.
    for o in [fx.preview(), fx.plain(&[])] {
        assert_eq!(code(&o), 3, "{}", stderr(&o));
        assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
        assert!(stderr(&o).contains("nothing was fetched"), "{}", stderr(&o));
        assert!(
            stderr(&o).contains(&format!("run sync --trust-origin {}", fx.origin())),
            "{}",
            stderr(&o)
        );
    }
    assert!(!fx.staging().exists(), "a staging directory was created");
    assert!(fx.grants().is_empty());
    // --preview cannot grant, and --yes-plan cannot grant.
    let o = fx.run(&[
        "sync",
        "--root",
        fx.root_arg(),
        "--preview",
        "--trust-origin",
        &fx.origin(),
    ]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    let o = fx.run(&[
        "sync",
        "--root",
        fx.root_arg(),
        "--yes-plan",
        &"0".repeat(64),
    ]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    let o = fx.run(&[
        "sync",
        "--root",
        fx.root_arg(),
        "--yes-plan",
        &"0".repeat(64),
        "--trust-origin",
        &fx.origin(),
    ]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(fx.grants().is_empty());
    // An origin that is not canonical cannot be granted.
    let o = fx.run(&[
        "sync",
        "--root",
        fx.root_arg(),
        "--trust-origin",
        "https://Example.invalid/other/repo.git",
    ]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("not canonical"), "{}", stderr(&o));
    assert!(fx.grants().is_empty());
    // The explicit grant is written under the lock and journaled (intent, then done), then the
    // fetch and the plan happen.
    fx.trust(&fx.origin());
    assert!(fx.grants().contains(&fx.origin()), "{}", fx.grants());
    let j = fx.journal();
    assert_eq!(j.matches("\"op\":\"grant-origin\"").count(), 2, "{j}");
    assert!(
        j.contains("\"phase\":\"intent\"") && j.contains("\"phase\":\"done\""),
        "{j}"
    );
    assert!(j.contains(&fx.origin()), "{j}");
    // Granting again writes nothing more.
    let o = fx.run(&[
        "sync",
        "--root",
        fx.root_arg(),
        "--trust-origin",
        &fx.origin(),
    ]);
    assert!(stdout(&o).contains("(already)"), "{}", stdout(&o));
    assert_eq!(fx.journal(), j);
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains(&format!(
            "source: src0 {} refs/heads/main current at",
            fx.origin()
        )),
        "{}",
        stdout(&o)
    );
    assert!(
        fx.staging_leftovers().is_empty(),
        "{:?}",
        fx.staging_leftovers()
    );
}

#[test]
fn sync_without_root_uses_the_checkout_bind_recorded() {
    // Fx binds without --root, so the binding records the clone, which holds .git.
    let fx = Fx::new();
    let p = fx.run(&["sync", "--preview"]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    assert_eq!(digest_of(&p), digest_of(&fx.preview()));
    let o = fx.run(&["sync", "--yes-plan", &digest_of(&p)]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("result: ok"), "{}", stdout(&o));
    assert!(fx.h(".zshrc").is_symlink());
}

#[test]
fn already_linked_sources_apply_once_and_a_repeat_sync_is_a_no_op() {
    let fx = Fx::new();
    let head = fx.upstream.head();
    assert_eq!(fx.head(), head);
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("published: src0 current at"), "{s}");
    assert!(s.contains("result: ok"), "{s}");
    assert!(fx.h(".zshrc").is_symlink());
    assert!(fx.h(".config/git/config").is_symlink());
    assert_eq!(fx.head(), head, "the checkout moved");
    assert_eq!(fx.receipts(), 1);
    // Nothing new upstream, nothing new in HOME: the second run changes nothing.
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains("published: src0 current at"),
        "{}",
        stdout(&o)
    );
    assert_eq!(fx.receipts(), 1, "a no-op sync wrote a receipt");
    assert_eq!(fx.head(), head);
    assert!(
        fx.staging_leftovers().is_empty(),
        "{:?}",
        fx.staging_leftovers()
    );
}

#[test]
fn a_sync_that_changes_nothing_needs_no_approval() {
    let fx = Fx::new();
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // Sources current and every operation current: no digest is asked for.
    let o = fx.run(&["sync", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("summary: current=4; nothing to change"), "{s}");
    assert!(s.contains("nothing to change: no approval needed"), "{s}");
    assert!(s.ends_with("result: ok\n"), "{s}");
    assert_eq!(fx.receipts(), 1, "a no-op sync wrote a receipt");
    // A new upstream commit is a publication: a plain sync publishes it, and no scope has
    // anything to do, so no receipt is written.
    let seed = &fx.upstream.seed;
    put(seed, "README.md", "notes\n");
    let head = fx.upstream.push("docs only");
    let o = fx.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(!s.contains("no approval needed"), "{s}");
    in_order(&s, &["published: src0 ", "  config: current", "result: ok"]);
    assert_eq!(fx.head(), head);
    assert_eq!(fx.receipts(), 1);
}

#[test]
fn an_advanced_upstream_branch_fast_forwards_the_checkout_and_applies_it() {
    let fx = Fx::new();
    let old = fx.head();
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // Local work that the update does not touch: a dirty tracked file and an untracked file.
    fs::write(
        fx.root.join("base/modules/core/files/ssh_config"),
        "Host me\n",
    )
    .unwrap();
    fs::write(fx.root.join("notes.txt"), "mine\n").unwrap();
    // Upstream adds a resource and changes a file.
    let seed = &fx.upstream.seed;
    put(
        seed,
        "base/modules/extra/module.toml",
        &format!(
            "{}\n[[resources]]\nid = \"alacritty\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/alacritty/alacritty.toml\"\n[resources.value]\nsource = \"files/alacritty.toml\"\n",
            common::EXTRA
        ),
    );
    put(seed, "base/modules/extra/files/alacritty.toml", "[font]\n");
    put(seed, "base/modules/extra/files/zshrc", "export A=2\n");
    let new = fx.upstream.push("add alacritty");
    let p = fx.preview();
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    assert!(
        s.contains(&format!(
            "source: src0 {} refs/heads/main {old}..{new} (tracking)",
            fx.origin()
        )),
        "{s}"
    );
    assert!(
        s.contains("planned extra.alacritty@.config/alacritty/alacritty.toml"),
        "{s}"
    );
    assert_eq!(fx.head(), old, "a preview moved the checkout");
    let o = fx.sync(&digest_of(&p));
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains(&format!("published: src0 {old}..{new}")), "{s}");
    assert!(s.contains("result: ok"), "{s}");
    assert_eq!(fx.head(), new);
    assert_eq!(
        git(&fx.root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
    assert!(fx.h(".config/alacritty/alacritty.toml").is_symlink());
    assert_eq!(fs::read_to_string(fx.h(".zshrc")).unwrap(), "export A=2\n");
    // The local work survived.
    assert_eq!(
        fs::read_to_string(fx.root.join("base/modules/core/files/ssh_config")).unwrap(),
        "Host me\n"
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("notes.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(fx.receipts(), 2);
}

#[test]
fn local_commits_that_upstream_lacks_are_refused_and_kept() {
    let fx = Fx::new();
    fs::write(fx.root.join("local.txt"), "local\n").unwrap();
    git(&fx.root, &["add", "local.txt"]);
    git(&fx.root, &["commit", "-q", "-m", "local work"]);
    let mine = fx.head();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    fx.upstream.push("upstream work");
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("never resets, rebases"),
        "{}",
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("Not possible to fast-forward"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fx.head(), mine);
    assert!(fx.root.join("local.txt").exists());
    assert!(
        !fx.h(".zshrc").exists(),
        "a refused sync applied configuration"
    );
}

#[test]
fn a_checkout_ahead_of_its_ref_is_current_and_applies_as_it_is() {
    let fx = Fx::new();
    let upstream = fx.upstream.head();
    // Local work on a branch, one commit on top of the ref, and nothing new upstream.
    git(&fx.root, &["checkout", "-q", "-b", "topic"]);
    put(
        &fx.root,
        "base/modules/extra/files/zshrc",
        "export A=local\n",
    );
    git(&fx.root, &["commit", "-q", "-am", "local work"]);
    let mine = fx.head();
    let p = fx.preview();
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let current = format!("current at {mine} (ahead of {upstream})");
    assert!(
        stdout(&p).contains(&format!(
            "source: src0 {} refs/heads/main {current} (tracking)",
            fx.origin()
        )),
        "{}",
        stdout(&p)
    );
    let o = fx.sync(&digest_of(&p));
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains(&format!("published: src0 {current}")), "{s}");
    assert!(s.contains("result: ok"), "{s}");
    assert_eq!(fx.head(), mine);
    assert_eq!(
        git(&fx.root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "topic"
    );
    assert!(fx.h(".zshrc").is_symlink());
    assert_eq!(
        fs::read_to_string(fx.h(".zshrc")).unwrap(),
        "export A=local\n"
    );
}

#[test]
fn a_local_edit_the_update_would_overwrite_is_refused_and_kept() {
    let fx = Fx::new();
    let old = fx.head();
    fs::write(
        fx.root.join("base/modules/extra/files/zshrc"),
        "export MINE=1\n",
    )
    .unwrap();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    fx.upstream.push("upstream zshrc");
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("base/modules/extra/files/zshrc"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fx.head(), old);
    assert_eq!(
        fs::read_to_string(fx.root.join("base/modules/extra/files/zshrc")).unwrap(),
        "export MINE=1\n"
    );
    // An untracked file the update would create is refused too; nothing is cleaned.
    fs::write(
        fx.root.join("base/modules/extra/files/zshrc"),
        "export A=1\n",
    )
    .unwrap();
    fs::write(fx.root.join("base/modules/extra/files/new.conf"), "mine\n").unwrap();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/new.conf",
        "theirs\n",
    );
    fx.upstream.push("upstream new.conf");
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(
        fs::read_to_string(fx.root.join("base/modules/extra/files/new.conf")).unwrap(),
        "mine\n"
    );
    assert_eq!(fx.head(), old);
}

#[test]
fn a_plain_sync_publishes_and_applies_without_a_digest() {
    let fx = Fx::new();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    let head = fx.upstream.push("upstream zshrc");
    let o = fx.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "plan digest: ",
            "published: src0 ",
            "  config: ok",
            "result: ok\n",
        ],
    );
    assert!(!s.contains("warning:"), "{s}");
    assert_eq!(fx.head(), head);
    assert!(fx.h(".zshrc").is_symlink());
    let r = fx.receipt_docs();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["approval"]["kind"], "unattended", "{}", r[0]);
    assert!(
        fx.staging_leftovers().is_empty(),
        "{:?}",
        fx.staging_leftovers()
    );
}

#[test]
fn a_deselected_module_s_owned_outputs_are_removed_without_a_flag() {
    let fx = Fx::new();
    let o = fx.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(fx.h(".zshrc").is_symlink());
    put(
        &fx.upstream.seed,
        "profiles/t-01.toml",
        &common::PROFILE.replace("modules = [\"extra\"]", "modules = []"),
    );
    fx.upstream.push("drop extra");
    let o = fx.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("  config: ok"), "{s}");
    for gone in [".zshrc", ".config/tmux/tmux.conf"] {
        assert!(fs::symlink_metadata(fx.h(gone)).is_err(), "{gone} stayed");
    }
    assert!(fx.h(".config/git/config").is_symlink());
    // The removal ran under the run's own approval: no per-operation flag is recorded.
    let r = fx.receipt_docs();
    let last = r.last().unwrap();
    assert_eq!(last["approval"]["kind"], "unattended", "{last}");
    assert!(
        last["approvals"].as_array().is_none_or(Vec::is_empty),
        "{last}"
    );
}

#[test]
fn an_unowned_link_in_the_way_is_not_adopted_and_the_warning_names_its_flag() {
    let fx = Fx::new();
    std::os::unix::fs::symlink(
        fx.root.join("base/modules/extra/files/zshrc"),
        fx.h(".zshrc"),
    )
    .unwrap();
    let warning = "warning: config held back: extra.zsh@.zshrc. Fix: review each, then run dfm sync --allow-adopt 'extra.zsh@.zshrc'";
    let o = fx.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "planned extra.zsh@.zshrc",
            "held back: extra.zsh@.zshrc (needs --allow-adopt)",
            "plan digest: ",
            "  config: ok",
            "result: ok\n",
        ],
    );
    assert!(s.ends_with(&format!("result: ok\n{warning}\n")), "{s}");
    // The rest ran; the adoption did not, so the ledger still does not own the link.
    assert!(fx.h(".config/tmux/tmux.conf").is_symlink());
    let r = fx.receipt_docs();
    assert_eq!(r.len(), 1);
    let ops = r[0]["operations"].as_array().unwrap();
    assert!(
        ops.iter().all(|op| op["id"] != "extra.zsh@.zshrc"),
        "{}",
        r[0]
    );
    // The next plain run has only the held adoption: it changes nothing and warns again.
    let o = fx.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("nothing to change: no approval needed"), "{s}");
    assert!(s.contains("  config: held back (extra.zsh@.zshrc)"), "{s}");
    assert!(s.ends_with(&format!("result: ok\n{warning}\n")), "{s}");
    assert_eq!(fx.receipts(), 1);
    // The flag the warning names adopts it.
    let o = fx.plain(&["--allow-adopt", "extra.zsh@.zshrc"]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(!stdout(&o).contains("warning:"), "{}", stdout(&o));
    let r = fx.receipt_docs();
    let last = r.last().unwrap();
    assert_eq!(last["approvals"][0]["how"], "allow_adopt", "{last}");
    let o = fx.plain(&[]);
    assert!(stdout(&o).contains("  config: current"), "{}", stdout(&o));
    assert!(!stdout(&o).contains("warning:"), "{}", stdout(&o));
}

#[test]
fn a_change_between_the_printed_plan_and_the_locked_plan_is_stale() {
    let fx = Fx::new();
    let old = fx.head();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    fx.upstream.push("first");
    let release = fx.work.path().join("release-lock");
    // The run prints its plan, then parks before it takes the writer lock.
    let parked = Command::new(BIN)
        .env_clear()
        .env("DFM_PROVIDER_PATH", "/usr/bin:/bin")
        .env(
            "DFM_FAILPOINT",
            format!("sync-before-lock:wait-{}", release.display()),
        )
        .args([
            "--home",
            fx.home.path().to_str().unwrap(),
            "sync",
            "--root",
            fx.root_arg(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reached = release.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < end,
            "the failpoint was not reached"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=4\n",
    );
    fx.upstream.push("second");
    fs::write(&release, "go").unwrap();
    let o = parked.wait_with_output().unwrap();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("the plan changed after it was printed"),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("run dfm sync again"), "{}", stderr(&o));
    assert_eq!(fx.head(), old);
    assert!(!fx.h(".zshrc").exists());
    assert_eq!(fx.receipts(), 0);
}

#[test]
fn a_change_between_preview_and_approval_is_stale_and_publishes_nothing() {
    let fx = Fx::new();
    let old = fx.head();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    fx.upstream.push("first");
    let p = fx.preview();
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=4\n",
    );
    fx.upstream.push("second");
    let o = fx.sync(&digest_of(&p));
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(fx.head(), old);
    assert!(!fx.h(".zshrc").exists());
    assert_eq!(fx.receipts(), 0);
    // The checkout moving under the plan is stale too.
    let p = fx.preview();
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    fs::write(fx.root.join("local.txt"), "local\n").unwrap();
    git(&fx.root, &["add", "local.txt"]);
    git(&fx.root, &["commit", "-q", "-m", "local work"]);
    let o = fx.sync(&digest_of(&p));
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(fx.root.join("local.txt").exists());
}

#[test]
fn a_fetch_that_cannot_read_its_remote_blocks_before_anything_is_published() {
    if common::is_root() {
        common::skip("root can read an unreadable remote");
        return;
    }
    let fx = Fx::new();
    let old = fx.head();
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    fs::set_permissions(&fx.upstream.bare, fs::Permissions::from_mode(0o000)).unwrap();
    let o = fx.preview();
    fs::set_permissions(&fx.upstream.bare, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_MISSING"), "{}", stderr(&o));
    // No fallback to the checkout's own content: no plan, no digest.
    assert!(!stdout(&o).contains("plan digest"), "{}", stdout(&o));
    assert_eq!(fx.head(), old);
    assert!(
        fx.staging_leftovers().is_empty(),
        "{:?}",
        fx.staging_leftovers()
    );
}

#[test]
fn an_exact_nested_pin_stays_exact_while_the_root_tracks() {
    let fx = Fx::new();
    let (ext, pin) = fx.add_ext();
    let root_new = fx.upstream.head();
    // The nested origin needs its own grant; the root's grant does not cover it.
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert!(stderr(&o).contains(&ext.origin()), "{}", stderr(&o));
    fx.trust(&ext.origin());
    // ext advances upstream after the pin was taken.
    put(&ext.seed, "modules/work/files/profile", "work=2\n");
    let ext_new = ext.push("ext advances");
    assert_ne!(ext_new, pin);
    let lock_before = fs::read(fx.upstream.seed.join("dotfiles-manager.lock")).unwrap();
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(
        s.contains(&format!("published: src1 new checkout at {pin}")),
        "{s}"
    );
    assert_eq!(fx.head(), root_new);
    let nested = fx.root.join("base/ext");
    assert_eq!(
        git(&nested, &["rev-parse", "HEAD"]),
        pin,
        "the pin was not kept"
    );
    assert_eq!(
        fs::read_to_string(fx.h(".config/work/profile")).unwrap(),
        "work=1\n"
    );
    assert_eq!(
        fs::read(fx.root.join("dotfiles-manager.lock")).unwrap(),
        lock_before
    );
    // A repeat sync keeps the pin and changes nothing.
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains(&format!("published: src1 current at {pin}")),
        "{}",
        stdout(&o)
    );
    assert_eq!(git(&nested, &["rev-parse", "HEAD"]), pin);
    assert_eq!(fx.receipts(), 1);
    // A local commit on top of the pin is kept: the source is current at it, not moved back.
    put(&nested, "modules/work/files/profile", "work=local\n");
    git(&nested, &["commit", "-q", "-am", "local work"]);
    let mine = git(&nested, &["rev-parse", "HEAD"]);
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains(&format!(
            "published: src1 current at {mine} (ahead of {pin})"
        )),
        "{}",
        stdout(&o)
    );
    assert_eq!(git(&nested, &["rev-parse", "HEAD"]), mine);
    assert_eq!(
        fs::read_to_string(fx.h(".config/work/profile")).unwrap(),
        "work=local\n"
    );
}

#[test]
fn a_pin_the_fetched_ref_does_not_hold_is_refused_without_substitution() {
    let fx = Fx::new();
    let (ext, _pin) = fx.add_ext();
    fx.trust(&ext.origin());
    let old = fx.head();
    // Point the pin at a commit that is not on ext's main.
    let orphan = "0123456789abcdef0123456789abcdef01234567";
    put(
        &fx.upstream.seed,
        "dotfiles-manager.lock",
        &lock_for(&[
            (&fx.origin(), None, "."),
            (&ext.origin(), Some(orphan), "base/ext"),
        ]),
    );
    fx.upstream.push("pin a missing commit");
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_MISSING"), "{}", stderr(&o));
    assert!(stderr(&o).contains(orphan), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("the head is not used in its place"),
        "{}",
        stderr(&o)
    );
    assert!(!stdout(&o).contains("plan digest"), "{}", stdout(&o));
    assert_eq!(
        fx.head(),
        old,
        "the root was published without its nested source"
    );
    assert!(!fx.root.join("base/ext").exists());
    assert!(!fx.h(".config/work/profile").exists());
}

#[test]
fn an_invalid_nested_module_blocks_publication_of_every_source() {
    let fx = Fx::new();
    let (ext, _pin) = fx.add_ext();
    fx.trust(&ext.origin());
    let old = fx.head();
    put(
        &ext.seed,
        "modules/work/module.toml",
        "schema = 1\nid = \"work\"\n",
    );
    let broken = ext.push("break the module");
    put(
        &fx.upstream.seed,
        "dotfiles-manager.lock",
        &lock_for(&[
            (&fx.origin(), None, "."),
            (&ext.origin(), Some(&broken), "base/ext"),
        ]),
    );
    fx.upstream.push("pin the broken module");
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
    assert_eq!(fx.head(), old);
    assert!(!fx.root.join("base/ext").exists());
    // The same with --yes-plan: nothing is published either.
    let o = fx.sync(&"0".repeat(64));
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert_eq!(fx.head(), old);
    assert_eq!(fx.receipts(), 0);
}

#[test]
fn read_only_commands_never_fetch_and_never_move_the_checkout() {
    let fx = Fx::new();
    let old = fx.head();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    fx.upstream.push("upstream zshrc");
    for args in [
        vec!["plan", "--root", fx.root_arg()],
        vec!["doctor", "--root", fx.root_arg()],
        vec!["status", "--root", fx.root_arg()],
        vec!["explain", "extra.zsh", "--root", fx.root_arg()],
        vec!["state", "check"],
    ] {
        let o = fx.run(&args);
        assert_ne!(code(&o), 2, "{args:?}: {}", stderr(&o));
        assert!(!stdout(&o).contains("source: src0"), "{args:?} fetched");
    }
    assert_eq!(fx.head(), old);
    assert!(
        fx.staging_leftovers().is_empty(),
        "{:?}",
        fx.staging_leftovers()
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("base/modules/extra/files/zshrc")).unwrap(),
        "export A=1\n"
    );
}

#[test]
fn a_lock_without_sources_or_a_root_that_is_no_checkout_is_refused() {
    let fx = Fx::new();
    fs::write(
        fx.root.join("dotfiles-manager.lock"),
        "[manager]\ntoolchain = \"1.98.1\"\n",
    )
    .unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("declares no [[sources]]"),
        "{}",
        stderr(&o)
    );
    git(&fx.root, &["checkout", "-q", "--", "dotfiles-manager.lock"]);
    // A nested source path cannot be a pin of the root, and the origin must be canonical.
    fs::write(
        fx.root.join("dotfiles-manager.lock"),
        lock_for(&[(&fx.origin(), Some(&"a".repeat(40)), ".")]),
    )
    .unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
    fs::write(
        fx.root.join("dotfiles-manager.lock"),
        lock_for(&[("https://Example.invalid/org/repo.git", None, ".")]),
    )
    .unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert!(stderr(&o).contains("not canonical"), "{}", stderr(&o));
}

#[test]
fn a_nested_path_through_a_symlink_is_refused_before_any_checkout_is_created() {
    let fx = Fx::new();
    let (ext, _pin) = fx.add_ext();
    fx.trust(&ext.origin());
    let old = fx.head();
    // `base/ext` would land through a link that leaves the checkout.
    let outside = TempDir::new("sync-outside");
    std::os::unix::fs::symlink(outside.path(), fx.root.join("base/ext")).unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_ESCAPE"), "{}", stderr(&o));
    assert!(stderr(&o).contains("symlink"), "{}", stderr(&o));
    assert_eq!(fx.head(), old);
    assert!(
        fs::read_dir(outside.path()).unwrap().next().is_none(),
        "something was written outside the checkout"
    );
    assert!(
        fx.root.join("base/ext").is_symlink(),
        "the link was replaced"
    );
}

#[test]
fn a_worktree_checkout_or_a_redirected_work_tree_is_refused() {
    let fx = Fx::new();
    let old = fx.head();
    // core.worktree pointing elsewhere would send the fast-forward there.
    let elsewhere = TempDir::new("sync-elsewhere");
    git(
        &fx.root,
        &[
            "config",
            "core.worktree",
            elsewhere.path().to_str().unwrap(),
        ],
    );
    let o = fx.preview();
    git(&fx.root, &["config", "--unset", "core.worktree"]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert!(stderr(&o).contains("plain checkout"), "{}", stderr(&o));
    assert!(fs::read_dir(elsewhere.path()).unwrap().next().is_none());
    assert_eq!(fx.head(), old);
    // A linked worktree keeps a gitfile whose git directory lies in the main checkout. Bind a
    // fresh machine to its profile so the refusal is the checkout's, not the binding's.
    let wt = fx.work.path().join("wt");
    git(
        &fx.root,
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    let other = TempDir::new("sync-home2");
    let p = wt.join("profiles/t-01.toml");
    let o = Command::new(BIN)
        .env_clear()
        .env("DFM_PROVIDER_PATH", "/usr/bin:/bin")
        .args([
            "--home",
            other.path().to_str().unwrap(),
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-test",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = Command::new(BIN)
        .env_clear()
        .env("DFM_PROVIDER_PATH", "/usr/bin:/bin")
        .args([
            "--home",
            other.path().to_str().unwrap(),
            "sync",
            "--root",
            wt.to_str().unwrap(),
            "--trust-origin",
            &fx.origin(),
        ])
        .output()
        .unwrap();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert!(stderr(&o).contains("plain checkout"), "{}", stderr(&o));
}

#[test]
fn a_new_commit_over_the_same_tree_between_preview_and_approval_is_stale() {
    let fx = Fx::new();
    let old = fx.head();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    fx.upstream.push("first");
    let p = fx.preview();
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    // Same tree, new commit: the plan is unchanged and the approval still does not cover it.
    let shown = stdout(&p)
        .lines()
        .find(|l| l.starts_with("source: src0 "))
        .and_then(|l| l.split("..").nth(1))
        .map(|s| s.split(' ').next().unwrap().to_string())
        .unwrap();
    let amended = fx.upstream.amend();
    assert_ne!(amended, shown, "the amend reproduced the commit");
    let o = fx.sync(&digest_of(&p));
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert_eq!(fx.head(), old);
    assert_ne!(fx.head(), amended);
    assert!(!fx.h(".zshrc").exists());
    // A fresh preview carries the new commit and syncs.
    let o = fx.sync_approved();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert_eq!(fx.head(), amended);
}

/// Runs `f` while the bare remote is moved away, so any fetch that reaches it fails with
/// `E_SOURCE_MISSING` instead of the refusal under test.
fn without_remote(fx: &Fx, f: impl FnOnce() -> Output) -> Output {
    let away = fx.work.path().join("upstream-away");
    fs::rename(&fx.upstream.bare, &away).unwrap();
    let o = f();
    fs::rename(&away, &fx.upstream.bare).unwrap();
    o
}

#[test]
fn a_graft_or_shallow_file_in_the_checkout_is_refused_before_any_fetch() {
    let fx = Fx::new();
    let old = fx.head();
    for file in ["info/grafts", "shallow"] {
        let path = fx.root.join(".git").join(file);
        fs::write(&path, format!("{old} {old}\n")).unwrap();
        let o = without_remote(&fx, || fx.preview());
        fs::remove_file(&path).unwrap();
        assert_eq!(code(&o), 3, "{file}: {}\n{}", stdout(&o), stderr(&o));
        let e = stderr(&o);
        assert!(e.contains("E_SOURCE_UNTRUSTED"), "{file}: {e}");
        assert!(e.contains(&format!(".git/{file}")), "{file}: {e}");
        assert!(!e.contains("E_SOURCE_MISSING"), "{file}: a fetch ran: {e}");
        assert!(!stdout(&o).contains("source: src0"), "{file}: a fetch ran");
        assert_eq!(fx.head(), old, "{file}");
        assert!(fx.staging_leftovers().is_empty(), "{file}");
    }
}

#[test]
fn a_checkout_configuration_outside_the_allowlist_is_refused_before_any_fetch() {
    let fx = Fx::new();
    let old = fx.head();
    let included = fx.work.path().join("included.gitconfig");
    fs::write(&included, "[core]\n\tfsmonitor = /bin/true\n").unwrap();
    for (key, value, printed) in [
        (
            "url.https://u:s3cret@evil.invalid/.insteadOf",
            fx.origin(),
            "url.*.insteadof",
        ),
        (
            "credential.helper",
            "!echo s3cret".to_string(),
            "credential.helper",
        ),
        (
            "core.sshCommand",
            "/bin/echo s3cret".to_string(),
            "core.sshcommand",
        ),
        (
            "include.path",
            included.to_str().unwrap().to_string(),
            "include.path",
        ),
        (
            "branch.main.mergeOptions",
            "--no-ff".to_string(),
            "branch.*.mergeoptions",
        ),
        // A remote section named after the origin URL: git matches the fetch argument against
        // it and would fetch from its url instead.
        (
            &format!("remote.{}.url", fx.origin()),
            "https://u:s3cret@evil.invalid/decoy".to_string(),
            "remote.*.url",
        ),
    ] {
        git(&fx.root, &["config", key, &value]);
        let o = without_remote(&fx, || fx.preview());
        git(&fx.root, &["config", "--unset", key]);
        assert_eq!(code(&o), 3, "{key}: {}\n{}", stdout(&o), stderr(&o));
        let e = stderr(&o);
        assert!(e.contains("E_SOURCE_UNTRUSTED"), "{key}: {e}");
        assert!(e.contains(printed), "{key}: {e}");
        assert!(!e.contains("s3cret"), "{key}: a value was printed: {e}");
        assert!(!e.contains("E_SOURCE_MISSING"), "{key}: a fetch ran: {e}");
        assert!(!stdout(&o).contains("source: src0"), "{key}: a fetch ran");
        assert_eq!(fx.head(), old, "{key}");
        assert!(
            fx.staging_leftovers().is_empty(),
            "{key}: {:?}",
            fx.staging_leftovers()
        );
    }
    // The configuration a clone writes (core.*, remote.origin.*, branch.main.*) passes, with a
    // second remote and the keys editor and tool integrations leave in a checkout.
    for (key, value) in [
        ("remote.second.url", "/nowhere"),
        ("remote.second.fetch", "+refs/heads/*:refs/remotes/second/*"),
        ("branch.main.vscode-merge-base", "origin/main"),
        ("coderabbit.defaultBranch", "main"),
        ("worktrunk.default-branch", "main"),
        ("worktrunk.hints.shell-integration", "false"),
        ("worktrunk.history", "1"),
    ] {
        git(&fx.root, &["config", key, value]);
    }
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
}

#[test]
fn two_grants_that_race_for_the_writer_lock_both_survive() {
    let fx = Fx::ungranted();
    let release = fx.work.path().join("release-grant");
    // The first run reads grants.json (empty), then parks before it takes the writer lock.
    let parked = Command::new(BIN)
        .env_clear()
        .env("DFM_PROVIDER_PATH", "/usr/bin:/bin")
        .env(
            "DFM_FAILPOINT",
            format!("sync-before-grant-lock:wait-{}", release.display()),
        )
        .args([
            "--home",
            fx.home.path().to_str().unwrap(),
            "sync",
            "--root",
            fx.root_arg(),
            "--trust-origin",
            &fx.origin(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reached = release.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < end,
            "the failpoint was not reached"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // A second run grants another origin meanwhile and writes grants.json.
    let other = "https://example.invalid/other/repo";
    let o = fx.run(&["sync", "--root", fx.root_arg(), "--trust-origin", other]);
    assert!(
        stdout(&o).contains(&format!("granted: {other} grant-")),
        "{}\n{}",
        stdout(&o),
        stderr(&o)
    );
    fs::write(&release, "go").unwrap();
    let o = parked.wait_with_output().unwrap();
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
    let g = fx.grants();
    assert!(g.contains(other), "the first grant was dropped: {g}");
    assert!(g.contains(&fx.origin()), "the second grant is missing: {g}");
    assert_eq!(
        fx.journal().matches("\"op\":\"grant-origin\"").count(),
        4,
        "{}",
        fx.journal()
    );
}

#[test]
fn a_dead_run_s_staging_directory_is_removed_by_the_next_writer() {
    let fx = Fx::new();
    let dead = fx.staging().join("4000000-1");
    fs::create_dir_all(dead.join("tree")).unwrap();
    fs::write(dead.join("tree/secret.txt"), "left behind\n").unwrap();
    let foreign = fx.staging().join("notes");
    fs::create_dir_all(&foreign).unwrap();
    // A review pass holds only the shared lock and removes nothing.
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(dead.exists(), "a preview removed a staging directory");
    // The approved run holds the writer lock and sweeps.
    let o = fx.sync(&digest_of(&o));
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(!dead.exists(), "the dead run's staging directory stayed");
    assert!(
        foreign.exists(),
        "an entry that is not a staging directory was removed"
    );
    assert_eq!(fx.staging_leftovers(), ["notes"]);
}

// ---- the composed run (DFM-63): configuration, packages, secrets, services -----------------

/// A module with one resource of each scope: a launch agent and the link that publishes its
/// definition, a Homebrew package and a 1Password secret.
const LIFE: &str = r#"schema = 1
id = "life"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "agent-plist"
kind = "link"
adapter = "fs-link/v1"
target = "Library/LaunchAgents/org.example.agent.plist"
[resources.value]
source = "files/org.example.agent.plist"

[[resources]]
id = "agent"
kind = "service"
adapter = "launchd-agent/v1"
[resources.value]
label = "org.example.agent"
plist = "Library/LaunchAgents/org.example.agent.plist"

[[resources]]
id = "atuin"
kind = "package"
adapter = "homebrew/v1"
[resources.value]
name = "atuin"
update_policy = "rolling"
downgrade = "forbid"

[[resources]]
id = "token"
kind = "secret"
adapter = "op-template/v1"
target = ".config/example/personal.env"
[resources.value]
template = "files/personal.env.tpl"
account = "example-account"
mode = 0o600
"#;

const BUNDLE_LIFE: &str = r#"schema = 1
id = "t-base"
core = "modules/core/module.toml"

[modules]
extra = "modules/extra/module.toml"
life = "modules/life/module.toml"
"#;

const AGENT_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>org.example.agent</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/bin/true</string>
	</array>
</dict>
</plist>
"#;

const AGENT_OP: &str = "life.agent@service";

/// `op whoami` exits 0 only with the `signed-in` marker, and kills itself with the
/// `whoami-killed` marker; `op inject` fills the one reference.
const FAKE_OP: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'op %s\n' "$*" >> "$here/../log"
[ -e "$here/../whoami-killed" ] && kill -KILL $$
[ -e "$here/../signed-in" ] || { echo "[ERROR] no session" >&2; exit 1; }
case "$1" in
  whoami) echo me; exit 0 ;;
  inject) sed 's|{{ op://vault/item/field }}|planted-value|' ;;
  *) exit 64 ;;
esac
"#;

/// `launchctl print` answers from `loaded` in launchd's report shape or exits 113;
/// `bootstrap` adds the plist's label with the resolved path of the plist, as launchd reports it,
/// `bootout` removes it (crates/cli/tests/services.rs keeps
/// the full look-alike).
const FAKE_LAUNCHCTL: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'launchctl %s\n' "$*" >> "$here/../log"
db="$here/../loaded"
case "$1" in
  print)
    label=${2##*/}
    line=$(grep "^$label " "$db" 2>/dev/null | head -1)
    if [ -z "$line" ]; then
      echo "Could not find service \"$label\" in domain for user gui: 501" >&2
      exit 113
    fi
    set -- $line
    printf 'gui/501/%s = {\n\tactive count = 0\n\tpath = %s\n\ttype = LaunchAgent\n\tstate = not running\n\n\tprogram = /usr/bin/true\n}\n' "$1" "$2"
    exit 0 ;;
  bootstrap)
    label=$(tr -d '\n\t' < "$3" | sed -n 's/.*<key>Label<\/key> *<string>\([^<]*\)<\/string>.*/\1/p')
    echo "$label $(readlink -f "$3")" >> "$db"; exit 0 ;;
  bootout)
    label=${2##*/}
    grep -v "^$label " "$db" > "$db.tmp" 2>/dev/null; mv "$db.tmp" "$db"; exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `brew services info|start|restart <name>` answers from `brew-<name>` (`started`: loaded and
/// running; anything else: installed, not loaded; no file: not installed), as
/// crates/cli/tests/services.rs keeps it. Every other command goes to the package look-alike.
const FAKE_BREW_WRAPPER: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
[ "$1" = services ] || exec "$here/brew-packages" "$@"
printf 'brew %s\n' "$*" >> "$here/../log"
name=$3
st="$here/../brew-$name"
case "$2" in
  info)
    [ -e "$st" ] || { echo "Error: No available formula with the name \"$name\"." >&2; exit 1; }
    s=$(cat "$st")
    if [ "$s" = started ]; then b=true; else b=false; fi
    printf '[{"name":"%s","service_name":"sh.brew.%s","running":%s,"loaded":%s,"pid":null,"status":"%s"}]\n' "$name" "$name" "$b" "$b" "$s"
    exit 0 ;;
  start|restart) echo started > "$st"; exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// A synced checkout whose profile also selects `life`, with `brew`, `op` and `launchctl`
/// look-alikes first on the provider path (git still comes from `/usr/bin`).
struct Life {
    fx: Fx,
    fake: TempDir,
}

impl Life {
    /// The `life` module is in the checkout and its configuration is published, so the agent's
    /// definition exists: every scope has work and none is blocked.
    fn new() -> Life {
        let l = Life::unpublished();
        let p = l.run(&["apply", "--root", l.fx.root_arg(), "--preview"]);
        assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
        let o = l.run(&["apply", "--root", l.fx.root_arg(), "-y", &digest_of(&p)]);
        assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
        assert!(
            l.fx.h("Library/LaunchAgents/org.example.agent.plist")
                .is_symlink()
        );
        l
    }

    /// The `life` module pushed upstream and pulled into the checkout, nothing published.
    fn unpublished() -> Life {
        let fx = Fx::new();
        let fake = TempDir::new("sync-providers");
        let bin = fake.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for (name, body) in [
            ("brew", FAKE_BREW_WRAPPER),
            ("brew-packages", common::providers::FAKE_BREW),
            ("op", FAKE_OP),
            ("launchctl", FAKE_LAUNCHCTL),
        ] {
            let p = bin.join(name);
            fs::write(&p, body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let l = Life { fx, fake };
        l.sign_in(true);
        let seed = &l.fx.upstream.seed;
        put(seed, "base/bundle.toml", BUNDLE_LIFE);
        put(seed, "base/modules/life/module.toml", LIFE);
        put(
            seed,
            "base/modules/life/files/org.example.agent.plist",
            AGENT_PLIST,
        );
        put(
            seed,
            "base/modules/life/files/personal.env.tpl",
            "TOKEN={{ op://vault/item/field }}\n",
        );
        put(
            seed,
            "profiles/t-01.toml",
            &common::PROFILE.replace("modules = [\"extra\"]", "modules = [\"extra\", \"life\"]"),
        );
        l.fx.upstream.push("add the life module");
        git(&l.fx.root, &["pull", "-q", "--ff-only", "origin", "main"]);
        l
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }

    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        self.command(env, args).output().unwrap()
    }

    fn command(&self, env: &[(&str, &str)], args: &[&str]) -> Command {
        let path = format!("{}:/usr/bin:/bin", self.fake.path().join("bin").display());
        let mut c = Command::new(BIN);
        c.env_clear()
            .envs(env.iter().copied())
            .env("DFM_PROVIDER_PATH", path)
            .env("DFM_SERVICE_TIMEOUT_SECS", "1")
            .arg("--home")
            .arg(self.fx.home.path())
            .args(args);
        c
    }

    /// Adds a running brew service `bar` whose configuration file links to `files/barrc`, and
    /// syncs it, so every scope is current except the agent the caller unloads.
    fn with_bar(&self) {
        fs::write(self.fake.path().join("brew-bar"), "started").unwrap();
        self.push_module(
            &bar_module("barrc"),
            &[("barrc", "height=1\n"), ("barrc2", "height=2\n")],
            "add bar",
        );
        let o = self.approved(&[]);
        assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
        assert!(self.fx.h(".config/bar/barrc").is_symlink());
    }

    fn preview(&self, flags: &[&str]) -> Output {
        let mut a = vec!["sync", "--root", self.fx.root_arg(), "--preview"];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    fn sync(&self, digest: &str, flags: &[&str]) -> Output {
        let mut a = vec!["sync", "--root", self.fx.root_arg(), "-y", digest];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    /// `sync` without a digest, with the look-alikes.
    fn plain(&self, flags: &[&str]) -> Output {
        let mut a = vec!["sync", "--root", self.fx.root_arg()];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    /// Preview, then sync with the previewed digest.
    fn approved(&self, flags: &[&str]) -> Output {
        let p = self.preview(flags);
        assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
        self.sync(&digest_of(&p), flags)
    }

    /// Sync with `digest`, and run `between` after the configuration scope and before the
    /// next scope starts (the `sync-after-scope` failpoint waits for it).
    fn sync_pausing(&self, digest: &str, flags: &[&str], between: impl FnOnce()) -> Output {
        let release = self.fake.path().join("release");
        let fp = format!("sync-after-scope:wait-{}", release.display());
        let mut a = vec!["sync", "--root", self.fx.root_arg(), "-y", digest];
        a.extend_from_slice(flags);
        let run = self
            .command(&[("DFM_FAILPOINT", fp.as_str())], &a)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let reached = release.with_extension("reached");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !reached.exists() {
            assert!(
                std::time::Instant::now() < end,
                "the failpoint was not reached"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        between();
        fs::write(&release, "").unwrap();
        run.wait_with_output().unwrap()
    }

    fn sign_in(&self, on: bool) {
        let m = self.fake.path().join("signed-in");
        if on {
            fs::write(m, "").unwrap();
        } else {
            let _ = fs::remove_file(m);
        }
    }

    fn mark(&self, name: &str) {
        fs::write(self.fake.path().join(name), "").unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    /// Replaces `life`'s module upstream with `LIFE` plus `extra` resources, and adds `files`.
    fn push_module(&self, extra: &str, files: &[(&str, &str)], message: &str) {
        let seed = &self.fx.upstream.seed;
        put(
            seed,
            "base/modules/life/module.toml",
            &format!("{LIFE}{extra}"),
        );
        for (f, c) in files {
            put(seed, &format!("base/modules/life/files/{f}"), c);
        }
        self.fx.upstream.push(message);
    }

    /// A configuration change upstream: one more link.
    fn push_link(&self) {
        let seed = &self.fx.upstream.seed;
        put(
            seed,
            "base/modules/life/module.toml",
            &format!(
                "{LIFE}\n[[resources]]\nid = \"notes\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/life/notes\"\n[resources.value]\nsource = \"files/notes\"\n"
            ),
        );
        put(seed, "base/modules/life/files/notes", "notes\n");
        self.fx.upstream.push("add a link");
    }

    /// Points the HOME link `rel` at `name`, next to its current target, as another program
    /// would.
    fn relink(&self, rel: &str, name: &str) {
        let link = self.fx.h(rel);
        let target = fs::read_link(&link).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(target.with_file_name(name), &link).unwrap();
    }

    /// The `command` of each receipt, oldest first.
    fn receipt_commands(&self) -> Vec<String> {
        self.fx
            .receipt_docs()
            .iter()
            .map(|r| r["command"].as_str().unwrap().to_string())
            .collect()
    }
}

/// The byte offset of each needle in `hay`, which must hold all of them in this order.
fn in_order(hay: &str, needles: &[&str]) {
    let mut at = 0;
    for n in needles {
        match hay[at..].find(n) {
            Some(i) => at += i + n.len(),
            None => panic!("{n:?} is missing or out of order in\n{hay}"),
        }
    }
}

#[test]
fn sync_runs_configuration_packages_secrets_and_services_in_order_under_one_approval() {
    let l = Life::new();
    l.push_link();
    let before = l.receipt_commands();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    in_order(
        &s,
        &[
            "scope: config",
            "planned life.notes@.config/life/notes",
            "scope: packages",
            "planned life.atuin@package",
            "scope: secrets",
            "planned life.token@.config/example/personal.env",
            "scope: services",
            "planned life.agent@service",
            "plan digest: ",
        ],
    );
    // The preview lists each scope's own operations only, and writes nothing.
    assert!(!s.contains("not_in_scope"), "{s}");
    for effect in ["brew install", "op inject", "launchctl bootstrap"] {
        assert!(!l.log().contains(effect), "the preview ran {effect}");
    }
    assert!(!l.fx.h(".config/life/notes").exists());
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "published: src0 ",
            "  config: ok",
            "  packages: ok",
            "  secrets: ok",
            "  services: ok",
        ],
    );
    assert!(s.ends_with("result: ok\n"), "{s}");
    in_order(
        &l.log(),
        &["brew install", "op inject", "launchctl bootstrap"],
    );
    assert!(l.fx.h(".config/life/notes").is_symlink());
    assert_eq!(
        fs::read_to_string(l.fx.h(".config/example/personal.env")).unwrap(),
        "TOKEN=planted-value\n"
    );
    // Each scope wrote its own receipt, in run order, approved by the digest.
    let after = l.receipt_commands();
    assert_eq!(
        after[before.len()..],
        ["apply", "packages", "secrets", "services"]
    );
    for r in &l.fx.receipt_docs()[before.len()..] {
        assert_eq!(r["approval"]["kind"], "yes_plan", "{r}");
    }
}

#[test]
fn a_plain_sync_runs_every_scope_without_a_digest() {
    let l = Life::new();
    l.push_link();
    let before = l.receipt_commands();
    let o = l.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "plan digest: ",
            "published: src0 ",
            "  config: ok",
            "  packages: ok",
            "  secrets: ok",
            "  services: ok",
        ],
    );
    assert!(s.ends_with("result: ok\n"), "{s}");
    in_order(
        &l.log(),
        &["brew install", "op inject", "launchctl bootstrap"],
    );
    assert!(l.fx.h(".config/life/notes").is_symlink());
    assert_eq!(
        l.receipt_commands()[before.len()..],
        ["apply", "packages", "secrets", "services"]
    );
    for r in &l.fx.receipt_docs()[before.len()..] {
        assert_eq!(r["approval"]["kind"], "unattended", "{r}");
    }
}

#[test]
fn a_packages_scope_after_the_configuration_scope_probes_once_per_plan() {
    let l = Life::new();
    l.push_link();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let log = l.log();
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // One package probe runs `brew ruby` once. The review, the plan under the sync lock, the
    // recheck after the configuration scope and the plan under the packages scope's lock each
    // probe, and the engine probes once before the install; the scope's first look reuses the
    // recheck.
    let probes = l.log()[log.len()..].matches("\nbrew ruby ").count();
    assert_eq!(probes, 5, "{}", l.log());
}

#[test]
fn a_package_failure_leaves_the_later_scopes_not_started() {
    let l = Life::new();
    l.mark("fail");
    let o = l.approved(&[]);
    assert_eq!(code(&o), 1, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "  config: current",
            "  packages: failed",
            "  secrets: not_started",
            "  services: not_started",
            "result: failed",
        ],
    );
    assert!(l.log().contains("brew install"), "{}", l.log());
    for effect in ["op inject", "launchctl bootstrap"] {
        assert!(!l.log().contains(effect), "a later scope ran {effect}");
    }
    assert!(!l.fx.h(".config/example/personal.env").exists());
}

#[test]
fn secrets_without_a_session_are_skipped_and_the_warning_ends_stdout() {
    let l = Life::new();
    l.sign_in(false);
    let warning = "warning: secrets skipped, no 1Password session for example-account (op whoami exited 1): life.token. Fix: unlock 1Password, then run dfm sync or dfm secrets again";
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    in_order(
        &s,
        &[
            "scope: secrets",
            "skipped: no session life.token",
            "scope: services",
        ],
    );
    assert!(s.ends_with(&format!("{warning}\n")), "{s}");
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "  packages: ok",
            "  secrets: skipped: no session (life.token)",
            "  services: ok",
            "result: ok\n",
        ],
    );
    assert!(s.ends_with(&format!("result: ok\n{warning}\n")), "{s}");
    assert!(!l.log().contains("op inject"), "{}", l.log());
    assert!(l.log().contains("launchctl bootstrap"), "{}", l.log());
    assert!(!l.fx.h(".config/example/personal.env").exists());
    // With a session the skipped secret renders. Afterwards its destination matches the
    // ledger: a run with nothing else new needs no approval and does not contact the vault.
    l.sign_in(true);
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("  secrets: ok"), "{}", stdout(&o));
    l.sign_in(false);
    let log = l.log();
    let o = l.run(&["sync", "--root", l.fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("  secrets: current, not rendered again"), "{s}");
    assert!(s.ends_with("result: ok\n"), "{s}");
    assert!(!l.log()[log.len()..].contains("op "), "{}", l.log());
}

#[test]
fn the_sync_digest_changes_when_only_the_package_plan_changes() {
    let l = Life::new();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let first = digest_of(&p);
    // Install the package behind the manager's back: the sources, the configuration, the
    // secrets and the services are as they were.
    let keg = l.fake.path().join("Cellar/atuin/1.0.0");
    fs::create_dir_all(&keg).unwrap();
    fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        r#"{"source":{"tap":"homebrew/core"}}"#,
    )
    .unwrap();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    assert!(
        stdout(&p).contains("current life.atuin@package"),
        "{}",
        stdout(&p)
    );
    assert_ne!(digest_of(&p), first);
    let o = l.sync(&first, &[]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!l.log().contains("op inject"), "{}", l.log());
}

#[test]
fn a_repeat_sync_with_nothing_new_in_any_scope_is_a_no_op() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let (receipts, log) = (l.receipt_commands(), l.log());
    let o = l.run(&["sync", "--root", l.fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("nothing to change: no approval needed"), "{s}");
    assert!(s.contains("  secrets: current, not rendered again"), "{s}");
    // Nothing was skipped, so no warning.
    assert!(
        s.ends_with("result: ok\n") && !s.contains("warning:"),
        "{s}"
    );
    let new = &l.log()[log.len()..];
    for effect in ["brew install", "op ", "launchctl bootstrap"] {
        assert!(!new.contains(effect), "a no-op sync ran {effect}:\n{new}");
    }
    // An approved repeat publishes nothing and writes nothing. Only the secret is rendered
    // again, because the plan cannot tell its result before the render; the result is equal.
    let log = l.log();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("published: src0 current at"), "{s}");
    in_order(
        &s,
        &[
            "  config: current",
            "  packages: current",
            "  secrets: ok",
            "  services: current",
        ],
    );
    assert!(
        s.contains("current life.token@.config/example/personal.env"),
        "{s}"
    );
    assert_eq!(
        l.receipt_commands(),
        receipts,
        "a no-op sync wrote a receipt"
    );
    let new = &l.log()[log.len()..];
    for effect in ["brew install", "launchctl bootstrap"] {
        assert!(!new.contains(effect), "a no-op sync ran {effect}:\n{new}");
    }
}

#[test]
fn a_changed_secret_mode_is_not_a_no_op() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // A local commit declares a stricter mode; the sources stay current (ahead of the ref).
    git(&l.fx.root, &["checkout", "-q", "-b", "topic"]);
    put(
        &l.fx.root,
        "base/modules/life/module.toml",
        &LIFE.replace("mode = 0o600", "mode = 0o400"),
    );
    git(&l.fx.root, &["commit", "-q", "-am", "stricter secret mode"]);
    // The observed mode (0600) is not the declared one (0400): the plan can see the change,
    // so a plain sync asks for its approval instead of ending as a no-op.
    let o = l.run(&["sync", "--root", l.fx.root_arg()]);
    let s = stdout(&o);
    assert_eq!(code(&o), 3, "{s}\n{}", stderr(&o));
    assert!(!s.contains("nothing to change: no approval needed"), "{s}");
    assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
}

#[test]
fn restart_is_forwarded_to_the_services_scope_and_runs_only_when_named() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let o = l.preview(&["--restart", "nope"]);
    assert_eq!(code(&o), 2, "{}\n{}", stdout(&o), stderr(&o));
    // A plain run never restarts a running service on its own.
    let o = l.plain(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(!l.log().contains("launchctl bootout"), "{}", l.log());
    // --restart is the approval: no --allow-disruptive flag is needed.
    let flags = ["--restart", "life.agent"];
    let p = l.preview(&flags);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    // The digest covers the restart: without it the approval is stale.
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!l.log().contains("launchctl bootout"), "{}", l.log());
    let o = l.plain(&flags);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(&l.log(), &["launchctl bootout", "launchctl bootstrap"]);
    assert!(stdout(&o).contains("  services: ok"), "{}", stdout(&o));
}

#[test]
fn a_service_whose_definition_the_same_run_publishes_is_planned_not_blocked() {
    // The agent's definition is not published yet: the configuration scope publishes it, and
    // the services scope starts the agent in the same run.
    let l = Life::unpublished();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    in_order(
        &s,
        &[
            "scope: config",
            "planned life.agent-plist@Library/LaunchAgents/org.example.agent.plist",
            "scope: services",
            "planned life.agent@service [life] start life.agent after the configuration scope publishes Library/LaunchAgents/org.example.agent.plist (life.agent-plist@Library/LaunchAgents/org.example.agent.plist)",
        ],
    );
    assert!(!s.contains("blocked"), "{s}");
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &[
            "  config: ok",
            "  packages: ok",
            "  secrets: ok",
            "  services: ok",
            "result: ok",
        ],
    );
    assert!(l.log().contains("launchctl bootstrap"), "{}", l.log());
    assert_eq!(
        l.receipt_commands(),
        ["apply", "packages", "secrets", "services"]
    );
}

#[test]
fn a_deferred_definition_another_program_retargets_after_the_configuration_scope_is_stale() {
    let l = Life::unpublished();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    // Another program points the published definition link at a plist that runs another
    // program, before the services scope plans again.
    let other = l.fake.path().join("other.plist");
    fs::write(
        &other,
        AGENT_PLIST.replace("/usr/bin/true", "/usr/bin/false"),
    )
    .unwrap();
    let rel = "Library/LaunchAgents/org.example.agent.plist";
    let o = l.sync_pausing(&digest_of(&p), &[], || {
        fs::remove_file(l.fx.h(rel)).unwrap();
        std::os::unix::fs::symlink(&other, l.fx.h(rel)).unwrap();
    });
    assert_eq!(code(&o), 1, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &[
            "  config: ok",
            "  services: failed (E_PLAN_STALE)",
            "result: failed",
        ],
    );
    assert!(!l.log().contains("launchctl bootstrap"), "{}", l.log());
}

#[test]
fn a_blocked_services_plan_does_not_stop_the_other_scopes_and_is_warned_last() {
    let l = Life::new();
    // launchd holds the label from another definition: a blocker the run cannot clear.
    fs::write(
        l.fake.path().join("loaded"),
        "org.example.agent /tmp/elsewhere.plist\n",
    )
    .unwrap();
    l.push_link();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    in_order(
        &stdout(&p),
        &[
            "scope: services",
            "blocked life.agent@service",
            "blocked: this scope does not run: life.agent@service E_CONFLICT",
            "plan digest: ",
            "warning: services blocked: life.agent@service E_CONFLICT. Fix: ",
        ],
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "  config: ok",
            "  packages: ok",
            "  secrets: ok",
            "  services: blocked (life.agent@service E_CONFLICT)",
            "result: ok\nwarning: services blocked: life.agent@service E_CONFLICT. Fix: unload it by hand",
        ],
    );
    assert_eq!(
        s.lines().last().unwrap(),
        s.lines()
            .find(|l| l.starts_with("warning: services"))
            .unwrap(),
        "{s}"
    );
    assert!(l.fx.h(".config/life/notes").is_symlink());
    assert!(!l.log().contains("launchctl bootstrap"), "{}", l.log());
}

#[test]
fn an_edited_definition_is_reviewed_from_staging_and_its_restart_runs() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // Upstream changes the program of the loaded agent; the owner asks for the restart that
    // loads it.
    put(
        &l.fx.upstream.seed,
        "base/modules/life/files/org.example.agent.plist",
        &AGENT_PLIST.replace("/usr/bin/true", "/usr/bin/false"),
    );
    l.fx.upstream.push("the agent runs another program");
    let flags = ["--restart", "life.agent", "--allow-disruptive", AGENT_OP];
    let p = l.preview(&flags);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    assert!(s.contains("the definition runs /usr/bin/false"), "{s}");
    assert!(!s.contains("the definition runs /usr/bin/true"), "{s}");
    let o = l.sync(&digest_of(&p), &flags);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("  services: ok"), "{}", stdout(&o));
    in_order(&l.log(), &["launchctl bootout", "launchctl bootstrap"]);
}

#[test]
fn a_generated_definition_the_configuration_changes_is_reviewed_from_its_new_bytes() {
    // The agent's definition is a generated file (`text-fragments/v1`), not a link: the review
    // reads the bytes the configuration scope will write, not the old HOME file.
    let l = Life::unpublished();
    let generated = LIFE.replace(
        "kind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"Library/LaunchAgents/org.example.agent.plist\"\n[resources.value]\nsource = \"files/org.example.agent.plist\"",
        "kind = \"assemble\"\nadapter = \"text-fragments/v1\"\ntarget = \"Library/LaunchAgents/org.example.agent.plist\"\n[resources.value]\nbody = \"files/org.example.agent.plist\"\nfragments_dir = \".config/agent\"\npattern = \"*.frag\"\nseparator = \"\\n\"",
    );
    assert_ne!(generated, LIFE);
    let seed = &l.fx.upstream.seed;
    put(seed, "base/modules/life/module.toml", &generated);
    l.fx.upstream.push("generate the agent definition");
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let plist = l.fx.h("Library/LaunchAgents/org.example.agent.plist");
    assert!(plist.is_file() && !plist.is_symlink());
    assert!(l.log().contains("launchctl bootstrap"), "{}", l.log());
    // Upstream changes the program; the owner asks for the restart that loads it.
    put(
        seed,
        "base/modules/life/files/org.example.agent.plist",
        &AGENT_PLIST.replace("/usr/bin/true", "/usr/bin/false"),
    );
    l.fx.upstream.push("the agent runs another program");
    let flags = ["--restart", "life.agent", "--allow-disruptive", AGENT_OP];
    let p = l.preview(&flags);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    assert!(s.contains("the definition runs /usr/bin/false"), "{s}");
    assert!(!s.contains("the definition runs /usr/bin/true"), "{s}");
    let log = l.log();
    let o = l.sync(&digest_of(&p), &flags);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    in_order(&s, &["  config: ok", "  services: ok", "result: ok"]);
    assert!(
        fs::read_to_string(&plist)
            .unwrap()
            .contains("/usr/bin/false")
    );
    in_order(
        &l.log()[log.len()..],
        &["launchctl bootout", "launchctl bootstrap"],
    );
}

/// A brew service `bar` and the link of its configuration file to `files/<source>`.
fn bar_module(source: &str) -> String {
    format!(
        "\n[[resources]]\nid = \"barrc\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/bar/barrc\"\n[resources.value]\nsource = \"files/{source}\"\n\n[[resources]]\nid = \"bar\"\nkind = \"service\"\nadapter = \"brew-service/v1\"\n[resources.value]\nname = \"bar\"\nconfig = [\".config/bar/barrc\"]\n"
    )
}

#[test]
fn a_service_with_a_second_missing_config_file_the_configuration_does_not_publish_is_blocked() {
    let l = Life::new();
    fs::write(l.fake.path().join("brew-bar"), "stopped").unwrap();
    // bar reads two config files; the configuration scope publishes only the first, and
    // nothing publishes the second.
    let module = bar_module("barrc").replace(
        "config = [\".config/bar/barrc\"]",
        "config = [\".config/bar/barrc\", \".config/bar/barrc2\"]",
    );
    l.push_module(&module, &[("barrc", "height=1\n")], "bar reads two files");
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    in_order(
        &stdout(&p),
        &[
            "planned life.barrc@.config/bar/barrc",
            "scope: services",
            "blocked: this scope does not run: ",
            "warning: services blocked: ",
        ],
    );
    let s = stdout(&p);
    assert!(
        s.contains(".config/bar/barrc2, which is not published"),
        "{s}"
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "  config: ok",
            "  services: blocked (",
            "result: ok\nwarning: services blocked: ",
        ],
    );
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    assert!(l.fx.h(".config/bar/barrc").is_symlink());
    assert!(!l.log().contains("brew services start"), "{}", l.log());
}

/// A new agent whose staged definition `plist` the review must refuse: the preview blocks the
/// services scope with `why`, and the sync publishes the configuration and ends with a warning,
/// not with a stale services plan after publication.
fn a_new_agent_with_a_bad_staged_definition_is_blocked_in_the_review(plist: &str, why: &str) {
    let l = Life::unpublished();
    put(
        &l.fx.upstream.seed,
        "base/modules/life/files/org.example.agent.plist",
        plist,
    );
    l.fx.upstream.push("a bad agent definition");
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    in_order(
        &s,
        &[
            "planned life.agent-plist@Library/LaunchAgents/org.example.agent.plist",
            "scope: services",
            "blocked life.agent@service",
            "blocked: this scope does not run: life.agent@service E_CONFLICT, life.agent@service E_MANIFEST_INVALID",
            "warning: services blocked: ",
        ],
    );
    assert!(s.contains(why), "{s}");
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "  config: ok",
            "  services: blocked (",
            "result: ok\nwarning: services blocked: ",
        ],
    );
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    assert!(
        l.fx.h("Library/LaunchAgents/org.example.agent.plist")
            .is_symlink()
    );
    assert!(!l.log().contains("launchctl bootstrap"), "{}", l.log());
}

#[test]
fn a_new_agent_whose_staged_definition_declares_another_label_is_blocked_in_the_review() {
    a_new_agent_with_a_bad_staged_definition_is_blocked_in_the_review(
        &AGENT_PLIST.replace(
            "<string>org.example.agent</string>",
            "<string>org.example.other</string>",
        ),
        "declares another label than org.example.agent",
    );
}

#[test]
fn a_new_agent_whose_staged_definition_is_not_xml_is_blocked_in_the_review() {
    a_new_agent_with_a_bad_staged_definition_is_blocked_in_the_review(
        &AGENT_PLIST.replace("</dict>", "</dct>"),
        "is not an XML property list the adapter reads",
    );
}

#[test]
fn a_brew_service_without_its_formula_is_blocked_even_when_the_configuration_publishes_its_file() {
    // The formula bar is not installed and no package operation installs it; the configuration
    // scope publishes bar's config file. Publishing the file does not unblock bar.
    let l = Life::new();
    l.push_module(&bar_module("barrc"), &[("barrc", "height=1\n")], "add bar");
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    in_order(
        &s,
        &[
            "planned life.barrc@.config/bar/barrc",
            "scope: services",
            "blocked life.bar@service",
            "blocked: this scope does not run: ",
            "warning: services blocked: ",
        ],
    );
    assert!(
        s.contains(".config/bar/barrc, which is not published"),
        "{s}"
    );
    assert!(s.contains("the formula bar is not installed"), "{s}");
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "  config: ok",
            "  services: blocked (",
            "result: ok\nwarning: services blocked: ",
        ],
    );
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    assert!(l.fx.h(".config/bar/barrc").is_symlink());
    assert!(!l.log().contains("brew services start"), "{}", l.log());
}

#[test]
fn a_service_config_file_the_configuration_scope_rewrites_does_not_make_services_stale() {
    let l = Life::new();
    l.with_bar();
    // The configuration scope now points bar's config file elsewhere, and the agent is not
    // loaded: the services scope runs with a changed `path:` precondition of another service.
    l.push_module(&bar_module("barrc2"), &[], "bar reads barrc2");
    fs::write(l.fake.path().join("loaded"), "").unwrap();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    in_order(
        &stdout(&p),
        &[
            "planned life.barrc@.config/bar/barrc",
            "planned life.agent@service",
        ],
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &["  config: ok", "  services: ok", "result: ok"],
    );
    assert!(
        fs::read_link(l.fx.h(".config/bar/barrc"))
            .unwrap()
            .ends_with("barrc2")
    );
    assert!(l.log().contains("launchctl bootstrap"), "{}", l.log());
}

#[test]
fn a_service_precondition_on_a_path_the_configuration_did_not_write_stays_stale() {
    let l = Life::new();
    l.with_bar();
    // The configuration scope writes another file; bar's config file is not in its plan.
    l.push_module(
        &format!(
            "{}\n[[resources]]\nid = \"notes\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/life/notes\"\n[resources.value]\nsource = \"files/notes\"\n",
            bar_module("barrc")
        ),
        &[("notes", "notes\n")],
        "add a link",
    );
    fs::write(l.fake.path().join("loaded"), "").unwrap();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    assert!(
        stdout(&p).contains("current life.barrc@.config/bar/barrc"),
        "{}",
        stdout(&p)
    );
    let log = l.log();
    // After the configuration scope, bar's config file changes by another hand.
    let o = l.sync_pausing(&digest_of(&p), &[], || {
        l.relink(".config/bar/barrc", "barrc2")
    });
    assert_eq!(code(&o), 1, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &[
            "  config: ok",
            "  services: failed (E_PLAN_STALE)",
            "result: failed",
        ],
    );
    let new = &l.log()[log.len()..];
    assert!(!new.contains("launchctl bootstrap"), "{new}");
}

#[test]
fn a_service_config_link_another_program_retargets_after_the_configuration_scope_is_stale() {
    let l = Life::new();
    l.with_bar();
    // The configuration scope points bar's config file at barrc2; then another program points
    // it at barrc3, before the services scope plans again.
    l.push_module(
        &bar_module("barrc2"),
        &[("barrc3", "height=3\n")],
        "bar reads barrc2",
    );
    fs::write(l.fake.path().join("loaded"), "").unwrap();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let log = l.log();
    let o = l.sync_pausing(&digest_of(&p), &[], || {
        l.relink(".config/bar/barrc", "barrc3")
    });
    assert_eq!(code(&o), 1, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &[
            "  config: ok",
            "  services: failed (E_PLAN_STALE)",
            "result: failed",
        ],
    );
    let new = &l.log()[log.len()..];
    assert!(!new.contains("brew services restart"), "{new}");
    assert!(!new.contains("brew services start"), "{new}");
    assert!(!new.contains("launchctl bootstrap"), "{new}");
}

#[test]
fn a_directory_the_configuration_scope_creates_for_a_new_secret_does_not_make_secrets_stale() {
    let l = Life::new();
    assert!(!l.fx.h(".config/example").exists());
    // A new link in the directory of the secret that was never rendered: the configuration and
    // the secrets plans both create .config/example.
    l.push_module(
        "\n[[resources]]\nid = \"exconf\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/example/config\"\n[resources.value]\nsource = \"files/exconf\"\n",
        &[("exconf", "conf\n")],
        "add a link next to the secret",
    );
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    in_order(
        &stdout(&p),
        &[
            "scope: config",
            "planned mkdir@.config/example",
            "scope: secrets",
            "planned mkdir@.config/example",
            "scope: services",
        ],
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &[
            "  config: ok",
            "  packages: ok",
            "  secrets: ok",
            "  services: ok",
            "result: ok",
        ],
    );
    assert_eq!(
        fs::read_to_string(l.fx.h(".config/example/personal.env")).unwrap(),
        "TOKEN=planted-value\n"
    );
}

#[test]
fn a_deferred_service_named_by_restart_is_reviewed_and_run_as_a_restart() {
    let l = Life::new();
    l.with_bar();
    // The running bar now also reads a file that only this run publishes: its operation is
    // deferred, and the owner asks for the restart that makes bar read it.
    let module = format!(
        "{}\n[[resources]]\nid = \"barextra\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/bar/extra\"\n[resources.value]\nsource = \"files/barextra\"\n",
        bar_module("barrc").replace(
            "config = [\".config/bar/barrc\"]",
            "config = [\".config/bar/barrc\", \".config/bar/extra\"]"
        )
    );
    l.push_module(&module, &[("barextra", "width=1\n")], "bar reads extra");
    const BAR_OP: &str = "life.bar@service";
    // The restart is disruptive: the preview asks for its flag before anything is published.
    let o = l.preview(&["--restart", "life.bar"]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains(&format!("--allow-disruptive '{BAR_OP}'")),
        "{}",
        stdout(&o)
    );
    let o = l.sync(&digest_of(&o), &["--restart", "life.bar"]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(!l.fx.h(".config/bar/extra").exists());
    let flags = ["--restart", "life.bar", "--allow-disruptive", BAR_OP];
    let p = l.preview(&flags);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    in_order(
        &stdout(&p),
        &[
            "planned life.barextra@.config/bar/extra",
            "planned life.bar@service [life] restart life.bar on request (--restart) after the configuration scope publishes .config/bar/extra",
        ],
    );
    let log = l.log();
    let o = l.sync(&digest_of(&p), &flags);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &["  config: ok", "  services: ok", "result: ok"],
    );
    assert!(
        l.log()[log.len()..].contains("brew services restart bar"),
        "{}",
        l.log()
    );
}

#[test]
fn a_deferred_start_preview_does_not_approve_a_restart_of_that_service() {
    let l = Life::new();
    l.with_bar();
    let module = format!(
        "{}\n[[resources]]\nid = \"barextra\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/bar/extra\"\n[resources.value]\nsource = \"files/barextra\"\n",
        bar_module("barrc").replace(
            "config = [\".config/bar/barrc\"]",
            "config = [\".config/bar/barrc\", \".config/bar/extra\"]"
        )
    );
    l.push_module(&module, &[("barextra", "width=1\n")], "bar reads extra");
    // The preview reviews bar's deferred operation as a start: its digest does not approve
    // the restart that the same flags plus `--restart` would review.
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let log = l.log();
    let flags = [
        "--restart",
        "life.bar",
        "--allow-disruptive",
        "life.bar@service",
    ];
    let o = l.sync(&digest_of(&p), &flags);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!l.fx.h(".config/bar/extra").exists());
    assert!(
        !l.log()[log.len()..].contains("brew services restart bar"),
        "{}",
        l.log()
    );
}

#[test]
fn a_new_agent_declared_in_another_case_than_its_link_starts_in_the_same_sync() {
    // The agent declares its definition in another case than the link that publishes it,
    // which the case-insensitive file system resolves to the same file.
    let l = Life::unpublished();
    let seed = &l.fx.upstream.seed;
    put(
        seed,
        "base/modules/life/module.toml",
        &LIFE.replace(
            "plist = \"Library/LaunchAgents/org.example.agent.plist\"",
            "plist = \"Library/LaunchAgents/Org.Example.Agent.plist\"",
        ),
    );
    l.fx.upstream.push("declare the definition in another case");
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    assert!(!stdout(&p).contains("blocked"), "{}", stdout(&p));
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &["  config: ok", "  services: ok", "result: ok"],
    );
    assert!(l.log().contains("launchctl bootstrap"), "{}", l.log());
}

#[test]
fn a_definition_link_the_configuration_retargets_is_reviewed_from_its_new_target() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // Upstream points the loaded agent's definition link at another source file, which runs
    // another program. Both source files stay present.
    let seed = &l.fx.upstream.seed;
    put(
        seed,
        "base/modules/life/module.toml",
        &LIFE.replace(
            "source = \"files/org.example.agent.plist\"",
            "source = \"files/agent2.plist\"",
        ),
    );
    put(
        seed,
        "base/modules/life/files/agent2.plist",
        &AGENT_PLIST.replace("/usr/bin/true", "/usr/bin/false"),
    );
    l.fx.upstream
        .push("the agent link points at another definition");
    let flags = ["--restart", "life.agent", "--allow-disruptive", AGENT_OP];
    let p = l.preview(&flags);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    assert!(s.contains("the definition runs /usr/bin/false"), "{s}");
    assert!(!s.contains("the definition runs /usr/bin/true"), "{s}");
    let o = l.sync(&digest_of(&p), &flags);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &["  config: ok", "  services: ok", "result: ok"],
    );
    in_order(&l.log(), &["launchctl bootout", "launchctl bootstrap"]);
    // The launcher now holds the new definition, so the next sync has nothing to do.
    let loaded = fs::read_to_string(l.fake.path().join("loaded")).unwrap();
    assert!(loaded.ends_with("/agent2.plist\n"), "{loaded}");
    let o = l.run(&["sync", "--root", l.fx.root_arg(), "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(!stdout(&o).contains("blocked"), "{}", stdout(&o));
}

#[test]
fn a_retargeted_definition_declared_in_another_case_is_reviewed_from_its_new_target() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // The service declares its definition in another case than the link's target, which the
    // case-insensitive file system resolves to the same file.
    let seed = &l.fx.upstream.seed;
    put(
        seed,
        "base/modules/life/module.toml",
        &LIFE
            .replace(
                "source = \"files/org.example.agent.plist\"",
                "source = \"files/agent2.plist\"",
            )
            .replace(
                "plist = \"Library/LaunchAgents/org.example.agent.plist\"",
                "plist = \"Library/LaunchAgents/Org.Example.Agent.plist\"",
            ),
    );
    put(
        seed,
        "base/modules/life/files/agent2.plist",
        &AGENT_PLIST.replace("/usr/bin/true", "/usr/bin/false"),
    );
    l.fx.upstream
        .push("the agent link points at another definition");
    let flags = ["--restart", "life.agent", "--allow-disruptive", AGENT_OP];
    let p = l.preview(&flags);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    assert!(s.contains("the definition runs /usr/bin/false"), "{s}");
    assert!(!s.contains("the definition runs /usr/bin/true"), "{s}");
    let o = l.sync(&digest_of(&p), &flags);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &["  config: ok", "  services: ok", "result: ok"],
    );
    in_order(&l.log(), &["launchctl bootout", "launchctl bootstrap"]);
    let loaded = fs::read_to_string(l.fake.path().join("loaded")).unwrap();
    assert!(loaded.ends_with("/agent2.plist\n"), "{loaded}");
}

#[test]
fn an_agent_loaded_from_the_definition_before_a_retarget_stays_the_managers() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let db = l.fake.path().join("loaded");
    let old = fs::read_to_string(&db).unwrap();
    // Upstream points the definition link at another source file; the owner does not restart.
    let seed = &l.fx.upstream.seed;
    put(
        seed,
        "base/modules/life/module.toml",
        &LIFE.replace(
            "source = \"files/org.example.agent.plist\"",
            "source = \"files/agent2.plist\"",
        ),
    );
    put(
        seed,
        "base/modules/life/files/agent2.plist",
        &AGENT_PLIST.replace("/usr/bin/true", "/usr/bin/false"),
    );
    l.fx.upstream
        .push("the agent link points at another definition");
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // launchd still reports the file the link resolved to before.
    assert_eq!(fs::read_to_string(&db).unwrap(), old);
    // The next sync and a standalone services run are not blocked by that load.
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    assert!(!stdout(&p).contains("blocked"), "{}", stdout(&p));
    let s = l.run(&["services", "--root", l.fx.root_arg(), "--preview"]);
    assert_eq!(code(&s), 0, "{}\n{}", stdout(&s), stderr(&s));
    assert!(!stdout(&s).contains("blocked"), "{}", stdout(&s));
    // A requested restart loads the new definition.
    let o = l.approved(&["--restart", "life.agent", "--allow-disruptive", AGENT_OP]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let loaded = fs::read_to_string(&db).unwrap();
    assert!(loaded.ends_with("/agent2.plist\n"), "{loaded}");
    // A load from a file outside the checkout root stays foreign.
    let copy = l.fake.path().join("agent2.plist");
    fs::write(&copy, AGENT_PLIST).unwrap();
    let copy = fs::canonicalize(copy).unwrap();
    fs::write(&db, format!("org.example.agent {}\n", copy.display())).unwrap();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    assert!(
        stdout(&p).contains("blocked life.agent@service"),
        "{}",
        stdout(&p)
    );
}

#[test]
fn a_session_that_ends_before_the_secrets_scope_names_dfm_secrets_as_the_fix() {
    let l = Life::new();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // A template change is a re-render: the review does not ask the vault, the scope does.
    put(
        &l.fx.upstream.seed,
        "base/modules/life/files/personal.env.tpl",
        "TOKEN={{ op://vault/item/field }}\nMORE=1\n",
    );
    l.fx.upstream.push("the template changes");
    l.sign_in(false);
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(
        s.contains("  secrets: skipped: no session (life.token)"),
        "{s}"
    );
    assert!(
        s.ends_with("result: ok\nwarning: secrets skipped, no 1Password session for example-account (op whoami exited 1): life.token. Fix: unlock 1Password, then run dfm secrets, which renders them\n"),
        "{s}"
    );
    assert_eq!(
        fs::read_to_string(l.fx.h(".config/example/personal.env")).unwrap(),
        "TOKEN=planted-value\n"
    );
    // A plain sync now has nothing to approve; `dfm secrets` renders the change.
    l.sign_in(true);
    let p = l.run(&["secrets", "--root", l.fx.root_arg(), "--preview"]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let o = l.run(&["secrets", "--root", l.fx.root_arg(), "-y", &digest_of(&p)]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert_eq!(
        fs::read_to_string(l.fx.h(".config/example/personal.env")).unwrap(),
        "TOKEN=planted-value\nMORE=1\n"
    );
}

#[test]
fn a_session_check_that_does_not_answer_fails_instead_of_skipping() {
    let l = Life::new();
    l.mark("whoami-killed");
    let o = l.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("E_CREDENTIAL_UNAVAILABLE"),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("ended by a signal"), "{}", stderr(&o));
    assert!(!stdout(&o).contains("skipped"), "{}", stdout(&o));
}

#[test]
fn a_stop_request_between_scopes_starts_no_later_scope() {
    let l = Life::new();
    l.push_link();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let o = l.run_env(
        &[("DFM_FAILPOINT", "sync-after-scope:stop")],
        &["sync", "--root", l.fx.root_arg(), "-y", &digest_of(&p)],
    );
    assert_eq!(code(&o), 143, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &[
            "  config: ok",
            "  packages: not_started",
            "  secrets: not_started",
            "  services: not_started",
            "result: terminated",
        ],
    );
    assert!(!l.log().contains("brew install"), "{}", l.log());
}

#[test]
fn a_refused_sync_ends_with_its_refusal_not_the_warnings() {
    let l = Life::new();
    l.sign_in(false);
    l.push_link();
    // A digest that is not this run's is refused: the plans list the skipped secrets, and no
    // warning follows.
    let o = l.sync(&"0".repeat(64), &[]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("skipped: no session life.token"), "{s}");
    assert!(s.ends_with("result: blocked\n"), "{s}");
    assert!(!s.contains("warning:"), "{s}");
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
}

#[test]
fn a_dangling_service_config_link_the_configuration_removes_leaves_services_blocked() {
    let l = Life::new();
    l.with_bar();
    // Upstream drops bar's config link and its source, while bar still declares the file. The
    // published link now dangles, and the configuration plan removes it: nothing publishes the
    // file again, so the services scope stays blocked.
    let seed = &l.fx.upstream.seed;
    fs::remove_file(seed.join("base/modules/life/files/barrc")).unwrap();
    let service = bar_module("barrc");
    let service = &service[service.find("\n[[resources]]\nid = \"bar\"").unwrap()..];
    put(
        seed,
        "base/modules/life/module.toml",
        &format!("{LIFE}{service}"),
    );
    l.fx.upstream.push("drop bar's config link");
    git(&l.fx.root, &["pull", "-q", "--ff-only", "origin", "main"]);
    assert!(fs::metadata(l.fx.h(".config/bar/barrc")).is_err());
    let allow = ["--allow-disruptive", "remove@life.barrc@.config/bar/barrc"];
    let p = l.preview(&allow);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    in_order(
        &s,
        &[
            "scope: config",
            "planned remove@life.barrc@.config/bar/barrc",
            "scope: services",
            "blocked: this scope does not run: life.bar@service E_CONFLICT",
            "warning: services blocked: life.bar@service E_CONFLICT",
        ],
    );
    assert!(
        !s.contains("after the configuration scope publishes"),
        "{s}"
    );
    let log = l.log();
    let o = l.sync(&digest_of(&p), &allow);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    in_order(
        &s,
        &[
            "  config: ok",
            "  services: blocked (life.bar@service E_CONFLICT)",
            "result: ok\nwarning: services blocked: life.bar@service E_CONFLICT",
        ],
    );
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    assert!(!l.fx.h(".config/bar/barrc").is_symlink());
    // Read-only `brew services info` probes run; nothing starts or restarts the service.
    let new = &l.log()[log.len()..];
    for effect in ["brew services start", "brew services restart"] {
        assert!(!new.contains(effect), "{new}");
    }
}

#[test]
fn a_sync_whose_scopes_fit_the_journal_alone_but_not_together_is_refused_before_publication() {
    use dfm_model::state::{Class, JournalEntry, MAX_JOURNAL_BYTES, Phase};

    let l = Life::unpublished();
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    // The journaled operations of each scope, from its summary list of planned ids, the
    // deferred service start included.
    let mut per_scope: Vec<u64> = Vec::new();
    for line in stdout(&p).lines() {
        if line.starts_with("scope: ") {
            per_scope.push(0);
        } else if line
            .strip_prefix("  planned ")
            .is_some_and(|id| !id.contains(' '))
        {
            *per_scope.last_mut().unwrap() += 1;
        }
    }
    let (most, total) = (
        *per_scope.iter().max().unwrap(),
        per_scope.iter().sum::<u64>(),
    );
    assert!(total > most, "{per_scope:?}");
    // Room for the largest scope alone (4096 bytes per operation), less than one more.
    let free = most * 4096 + 100;
    let journal = l.fx.h(".local/state/dotfiles-manager/state/journal.jsonl");
    let len = fs::metadata(&journal).map_or(0, |m| m.len());
    let line = JournalEntry {
        schema: dfm_model::state::SCHEMA,
        op_id: "closed-1".into(),
        op: "create-file".into(),
        phase: Phase::Done,
        at: "2026-10-10T00:00:00.000000000Z".into(),
        resource: "core.x".into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        recovery: None,
        token: None,
        reason: Some("x".repeat(1024)),
    }
    .to_line()
    .unwrap();
    let room = MAX_JOURNAL_BYTES - free - len;
    let mut bytes = fs::read(&journal).unwrap_or_default();
    bytes.extend(line.repeat(usize::try_from(room).unwrap() / line.len()));
    fs::write(&journal, &bytes).unwrap();
    let before = l.receipt_commands();
    for o in [l.preview(&[]), l.sync(&digest_of(&p), &[])] {
        assert_ne!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
        assert!(stderr(&o).contains("E_LIMIT_EXCEEDED"), "{}", stderr(&o));
    }
    assert_eq!(l.receipt_commands(), before);
    assert!(
        !l.fx
            .h("Library/LaunchAgents/org.example.agent.plist")
            .is_symlink()
    );
    assert!(!l.log().contains("launchctl bootstrap"), "{}", l.log());
}

#[test]
fn a_blocked_secret_and_a_secret_without_a_session_are_both_warned() {
    let l = Life::new();
    l.push_module(
        "\n[[resources]]\nid = \"token2\"\nkind = \"secret\"\nadapter = \"op-template/v1\"\ntarget = \".config/example2/second.env\"\n[resources.value]\ntemplate = \"files/personal.env.tpl\"\naccount = \"example-account\"\nmode = 0o600\n",
        &[],
        "add a second secret",
    );
    // A directory at the first secret's target blocks it; no session skips the second.
    fs::create_dir_all(l.fx.h(".config/example/personal.env")).unwrap();
    l.sign_in(false);
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    let warnings: Vec<&str> = s
        .lines()
        .filter(|l| l.starts_with("warning: secrets"))
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("secrets blocked: life.token")),
        "{s}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("secrets skipped, no 1Password session")
                && w.contains("life.token2")),
        "{s}"
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    let tail = &s[s.find("result: ok\n").unwrap()..];
    assert!(tail.contains("warning: secrets blocked: life.token"), "{s}");
    assert!(
        tail.contains("warning: secrets skipped, no 1Password session for example-account (op whoami exited 1): life.token2"),
        "{s}"
    );
    assert!(!l.log().contains("op inject"), "{}", l.log());
}

#[test]
fn a_definition_source_removed_upstream_gives_one_digest_and_blocks_only_services() {
    // The agent's definition is the owner's own HOME link into the checkout, not a resource.
    let l = Life::unpublished();
    let seed = &l.fx.upstream.seed;
    let link = LIFE.find("\n[[resources]]\nid = \"agent-plist\"").unwrap();
    let agent = LIFE.find("\n[[resources]]\nid = \"agent\"\n").unwrap();
    let unlinked = format!("{}{}", &LIFE[..link], &LIFE[agent..]);
    put(seed, "base/modules/life/module.toml", &unlinked);
    l.fx.upstream.push("the owner links the agent's definition");
    git(&l.fx.root, &["pull", "-q", "--ff-only", "origin", "main"]);
    let plist = l.fx.h("Library/LaunchAgents/org.example.agent.plist");
    fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(
        l.fx.root
            .join("base/modules/life/files/org.example.agent.plist"),
        &plist,
    )
    .unwrap();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // Upstream drops the definition's source while the agent stays declared, and adds a link.
    // The staged copy has no definition: the review must name the HOME path, not the run's
    // staging directory, or each preview gets another digest.
    fs::remove_file(seed.join("base/modules/life/files/org.example.agent.plist")).unwrap();
    put(
        seed,
        "base/modules/life/module.toml",
        &format!(
            "{unlinked}\n[[resources]]\nid = \"notes\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/life/notes\"\n[resources.value]\nsource = \"files/notes\"\n"
        ),
    );
    put(seed, "base/modules/life/files/notes", "notes\n");
    l.fx.upstream.push("drop the agent's definition");
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let again = l.preview(&[]);
    assert_eq!(code(&again), 0, "{}\n{}", stdout(&again), stderr(&again));
    assert_eq!(digest_of(&p), digest_of(&again), "{}", stdout(&p));
    let s = stdout(&p);
    assert!(!s.contains("staging"), "{s}");
    in_order(
        &s,
        &[
            "scope: services",
            "cannot read Library/LaunchAgents/org.example.agent.plist",
            // After publication the link dangles: the review sees it so too.
            "blocked: this scope does not run: life.agent@service E_CONFLICT, life.agent@service E_PROBE_FAILED",
            "warning: services blocked: life.agent@service E_CONFLICT, life.agent@service E_PROBE_FAILED",
        ],
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    in_order(
        &s,
        &[
            "  config: ok",
            "  services: blocked (life.agent@service E_CONFLICT, life.agent@service E_PROBE_FAILED)",
            "result: ok\nwarning: services blocked: life.agent@service E_CONFLICT, life.agent@service E_PROBE_FAILED",
        ],
    );
    assert!(l.fx.h(".config/life/notes").is_symlink());
}

/// `LIFE` with the agent's definition generated (`text-fragments/v1`) from its body and the
/// fragments in `.config/agent`.
fn generated_life() -> String {
    let generated = LIFE.replace(
        "kind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"Library/LaunchAgents/org.example.agent.plist\"\n[resources.value]\nsource = \"files/org.example.agent.plist\"",
        "kind = \"assemble\"\nadapter = \"text-fragments/v1\"\ntarget = \"Library/LaunchAgents/org.example.agent.plist\"\n[resources.value]\nbody = \"files/org.example.agent.plist\"\nfragments_dir = \".config/agent\"\npattern = \"*.frag\"\nseparator = \"\\n\"",
    );
    assert_ne!(generated, LIFE);
    generated
}

/// Pushes a generated definition of about 1.2 MiB: each part fits the source read limit, and
/// together they pass the 1 MiB definition limit.
fn push_large_definition(l: &Life) {
    let padding = |n: usize| format!("<!-- {} -->\n", "x".repeat(n));
    let seed = &l.fx.upstream.seed;
    put(seed, "base/modules/life/module.toml", &generated_life());
    put(
        seed,
        "base/modules/life/files/org.example.agent.plist",
        &AGENT_PLIST.replacen("<plist", &format!("{}<plist", padding(600 << 10)), 1),
    );
    l.fx.upstream.push("generate a large agent definition");
    let frag = l.fx.h(".config/agent/pad.frag");
    fs::create_dir_all(frag.parent().unwrap()).unwrap();
    fs::write(&frag, padding(600 << 10)).unwrap();
}

#[test]
fn a_generated_definition_over_1_mib_is_reviewed_as_the_published_file_is() {
    // The generated definition is larger than the adapter reads. The review must refuse it as
    // the services scope refuses the published file, not plan a start that goes stale after
    // the configuration scope wrote it.
    let l = Life::unpublished();
    push_large_definition(&l);
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    let s = stdout(&p);
    in_order(
        &s,
        &[
            "scope: services",
            "Library/LaunchAgents/org.example.agent.plist exceeds 1 MiB",
            // The definition is not published yet; its size blocks the start all the same.
            "warning: services blocked: life.agent@service E_CONFLICT, life.agent@service E_PROBE_FAILED",
        ],
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    in_order(
        &s,
        &[
            "  config: ok",
            "  services: blocked (life.agent@service E_CONFLICT, life.agent@service E_PROBE_FAILED)",
            "result: ok",
        ],
    );
    let plist = l.fx.h("Library/LaunchAgents/org.example.agent.plist");
    assert!(fs::metadata(&plist).unwrap().len() > 1 << 20);
    assert!(!l.log().contains("launchctl bootstrap"), "{}", l.log());
    // The services scope on its own gives the same result for the published file.
    let s = l.run(&["services", "--root", l.fx.root_arg(), "--preview"]);
    assert!(
        stdout(&s).contains(
            "blocked: E_PROBE_FAILED life.agent in base/modules/life/module.toml: the state of org.example.agent with launchd-agent/v1 is unknown: Library/LaunchAgents/org.example.agent.plist exceeds 1 MiB"
        ),
        "{}\n{}",
        stdout(&s),
        stderr(&s)
    );
}

#[test]
fn a_published_generated_definition_that_grows_past_1_mib_is_refused_in_the_review() {
    let l = Life::unpublished();
    put(
        &l.fx.upstream.seed,
        "base/modules/life/module.toml",
        &generated_life(),
    );
    l.fx.upstream.push("generate the agent definition");
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(l.log().contains("launchctl bootstrap"), "{}", l.log());
    push_large_definition(&l);
    let p = l.preview(&[]);
    assert_eq!(code(&p), 0, "{}\n{}", stdout(&p), stderr(&p));
    in_order(
        &stdout(&p),
        &[
            "scope: services",
            "Library/LaunchAgents/org.example.agent.plist exceeds 1 MiB",
            "warning: services blocked: life.agent@service E_PROBE_FAILED.",
        ],
    );
    let log = l.log();
    let o = l.sync(&digest_of(&p), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    in_order(
        &s,
        &[
            "  config: ok",
            "  services: blocked (life.agent@service E_PROBE_FAILED)",
            "result: ok",
        ],
    );
    let new = &l.log()[log.len()..];
    assert!(!new.contains("launchctl bootout"), "{new}");
}

#[test]
fn a_hardlinked_generator_input_the_update_keeps_is_refused_at_preview_as_apply_refuses_it() {
    let l = Life::unpublished();
    let seed = &l.fx.upstream.seed;
    put(seed, "base/modules/life/module.toml", &generated_life());
    l.fx.upstream.push("generate the agent definition");
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    // A second hard link to the body in the live checkout. The update does not change the body,
    // so publication keeps the file and its links, while the staged copy has one link.
    let body = "base/modules/life/files/org.example.agent.plist";
    fs::hard_link(l.fx.root.join(body), l.fake.path().join("body-link")).unwrap();
    put(
        seed,
        "base/modules/life/module.toml",
        &format!(
            "{}\n[[resources]]\nid = \"notes\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/life/notes\"\n[resources.value]\nsource = \"files/notes\"\n",
            generated_life()
        ),
    );
    put(seed, "base/modules/life/files/notes", "notes\n");
    l.fx.upstream.push("add a link");
    let refused = format!(
        "E_SOURCE_UNTRUSTED life.agent-plist (Library/LaunchAgents/org.example.agent.plist, value) in base/modules/life/module.toml: source {body} is not a regular file owned by you with one hard link"
    );
    let p = l.preview(&[]);
    assert_eq!(code(&p), 3, "{}\n{}", stdout(&p), stderr(&p));
    in_order(&stdout(&p), &["scope: config", &refused, "result: blocked"]);
    let o = l.sync(&digest_of(&p), &[]);
    assert_ne!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(!stdout(&o).contains("E_PLAN_STALE"), "{}", stdout(&o));
    assert!(!stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!l.fx.h(".config/life/notes").exists());
    // Live apply on the checkout as publication would leave it refuses the same source.
    git(&l.fx.root, &["pull", "-q", "--ff-only", "origin", "main"]);
    let a = l.run(&["apply", "--root", l.fx.root_arg(), "--preview"]);
    assert!(
        stdout(&a).contains(&refused),
        "{}\n{}",
        stdout(&a),
        stderr(&a)
    );
}

#[test]
fn a_hardlinked_generator_input_the_update_rewrites_is_accepted() {
    let l = Life::unpublished();
    let seed = &l.fx.upstream.seed;
    put(seed, "base/modules/life/module.toml", &generated_life());
    l.fx.upstream.push("generate the agent definition");
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let body = "base/modules/life/files/org.example.agent.plist";
    fs::hard_link(l.fx.root.join(body), l.fake.path().join("body-link")).unwrap();
    // The fast-forward writes the changed body as a new file with one link.
    put(
        seed,
        body,
        &AGENT_PLIST.replace("/usr/bin/true", "/usr/bin/false"),
    );
    l.fx.upstream.push("the agent runs another program");
    let flags = ["--restart", "life.agent", "--allow-disruptive", AGENT_OP];
    let o = l.approved(&flags);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    in_order(
        &stdout(&o),
        &["  config: ok", "  services: ok", "result: ok"],
    );
    assert!(
        fs::read_to_string(l.fx.h("Library/LaunchAgents/org.example.agent.plist"))
            .unwrap()
            .contains("/usr/bin/false")
    );
}

#[test]
fn a_link_and_its_source_removed_upstream_are_removed_by_a_plain_sync() {
    let l = Life::new();
    l.push_link();
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(l.fx.h(".config/life/notes").is_symlink());
    // Upstream deletes the link resource and its source file. The review sees the source in
    // --root, which publication then deletes: the removal must not go stale on that.
    let seed = &l.fx.upstream.seed;
    fs::remove_file(seed.join("base/modules/life/files/notes")).unwrap();
    put(seed, "base/modules/life/module.toml", LIFE);
    l.fx.upstream.push("drop the notes link");
    let allow = ["--allow-disruptive", "remove@life.notes@.config/life/notes"];
    let o = l.approved(&allow);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let s = stdout(&o);
    assert!(!s.contains("E_PLAN_STALE"), "{s}");
    in_order(&s, &["  config: ok", "result: ok"]);
    assert!(fs::symlink_metadata(l.fx.h(".config/life/notes")).is_err());
}

/// A settings merge whose fragment directory holds the file a link resource publishes.
const MERGE_FRAGMENT: &str = r#"
[[resources]]
id = "settings"
kind = "merge"
adapter = "claude-settings/v1"
target = ".claude/settings.json"
[resources.value]
fragments_dir = ".claude/settings.d"
pattern = "*.json"
[resources.value.managed]
outputStyle = "STE"

[[resources]]
id = "frag"
kind = "link"
adapter = "fs-link/v1"
target = ".claude/settings.d/10-local.json"
[resources.value]
source = "files/frag.json"
"#;

#[test]
fn a_hardlinked_merge_fragment_source_the_update_keeps_is_refused_at_preview_as_apply_refuses_it() {
    let l = Life::new();
    l.push_module(
        MERGE_FRAGMENT,
        &[("frag.json", r#"{"fromFragment":true}"#)],
        "merge a fragment",
    );
    let o = l.approved(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(
        fs::read_to_string(l.fx.h(".claude/settings.json"))
            .unwrap()
            .contains("fromFragment")
    );
    // A second hard link to the fragment's source in the live checkout, which the update keeps.
    let src = "base/modules/life/files/frag.json";
    fs::hard_link(l.fx.root.join(src), l.fake.path().join("frag-link")).unwrap();
    l.push_module(
        &format!(
            "{MERGE_FRAGMENT}\n[[resources]]\nid = \"notes\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/life/notes\"\n[resources.value]\nsource = \"files/notes\"\n"
        ),
        &[("notes", "notes\n")],
        "add a link",
    );
    let refused = "the source of fragment .claude/settings.d/10-local.json is not a regular file owned by you with one hard link";
    let p = l.preview(&[]);
    assert_eq!(code(&p), 3, "{}\n{}", stdout(&p), stderr(&p));
    in_order(
        &stdout(&p),
        &[
            "scope: config",
            "E_SOURCE_UNTRUSTED life.settings",
            refused,
            "result: blocked",
        ],
    );
    let o = l.sync(&digest_of(&p), &[]);
    assert_ne!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    assert!(!stdout(&o).contains("E_PLAN_STALE"), "{}", stdout(&o));
    assert!(!stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!l.fx.h(".config/life/notes").exists());
    // Live apply on the checkout as publication would leave it refuses the same source.
    git(&l.fx.root, &["pull", "-q", "--ff-only", "origin", "main"]);
    let a = l.run(&["apply", "--root", l.fx.root_arg(), "--preview"]);
    in_order(&stdout(&a), &["E_SOURCE_UNTRUSTED life.settings", refused]);
}
