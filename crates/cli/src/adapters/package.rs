//! The package adapters `homebrew/v1`, `mise/v1`, `apt/v1` and `uv-tool/v1` (docs/packages.md,
//! docs/toolchains.md): a read-only probe of the declared provider's own listing, an owner check
//! by executable name, and an install through the same provider. Nothing here reads or writes a
//! version pin: a package plan makes a declared package present and never upgrades, downgrades
//! or removes. A rolling update ([`maintain`], [`upgrade`]) asks mise or uv for its newest
//! eligible version of a rolling tool and installs it beside the installed ones.
//!
//! Every child runs through the engine runner (fixed search path, rebuilt environment, process
//! group, timeout). The providers' standard error is bounded and never printed or persisted.

use super::checkout;
use super::script::{self, ScriptCx};
use dfm_engine::{End, ProcOutput, ProcRequest, Runner};
use dfm_model::plan::{PackageState, Scope};
use dfm_model::resolve::{Resolved, ResolvedResource, Value};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const HOMEBREW: &str = "homebrew/v1";
pub const MISE: &str = "mise/v1";
pub const APT: &str = "apt/v1";
pub const UV_TOOL: &str = "uv-tool/v1";

pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A listing or an install log is bounded; its content is not kept.
const MAX_OUTPUT: usize = 1 << 20;

/// Homebrew probes and installs disable auto-update, analytics, hints and cleanup
/// (docs/packages.md), the upgrade of an outdated requested formula, and the
/// upgrade or reinstall of its installed dependents: a package plan installs what is missing and
/// changes nothing it did not list (DESIGN.md: a plan contains no implicit upgrades). These
/// variables reach only the manager's own `brew` child, never the user's shell.
fn brew_env() -> Vec<(String, String)> {
    [
        "HOMEBREW_NO_AUTO_UPDATE",
        "HOMEBREW_NO_ANALYTICS",
        "HOMEBREW_NO_ENV_HINTS",
        "HOMEBREW_NO_INSTALL_CLEANUP",
        "HOMEBREW_NO_INSTALL_UPGRADE",
        "HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK",
        // `brew uninstall` runs autoremove unless this is set: purge removes only what it names.
        "HOMEBREW_NO_AUTOREMOVE",
    ]
    .iter()
    .map(|k| (k.to_string(), "1".to_string()))
    .collect()
}

/// mise probes and installs answer no prompt, install nothing a listing asks about, and load no
/// mise configuration file. Config evaluation can run hooks (a `postinstall` hook runs after an
/// install), env sources and remote includes, none of which the plan shows or the digest covers.
/// Installed versions are listed from mise's data directory, which needs no config. Without
/// config, `mise install tool` installs mise's newest version of the tool ("the provider's
/// current version" of the plan) and `mise install tool@version` that version.
fn mise_env() -> Vec<(String, String)> {
    vec![
        ("MISE_YES".into(), "1".into()),
        ("MISE_NOT_FOUND_AUTO_INSTALL".into(), "false".into()),
        ("MISE_NO_CONFIG".into(), "1".into()),
    ]
}

/// uv probes and changes read no uv configuration file (an index or a source override there
/// is not in the plan), show no progress, and never download a Python interpreter: a tool
/// whose interpreter is missing fails instead of installing a runtime the plan does not show.
/// The probe (`uv tool list` reads uv's own tool directory) also runs offline.
fn uv_env(offline: bool) -> Vec<(String, String)> {
    let mut env = vec![
        ("UV_NO_CONFIG".to_string(), "1".to_string()),
        ("UV_NO_PROGRESS".into(), "1".into()),
        ("UV_PYTHON_DOWNLOADS".into(), "never".into()),
    ];
    if offline {
        env.push(("UV_OFFLINE".into(), "1".into()));
    }
    env
}

/// The package name as a checked parameter: letters, digits,
/// `.`, `_`, `-`, `+`, `@`, `/` and `:` (a Homebrew tap, a version suffix or a mise backend
/// prefix such as `npm:`), not starting with `-`, at most 128 characters.
pub fn name(value: &Value) -> Result<String, Refusal> {
    let n = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Refusal::new(Code::ManifestInvalid, "a package needs name"))?;
    if !valid_name(n) {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            "a package name must be letters, digits, '.', '_', '-', '+', '@', '/' or ':', not start with '-', and be at most 128 characters",
        ));
    }
    Ok(n.to_string())
}

/// The declared `update_policy`, one of `rolling`, `hold` and `native-request`. Any other
/// value is a manifest error, so a typo is refused instead of holding the package silently.
fn policy(value: &Value) -> Result<&str, Refusal> {
    let p = value
        .get("update_policy")
        .and_then(Value::as_str)
        .unwrap_or("unset");
    if matches!(p, "rolling" | "hold" | "native-request") {
        Ok(p)
    } else {
        Err(Refusal::new(
            Code::ManifestInvalid,
            format!("update_policy must be rolling, hold or native-request, not {p}"),
        ))
    }
}

/// Whether `n` is a package name the adapters pass to a provider as an argument.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 128
        && !n.starts_with('-')
        && n.split('/').all(|c| !matches!(c, "" | "." | ".."))
        && n.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@' | '/' | ':')
        })
}

pub(super) fn request<'a>(
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
pub(super) fn ended(what: &str, out: &ProcOutput) -> Option<String> {
    match &out.end {
        End::Exited(_) => None,
        End::TimedOut => Some(format!("{what} did not answer within {PROBE_TIMEOUT:?}")),
        End::Signaled => Some(format!("{what} was ended by a signal")),
        End::Stopped(r) => Some(r.detail.clone()),
    }
}

/// Homebrew's install roots. `brew --cellar` and `brew --caskroom` print a path and load no
/// formula or cask, so they are safe before approval for any package, a tap's included.
fn brew_roots(runner: &Runner) -> Result<(PathBuf, PathBuf), String> {
    let env = brew_env();
    let mut roots = Vec::new();
    for flag in ["--cellar", "--caskroom"] {
        let args = [flag.to_string()];
        let out = runner
            .run(&request("brew", &args, &env, PROBE_TIMEOUT))
            .map_err(|e| e.detail)?;
        if let Some(why) = ended(&format!("brew {flag}"), &out) {
            return Err(why);
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !matches!(out.end, End::Exited(0)) || !path.starts_with('/') {
            return Err(format!("brew {flag} did not print an absolute path"));
        }
        roots.push(PathBuf::from(path));
    }
    let caskroom = roots.pop().unwrap_or_default();
    let cellar = roots.pop().unwrap_or_default();
    Ok((cellar, caskroom))
}

/// The versions installed under `root/<name>`: the names of its version directories, sorted.
/// Only directory entries are read; no rack is no version. A name is a checked package name, so
/// it never leaves `root`.
fn rack_versions(root: &Path, name: &str) -> Result<Vec<String>, String> {
    let rack = root.join(name);
    let shown = |e: std::io::Error| format!("cannot read {}: {}", rack.display(), e.kind());
    let entries = match std::fs::read_dir(&rack) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(shown(e)),
    };
    let mut versions = Vec::new();
    for e in entries {
        let e = e.map_err(shown)?;
        let dir = e.file_type().map_err(shown)?.is_dir();
        match e.file_name().to_str() {
            Some(n) if dir && !n.starts_with('.') => versions.push(n.to_string()),
            _ => {}
        }
    }
    versions.sort();
    Ok(versions)
}

/// Whether the declared package is installed by Homebrew: a version directory under its rack in
/// the Cellar (a formula) or the Caskroom (a cask), as `brew list` itself decides. Reading the
/// directories loads no formula or cask, so a tap's code never runs here. `homebrew/core/` names
/// a formula, `homebrew/cask/` a cask. A rack is keyed by the short name only, so for a
/// tap-qualified name the install receipts must name that tap: a keg from another tap is another
/// owner, and a receipt that cannot be read fails closed. A bare name accepts any tap.
fn brew_probe(runner: &Runner, pkg: &str) -> PackageState {
    let (cellar, caskroom) = match brew_roots(runner) {
        Ok(r) => r,
        Err(why) => return PackageState::Unknown(why),
    };
    let short = pkg.rsplit('/').next().unwrap_or(pkg);
    let tap = pkg.rsplit_once('/').map(|(t, _)| t.to_ascii_lowercase());
    let roots = match tap.as_deref() {
        Some("homebrew/core") => vec![(cellar, false)],
        Some("homebrew/cask") => vec![(caskroom, true)],
        _ => vec![(cellar, false), (caskroom, true)],
    };
    let (mut versions, mut receipts) = (Vec::new(), Vec::new());
    for (root, cask) in &roots {
        let rack = root.join(short);
        match rack_versions(root, short) {
            Ok(v) if v.is_empty() => {}
            // A cask keeps one receipt for the rack, a formula one per keg.
            Ok(v) if *cask => {
                receipts.push(rack.join(".metadata/INSTALL_RECEIPT.json"));
                versions.extend(v);
            }
            Ok(v) => {
                receipts.extend(
                    v.iter()
                        .map(|ver| rack.join(ver).join("INSTALL_RECEIPT.json")),
                );
                versions.extend(v);
            }
            Err(why) => return PackageState::Unknown(why),
        }
    }
    if versions.is_empty() {
        return PackageState::Missing(Vec::new());
    }
    if let Some(want) = &tap {
        for r in &receipts {
            match receipt_tap(r) {
                Ok(t) if t == *want => {}
                Ok(t) => {
                    return PackageState::OtherOwner(format!(
                        "{short} is installed from the tap {t}, not from {want}"
                    ));
                }
                Err(why) => return PackageState::Unknown(why),
            }
        }
    }
    PackageState::Installed(versions.join(" "))
}

/// The official taps: a package named in one is written by its short name in a Brewfile.
const OFFICIAL_TAPS: [&str; 2] = ["homebrew/core", "homebrew/cask"];

/// The name a Brewfile line gives the recorded brew package, and every spelling of that package
/// a Brewfile can already hold. An official package is written by its short name (`tuicr`, from
/// `homebrew/core/tuicr` too). A third-party tap package keeps `owner/tap/name`: its short name
/// would install another formula, or none, on a fresh machine. A version suffix (`node@20`) is
/// part of the formula name and is kept.
pub(crate) fn brewfile_names(pkg: &str) -> (String, Vec<String>) {
    match pkg.rsplit_once('/') {
        Some((tap, _)) if !OFFICIAL_TAPS.contains(&tap.to_ascii_lowercase().as_str()) => {
            (pkg.to_string(), vec![pkg.to_string()])
        }
        found => {
            let short = found.map_or(pkg, |(_, s)| s);
            let mut all = vec![short.to_string()];
            all.extend(OFFICIAL_TAPS.iter().map(|t| format!("{t}/{short}")));
            (short.to_string(), all)
        }
    }
}

/// Whether brew holds `pkg` as a cask: a name pinned to `homebrew/cask/`, or a rack in the
/// Caskroom and none in the Cellar. Reads the directories only, as [`brew_probe`] does.
pub(crate) fn brew_is_cask(runner: &Runner, pkg: &str) -> Result<bool, Refusal> {
    match pkg
        .rsplit_once('/')
        .map(|(t, _)| t.to_ascii_lowercase())
        .as_deref()
    {
        Some("homebrew/cask") => return Ok(true),
        Some("homebrew/core") => return Ok(false),
        _ => {}
    }
    let failed = |why: String| Refusal::new(Code::ProbeFailed, why);
    let (cellar, caskroom) = brew_roots(runner).map_err(failed)?;
    let short = pkg.rsplit('/').next().unwrap_or(pkg);
    let has = |root: &Path| {
        rack_versions(root, short)
            .map(|v| !v.is_empty())
            .map_err(failed)
    };
    Ok(!has(&cellar)? && has(&caskroom)?)
}

/// The tap an install receipt names (`source.tap`, lowercase). The file is read without
/// following a link and within 1 MiB.
fn receipt_tap(path: &Path) -> Result<String, String> {
    const LIMIT: u64 = 1 << 20;
    let shown = path.display().to_string();
    let bytes = crate::read_fs::read_regular_bounded(
        path,
        LIMIT,
        |e| Refusal::new(Code::IoFailed, format!("cannot read {shown}: {}", e.kind())),
        || Refusal::new(Code::IoFailed, format!("{shown} is not a regular file")),
    )
    .map_err(|r| r.detail)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > LIMIT {
        return Err(format!("{shown} exceeds 1 MiB"));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| format!("{shown} is not JSON"))?;
    v.get("source")
        .and_then(|s| s.get("tap"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| format!("{shown} names no source tap"))
}

/// An official Homebrew package, pinned to the namespace it was found in. Every later brew call
/// names it this way, so brew cannot resolve the same short name to a tap's formula instead.
#[derive(Debug, PartialEq, Eq)]
enum Official {
    Formula(String),
    Cask(String),
}

impl Official {
    /// The arguments that name it: `homebrew/core/<name>`, or `--cask <name>`.
    fn args(&self) -> Vec<String> {
        match self {
            Official::Formula(n) => vec![core(n)],
            Official::Cask(n) => vec!["--cask".into(), n.clone()],
        }
    }
}

/// A formula name pinned to Homebrew's core namespace.
fn core(name: &str) -> String {
    format!("homebrew/core/{name}")
}

/// Whether `pkg` is an official Homebrew formula or cask, without loading any formula file:
/// `brew formulae` and `brew casks` print the names Homebrew knows, official ones bare and tap
/// ones qualified (`user/tap/name`). A tap's formula is Ruby that runs when brew reads it, so the
/// dependency calls below (`deps`, `info`, `outdated`) and the install run only for an official
/// package, and name it in the namespace that matched. A name declared as `homebrew/core/<n>` is
/// looked up as a formula only, `homebrew/cask/<n>` as a cask only.
fn brew_official(runner: &Runner, pkg: &str) -> Result<Option<Official>, String> {
    let (short, lists): (&str, &[&str]) = match pkg.rsplit_once('/') {
        None => (pkg, &["formulae", "casks"]),
        Some(("homebrew/core", n)) => (n, &["formulae"]),
        Some(("homebrew/cask", n)) => (n, &["casks"]),
        Some(_) => return Ok(None),
    };
    let env = brew_env();
    for &list in lists {
        let args = [list.to_string()];
        let out = runner
            .run(&request("brew", &args, &env, PROBE_TIMEOUT))
            .map_err(|e| e.detail)?;
        if let Some(why) = ended(&format!("brew {list}"), &out) {
            return Err(why);
        }
        if !matches!(out.end, End::Exited(0)) {
            return Err(format!("brew {list} exited with an error"));
        }
        if String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == short)
        {
            return Ok(Some(match list {
                "formulae" => Official::Formula(short.to_string()),
                _ => Official::Cask(short.to_string()),
            }));
        }
    }
    Ok(None)
}

/// What `brew install <pkg>` would do to the formula's dependencies, from local data only (no
/// download, auto-update is off): `brew deps <pkg>` lists them, their Cellar racks say which are
/// installed, `brew info --json=v2` whether the package and the missing ones have a bottle
/// for this machine, and `brew outdated --json=v2` which installed ones the install would
/// upgrade. The missing ones become part of the plan. A source build blocks it, because its
/// build-time dependencies are not in the listing; so does an outdated dependency.
fn brew_dependencies(runner: &Runner, off: &Official) -> PackageState {
    let pkg = match off {
        Official::Formula(n) | Official::Cask(n) => n.as_str(),
    };
    let env = brew_env();
    let run = |what: &str, args: Vec<String>, ok: &[i32]| -> Result<String, String> {
        let out = runner
            .run(&request("brew", &args, &env, PROBE_TIMEOUT))
            .map_err(|e| e.detail)?;
        if let Some(why) = ended(what, &out) {
            return Err(why);
        }
        match out.end {
            End::Exited(c) if ok.contains(&c) => Ok(String::from_utf8_lossy(&out.stdout).into()),
            End::Exited(c) => Err(format!("{what} exited {c} for {pkg}")),
            _ => Err(format!("{what} gave no exit code")),
        }
    };
    let mut args = vec!["deps".to_string()];
    args.extend(off.args());
    // The dependencies of an official package are official formulae, named bare; every later
    // call pins them to the core namespace too. A cask's cask dependency is not a formula, so
    // its `brew info` fails and the state is Unknown: the check fails closed.
    let deps: Vec<String> = match run("brew deps", args, &[0]) {
        Ok(t) => t.split_whitespace().map(str::to_string).collect(),
        Err(why) => return PackageState::Unknown(why),
    };
    if let Some(bad) = deps.iter().find(|d| !valid_name(d)) {
        return PackageState::Unknown(format!(
            "brew deps named {bad:?}, which is not a package name"
        ));
    }
    // A dependency is installed when its rack in the Cellar holds a version, as for the package.
    let cellar = match brew_roots(runner) {
        Ok((cellar, _)) => cellar,
        Err(why) => return PackageState::Unknown(why),
    };
    let (mut have, mut missing) = (Vec::new(), Vec::new());
    for d in deps {
        match rack_versions(&cellar, &d) {
            Ok(v) if v.is_empty() => missing.push(d),
            Ok(_) => have.push(d),
            Err(why) => return PackageState::Unknown(why),
        }
    }
    // Everything the install would add must come as a bottle brew pours on this machine. Brew
    // decides that itself: `Formula#bottle` falls back to an older macOS bottle and
    // `pour_bottle?` checks the formula's pour conditions. The names are official and pinned to
    // the core namespace, so the formulae load from the API and no tap code runs. A cask is not
    // built, so only its missing formulae are asked about.
    let mut formulae: Vec<String> = Vec::new();
    if let Official::Formula(n) = off {
        formulae.push(core(n));
    }
    formulae.extend(missing.iter().map(|d| core(d)));
    if !formulae.is_empty() {
        let mut args = vec!["ruby".to_string(), "-e".into(), POURS.into()];
        args.extend(formulae.iter().cloned());
        match run("brew ruby", args, &[0]).map(|t| unpoured(&t, &formulae)) {
            Ok(Some(n)) if n.is_empty() => {}
            Ok(Some(n)) => return PackageState::SourceBuild(n),
            Ok(None) => {
                return PackageState::Unknown(format!(
                    "brew did not answer for every formula the install of {pkg} would add"
                ));
            }
            Err(why) => return PackageState::Unknown(why),
        }
    }
    if have.is_empty() {
        return PackageState::Missing(missing);
    }
    // `brew outdated` exits 1 when it finds an outdated one; the JSON is the answer either way.
    let mut args = vec!["outdated".to_string(), "--json=v2".into()];
    args.extend(have.iter().map(|d| core(d)));
    let outdated = match run("brew outdated", args, &[0, 1]).map(|t| outdated_names(&t)) {
        Ok(Some(n)) => n,
        Ok(None) => {
            return PackageState::Unknown(format!(
                "brew outdated --json=v2 for the dependencies of {pkg} is not the answer the adapter reads"
            ));
        }
        Err(why) => return PackageState::Unknown(why),
    };
    if outdated.is_empty() {
        PackageState::Missing(missing)
    } else {
        PackageState::Upgrades(outdated)
    }
}

/// Ruby for `brew ruby -e`, given pinned formula names as arguments: one line `<name> <1|0>` per
/// formula, 1 when brew would pour a bottle on this machine. It reads Homebrew's own formula
/// objects and changes nothing.
const POURS: &str =
    r##"ARGV.each { |n| f = Formula[n]; puts "#{f.name} #{f.bottle && f.pour_bottle? ? 1 : 0}" }"##;

/// The formulae in a `POURS` answer that brew would build from source, or `None` when the answer
/// is not one `<name> <1|0>` line per asked formula, in order.
fn unpoured(text: &str, asked: &[String]) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() != asked.len() {
        return None;
    }
    let mut out = Vec::new();
    for (line, want) in lines.iter().zip(asked) {
        let (name, pours) = line.trim().split_once(' ')?;
        if name != want.rsplit('/').next().unwrap_or(want) {
            return None;
        }
        match pours {
            "1" => {}
            "0" => out.push(name.to_string()),
            _ => return None,
        }
    }
    Some(out)
}

/// The formula and cask names in a `brew outdated --json=v2` answer, or `None` when it is not
/// that shape.
fn outdated_names(text: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();
    for key in ["formulae", "casks"] {
        for e in v.get(key)?.as_array()? {
            out.push(e.get("name")?.as_str()?.to_string());
        }
    }
    Some(out)
}

/// `mise ls --installed --json <tool>`: a JSON array with one `{"version", "installed"}` object
/// per installed version, `[]` when none is installed (JSON, because the table truncates a long
/// tool name). A tool mise does not know is an error (exit 1). A declared
/// `tool@version` is listed by its tool name and counts as installed when an installed version
/// is the declared one or extends it at a component boundary (`node@20` matches `20.20.2`, not
/// `2.1.0`). A selector only mise can resolve is refused: the probe has no network.
fn mise_probe(runner: &Runner, pkg: &str) -> PackageState {
    match mise_listing(runner, pkg) {
        Ok(listed) => mise_state(&listed, pkg),
        Err(state) => state,
    }
}

/// Every installed version of the declared tool, as mise lists them, or the state that stops
/// the probe.
fn mise_listing(runner: &Runner, pkg: &str) -> Result<Vec<String>, PackageState> {
    let (tool, want) = tool_and_version(pkg);
    if symbolic(want) {
        return Err(PackageState::Refused(format!(
            "{pkg}: mise/v1 takes an exact version or a version prefix (node@20, node@20.1), not a selector mise resolves online ({want})"
        )));
    }
    let args = [
        "ls".to_string(),
        "--installed".into(),
        "--json".into(),
        tool.into(),
    ];
    let out = runner
        .run(&request("mise", &args, &mise_env(), PROBE_TIMEOUT))
        .map_err(|e| PackageState::Unknown(e.detail))?;
    if let Some(why) = ended("mise ls", &out) {
        return Err(PackageState::Unknown(why));
    }
    if !matches!(out.end, End::Exited(0)) {
        return Err(PackageState::Unknown(format!(
            "mise ls exited with an error for {pkg} (an unknown tool, or a broken configuration)"
        )));
    }
    installed_versions(&out.stdout).ok_or_else(|| {
        PackageState::Unknown(format!(
            "mise ls --json for {pkg} is not the array of versions the adapter reads"
        ))
    })
}

/// The probe's answer from a listing: the versions that satisfy the declared one, or `Missing`.
fn mise_state(listed: &[String], pkg: &str) -> PackageState {
    let (_, want) = tool_and_version(pkg);
    let versions: Vec<&str> = listed
        .iter()
        .map(String::as_str)
        .filter(|v| version_matches(v, want))
        .collect();
    if versions.is_empty() {
        PackageState::Missing(Vec::new())
    } else {
        PackageState::Installed(versions.join(" "))
    }
}

/// `uv tool list`: one `<name> v<version>` line per installed tool, each followed by its
/// `- <entry point>` lines, or `No tools installed`. A tool is installed in one version, which
/// `uv tool upgrade` replaces. The name is a package-index name: a git, path or URL source, or a
/// version, is refused, because the plan shows no origin for it and uv has no read-only way to
/// tell what such a source would install.
fn uv_probe(runner: &Runner, pkg: &str) -> PackageState {
    if !uv_name(pkg) {
        return PackageState::Refused(format!(
            "{pkg}: uv-tool/v1 takes a package-index name (letters, digits, '.', '_', '-'); a git, path or URL source, or a version, is not supported"
        ));
    }
    let args = ["tool".to_string(), "list".into()];
    let out = match runner.run(&request("uv", &args, &uv_env(true), PROBE_TIMEOUT)) {
        Ok(o) => o,
        Err(e) => return PackageState::Unknown(e.detail),
    };
    if let Some(why) = ended("uv tool list", &out) {
        return PackageState::Unknown(why);
    }
    if !matches!(out.end, End::Exited(0)) {
        return PackageState::Unknown("uv tool list exited with an error".into());
    }
    match uv_tool_version(&String::from_utf8_lossy(&out.stdout), pkg) {
        Some(v) => PackageState::Installed(v),
        None => PackageState::Missing(Vec::new()),
    }
}

/// Whether `n` is a package-index name uv installs by name.
fn uv_name(n: &str) -> bool {
    n.starts_with(|c: char| c.is_ascii_alphanumeric())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The version `uv tool list` shows for `name`: the `v...` of its `<name> v<version>` line. The
/// entry-point lines start with `-`, and `No tools installed` has no `v` word. Names compare as
/// uv normalizes them.
fn uv_tool_version(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        let n = words.next()?;
        let v = words.next()?.strip_prefix('v')?;
        (uv_norm(n) == uv_norm(name)).then(|| v.to_string())
    })
}

/// A name as uv normalizes it (PEP 503): lowercase, every run of `_`, `.` and `-` as one `-`.
/// uv names a tool's directory under `~/.local/share/uv/tools` this way too.
fn uv_norm(s: &str) -> String {
    s.chars()
        .fold(String::with_capacity(s.len()), |mut out, c| {
            match c {
                '_' | '.' | '-' if out.ends_with('-') => {}
                '_' | '.' | '-' => out.push('-'),
                c => out.push(c.to_ascii_lowercase()),
            }
            out
        })
}

/// What in uv's receipt for an installed tool (`~/.local/share/uv/tools/<name>/uv-receipt.toml`)
/// an upgrade would keep that no plan row shows: a `git`, `path`, `directory`, `url` or
/// `editable` requirement in `[tool].requirements` (an origin `uv tool upgrade` resolves again),
/// a requirement written as one PEP 508 string instead of a table (uv's legacy form, whose
/// source those keys cannot show), or any `[tool.options]` entry (`index-url`,
/// `default-index`, `find-links`, `no-index`, `prerelease` and the other install settings uv
/// persists and reuses); `Ok(Some(reason))` says which. A receipt that cannot be read or has
/// no requirements is an error: the update cannot tell what it would resolve.
fn uv_receipt_source(home: &Path, name: &str) -> Result<Option<String>, String> {
    let (v, shown) = uv_receipt(home, name)?;
    let tool = v.get("tool");
    let reqs = tool
        .and_then(|t| t.get("requirements"))
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{shown} lists no tool requirements"))?;
    for r in reqs {
        let Some(r) = r.as_table() else {
            return Ok(Some(
                "installed with a legacy string requirement in the receipt, whose source an upgrade would resolve again outside the plan".into(),
            ));
        };
        if let Some(key) = ["git", "path", "directory", "url", "editable"]
            .iter()
            .find(|k| r.contains_key(**k))
        {
            return Ok(Some(format!(
                "installed from a {key} source, which an upgrade would resolve again outside the plan"
            )));
        }
        // `uv tool upgrade` respects the version constraint the install recorded, so an
        // `==` or `<` specifier means the upgrade can never advance while the row would read
        // as rolling. The constraint is held until the plan represents it.
        if let Some(spec) = r.get("specifier").and_then(Value::as_str)
            && !spec.trim().is_empty()
        {
            return Ok(Some(
                "installed with a version constraint in the receipt, which an upgrade respects outside the plan".into(),
            ));
        }
    }
    let options = tool
        .and_then(|t| t.get("options"))
        .and_then(Value::as_table)
        .filter(|o| !o.is_empty());
    Ok(options.map(|o| {
        let keys: Vec<&str> = o.keys().map(String::as_str).collect();
        format!(
            "installed with uv options ({}), which an upgrade would keep outside the plan",
            keys.join(", ")
        )
    }))
}

/// uv's receipt for an installed tool, `~/.local/share/uv/tools/<name>/uv-receipt.toml`, read
/// as a regular file of at most 1 MiB, with its path as shown in errors.
fn uv_receipt(home: &Path, name: &str) -> Result<(dfm_model::resolve::Table, String), String> {
    let path = home
        .join(".local/share/uv/tools")
        .join(uv_norm(name))
        .join("uv-receipt.toml");
    let shown = path.display().to_string();
    let bytes = crate::read_fs::read_regular_bounded(
        &path,
        1 << 20,
        |e| Refusal::new(Code::IoFailed, format!("cannot read {shown}: {}", e.kind())),
        || Refusal::new(Code::IoFailed, format!("{shown} is not a regular file")),
    )
    .map_err(|r| r.detail)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8"))?;
    let v = text.parse().map_err(|_| format!("{shown} is not TOML"))?;
    Ok((v, shown))
}

/// Each package adapter and the pointer of the source it fetches from. That source needs no grant
/// only when the bound checkout declared it ([`owner_source`], docs/sync.md, "Source
/// identities"); otherwise its identity needs one. `None`: the adapter installs from its
/// provider's registry, or its source always needs a grant (`script/v1`). A new package adapter
/// must be listed here (`every_package_adapter_states_its_source_rule`).
pub(crate) const SOURCE_ADAPTERS: &[(&str, Option<&str>)] = &[
    (HOMEBREW, None),
    (MISE, Some("/name")),
    (APT, None),
    (UV_TOOL, Some("/source")),
    (checkout::ADAPTER, Some("/url")),
    (script::ADAPTER, None),
];

/// The [`SOURCE_ADAPTERS`] pointer of `adapter`, or `None`.
fn source_pointer(adapter: &str) -> Option<&'static str> {
    SOURCE_ADAPTERS
        .iter()
        .find(|(a, _)| *a == adapter)
        .and_then(|(_, p)| *p)
}

/// `Ungranted` for `identity`, with the [`SOURCE_ADAPTERS`] pointer of `r` as the plan's field.
pub(crate) fn ungranted_state(r: &ResolvedResource, identity: String) -> PackageState {
    PackageState::Ungranted {
        identity,
        field: source_pointer(&r.adapter).unwrap_or("/url"),
    }
}

/// Whether the bound checkout declared the source of `r` (its [`SOURCE_ADAPTERS`] pointer).
/// `nested` holds the nested source paths of the lock (`sync_cmd::nested_paths`); `None`, an
/// unreadable lock, is never the owner's.
pub(crate) fn owner_source(nested: Option<&[String]>, r: &ResolvedResource) -> bool {
    match (source_pointer(&r.adapter), nested) {
        (Some(p), Some(n)) => dfm_model::resolve::owner_declared(r, p, n),
        _ => false,
    }
}

/// The backend tool of a backend-qualified `mise/v1` name and the source identity it fetches
/// from, or `None` for a registry name (no `<backend>:`). `github:o/r` and `ubi:o/r` are the
/// repository (`https://github.com/o/r`), `npm:<name>` is `npm:<name>`. A version after the
/// first `@` past the start must be `latest` or a plain version (`[0-9A-Za-z][0-9A-Za-z.+-]*`):
/// anything else (an npm alias `npm:y`, a git or URL spec) would make the tool fetch another
/// source than the identity names. Any other backend, or another version, is
/// `E_SOURCE_UNTRUSTED`.
fn mise_identity(name: &str) -> Result<Option<(String, String)>, Refusal> {
    let Some((backend, rest)) = name.split_once(':') else {
        return Ok(None);
    };
    let untrusted = |why: &str| {
        Refusal::new(
            Code::SourceUntrusted,
            format!("mise/v1 tool {}: {why}", dfm_model::display::visible(name)),
        )
    };
    // `npm:@scope/x` starts with `@`; the first `@` after the start begins the version.
    let (rest, version) = match rest.char_indices().skip(1).find(|(_, c)| *c == '@') {
        Some((i, _)) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let plain = |v: &str| {
        v == "latest"
            || v.starts_with(|c: char| c.is_ascii_alphanumeric())
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
    };
    if let Some(v) = version.filter(|v| !plain(v)) {
        return Err(untrusted(&format!(
            "the version {} from a nested source must be latest or a plain version such as 1.2.3",
            dfm_model::display::visible(v)
        )));
    }
    let source = match backend {
        "github" | "ubi" => rest.to_string(),
        "npm" => format!("{}{rest}", dfm_model::lock::NPM),
        _ => {
            return Err(untrusted(
                "a nested source may declare only registry names or github:, ubi: or npm: mise tools",
            ));
        }
    };
    dfm_model::lock::source_identity(&source, cfg!(feature = "test-isolation"))
        .map(|id| Some((rest.to_string(), id)))
        .map_err(|e| untrusted(&e.detail))
}

/// The state of a `mise/v1` tool that the bound checkout did not declare: a backend-qualified
/// name that would be installed or rolled needs a grant for its source (`Ungranted`), and a
/// backend without one identity is `Unofficial`. A registry name, the bound checkout's own tool,
/// and any state without `cx` are kept as they are.
pub(crate) fn mise_gated(
    cx: Option<&ScriptCx>,
    r: &ResolvedResource,
    state: PackageState,
) -> Result<PackageState, Refusal> {
    let gated = matches!(state, PackageState::Missing(_) | PackageState::Rolling(_));
    let Some(cx) = cx.filter(|_| r.adapter == MISE && gated) else {
        return Ok(state);
    };
    if owner_source(cx.nested(), r) {
        return Ok(state);
    }
    Ok(match mise_identity(&name(&r.value)?) {
        Ok(None) => state,
        Ok(Some((_, id))) => ungranted(cx, r, id)?.map_or(state, |id| ungranted_state(r, id)),
        Err(e) => PackageState::Unofficial(e.detail),
    })
}

/// The re-check under the writer lock before `mise install` or an upgrade of a backend-qualified
/// name that the bound checkout did not declare ([`source_gate`]). Without `cx` such a name is
/// not installed. The result is the extra provider variables: an `npm:` tool from a nested
/// source installs from the default registry, with the pins the `npm` updater sets.
pub(crate) fn mise_gate(
    cx: Option<&ScriptCx>,
    r: &ResolvedResource,
) -> Result<Vec<(String, String)>, Refusal> {
    if r.adapter != MISE {
        return Ok(Vec::new());
    }
    let n = name(&r.value)?;
    if !n.contains(':') {
        return Ok(Vec::new());
    }
    let cx = cx.ok_or_else(|| {
        Refusal::new(
            Code::GeneratorUnavailable,
            format!("no source context for the mise/v1 tool {n}"),
        )
    })?;
    if owner_source(cx.nested(), r) {
        return Ok(Vec::new());
    }
    let Some((tool, id)) = mise_identity(&n)? else {
        return Ok(Vec::new());
    };
    source_gate(cx, r, id)?;
    Ok(if n.starts_with(dfm_model::lock::NPM) {
        super::updater::npm_env(&[tool])
    } else {
        Vec::new()
    })
}

/// `identity` when the source of `r` still needs a grant: the bound checkout did not declare it
/// and `identity` has none.
pub(crate) fn ungranted(
    cx: &ScriptCx,
    r: &ResolvedResource,
    identity: String,
) -> Result<Option<String>, Refusal> {
    Ok((!owner_source(cx.nested(), r) && !cx.trusts(&identity)?).then_some(identity))
}

/// The re-check under the writer lock before a source is fetched: `E_SOURCE_UNTRUSTED` with the
/// `--trust-origin` command when [`ungranted`] holds.
pub(crate) fn source_gate(
    cx: &ScriptCx,
    r: &ResolvedResource,
    identity: String,
) -> Result<(), Refusal> {
    match ungranted(cx, r, identity)? {
        None => Ok(()),
        Some(id) => Err(Refusal::new(
            Code::SourceUntrusted,
            format!(
                "the source {id} of {} has no grant; nothing was run. Fix: {}",
                r.id,
                dfm_model::lock::trust_command(&id)
            ),
        )),
    }
}

/// A `uv-tool/v1` source other than the package index (docs/packages.md, "uv tools from a
/// source").
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UvSource {
    /// `git+<url>@<rev>`: an https repository at a tag or a full commit; `git+<url>`, the
    /// rolling form: its default branch.
    Git { url: String, rev: Option<String> },
    /// `path:<rel>`: a directory in the bound checkout, canonical.
    Path(PathBuf),
}

const SOURCE_FORMS: &str = "git+https://<repository>, git+https://<repository>@<tag or 40-hex commit> or path:<directory in the checkout>";

fn bad_source(s: &str, why: &str) -> Refusal {
    Refusal::new(
        Code::ManifestInvalid,
        format!(
            "uv-tool/v1 source {} {why}; the accepted forms are {SOURCE_FORMS}",
            dfm_model::display::visible(s)
        ),
    )
}

/// The declared `source`, or `None` for a package-index tool.
fn source_text(value: &Value) -> Option<&str> {
    value.get("source").and_then(Value::as_str)
}

/// The checked source of a `uv-tool/v1` value: `None` without `source`, `E_MANIFEST_INVALID`
/// for a bad one. A path source must resolve, through links, to a directory inside `root`.
pub(crate) fn uv_source(value: &Value, root: &Path) -> Result<Option<UvSource>, Refusal> {
    let Some(s) = source_text(value) else {
        return Ok(None);
    };
    let Some(rel) = s.strip_prefix("path:") else {
        return git_source(s).map(Some);
    };
    let outside = || bad_source(s, "is not a directory inside the checkout");
    if rel.is_empty() {
        return Err(outside());
    }
    let root = root.canonicalize().map_err(|_| outside())?;
    let dir = root.join(rel).canonicalize().map_err(|_| outside())?;
    if dir.starts_with(&root) && dir.is_dir() {
        Ok(Some(UvSource::Path(dir)))
    } else {
        Err(outside())
    }
}

/// [`uv_source`] of `r`, which must have one. A path source must come from the bound checkout:
/// one that a nested source declares or changes is `E_MANIFEST_INVALID`.
fn checked_source(r: &ResolvedResource, cx: &ScriptCx) -> Result<UvSource, Refusal> {
    let text = source_text(&r.value).unwrap_or_default();
    let source = uv_source(&r.value, cx.root())?.ok_or_else(|| bad_source(text, "is missing"))?;
    if matches!(source, UvSource::Path(_)) && !owner_source(cx.nested(), r) {
        return Err(bad_source(
            text,
            "is not declared by the bound checkout; a path source must be declared in the bound checkout",
        ));
    }
    Ok(source)
}

/// A git source's state, or `Ungranted` when its repository needs a grant it lacks.
fn gated(
    cx: &ScriptCx,
    r: &ResolvedResource,
    source: &UvSource,
    state: PackageState,
) -> Result<PackageState, Refusal> {
    if let (PackageState::Missing(_) | PackageState::Rolling(_), UvSource::Git { url, .. }) =
        (&state, source)
        && let Some(id) = ungranted(cx, r, git_identity(url)?)?
    {
        return Ok(ungranted_state(r, id));
    }
    Ok(state)
}

/// `git+https://<repository>`, or `git+https://<repository>@<rev>` split at the last `@` (an
/// `@` followed by a `/` belongs to the url). The repository must map to a source identity; a
/// rev must be a tag shaped like a version or a full commit.
/// shortcut: a version-shaped ref is taken to be a tag without asking the remote; a branch named
/// like a version passes, upgrade by checking refs/tags with git ls-remote.
fn git_source(s: &str) -> Result<UvSource, Refusal> {
    let rest = s
        .strip_prefix("git+")
        .filter(|r| r.starts_with("https://"))
        .ok_or_else(|| bad_source(s, "is not a git+https source"))?;
    let (url, rev) = match rest.rsplit_once('@') {
        Some((url, rev)) if !rev.contains('/') => (url, Some(rev)),
        _ => (rest, None),
    };
    git_identity(url).map_err(|e| bad_source(s, &e.detail))?;
    let Some(rev) = rev else {
        return Ok(UvSource::Git {
            url: url.into(),
            rev: None,
        });
    };
    let hex = rev.len() == 40 && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let parts: Vec<&str> = rev.strip_prefix('v').unwrap_or(rev).split('.').collect();
    let version = parts.len() > 1
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !hex && !version {
        return Err(bad_source(
            s,
            "does not end in a tag like 1.2.3 or v1.2.3 or a 40-hex commit; a branch moves outside the plan",
        ));
    }
    Ok(UvSource::Git {
        url: url.into(),
        rev: Some(rev.into()),
    })
}

/// The source identity of a git source's repository.
fn git_identity(url: &str) -> Result<String, Refusal> {
    dfm_model::lock::source_identity(url, cfg!(feature = "test-isolation"))
}

/// Whether a resource is a `uv-tool/v1` tool with a `source`, which [`uv_source_probe`] and
/// [`uv_source_install`] handle instead of [`probe`] and [`install`].
pub(crate) fn uv_sourced(adapter: &str, value: &Value) -> bool {
    adapter == UV_TOOL && source_text(value).is_some()
}

/// The installed state of a `uv-tool/v1` tool with a `source`. A source the bound checkout
/// declared needs no grant; a git source from a nested source that is to be installed needs a
/// grant for its repository, else it is `Ungranted` ([`owner_source`]). Without `cx` a
/// source that can be judged without the checkout is checked and the tool is `Missing`. uv's
/// receipt decides: a requirement from the declared source ([`uv_receipt_names`]) is
/// `Installed`, anything else (no receipt, another source, an unreadable receipt, which
/// `--force` replaces) is `Missing`, with the owner check.
pub(crate) fn uv_source_probe(
    runner: &Runner,
    r: &ResolvedResource,
    cx: Option<&ScriptCx>,
) -> Result<PackageState, Refusal> {
    let value = &r.value;
    let pkg = uv_source_name(value)?;
    policy(value)?;
    let text = source_text(value).unwrap_or_default();
    let Some(cx) = cx else {
        if !text.starts_with("path:") {
            git_source(text)?;
        }
        return Ok(PackageState::Missing(Vec::new()));
    };
    let source = checked_source(r, cx)?;
    let state = if uv_receipt_names(&runner.home, &pkg, &source) {
        PackageState::Installed(match &source {
            UvSource::Git { rev: Some(rev), .. } => rev.clone(),
            _ => text.to_string(),
        })
    } else {
        PackageState::Missing(Vec::new())
    };
    gated(cx, r, &source, settled(runner, UV_TOOL, &pkg, state))
}

/// Whether uv's receipt for `pkg` has the requirement of `pkg` itself (its `name` equal as uv
/// normalizes it) from `source`: a `git` of `<url>?rev=<rev>` for a source with a rev, a `git`
/// of the same repository at any rev for a ref-less one, or a `directory` of the canonical path.
/// A `--with` extra from the source does not count. An unreadable receipt does not.
fn uv_receipt_names(home: &Path, pkg: &str, source: &UvSource) -> bool {
    let (key, want) = match source {
        UvSource::Git {
            url,
            rev: Some(rev),
        } => ("git", format!("{url}?rev={rev}")),
        UvSource::Git { url, rev: None } => ("git", url.clone()),
        UvSource::Path(dir) => ("directory", dir.display().to_string()),
    };
    let rolling = matches!(source, UvSource::Git { rev: None, .. });
    let same = |got: &str| {
        if rolling {
            got.split(['?', '#']).next() == Some(want.as_str())
        } else {
            got == want
        }
    };
    let pkg = uv_norm(pkg);
    uv_receipt(home, &pkg).is_ok_and(|(v, _)| {
        v.get("tool")
            .and_then(|t| t.get("requirements"))
            .and_then(Value::as_array)
            .is_some_and(|reqs| {
                reqs.iter().any(|r| {
                    r.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| uv_norm(n) == pkg)
                        && r.get(key).and_then(Value::as_str).is_some_and(same)
                })
            })
    })
}

/// Installs a `uv-tool/v1` tool from its `source` with `uv tool install --force`, which replaces
/// a copy from another source. A git source from a nested source is checked for its grant
/// again first ([`source_gate`]). The caller probes again afterwards.
pub(crate) fn uv_source_install(
    runner: &Runner,
    r: &ResolvedResource,
    cx: &ScriptCx,
) -> Result<(), Refusal> {
    let pkg = uv_source_name(&r.value)?;
    let text = source_text(&r.value).unwrap_or_default();
    let source = checked_source(r, cx)?;
    uv_source_gate(r, cx, &source)?;
    let arg = match &source {
        UvSource::Git {
            url,
            rev: Some(rev),
        } => format!("git+{url}@{rev}"),
        UvSource::Git { url, rev: None } => format!("git+{url}"),
        UvSource::Path(dir) => dir
            .to_str()
            .ok_or_else(|| bad_source(text, "resolves to a path that is not UTF-8"))?
            .to_string(),
    };
    let args = ["tool".to_string(), "install".into(), "--force".into(), arg];
    let what = format!("uv tool install --force {}", args[3]);
    change(runner, "uv", &args, &uv_env(false), &what)?;
    // uv names the tool's directory after the package at the source, not after the declaration.
    uv_receipt(&runner.home, &pkg).map(|_| ()).map_err(|why| {
        Refusal::new(
            Code::IoFailed,
            format!(
                "{what} succeeded, but no receipt names {pkg} ({why}); the package at the source may have another name than the declared one"
            ),
        )
    })
}

/// [`source_gate`] for a git source; a path source is checked by [`checked_source`].
fn uv_source_gate(r: &ResolvedResource, cx: &ScriptCx, source: &UvSource) -> Result<(), Refusal> {
    match source {
        UvSource::Git { url, .. } => source_gate(cx, r, git_identity(url)?),
        UvSource::Path(_) => Ok(()),
    }
}

/// The re-check under the writer lock before `uv tool upgrade` of a source tool resolves its
/// source again.
pub(crate) fn uv_source_upgrade_gate(r: &ResolvedResource, cx: &ScriptCx) -> Result<(), Refusal> {
    uv_source_gate(r, cx, &checked_source(r, cx)?)
}

/// The declared name of a source tool: a package name that is also a package-index name, so
/// the receipt path takes no `/`.
fn uv_source_name(value: &Value) -> Result<String, Refusal> {
    let pkg = name(value)?;
    if !uv_name(&pkg) {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            format!(
                "{pkg}: uv-tool/v1 takes a package-index name (letters, digits, '.', '_', '-'); a source goes in source"
            ),
        ));
    }
    Ok(pkg)
}

/// `packages --update` for a source tool. The name and the source are checked first, as the
/// plan without `--update` checks them. Then [`maintain`] decides, and it holds a tool that
/// uv's receipt shows from a pinned source (a tag, a commit or a path). A rolling tool from a
/// ref-less git source that the receipt shows is `Rolling` instead, so `uv tool upgrade` moves
/// it to the default branch's newest commit, with the owner check. A tool installed from
/// anywhere else (the package index, another rev or repository) is `Missing`, with the owner
/// check (`OtherOwner` blocks), so it is installed from its source and never upgraded from the
/// index. A git source from a nested source that would be installed or rolled needs a grant,
/// else it is `Ungranted`. Without `cx` (`doctor`) a path source is not resolved.
pub(crate) fn maintain_sourced(
    runner: &Runner,
    r: &ResolvedResource,
    cx: Option<&ScriptCx>,
) -> Result<PackageState, Refusal> {
    let value = &r.value;
    let pkg = uv_source_name(value)?;
    let text = source_text(value).unwrap_or_default();
    let Some(cx) = cx else {
        if !text.starts_with("path:") {
            git_source(text)?;
        }
        return maintain(runner, UV_TOOL, value);
    };
    let source = checked_source(r, cx)?;
    let state = match maintain(runner, UV_TOOL, value)? {
        // `maintain` holds some copies before its owner check, so a copy it now treats as
        // missing gets that check here, as the plain probe does.
        PackageState::Installed(_) | PackageState::Held(_) | PackageState::Rolling(_)
            if !uv_receipt_names(&runner.home, &pkg, &source) =>
        {
            settled(runner, UV_TOOL, &pkg, PackageState::Missing(Vec::new()))
        }
        // `maintain` holds every git receipt; a ref-less source is the rolling form.
        PackageState::Held(_)
            if matches!(source, UvSource::Git { rev: None, .. }) && policy(value)? == "rolling" =>
        {
            match (
                probe(runner, UV_TOOL, value)?,
                other_owner(runner, UV_TOOL, &pkg),
            ) {
                (_, Ok(Some(why))) => PackageState::OtherOwner(why),
                (_, Err(why)) => PackageState::Unknown(why),
                (PackageState::Installed(v), Ok(None)) => PackageState::Rolling(v),
                (state, Ok(None)) => state,
            }
        }
        state => state,
    };
    gated(cx, r, &source, state)
}

/// `dpkg-query -W -f=${Status} <name>`: `install ok installed` means installed. A missing
/// package is exit 1. Installing needs root, which a HOME adapter never has, so a missing apt
/// package is `Unsupported`.
fn apt_probe(runner: &Runner, pkg: &str) -> PackageState {
    let args = ["-W".to_string(), "-f=${Status}".into(), pkg.into()];
    let out = match runner.run(&request("dpkg-query", &args, &[], PROBE_TIMEOUT)) {
        Ok(o) => o,
        Err(e) => return PackageState::Unknown(e.detail),
    };
    if let Some(why) = ended("dpkg-query", &out) {
        return PackageState::Unknown(why);
    }
    match out.end {
        End::Exited(0) if String::from_utf8_lossy(&out.stdout).contains("install ok installed") => {
            PackageState::Installed("installed".into())
        }
        End::Exited(0 | 1) => PackageState::Unsupported(format!(
            "apt needs root; run `sudo apt-get install {pkg}` by hand"
        )),
        End::Exited(c) => PackageState::Unknown(format!("dpkg-query exited {c}")),
        _ => PackageState::Unknown("dpkg-query gave no exit code".into()),
    }
}

/// A declared name split into the tool and the wanted version: the part after the last `@`,
/// when that part is a version and not a path. A scoped npm tool (`npm:@scope/pkg`) keeps its
/// `@` and has no version; `npm:@scope/pkg@1.2` has one.
pub(crate) fn tool_and_version(pkg: &str) -> (&str, &str) {
    match pkg.rsplit_once('@') {
        Some((tool, version)) if !tool.is_empty() && !version.contains('/') => (tool, version),
        _ => (pkg, ""),
    }
}

/// The installed versions in a `mise ls --installed --json <tool>` answer, or `None` when it is
/// not an array of objects with a string `version`.
fn installed_versions(stdout: &[u8]) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    v.as_array()?
        .iter()
        .filter(|e| e.get("installed").and_then(serde_json::Value::as_bool) != Some(false))
        .map(|e| e.get("version")?.as_str().map(str::to_string))
        .collect()
}

/// Whether an installed version satisfies the declared one: equal, or longer by whole
/// components (`20.20.2` satisfies `20` and `20.20`; `2.1.0` does not satisfy `20`).
fn version_matches(have: &str, want: &str) -> bool {
    want.is_empty()
        || have
            .strip_prefix(want)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '-', '+']))
}

/// A mise selector that names no fixed version: mise resolves it from its remote listing, which
/// a read-only probe never fetches.
fn symbolic(want: &str) -> bool {
    matches!(want, "latest" | "lts" | "system")
        || ["lts-", "lts/", "sub-", "ref:", "path:", "prefix:"]
            .iter()
            .any(|p| want.starts_with(p))
}

/// The executable name a package is looked for under: the last `/` component of a tap or
/// backend-qualified name, without a `@version` suffix (`homebrew/core/atuin`, `node@20` and
/// `npm:@scope/pkg` give `atuin`, `node` and `pkg`).
pub(crate) fn leaf(pkg: &str) -> &str {
    let (tool, _) = tool_and_version(pkg);
    tool.rsplit('/').next().unwrap_or(tool)
}

/// Directories where another owner can have put an executable of the package's name: the other
/// search-path prefixes, Nix profiles, `~/.local/bin`, `~/.cargo/bin` and the mise shims, less
/// the ones [`skipped`] names. A directory that cannot be examined is an error, so the check
/// fails closed.
/// shortcut: the check compares the package name with an executable name, so a package whose
/// binary is named differently is not caught, upgrade when adapters declare their binaries.
fn other_owner(runner: &Runner, adapter: &str, pkg: &str) -> Result<Option<String>, String> {
    let home = &runner.home;
    let shims = home.join(".local/share/mise/shims");
    let local_bin = home.join(".local/bin");
    let mut dirs: Vec<PathBuf> = runner.search_path.clone();
    dirs.extend([
        home.join(".nix-profile/bin"),
        PathBuf::from("/nix/var/nix/profiles/default/bin"),
        local_bin.clone(),
        home.join(".cargo/bin"),
        shims.clone(),
    ]);
    let uv_tools = home.join(".local/share/uv/tools");
    let name = leaf(pkg);
    for d in dirs.iter().filter(|d| !skipped(adapter, d, &shims)) {
        let p = d.join(name);
        // uv links a tool's entry point into `~/.local/bin`. Only a link into uv's own tool
        // directory is the declared provider's there; a file, or a link elsewhere, replaced it.
        if adapter == UV_TOOL
            && *d == local_bin
            && std::fs::read_link(&p).is_ok_and(|t| d.join(t).starts_with(&uv_tools))
        {
            continue;
        }
        // The entry itself (a link counts), then what a link points to.
        for meta in [std::fs::symlink_metadata(&p), std::fs::metadata(&p)] {
            match meta {
                Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => {
                    return Ok(Some(format!("an executable {} exists", p.display())));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    return Err(format!("cannot examine {}: {}", p.display(), e.kind()));
                }
            }
        }
    }
    Ok(None)
}

/// Whether the owner check skips `d`: the macOS system directories (Apple's tools there are part
/// of the OS, which no package owns and none can replace; a provider's copy shadows them by
/// search order) and the declared provider's own directories (`homebrew` path component for
/// brew, the mise shims for mise), matched by component or exact path, never by substring. uv
/// has no directory of its own on the list: its links in `~/.local/bin` are told apart by
/// target in [`other_owner`].
fn skipped(adapter: &str, d: &Path, shims: &Path) -> bool {
    dfm_platform_macos::SYSTEM_PATH
        .iter()
        .any(|s| d == Path::new(s))
        || match adapter {
            HOMEBREW => d.components().any(|c| c.as_os_str() == "homebrew"),
            MISE => d == shims,
            _ => false,
        }
}

/// The installed state of one package by its adapter, with the owner check when the provider
/// does not list it.
pub fn probe(runner: &Runner, adapter: &str, value: &Value) -> Result<PackageState, Refusal> {
    let pkg = name(value)?;
    policy(value)?;
    let state = match adapter {
        HOMEBREW => brew_probe(runner, &pkg),
        MISE => mise_probe(runner, &pkg),
        APT => apt_probe(runner, &pkg),
        UV_TOOL => uv_probe(runner, &pkg),
        other => {
            return Err(Refusal::new(
                Code::GeneratorUnavailable,
                format!("no package provider for {other}"),
            ));
        }
    };
    Ok(settled(runner, adapter, &pkg, state))
}

/// The owner check for a package its provider does not list; any other state is the probe's.
fn settled(runner: &Runner, adapter: &str, pkg: &str, state: PackageState) -> PackageState {
    match state {
        PackageState::Missing(_) => match other_owner(runner, adapter, pkg) {
            Ok(Some(why)) => PackageState::OtherOwner(why),
            Ok(None) if adapter == HOMEBREW => match brew_official(runner, pkg) {
                Ok(Some(off)) => brew_dependencies(runner, &off),
                Ok(None) => PackageState::Unofficial(format!(
                    "{pkg} is not an official Homebrew formula or cask; reading its dependencies would load tap code before approval"
                )),
                Err(why) => PackageState::Unknown(why),
            },
            Ok(None) => PackageState::Missing(Vec::new()),
            Err(why) => PackageState::Unknown(why),
        },
        s => s,
    }
}

/// Installs one package through its provider. The caller probes again afterwards. A mise tool
/// is always sent with a selector, `<tool>@latest` for a bare name: `mise install <tool>`
/// installs only the version mise's configuration requests, and `MISE_NO_CONFIG=1` hides that
/// configuration.
pub fn install(runner: &Runner, adapter: &str, value: &Value) -> Result<(), Refusal> {
    install_env(runner, adapter, value, &[])
}

/// [`install`] with `extra` variables for the provider (the npm registry pins of
/// [`mise_gate`]).
pub(crate) fn install_env(
    runner: &Runner,
    adapter: &str,
    value: &Value,
    extra: &[(String, String)],
) -> Result<(), Refusal> {
    let pkg = name(value)?;
    let pkg = if adapter == MISE {
        mise_selector(&pkg)
    } else {
        pkg
    };
    let (program, args, env) = match adapter {
        HOMEBREW => {
            // The same namespace pin as the probe: the install cannot resolve the short name
            // to another package than the one the plan named.
            let off = brew_official(runner, &pkg)
                .map_err(|why| Refusal::new(Code::ProbeFailed, why))?
                .ok_or_else(|| {
                    Refusal::new(
                        Code::SourceUntrusted,
                        format!("{pkg} is not an official Homebrew formula or cask"),
                    )
                })?;
            let mut args = vec!["install".to_string()];
            args.extend(off.args());
            ("brew", args, brew_env())
        }
        MISE => (
            "mise",
            vec!["install".to_string(), pkg.clone()],
            mise_env()
                .into_iter()
                .chain(extra.iter().cloned())
                .collect(),
        ),
        UV_TOOL => (
            "uv",
            vec!["tool".to_string(), "install".into(), pkg.clone()],
            uv_env(false),
        ),
        _ => {
            return Err(Refusal::new(
                Code::PrivilegeUnsupported,
                format!("{adapter} cannot install {pkg}: a HOME adapter never escalates"),
            ));
        }
    };
    change(
        runner,
        program,
        &args,
        &env,
        &format!("{program} install {pkg}"),
    )
}

/// Uninstalls one package through its provider (`purge`, docs/purge.md): every installed
/// version, without `--ignore-dependencies`, `--zap` or `autoremove`. Homebrew refuses a formula
/// that another installed formula needs; that refusal is an error here and the caller decides.
/// The caller probes again afterwards.
pub fn uninstall(runner: &Runner, adapter: &str, value: &Value) -> Result<(), Refusal> {
    let pkg = name(value)?;
    let (program, args, env) = match adapter {
        HOMEBREW => {
            let off = brew_official(runner, &pkg)
                .map_err(|why| Refusal::new(Code::ProbeFailed, why))?
                .ok_or_else(|| {
                    Refusal::new(
                        Code::SourceUntrusted,
                        format!("{pkg} is not an official Homebrew formula or cask"),
                    )
                })?;
            let args = match off {
                // `--force` removes every installed keg, not only the latest one.
                Official::Formula(n) => {
                    vec!["uninstall".into(), "--formula".into(), "--force".into(), n]
                }
                Official::Cask(n) => vec!["uninstall".into(), "--cask".into(), n],
            };
            ("brew", args, brew_env())
        }
        MISE => {
            let (tool, want) = tool_and_version(&pkg);
            let args = if want.is_empty() {
                vec!["uninstall".to_string(), "--all".into(), tool.into()]
            } else {
                // A pinned trial removes only the versions its pin matches: another version of
                // the tool can predate the trial (`node@20` beside a trial of `node@22`).
                let mut args = vec!["uninstall".to_string()];
                args.extend(
                    mise_listing(runner, &pkg)
                        .map_err(|s| {
                            Refusal::new(Code::ProbeFailed, format!("{pkg}: {}", s.state()))
                        })?
                        .iter()
                        .filter(|v| version_matches(v, want))
                        .map(|v| format!("{tool}@{v}")),
                );
                if args.len() == 1 {
                    return Ok(());
                }
                args
            };
            ("mise", args, mise_env())
        }
        _ => {
            return Err(Refusal::new(
                Code::PrivilegeUnsupported,
                format!("{adapter} cannot uninstall {pkg}"),
            ));
        }
    };
    change(
        runner,
        program,
        &args,
        &env,
        &format!("{program} uninstall {pkg}"),
    )
}

/// Uninstalls Homebrew formulae by name in one call (`purge`'s dependencies). Brew checks the
/// remaining installed formulae against the whole set and refuses the call when one of them
/// still needs a name: that refusal alone is `E_CONFLICT`. Any other failure is
/// `E_IO_FAILED`. A name that starts with `-` is refused before brew runs.
pub fn uninstall_formulae(runner: &Runner, names: &[String]) -> Result<(), Refusal> {
    if let Some(n) = names.iter().find(|n| n.starts_with('-') || n.is_empty()) {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            format!("{n} is not a formula name"),
        ));
    }
    let mut args = vec![
        "uninstall".to_string(),
        "--formula".into(),
        "--force".into(),
    ];
    args.extend(names.iter().cloned());
    let what = format!("brew uninstall {}", names.join(" "));
    let out = runner.run(&request("brew", &args, &brew_env(), INSTALL_TIMEOUT))?;
    if matches!(out.end, End::Exited(c) if c != 0)
        && String::from_utf8_lossy(&out.stderr).contains("because it is required by")
    {
        return Err(Refusal::new(
            Code::Conflict,
            format!("{what}: an installed formula still needs it"),
        ));
    }
    match out.end {
        End::Exited(0) => Ok(()),
        End::Exited(c) => Err(Refusal::new(
            Code::IoFailed,
            format!(
                "{what} exited {c}; its {} bytes of standard error are withheld. Run it by hand to see them",
                out.stderr.len()
            ),
        )),
        End::TimedOut => Err(Refusal::new(
            Code::IoFailed,
            format!("{what} did not finish within {INSTALL_TIMEOUT:?}"),
        )),
        End::Signaled => Err(Refusal::new(
            Code::IoFailed,
            format!("{what} was ended by a signal"),
        )),
        End::Stopped(r) => Err(r),
    }
}

/// Whether the provider itself lists the package (`purge`): the provider's listing only, without
/// the owner check, the official-name check or the dependency listing that plan a missing
/// install. `Installed`, `Missing` or `Unknown`.
pub fn listed(runner: &Runner, adapter: &str, value: &Value) -> Result<PackageState, Refusal> {
    let pkg = name(value)?;
    Ok(match adapter {
        HOMEBREW => brew_probe(runner, &pkg),
        MISE => match mise_listing(runner, &pkg) {
            Ok(v) => {
                let (_, want) = tool_and_version(&pkg);
                let v: Vec<String> = v.into_iter().filter(|v| version_matches(v, want)).collect();
                if v.is_empty() {
                    PackageState::Missing(Vec::new())
                } else {
                    PackageState::Installed(v.join(", "))
                }
            }
            Err(s) => s,
        },
        other => PackageState::Unknown(format!("{other} has no listing purge can read")),
    })
}

/// The rolling-update view of one installed package (docs/toolchains.md): the probe, then for
/// a listed package whether the run may ask the provider for a newer version. Only a
/// `rolling` declaration of a mise or uv tool is `Rolling`; a held or other policy, a provider
/// with no rolling path, an installed local build (a mise `ref:` or `path:` version of the
/// tool, inside or outside the declared prefix, which the update would not replace and the
/// manager never removes), an exact mise pin (a declared version the listing contains as it
/// is, which `mise install` resolves to itself), or a uv tool whose receipt names a git, path,
/// url or editable source or keeps install options, is `Held` with the reason. A copy of the
/// tool from another owner blocks the update (`OtherOwner`): two owners of one tool is the
/// conflict the update must not deepen.
pub fn maintain(runner: &Runner, adapter: &str, value: &Value) -> Result<PackageState, Refusal> {
    let pkg = name(value)?;
    let policy = policy(value)?;
    // The mise probe narrows the listing to the declared prefix; a local build outside it
    // (`go@1.27` declared, `ref:feature` installed) holds the tool too, so the whole listing
    // is examined before the narrowing.
    let (state, local) = if adapter == MISE {
        match mise_listing(runner, &pkg) {
            Ok(all) => (
                settled(runner, adapter, &pkg, mise_state(&all, &pkg)),
                all.iter().find(|v| local_build(v)).cloned(),
            ),
            Err(state) => (state, None),
        }
    } else {
        (probe(runner, adapter, value)?, None)
    };
    let PackageState::Installed(versions) = state else {
        return Ok(state);
    };
    let held = |why: String| Ok(PackageState::Held(format!("{versions}, {why}")));
    if policy != "rolling" {
        return held(format!("update_policy = {policy}"));
    }
    if !matches!(adapter, MISE | UV_TOOL) {
        return held(format!(
            "{adapter} has no rolling update path in this build"
        ));
    }
    if let Some(local) = local {
        return held(format!("{local} is a local build, which is kept"));
    }
    // `mise install <tool>@<version>` with that version installed is a no-op: the tool cannot
    // advance until the pin is edited, so it is not planned as an upgrade every run.
    let (_, want) = tool_and_version(&pkg);
    if adapter == MISE && !want.is_empty() && versions.split(' ').any(|v| v == want) {
        return held("an exact pin resolves to itself".into());
    }
    // `uv tool upgrade` resolves the tool from its receipt again: a git, path or url source,
    // or a persisted option such as an index, is a setting the plan never showed, so such a
    // tool is kept.
    if adapter == UV_TOOL {
        match uv_receipt_source(&runner.home, &pkg) {
            Ok(None) => {}
            Ok(Some(why)) | Err(why) => return held(why),
        }
    }
    Ok(match other_owner(runner, adapter, &pkg) {
        Ok(Some(why)) => PackageState::OtherOwner(why),
        Ok(None) => PackageState::Rolling(versions),
        Err(why) => PackageState::Unknown(why),
    })
}

/// A mise version that was built from a git ref or taken from a local path, not resolved from
/// the registry: a rolling update does not replace it.
/// shortcut: the `ref-` and `path-` spellings are a guess at how mise lists such versions,
/// upgrade when a live listing of one is available.
fn local_build(v: &str) -> bool {
    ["ref:", "ref-", "path:", "path-"]
        .iter()
        .any(|p| v.starts_with(p))
}

/// The `TOOL@VERSION` argument mise gets for a declared name: the declared `tool@version` as
/// it is, `tool@latest` for a bare name.
fn mise_selector(pkg: &str) -> String {
    let (tool, want) = tool_and_version(pkg);
    let want = if want.is_empty() { "latest" } else { want };
    format!("{tool}@{want}")
}

/// Asks the provider for its newest version of a rolling tool and installs it beside the
/// installed ones: for mise the install itself (`mise install <tool>@latest` or
/// `<tool>@<prefix>` resolves the newest matching version and is a no-op when it is
/// installed); `uv tool upgrade <name>` replaces the tool's one version. The caller probes
/// again and compares the listing to decide `updated` or `current`. Neither removes a version.
pub fn upgrade(runner: &Runner, adapter: &str, value: &Value) -> Result<(), Refusal> {
    let pkg = name(value)?;
    match adapter {
        MISE => install(runner, adapter, value),
        UV_TOOL => {
            let args = ["tool".to_string(), "upgrade".into(), pkg.clone()];
            change(
                runner,
                "uv",
                &args,
                &uv_env(false),
                &format!("uv tool upgrade {pkg}"),
            )
        }
        _ => Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("{adapter} has no rolling update path for {pkg}"),
        )),
    }
}

/// Runs a provider command that changes the machine, within the install timeout.
pub(super) fn change(
    runner: &Runner,
    program: &str,
    args: &[String],
    env: &[(String, String)],
    what: &str,
) -> Result<(), Refusal> {
    let out = runner.run(&request(program, args, env, INSTALL_TIMEOUT))?;
    match out.end {
        End::Exited(0) => Ok(()),
        End::Exited(c) => Err(Refusal::new(
            Code::IoFailed,
            format!(
                "{what} exited {c}; its {} bytes of standard error are withheld. Run it by hand to see them",
                out.stderr.len()
            ),
        )),
        End::TimedOut => Err(Refusal::new(
            Code::IoFailed,
            format!("{what} did not finish within {INSTALL_TIMEOUT:?}"),
        )),
        End::Signaled => Err(Refusal::new(
            Code::IoFailed,
            format!("{what} was ended by a signal"),
        )),
        End::Stopped(r) => Err(r),
    }
}

/// Probes every selected package resource for a packages plan, by resource id. A stop request
/// (SIGINT or SIGTERM to the manager) ends the probing: the probe it arrived in is ended by
/// the runner, no further provider is started, and the request is the error.
/// A `script/v1` resource is probed by its adapter with `scripts` (`None` fetches nothing, for
/// `doctor`).
pub fn package_facts(
    resolved: &Resolved,
    runner: &Runner,
    scripts: Option<&ScriptCx>,
) -> Result<BTreeMap<String, PackageState>, Refusal> {
    facts(resolved, runner, scripts, false)
}

/// The same for a rolling update (`packages --update`, docs/toolchains.md): each installed
/// rolling mise or uv tool is `Rolling`, the rest as [`maintain`] decides. The probes are the
/// same read-only listings.
pub fn update_facts(
    resolved: &Resolved,
    runner: &Runner,
    scripts: Option<&ScriptCx>,
) -> Result<BTreeMap<String, PackageState>, Refusal> {
    facts(resolved, runner, scripts, true)
}

fn facts(
    resolved: &Resolved,
    runner: &Runner,
    scripts: Option<&ScriptCx>,
    update: bool,
) -> Result<BTreeMap<String, PackageState>, Refusal> {
    let look = if update { maintain } else { probe };
    let mut out = BTreeMap::new();
    for r in resolved
        .resources
        .values()
        .filter(|r| Scope::Packages.covers(r))
    {
        if let Some(stop) = dfm_engine::signal::requested() {
            return Err(stop);
        }
        let looked = if r.adapter == checkout::ADAPTER {
            checkout::probe(runner, r, scripts)
        } else if r.adapter == script::ADAPTER {
            script::probe(runner, &r.id, &r.value, scripts, update)
        } else if uv_sourced(&r.adapter, &r.value) {
            if update {
                maintain_sourced(runner, r, scripts)
            } else {
                uv_source_probe(runner, r, scripts)
            }
        } else {
            look(runner, &r.adapter, &r.value).and_then(|s| mise_gated(scripts, r, s))
        };
        let state = match looked {
            Ok(s) => s,
            Err(e) if e.code == Code::ManifestInvalid => PackageState::Refused(e.detail),
            Err(e) => PackageState::Unknown(e.detail),
        };
        out.insert(r.id.clone(), state);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mise_backend_name_maps_to_the_identity_it_fetches_from() {
        for (n, want) in [
            ("duckdb", None),
            ("duckdb@1.2", None),
            ("github:o/r", Some("https://github.com/o/r")),
            ("ubi:o/r@v1.2.3", Some("https://github.com/o/r")),
            ("npm:prettier@3", Some("npm:prettier")),
            ("npm:@scope/x@1.2", Some("npm:@scope/x")),
            ("npm:@scope/x", Some("npm:@scope/x")),
            ("npm:x@latest", Some("npm:x")),
        ] {
            let got = mise_identity(n).unwrap().map(|(_, id)| id);
            assert_eq!(got.as_deref(), want, "{n}");
        }
        for n in [
            "aqua:o/r",
            "cargo:x",
            "core:node",
            "github:o",
            "npm:x@git+https://e/x",
            "npm:x@npm:y",
            "github:o/r@https://e",
            "npm:x@1@2",
        ] {
            let e = mise_identity(n).unwrap_err();
            assert_eq!(e.code, Code::SourceUntrusted, "{n}: {e}");
        }
    }

    /// A new package adapter must state its source rule, so it cannot skip the grant of a
    /// source that a nested source declares (docs/sync.md, "Source identities").
    #[test]
    fn every_package_adapter_states_its_source_rule() {
        let table = super::super::registry();
        let packages: Vec<&str> = table
            .iter()
            .filter(|a| a.spec.kind == "package")
            .map(|a| a.spec.id.as_str())
            .collect();
        for id in &packages {
            assert!(
                SOURCE_ADAPTERS.iter().any(|(a, _)| a == id),
                "{id} is not in SOURCE_ADAPTERS"
            );
        }
        assert_eq!(SOURCE_ADAPTERS.len(), packages.len());
    }

    #[test]
    fn a_brewfile_keeps_a_third_party_tap_and_shortens_an_official_one() {
        let (name, all) = brewfile_names("tuicr");
        assert_eq!(name, "tuicr");
        assert_eq!(all, ["tuicr", "homebrew/core/tuicr", "homebrew/cask/tuicr"]);
        assert_eq!(brewfile_names("homebrew/core/tuicr").0, "tuicr");
        assert_eq!(brewfile_names("Homebrew/Cask/x").0, "x");
        assert_eq!(brewfile_names("node@20").0, "node@20");
        let (name, all) = brewfile_names("owner/tap/x");
        assert_eq!(
            (name.as_str(), all),
            ("owner/tap/x", vec!["owner/tap/x".to_string()])
        );
    }

    #[test]
    fn the_executable_name_drops_tap_and_version() {
        assert_eq!(leaf("atuin"), "atuin");
        assert_eq!(leaf("homebrew/core/atuin"), "atuin");
        assert_eq!(leaf("node@20"), "node");
        assert_eq!(leaf("npm:left-pad"), "npm:left-pad");
        assert_eq!(leaf("npm:@scope/pkg"), "pkg");
        assert_eq!(leaf("npm:@scope/pkg@1.2"), "pkg");
    }

    #[test]
    fn a_version_is_only_what_follows_the_last_at_sign_outside_a_path() {
        assert_eq!(tool_and_version("node"), ("node", ""));
        assert_eq!(tool_and_version("node@20"), ("node", "20"));
        assert_eq!(tool_and_version("npm:@scope/pkg"), ("npm:@scope/pkg", ""));
        assert_eq!(
            tool_and_version("npm:@scope/pkg@1.2"),
            ("npm:@scope/pkg", "1.2")
        );
        assert_eq!(tool_and_version("@odd"), ("@odd", ""));
    }

    #[test]
    fn brews_own_pour_answer_is_read_one_line_per_formula() {
        let asked: Vec<String> = ["homebrew/core/imagemagick", "homebrew/core/afuse"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            unpoured("imagemagick 1\nafuse 0\n", &asked),
            Some(vec!["afuse".to_string()])
        );
        assert_eq!(unpoured("imagemagick 1\nafuse 1\n", &asked), Some(vec![]));
        for bad in [
            "",
            "imagemagick 1\n",
            "afuse 0\nimagemagick 1\n",
            "imagemagick yes\nafuse 0\n",
            "imagemagick 1\nafuse 0\nextra 1\n",
        ] {
            assert_eq!(unpoured(bad, &asked), None, "{bad:?}");
        }
    }

    #[test]
    fn system_directories_and_the_providers_own_are_not_another_owner() {
        let shims = PathBuf::from("/Users/a/.local/share/mise/shims");
        let local_bin = PathBuf::from("/Users/a/.local/bin");
        for d in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            assert!(skipped(HOMEBREW, Path::new(d), &shims), "{d}");
            assert!(skipped(MISE, Path::new(d), &shims), "{d}");
        }
        assert!(skipped(HOMEBREW, Path::new("/opt/homebrew/bin"), &shims));
        assert!(!skipped(HOMEBREW, Path::new("/usr/local/bin"), &shims));
        assert!(!skipped(HOMEBREW, &local_bin, &shims));
        assert!(skipped(MISE, &shims, &shims));
        assert!(!skipped(MISE, Path::new("/opt/homebrew/bin"), &shims));
        assert!(!skipped(MISE, &local_bin, &shims));
        // uv's `~/.local/bin` is examined: only a link into uv's tool directory is its own.
        assert!(!skipped(UV_TOOL, &local_bin, &shims));
        assert!(!skipped(UV_TOOL, &shims, &shims));
    }

    #[test]
    fn a_uv_receipt_names_the_source_an_upgrade_would_resolve_again() {
        let home = std::env::temp_dir().join(format!("dfm-uv-receipt-{}", std::process::id()));
        let dir = home.join(".local/share/uv/tools/agent-reap");
        std::fs::create_dir_all(&dir).unwrap();
        let receipt = |body: &str| std::fs::write(dir.join("uv-receipt.toml"), body).unwrap();
        receipt("[tool]\nrequirements = [{ name = \"agent-reap\" }]\nentrypoints = []\n");
        assert_eq!(uv_receipt_source(&home, "agent_reap"), Ok(None));
        assert_eq!(uv_receipt_source(&home, "Agent..Reap"), Ok(None));
        // An empty options table is the default.
        receipt("[tool]\nrequirements = [{ name = \"agent-reap\" }]\n[tool.options]\n");
        assert_eq!(uv_receipt_source(&home, "agent-reap"), Ok(None));
        let source = |key: &str| {
            Some(format!(
                "installed from a {key} source, which an upgrade would resolve again outside the plan"
            ))
        };
        receipt(
            "[tool]\nrequirements = [{ name = \"agent-reap\", git = \"ssh://git@example.invalid/x\" }]\n",
        );
        assert_eq!(uv_receipt_source(&home, "agent-reap"), Ok(source("git")));
        receipt("[tool]\nrequirements = [{ name = \"agent-reap\", path = \"/tmp/x\" }]\n");
        assert_eq!(uv_receipt_source(&home, "agent-reap"), Ok(source("path")));
        receipt("[tool]\nrequirements = [{ name = \"agent-reap\", directory = \"/tmp/x\" }]\n");
        assert_eq!(
            uv_receipt_source(&home, "agent-reap"),
            Ok(source("directory"))
        );
        // A retained version constraint is respected by `uv tool upgrade`, so it holds too; an
        // empty specifier is the default and rolls.
        receipt("[tool]\nrequirements = [{ name = \"agent-reap\", specifier = \"==0.5\" }]\n");
        assert_eq!(
            uv_receipt_source(&home, "agent-reap"),
            Ok(Some(
                "installed with a version constraint in the receipt, which an upgrade respects outside the plan"
                    .into()
            ))
        );
        receipt("[tool]\nrequirements = [{ name = \"agent-reap\", specifier = \"\" }]\n");
        assert_eq!(uv_receipt_source(&home, "agent-reap"), Ok(None));
        // A persisted install option is kept by `uv tool upgrade`: an index the plan never
        // showed, or any other setting, holds the tool.
        for (options, keys) in [
            (
                "index-url = \"https://example.invalid/simple\"",
                "index-url",
            ),
            (
                "default-index = \"https://example.invalid/simple\"",
                "default-index",
            ),
            (
                "compile-bytecode = true\nprerelease = \"allow\"",
                "compile-bytecode, prerelease",
            ),
        ] {
            receipt(&format!(
                "[tool]\nrequirements = [{{ name = \"agent-reap\" }}]\n[tool.options]\n{options}\n"
            ));
            assert_eq!(
                uv_receipt_source(&home, "agent-reap"),
                Ok(Some(format!(
                    "installed with uv options ({keys}), which an upgrade would keep outside the plan"
                ))),
                "{options}"
            );
        }
        // uv's legacy receipt wrote a requirement as one PEP 508 string, whose source the
        // table keys cannot see: any string requirement, even a bare name, holds.
        let legacy = Some(
            "installed with a legacy string requirement in the receipt, whose source an upgrade would resolve again outside the plan".to_string(),
        );
        for reqs in [
            "[\"agent-reap @ git+https://example.invalid/x\"]",
            "[\"agent-reap\"]",
            "[{ name = \"agent-reap\" }, \"other>=1\"]",
        ] {
            receipt(&format!("[tool]\nrequirements = {reqs}\n"));
            assert_eq!(
                uv_receipt_source(&home, "agent-reap"),
                Ok(legacy.clone()),
                "{reqs}"
            );
        }
        receipt("[tool]\nentrypoints = []\n");
        assert!(uv_receipt_source(&home, "agent-reap").is_err());
        receipt("not = [toml");
        assert!(uv_receipt_source(&home, "agent-reap").is_err());
        assert!(uv_receipt_source(&home, "absent").is_err());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_mise_install_always_names_a_version() {
        assert_eq!(mise_selector("duckdb"), "duckdb@latest");
        assert_eq!(mise_selector("go@1.27"), "go@1.27");
        assert_eq!(mise_selector("npm:@scope/pkg"), "npm:@scope/pkg@latest");
        assert_eq!(mise_selector("npm:@scope/pkg@1.2"), "npm:@scope/pkg@1.2");
    }

    #[test]
    fn uv_names_normalize_as_pep_503_does() {
        for spelled in [
            "Friendly__Bard",
            "friendly--bard",
            "friendly.bard",
            "friendly-_.bard",
        ] {
            assert_eq!(uv_norm(spelled), "friendly-bard", "{spelled}");
        }
    }

    #[test]
    fn the_uv_listing_is_read_by_tool_name_and_a_source_is_refused() {
        let listing = "agent-reap v0.1.0\n- agent-reap\nnuv v0.5.2\n- nuv\n";
        assert_eq!(uv_tool_version(listing, "nuv"), Some("0.5.2".into()));
        assert_eq!(uv_tool_version(listing, "agent_reap"), Some("0.1.0".into()));
        assert_eq!(
            uv_tool_version(listing, "Agent__Reap"),
            Some("0.1.0".into())
        );
        assert_eq!(uv_tool_version(listing, "agent"), None);
        assert_eq!(uv_tool_version("No tools installed\n", "nuv"), None);
        assert_eq!(uv_tool_version("No tools installed\n", "No"), None);
        for good in ["ruff", "agent_reap", "a.b"] {
            assert!(uv_name(good), "{good}");
        }
        for bad in [
            "git+https://example.invalid/x",
            "./here",
            "ruff==0.5",
            "ruff@0.5",
            "-x",
            "",
        ] {
            assert!(!uv_name(bad), "{bad}");
        }
        assert!(local_build("ref:main"));
        assert!(local_build("path:/tmp/x"));
        assert!(!local_build("1.5.6"));
    }

    #[test]
    fn the_brew_outdated_answer_is_read_as_json_only() {
        let ok = r#"{"formulae":[{"name":"libpng","current_version":"1.7"}],"casks":[]}"#;
        assert_eq!(outdated_names(ok), Some(vec!["libpng".to_string()]));
        assert_eq!(
            outdated_names(r#"{"formulae":[],"casks":[]}"#),
            Some(vec![])
        );
        for bad in ["", "libpng", "{}", r#"{"formulae":[{}],"casks":[]}"#] {
            assert_eq!(outdated_names(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_mise_listing_is_read_as_json_only() {
        let ok = br#"[{"version":"1.5.6","installed":true},{"version":"2.0.0","installed":false}]"#;
        assert_eq!(installed_versions(ok), Some(vec!["1.5.6".to_string()]));
        assert_eq!(installed_versions(b"[]"), Some(vec![]));
        for bad in [&b"duckdb 1.5.6"[..], b"{}", br#"[{"installed":true}]"#, b""] {
            assert_eq!(
                installed_versions(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn an_installed_version_matches_the_declared_one_by_whole_components() {
        assert!(version_matches("20.20.2", ""));
        assert!(version_matches("20.20.2", "20"));
        assert!(version_matches("20.20.2", "20.20"));
        assert!(version_matches("20.20.2", "20.20.2"));
        assert!(version_matches("openjdk-21.0.2", "openjdk-21"));
        assert!(!version_matches("20.20.2", "2"));
        assert!(!version_matches("20.20.2", "20.2"));
        assert!(!version_matches("2.1.0", "20"));
        for s in [
            "latest",
            "lts",
            "lts-hydrogen",
            "sub-1:latest",
            "ref:main",
            "system",
        ] {
            assert!(symbolic(s), "{s}");
        }
        for s in ["", "20", "20.1", "openjdk-21", "2024-01"] {
            assert!(!symbolic(s), "{s}");
        }
    }

    #[test]
    fn an_update_policy_outside_the_known_three_is_a_manifest_error() {
        let v = |p: Option<&str>| {
            let mut t = dfm_model::resolve::Table::new();
            if let Some(p) = p {
                t.insert("update_policy".into(), Value::String(p.into()));
            }
            Value::Table(t)
        };
        for good in ["rolling", "hold", "native-request"] {
            assert_eq!(policy(&v(Some(good))).unwrap(), good);
        }
        for bad in [Some("rollng"), Some(""), None] {
            let e = policy(&v(bad)).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{bad:?}");
            assert!(
                e.detail
                    .ends_with(&format!("not {}", bad.unwrap_or("unset"))),
                "{}",
                e.detail
            );
        }
    }

    #[test]
    fn a_git_source_needs_a_tag_or_a_commit() {
        let root = Path::new("/r");
        let v = |s: &str| {
            let mut t = dfm_model::resolve::Table::new();
            t.insert("name".into(), Value::String("token-auditor".into()));
            t.insert("update_policy".into(), Value::String("rolling".into()));
            t.insert("source".into(), Value::String(s.into()));
            Value::Table(t)
        };
        assert_eq!(
            uv_source(&v("git+https://github.com/o/r@2026.11.11"), root),
            Ok(Some(UvSource::Git {
                url: "https://github.com/o/r".into(),
                rev: Some("2026.11.11".into())
            }))
        );
        // Without a ref: the rolling form.
        assert_eq!(
            uv_source(&v("git+https://github.com/o/r"), root),
            Ok(Some(UvSource::Git {
                url: "https://github.com/o/r".into(),
                rev: None
            }))
        );
        assert!(matches!(
            uv_source(&v("git+https://github.com/o/r@v1.2.3"), root),
            Ok(Some(_))
        ));
        let commit = format!("git+https://github.com/o/r@{}", "a".repeat(40));
        assert!(matches!(uv_source(&v(&commit), root), Ok(Some(_))));
        for bad in [
            "git+https://github.com/o/r@main",
            "git+http://github.com/o/r@1.0",
            "git+ssh://git@github.com/o/r@1.0",
            "https://github.com/o/r@1.0",
            "git+https://github.com/o/r@1",
            "git+https://github.com/o/r@v1.",
            &format!("git+https://github.com/o/r@{}", "A".repeat(40)),
        ] {
            assert_eq!(
                uv_source(&v(bad), root).unwrap_err().code,
                Code::ManifestInvalid,
                "{bad}"
            );
        }
        let mut t = dfm_model::resolve::Table::new();
        t.insert("name".into(), Value::String("token-auditor".into()));
        assert_eq!(uv_source(&Value::Table(t), root), Ok(None));
    }

    /// A new directory for one test, as the other test modules of this crate make it.
    fn scratch(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "dfm-package-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_receipt_matches_the_source_only_through_the_declared_package() {
        let home = scratch("uv-names");
        let dir = home.join(".local/share/uv/tools/token-auditor");
        std::fs::create_dir_all(&dir).unwrap();
        let source = UvSource::Git {
            url: "https://github.com/o/r".into(),
            rev: Some("1.0.0".into()),
        };
        let receipt = |reqs: &str| {
            std::fs::write(
                dir.join("uv-receipt.toml"),
                format!("[tool]\nrequirements = [{reqs}]\n"),
            )
            .unwrap();
        };
        // The tool itself from the index, a `--with` extra from the declared source.
        receipt(
            r#"{ name = "token-auditor" }, { name = "helper", git = "https://github.com/o/r?rev=1.0.0" }"#,
        );
        assert!(!uv_receipt_names(&home, "token-auditor", &source));
        // The tool from the source, under a name uv spells another way.
        receipt(r#"{ name = "Token_Auditor", git = "https://github.com/o/r?rev=1.0.0" }"#);
        assert!(uv_receipt_names(&home, "token-auditor", &source));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_path_source_is_a_directory_inside_the_checkout() {
        let tmp = scratch("uv-path");
        let root = tmp.join("root");
        std::fs::create_dir_all(root.join("agent_reap")).unwrap();
        std::fs::create_dir_all(tmp.join("x")).unwrap();
        std::fs::write(root.join("file"), "").unwrap();
        std::os::unix::fs::symlink("/tmp", root.join("link")).unwrap();
        let v = |s: &str| {
            let mut t = dfm_model::resolve::Table::new();
            t.insert("source".into(), Value::String(s.into()));
            Value::Table(t)
        };
        let canon = root.canonicalize().unwrap().join("agent_reap");
        assert_eq!(
            uv_source(&v("path:agent_reap"), &root),
            Ok(Some(UvSource::Path(canon)))
        );
        for bad in [
            "path:../x",
            "path:link",
            "path:file",
            "path:absent",
            "path:",
            "path:/tmp",
        ] {
            assert_eq!(
                uv_source(&v(bad), &root).unwrap_err().code,
                Code::ManifestInvalid,
                "{bad}"
            );
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_package_name_is_a_checked_parameter() {
        let v = |n: &str| {
            let mut t = dfm_model::resolve::Table::new();
            t.insert("name".into(), Value::String(n.into()));
            Value::Table(t)
        };
        for good in [
            "atuin",
            "homebrew/cask/foo",
            "node@20",
            "libc++",
            "npm:left-pad",
        ] {
            assert_eq!(name(&v(good)).unwrap(), good, "{good}");
        }
        for bad in [
            "",
            "-x",
            "a b",
            "a;b",
            "..",
            "a/../b",
            "a//b",
            &"a".repeat(129),
        ] {
            assert_eq!(
                name(&v(bad)).unwrap_err().code,
                Code::ManifestInvalid,
                "{bad:?}"
            );
        }
    }
}
