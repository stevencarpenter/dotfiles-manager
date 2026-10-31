//! `adopt`: a trial's configuration is promoted into a module, its package into the Brewfile,
//! and the live files become dfm links (DFM-33, docs/adopt.md). Runs the isolated binary with
//! the `brew` and `gitleaks` look-alikes and the system `git`. No real provider runs.

mod common;

use common::providers::{FAKE_BREW, FAKE_GITLEAKS, FAKE_MISE};
use common::{TempDir, code, is_root, skip, text};
use std::collections::{BTreeMap, BTreeSet};
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
        .filter(|e| e["op"] == "trial-adopt")
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

#[test]
fn a_third_party_tap_keeps_its_name_in_the_brewfile() {
    let fx = Fx::bare();
    // tuicr is installed from a third-party tap, so the trial finds it and installs nothing.
    let keg = fx.fake.path().join("Cellar/tuicr/1.0.0");
    fs::create_dir_all(&keg).unwrap();
    fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        r#"{"source":{"tap":"owner/tap"}}"#,
    )
    .unwrap();
    let o = fx.run(&["trial", "owner/tap/tuicr", "--provider", BREW, "--preview"]);
    let d = digest_of(&o);
    let o = fx.run(&[
        "trial",
        "owner/tap/tuicr",
        "--provider",
        BREW,
        "--yes-plan",
        &d,
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    fx.live(LIVE, TUICR);
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert!(
        out(&o).contains("brewfile: Brewfile += brew \"owner/tap/tuicr\""),
        "{}",
        out(&o)
    );
}

#[test]
fn an_official_package_listed_with_its_tap_gets_no_second_line() {
    let fx = Fx::new();
    fx.put("Brewfile", "brew \"homebrew/core/tuicr\"\n");
    git(&fx.root, &["commit", "-q", "-am", "tap spelling"]);
    fx.live(LIVE, TUICR);
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert!(
        out(&o).contains("brewfile: none (Brewfile already lists tuicr)"),
        "{}",
        out(&o)
    );
}

#[test]
fn a_cask_line_of_the_same_name_does_not_hide_the_formula_line() {
    let fx = Fx::new();
    fx.put("Brewfile", "cask \"tuicr\"\n");
    git(&fx.root, &["commit", "-q", "-am", "cask spelling"]);
    fx.live(LIVE, TUICR);
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert!(
        out(&o).contains("brewfile: Brewfile += brew \"tuicr\""),
        "{}",
        out(&o)
    );
}

#[test]
fn a_manifest_edited_after_the_preview_is_stale() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let d = digest_of(&fx.adopt(&["--module", "core", "--preview"]));
    // Committed, so the clean-checkout check passes and only the digest can catch it.
    let manifest = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
    fx.put(MANIFEST, &format!("{manifest}# reviewed\n"));
    git(&fx.root, &["commit", "-q", "-am", "edit"]);
    let before = fx.snapshot();
    refused(
        &fx.adopt(&["--module", "core", "--yes-plan", &d]),
        "E_PLAN_STALE",
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_change_between_the_check_and_the_lock_is_caught_under_the_lock() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let d = digest_of(&fx.adopt(&["--module", "core", "--preview"]));
    // The run parks after its pre-lock check; the live file changes meanwhile.
    let gate = fx.fake.path().join("go");
    let parked = Command::new(BIN)
        .env_clear()
        .env(
            "DFM_PROVIDER_PATH",
            format!("{}:/usr/bin:/bin", fx.bin().display()),
        )
        .env(
            "DFM_FAILPOINT",
            format!("adopt-before-lock:wait-{}", gate.display()),
        )
        .arg("--home")
        .arg(fx.home.path())
        .args(["adopt", "trial-tuicr", "--module", "core", "--yes-plan", &d])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reached = gate.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < end,
            "the failpoint was not reached"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fx.live(LIVE, "theme = \"light\"\n");
    let before = fx.snapshot();
    fs::write(&gate, "").unwrap();
    let o = parked.wait_with_output().unwrap();
    refused(&o, "E_PLAN_STALE");
    assert_eq!(fx.snapshot(), before);
    assert!(!fx.root.join(REPO_FILE).exists());
}

impl Fx {
    /// The ledger resources that own the live path `rel`.
    fn owners(&self, rel: &str) -> Vec<String> {
        let Ok(bytes) = fs::read(self.state().join("ledger.json")) else {
            return Vec::new();
        };
        let ledger: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        ledger["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| {
                r["owned_outputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|o| o["path"].as_str().unwrap().ends_with(rel))
            })
            .map(|r| r["resource"].as_str().unwrap().to_string())
            .collect()
    }

    /// The `trial-adopt` journal entries, oldest first.
    fn adopt_journal(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.state().join("journal.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["op"] == "trial-adopt")
            .collect()
    }

    /// How often each repo path appears as a written operation across every receipt.
    fn receipt_writes(&self) -> BTreeMap<String, usize> {
        let mut n = BTreeMap::new();
        for r in self.entries("receipts") {
            for o in self.json(&r)["operations"].as_array().unwrap() {
                let res = o["resource"].as_str().unwrap();
                if [MANIFEST, "Brewfile", REPO_FILE].contains(&res) {
                    *n.entry(res.to_string()).or_insert(0) += 1;
                }
            }
        }
        n
    }

    /// The repo, the live tuicr paths and the ledger owner after a complete adoption, as
    /// `adopts_the_tuicr_shape` leaves them, with nothing written twice.
    fn assert_adopted(&self, reference: &Files) {
        assert_eq!(&tree(&self.root), reference);
        assert_eq!(
            fs::read_link(self.h(LIVE)).unwrap(),
            self.root.join(REPO_FILE)
        );
        let manifest = fs::read_to_string(self.root.join(MANIFEST)).unwrap();
        assert_eq!(
            manifest
                .matches("id = \"config-tuicr-config-toml\"")
                .count(),
            1
        );
        let brewfile = fs::read_to_string(self.root.join("Brewfile")).unwrap();
        assert_eq!(brewfile.matches("brew \"tuicr\"").count(), 1);
        assert_eq!(self.owners(LIVE), ["core.config-tuicr-config-toml"]);
        // Each repo path is written by one run at most; a killed run leaves no receipt.
        assert!(
            self.receipt_writes().values().all(|n| *n == 1),
            "{:?}",
            self.receipt_writes()
        );
        let r = self.record();
        assert_eq!(
            (r["schema"].as_i64(), r["adopted"]["module"].as_str()),
            (Some(3), Some("core"))
        );
        let last = self
            .adopt_journal()
            .into_iter()
            .rfind(|e| e["phase"] == "intent")
            .unwrap();
        assert_eq!(r["adopted"]["op_id"], last["op_id"]);
        // Every interrupted run's intent is closed: state and doctor report nothing open.
        let o = self.run(&["state", "check"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
        let o = self.run(&["doctor", "--root", self.root.to_str().unwrap()]);
        assert!(!out(&o).contains("trial-adopt"), "{}", out(&o));
        let status = git(&self.root, &["status", "--porcelain=v1", "-uall"]);
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
    }

    /// Every path below the root (less `.git`) and the live tuicr directory.
    fn paths(&self) -> BTreeSet<String> {
        let (repo, live, _) = self.snapshot();
        repo.into_keys()
            .chain(live.into_keys().map(|k| format!("~/{k}")))
            .collect()
    }

    fn ino(&self, p: &Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(p).ok().map(|m| m.ino())
    }
}

/// The repo tree of a complete, uninterrupted adoption.
fn reference_tree() -> Files {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let o = fx.adopt_approved("core");
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    tree(&fx.root)
}

/// Runs the approved adoption with `DFM_FAILPOINT=<spec>` and checks it did not complete.
fn interrupted(fx: &Fx, spec: &str) {
    let d = digest_of(&fx.adopt(&["--module", "core", "--preview"]));
    let o = fx.run_env(
        &[("DFM_FAILPOINT", spec)],
        &["adopt", "trial-tuicr", "--module", "core", "--yes-plan", &d],
    );
    // An abort ends the process by a signal, with no exit code.
    if let Some(c) = o.status.code() {
        assert!(
            matches!(c, 1 | 3),
            "{spec}: exit {c}\n{}\n{}",
            stderr(&o),
            out(&o)
        );
    }
    assert_eq!(fx.record()["schema"], 2, "{spec}");
}

/// A refusal with `E_CONFLICT` that names `path`.
fn conflict_on(o: &Output, path: &str) {
    refused(o, "E_CONFLICT");
    assert!(stderr(o).contains(path), "{}", stderr(o));
}

#[test]
fn a_second_run_completes_after_a_stop_at_each_boundary() {
    let reference = reference_tree();
    let block_id = "id = \"config-tuicr-config-toml\"";
    for (stage, point) in [
        "adopt-after-copy",
        "adopt-after-manifest",
        "adopt-after-brewfile",
        "adopt-after-link",
    ]
    .into_iter()
    .enumerate()
    {
        for action in ["error", "abort"] {
            let spec = format!("{point}:{action}");
            let fx = Fx::new();
            fx.live(LIVE, TUICR);
            let start = fx.paths();
            interrupted(&fx, &spec);
            let mid = fx.paths();
            assert!(mid.is_superset(&start), "{spec}: {start:?} -> {mid:?}");
            // The state where the run stopped: each boundary adds one write.
            let manifest = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
            let brewfile = fs::read_to_string(fx.root.join("Brewfile")).unwrap();
            let is_link = fs::read_link(fx.h(LIVE)).is_ok();
            assert_eq!(
                fs::read_to_string(fx.root.join(REPO_FILE)).unwrap(),
                TUICR,
                "{spec}"
            );
            assert_eq!(manifest.contains(block_id), stage >= 1, "{spec}");
            assert_eq!(brewfile.contains("brew \"tuicr\""), stage >= 2, "{spec}");
            assert_eq!(is_link, stage >= 3, "{spec}");
            let copy_ino = fx.ino(&fx.root.join(REPO_FILE));

            let o = fx.adopt(&["--module", "core", "--preview"]);
            assert_eq!(code(&o), 0, "{spec}\n{}\n{}", stderr(&o), out(&o));
            let t = out(&o);
            assert!(t.contains("resume: "), "{spec}\n{t}");
            let line = |p: &str| t.lines().find(|l| l.starts_with(p)).unwrap().to_string();
            let kept = "kept from the interrupted run";
            assert_eq!(
                line("candidate: ").contains("(repo copy kept"),
                stage < 3,
                "{spec}\n{t}"
            );
            assert_eq!(
                line("candidate: ").contains("(link kept"),
                stage >= 3,
                "{spec}\n{t}"
            );
            assert_eq!(line("manifest: ").contains(kept), stage >= 1, "{spec}\n{t}");
            assert_eq!(line("brewfile: ").contains(kept), stage >= 2, "{spec}\n{t}");

            let o = fx.adopt(&["--module", "core", "--yes-plan", &digest_of(&o)]);
            assert_eq!(code(&o), 0, "{spec}\n{}\n{}", stderr(&o), out(&o));
            assert!(fx.paths().is_superset(&mid), "{spec}");
            // The copy is the one the first run wrote, and the manifest only grew.
            assert_eq!(fx.ino(&fx.root.join(REPO_FILE)), copy_ino, "{spec}");
            let end = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
            assert!(end.starts_with(&manifest), "{spec}");
            fx.assert_adopted(&reference);
            // A run stopped by an error writes its receipt: then every write is in exactly one.
            if action == "error" {
                let want = [(REPO_FILE, 0), (MANIFEST, 1), ("Brewfile", 2)]
                    .into_iter()
                    .map(|(p, _)| (p.to_string(), 1))
                    .collect::<BTreeMap<_, _>>();
                assert_eq!(fx.receipt_writes(), want, "{spec}");
            }
        }
    }
}

#[test]
fn a_third_run_completes_after_two_stops_in_a_row() {
    let reference = reference_tree();
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-copy:abort");
    interrupted(&fx, "adopt-after-manifest:abort");
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let o = fx.adopt(&["--module", "core", "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    fx.assert_adopted(&reference);
    // Both stopped runs are closed as superseded by the run that completed.
    let done = fx.record()["adopted"]["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    let journal = fx.adopt_journal();
    let intents: Vec<&str> = journal
        .iter()
        .filter(|e| e["phase"] == "intent")
        .map(|e| e["op_id"].as_str().unwrap())
        .collect();
    assert_eq!(intents.len(), 3, "{journal:?}");
    for id in &intents[..2] {
        let closed: Vec<_> = journal
            .iter()
            .filter(|e| e["op_id"] == *id && e["phase"] != "intent")
            .collect();
        assert_eq!(closed.len(), 1, "{id}: {journal:?}");
        assert_eq!(closed[0]["phase"], "failed", "{id}");
        assert_eq!(
            closed[0]["reason"].as_str().unwrap(),
            format!("superseded by {done}"),
            "{id}"
        );
    }
}

#[test]
fn a_second_run_completes_after_state_recover_left_the_link_unowned() {
    let reference = reference_tree();
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "link-after-rename:abort");
    // Recovery alone closes the engine's intent without recording the owner.
    let o = fx.run(&["state", "recover"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert!(fs::read_link(fx.h(LIVE)).is_ok());
    assert!(fx.owners(LIVE).is_empty());
    let link_ino = fx.ino(&fx.h(LIVE));
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let o = fx.adopt(&["--module", "core", "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert_eq!(fx.ino(&fx.h(LIVE)), link_ino);
    fx.assert_adopted(&reference);
}

#[test]
fn a_second_run_completes_after_a_stop_in_the_link_rename_without_recover() {
    let reference = reference_tree();
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "link-after-rename:abort");
    assert!(fs::read_link(fx.h(LIVE)).is_ok());
    let link_ino = fx.ino(&fx.h(LIVE));
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let o = fx.adopt(&["--module", "core", "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert_eq!(fx.ino(&fx.h(LIVE)), link_ino);
    fx.assert_adopted(&reference);
}

#[test]
fn two_candidates_complete_after_a_stop_between_their_links() {
    const KEYS: &str = ".config/tuicr/keys.toml";
    const KEYS_REPO: &str = "base/modules/core/home/.config/tuicr/keys.toml";
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    fx.live(KEYS, "quit = \"q\"\n");
    interrupted(&fx, "link-after-rename:abort");
    let linked = [LIVE, KEYS]
        .iter()
        .filter(|p| fs::read_link(fx.h(p)).is_ok())
        .count();
    assert_eq!(linked, 1, "one link is in place, the other file is regular");
    let o = fx.adopt(&["--module", "core", "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    let o = fx.adopt(&["--module", "core", "--yes-plan", &digest_of(&o)]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    for (live, repo, id) in [
        (LIVE, REPO_FILE, "config-tuicr-config-toml"),
        (KEYS, KEYS_REPO, "config-tuicr-keys-toml"),
    ] {
        assert_eq!(fs::read_link(fx.h(live)).unwrap(), fx.root.join(repo));
        assert_eq!(fx.owners(live), [format!("core.{id}")]);
        let manifest = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
        assert_eq!(manifest.matches(&format!("id = \"{id}\"")).count(), 1);
    }
    assert_eq!(
        fs::read_to_string(fx.root.join(KEYS_REPO)).unwrap(),
        "quit = \"q\"\n"
    );
    assert_eq!(fx.record()["schema"], 3);
}

#[test]
fn a_foreign_edit_during_recovery_is_a_conflict() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-manifest:error");
    let manifest = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
    fx.put(MANIFEST, &format!("{manifest}# hand edit\n"));
    let before = fx.snapshot();
    conflict_on(&fx.adopt(&["--module", "core", "--preview"]), MANIFEST);
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_torn_manifest_append_is_refused() {
    // A stop inside the append leaves a prefix of the planned block; it is never recovered. A
    // prefix that still parses is not the recorded append; one that does not parse is refused
    // by the resolver first.
    for (tail, code_name) in [
        ("\n", "E_CONFLICT"),
        ("\n[[resources]]\nid = \"config-tuicr", "E_MANIFEST_INVALID"),
    ] {
        let fx = Fx::new();
        fx.live(LIVE, TUICR);
        interrupted(&fx, "adopt-after-copy:abort");
        let manifest = fs::read_to_string(fx.root.join(MANIFEST)).unwrap();
        fx.put(MANIFEST, &format!("{manifest}{tail}"));
        let before = fx.snapshot();
        let o = fx.adopt(&["--module", "core", "--preview"]);
        refused(&o, code_name);
        assert!(stderr(&o).contains(MANIFEST), "{}", stderr(&o));
        assert_eq!(fx.snapshot(), before);
    }
}

#[test]
fn a_brewfile_hand_edit_during_recovery_is_a_conflict() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-brewfile:error");
    let brewfile = fs::read_to_string(fx.root.join("Brewfile")).unwrap();
    fx.put("Brewfile", &format!("{brewfile}brew \"jq\"\n"));
    let before = fx.snapshot();
    conflict_on(&fx.adopt(&["--module", "core", "--preview"]), "Brewfile");
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_live_file_edited_during_recovery_is_a_conflict() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-copy:abort");
    fx.live(LIVE, "theme = \"light\"\n");
    let before = fx.snapshot();
    conflict_on(
        &fx.adopt(&["--module", "core", "--preview"]),
        &fx.h(LIVE).display().to_string(),
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_live_link_retargeted_during_recovery_is_a_conflict() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-link:error");
    let elsewhere = fx.h(".config/tuicr-other.toml");
    fs::write(&elsewhere, TUICR).unwrap();
    fs::remove_file(fx.h(LIVE)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, fx.h(LIVE)).unwrap();
    let before = fx.snapshot();
    conflict_on(
        &fx.adopt(&["--module", "core", "--preview"]),
        &fx.h(LIVE).display().to_string(),
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_changed_repo_copy_during_recovery_is_a_conflict() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-copy:abort");
    fx.put(REPO_FILE, "theme = \"other\"\n");
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "core", "--preview"]);
    conflict_on(&o, REPO_FILE);
    assert!(stderr(&o).contains("is not the copy"), "{}", stderr(&o));
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_repo_copy_changed_through_the_link_names_the_takeover() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-link:error");
    // The tool writes through the live link into the repo copy.
    fs::write(fx.h(LIVE), "theme = \"light\"\n").unwrap();
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "core", "--preview"]);
    conflict_on(&o, REPO_FILE);
    assert!(
        stderr(&o).contains("changed after the takeover"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fx.snapshot(), before);
}

#[test]
fn a_kept_copy_reached_through_a_linked_directory_is_a_conflict() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    interrupted(&fx, "adopt-after-copy:abort");
    // A directory on the way to the copy is swapped for a link to an outside directory that
    // holds the same bytes.
    let outside = fx._src.path().join("outside");
    fs::create_dir_all(outside.join("tuicr")).unwrap();
    fs::write(outside.join("tuicr/config.toml"), TUICR).unwrap();
    let dir = fx.root.join("base/modules/core/home/.config");
    fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&outside, &dir).unwrap();
    let before = fx.snapshot();
    let o = fx.adopt(&["--module", "core", "--preview"]);
    conflict_on(&o, "base/modules/core/home/.config");
    assert!(stderr(&o).contains("is a link"), "{}", stderr(&o));
    assert_eq!(fx.snapshot(), before);
    assert!(fs::symlink_metadata(fx.h(LIVE)).unwrap().is_file());
}

/// A run parked at a failpoint. Dropping it opens the gate and ends the child, so a failed
/// assertion does not leave the process behind.
struct Parked {
    child: Option<std::process::Child>,
    gate: PathBuf,
}

impl Drop for Parked {
    fn drop(&mut self) {
        let _ = fs::write(&self.gate, "");
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn two_adopts_at_once_one_waits_or_is_locked() {
    let fx = Fx::new();
    fx.live(LIVE, TUICR);
    let d = digest_of(&fx.adopt(&["--module", "core", "--preview"]));
    let gate = fx.fake.path().join("go");
    let child = Command::new(BIN)
        .env_clear()
        .env(
            "DFM_PROVIDER_PATH",
            format!("{}:/usr/bin:/bin", fx.bin().display()),
        )
        .env(
            "DFM_FAILPOINT",
            format!("adopt-after-copy:wait-{}", gate.display()),
        )
        .arg("--home")
        .arg(fx.home.path())
        .args(["adopt", "trial-tuicr", "--module", "core", "--yes-plan", &d])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut parked = Parked {
        child: Some(child),
        gate: gate.clone(),
    };
    let reached = gate.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < end,
            "the failpoint was not reached"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The first run holds the writer lock with its repo copy written. The second run's preview
    // reads that partial work; its approval then needs the lock.
    let before = fx.snapshot();
    let d2 = digest_of(&fx.adopt(&["--module", "core", "--preview"]));
    let o = fx.adopt(&["--module", "core", "--yes-plan", &d2]);
    refused(&o, "E_LOCKED");
    assert_eq!(fx.snapshot(), before);
    fs::write(&gate, "").unwrap();
    let o = parked.child.take().unwrap().wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), out(&o));
    assert_eq!(fs::read_link(fx.h(LIVE)).unwrap(), fx.root.join(REPO_FILE));
}
