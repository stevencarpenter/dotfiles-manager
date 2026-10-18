//! The package adapters `homebrew/v1`, `mise/v1` and `apt/v1` (docs/packages.md): a read-only
//! probe of the declared provider's own listing, an owner check by executable name, and an
//! install through the same provider. Nothing here reads or writes a version pin: a package
//! plan makes a declared package present and never upgrades, downgrades or removes.
//!
//! Every child runs through the engine runner (fixed search path, rebuilt environment, process
//! group, timeout). The providers' standard error is bounded and never printed or persisted.

use dfm_engine::{End, ProcOutput, ProcRequest, Runner};
use dfm_model::plan::{PackageState, Scope};
use dfm_model::resolve::{Resolved, Value};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

pub const HOMEBREW: &str = "homebrew/v1";
pub const MISE: &str = "mise/v1";
pub const APT: &str = "apt/v1";

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A listing or an install log is bounded; its content is not kept.
const MAX_OUTPUT: usize = 1 << 20;

/// Homebrew probes and installs disable auto-update, analytics, hints and cleanup
/// (architecture section 8 item 5), the upgrade of an outdated requested formula, and the
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

/// The package name as a checked parameter (architecture section 8 item 3): letters, digits,
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

/// Whether `n` is a package name the adapters pass to a provider as an argument.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 128
        && !n.starts_with('-')
        && n.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@' | '/' | ':')
        })
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
fn ended(what: &str, out: &ProcOutput) -> Option<String> {
    match &out.end {
        End::Exited(_) => None,
        End::TimedOut => Some(format!("{what} did not answer within {PROBE_TIMEOUT:?}")),
        End::Signaled => Some(format!("{what} was ended by a signal")),
        End::Stopped(r) => Some(r.detail.clone()),
    }
}

/// `brew list --versions <name>`: exit 0 and a line `<name> <version>` means installed; exit 1
/// with no output means not installed.
fn brew_probe(runner: &Runner, pkg: &str) -> PackageState {
    let args = ["list".to_string(), "--versions".into(), pkg.into()];
    let out = match runner.run(&request("brew", &args, &brew_env(), PROBE_TIMEOUT)) {
        Ok(o) => o,
        Err(e) => return PackageState::Unknown(e.detail),
    };
    if let Some(why) = ended("brew list", &out) {
        return PackageState::Unknown(why);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `brew list --versions homebrew/core/atuin` prints `atuin 18.24.1`: the short name, which
    // keeps a `@version` suffix (`node@20 20.1.0`).
    let short = pkg.rsplit('/').next().unwrap_or(pkg);
    let line = text.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        (parts.next() == Some(short)).then(|| parts.collect::<Vec<_>>().join(" "))
    });
    match (&out.end, line) {
        (End::Exited(0), Some(v)) if !v.is_empty() => PackageState::Installed(v),
        (End::Exited(1), None) if text.trim().is_empty() => PackageState::Missing(Vec::new()),
        (End::Exited(c), _) => PackageState::Unknown(format!(
            "brew list exited {c} with an answer the adapter does not read"
        )),
        _ => PackageState::Unknown("brew list gave no exit code".into()),
    }
}

/// Whether `pkg` is an official Homebrew formula or cask, without loading any formula file:
/// `brew formulae` and `brew casks` print the names Homebrew knows, official ones bare and tap
/// ones qualified (`user/tap/name`). A tap's formula is Ruby that runs when brew reads it, so the
/// dependency calls below (`deps`, `info`, `outdated`) are made only for an official name.
fn brew_official(runner: &Runner, pkg: &str) -> Result<bool, String> {
    let short = match pkg.rsplit_once('/') {
        None => pkg,
        Some(("homebrew/core" | "homebrew/cask", n)) => n,
        Some(_) => return Ok(false),
    };
    let env = brew_env();
    for list in ["formulae", "casks"] {
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
            return Ok(true);
        }
    }
    Ok(false)
}

/// What `brew install <pkg>` would do to the formula's dependencies, from local data only (no
/// download, auto-update is off): `brew deps <pkg>` lists them, `brew list --versions` says which
/// are installed, `brew info --json=v2` whether the package and the missing ones have a bottle
/// for this machine, and `brew outdated --json=v2` which installed ones the install would
/// upgrade. The missing ones become part of the plan. A source build blocks it, because its
/// build-time dependencies are not in the listing; so does an outdated dependency.
fn brew_dependencies(runner: &Runner, pkg: &str) -> PackageState {
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
    let deps: Vec<String> = match run("brew deps", vec!["deps".into(), pkg.into()], &[0]) {
        Ok(t) => t.split_whitespace().map(str::to_string).collect(),
        Err(why) => return PackageState::Unknown(why),
    };
    if let Some(bad) = deps.iter().find(|d| !valid_name(d)) {
        return PackageState::Unknown(format!(
            "brew deps named {bad:?}, which is not a package name"
        ));
    }
    let (have, missing): (Vec<String>, Vec<String>) = if deps.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        // `brew list --versions` prints the installed ones by short name and exits 1 if any is
        // absent.
        let mut args = vec!["list".to_string(), "--versions".into()];
        args.extend(deps.iter().cloned());
        let listed = match run("brew list", args, &[0, 1]) {
            Ok(t) => t,
            Err(why) => return PackageState::Unknown(why),
        };
        let installed: BTreeSet<&str> = listed
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        deps.into_iter()
            .partition(|d| installed.contains(d.rsplit('/').next().unwrap_or(d)))
    };
    // Everything the install would add must come as a bottle for this machine.
    let mut args = vec!["info".to_string(), "--json=v2".into(), pkg.into()];
    args.extend(missing.iter().cloned());
    match run("brew info", args, &[0]).map(|t| unbottled(&t)) {
        Ok(Some(n)) if n.is_empty() => {}
        Ok(Some(n)) => return PackageState::SourceBuild(n),
        Ok(None) => {
            return PackageState::Unknown(format!(
                "brew info --json=v2 for {pkg} is not the answer the adapter reads"
            ));
        }
        Err(why) => return PackageState::Unknown(why),
    }
    if have.is_empty() {
        return PackageState::Missing(missing);
    }
    // `brew outdated` exits 1 when it finds an outdated one; the JSON is the answer either way.
    let mut args = vec!["outdated".to_string(), "--json=v2".into()];
    args.extend(have.iter().cloned());
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

/// The formulae in a `brew info --json=v2` answer that have no bottle for this machine, or
/// `None` when it is not that shape. Homebrew reports only this machine's bottle tags, so an
/// empty file list means a source build. A cask has no source build.
fn unbottled(text: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();
    for f in v.get("formulae")?.as_array()? {
        let name = f.get("name")?.as_str()?;
        let files = f
            .get("bottle")?
            .get("stable")
            .and_then(|s| s.get("files"))
            .and_then(serde_json::Value::as_object);
        if files.is_none_or(|m| m.is_empty()) {
            out.push(name.to_string());
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
    let (tool, want) = tool_and_version(pkg);
    if symbolic(want) {
        return PackageState::Refused(format!(
            "{pkg}: mise/v1 takes an exact version or a version prefix (node@20, node@20.1), not a selector mise resolves online ({want})"
        ));
    }
    let args = [
        "ls".to_string(),
        "--installed".into(),
        "--json".into(),
        tool.into(),
    ];
    let out = match runner.run(&request("mise", &args, &mise_env(), PROBE_TIMEOUT)) {
        Ok(o) => o,
        Err(e) => return PackageState::Unknown(e.detail),
    };
    if let Some(why) = ended("mise ls", &out) {
        return PackageState::Unknown(why);
    }
    if !matches!(out.end, End::Exited(0)) {
        return PackageState::Unknown(format!(
            "mise ls exited with an error for {pkg} (an unknown tool, or a broken configuration)"
        ));
    }
    let Some(listed) = installed_versions(&out.stdout) else {
        return PackageState::Unknown(format!(
            "mise ls --json for {pkg} is not the array of versions the adapter reads"
        ));
    };
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
fn tool_and_version(pkg: &str) -> (&str, &str) {
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
fn leaf(pkg: &str) -> &str {
    let (tool, _) = tool_and_version(pkg);
    tool.rsplit('/').next().unwrap_or(tool)
}

/// Directories where another owner can have put an executable of the package's name: the other
/// search-path prefixes, Nix profiles, `~/.local/bin`, `~/.cargo/bin` and the mise shims. The
/// declared provider's own directories are skipped by path component (`homebrew` for brew, the
/// mise shim directory for mise), never by substring. A directory that cannot be examined is an
/// error, so the check fails closed.
/// shortcut: the check compares the package name with an executable name, so a package whose
/// binary is named differently is not caught; upgrade when adapters declare their binaries.
fn other_owner(runner: &Runner, adapter: &str, pkg: &str) -> Result<Option<String>, String> {
    let home = &runner.home;
    let shims = home.join(".local/share/mise/shims");
    let mut dirs: Vec<PathBuf> = runner.search_path.clone();
    dirs.extend([
        home.join(".nix-profile/bin"),
        PathBuf::from("/nix/var/nix/profiles/default/bin"),
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        shims.clone(),
    ]);
    let own = |d: &PathBuf| match adapter {
        HOMEBREW => d.components().any(|c| c.as_os_str() == "homebrew"),
        MISE => *d == shims,
        _ => false,
    };
    let name = leaf(pkg);
    for d in dirs.iter().filter(|d| !own(d)) {
        let p = d.join(name);
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

/// The installed state of one package by its adapter, with the owner check when the provider
/// does not list it.
pub fn probe(runner: &Runner, adapter: &str, value: &Value) -> Result<PackageState, Refusal> {
    let pkg = name(value)?;
    let state = match adapter {
        HOMEBREW => brew_probe(runner, &pkg),
        MISE => mise_probe(runner, &pkg),
        APT => apt_probe(runner, &pkg),
        other => {
            return Err(Refusal::new(
                Code::GeneratorUnavailable,
                format!("no package provider for {other}"),
            ));
        }
    };
    Ok(match state {
        PackageState::Missing(_) => match other_owner(runner, adapter, &pkg) {
            Ok(Some(why)) => PackageState::OtherOwner(why),
            Ok(None) if adapter == HOMEBREW => match brew_official(runner, &pkg) {
                Ok(true) => brew_dependencies(runner, &pkg),
                Ok(false) => PackageState::Unofficial(format!(
                    "{pkg} is not an official Homebrew formula or cask; reading its dependencies would load tap code before approval"
                )),
                Err(why) => PackageState::Unknown(why),
            },
            Ok(None) => PackageState::Missing(Vec::new()),
            Err(why) => PackageState::Unknown(why),
        },
        s => s,
    })
}

/// Installs one package through its provider. The caller probes again afterwards.
pub fn install(runner: &Runner, adapter: &str, value: &Value) -> Result<(), Refusal> {
    let pkg = name(value)?;
    let (program, args, env) = match adapter {
        HOMEBREW => ("brew", vec!["install".to_string(), pkg.clone()], brew_env()),
        MISE => ("mise", vec!["install".to_string(), pkg.clone()], mise_env()),
        _ => {
            return Err(Refusal::new(
                Code::PrivilegeUnsupported,
                format!("{adapter} cannot install {pkg}: a HOME adapter never escalates"),
            ));
        }
    };
    let out = runner.run(&request(program, &args, &env, INSTALL_TIMEOUT))?;
    match out.end {
        End::Exited(0) => Ok(()),
        End::Exited(c) => Err(Refusal::new(
            Code::IoFailed,
            format!(
                "{program} install {pkg} exited {c}; its {} bytes of standard error are withheld. Run it by hand to see them",
                out.stderr.len()
            ),
        )),
        End::TimedOut => Err(Refusal::new(
            Code::IoFailed,
            format!("{program} install {pkg} did not finish within {INSTALL_TIMEOUT:?}"),
        )),
        End::Signaled => Err(Refusal::new(
            Code::IoFailed,
            format!("{program} install {pkg} was ended by a signal"),
        )),
        End::Stopped(r) => Err(r),
    }
}

/// Probes every selected package resource for a packages plan, by resource id. A stop request
/// (SIGINT or SIGTERM to the manager) ends the probing: the probe it arrived in is ended by
/// the runner, no further provider is started, and the request is the error.
pub fn package_facts(
    resolved: &Resolved,
    runner: &Runner,
) -> Result<BTreeMap<String, PackageState>, Refusal> {
    let mut out = BTreeMap::new();
    for r in resolved
        .resources
        .values()
        .filter(|r| Scope::Packages.covers(r))
    {
        if let Some(stop) = dfm_engine::signal::requested() {
            return Err(stop);
        }
        let state = match probe(runner, &r.adapter, &r.value) {
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
    fn a_formula_without_a_bottle_for_this_machine_is_a_source_build() {
        let t = r#"{"formulae":[
            {"name":"imagemagick","bottle":{"stable":{"files":{"arm64_golden_gate":{}}}}},
            {"name":"afuse","bottle":{}}],"casks":[{"token":"x"}]}"#;
        assert_eq!(unbottled(t), Some(vec!["afuse".to_string()]));
        assert_eq!(unbottled(r#"{"formulae":[],"casks":[]}"#), Some(vec![]));
        for bad in [
            "",
            "{}",
            r#"{"formulae":[{"bottle":{}}]}"#,
            r#"{"formulae":[{"name":"x"}]}"#,
        ] {
            assert_eq!(unbottled(bad), None, "{bad:?}");
        }
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
        for bad in ["", "-x", "a b", "a;b", &"a".repeat(129)] {
            assert_eq!(
                name(&v(bad)).unwrap_err().code,
                Code::ManifestInvalid,
                "{bad:?}"
            );
        }
    }
}
