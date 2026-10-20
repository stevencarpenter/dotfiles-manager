//! `secrets`: the `op-template/v1` adapter through the isolated binary (DFM-22, docs/secrets.md).
//!
//! Every run passes `--home <temp dir>` and an empty environment, plus `DFM_PROVIDER_PATH`
//! pointing at a directory that holds an `op` look-alike. The look-alike logs its arguments,
//! answers `whoami` from a marker file and renders `inject` from a value file, so each test
//! controls the session and the vault without 1Password.

mod common;

use common::{TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");
/// The value the fake vault renders. It must appear in the destination and nowhere else.
const SECRET: &str = "PLANTED-SECRET-5b1e2d";
/// A marker the fake writes on standard error. It must never reach the manager's output.
const STDERR_MARK: &str = "PLANTED-STDERR-9c4f";
const TARGET: &str = ".config/example/personal.env";
const OP_ID: &str = "extra.token@.config/example/personal.env";

const EXTRA: &str = r#"schema = 1
id = "extra"
platforms = ["macos", "linux"]
requires = []

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

const FAKE_OP: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf '%s | session=%s user=%s\n' "$*" "${OP_SESSION_planted:-unset}" "${USER:-none}" >> "$here/../log"
if [ ! -e "$here/../signed-in" ]; then
  echo "[ERROR] no session PLANTED-STDERR-9c4f" >&2
  exit 1
fi
case "$1" in
  whoami) echo "me"; exit 0 ;;
  inject)
    [ -e "$here/../partial" ] && { echo "TOKEN=partial"; exit 0; }
    cat > "$here/../stdin"
    [ -e "$here/../empty" ] && exit 0
    if [ -e "$here/../fail" ]; then echo "[ERROR] boom PLANTED-STDERR-9c4f" >&2; exit 1; fi
    value=$(cat "$here/../value")
    sed "s|{{ op://vault/item/field }}|$value|" < "$here/../stdin"
    ;;
  *) exit 64 ;;
esac
"#;

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
    fake: TempDir,
}

fn stderr(o: &Output) -> String {
    text(&o.stderr)
}

/// The digest line of a printed plan.
fn digest_of(o: &Output) -> String {
    text(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no digest in\n{}\n{}", text(&o.stdout), stderr(o)))
}

impl Fx {
    fn new() -> Fx {
        let fx = Fx {
            home: TempDir::new("secrets-home"),
            _src: TempDir::new("secrets-src"),
            root: PathBuf::new(),
            fake: TempDir::new("secrets-op"),
        };
        let root = fx._src.path().join("root");
        let fx = Fx { root, ..fx };
        fx.put("base/bundle.toml", common::BUNDLE);
        fx.put("base/modules/core/module.toml", common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/extra/module.toml", EXTRA);
        fx.put(
            "base/modules/extra/files/personal.env.tpl",
            "TOKEN={{ op://vault/item/field }}\n",
        );
        fx.put("profiles/t-01.toml", common::PROFILE);
        fs::create_dir_all(fx.bin()).unwrap();
        let op = fx.bin().join("op");
        fs::write(&op, FAKE_OP).unwrap();
        fs::set_permissions(&op, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(fx.fake.path().join("value"), SECRET).unwrap();
        fx.sign_in(true);
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

    fn bin(&self) -> PathBuf {
        self.fake.path().join("bin")
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn sign_in(&self, on: bool) {
        let m = self.fake.path().join("signed-in");
        if on {
            fs::write(m, "").unwrap();
        } else {
            let _ = fs::remove_file(m);
        }
    }

    fn marker(&self, name: &str) {
        fs::write(self.fake.path().join(name), "").unwrap();
    }

    /// Every `op` invocation so far, one line of arguments each. The file exists only once `op`
    /// ran; a test that expects no run checks that.
    fn op_log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    fn op_ran(&self) -> bool {
        self.fake.path().join("log").exists()
    }

    fn set_modules(&self, modules: &str) {
        self.put(
            "profiles/t-01.toml",
            &common::PROFILE.replace("modules = [\"extra\"]", &format!("modules = [{modules}]")),
        );
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
            .env("OP_SESSION_planted", "should-pass-through")
            // Set so the look-alike's `user=` check can fail: only OP_* may reach op.
            .env("USER", "leak-check")
            .arg("--home")
            .arg(self.home.path())
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn preview(&self) -> Output {
        self.run(&["secrets", "--root", self.root_arg(), "--preview"])
    }

    fn secrets(&self, digest: &str, adopt: &[&str]) -> Output {
        let mut a = vec!["secrets", "--root", self.root_arg(), "--yes-plan", digest];
        for id in adopt {
            a.push("--allow-adopt");
            a.push(id);
        }
        self.run(&a)
    }

    fn state(&self) -> PathBuf {
        self.h(".local/state/dotfiles-manager/state")
    }

    fn ledger(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.state().join("ledger.json")).unwrap()).unwrap()
    }

    fn receipts(&self) -> Vec<serde_json::Value> {
        let mut v: Vec<_> = fs::read_dir(self.state().join("receipts"))
            .unwrap()
            .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect();
        v.sort_by_key(|r: &serde_json::Value| r["started_at"].as_str().unwrap().to_string());
        v
    }

    fn secret_entry(&self) -> Option<serde_json::Value> {
        self.ledger()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["resource"] == "extra.token")
            .cloned()
    }

    fn content(&self) -> Vec<u8> {
        fs::read(self.h(TARGET)).unwrap()
    }

    fn mode(&self) -> u32 {
        fs::metadata(self.h(TARGET)).unwrap().permissions().mode() & 0o777
    }
}

/// Every byte under `dir`, for leak checks.
fn all_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(all_bytes(&p));
        } else if p.is_file() {
            out.extend(fs::read(&p).unwrap());
        }
    }
    out
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn a_configuration_plan_lists_the_secret_out_of_scope_and_never_starts_op() {
    let fx = Fx::new();
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let t = text(&o.stdout);
    assert!(t.contains("not_in_scope extra.token@scope"), "{t}");
    assert!(t.contains("scope=config"), "{t}");
    let d = digest_of(&o);
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &d]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains("not_in_scope extra.token@scope"));
    assert!(!fx.h(TARGET).exists());
    assert!(
        !fx.op_ran(),
        "op was started by plan or apply: {}",
        fx.op_log()
    );
}

#[test]
fn without_a_session_secrets_exits_3_before_any_write_and_the_preview_still_shows_the_plan() {
    let fx = Fx::new();
    fx.sign_in(false);
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_CREDENTIAL_UNAVAILABLE"),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("example-account"), "{}", stderr(&o));
    let t = text(&o.stdout);
    assert!(t.contains("scope=secrets"), "{t}");
    assert!(t.contains(&format!("planned {OP_ID}")), "{t}");
    assert!(t.contains("render"), "{t}");
    assert!(t.contains("not_in_scope core.git@scope"), "{t}");
    let d = digest_of(&o);
    let o = fx.secrets(&d, &[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CREDENTIAL_UNAVAILABLE"));
    assert!(!fx.h(TARGET).exists());
    assert!(!fx.state().join("receipts").exists() || fx.receipts().is_empty());
    // Only the session check ran, with the declared account. Nothing was rendered.
    let log = fx.op_log();
    assert!(
        log.lines()
            .all(|l| l.starts_with("whoami --account example-account |")),
        "{log}"
    );
    assert!(!stderr(&o).contains(STDERR_MARK) && !text(&o.stdout).contains(STDERR_MARK));
}

#[test]
fn a_secret_is_rendered_once_recorded_salted_and_a_rerun_is_current() {
    let fx = Fx::new();
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let d = digest_of(&o);
    let o = fx.secrets(&d, &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(
        out.contains(&format!("updated {OP_ID} reload=not_attempted")),
        "{out}"
    );
    assert!(out.contains("result: ok"), "{out}");
    assert_eq!(fx.content(), format!("TOKEN={SECRET}\n").as_bytes());
    assert_eq!(fx.mode(), 0o600);
    // The template reached op on standard input, for the declared account.
    assert_eq!(
        fs::read_to_string(fx.fake.path().join("stdin")).unwrap(),
        "TOKEN={{ op://vault/item/field }}\n"
    );
    assert!(
        fx.op_log().contains("inject --account example-account"),
        "{}",
        fx.op_log()
    );
    // The ledger records the output as sensitive with a salted hash and no fingerprint.
    let e = fx.secret_entry().expect("ledger entry");
    assert_eq!(e["class"], "sensitive");
    assert_eq!(e["adapter"], "op-template/v1");
    let out0 = &e["owned_outputs"][0];
    assert!(out0["path"].as_str().unwrap().ends_with(TARGET));
    assert_eq!(out0["mode"], 0o600);
    assert!(out0["fingerprint"].is_null());
    let hash = out0["coowned"]["managed_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(hash.len(), 64);
    // The receipt names the command and the operation, with no fingerprints.
    let r = fx.receipts();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["command"], "secrets");
    // The resource has two operations: the parent directory (normal class) and the secret.
    let ops = r[0]["operations"].as_array().unwrap();
    let op = ops
        .iter()
        .find(|o| o["resource"] == "extra.token" && o["class"] == "sensitive")
        .unwrap();
    assert_eq!(op["status"], "updated", "{op}");
    assert!(op["before"].is_null() && op["after"].is_null());
    assert_eq!(op["owned_outputs"].as_array().unwrap().len(), 1, "{op}");
    assert_eq!(op["reload"], "not_attempted");
    assert!(
        ops.iter()
            .filter(|o| o["resource"] != "extra.token")
            .all(|o| o["status"] == "not_in_scope")
    );
    // Nothing but the destination holds the value, its plain hash or the salted hash.
    let plain = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(format!("TOKEN={SECRET}\n")))
    };
    let state = all_bytes(&fx.state());
    assert!(
        !contains(&state, SECRET),
        "the secret is in the state store"
    );
    assert!(
        !contains(&state, &plain),
        "a plain hash of the secret is in the state store"
    );
    assert!(!out.contains(SECRET) && !stderr(&o).contains(SECRET));
    assert!(!out.contains(&hash) && !stderr(&o).contains(&hash));
    let printed = text(&fx.preview().stdout);
    assert!(
        !printed.contains(SECRET) && !printed.contains(&hash),
        "{printed}"
    );
    // A rerun renders again, finds the same bytes and changes nothing: no receipt, no journal.
    let journal = fs::read_to_string(fx.state().join("journal.jsonl")).unwrap();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("current {OP_ID}")),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.receipts().len(), 1);
    assert_eq!(
        fs::read_to_string(fx.state().join("journal.jsonl")).unwrap(),
        journal
    );
    // A changed vault value is written on the next run, under the same ledger entry.
    fs::write(fx.fake.path().join("value"), "ROTATED").unwrap();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.content(), b"TOKEN=ROTATED\n");
    assert_eq!(fx.mode(), 0o600);
    assert_eq!(fx.receipts().len(), 2);
    let e = fx.secret_entry().unwrap();
    assert_ne!(e["owned_outputs"][0]["coowned"]["managed_hash"], hash);
}

#[test]
fn a_hand_edited_destination_is_a_takeover_that_needs_allow_adopt() {
    let fx = Fx::new();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    fs::write(fx.h(TARGET), "TOKEN=edited-by-hand\n").unwrap();
    let o = fx.preview();
    let t = text(&o.stdout);
    assert!(t.contains("take over"), "{t}");
    assert!(stderr(&o).contains("--allow-adopt"), "{}", stderr(&o));
    assert_eq!(code(&o), 3);
    let d = digest_of(&o);
    let o = fx.secrets(&d, &[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_ADOPT_UNAPPROVED"), "{}", stderr(&o));
    assert_eq!(fx.content(), b"TOKEN=edited-by-hand\n");
    let o = fx.secrets(&d, &[OP_ID]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.content(), format!("TOKEN={SECRET}\n").as_bytes());
    let r = fx.receipts();
    assert_eq!(r.last().unwrap()["approvals"][0]["how"], "allow_adopt");
    // An existing file the manager never wrote is also a takeover.
    let fresh = Fx::new();
    fs::create_dir_all(fresh.h(".config/example")).unwrap();
    fs::write(fresh.h(TARGET), "TOKEN=old\n").unwrap();
    fs::set_permissions(fresh.h(TARGET), fs::Permissions::from_mode(0o600)).unwrap();
    let o = fresh.preview();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_ADOPT_UNAPPROVED"), "{}", stderr(&o));
    // A file open to group or others is a conflict, not a takeover.
    fs::set_permissions(fresh.h(TARGET), fs::Permissions::from_mode(0o644)).unwrap();
    let o = fresh.preview();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(stderr(&o).contains("chmod 600"), "{}", stderr(&o));
}

#[test]
fn a_template_changed_after_the_locked_plan_blocks_the_render() {
    let fx = Fx::new();
    let d = digest_of(&fx.preview());
    // The run parks after the lock and the second plan, before it reads the template.
    let release = fx.fake.path().join("release");
    let child = fx
        .cmd(&["secrets", "--root", fx.root_arg(), "--yes-plan", &d])
        .env(
            "DFM_FAILPOINT",
            format!("apply-after-prereq:wait-{}", release.display()),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reached = release.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(std::time::Instant::now() < end, "the run never parked");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    fx.put(
        "base/modules/extra/files/personal.env.tpl",
        "TOKEN={{ op://vault/other-item/field }}\n",
    );
    fs::write(&release, "").unwrap();
    let o = child.wait_with_output().unwrap();
    // The parent directory was created first, so the run already mutated: exit 1, not 3.
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(
        text(&o.stdout).contains(&format!("blocked {OP_ID}")),
        "{}",
        text(&o.stdout)
    );
    assert!(!fx.h(TARGET).exists());
    assert!(!fx.op_log().contains("inject"), "{}", fx.op_log());
}

/// Starts `secrets --yes-plan <digest>` parked at `point`, waits until it is there, runs
/// `change`, releases it and returns its output.
fn parked_secrets(fx: &Fx, d: &str, point: &str, change: impl FnOnce()) -> Output {
    let release = fx.fake.path().join(format!("release-{point}"));
    let child = fx
        .cmd(&["secrets", "--root", fx.root_arg(), "--yes-plan", d])
        .env(
            "DFM_FAILPOINT",
            format!("{point}:wait-{}", release.display()),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reached = release.with_extension("reached");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(std::time::Instant::now() < end, "the run never parked");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    change();
    fs::write(&release, "").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn a_destination_changed_after_the_locked_plan_is_not_overwritten() {
    // Hand-edited after the plan was approved and locked: the ledger no longer proves the
    // manager wrote it, so it is a conflict, not a silent overwrite.
    let fx = Fx::new();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let d = digest_of(&fx.preview());
    let o = parked_secrets(&fx, &d, "merge-before-read", || {
        fs::write(fx.h(TARGET), "TOKEN=edited-after-approval\n").unwrap();
    });
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("the ledger no longer proves the manager wrote it"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fx.content(), b"TOKEN=edited-after-approval\n");
    // The salt removed after the locked plan: the recorded hash cannot be checked, same refusal.
    let fx = Fx::new();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let before = fx.content();
    let d = digest_of(&fx.preview());
    let o = parked_secrets(&fx, &d, "merge-before-read", || {
        fs::remove_file(fx.state().join("salt")).unwrap();
    });
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert_eq!(fx.content(), before);
    // At plan time, a recorded hash with no salt is a state problem with its own fix text.
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_STATE_INVALID"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("state/salt does not exist"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_destination_that_is_not_a_plain_file_of_this_secret_is_refused() {
    let refused = |fx: &Fx, code_name: &str, detail: &str| {
        let o = fx.preview();
        assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(stderr(&o).contains(code_name), "{}", stderr(&o));
        assert!(stderr(&o).contains(detail), "{}", stderr(&o));
        assert!(!fx.op_log().contains("inject"), "{}", fx.op_log());
    };
    // A directory.
    let fx = Fx::new();
    fs::create_dir_all(fx.h(TARGET)).unwrap();
    refused(
        &fx,
        "E_CONFLICT",
        "is a directory where a secret file is needed",
    );
    // A symbolic link, even to a private file.
    let fx = Fx::new();
    fs::create_dir_all(fx.h(".config/example")).unwrap();
    fs::write(fx.h("elsewhere"), "x").unwrap();
    fs::set_permissions(fx.h("elsewhere"), fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(fx.h("elsewhere"), fx.h(TARGET)).unwrap();
    refused(&fx, "E_CONFLICT", "is a link or special file");
    // Larger than the 16 MiB read bound.
    let fx = Fx::new();
    fs::create_dir_all(fx.h(".config/example")).unwrap();
    fs::write(fx.h(TARGET), vec![b'x'; 17 << 20]).unwrap();
    fs::set_permissions(fx.h(TARGET), fs::Permissions::from_mode(0o600)).unwrap();
    refused(&fx, "E_LIMIT_EXCEEDED", "exceeds 16 MiB");
    // Recorded in the ledger as another resource's output.
    let fx = Fx::new();
    let out = fx.h(TARGET);
    fs::create_dir_all(out.parent().unwrap()).unwrap();
    let o = fx.run(&[
        "__record-op",
        "--resource",
        "core.git",
        "--output",
        out.to_str().unwrap(),
        "--content",
        "other",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    fs::set_permissions(&out, fs::Permissions::from_mode(0o600)).unwrap();
    refused(
        &fx,
        "E_CONFLICT",
        "is recorded in the ledger as an output of core.git",
    );
}

#[test]
fn a_render_from_part_of_the_template_is_refused() {
    let fx = Fx::new();
    // Larger than a pipe buffer, so an op that exits without reading cannot take it all.
    fx.put(
        "base/modules/extra/files/personal.env.tpl",
        &format!(
            "TOKEN={{{{ op://vault/item/field }}}}\n#{}\n",
            "x".repeat(200_000)
        ),
    );
    fx.marker("partial");
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_RENDER_FAILED"), "{}", stderr(&o));
    assert!(
        text(&o.stdout).contains("exited before it read the whole template"),
        "{}",
        text(&o.stdout)
    );
    assert!(!fx.h(TARGET).exists());
}

#[test]
fn a_failed_or_empty_render_keeps_the_previous_file_and_withholds_provider_stderr() {
    let fx = Fx::new();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let before = fx.content();
    let journal = fs::read_to_string(fx.state().join("journal.jsonl")).unwrap();
    fx.marker("empty");
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_RENDER_FAILED"), "{}", stderr(&o));
    assert!(
        text(&o.stdout).contains(&format!("failed {OP_ID}")),
        "{}",
        text(&o.stdout)
    );
    assert!(
        text(&o.stdout).contains("rendered nothing"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.content(), before);
    fs::remove_file(fx.fake.path().join("empty")).unwrap();
    fx.marker("fail");
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_RENDER_FAILED"), "{}", stderr(&o));
    assert!(text(&o.stdout).contains("withheld"), "{}", text(&o.stdout));
    assert!(!text(&o.stdout).contains(STDERR_MARK) && !stderr(&o).contains(STDERR_MARK));
    assert_eq!(fx.content(), before);
    // Both failures happened before the intent entry: nothing was journaled, no receipt added.
    assert_eq!(
        fs::read_to_string(fx.state().join("journal.jsonl")).unwrap(),
        journal
    );
    assert_eq!(fx.receipts().len(), 1);
    assert!(!contains(&all_bytes(&fx.state()), STDERR_MARK));
}

#[test]
fn secrets_takes_no_removal_flag_and_doctor_still_runs() {
    let fx = Fx::new();
    let o = fx.run(&[
        "secrets",
        "--root",
        fx.root_arg(),
        "--allow-disruptive",
        "x",
    ]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    let o = fx.run(&["doctor", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(!fx.op_ran(), "doctor started op: {}", fx.op_log());
}

#[test]
fn a_deselected_secret_keeps_its_file_and_entry_and_apply_still_runs() {
    let fx = Fx::new();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let before = fx.content();
    fx.set_modules("");
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("current remove@extra.token@"), "{t}");
    assert!(t.contains("kept with its ledger entry"), "{t}");
    assert!(!t.contains("retire"), "{t}");
    let d = digest_of(&o);
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &d]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains("result: ok"));
    assert_eq!(fx.content(), before);
    assert!(fx.secret_entry().is_some(), "the ledger entry was dropped");
    // The secrets plan of the deselected profile lists nothing for it either.
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        !text(&o.stdout).contains("extra.token@"),
        "{}",
        text(&o.stdout)
    );
}

#[test]
fn a_changed_declared_mode_is_applied_and_the_session_reaches_op() {
    let fx = Fx::new();
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fx.mode(), 0o600);
    // Only the OP_* variables of the caller reach op; USER does not.
    let log = fx.op_log();
    assert!(
        log.lines()
            .all(|l| l.ends_with("| session=should-pass-through user=none")),
        "{log}"
    );
    // Same bytes, another declared mode: written, not current.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("mode = 0o600", "mode = 0o400"),
    );
    let o = fx.secrets(&digest_of(&fx.preview()), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("updated {OP_ID}")),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(fx.mode(), 0o400);
    assert_eq!(fx.content(), format!("TOKEN={SECRET}\n").as_bytes());
}
