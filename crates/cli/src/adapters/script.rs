//! The `script/v1` package adapter (DFM-83, docs/packages.md "Script declarations"): a tool
//! that `dfm adopt` took over from a script trial. The binary path decides whether it is
//! installed. A missing binary is installed by running the declared script again, under the
//! trial's sandbox, environment rule and tracked-file guard, without a terminal, and only when
//! the script's identity `script:<url>` has a grant.

use crate::sync_cmd::{Git, guarded_checkouts, tracked_status};
use crate::trial_cmd::FIXED_ENV;
use crate::trial_cmd::script::{
    Confined, account_login_shell, cache_paths, cached, ended, fetch, io_failed, profile,
    protected, ranked, sandbox_exec, script_env, tracked_changes, write_private,
};
use crate::{Ctx, state_cmd};
use dfm_engine::{Layout, Runner, Store, Sys};
use dfm_model::digest::sha256_hex;
use dfm_model::display::visible;
use dfm_model::lock::{SCRIPT, script_url, trust_command};
use dfm_model::plan::PackageState;
use dfm_model::resolve::Value;
use dfm_model::state::{SHELLS, valid_path};
use dfm_model::{Code, Refusal};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const ADAPTER: &str = "script/v1";

/// A `script/v1` resource value, checked (docs/packages.md, "Script declarations").
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Declared {
    pub url: String,
    pub effective_url: String,
    pub shell: String,
    pub args: Vec<String>,
    /// `--env` values of the trial, sorted by name. None is a name the run fixes.
    pub env: Vec<(String, String)>,
    /// The tool's executable, relative to HOME.
    pub binary: String,
}

impl Declared {
    /// The grant identity: `script:` and the declared URL as written.
    pub fn identity(&self) -> String {
        format!("{SCRIPT}{}", self.url)
    }
}

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::new(Code::ManifestInvalid, why)
}

/// An environment name a `--env` value may set: `[A-Za-z_][A-Za-z0-9_]*`, not in the fixed list.
fn env_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
        && !FIXED_ENV.contains(&n)
}

/// The value as a declaration. The resolver checks the field types and the HOME path of
/// `binary`; this checks the rest. A bad value is `E_MANIFEST_INVALID`.
pub(crate) fn declared(value: &Value) -> Result<Declared, Refusal> {
    let text = |k: &str| {
        value
            .get(k)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("a script/v1 declaration needs {k}")))
    };
    let (url, effective_url) = (text("url")?, text("effective_url")?);
    for (k, u) in [("url", url), ("effective_url", effective_url)] {
        if !script_url(u) {
            return Err(invalid(format!(
                "{k} {} is not an https:// URL without spaces or control characters (at most 2048 bytes)",
                visible(u)
            )));
        }
    }
    let shell = text("shell")?;
    if !SHELLS.contains(&shell) {
        return Err(invalid(format!(
            "shell is {}, not {}",
            SHELLS.join(", "),
            visible(shell)
        )));
    }
    let mut args = Vec::new();
    for a in value
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match a.as_str() {
            Some(a) if !a.chars().any(char::is_control) => args.push(a.to_string()),
            _ => return Err(invalid("args holds a value with a control character")),
        }
    }
    let mut env = Vec::new();
    for (n, v) in value
        .get("env")
        .and_then(Value::as_table)
        .into_iter()
        .flatten()
    {
        if !env_name(n) {
            return Err(invalid(format!(
                "env name {} must match [A-Za-z_][A-Za-z0-9_]* and not be one the run fixes ({})",
                visible(n),
                FIXED_ENV.join(", ")
            )));
        }
        match v.as_str() {
            Some(v) if !v.chars().any(|c| c != '\t' && c.is_control()) => {
                env.push((n.clone(), v.to_string()));
            }
            _ => return Err(invalid(format!("env {n} holds a control character"))),
        }
    }
    env.sort();
    let binary = text("binary")?;
    if !valid_path(&format!("/{binary}")) {
        return Err(invalid(format!(
            "binary {} is not a plain path relative to HOME",
            visible(binary)
        )));
    }
    Ok(Declared {
        url: url.into(),
        effective_url: effective_url.into(),
        shell: shell.into(),
        args,
        env,
        binary: binary.into(),
    })
}

/// What a script install needs besides the runner: the layout, the bound checkout the guard
/// watches, and git for that guard. The grants and the ledger are read when needed.
pub(crate) struct ScriptCx {
    layout: Layout,
    sys: Sys,
    git: Git,
    root: PathBuf,
}

impl ScriptCx {
    pub(crate) fn new(ctx: &Ctx, sys: &Sys, root: &Path) -> ScriptCx {
        ScriptCx {
            layout: ctx.layout(),
            sys: *sys,
            git: Git::new(ctx, sys),
            root: root.to_path_buf(),
        }
    }

    /// A read view of the state. A writer in this process may hold the lock: the reads take none.
    fn store(&self) -> Result<Store, Refusal> {
        Store::open_read(self.sys, &self.layout, state_cmd::fail_from_env()?)?
            .ok_or_else(crate::state_lost_refusal)
    }
}

/// A regular file with an execute bit at `path`, through links.
fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn redirected(d: &Declared, got: &str) -> String {
    format!(
        "the script {} now ends at {}, not at the declared effective URL {}; a redirect is followed only to the declared one",
        visible(&d.url),
        visible(got),
        visible(&d.effective_url)
    )
}

/// The installed state. A present binary is `Installed` (`Held` for `packages --update`: the
/// tool owns its upgrades) and nothing is fetched. A missing one is `Ungranted` without a
/// grant, and otherwise the script is fetched into the cache with the trial's curl rule: a fetch
/// that ends anywhere but at the declared effective URL is `Unofficial`. Without `cx` (doctor)
/// nothing is fetched and a missing binary is `Missing`.
pub(crate) fn probe(
    runner: &Runner,
    value: &Value,
    cx: Option<&ScriptCx>,
    update: bool,
) -> Result<PackageState, Refusal> {
    let d = declared(value)?;
    let bin = runner.home.join(&d.binary);
    if executable(&bin) {
        return Ok(if update {
            PackageState::Held("the tool owns its upgrades; dfm never runs its script again".into())
        } else {
            PackageState::Installed(format!("~/{}", d.binary))
        });
    }
    let Some(cx) = cx else {
        return Ok(PackageState::Missing(Vec::new()));
    };
    if !cx.store()?.read_grants()?.trusts(&d.identity()) {
        return Ok(PackageState::Ungranted(d.identity()));
    }
    let f = match fetch(runner, &cx.layout.cache_dir, &d.url) {
        Ok(f) => f,
        Err(e) if e.code == Code::SourceUntrusted => return Ok(PackageState::Unofficial(e.detail)),
        Err(e) => return Err(e),
    };
    if f.effective_url != d.effective_url {
        drop_cache(&cx.layout.cache_dir, &d.url);
        return Ok(PackageState::Unofficial(redirected(&d, &f.effective_url)));
    }
    Ok(PackageState::Missing(Vec::new()))
}

fn drop_cache(cache_dir: &Path, url: &str) {
    let (file, sidecar) = cache_paths(cache_dir, url);
    for p in [file, sidecar] {
        let _ = fs::remove_file(p);
    }
}

/// Runs the declared script for a missing binary (docs/packages.md, "Script declarations"). It
/// runs the bytes the probe under the writer lock just fetched, as a private copy, under the
/// trial's sandbox profile and environment rule with the `--env` values, without a terminal
/// (stdin is `/dev/null`, so a prompt fails), in a fresh work directory. A tracked file the
/// script changed fails the install and stays in place. The caller probes again: the binary
/// must then exist and be executable.
pub(crate) fn install(runner: &Runner, value: &Value, cx: &ScriptCx) -> Result<(), Refusal> {
    let d = declared(value)?;
    let store = cx.store()?;
    if !store.read_grants()?.trusts(&d.identity()) {
        return Err(Refusal::new(
            Code::SourceUntrusted,
            format!(
                "the install script {} has no grant; nothing was run. Fix: {}",
                visible(&d.url),
                trust_command(&d.identity())
            ),
        ));
    }
    let cache_dir = &cx.layout.cache_dir;
    let f = cached(cache_dir, &d.url)?.ok_or_else(|| {
        Refusal::new(
            Code::PlanStale,
            format!(
                "the script cache for {} is missing; nothing was run. Fix: run sync again",
                visible(&d.url)
            ),
        )
    })?;
    if f.effective_url != d.effective_url {
        drop_cache(cache_dir, &d.url);
        return Err(Refusal::new(
            Code::SourceUntrusted,
            redirected(&d, &f.effective_url),
        ));
    }
    let dfm_env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let env = script_env(runner, &dfm_env, false, &d.env, account_login_shell)?;
    let temp = env
        .iter()
        .find_map(|(n, v)| (n == "TMPDIR").then(|| PathBuf::from(v)))
        .unwrap_or_else(std::env::temp_dir);
    let ledger = store.read_ledger_lenient()?;
    drop(store);
    let profile_text = profile(&protected(&cx.layout, &cx.root, &ledger, &temp)?)?;
    let sandbox = sandbox_exec(runner, &profile_text)?;
    let checkouts = guarded_checkouts(&cx.root)?;
    let tracked = tracked_status(&cx.git, &checkouts)?;
    let tag = format!(
        "{}-{}",
        &sha256_hex(d.url.as_bytes())[..16],
        std::process::id()
    );
    let run_file = cache_dir.join("scripts").join(format!("run-sync-{tag}"));
    write_private(&run_file, &f.body)?;
    let work = temp.join(format!("dfm-sync-{tag}"));
    let mut made = false;
    let end = Confined {
        sandbox: &sandbox,
        profile: profile_text,
        shell: &d.shell,
        file: &run_file,
        args: &d.args,
        env: &env,
        tty: false,
        work: &work,
    }
    .run(runner, &mut made);
    let (_, failed) = ended(end);
    let mut problems: Vec<Refusal> = failed.into_iter().collect();
    match tracked_status(&cx.git, &checkouts) {
        Ok(now) => {
            let changed = tracked_changes(&tracked, &now);
            if !changed.is_empty() {
                problems.push(Refusal::new(Code::Conflict, changed.join("; ")));
            }
        }
        Err(e) => problems.push(Refusal::new(
            e.code,
            format!(
                "the tracked files could not be checked after the script: {}",
                e.detail
            ),
        )),
    }
    let (cache_file, sidecar) = cache_paths(cache_dir, &d.url);
    for p in [cache_file, sidecar, run_file] {
        if let Err(e) = fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            problems.push(io_failed(&p)(e));
        }
    }
    if made
        && let Err(e) = fs::remove_dir_all(&work)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        problems.push(io_failed(&work)(e));
    }
    ranked(problems).map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> Value {
        Value::Table(toml::from_str(text).unwrap())
    }

    const GOOD: &str = r#"
url = "https://example.invalid/install.sh"
effective_url = "https://cdn.example.invalid/install.sh"
shell = "bash"
args = ["--no-modify-path"]
binary = ".local/bin/boat"
[env]
BOAT_NO_UPDATE = "1"
"#;

    #[test]
    fn a_declaration_reads_every_field() {
        let d = declared(&value(GOOD)).unwrap();
        assert_eq!(d.identity(), "script:https://example.invalid/install.sh");
        assert_eq!(d.shell, "bash");
        assert_eq!(d.args, ["--no-modify-path"]);
        assert_eq!(d.env, [("BOAT_NO_UPDATE".to_string(), "1".to_string())]);
        assert_eq!(d.binary, ".local/bin/boat");
    }

    #[test]
    fn a_bad_declaration_is_a_manifest_error() {
        for (from, to) in [
            (
                "https://example.invalid/install.sh",
                "http://example.invalid/i.sh",
            ),
            ("https://cdn.example.invalid/install.sh", "ftp://x"),
            ("\"bash\"", "\"fish\""),
            ("BOAT_NO_UPDATE", "PATH"),
            ("BOAT_NO_UPDATE", "1BAD"),
            (".local/bin/boat", "../bin/boat"),
            (".local/bin/boat", "/usr/local/bin/boat"),
            ("--no-modify-path", "a\\u0007b"),
        ] {
            let text = GOOD.replace(from, to);
            let e = declared(&value(&text)).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{to}: {e}");
        }
        let e = declared(&value("url = \"https://e.invalid/i.sh\"")).unwrap_err();
        assert!(e.detail.contains("needs effective_url"), "{e}");
    }
}
