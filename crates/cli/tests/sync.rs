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

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");

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

    fn grants(&self) -> String {
        fs::read_to_string(self.h(".local/state/dotfiles-manager/state/grants.json"))
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
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
    assert!(stderr(&o).contains("nothing was fetched"), "{}", stderr(&o));
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
    // The explicit grant is written under the lock, then the fetch and the plan happen.
    fx.trust(&fx.origin());
    assert!(fx.grants().contains(&fx.origin()), "{}", fx.grants());
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
fn declined_approval_changes_nothing() {
    let fx = Fx::new();
    let old = fx.head();
    put(
        &fx.upstream.seed,
        "base/modules/extra/files/zshrc",
        "export A=3\n",
    );
    fx.upstream.push("upstream zshrc");
    let o = fx.run(&["sync", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 3, "{}\n{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
    assert!(stdout(&o).contains("plan digest: "), "{}", stdout(&o));
    assert!(stdout(&o).contains("result: blocked"), "{}", stdout(&o));
    assert_eq!(fx.head(), old);
    assert!(!fx.h(".zshrc").exists());
    assert_eq!(fx.receipts(), 0);
    assert!(
        fx.staging_leftovers().is_empty(),
        "{:?}",
        fx.staging_leftovers()
    );
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
