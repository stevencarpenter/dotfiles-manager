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
use std::collections::BTreeMap;
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

/// Homebrew probes and installs disable auto-update, analytics, hints, cleanup and the implicit
/// upgrade of dependents (architecture section 8 item 5).
fn brew_env() -> Vec<(String, String)> {
    [
        "HOMEBREW_NO_AUTO_UPDATE",
        "HOMEBREW_NO_ANALYTICS",
        "HOMEBREW_NO_ENV_HINTS",
        "HOMEBREW_NO_INSTALL_CLEANUP",
        "HOMEBREW_NO_INSTALL_UPGRADE",
    ]
    .iter()
    .map(|k| (k.to_string(), "1".to_string()))
    .collect()
}

/// mise probes and installs answer no prompt and install nothing a listing asks about.
fn mise_env() -> Vec<(String, String)> {
    vec![
        ("MISE_YES".into(), "1".into()),
        ("MISE_NOT_FOUND_AUTO_INSTALL".into(), "false".into()),
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
    let ok = !n.is_empty()
        && n.len() <= 128
        && !n.starts_with('-')
        && n.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@' | '/' | ':')
        });
    if !ok {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            "a package name must be letters, digits, '.', '_', '-', '+', '@', '/' or ':', not start with '-', and be at most 128 characters",
        ));
    }
    Ok(n.to_string())
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
        (End::Exited(1), None) if text.trim().is_empty() => PackageState::Missing,
        (End::Exited(c), _) => PackageState::Unknown(format!(
            "brew list exited {c} with an answer the adapter does not read"
        )),
        _ => PackageState::Unknown("brew list gave no exit code".into()),
    }
}

/// `mise ls --installed <tool>`: one line `<tool> <version> ...` per installed version, no line
/// when none is installed. A tool mise does not know is an error (exit 1). A declared
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
    let args = ["ls".to_string(), "--installed".into(), tool.into()];
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
    let text = String::from_utf8_lossy(&out.stdout);
    let versions: Vec<&str> = text
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            (parts.next() == Some(tool)).then(|| parts.next()).flatten()
        })
        .filter(|v| !v.contains("missing"))
        .filter(|v| version_matches(v, want))
        .collect();
    if versions.is_empty() {
        PackageState::Missing
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
        PackageState::Missing => match other_owner(runner, adapter, &pkg) {
            Ok(Some(why)) => PackageState::OtherOwner(why),
            Ok(None) => PackageState::Missing,
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
