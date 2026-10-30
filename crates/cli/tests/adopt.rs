//! `adopt`: a trial's configuration is promoted into a module, its package into the Brewfile,
//! and the live files become dfm links (DFM-33, docs/adopt.md). Runs the isolated binary with
//! the `brew` and `gitleaks` look-alikes and the system `git`. No real provider runs.

mod common;

use common::providers::{FAKE_BREW, FAKE_GITLEAKS, FAKE_MISE};
use common::{TempDir, code, is_root, skip, text};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");
const BREW: &str = "homebrew/v1";
const MANIFEST: &str = "base/modules/core/module.toml";
const REPO_FILE: &str = "base/modules/core/home/.config/tuicr/config.toml";
const LIVE: &str = ".config/tuicr/config.toml";
const TUICR: &str = "theme = \"dark\"\n";

/// A bundle that also declares `work`, which the profile does not select.
const BUNDLE: &str = r#"schema = 1
id = "t-base"
core = "modules/core/module.toml"

[modules]
extra = "modules/extra/module.toml"
work = "modules/work/module.toml"
"#;

const WORK: &str = "schema = 1\nid = \"work\"\nplatforms = [\"macos\", \"linux\"]\nrequires = []\n";

/// Paths and bytes of a directory tree.
type Files = BTreeMap<String, Vec<u8>>;

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
    fake: TempDir,
}

fn stderr(o: &Output) -> String {
    text(&o.stderr)
}

fn out(o: &Output) -> String {
    text(&o.stdout)
}

fn digest_of(o: &Output) -> String {
    out(o)
        .lines()
        .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no digest in\n{}\n{}", out(o), stderr(o)))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("/usr/bin/git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", stderr(&o));
    out(&o)
}

/// Every file below `dir` except `.git`, with its bytes; a link is kept as its target text.
fn tree(dir: &Path) -> Files {
    fn go(base: &Path, dir: &Path, map: &mut Files) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.file_name().unwrap() == ".git" {
                continue;
            }
            let m = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            if m.file_type().is_symlink() {
                let t = fs::read_link(&p).unwrap();
                map.insert(rel, format!("-> {}", t.display()).into_bytes());
            } else if m.is_dir() {
                go(base, &p, map);
            } else {
                map.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut map = BTreeMap::new();
    go(dir, dir, &mut map);
    map
}

impl Fx {
    /// A bound machine over a git checkout with a committed `Brewfile`, and the `tuicr` trial
    /// recorded: brew installed it and `~/.config/tuicr` was absent.
    fn new() -> Fx {
        let fx = Fx::bare();
        fx.trial_tuicr(&[]);
        fx
    }

    fn bare() -> Fx {
        let src = TempDir::new("adopt-src");
        let fx = Fx {
            home: TempDir::new("adopt-home"),
            root: src.path().join("root"),
            _src: src,
            fake: TempDir::new("adopt-providers"),
        };
        fx.put("base/bundle.toml", BUNDLE);
        fx.put(MANIFEST, common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/extra/module.toml", common::EXTRA);
        fx.put("base/modules/extra/files/tmux.conf", "set -g mouse on\n");
        fx.put("base/modules/extra/files/zshrc", "export A=1\n");
        fx.put("base/modules/work/module.toml", WORK);
        fx.put("profiles/t-01.toml", common::PROFILE);
        fx.put("Brewfile", "brew \"git\"\n");
        git(&fx.root, &["init", "-q", "."]);
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "-q", "-m", "init"]);
        fs::create_dir_all(fx.bin()).unwrap();
        for (name, body) in [
            ("brew", FAKE_BREW),
            ("mise", FAKE_MISE),
            ("gitleaks", FAKE_GITLEAKS),
        ] {
            let p = fx.bin().join(name);
            fs::write(&p, body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(fx.fake.path().join("official-brew"), "tuicr\n").unwrap();
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

    fn trial_tuicr(&self, extra: &[&str]) {
        let mut args = vec!["trial", "tuicr", "--provider", BREW];
        args.extend_from_slice(extra);
        let mut preview = args.clone();
        preview.push("--preview");
        let d = digest_of(&self.run(&preview));
        args.extend_from_slice(&["--yes-plan", &d]);
        let o = self.run(&args);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    }

    fn bin(&self) -> PathBuf {
        self.fake.path().join("bin")
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn live(&self, rel: &str, content: &str) {
        let p = self.h(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.bin().display());
        Command::new(BIN)
            .env_clear()
            .env("DFM_PROVIDER_PATH", path)
            .envs(env.iter().copied())
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }

    fn adopt(&self, args: &[&str]) -> Output {
        let mut v = vec!["adopt", "trial-tuicr"];
        v.extend_from_slice(args);
        self.run(&v)
    }

    /// Preview, then approve the digest the preview printed.
    fn adopt_approved(&self, module: &str) -> Output {
        let o = self.adopt(&["--module", module, "--preview"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
        self.adopt(&["--module", module, "--yes-plan", &digest_of(&o)])
    }

    fn state(&self) -> PathBuf {
        self.home.path().join(".local/state/dotfiles-manager/state")
    }

    fn entries(&self, sub: &str) -> Vec<PathBuf> {
        let Ok(dir) = fs::read_dir(self.state().join(sub)) else {
            return Vec::new();
        };
        let mut v: Vec<PathBuf> = dir.map(|e| e.unwrap().path()).collect();
        v.sort();
        v
    }

    fn json(&self, path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn record(&self) -> serde_json::Value {
        self.json(&self.state().join("trials/trial-tuicr.json"))
    }

    /// What a refused run must leave unchanged: the repo (less `.git`), the live tuicr
    /// directory, the trial record, the journal and the receipts.
    fn snapshot(&self) -> (Files, Files, String) {
        let state = format!(
            "{:?}{:?}{:?}",
            fs::read(self.state().join("trials/trial-tuicr.json")).ok(),
            fs::read(self.state().join("journal.jsonl")).ok(),
            self.entries("receipts"),
        );
        let tuicr = self.h(".config/tuicr");
        let live = if tuicr.exists() {
            tree(&tuicr)
        } else {
            BTreeMap::new()
        };
        (tree(&self.root), live, state)
    }
}

fn refused(o: &Output, needle: &str) {
    assert_eq!(code(o), 3, "{}\n{}", stderr(o), out(o));
    assert!(stderr(o).contains(needle), "{}\n{}", stderr(o), out(o));
}

#[test]
fn adopts_the_tuicr_shape() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let t = out(&o);
    assert!(t.contains("adopt: trial-tuicr module=core"), "{t}");
    assert!(
        t.contains(&format!(
            "candidate: {} -> {REPO_FILE} id=config-tuicr-config-toml",
            fx.h(LIVE).display()
        )),
        "{t}"
    );
    assert!(t.contains("brewfile: Brewfile += brew \"tuicr\""), "{t}");
    assert!(
        t.contains("unknown: services paths-outside-baseline provider-caches"),
        "{t}"
    );
    let o = fx.adopt(&["--module", "core", "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let t = out(&o);
    assert!(t.contains(&format!("changed: {REPO_FILE}")), "{t}");
    assert!(t.contains(&format!("changed: {MANIFEST}")), "{t}");
    assert!(t.contains("changed: Brewfile"), "{t}");
    assert!(t.ends_with("result: ok\n"), "{t}");
    // The repo file holds the live bytes; the manifest and the Brewfile only grew.
    assert_eq!(fs::read_to_string(fx.root.join(REPO_FILE)).unwrap(), TUICR);
    let manifest = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
    assert!(manifest.starts_with(common::CORE), "{manifest}");
    assert!(
        manifest.ends_with(
            "\n[[resources]]\nid = \"config-tuicr-config-toml\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/tuicr/config.toml\"\n[resources.value]\nsource = \"home/.config/tuicr/config.toml\"\n"
        ),
        "{manifest}"
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("Brewfile")).unwrap(),
        "brew \"git\"\nbrew \"tuicr\"\n"
    );
    // The live path is a link to the repo file.
    let link = fs::read_link(fx.h(LIVE)).unwrap();
    assert_eq!(link, fx.root.join(REPO_FILE));
    assert_eq!(fs::read_to_string(fx.h(LIVE)).unwrap(), TUICR);
    // The ledger owns it.
    let ledger = fs::read_to_string(fx.state().join("ledger.json")).unwrap();
    assert!(ledger.contains("config-tuicr-config-toml"), "{ledger}");
    let o = fx.run(&["doctor", "--root", fx.root.to_str().unwrap()]);
    assert!(
        out(&o).contains("1 resources, 1 owned outputs"),
        "{}",
        out(&o)
    );
    // The trial ended.
    let r = fx.record();
    assert_eq!(r["schema"], 3);
    assert_eq!(r["adopted"]["module"], "core");
    // The receipt.
    let receipts = fx.entries("receipts");
    let rc = fx.json(receipts.last().unwrap());
    assert_eq!(rc["command"], "adopt");
    assert_eq!(rc["result"]["outcome"], "ok");
    // Three changes, nothing committed.
    let status = git(&fx.root, &["status", "--porcelain=v1", "-uall"]);
    let mut lines: Vec<&str> = status.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            " M Brewfile".to_string(),
            format!(" M {MANIFEST}"),
            format!("?? {REPO_FILE}")
        ],
        "{status}"
    );
    assert_eq!(git(&fx.root, &["rev-list", "--count", "HEAD"]).trim(), "1");
    // The journal: the adopt intent carries what a later run needs, and is closed.
    let journal = fs::read_to_string(fx.state().join("journal.jsonl")).unwrap();
    let adopt: Vec<serde_json::Value> = journal
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["op"] == "adopt")
        .collect();
    assert_eq!(adopt.len(), 2, "{journal}");
    assert_eq!(
        (adopt[0]["phase"].as_str(), adopt[1]["phase"].as_str()),
        (Some("intent"), Some("done"))
    );
    let token: serde_json::Value =
        serde_json::from_str(adopt[0]["token"].as_str().unwrap()).unwrap();
    assert_eq!(token["module"], "core");
    assert_eq!(token["manifest"]["path"], MANIFEST);
    assert_eq!(token["brewfile"]["path"], "Brewfile");
    assert_eq!(token["files"][0]["repo"], REPO_FILE);
}

#[test]
fn apply_after_adopt_plans_the_links_as_current() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let o = fx.adopt_approved("core");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let o = fx.run(&["apply", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let t = out(&o);
    assert!(
        t.lines()
            .any(|l| l.contains("current") && l.contains("config-tuicr-config-toml@")),
        "{t}"
    );
    assert!(!t.contains("remove@config-tuicr"), "{t}");
    assert!(!t.contains("blocked"), "{t}");
}

#[test]
fn apply_still_refuses_an_equal_regular_file() {
    let fx = Fx::new();
    // core's `ssh` links `.ssh/config` to `files/ssh_config`; a regular file with the same bytes
    // is still a conflict for apply.
    fx.live(".ssh/config", "Host *\n");
    let o = fx.run(&["apply", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), out(&o));
    assert!(
        out(&o).contains("is a regular file, not the link"),
        "{}",
        out(&o)
    );
}

#[test]
fn a_secret_is_refused_with_zero_writes() {
    let fx = Fx::new();
    fx.live(LIVE, "token = \"FAKE_SECRET\"\n");
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "core", "--preview"]);
    refused(&o, &format!("{}", fx.h(LIVE).display()));
    assert!(!stderr(&o).contains("FAKE_SECRET"), "{}", stderr(&o));
    assert!(!out(&o).contains("FAKE_SECRET"), "{}", out(&o));
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn gitleaks_missing_blocks() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    fs::remove_file(fx.bin().join("gitleaks")).unwrap();
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "E_PROBE_FAILED",
    );
    // A failing scan blocks the same way.
    fs::write(fx.bin().join("gitleaks"), FAKE_GITLEAKS).unwrap();
    fs::set_permissions(fx.bin().join("gitleaks"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(fx.fake.path().join("gitleaks-fail"), "").unwrap();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "E_PROBE_FAILED",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn an_unselected_module_names_the_profile_line() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "work", "--preview"]);
    refused(&o, "profiles/t-01.toml");
    assert!(
        stderr(&o).contains("modules = [\"extra\"]"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn an_undeclared_module_is_refused() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "nosuch", "--preview"]);
    refused(&o, "does not declare a module nosuch");
    assert!(stderr(&o).contains("profiles/t-01.toml"), "{}", stderr(&o));
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_target_owned_by_core_is_refused() {
    let fx = Fx::bare();
    fx.trial_tuicr(&["--config", ".ssh/config"]);
    fx.live(".ssh/config", "Host other\n");
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "is already claimed by resource core.ssh",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_missing_root_is_refused_before_any_write() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let before = fx.snapshot();
    let gone = fx._src.path().join("gone");
    let o = fx.adopt(&[
        "--module",
        "core",
        "--root",
        gone.to_str().unwrap(),
        "--preview",
    ]);
    // The shared root check of plan and apply: a root that does not exist is E_REPORT_UNAVAILABLE.
    assert_eq!(code(&o), 2, "{}\n{}", stderr(&o), out(&o));
    assert!(
        stderr(&o).contains("E_REPORT_UNAVAILABLE"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_root_that_is_not_writable_is_refused_before_any_write() {
    if is_root() {
        skip("uid 0 can write a 0555 directory");
        return;
    }
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let dir = fx.root.join("base/modules/core");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "core", "--preview"]);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    refused(&o, "is not writable");
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_malformed_manifest_is_refused_before_any_write() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    fx.put(MANIFEST, "schema = 1\nid = \"core\"\n[[resources]\n");
    git(&fx.root, &["commit", "-q", "-am", "broken"]);
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), out(&o));
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_live_file_changed_after_the_preview_is_stale() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let o = fx.adopt(&["--module", "core", "--preview"]);
    let d = digest_of(&o);
    fx.live(LIVE, "theme = \"light\"\n");
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--yes-plan", &d]),
        "E_PLAN_STALE",
    );
    assert_eq!(fx.snapshot(), before);
    assert!(!fx.root.join(REPO_FILE).exists());
}

#[test]
fn a_dirty_manifest_is_refused() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let manifest = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
    fx.put(MANIFEST, &format!("{manifest}# local edit\n"));
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "uncommitted changes",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_gitignored_brewfile_is_refused() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    fx.put("Brewfile.core", "");
    fx.put(".gitignore", "Brewfile.core\n");
    git(&fx.root, &["add", ".gitignore"]);
    git(&fx.root, &["commit", "-q", "-m", "ignore"]);
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "Brewfile.core is not inside the git checkout",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn an_existing_repo_file_is_refused() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    fx.put(REPO_FILE, "other\n");
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "already exists",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_duplicate_resource_id_among_the_candidates_is_refused() {
    let fx = Fx::new();
    fx.live(".config/tuicr/a.b", "x\n");
    fx.live(".config/tuicr/a/b", "y\n");
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "both give the resource id config-tuicr-a-b",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn an_unchanged_preexisting_file_is_not_a_candidate() {
    let fx = Fx::bare();
    fx.live(".config/tuicr/a.toml", "a = 1\n");
    fx.live(".config/tuicr/b.toml", "b = 1\n");
    fx.trial_tuicr(&[
        "--config",
        ".config/tuicr/a.toml",
        "--config",
        ".config/tuicr/b.toml",
    ]);
    fx.live(".config/tuicr/b.toml", "b = 2\n");
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let t = out(&o);
    assert!(t.contains("config-tuicr-b-toml"), "{t}");
    assert!(!t.contains("config-tuicr-a-toml"), "{t}");
}

#[test]
fn a_mise_trial_gets_no_brewfile_line() {
    let fx = Fx::bare();
    let o = fx.run(&["trial", "tuicr", "--provider", "mise/v1", "--preview"]);
    let d = digest_of(&o);
    let o = fx.run(&["trial", "tuicr", "--provider", "mise/v1", "--yes-plan", &d]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    fx.live(LIVE, TUICR);
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert!(
        out(&o).contains("brewfile: none (the trial package is mise/v1"),
        "{}",
        out(&o)
    );
}

#[test]
fn adopting_twice_is_refused() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let o = fx.adopt_approved("core");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "already adopted",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn nothing_found_is_refused() {
    let fx = Fx::new();
    refused(
        &fx.adopt(&["--module", "core", "--preview"]),
        "nothing to adopt",
    );
}
