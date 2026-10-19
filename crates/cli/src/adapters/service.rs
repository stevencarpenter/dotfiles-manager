//! The service adapters `launchd-agent/v1` and `brew-service/v1` (docs/lifecycle.md): a
//! read-only report from the service's own launcher, then a start or restart through that same
//! launcher, never through a competing one. Nothing here stops, unloads for good, uninstalls or
//! removes a service, and nothing reads or writes application data.
//!
//! Every child runs through the engine runner (fixed search path, rebuilt environment, process
//! group, timeout). The launchers' standard error is bounded and never printed or persisted.

use dfm_engine::{Completion, End, ProcOutput, ProcRequest, Runner};
use dfm_model::digest::sha256_hex;
use dfm_model::plan::{Action, Definition, Scope, ServiceFacts, ServiceState};
use dfm_model::resolve::{Resolved, Value};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const LAUNCHD: &str = "launchd-agent/v1";
pub const BREW_SERVICE: &str = "brew-service/v1";

/// A report or an action log is bounded; its content is not kept.
const MAX_OUTPUT: usize = 1 << 20;
/// A definition file is read once, to check its label.
const MAX_DEFINITION: u64 = 1 << 20;
/// `launchctl print` for a service the domain does not hold.
const LAUNCHCTL_NOT_FOUND: i32 = 113;
const POLL: Duration = Duration::from_millis(500);

/// The time bounds of one adapter call: a report, an action, and the wait for the launcher to
/// finish what it accepted (architecture section 10 item 6: an adapter may lower the default).
struct Bounds {
    probe: Duration,
    act: Duration,
    settle: Duration,
}

/// The isolated build reads `DFM_SERVICE_TIMEOUT_SECS` for every bound, so a test of a launcher
/// that hangs or never completes does not wait a minute.
fn bounds() -> Bounds {
    #[cfg(feature = "test-isolation")]
    if let Some(d) = std::env::var("DFM_SERVICE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
    {
        return Bounds {
            probe: d,
            act: d,
            settle: d,
        };
    }
    Bounds {
        probe: Duration::from_secs(60),
        act: Duration::from_secs(120),
        settle: Duration::from_secs(10),
    }
}

/// Homebrew's service commands run with auto-update, analytics and hints off (architecture
/// section 8 item 5). The variables reach only the manager's own `brew` child.
fn brew_env() -> Vec<(String, String)> {
    [
        "HOMEBREW_NO_AUTO_UPDATE",
        "HOMEBREW_NO_ANALYTICS",
        "HOMEBREW_NO_ENV_HINTS",
    ]
    .iter()
    .map(|k| (k.to_string(), "1".to_string()))
    .collect()
}

/// A checked name parameter (architecture section 8 item 3): `allowed` punctuation beside
/// letters and digits, not starting with `-`, at most 128 characters.
fn checked(value: &Value, key: &str, allowed: &[char], what: &str) -> Result<String, Refusal> {
    let n = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Refusal::new(Code::ManifestInvalid, format!("a service needs {key}")))?;
    let ok = !n.is_empty()
        && n.len() <= 128
        && !n.starts_with('-')
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || allowed.contains(&c));
    if !ok {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            format!(
                "{what} must be letters, digits or {}, not start with '-', and be at most 128 characters",
                allowed
                    .iter()
                    .map(|c| format!("'{c}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    Ok(n.to_string())
}

/// The launchd label of an agent (`org.example.thing`).
fn label(value: &Value) -> Result<String, Refusal> {
    checked(value, "label", &['.', '_', '-'], "a launchd label")
}

/// The HOME-relative path of the agent's definition. The resolver validated it as a HOME path.
fn definition(value: &Value) -> Result<String, Refusal> {
    value
        .get("plist")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Refusal::new(Code::ManifestInvalid, "a launchd agent needs plist"))
}

/// The Homebrew formula whose service is meant.
fn formula(value: &Value) -> Result<String, Refusal> {
    checked(value, "name", &['.', '_', '-', '+', '@'], "a formula name")
}

fn request<'a>(
    program: &'a str,
    args: &'a [String],
    env: &'a [(String, String)],
    timeout: Duration,
) -> ProcRequest<'a> {
    ProcRequest {
        program,
        args,
        env,
        stdin: None,
        timeout,
        max_stdout: MAX_OUTPUT,
    }
}

/// Why a child did not end with an exit code the adapter can read.
fn ended(what: &str, out: &ProcOutput, timeout: Duration) -> Result<i32, String> {
    match &out.end {
        End::Exited(c) => Ok(*c),
        End::TimedOut => Err(format!("{what} did not answer within {timeout:?}")),
        End::Signaled => Err(format!("{what} was ended by a signal")),
        End::Stopped(r) => Err(r.detail.clone()),
    }
}

/// A launcher that ran and failed. Its standard error is withheld, as for every provider: the
/// message names its size and the command to run by hand.
fn failed(what: &str, code: i32, out: &ProcOutput, by_hand: &str) -> Refusal {
    Refusal::new(
        Code::IoFailed,
        format!(
            "{what} exited {code}; its {} bytes of standard error are withheld. Run `{by_hand}` by hand to see them",
            out.stderr.len()
        ),
    )
}

/// The bytes of a definition file, read through its link within the bound.
fn definition_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let shown = path.display().to_string();
    let io = |e: std::io::Error| format!("cannot read {shown}: {}", e.kind());
    let file = std::fs::File::open(path).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_DEFINITION + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DEFINITION {
        return Err(format!("{shown} exceeds 1 MiB"));
    }
    Ok(bytes)
}

/// The `<string>` values directly inside the element that follows `<key>name</key>`: one for a
/// string value, each item for an array. Empty when the key is absent or holds another type.
fn strings_after(text: &str, name: &str) -> Vec<String> {
    let Some((_, rest)) = text.split_once(&format!("<key>{name}</key>")) else {
        return Vec::new();
    };
    let rest = rest.trim_start();
    let (body, array) = if let Some(s) = rest.strip_prefix("<string>") {
        (s.split_once("</string>").map_or("", |(v, _)| v), false)
    } else if let Some(a) = rest.strip_prefix("<array>") {
        (a.split_once("</array>").map_or("", |(v, _)| v), true)
    } else {
        return Vec::new();
    };
    let items: Vec<&str> = if array {
        body.split("<string>")
            .skip(1)
            .filter_map(|part| part.split_once("</string>").map(|(v, _)| v))
            .collect()
    } else {
        vec![body]
    };
    items
        .into_iter()
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.contains('<'))
        .map(str::to_string)
        .collect()
}

/// `<key>Label</key>` followed by one `<string>…</string>`, white space between them allowed.
fn label_in(text: &str) -> Option<String> {
    match strings_after(text, "Label").as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// What the definition runs, for the plan: `Program`, else `ProgramArguments` joined by spaces.
fn program_in(text: &str) -> String {
    let mut words = strings_after(text, "Program");
    if words.is_empty() {
        words = strings_after(text, "ProgramArguments");
    }
    if words.is_empty() {
        "(no program)".to_string()
    } else {
        words.join(" ")
    }
}

/// What `launchctl print` reports: the definition the domain loaded and the run state.
struct Loaded {
    path: PathBuf,
    state: String,
}

/// The `path = ` and `state = ` lines of a `launchctl print` report: the top-level fields, which
/// launchd indents with one tab. A nested block (the resource coalition has a `state` too) is
/// indented further and skipped.
fn parse_print(text: &str) -> Option<Loaded> {
    let field = |name: &str| {
        text.lines()
            .filter_map(|l| l.strip_prefix('\t'))
            .filter(|l| !l.starts_with(['\t', ' ']))
            .find_map(|l| {
                let v = l.strip_prefix(name)?.trim_start().strip_prefix('=')?.trim();
                (!v.is_empty()).then(|| v.to_string())
            })
    };
    Some(Loaded {
        path: PathBuf::from(field("path")?),
        state: field("state")?,
    })
}

fn domain(runner: &Runner) -> String {
    format!("gui/{}", runner.uid)
}

/// `launchctl print gui/<uid>/<label>`: the loaded definition and state, or `None` when the
/// domain does not hold the label (exit 113, "Could not find service").
fn launchd_report(runner: &Runner, label: &str) -> Result<Option<Loaded>, String> {
    let b = bounds();
    let args = ["print".to_string(), format!("{}/{label}", domain(runner))];
    let out = runner
        .run(&request("launchctl", &args, &[], b.probe))
        .map_err(|e| e.detail)?;
    let not_found = || {
        String::from_utf8_lossy(&out.stderr).contains("Could not find service")
            || String::from_utf8_lossy(&out.stdout).contains("Could not find service")
    };
    match ended("launchctl print", &out, b.probe)? {
        0 => parse_print(&String::from_utf8_lossy(&out.stdout))
            .map(Some)
            .ok_or_else(|| {
                format!("launchctl print for {label} did not report a path and a state")
            }),
        LAUNCHCTL_NOT_FOUND => Ok(None),
        _ if not_found() => Ok(None),
        c => Err(format!("launchctl print exited {c} for {label}")),
    }
}

fn only(state: ServiceState) -> ServiceFacts {
    ServiceFacts {
        state,
        definition: None,
    }
}

/// The state of a launchd user agent: its definition must be published under HOME and declare
/// the label; the agent is current when the domain loaded that very file (compared after
/// resolving links, as launchd reports the resolved path). Loaded from another file is foreign:
/// the manager did not load it and never unloads it. The run state is reported, not required: a
/// one-shot agent is not running after it ran. The definition's content hash and program go to
/// the plan.
fn launchd_state(runner: &Runner, value: &Value) -> Result<ServiceFacts, Refusal> {
    let label = label(value)?;
    let plist = definition(value)?;
    let abs = runner.home.join(&plist);
    let canonical = match std::fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(only(ServiceState::Missing(format!(
                "its definition {plist} is not published"
            ))));
        }
        Err(e) => {
            return Ok(only(ServiceState::Unknown(format!(
                "cannot resolve {plist}: {}",
                e.kind()
            ))));
        }
    };
    let bytes = match definition_bytes(&canonical) {
        Ok(b) => b,
        Err(why) => return Ok(only(ServiceState::Unknown(why))),
    };
    let text = String::from_utf8_lossy(&bytes);
    // The found label is never repeated: the definition can be a secret output, and a template
    // error can put a token in the Label slot. The declared label is manifest data.
    match label_in(&text) {
        Some(l) if l == label => {}
        Some(_) => {
            return Ok(only(ServiceState::Refused(format!(
                "{plist} declares another label than {label}; loading it would load another agent"
            ))));
        }
        None => {
            return Ok(only(ServiceState::Refused(format!(
                "{plist} is not an XML property list with a Label string; the adapter cannot check which agent it defines"
            ))));
        }
    }
    let definition = Some(Definition {
        path: plist,
        sha256: sha256_hex(&bytes),
        program: program_in(&text),
    });
    let state = match launchd_report(runner, &label) {
        Err(why) => ServiceState::Unknown(why),
        Ok(None) => ServiceState::Stopped("definition published, not loaded".into()),
        Ok(Some(l)) => {
            let run = if l.state == "running" {
                "running"
            } else {
                "not running"
            };
            if std::fs::canonicalize(&l.path).is_ok_and(|p| p == canonical) {
                ServiceState::Current(format!("loaded from the declared definition, {run}"))
            } else {
                ServiceState::Foreign(format!(
                    "loaded from a definition the manager does not own ({}), {run}; unload it with `launchctl bootout {}/{label}`",
                    l.path.display(),
                    domain(runner)
                ))
            }
        }
    };
    Ok(ServiceFacts { state, definition })
}

/// What `brew services info --json` reports for one formula.
struct BrewInfo {
    loaded: bool,
    running: bool,
    status: String,
}

/// The first object of a `brew services info --json` array, or `None` when the text is not
/// that shape.
fn parse_brew_info(text: &str) -> Option<BrewInfo> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let e = v.as_array()?.first()?;
    Some(BrewInfo {
        loaded: e.get("loaded")?.as_bool()?,
        running: e.get("running")?.as_bool()?,
        status: e.get("status")?.as_str()?.to_string(),
    })
}

/// `brew services info <name> --json`, or `None` when the formula is not installed (brew's
/// `No available formula` and `is not installed` errors).
/// shortcut: "not installed" is read from brew's error text, upgrade when `brew services info`
/// answers in a structured form for a missing formula.
fn brew_report(runner: &Runner, name: &str) -> Result<Option<BrewInfo>, String> {
    let b = bounds();
    let args = [
        "services".to_string(),
        "info".into(),
        name.into(),
        "--json".into(),
    ];
    let out = runner
        .run(&request("brew", &args, &brew_env(), b.probe))
        .map_err(|e| e.detail)?;
    let err = String::from_utf8_lossy(&out.stderr);
    match ended("brew services info", &out, b.probe)? {
        0 => parse_brew_info(&String::from_utf8_lossy(&out.stdout))
            .map(Some)
            .ok_or_else(|| {
                format!("brew services info --json for {name} is not the array the adapter reads")
            }),
        _ if err.contains("No available formula") || err.contains("is not installed") => Ok(None),
        c => Err(format!("brew services info exited {c} for {name}")),
    }
}

/// The state of a Homebrew service: installed (brew knows the formula), loaded (launchd holds
/// brew's definition) and running (a process). A service is current only when it runs: a
/// Homebrew service is a daemon. Loaded and not running is stale (it exited or failed); the
/// manager did not start that process, so the planner blocks until the user requests the
/// restart.
fn brew_state(runner: &Runner, value: &Value) -> Result<ServiceFacts, Refusal> {
    let name = formula(value)?;
    Ok(only(match brew_report(runner, &name) {
        Err(why) => ServiceState::Unknown(why),
        Ok(None) => ServiceState::Missing(format!(
            "the formula {name} is not installed; packages installs it"
        )),
        Ok(Some(i)) if i.loaded && i.running => {
            ServiceState::Current("installed, loaded, running".into())
        }
        Ok(Some(i)) if i.loaded => ServiceState::Stale(format!(
            "installed, loaded, not running (status {})",
            i.status
        )),
        Ok(Some(_)) => ServiceState::Stopped("installed, not loaded".into()),
    }))
}

/// The launcher's report for one service by its adapter, with the definition facts when the
/// adapter read one. A bad declaration is `E_MANIFEST_INVALID`.
pub fn probe(runner: &Runner, adapter: &str, value: &Value) -> Result<ServiceFacts, Refusal> {
    match adapter {
        LAUNCHD => launchd_state(runner, value),
        BREW_SERVICE => brew_state(runner, value),
        other => Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no service launcher for {other}"),
        )),
    }
}

/// A stop request seen while the adapter waits between two launcher calls.
fn stop_requested() -> Result<(), Refusal> {
    dfm_engine::signal::requested().map_or(Ok(()), Err)
}

/// Runs one launcher action to its end. An exit outside `ok` is a failure with the error
/// withheld.
fn run_action(
    runner: &Runner,
    program: &str,
    args: &[String],
    env: &[(String, String)],
    ok: &[i32],
    by_hand: &str,
) -> Result<(), Refusal> {
    let b = bounds();
    let what = format!("{program} {}", args.join(" "));
    let out = runner.run(&request(program, args, env, b.act))?;
    match out.end {
        End::Exited(c) if ok.contains(&c) => Ok(()),
        End::Exited(c) => Err(failed(&what, c, &out, by_hand)),
        End::TimedOut => Err(Refusal::new(
            Code::IoFailed,
            format!("{what} did not finish within {:?}", b.act),
        )),
        End::Signaled => Err(Refusal::new(
            Code::IoFailed,
            format!("{what} was ended by a signal"),
        )),
        End::Stopped(r) => Err(r),
    }
}

/// `launchctl bootstrap gui/<uid> <definition>`: loads the agent. For a restart, `launchctl
/// bootout gui/<uid>/<label>` first (a label the domain does not hold is accepted), then a
/// bounded wait until the domain no longer holds it, because launchd tears a service down
/// asynchronously and a bootstrap during the teardown fails. `kickstart` is not used: it restarts
/// the process under the definition launchd already holds, which is not a reload of an edited
/// definition.
fn launchd_act(runner: &Runner, value: &Value, action: Action) -> Result<Completion, Refusal> {
    let label = label(value)?;
    let plist = definition(value)?;
    let abs = runner.home.join(&plist);
    let dom = domain(runner);
    let target = format!("{dom}/{label}");
    let by_hand = format!("launchctl print {target}");
    if action == Action::Restart {
        let args = ["bootout".to_string(), target.clone()];
        run_action(
            runner,
            "launchctl",
            &args,
            &[],
            &[0, LAUNCHCTL_NOT_FOUND],
            &by_hand,
        )?;
        let b = bounds();
        let start = Instant::now();
        loop {
            stop_requested()?;
            match launchd_report(runner, &label) {
                Ok(None) => break,
                Ok(Some(_)) if start.elapsed() < b.settle => std::thread::sleep(POLL),
                Ok(Some(_)) => {
                    return Err(Refusal::new(
                        Code::IoFailed,
                        format!(
                            "{label} was still loaded {:?} after bootout; nothing was loaded. Run `{by_hand}` by hand",
                            b.settle
                        ),
                    ));
                }
                Err(why) => return Err(Refusal::new(Code::ProbeFailed, why)),
            }
        }
    }
    let args = [
        "bootstrap".to_string(),
        dom,
        abs.to_string_lossy().into_owned(),
    ];
    run_action(runner, "launchctl", &args, &[], &[0], &by_hand)?;
    Ok(Completion::Completed)
}

/// `brew services start|restart <name>`, then a bounded wait for brew to report the service
/// running. Brew returns once launchd accepted the definition, before the process is up: a
/// service that is loaded and not yet running when the bound elapses is `Queued`, never a
/// success. One that is not even loaded is reported as completed, so the caller's postcondition
/// fails on the launcher's own report.
fn brew_act(runner: &Runner, value: &Value, action: Action) -> Result<Completion, Refusal> {
    let name = formula(value)?;
    let verb = if action == Action::Restart {
        "restart"
    } else {
        "start"
    };
    let args = ["services".to_string(), verb.into(), name.clone()];
    let by_hand = format!("brew services info {name}");
    run_action(runner, "brew", &args, &brew_env(), &[0], &by_hand)?;
    let b = bounds();
    let start = Instant::now();
    loop {
        stop_requested()?;
        match brew_report(runner, &name) {
            Ok(Some(i)) if i.loaded && i.running => return Ok(Completion::Completed),
            Ok(Some(i)) if start.elapsed() >= b.settle => {
                return Ok(if i.loaded {
                    Completion::Queued(format!(
                        "loaded, not running {:?} after the {verb} (status {})",
                        b.settle, i.status
                    ))
                } else {
                    Completion::Completed
                });
            }
            Ok(Some(_)) => std::thread::sleep(POLL),
            Ok(None) => return Ok(Completion::Completed),
            Err(why) => return Err(Refusal::new(Code::ProbeFailed, why)),
        }
    }
}

/// Starts or restarts one service through its own launcher, as `action` says. The caller probes
/// again afterwards.
pub fn act(
    runner: &Runner,
    adapter: &str,
    value: &Value,
    action: Action,
) -> Result<Completion, Refusal> {
    match adapter {
        LAUNCHD => launchd_act(runner, value, action),
        BREW_SERVICE => brew_act(runner, value, action),
        other => Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no service launcher for {other}"),
        )),
    }
}

/// Probes every selected service resource for a services plan, by resource id. A stop request
/// ends the probing: the probe it arrived in is ended by the runner, no further launcher is
/// started, and the request is the error.
pub fn service_facts(
    resolved: &Resolved,
    runner: &Runner,
) -> Result<BTreeMap<String, ServiceFacts>, Refusal> {
    let mut out = BTreeMap::new();
    for r in resolved
        .resources
        .values()
        .filter(|r| Scope::Services.covers(r))
    {
        if let Some(stop) = dfm_engine::signal::requested() {
            return Err(stop);
        }
        let facts = match probe(runner, &r.adapter, &r.value) {
            Ok(f) => f,
            Err(e) if e.code == Code::ManifestInvalid => only(ServiceState::Refused(e.detail)),
            Err(e) => only(ServiceState::Unknown(e.detail)),
        };
        out.insert(r.id.clone(), facts);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(key: &str, s: &str) -> Value {
        let mut t = dfm_model::resolve::Table::new();
        t.insert(key.into(), Value::String(s.into()));
        Value::Table(t)
    }

    #[test]
    fn a_label_and_a_formula_are_checked_parameters() {
        assert_eq!(
            label(&v("label", "org.example.maxfiles")).unwrap(),
            "org.example.maxfiles"
        );
        for bad in ["", "-x", "a b", "a;b", "a/b", &"a".repeat(129)] {
            assert_eq!(
                label(&v("label", bad)).unwrap_err().code,
                Code::ManifestInvalid,
                "{bad:?}"
            );
        }
        assert_eq!(formula(&v("name", "sketchybar")).unwrap(), "sketchybar");
        assert!(formula(&v("name", "a/b")).is_err());
        assert!(formula(&v("label", "x")).is_err());
    }

    #[test]
    fn the_label_of_an_xml_definition_is_read_and_a_binary_one_is_not() {
        let xml = "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>org.example.maxfiles</string>\n\t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n";
        assert_eq!(label_in(xml).as_deref(), Some("org.example.maxfiles"));
        assert_eq!(
            label_in("<key>Label</key><string> a.b </string>").as_deref(),
            Some("a.b")
        );
        assert_eq!(program_in(xml), "(no program)");
        let args = "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/bin/launchctl</string>\n\t\t<string>limit</string>\n\t</array>\n<key>RunAtLoad</key><true/>";
        assert_eq!(program_in(args), "/bin/launchctl limit");
        assert_eq!(
            program_in("<key>Program</key><string>/usr/bin/true</string>"),
            "/usr/bin/true"
        );
        for bad in [
            "bplist00\u{0}",
            "<key>Label</key><integer>1</integer>",
            "<key>Label</key><string></string>",
            "<key>Label</key><string><x/></string>",
            "",
        ] {
            assert_eq!(label_in(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_launchctl_report_yields_its_path_and_state() {
        let text = "gui/501/org.example.x = {\n\tactive count = 0\n\tpath = /Users/a/Library/LaunchAgents/org.example.x.plist\n\ttype = LaunchAgent\n\tstate = not running\n\n\tprogram = /bin/launchctl\n\tresource coalition = {\n\t\tstate = active\n\t}\n}\n";
        let l = parse_print(text).unwrap();
        assert_eq!(
            l.path,
            PathBuf::from("/Users/a/Library/LaunchAgents/org.example.x.plist")
        );
        assert_eq!(l.state, "not running");
        // A nested block before the top-level state does not stand in for it.
        let nested = "gui/501/x = {\n\tpath = /p\n\tresource coalition = {\n\t\tstate = active\n\t}\n\tstate = running\n}\n";
        assert_eq!(parse_print(nested).unwrap().state, "running");
        assert!(
            parse_print(
                "gui/501/x = {\n\tpath = /p\n\tcoalition = {\n\t\tstate = active\n\t}\n}\n"
            )
            .is_none()
        );
        assert!(parse_print("Bad request.\n").is_none());
        assert!(parse_print("path = /x\nstate = running\n").is_none());
    }

    #[test]
    fn a_brew_services_report_is_read_as_json_only() {
        let ok =
            r#"[{"name":"sketchybar","running":true,"loaded":true,"pid":4031,"status":"started"}]"#;
        let i = parse_brew_info(ok).unwrap();
        assert!(i.loaded && i.running);
        assert_eq!(i.status, "started");
        for bad in [
            "",
            "sketchybar started",
            "[]",
            r#"[{"name":"x","loaded":true}]"#,
            r#"{"loaded":true,"running":true,"status":"x"}"#,
        ] {
            assert!(parse_brew_info(bad).is_none(), "{bad:?}");
        }
    }
}
