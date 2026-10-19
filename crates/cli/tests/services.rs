//! `services`: the `launchd-agent/v1` and `brew-service/v1` adapters through the isolated binary
//! (DFM-26, docs/lifecycle.md).
//!
//! Every run passes `--home <temp dir>` and an empty environment, plus `DFM_PROVIDER_PATH`
//! pointing at a directory with `launchctl` and `brew` look-alikes. Each look-alike logs its
//! arguments and the provider variables it saw and keeps its "loaded" state in a file, so no
//! real launcher runs. `DFM_SERVICE_TIMEOUT_SECS=1` keeps the hang and the never-completing
//! start to one second.

mod common;

use common::{TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");
const AGENT_OP: &str = "extra.maxfiles@service";
const BAR_OP: &str = "extra.sketchybar@service";
const LABEL: &str = "org.example.maxfiles";
const PLIST: &str = "Library/LaunchAgents/org.example.maxfiles.plist";

const EXTRA: &str = r#"schema = 1
id = "extra"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "maxfiles-plist"
kind = "link"
adapter = "fs-link/v1"
target = "Library/LaunchAgents/org.example.maxfiles.plist"
[resources.value]
source = "files/org.example.maxfiles.plist"

[[resources]]
id = "maxfiles"
kind = "service"
adapter = "launchd-agent/v1"
[resources.value]
label = "org.example.maxfiles"
plist = "Library/LaunchAgents/org.example.maxfiles.plist"

[[resources]]
id = "sketchybarrc"
kind = "link"
adapter = "fs-link/v1"
target = ".config/sketchybar/sketchybarrc"
[resources.value]
source = "files/sketchybarrc"

[[resources]]
id = "sketchybar-pkg"
kind = "package"
adapter = "homebrew/v1"
[resources.value]
name = "sketchybar"
update_policy = "rolling"

[[resources]]
id = "sketchybar"
kind = "service"
adapter = "brew-service/v1"
[resources.value]
name = "sketchybar"
config = [".config/sketchybar/sketchybarrc"]
"#;

const AGENT_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>org.example.maxfiles</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/launchctl</string>
		<string>limit</string>
		<string>maxfiles</string>
		<string>65536</string>
		<string>2097152</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#;

/// `launchctl print <domain>/<label>` answers from `loaded` (one `<label> <path> <state>` line
/// each) in launchd's report shape, or exits 113 with "Could not find service" as launchd does.
/// `bootstrap <domain> <plist>` reads the Label of the plist and adds a line; `bootout
/// <domain>/<label>` removes it. Markers: `hang` makes `print` sleep past the bound, `stop` makes
/// it send SIGTERM to the manager, `fail` makes `bootstrap` exit 5, `noop` makes it exit 0 without
/// loading.
const FAKE_LAUNCHCTL: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'launchctl %s | user=%s\n' "$*" "${USER:-none}" >> "$here/../log"
db="$here/../loaded"
case "$1" in
  print)
    [ -e "$here/../hang" ] && sleep 30
    [ -e "$here/../stop" ] && kill -TERM $PPID
    label=${2##*/}
    line=$(grep "^$label " "$db" 2>/dev/null | head -1)
    if [ -z "$line" ]; then
      echo "Bad request." >&2
      echo "Could not find service \"$label\" in domain for user gui: 501" >&2
      exit 113
    fi
    set -- $line
    if [ "$3" = running ]; then st=running; else st="not running"; fi
    printf 'gui/501/%s = {\n\tactive count = 0\n\tpath = %s\n\ttype = LaunchAgent\n\tstate = %s\n\n\tprogram = /bin/launchctl\n}\n' "$1" "$2" "$st"
    exit 0 ;;
  bootstrap)
    [ -e "$here/../fail" ] && { echo "Bootstrap failed: 5: Input/output error PLANTED-STDERR" >&2; exit 5; }
    [ -e "$here/../noop" ] && exit 0
    label=$(tr -d '\n\t' < "$3" | sed -n 's/.*<key>Label<\/key> *<string>\([^<]*\)<\/string>.*/\1/p')
    echo "$label $3 stopped" >> "$db"; exit 0 ;;
  bootout)
    label=${2##*/}
    grep -v "^$label " "$db" > "$db.tmp" 2>/dev/null; mv "$db.tmp" "$db"; exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `brew services info <name> --json` answers from `brew-<name>` (`none`: installed and not
/// loaded; `started`: loaded and running; `error`: loaded and not running); no file is a
/// formula that is not installed. `start` and `restart` set `started`, or fail with the `fail`
/// marker. `queue-after-start` holds a count: that many `info` calls after a start report the
/// service loaded and not running, as a slow daemon does.
const FAKE_BREW: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'brew %s | auto=%s analytics=%s hints=%s user=%s\n' "$*" "${HOMEBREW_NO_AUTO_UPDATE:-unset}" "${HOMEBREW_NO_ANALYTICS:-unset}" "${HOMEBREW_NO_ENV_HINTS:-unset}" "${USER:-none}" >> "$here/../log"
[ "$1" = services ] || exit 64
name=$3
st="$here/../brew-$name"
case "$2" in
  info)
    [ -e "$st" ] || { echo "Error: No available formula with the name \"$name\"." >&2; exit 1; }
    s=$(cat "$st")
    q="$here/../queue-$name"
    if [ -e "$q" ]; then
      n=$(cat "$q")
      if [ "$n" -gt 0 ]; then echo $((n-1)) > "$q"; s=error; fi
    fi
    case "$s" in
      started) loaded=true; running=true ;;
      error) loaded=true; running=false ;;
      *) loaded=false; running=false ;;
    esac
    printf '[{"name":"%s","service_name":"sh.brew.%s","running":%s,"loaded":%s,"pid":null,"status":"%s"}]\n' "$name" "$name" "$running" "$loaded" "$s"
    exit 0 ;;
  start|restart)
    [ -e "$here/../fail" ] && { echo "Error: PLANTED-STDERR" >&2; exit 1; }
    [ -e "$st" ] || exit 1
    [ -e "$here/../queue-after-start" ] && cp "$here/../queue-after-start" "$here/../queue-$name"
    echo started > "$st"; exit 0 ;;
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

/// The current user id, as the launchd domain `gui/<uid>` names it.
fn uid() -> String {
    text(&Command::new("id").arg("-u").output().unwrap().stdout)
        .trim()
        .to_string()
}

fn digest_of(o: &Output) -> String {
    text(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no digest in\n{}\n{}", text(&o.stdout), stderr(o)))
}

impl Fx {
    fn new() -> Fx {
        let fx = Fx {
            home: TempDir::new("services-home"),
            _src: TempDir::new("services-src"),
            root: PathBuf::new(),
            fake: TempDir::new("services-providers"),
        };
        let root = fx._src.path().join("root");
        let fx = Fx { root, ..fx };
        fx.put("base/bundle.toml", common::BUNDLE);
        fx.put("base/modules/core/module.toml", common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/extra/module.toml", EXTRA);
        fx.put(
            "base/modules/extra/files/org.example.maxfiles.plist",
            AGENT_PLIST,
        );
        fx.put("base/modules/extra/files/sketchybarrc", "bar height=32\n");
        fx.put("profiles/t-01.toml", common::PROFILE);
        fs::create_dir_all(fx.bin()).unwrap();
        for (name, body) in [("launchctl", FAKE_LAUNCHCTL), ("brew", FAKE_BREW)] {
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

    /// A marker or state file the look-alikes read.
    fn mark(&self, name: &str, content: &str) {
        fs::write(self.fake.path().join(name), content).unwrap();
    }

    fn unmark(&self, name: &str) {
        let _ = fs::remove_file(self.fake.path().join(name));
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    fn launcher_ran(&self) -> bool {
        self.fake.path().join("log").exists()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
            .env("DFM_SERVICE_TIMEOUT_SECS", "1")
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

    fn preview(&self, flags: &[&str]) -> Output {
        let mut a = vec!["services", "--root", self.root_arg(), "--preview"];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    fn services(&self, digest: &str, flags: &[&str]) -> Output {
        let mut a = vec!["services", "--root", self.root_arg(), "--yes-plan", digest];
        a.extend_from_slice(flags);
        self.run(&a)
    }

    /// Publishes the configuration (the plist link and the bar config) with `apply`.
    fn publish(&self) {
        let o = self.run(&["apply", "--root", self.root_arg(), "--preview"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        let d = digest_of(&o);
        let o = self.run(&["apply", "--root", self.root_arg(), "--yes-plan", &d]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(self.home.path().join(PLIST).is_symlink());
    }

    fn state(&self) -> PathBuf {
        self.home.path().join(".local/state/dotfiles-manager/state")
    }

    fn receipts(&self) -> Vec<serde_json::Value> {
        let Ok(dir) = fs::read_dir(self.state().join("receipts")) else {
            return Vec::new();
        };
        let mut v: Vec<serde_json::Value> = dir
            .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect();
        v.sort_by_key(|r| r["started_at"].as_str().unwrap_or("").to_string());
        v
    }

    fn journal(&self) -> String {
        fs::read_to_string(self.state().join("journal.jsonl")).unwrap_or_default()
    }

    fn op<'a>(&self, receipt: &'a serde_json::Value, resource: &str) -> &'a serde_json::Value {
        receipt["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["resource"] == resource)
            .unwrap()
    }
}

#[test]
fn a_configuration_plan_lists_services_out_of_scope_and_starts_no_launcher() {
    let fx = Fx::new();
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let t = text(&o.stdout);
    assert!(t.contains("not_in_scope extra.maxfiles@scope"), "{t}");
    assert!(
        t.contains("service is lifecycle, outside a config plan"),
        "{t}"
    );
    let o = fx.run(&["doctor", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // The other scopes do not act on a service either, and none of them asks a launcher.
    fx.mark("brew-sketchybar", "none\n");
    let o = fx.run(&["packages", "--root", fx.root_arg(), "--preview"]);
    assert!(
        text(&o.stdout).contains("not_in_scope extra.sketchybar@scope"),
        "{}\n{}",
        stderr(&o),
        text(&o.stdout)
    );
    assert!(!fx.log().contains("services"), "{}", fx.log());
    assert!(!fx.log().contains("launchctl"), "{}", fx.log());
    // The services plan lists the configuration out of scope, and the flags of apply that do
    // not apply are usage errors.
    fx.unmark("brew-sketchybar");
    assert_eq!(
        code(&fx.preview(&["--allow-adopt", "x"])),
        2,
        "--allow-adopt is not a services flag"
    );
    assert_eq!(code(&fx.preview(&["--restart"])), 2);
    assert!(!fx.launcher_ran() || !fx.log().contains("launchctl"));
}

#[test]
fn a_missing_definition_blocks_and_a_published_one_is_loaded_through_launchd() {
    let fx = Fx::new();
    fx.mark("brew-sketchybar", "started\n");
    // Nothing is published yet: the agent's definition is a prerequisite the plan cannot
    // satisfy, so the plan is blocked and no launcher is asked about the agent.
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(stderr(&o).contains("not published"), "{}", stderr(&o));
    assert!(
        text(&o.stdout).contains(&format!("blocked {AGENT_OP}")),
        "{}",
        text(&o.stdout)
    );
    assert!(!fx.log().contains("launchctl"), "{}", fx.log());
    fx.publish();
    // Published and not loaded: a start is planned, after the operation that publishes the
    // definition. The probe is read-only, and nothing of the caller's environment leaks.
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("scope=services"), "{t}");
    assert!(t.contains(&format!("planned {AGENT_OP}")), "{t}");
    assert!(
        t.contains(
            "start org.example.maxfiles with launchd-agent/v1: definition published, not loaded"
        ),
        "{t}"
    );
    // The plan shows what the definition would run, so a reviewer sees it before approving.
    assert!(
        t.contains("the definition runs /bin/launchctl limit maxfiles 65536 2097152"),
        "{t}"
    );
    assert!(t.contains("after: extra.maxfiles-plist@scope"), "{t}");
    assert!(t.contains(&format!("current {BAR_OP}")), "{t}");
    assert!(t.contains("not_in_scope core.git@scope"), "{t}");
    assert!(t.contains("not_in_scope extra.sketchybar-pkg@scope"), "{t}");
    let log = fx.log();
    assert!(
        log.contains(&format!(
            "launchctl print gui/{}/{LABEL} | user=none\n",
            uid()
        )),
        "{log}"
    );
    assert!(
        log.contains("brew services info sketchybar --json | auto=1 analytics=1 hints=1 user=none"),
        "{log}"
    );
    assert!(
        !log.contains("bootstrap") && !log.contains("bootout"),
        "{log}"
    );
    // The start loads the published definition and the launcher's report is the postcondition.
    let o = fx.services(&digest_of(&o), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(
        out.contains(&format!("updated {AGENT_OP} reload=reloaded")),
        "{out}"
    );
    assert!(out.contains("result: ok"), "{out}");
    assert!(!out.contains("reload: not attempted"), "{out}");
    let plist = fx.home.path().join(PLIST);
    assert!(
        fx.log().contains(&format!(
            "launchctl bootstrap gui/{} {} | user=none\n",
            uid(),
            plist.display()
        )),
        "{}",
        fx.log()
    );
    assert!(
        fx.journal().contains("\"op\":\"start\""),
        "{}",
        fx.journal()
    );
    // The receipt: command, the operation with its reload state, no outputs, no fingerprints.
    let r = fx.receipts();
    let receipt = r.iter().find(|r| r["command"] == "services").unwrap();
    let op = fx.op(receipt, "extra.maxfiles");
    assert_eq!(op["status"], "updated", "{op}");
    assert_eq!(op["reload"], "reloaded", "{op}");
    assert!(op["owned_outputs"].as_array().unwrap().is_empty());
    assert!(op["before"].is_null() && op["after"].is_null());
    // The rerun finds it loaded from the declared definition: current, no receipt, no action.
    let before = fx.receipts().len();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("current {AGENT_OP}"))
            && text(&o.stdout).contains("loaded from the declared definition, not running"),
        "{}",
        text(&o.stdout)
    );
    let o = fx.services(&digest_of(&o), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert_eq!(fx.receipts().len(), before);
}

#[test]
fn a_restart_is_disruptive_and_needs_its_approval() {
    let fx = Fx::new();
    fx.mark("brew-sketchybar", "started\n");
    fx.publish();
    // launchd holds the label from a definition the manager did not load (a hand-installed
    // agent under the same label): the manager never unloads it, with or without a request.
    fx.mark(
        "loaded",
        &format!("{LABEL} /somewhere/else/{LABEL}.plist running\n"),
    );
    for flags in [&[][..], &["--restart", "extra.maxfiles"][..]] {
        let o = fx.preview(flags);
        assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
        assert!(
            stderr(&o).contains(
                "loaded from a definition the manager does not own (/somewhere/else/org.example.maxfiles.plist), running"
            ),
            "{}",
            stderr(&o)
        );
        assert!(
            stderr(&o).contains(&format!(
                "unload it with `launchctl bootout gui/{}/{LABEL}`",
                uid()
            )),
            "{}",
            stderr(&o)
        );
        assert!(
            text(&o.stdout).contains(&format!("blocked {AGENT_OP}")),
            "{}",
            text(&o.stdout)
        );
    }
    assert!(
        !fx.log().contains("bootout") && !fx.log().contains("bootstrap"),
        "{}",
        fx.log()
    );
    // Loaded from the declared definition: current. A current service restarts only on
    // request, and the request is disruptive: the preview lists the flag and refuses without it.
    // A request that names no service is a usage error, and so is an approval that names no
    // restart.
    let plist = fx.home.path().join(PLIST);
    fx.mark("loaded", &format!("{LABEL} {} running\n", plist.display()));
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("current {AGENT_OP}")),
        "{}",
        text(&o.stdout)
    );
    let o = fx.preview(&["--restart", "extra.maxfiles"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(
            "restart org.example.maxfiles with launchd-agent/v1 on request (--restart): disruptive"
        ),
        "{}",
        text(&o.stdout)
    );
    assert!(
        stderr(&o).contains("E_DISRUPTIVE_UNAPPROVED"),
        "{}",
        stderr(&o)
    );
    assert!(
        text(&o.stdout).contains(&format!("--allow-disruptive '{AGENT_OP}'")),
        "{}",
        text(&o.stdout)
    );
    let o = fx.preview(&["--restart", "extra.nope"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("names no service"), "{}", stderr(&o));
    let o = fx.preview(&["--allow-disruptive", AGENT_OP]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("names no removal or restart"),
        "{}",
        stderr(&o)
    );
    // With the request and the approval: unload, wait until the domain no longer holds it,
    // load the declared definition again, and record the approval.
    let o = fx.preview(&[
        "--restart",
        "extra.maxfiles",
        "--allow-disruptive",
        AGENT_OP,
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.services(
        &digest_of(&o),
        &[
            "--restart",
            "extra.maxfiles",
            "--allow-disruptive",
            AGENT_OP,
        ],
    );
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("updated {AGENT_OP} reload=reloaded")),
        "{}",
        text(&o.stdout)
    );
    let log = fx.log();
    let bootout = format!("launchctl bootout gui/{}/{LABEL} | user=none\n", uid());
    let bootstrap = format!(
        "launchctl bootstrap gui/{} {} | user=none\n",
        uid(),
        plist.display()
    );
    let at = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing: {log}"))
    };
    assert!(at(&bootout) < at(&bootstrap), "{log}");
    assert_eq!(log.matches(&bootstrap).count(), 1, "{log}");
    assert!(
        fx.journal().contains("\"op\":\"restart\""),
        "{}",
        fx.journal()
    );
    let r = fx.receipts();
    let receipt = r.iter().find(|r| r["command"] == "services").unwrap();
    let approvals = receipt["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{receipt}");
    assert_eq!(approvals[0]["how"], "allow_disruptive");
}

#[test]
fn a_brew_service_is_started_and_a_queued_start_is_pending_not_updated() {
    let fx = Fx::new();
    fx.publish();
    fx.mark(
        "loaded",
        &format!("{LABEL} {} stopped\n", fx.home.path().join(PLIST).display()),
    );
    // The formula is not installed: the service depends on its package, which another command
    // installs. Nothing is started.
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("the formula sketchybar is not installed; packages installs it"),
        "{}",
        stderr(&o)
    );
    assert!(!fx.log().contains("services start"), "{}", fx.log());
    // Installed and not loaded: a start, after the package and the config file operations.
    fx.mark("brew-sketchybar", "none\n");
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("planned {BAR_OP}")), "{t}");
    assert!(
        t.contains("start sketchybar with brew-service/v1: installed, not loaded; not disruptive"),
        "{t}"
    );
    assert!(
        t.contains("after: extra.sketchybar-pkg@scope, extra.sketchybarrc@scope"),
        "{t}"
    );
    // The daemon never comes up within the bound: launchd accepted the request, so the
    // operation is pending (exit 5), recorded as requested, never as updated.
    fx.mark("queue-after-start", "999\n");
    let o = fx.services(&digest_of(&o), &[]);
    assert_eq!(code(&o), 5, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(
        out.contains(&format!("pending {BAR_OP} reload=requested")),
        "{out}"
    );
    assert!(out.contains("E_PENDING"), "{out}");
    assert!(out.contains("did not report completion in time"), "{out}");
    assert!(out.contains("result: pending"), "{out}");
    assert!(!out.contains(&format!("updated {BAR_OP}")), "{out}");
    assert!(
        fx.log()
            .contains("brew services start sketchybar | auto=1 analytics=1 hints=1 user=none"),
        "{}",
        fx.log()
    );
    let r = fx.receipts();
    let receipt = r.iter().find(|r| r["command"] == "services").unwrap();
    assert_eq!(receipt["result"]["outcome"], "pending", "{receipt}");
    assert_eq!(receipt["result"]["exit_code"], 5, "{receipt}");
    let op = fx.op(receipt, "extra.sketchybar");
    assert_eq!(op["status"], "pending", "{op}");
    assert_eq!(op["reload"], "requested", "{op}");
    assert!(
        fx.journal().contains("\"phase\":\"pending\""),
        "{}",
        fx.journal()
    );
    // The next run probes again: loaded and not running is a process the manager did not
    // start, so nothing restarts it by itself. The plan blocks and names the request to make;
    // the request is disruptive. A restart whose daemon comes up after one more report completes.
    fx.unmark("queue-after-start");
    fx.unmark("queue-sketchybar");
    fx.mark("brew-sketchybar", "error\n");
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains(
            "is installed, loaded, not running (status error); a restart would replace a process the manager did not start"
        ),
        "{}",
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("request it with `--restart extra.sketchybar`"),
        "{}",
        stderr(&o)
    );
    assert!(!fx.log().contains("services restart"), "{}", fx.log());
    let o = fx.preview(&["--restart", "extra.sketchybar"]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_DISRUPTIVE_UNAPPROVED"),
        "{}",
        stderr(&o)
    );
    assert!(
        text(&o.stdout)
            .contains("restart sketchybar with brew-service/v1 on request (--restart): disruptive"),
        "{}",
        text(&o.stdout)
    );
    fx.mark("queue-after-start", "1\n");
    let flags = [
        "--restart",
        "extra.sketchybar",
        "--allow-disruptive",
        BAR_OP,
    ];
    let o = fx.preview(&flags);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.services(&digest_of(&o), &flags);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("updated {BAR_OP} reload=reloaded")),
        "{}",
        text(&o.stdout)
    );
    assert!(
        fx.log()
            .contains("brew services restart sketchybar | auto=1 analytics=1 hints=1 user=none\n"),
        "{}",
        fx.log()
    );
    assert_eq!(
        fs::read_to_string(fx.fake.path().join("queue-sketchybar"))
            .unwrap()
            .trim(),
        "0"
    );
    // Current afterwards, and nothing else was ever run against the service.
    let o = fx.preview(&[]);
    assert!(
        text(&o.stdout).contains(&format!("current {BAR_OP}")),
        "{}",
        text(&o.stdout)
    );
    for forbidden in [
        "services stop",
        "services kill",
        "services remove",
        "services cleanup",
        "uninstall",
        "launchctl unload",
        "launchctl remove",
        "kickstart",
    ] {
        assert!(!fx.log().contains(forbidden), "{forbidden}: {}", fx.log());
    }
}

#[test]
fn a_launcher_that_fails_hangs_or_is_interrupted_never_reports_success() {
    let fx = Fx::new();
    fx.mark("brew-sketchybar", "started\n");
    fx.publish();
    // A start that fails: the operation is failed, exit 1, the launcher's error withheld, the
    // journal records the attempt and its failure.
    fx.mark("fail", "");
    let d = digest_of(&fx.preview(&[]));
    let o = fx.services(&d, &[]);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("failed {AGENT_OP}")), "{out}");
    assert!(out.contains("withheld"), "{out}");
    assert!(!out.contains("PLANTED-STDERR") && !stderr(&o).contains("PLANTED-STDERR"));
    assert!(out.contains("result: failed"), "{out}");
    assert!(fx.journal().contains("\"failed\""), "{}", fx.journal());
    fx.unmark("fail");
    // A start that exits 0 without loading fails its postcondition on the launcher's report.
    fx.mark("noop", "");
    let o = fx.services(&digest_of(&fx.preview(&[])), &[]);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("postcondition failed"),
        "{}",
        text(&o.stdout)
    );
    fx.unmark("noop");
    // A launcher that does not answer within the bound is an unknown state: blocked, exit 3,
    // before anything is approved or written.
    fx.mark("hang", "");
    let t = std::time::Instant::now();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PROBE_FAILED"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("did not answer within"),
        "{}",
        stderr(&o)
    );
    assert!(
        t.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        t.elapsed()
    );
    fx.unmark("hang");
    // A stop request during the probe ends the run as terminated, before any write, and the
    // second launcher is never asked.
    let receipts = fx.receipts().len();
    fx.mark("stop", "");
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 143, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_TERMINATED"), "{}", stderr(&o));
    assert_eq!(fx.receipts().len(), receipts);
    fx.unmark("stop");
    // A stop right after the action, before the postcondition, is reported as the stop: the
    // start may have completed, and the journal says so instead of claiming a failure.
    let d = digest_of(&fx.preview(&[]));
    let o = fx
        .cmd(&["services", "--root", fx.root_arg(), "--yes-plan", &d])
        .env("DFM_FAILPOINT", "apply-after-effect:stop")
        .output()
        .unwrap();
    assert_eq!(code(&o), 143, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("failed {AGENT_OP}")), "{out}");
    assert!(
        out.contains("stopped before the postcondition was checked"),
        "{out}"
    );
    assert!(!out.contains("postcondition failed"), "{out}");
}

#[test]
fn the_definition_must_declare_the_label_and_a_changed_prerequisite_makes_the_plan_stale() {
    let fx = Fx::new();
    fx.mark("brew-sketchybar", "started\n");
    fx.publish();
    // A published definition for another label would load another agent: a declaration error.
    fx.put(
        "base/modules/extra/files/org.example.maxfiles.plist",
        &AGENT_PLIST.replace("org.example.maxfiles", "org.example.other"),
    );
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("declares another label than org.example.maxfiles"),
        "{}",
        stderr(&o)
    );
    assert!(!stderr(&o).contains("org.example.other"), "{}", stderr(&o));
    assert!(!fx.log().contains("launchctl"), "{}", fx.log());
    fx.put(
        "base/modules/extra/files/org.example.maxfiles.plist",
        AGENT_PLIST,
    );
    // A bad label is a manifest error too, before any launcher runs.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("label = \"org.example.maxfiles\"", "label = \"a b\""),
    );
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
    assert!(!fx.log().contains("launchctl"), "{}", fx.log());
    fx.put("base/modules/extra/module.toml", EXTRA);
    // A prerequisite that changed between the preview and the run makes the approved plan
    // stale: the plist link now points elsewhere, so nothing is loaded.
    let d = digest_of(&fx.preview(&[]));
    let plist = fx.home.path().join(PLIST);
    fs::remove_file(&plist).unwrap();
    std::os::unix::fs::symlink(
        fx.root.join("base/modules/extra/files/sketchybarrc"),
        &plist,
    )
    .unwrap();
    let o = fx.services(&d, &[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!fx.log().contains("bootstrap"), "{}", fx.log());
    // The link stays and only the content behind it changes (a file outside the sources, so
    // the source tree is unchanged too): the definition's content is a precondition, so the
    // approved plan is stale and nothing is loaded.
    let copy = fx.fake.path().join("maxfiles.plist");
    fs::write(&copy, AGENT_PLIST).unwrap();
    fs::remove_file(&plist).unwrap();
    std::os::unix::fs::symlink(&copy, &plist).unwrap();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains("the definition runs /bin/launchctl limit maxfiles 65536 2097152"),
        "{}",
        text(&o.stdout)
    );
    let d = digest_of(&o);
    fs::write(&copy, AGENT_PLIST.replace("65536", "65537")).unwrap();
    let o = fx.services(&d, &[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!fx.log().contains("bootstrap"), "{}", fx.log());
    // A config file the bar service declares must be published too.
    fs::remove_file(&plist).unwrap();
    std::os::unix::fs::symlink(
        fx.root
            .join("base/modules/extra/files/org.example.maxfiles.plist"),
        &plist,
    )
    .unwrap();
    fs::remove_file(fx.home.path().join(".config/sketchybar/sketchybarrc")).unwrap();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("depends on .config/sketchybar/sketchybarrc, which is not published"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_sensitive_prerequisite_is_recorded_by_mode_only() {
    use sha2::{Digest, Sha256};
    let fx = Fx::new();
    // The bar reads a secret output too. A secrets plan renders it; here it is declared as a
    // prerequisite, so the planner observes it, and nothing may hash or show its content.
    fx.put(
        "base/modules/extra/module.toml",
        &format!(
            "{}\n[[resources]]\nid = \"bar-secret\"\nkind = \"secret\"\nadapter = \"op-template/v1\"\ntarget = \".config/sketchybar/secret.env\"\n[resources.value]\ntemplate = \"files/secret.tpl\"\naccount = \"my.1password.com\"\nmode = 0o600\n",
            EXTRA.replace(
                "config = [\".config/sketchybar/sketchybarrc\"]",
                "config = [\".config/sketchybar/sketchybarrc\", \".config/sketchybar/secret.env\"]"
            )
        ),
    );
    fx.put(
        "base/modules/extra/files/secret.tpl",
        "TOKEN={{ op://v/i/t }}\n",
    );
    fx.publish();
    let secret = fx.home.path().join(".config/sketchybar/secret.env");
    let content = "TOKEN=hunter2-rendered-value\n";
    fs::write(&secret, content).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
    fx.mark("brew-sketchybar", "none\n");
    fx.mark(
        "loaded",
        &format!("{LABEL} {} stopped\n", fx.home.path().join(PLIST).display()),
    );
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("planned {BAR_OP}")), "{t}");
    assert!(
        t.contains(
            "after: extra.bar-secret@scope, extra.sketchybar-pkg@scope, extra.sketchybarrc@scope"
        ),
        "{t}"
    );
    let mut everything = t.clone();
    let o = fx.services(&digest_of(&o), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("updated {BAR_OP} reload=reloaded")),
        "{}",
        text(&o.stdout)
    );
    everything.push_str(&text(&o.stdout));
    // The content changed and the mode did not: the prerequisite still holds, because the
    // plan recorded the mode only. A mode open to others is a change: the plan approved with
    // the old mode is stale, and the only difference between the two runs is the mode.
    fs::write(&secret, "TOKEN=other\n").unwrap();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("current {BAR_OP}")),
        "{}",
        text(&o.stdout)
    );
    everything.push_str(&text(&o.stdout));
    let d = digest_of(&o);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    let o = fx.services(&d, &[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    // Nothing printed, journaled or stored holds the content or its plain hash.
    everything.push_str(&fx.journal());
    everything.push_str(&fx.log());
    for r in fx.receipts() {
        everything.push_str(&r.to_string());
    }
    everything.push_str(&text(&o.stdout));
    everything.push_str(&stderr(&o));
    assert!(!everything.contains("hunter2"), "{everything}");
    assert!(!everything.contains(&hash), "{everything}");
}

#[test]
fn a_secret_definition_shows_neither_its_hash_nor_its_arguments() {
    use sha2::{Digest, Sha256};
    let fx = Fx::new();
    // The agent's definition is itself a secret output (a token among its arguments): the plan
    // keeps its mode only, shows no program, and records no content hash anywhere.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace(
            "id = \"maxfiles-plist\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"Library/LaunchAgents/org.example.maxfiles.plist\"\n[resources.value]\nsource = \"files/org.example.maxfiles.plist\"",
            "id = \"maxfiles-plist\"\nkind = \"secret\"\nadapter = \"op-template/v1\"\ntarget = \"Library/LaunchAgents/org.example.maxfiles.plist\"\n[resources.value]\ntemplate = \"files/org.example.maxfiles.plist\"\naccount = \"my.1password.com\"\nmode = 0o600",
        ),
    );
    fx.mark("brew-sketchybar", "started\n");
    // The configuration is published without the plist, which `secrets` would render.
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--preview"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let d = digest_of(&o);
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &d]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let plist = fx.home.path().join(PLIST);
    fs::create_dir_all(plist.parent().unwrap()).unwrap();
    let content = AGENT_PLIST.replace("2097152", "TOKEN-hunter2-argument");
    fs::write(&plist, &content).unwrap();
    fs::set_permissions(&plist, fs::Permissions::from_mode(0o600)).unwrap();
    let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("planned {AGENT_OP}")), "{t}");
    assert!(!t.contains("the definition runs"), "{t}");
    let d = digest_of(&o);
    let o = fx.services(&d, &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("updated {AGENT_OP} reload=reloaded")),
        "{}",
        text(&o.stdout)
    );
    let mut everything = t.clone();
    everything.push_str(&text(&o.stdout));
    everything.push_str(&stderr(&o));
    everything.push_str(&fx.journal());
    for r in fx.receipts() {
        everything.push_str(&r.to_string());
    }
    assert!(!everything.contains("hunter2"), "{everything}");
    assert!(!everything.contains(&hash), "{everything}");
    // A mode change is still a stale plan.
    fx.mark("loaded", "");
    let d = digest_of(&fx.preview(&[]));
    fs::set_permissions(&plist, fs::Permissions::from_mode(0o644)).unwrap();
    let o = fx.services(&d, &[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!stderr(&o).contains(&hash), "{}", stderr(&o));
    // A template error that puts a token in the Label slot: the blocker names the declared
    // label only, never the one found in the file.
    fs::set_permissions(&plist, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &plist,
        AGENT_PLIST.replace("org.example.maxfiles", "TOKEN-hunter2-label"),
    )
    .unwrap();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("declares another label than org.example.maxfiles"),
        "{}",
        stderr(&o)
    );
    let shown = format!("{}{}", text(&o.stdout), stderr(&o));
    assert!(!shown.contains("hunter2"), "{shown}");
}

#[test]
fn a_large_config_file_is_recorded_by_size_and_still_applies() {
    let fx = Fx::new();
    fx.publish();
    fx.mark("brew-sketchybar", "none\n");
    fx.mark(
        "loaded",
        &format!("{LABEL} {} stopped\n", fx.home.path().join(PLIST).display()),
    );
    // A config file over the 16 MiB hash cap is observed by size on both sides: the plan is not
    // stale against itself, and the start runs.
    let rc = fx.home.path().join(".config/sketchybar/sketchybarrc");
    fs::remove_file(&rc).unwrap();
    fs::write(&rc, vec![b'#'; (16 << 20) + 1]).unwrap();
    let o = fx.preview(&[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.services(&digest_of(&o), &[]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("updated {BAR_OP} reload=reloaded")),
        "{}",
        text(&o.stdout)
    );
}
