//! The `git-checkout/v1` package adapter (docs/packages.md, "Git checkouts"): a git repository
//! cloned to a HOME-relative path. A declared checkout needs no grant: the owner wrote the
//! declaration. The probe reads only local git state. The install clones a missing checkout
//! and never pulls (the `checkouts` step of `dfm sync` fast-forwards it).

use super::package::{PROBE_TIMEOUT, change, ended, request};
use dfm_engine::Runner;
use dfm_model::display::visible;
use dfm_model::lock::git_identity;
use dfm_model::plan::PackageState;
use dfm_model::resolve::Value;
use dfm_model::state::valid_path;
use dfm_model::{Code, Refusal};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub(crate) const ADAPTER: &str = "git-checkout/v1";

/// A `git-checkout/v1` resource value, checked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Declared {
    pub url: String,
    /// The checkout directory, relative to HOME.
    pub path: PathBuf,
    pub branch: Option<String>,
}

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::new(Code::ManifestInvalid, why)
}

fn identity(url: &str) -> Result<String, Refusal> {
    git_identity(url, cfg!(feature = "test-isolation"))
}

/// The value as a declaration. The resolver checks the field types and the HOME path form; this
/// checks the URL (https, or ssh with userinfo `git` only, never a local path) and the branch.
/// A bad value is `E_MANIFEST_INVALID`.
pub(crate) fn declared(value: &Value) -> Result<Declared, Refusal> {
    let text = |k: &str| {
        value
            .get(k)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("a git-checkout/v1 declaration needs {k}")))
    };
    let url = text("url")?;
    // `o/r` is a GitHub shorthand for a source identity, but git reads it as a local path.
    if !(url.contains("://") || url.starts_with("git@")) {
        return Err(invalid(format!(
            "url {} is not an https:// or ssh remote",
            visible(url)
        )));
    }
    identity(url).map_err(|e| invalid(format!("url: {}", e.detail)))?;
    let path = text("path")?;
    if !valid_path(&format!("/{path}")) {
        return Err(invalid(format!(
            "path {} is not a plain path relative to HOME",
            visible(path)
        )));
    }
    let branch = match value.get("branch") {
        None => None,
        Some(b) => {
            let b = b
                .as_str()
                .filter(|b| {
                    !b.is_empty()
                        && !b.starts_with('-')
                        && !b.contains("..")
                        && !b.chars().any(|c| c.is_whitespace() || c.is_control())
                })
                .ok_or_else(|| {
                    invalid("branch must be a name without whitespace, a leading '-' or '..'")
                })?;
            Some(b.to_string())
        }
    };
    Ok(Declared {
        url: url.into(),
        path: path.into(),
        branch,
    })
}

/// Runs a read-only `git -C <dir>` query. `Ok(None)` is an exit code other than 0.
fn query(runner: &Runner, dir: &Path, rest: &[&str]) -> Result<Option<String>, Refusal> {
    let mut args = vec!["-C".to_string(), dir.display().to_string()];
    args.extend(rest.iter().map(|a| a.to_string()));
    let what = format!("git {}", rest.join(" "));
    let out = runner.run(&request("git", &args, &[], PROBE_TIMEOUT))?;
    if let Some(why) = ended(&what, &out) {
        return Err(Refusal::new(Code::ProbeFailed, why));
    }
    Ok(matches!(out.end, dfm_engine::End::Exited(0))
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// The state of the checkout: no path is `Missing`, a git checkout of the declared repository
/// is `Installed` (its short HEAD), and anything else at the path is `OtherOwner`.
pub(crate) fn probe(runner: &Runner, value: &Value) -> Result<PackageState, Refusal> {
    let d = declared(value)?;
    let dir = runner.home.join(&d.path);
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(PackageState::Missing(Vec::new())),
        Err(e) => {
            return Err(Refusal::new(
                Code::ProbeFailed,
                format!("{} could not be read: {}", dir.display(), e.kind()),
            ));
        }
        Ok(_) => {}
    }
    let other = |why: &str| PackageState::OtherOwner(format!("{} {why}", dir.display()));
    if fs::symlink_metadata(dir.join(".git")).is_err() {
        return Ok(other("exists and is not a git checkout"));
    }
    let Some(origin) = query(runner, &dir, &["config", "--get", "remote.origin.url"])? else {
        return Ok(other("is a git checkout with no origin"));
    };
    let want = identity(&d.url)?;
    match identity(&origin) {
        Ok(got) if got == want => {}
        Ok(got) => return Ok(other(&format!("is a checkout of {got}, not {want}"))),
        Err(_) => {
            return Ok(other(
                "is a checkout whose origin is not a canonical https or ssh remote",
            ));
        }
    }
    let head = query(runner, &dir, &["rev-parse", "--short", "HEAD"])?
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "no commit".into());
    Ok(PackageState::Installed(head))
}

/// Clones the declared repository into the missing path, with prompts off. The caller probes
/// again afterwards. A failed clone leaves nothing this function removes: git cleans its own.
pub(crate) fn install(runner: &Runner, value: &Value) -> Result<(), Refusal> {
    let d = declared(value)?;
    let dir = runner.home.join(&d.path);
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            Refusal::new(
                Code::IoFailed,
                format!("{} could not be created: {}", parent.display(), e.kind()),
            )
        })?;
    }
    let dest = dir
        .to_str()
        .ok_or_else(|| invalid("path resolves to a path that is not UTF-8"))?;
    let mut args = vec!["clone".to_string()];
    if let Some(b) = &d.branch {
        args.extend(["--branch".to_string(), b.clone()]);
    }
    args.extend([d.url.clone(), dest.to_string()]);
    let env = [("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())];
    change(
        runner,
        "git",
        &args,
        &env,
        &format!("git {}", args.join(" ")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(url: &str, extra: &str) -> Value {
        Value::Table(toml::from_str(&format!("url = {url:?}\npath = \"x/r\"\n{extra}")).unwrap())
    }

    #[test]
    fn the_accepted_remote_forms_are_declarations() {
        for url in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r",
        ] {
            let d = declared(&value(url, "branch = \"main\"")).unwrap();
            assert_eq!(d.url, url);
            assert_eq!(d.path, PathBuf::from("x/r"));
            assert_eq!(d.branch.as_deref(), Some("main"));
        }
        assert_eq!(
            declared(&value("https://h.io/o/r", "")).unwrap().branch,
            None
        );
    }

    #[test]
    fn a_bad_url_or_branch_is_a_manifest_error() {
        let mut bad = vec![
            value("/srv/git/r", ""),
            value("o/r", ""),
            value("https://user@github.com/o/r", ""),
            value("ssh://bob@github.com/o/r", ""),
            value("http://github.com/o/r", ""),
        ];
        #[cfg(not(feature = "test-isolation"))]
        bad.push(value("file:///srv/git/r", ""));
        for b in [
            "\"a b\"",
            "\"-x\"",
            "\"a..b\"",
            "\"\"",
            "\"a\\u0007b\"",
            "7",
        ] {
            bad.push(value("https://github.com/o/r", &format!("branch = {b}")));
        }
        for v in bad {
            let e = declared(&v).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{v:?}: {e}");
        }
    }
}
