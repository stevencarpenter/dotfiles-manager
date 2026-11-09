//! The `--script` trial: fetch an install script over HTTPS and run it in a recorded window.

use super::window::{self, Watch};
use super::{
    Args, Outcome, ScriptArgs, candidates, conclude, data_candidates, observe, print_observed,
};
use crate::apply_cmd::stale;
use crate::read_fs::read_regular_bounded;
use crate::sync_cmd::{Git, guarded_checkouts, tracked_status};
use crate::{Ctx, read_existing_binding, state_cmd, usage};
use dfm_engine::proc::{End, Request, Shown};
use dfm_engine::{Layout, Runner, Store, now_ns, signal};
use dfm_model::digest::sha256_hex;
use dfm_model::display::{bounded_visible, visible};
use dfm_model::plan::Binary;
use dfm_model::state::{
    Baseline, BaselinePath, Class, JournalEntry, Ledger, Observed, Phase, Recovery, SCHEMA,
    ScriptExit, Stamp, Status, TRIAL_SCHEMA_SCRIPT, TrialRecord, TrialScript, rfc3339, valid_id,
};
use dfm_model::{Code, Refusal};
use serde_json::json;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The trial's unknown effects plus the ones a script adds ([`dfm_model::state::SCRIPT_UNKNOWN`]).
pub(super) const UNKNOWN: [&str; 7] = [
    "services",
    "paths-outside-baseline",
    "provider-caches",
    "script-downloads",
    "later-self-updates",
    "keychain",
    "remote-effects",
];

pub(super) const HINT_TTY: &str =
    "note: the script reads /dev/tty; pass --tty to answer its prompts";
pub(super) const HINT_STARTUP: &str = "note: the script edits shell startup files; files in the dotfiles repo are write-protected during the run; pass the script's own opt-out if it has one";

/// How long the script may run before its process group is ended.
// shortcut: one fixed hour, upgrade when a flag asks for it
pub(crate) const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The adapter a journal entry or receipt operation of a script trial names.
const ADAPTER: &str = "script/v1";

/// The largest script dfm fetches, as curl's `--max-filesize` enforces it.
const MAX_SCRIPT: u64 = 1_048_576;

/// A script in the cache, as the preview showed it and the run uses it.
#[derive(Debug)]
pub(crate) struct Fetched {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub effective_url: String,
    pub body: Vec<u8>,
}

/// `<cache_dir>/scripts/<sha256(url)>` and its sidecar `<...>.json`
/// `{"url","effective_url","sha256","bytes"}`.
pub(crate) fn cache_paths(cache_dir: &Path, url: &str) -> (PathBuf, PathBuf) {
    let file = cache_dir.join("scripts").join(sha256_hex(url.as_bytes()));
    let sidecar = file.with_extension("json");
    (file, sidecar)
}

pub(crate) fn io_failed(path: &Path) -> impl Fn(std::io::Error) -> Refusal + '_ {
    move |e| Refusal::new(Code::IoFailed, format!("{}: {}", path.display(), e.kind()))
}

/// Creates `path` (mode 0600) with `body`. An existing entry, also a link, is `E_CONFLICT` and
/// stays as it is; a failed write removes the new file.
pub(crate) fn write_private(path: &Path, body: &[u8]) -> Result<(), Refusal> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Refusal::new(
                    Code::Conflict,
                    format!("{} already exists; nothing was changed", path.display()),
                )
            } else {
                io_failed(path)(e)
            }
        })?;
    f.write_all(body).map_err(|e| {
        let _ = fs::remove_file(path);
        io_failed(path)(e)
    })
}

fn not_regular(path: &Path) -> Refusal {
    Refusal::new(
        Code::IoFailed,
        format!("{} is not a regular file", path.display()),
    )
}

fn too_large(url: &str) -> Refusal {
    Refusal::new(
        Code::LimitExceeded,
        format!(
            "{} is larger than the {MAX_SCRIPT}-byte script cap; nothing was changed",
            visible(url)
        ),
    )
}

/// The runner with `/usr/bin` first when it is on the search path, so the system's curl,
/// getconf and id run even when a provider directory such as Homebrew's comes earlier.
fn system_tools(runner: &Runner) -> Runner {
    let mut r = runner.clone();
    let usr_bin = Path::new(dfm_platform_macos::SYSTEM_PATH[0]);
    if let Some(i) = r.search_path.iter().position(|p| p == usr_bin) {
        let p = r.search_path.remove(i);
        r.search_path.insert(0, p);
    }
    r
}

/// Fetches `url` once with curl through the runner and caches the bytes (mode 0600) with a
/// sidecar. Only `https` is accepted, also after redirects.
pub(crate) fn fetch(runner: &Runner, cache_dir: &Path, url: &str) -> Result<Fetched, Refusal> {
    let (file, sidecar) = cache_paths(cache_dir, url);
    let dir = cache_dir.join("scripts");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(io_failed(&dir))?;
    let tmp = dir.join(format!(".fetch-{}", std::process::id()));
    // curl follows a link at its `-o` path, so a stale or planted entry goes first.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io_failed(&tmp)(e)),
        _ => {}
    }
    // `-q` first: `~/.curlrc` cannot add `insecure`, `proxy` or `cacert`.
    let args: Vec<String> = [
        "-q",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "-fsSL",
        "--max-filesize",
        "1048576",
        "-o",
    ]
    .iter()
    .map(|a| (*a).to_string())
    .chain([tmp.display().to_string()])
    .chain(["-w", "%{url_effective}", "--", url].map(String::from))
    .collect();
    let o = system_tools(runner).run(&Request {
        program: "curl",
        args: &args,
        env: &[],
        stdin: None,
        timeout: Duration::from_secs(120),
        max_stdout: 4096,
    });
    // Whatever curl left behind is not the approved script unless every check below passes.
    let drop_tmp = || {
        let _ = fs::remove_file(&tmp);
    };
    let o = o.inspect_err(|_| drop_tmp())?;
    let untrusted = |why: String| {
        drop_tmp();
        Refusal::new(
            Code::SourceUntrusted,
            format!("{}: {why}; nothing was changed", visible(url)),
        )
    };
    match o.end {
        End::Exited(0) => {}
        End::Stopped(r) => {
            drop_tmp();
            return Err(r);
        }
        // curl's exit for `--max-filesize`.
        End::Exited(63) => {
            drop_tmp();
            return Err(too_large(url));
        }
        end => {
            let how = match end {
                End::Exited(n) => format!("curl exited {n}"),
                End::TimedOut => "curl timed out after 120 seconds".into(),
                _ => "curl was ended by a signal".into(),
            };
            let err = String::from_utf8_lossy(&o.stderr);
            return Err(untrusted(format!(
                "the fetch failed ({how}): {}",
                bounded_visible(err.trim(), 512)
            )));
        }
    }
    let effective_url = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if !effective_url.starts_with("https://") {
        return Err(untrusted(format!(
            "the fetch ended at {}, not an https:// URL",
            visible(&effective_url)
        )));
    }
    let body = read_regular_bounded(&tmp, MAX_SCRIPT, io_failed(&tmp), || not_regular(&tmp))
        .inspect_err(|_| drop_tmp())?;
    let bytes = u64::try_from(body.len()).unwrap_or(u64::MAX);
    if bytes > MAX_SCRIPT {
        drop_tmp();
        return Err(too_large(url));
    }
    let sha256 = sha256_hex(&body);
    let private = fs::Permissions::from_mode(0o600);
    fs::set_permissions(&tmp, private.clone())
        .and_then(|()| fs::rename(&tmp, &file))
        .map_err(io_failed(&file))
        .inspect_err(|_| drop_tmp())?;
    let side =
        json!({"url": url, "effective_url": effective_url, "sha256": sha256, "bytes": bytes});
    fs::write(&sidecar, side.to_string())
        .and_then(|()| fs::set_permissions(&sidecar, private))
        .map_err(io_failed(&sidecar))?;
    Ok(Fetched {
        path: file,
        sha256,
        bytes,
        effective_url,
        body,
    })
}

/// The cached script without a fetch. `None` when the file or the sidecar is missing.
pub(crate) fn cached(cache_dir: &Path, url: &str) -> Result<Option<Fetched>, Refusal> {
    let (file, sidecar) = cache_paths(cache_dir, url);
    let missing =
        |p: &Path| fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
    if missing(&file) || missing(&sidecar) {
        return Ok(None);
    }
    let read = |p: &Path| read_regular_bounded(p, MAX_SCRIPT, io_failed(p), || not_regular(p));
    let (body, side) = (read(&file)?, read(&sidecar)?);
    let side: serde_json::Value = serde_json::from_slice(&side).unwrap_or_default();
    let sha256 = sha256_hex(&body);
    let changed = || {
        Refusal::new(
            Code::PlanStale,
            format!(
                "the cached script for {} changed since the preview; run the preview again",
                visible(url)
            ),
        )
    };
    if side["sha256"].as_str() != Some(sha256.as_str()) {
        return Err(changed());
    }
    let effective_url = side["effective_url"].as_str().ok_or_else(changed)?.into();
    Ok(Some(Fetched {
        path: file,
        sha256,
        bytes: u64::try_from(body.len()).unwrap_or(u64::MAX),
        effective_url,
        body,
    }))
}

/// The preview notes, from plain text matches. A hint is not a guarantee in either direction.
pub(super) fn hints(body: &[u8]) -> Vec<&'static str> {
    let has = |n: &[u8]| body.windows(n.len()).any(|w| w == n);
    let mut out = Vec::new();
    if has(b"/dev/tty") {
        out.push(HINT_TTY);
    }
    if [".zshrc", ".zshenv", ".bashrc", ".profile"]
        .iter()
        .any(|f| has(f.as_bytes()))
    {
        out.push(HINT_STARTUP);
    }
    out
}

/// Runs a short read-only probe through the runner and returns its trimmed output.
fn probe(runner: &Runner, program: &str, args: &[&str]) -> Result<String, Refusal> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let o = system_tools(runner).run(&Request {
        program,
        args: &args,
        env: &[],
        stdin: None,
        timeout: Duration::from_secs(10),
        max_stdout: 4096,
    })?;
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if !o.success() || v.is_empty() {
        return Err(Refusal::new(
            Code::ProbeFailed,
            format!("{program} {} gave no answer", args.join(" ")),
        ));
    }
    Ok(v)
}

/// The account's login shell. The isolated build takes it from `DFM_LOGIN_SHELL`, because the
/// test HOME has no directory-service record.
#[cfg(feature = "test-isolation")]
pub(crate) fn account_login_shell(user: &str) -> Option<String> {
    match std::env::var("DFM_LOGIN_SHELL") {
        Ok(v) if !v.is_empty() => Some(v),
        _ => dfm_platform_macos::login_shell(user),
    }
}

#[cfg(not(feature = "test-isolation"))]
pub(crate) fn account_login_shell(user: &str) -> Option<String> {
    dfm_platform_macos::login_shell(user)
}

/// The script's whole environment, sorted by name: a fixed list taken from `dfm_env` (dfm's
/// own environment) and the runner, so the script picks the same files as a `curl | sh` in the
/// owner's shell without seeing dfm's other variables. `tty` adds `TERM`; `extra` is `--env`.
pub(crate) fn script_env(
    runner: &Runner,
    dfm_env: &[(String, String)],
    tty: bool,
    extra: &[(String, String)],
    login_shell: impl FnOnce(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, Refusal> {
    let get = |n: &str| {
        dfm_env
            .iter()
            .find(|(k, v)| k == n && !v.is_empty())
            .map(|(_, v)| v.clone())
    };
    let user = match get("USER").or_else(|| get("LOGNAME")) {
        Some(u) => u,
        None => probe(runner, "id", &["-un"])?,
    };
    let tmp = match get("TMPDIR").filter(|t| t.starts_with('/')) {
        Some(t) => t,
        None => probe(runner, "getconf", &["DARWIN_USER_TEMP_DIR"])?,
    };
    // The sandbox matches real paths: `/var/folders/...` is `/private/var/folders/...`.
    let tmp = fs::canonicalize(&tmp).map_or(tmp, |p| p.display().to_string());
    let shell = login_shell(&user)
        .or_else(|| get("SHELL").filter(|s| s.starts_with('/')))
        .unwrap_or_else(|| "/bin/sh".into());
    let mut env = vec![
        ("HOME".to_string(), runner.home.display().to_string()),
        (
            "LOGNAME".into(),
            get("LOGNAME").unwrap_or_else(|| user.clone()),
        ),
        ("USER".into(), user),
        ("TMPDIR".into(), tmp),
        ("SHELL".into(), shell),
        ("PATH".into(), runner.joined_path()),
    ];
    let copied = [
        "LANG",
        "LC_ALL",
        "ZDOTDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_BIN_HOME",
    ];
    let term = if tty { &["TERM"][..] } else { &[] };
    for n in copied.iter().chain(term) {
        if let Some(v) = get(n) {
            env.push(((*n).to_string(), v));
        }
    }
    env.extend(extra.iter().cloned());
    if let Some((n, _)) = env.iter().find(|(_, v)| v.contains(['\0', '\n'])) {
        return Err(usage(format!(
            "the script's {n} holds a NUL byte or a newline"
        )));
    }
    env.sort();
    Ok(env)
}

/// Programs the script may not start, so it cannot ask for administrator rights (the design,
/// "The run and the sandbox"). This is not containment: launchd jobs and other services stay
/// `unknown` effects of the trial.
pub(super) const DENIED_EXEC: [&str; 3] = ["/usr/bin/sudo", "/usr/bin/su", "/usr/bin/osascript"];

/// What the script's sandbox lets it write: `home` and `temp` (canonical, from the caller), less
/// the denied trees and paths.
#[derive(Debug)]
pub(crate) struct Protected {
    pub home: PathBuf,
    pub temp: Vec<PathBuf>,
    /// Guarded checkouts, the state root, the config, data and cache dirs, dfm's install dir.
    pub denied_trees: Vec<PathBuf>,
    /// Ledger targets, the lock file, dfm's own executable path and each link on the way to a
    /// denied tree or path; a link is named itself, not its target.
    pub denied_paths: Vec<PathBuf>,
}

/// The path as an SBPL string body. `"` and `\` would end or escape the string, and a control
/// character cannot be shown in a refusal, so those paths are refused instead of escaped.
fn sbpl_str(p: &Path) -> Result<&str, Refusal> {
    p.to_str()
        .filter(|s| !s.contains(['"', '\\']) && !s.contains(char::is_control))
        .ok_or_else(|| {
            Refusal::new(
                Code::TargetInvalid,
                format!(
                    "the sandbox profile cannot hold the path {}",
                    visible(&p.to_string_lossy())
                ),
            )
        })
}

/// The SBPL text, one rule per line. Later rules win, so every specific deny follows the
/// home allow. A subpath deny does not stop a rename of an ancestor, so each ancestor strictly
/// below `home` of a denied tree or path gets a literal deny; a literal still allows new
/// entries inside it.
pub(crate) fn profile(p: &Protected) -> Result<String, Refusal> {
    let rules = |kind: &str, paths: &[&Path]| -> Result<String, Refusal> {
        paths
            .iter()
            .map(|d| Ok(format!(" ({kind} \"{}\")", sbpl_str(d)?)))
            .collect()
    };
    let mut allowed = vec![p.home.as_path()];
    allowed.extend(p.temp.iter().map(PathBuf::as_path));
    allowed.extend([Path::new("/private/tmp"), Path::new("/dev")]);
    let mut trees: Vec<&Path> = p.denied_trees.iter().map(PathBuf::as_path).collect();
    trees.sort_unstable();
    trees.dedup();
    let mut literals: Vec<&Path> = p.denied_paths.iter().map(PathBuf::as_path).collect();
    for d in p.denied_trees.iter().chain(&p.denied_paths) {
        literals.extend(
            d.ancestors()
                .skip(1)
                .filter(|a| a.starts_with(&p.home) && *a != p.home),
        );
    }
    literals.sort_unstable();
    literals.dedup();
    // No `file-link` rule: this profile already refuses a hard link from HOME to a denied file
    // (`ln` fails with EPERM on Darwin 27.2.0).
    let mut text =
        String::from("(version 1)\n(allow default)\n(deny file-write* (subpath \"/\"))\n");
    text += &format!("(allow file-write*{})\n", rules("subpath", &allowed)?);
    for t in trees {
        text += &format!("(deny file-write*{})\n", rules("subpath", &[t])?);
    }
    if !literals.is_empty() {
        text += &format!("(deny file-write*{})\n", rules("literal", &literals)?);
    }
    let exec: Vec<&Path> = DENIED_EXEC.iter().map(Path::new).collect();
    text += &format!("(deny process-exec{})\n", rules("literal", &exec)?);
    Ok(text)
}

/// `p` with its deepest existing ancestor canonicalized and the rest re-appended, because the
/// sandbox matches real paths and a denied path may not exist yet.
fn canonical(p: &Path) -> PathBuf {
    for a in p.ancestors() {
        if let Ok(mut real) = fs::canonicalize(a) {
            // Joining an empty rest adds a trailing `/`, which no sandbox rule matches.
            if let Ok(rest) = p.strip_prefix(a)
                && !rest.as_os_str().is_empty()
            {
                real.push(rest);
            }
            return real;
        }
    }
    p.to_path_buf()
}

/// `p` with its parent canonicalized and its last component kept, so a link at `p` is named
/// itself: a deny on its target would still let the script remove or replace the link.
fn canonical_nofollow(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) => canonical(dir).join(name),
        _ => canonical(p),
    }
}

/// Each link strictly below `home` on the way to `p`, `p` included, and on the way to each link
/// target in turn, as [`canonical_nofollow`] names it. Replacing any of them redirects `p`
/// without a write to `p` itself. The walk stops after the kernel's own link limit, so a loop
/// ends; a duplicate is left for `profile` to remove.
fn links_on(home: &Path, p: &Path) -> Vec<PathBuf> {
    // MAXSYMLINKS in <sys/param.h>: a longer chain cannot resolve, so it redirects nothing.
    const MAX_HOPS: usize = 32;
    let (mut out, mut todo, mut hops) = (Vec::new(), vec![p.to_path_buf()], 0);
    while let Some(q) = todo.pop() {
        for a in q.ancestors().filter(|a| a.starts_with(home) && *a != home) {
            let Ok(target) = fs::read_link(a) else {
                continue;
            };
            out.push(canonical_nofollow(a));
            if hops < MAX_HOPS {
                hops += 1;
                let dir = a.parent().unwrap_or(a);
                // `a` is an ancestor of `q`, so the prefix always strips.
                let rest = q.strip_prefix(a).unwrap_or(Path::new(""));
                let next = dir.join(target);
                todo.push(if rest.as_os_str().is_empty() {
                    next
                } else {
                    next.join(rest)
                });
            }
        }
    }
    out
}

/// The protected set for a run: the binding root's guarded checkouts, dfm's own directories,
/// install directory and executable, the lock file, and every ledger target.
pub(crate) fn protected(
    layout: &Layout,
    root: &Path,
    ledger: &Ledger,
    temp: &Path,
) -> Result<Protected, Refusal> {
    let exe = std::env::current_exe()
        .map_err(|e| Refusal::new(Code::IoFailed, format!("dfm's own path: {e}")))?;
    let checkouts = crate::sync_cmd::guarded_checkouts(root)?;
    Ok(protected_set(layout, checkouts, ledger, &exe, temp))
}

/// [`protected`] from its inputs. `exe` is the invoked path, a link on an installed machine.
/// A denied path is denied as itself and through its links: the no-follow form stops a rename,
/// removal or replacement of a link, the followed form stops a write through it when its
/// target is outside every denied tree. A tree is denied by its real path, and each link on
/// the way to a tree or path below HOME (the tree itself included) as a literal.
fn protected_set(
    layout: &Layout,
    checkouts: Vec<PathBuf>,
    ledger: &Ledger,
    exe: &Path,
    temp: &Path,
) -> Protected {
    let mut trees = checkouts;
    trees.extend([
        layout.state_root.clone(),
        layout.config_dir.clone(),
        layout.data_dir.clone(),
        layout.cache_dir.clone(),
    ]);
    trees.extend(canonical(exe).parent().map(Path::to_path_buf));
    let mut paths: Vec<PathBuf> = ledger
        .resources
        .iter()
        .flat_map(|r| r.owned_outputs.iter().map(|o| PathBuf::from(&o.path)))
        .collect();
    paths.extend([layout.lock_file.clone(), exe.to_path_buf()]);
    // `profile` sorts and deduplicates, so a path that is not a link may appear twice here.
    let mut denied_paths: Vec<PathBuf> = paths
        .iter()
        .flat_map(|p| [canonical_nofollow(p), canonical(p)])
        .collect();
    for p in trees.iter().chain(&paths) {
        denied_paths.extend(links_on(&layout.home, p));
    }
    Protected {
        home: canonical(&layout.home),
        temp: vec![canonical(temp)],
        denied_trees: trees.iter().map(|t| canonical(t)).collect(),
        denied_paths,
    }
}

/// The executables named `tool` in `path_var` and `provider_path(home)`, in search order: a
/// regular file with an execute bit, or a link to one. An unreadable entry counts as absent;
/// the sandbox, not this check, is what protects the machine. Relative and empty PATH entries
/// are skipped, as [`window::binary`] skips them.
pub(super) fn on_path(home: &Path, path_var: &OsStr, tool: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in std::env::split_paths(path_var).chain(dfm_platform_macos::provider_path(home)) {
        if d.is_absolute() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs.into_iter()
        .map(|d| d.join(tool))
        .filter(|p| {
            fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .collect()
}

/// What the script preview shows and the digest covers.
struct Decision {
    trial_id: String,
    tool: String,
    url: String,
    fetched: Fetched,
    shell: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    tty: bool,
    baseline: Vec<BaselinePath>,
    data: Vec<BaselinePath>,
    skipped: Vec<String>,
    binary: Binary,
    stamp: Stamp,
    /// SHA-256 of the sandbox profile text. The profile follows from the ledger, the layout and
    /// TMPDIR; a ledger change between the preview and the run makes the digest stale.
    profile_sha256: String,
}

impl Decision {
    /// Mirrors the package trial's digest, with the script in place of the provider.
    fn digest(&self) -> Result<String, Refusal> {
        let view = |ps: &[BaselinePath]| -> Vec<serde_json::Value> {
            ps.iter()
                .map(|p| json!({"path": p.path, "existed": p.existed, "sha256": p.sha256}))
                .collect()
        };
        let doc = json!({
            "trial_id": self.trial_id,
            "tool": self.tool,
            "script": {
                "url": self.url,
                "effective_url": self.fetched.effective_url,
                "sha256": self.fetched.sha256,
                "bytes": self.fetched.bytes,
                "shell": self.shell,
                "args": self.args,
                "env": self.env,
                "tty": self.tty,
            },
            "baseline": view(&self.baseline),
            "data": view(&self.data),
            "data_skipped": self.skipped,
            "unknown": UNKNOWN,
            "profile_sha256": self.profile_sha256,
            "binary": {"version": self.binary.version, "sha256": self.binary.sha256},
            "binding": {
                "profile": self.stamp.profile,
                "machine_id": self.stamp.machine_id,
                "binding_digest": self.stamp.binding_digest,
            },
        });
        // `serde_json::Value` keeps object keys sorted, so the bytes are canonical.
        serde_json::to_vec(&doc)
            .map(|b| sha256_hex(&b))
            .map_err(|e| Refusal::new(Code::IoFailed, format!("cannot digest the trial: {e}")))
    }

    /// The record as `create_trial` writes it before the run: no exit, window or binary yet.
    /// The tool is never pre-existing: `run` refuses a tool that is already on PATH.
    fn record(&self, created: u64, before: Vec<Observed>) -> TrialRecord {
        TrialRecord {
            schema: TRIAL_SCHEMA_SCRIPT,
            binding: self.stamp.clone(),
            trial_id: self.trial_id.clone(),
            tool: self.tool.clone(),
            created_unix_ns: created,
            baseline: Baseline {
                data: self.data.clone(),
                tool_preexisting: false,
                paths: self.baseline.clone(),
                owners: vec![],
            },
            owned_outputs: vec![],
            package: None,
            unknown: UNKNOWN.iter().map(|u| u.to_string()).collect(),
            adopted: None,
            script: Some(TrialScript {
                url: self.url.clone(),
                effective_url: self.fetched.effective_url.clone(),
                sha256: self.fetched.sha256.clone(),
                bytes: self.fetched.bytes,
                shell: self.shell.clone(),
                args: self.args.clone(),
                env: self.env.clone(),
                tty: self.tty,
                before,
                exit: None,
                window: None,
                binary: None,
            }),
        }
    }

    fn print(&self, out: &mut dyn Write, digest: &str) -> std::io::Result<()> {
        let f = &self.fetched;
        let file = visible(&f.path.display().to_string());
        writeln!(out, "trial: {} tool={}", self.trial_id, self.tool)?;
        writeln!(
            out,
            "script: {} -> {} sha256:{} bytes={}",
            visible(&self.url),
            visible(&f.effective_url),
            f.sha256,
            f.bytes
        )?;
        writeln!(out, "script file: {file}")?;
        let mut run = vec![format!("/bin/{}", self.shell), file];
        run.extend(self.args.iter().map(|a| visible(a)));
        writeln!(out, "run: sandbox-exec {} tty={}", run.join(" "), self.tty)?;
        let env: Vec<String> = self
            .env
            .iter()
            .map(|(n, v)| format!("{n}={}", visible(v)))
            .collect();
        writeln!(out, "env: {}", env.join(" "))?;
        for h in hints(&f.body) {
            writeln!(out, "{h}")?;
        }
        print_observed(out, &self.baseline, &self.data, &self.skipped)?;
        writeln!(
            out,
            "watch: shell startup files, bin directories and top-level listings are observed before and after the run; they are not in the digest"
        )?;
        writeln!(
            out,
            "cleanup scope: dfm purge of a script trial is not available yet (DFM-82); the window in the record lists what changed"
        )?;
        writeln!(
            out,
            "unknown: {} (candidates for review, never owned cleanup scope)",
            UNKNOWN.join(" ")
        )?;
        writeln!(out, "plan digest: {digest}")
    }
}

/// `sandbox-exec`, resolved with the system tools first, after a dry run of `profile` under it:
/// a profile the system does not accept refuses before the preview, not after the record.
pub(crate) fn sandbox_exec(runner: &Runner, profile: &str) -> Result<PathBuf, Refusal> {
    let unsupported = |why: String| Refusal::new(Code::PlatformUnsupported, why);
    let tools = system_tools(runner);
    let path = tools.resolve("sandbox-exec").map_err(|_| {
        unsupported("sandbox-exec is not available; a script trial needs it".into())
    })?;
    let args = ["-p", profile, "/usr/bin/true"].map(String::from);
    let o = tools.run(&Request {
        program: "sandbox-exec",
        args: &args,
        env: &[],
        stdin: None,
        timeout: Duration::from_secs(30),
        max_stdout: 4096,
    })?;
    match o.end {
        End::Exited(0) => Ok(path),
        End::Stopped(r) => Err(r),
        _ => Err(unsupported(format!(
            "the sandbox profile was not accepted: {}",
            bounded_visible(String::from_utf8_lossy(&o.stderr).trim(), 512)
        ))),
    }
}

/// One run of approved script bytes under the sandbox: `shell` runs `file` with `args` and
/// exactly `env`, in the fresh private directory `work`, with a terminal only when `tty`. The
/// trial (`dfm trial --script`) and the `script/v1` install of the packages scope share it.
pub(crate) struct Confined<'a> {
    pub sandbox: &'a Path,
    pub profile: String,
    pub shell: &'a str,
    pub file: &'a Path,
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub tty: bool,
    pub work: &'a Path,
}

impl Confined<'_> {
    /// Creates `work` (mode 0700; an existing entry is not dfm's and refuses the run), then runs
    /// the script there. `made` tells the caller to remove `work` afterwards.
    pub(crate) fn run(self, runner: &Runner, made: &mut bool) -> Result<End, Refusal> {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(self.work)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Refusal::new(
                        Code::Conflict,
                        format!(
                            "{} already exists; the script did not run",
                            self.work.display()
                        ),
                    )
                } else {
                    io_failed(self.work)(e)
                }
            })?;
        *made = true;
        let mut in_work = runner.clone();
        in_work.cwd = self.work.to_path_buf();
        let mut argv = vec![
            "-p".to_string(),
            self.profile,
            format!("/bin/{}", self.shell),
            self.file.display().to_string(),
        ];
        argv.extend(self.args.iter().cloned());
        in_work.run_shown(&Shown {
            program: self.sandbox,
            args: &argv,
            env: self.env,
            tty: self.tty,
            timeout: SCRIPT_TIMEOUT,
        })
    }
}

/// How the script ended, for the record, and the refusal a run that did not succeed ends with.
pub(crate) fn ended(end: Result<End, Refusal>) -> (Option<ScriptExit>, Option<Refusal>) {
    let signal = |s: &str| {
        Some(ScriptExit {
            code: None,
            signal: Some(s.into()),
        })
    };
    let failed = |why: String| Some(Refusal::new(Code::IoFailed, why));
    match end {
        Ok(End::Exited(code)) => (
            Some(ScriptExit {
                code: Some(code),
                signal: None,
            }),
            (code != 0)
                .then(|| format!("the script exited {code}"))
                .and_then(failed),
        ),
        // The stop request keeps its own code, so the exit stays 130 or 143.
        Ok(End::Stopped(r)) => (signal("stopped"), Some(r)),
        Ok(End::TimedOut) => (
            signal("timeout"),
            failed(format!(
                "the script ran longer than {} seconds; its process group was ended",
                SCRIPT_TIMEOUT.as_secs()
            )),
        ),
        Ok(End::Signaled) => (
            signal("signal"),
            failed("the script was ended by a signal".into()),
        ),
        Err(e) => (None, Some(e)),
    }
}

/// The problems of one run as one refusal: the most important code wins (a stop request, then
/// a tracked change, then the rest, such as a nonzero exit), and the text names every problem
/// in that order.
pub(crate) fn ranked(mut problems: Vec<Refusal>) -> Option<Refusal> {
    let rank = |c: Code| match c {
        Code::Interrupted | Code::Terminated => 0,
        Code::Conflict => 1,
        _ => 2,
    };
    problems.sort_by_key(|p| rank(p.code));
    let first = problems.first()?.code;
    let detail: Vec<&str> = problems.iter().map(|p| p.detail.as_str()).collect();
    Some(Refusal::new(first, detail.join("; ")))
}

/// The tracked changes between two `tracked_status` snapshots, as the upgrade step reports them.
pub(crate) fn tracked_changes(
    before: &std::collections::BTreeMap<String, (String, String)>,
    now: &std::collections::BTreeMap<String, (String, String)>,
) -> Vec<String> {
    let paths: BTreeSet<&String> = before.keys().chain(now.keys()).collect();
    paths
        .into_iter()
        .filter(|p| before.get(*p) != now.get(*p))
        .map(|p| {
            // A path that left the status was changed back to its committed form.
            let code = now.get(p).map_or("restored", |(xy, _)| xy.trim());
            format!(
                "the script changed the tracked file {} ({code}); it is left in place, not reverted, staged or committed",
                visible(p)
            )
        })
        .collect()
}

pub(super) fn run(
    ctx: &Ctx,
    args: &Args,
    s: &ScriptArgs,
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })?;
    // The tool name passed `valid_id` in `parse_args`.
    let tool = args.package.clone();
    let trial_id = args.id.clone().unwrap_or_else(|| format!("trial-{tool}"));
    if !valid_id(&trial_id) {
        return Err(usage(
            "--id must be lowercase ASCII letters, digits, '.', '_' or '-' (1 to 64 characters)",
        ));
    }
    let binary = crate::plan_cmd::binary_identity()?;
    let stamp = Stamp::of(&binding);
    // The sandbox dry run and the fetch run child processes, so a stop request ends them
    // instead of leaving them running.
    signal::install()?;
    let stop = || signal::requested().map_or(Ok(()), Err);
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    // A tool that is already on PATH would hide what the script installs; nothing is fetched.
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    // Checked here and again under the lock: another installer may add the tool in between.
    let preexisting = || {
        let found = on_path(&layout.home, &path_var, &tool);
        if found.is_empty() {
            return Ok(());
        }
        let shown: Vec<String> = found
            .iter()
            .map(|p| visible(&p.display().to_string()))
            .collect();
        Err(Refusal::new(
            Code::Conflict,
            format!(
                "an executable named {tool} is already on PATH: {}; remove or purge it first",
                shown.join(", ")
            ),
        ))
    };
    preexisting()?;
    // The checkouts the sandbox protects and the guard watches hang off the bound root.
    let (root, _) = crate::canonical_root(&binding, None)?;
    let runner = ctx.runner(&sys);
    let fail = state_cmd::fail_from_env()?;
    let readable =
        Store::open_read(sys, &layout, fail.clone())?.ok_or_else(crate::state_lost_refusal)?;
    readable.require_identity(&binding)?;
    let paths = candidates(&layout.home, &tool, &args.configs)?;
    for p in &paths {
        readable.check_claim(p)?;
    }
    let (mut data, mut skipped) = (Vec::new(), Vec::new());
    for d in data_candidates(&layout.home, &tool) {
        if paths.contains(&d) {
            continue;
        }
        match readable.check_claim(&d) {
            Ok(()) => data.push(d),
            Err(e) => skipped.push(format!("{d} not observed ({})", e.detail)),
        }
    }
    let missing = || {
        Refusal::new(
            Code::PlanStale,
            format!(
                "the script cache for {} is missing; run the preview again",
                visible(&s.url)
            ),
        )
    };
    let dfm_env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    // The script environment and the sandbox profile for `ledger`; neither needs the script.
    let confine = |ledger: &Ledger| -> Result<(Vec<(String, String)>, String), Refusal> {
        let env = script_env(&runner, &dfm_env, s.tty, &s.env, account_login_shell)?;
        let temp = env
            .iter()
            .find_map(|(n, v)| (n == "TMPDIR").then(|| PathBuf::from(v)))
            .unwrap_or_else(std::env::temp_dir);
        Ok((env, profile(&protected(&layout, &root, ledger, &temp)?)?))
    };
    // A system without a working sandbox refuses before anything is fetched or cached.
    let ledger = readable.read_ledger_lenient()?;
    let confined = confine(&ledger)?;
    let sandbox = sandbox_exec(&runner, &confined.1)?;
    stop()?;
    // An approval runs the bytes the preview cached; it never fetches again.
    let fetched = match &args.yes_plan {
        None => fetch(&runner, &layout.cache_dir, &s.url)?,
        Some(_) => cached(&layout.cache_dir, &s.url)?.ok_or_else(missing)?,
    };
    stop()?;
    // The decision and the sandbox profile for it; the digest covers the profile's hash.
    let decide = |fetched: Fetched,
                  (env, profile): (Vec<(String, String)>, String)|
     -> Result<(Decision, String), Refusal> {
        let baseline = paths.iter().map(|p| observe(p)).collect::<Result<_, _>>()?;
        let (mut seen, mut skipped) = (Vec::new(), skipped.clone());
        for p in &data {
            match observe(p) {
                Ok(b) => seen.push(b),
                Err(e) => skipped.push(format!("{p} not observed ({})", e.detail)),
            }
        }
        let decision = Decision {
            trial_id: trial_id.clone(),
            tool: tool.clone(),
            url: s.url.clone(),
            fetched,
            shell: s.shell.clone(),
            args: s.args.clone(),
            env,
            tty: s.tty,
            baseline,
            data: seen,
            skipped,
            binary: binary.clone(),
            stamp: stamp.clone(),
            profile_sha256: sha256_hex(profile.as_bytes()),
        };
        Ok((decision, profile))
    };
    let (first, _) = decide(fetched, confined)?;
    let digest = first.digest()?;
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    first.print(out, &digest).map_err(w)?;
    let approved = match &args.yes_plan {
        Some(approved) if *approved != digest => return Err(stale(approved, &digest)),
        Some(approved) => approved,
        None if args.preview => return Ok(()),
        None => {
            return Err(Refusal::new(
                Code::ApprovalRequired,
                format!(
                    "nothing was changed. Fix: read the trial above, then run again with --yes-plan {digest}"
                ),
            ));
        }
    };
    if s.tty && !std::io::stdin().is_terminal() {
        return Err(usage("--tty needs a terminal on standard input"));
    }
    let conflict = || {
        Refusal::new(
            Code::Conflict,
            format!(
                "trial {trial_id} already exists in state/trials/; nothing was changed. Fix: pass another --id"
            ),
        )
    };
    let file = format!("{trial_id}.json");
    if readable.load().trials.iter().any(|(n, _)| *n == file) {
        return Err(conflict());
    }
    drop(readable);
    fail.hit("trial-before-lock")?;

    // Take the lock, then decide again from the cached bytes: the ledger, the baseline or the
    // cache may have changed since the preview.
    let store = Store::open_write_with(sys, &layout, fail.clone(), false)?;
    if !store.is_initialized() {
        return Err(store.stamp(crate::state_lost_refusal()));
    }
    let locked = (|| {
        store.require_identity(&binding)?;
        preexisting()?;
        let ledger = store.read_ledger()?;
        let fetched = cached(&layout.cache_dir, &s.url)?.ok_or_else(missing)?;
        let (locked, profile) = decide(fetched, confine(&ledger)?)?;
        stop()?;
        let locked_digest = locked.digest()?;
        if locked_digest != digest {
            return Err(stale(approved, &locked_digest));
        }
        Ok((locked, profile))
    })();
    let (locked, profile_text) = locked.map_err(|r| store.stamp(r))?;
    let started = now_ns().map_err(|r| store.stamp(r))?;
    let op_id = format!("trial-{started}-{}", std::process::id());
    // The guard snapshot and the window baseline come before the record, so the record never
    // claims a window that was not observed.
    let git = Git::new(ctx, &sys);
    let watch: Vec<Watch> = window::watch_set(&layout.home, &locked.env);
    let (checkouts, tracked, before) = (|| {
        let checkouts = guarded_checkouts(&root)?;
        let tracked = tracked_status(&git, &checkouts)?;
        Ok((checkouts, tracked, window::observe(&watch)?))
    })()
    .map_err(|r: Refusal| store.stamp(r))?;
    let mut record = locked.record(started, before);
    // Half the record budget for `before` leaves room for any window `shrink_window` keeps.
    window::check_before_run(&record).map_err(|r| store.stamp(r))?;
    // The run executes a copy of the verified bytes, not the URL-keyed cache file a concurrent
    // preview (no lock) may replace. It is a manager-private temporary file, the only write
    // before the record, and it is removed on every exit path from here on.
    let run_file = layout
        .cache_dir
        .join("scripts")
        .join(format!("run-{trial_id}-{}", std::process::id()));
    write_private(&run_file, &locked.fetched.body).map_err(|r| store.stamp(r))?;
    let drop_run_file = || {
        let _ = fs::remove_file(&run_file);
    };
    // The exclusive create makes a record another run wrote since the check above a conflict.
    match store.create_trial(&record) {
        Ok(true) => {}
        Ok(false) => {
            drop_run_file();
            return Err(store.stamp(conflict()));
        }
        Err(e) => {
            drop_run_file();
            return Err(e);
        }
    }
    fail.hit("trial-script-after-record")
        .inspect_err(|_| drop_run_file())
        .map_err(|r| store.stamp(r))?;

    // From here on the record exists and every outcome ends in a receipt and a result line; a
    // refusal is after a mutation, so it exits 1 (or the stop request's own exit).
    let journal = |phase, recovery, reason| -> Result<(), Refusal> {
        store.append_journal(&JournalEntry {
            schema: SCHEMA,
            op_id: op_id.clone(),
            op: "trial-script".into(),
            phase,
            at: rfc3339(now_ns()?),
            resource: trial_id.clone(),
            adapter: ADAPTER.into(),
            class: Class::Normal,
            recovery,
            token: None,
            reason,
        })
    };
    // The intent has no path: the window in the record is what the run changed.
    let empty = Recovery {
        outputs: vec![],
        snapshot: None,
    };
    // The script's working directory: a fresh private directory under its TMPDIR, which the
    // sandbox lets it write (the runner's own directory is in the denied cache dir). It is
    // removed after the run; an entry that already exists is not dfm's and is left alone.
    let work = locked
        .env
        .iter()
        .find_map(|(n, v)| (n == "TMPDIR").then(|| PathBuf::from(v)))
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("dfm-trial-{trial_id}-{}", std::process::id()));
    let mut work_made = false;
    let end = journal(Phase::Intent, Some(empty), None).and_then(|()| {
        Confined {
            sandbox: &sandbox,
            profile: profile_text,
            shell: &locked.shell,
            file: &run_file,
            args: &locked.args,
            env: &locked.env,
            tty: locked.tty,
            work: &work,
        }
        .run(&runner, &mut work_made)
    });
    let (exit, failed) = ended(end);
    let mut problems: Vec<Refusal> = failed.into_iter().collect();
    let found = window::binary(&layout.home, &path_var, &tool);
    if let Some(sc) = record.script.as_mut() {
        sc.window = match window::observe(&watch) {
            Ok(after) => Some(window::diff(&sc.before, &after)),
            Err(e) => {
                problems.push(e);
                None
            }
        };
        sc.exit = exit;
        sc.binary = found;
    }
    match tracked_status(&git, &checkouts) {
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
    // The exit and the binary always fit; the window shrinks to what the write cap allows.
    let shrunk = match window::shrink_window(&mut record, window::MAX_RECORD_BYTES) {
        Ok(s) => s,
        Err(e) => {
            problems.push(e);
            window::Shrunk::default()
        }
    };
    let written = match store.write_trial(&record) {
        Ok(()) => true,
        Err(e) => {
            problems.push(e);
            false
        }
    };
    // The record keeps the script's sha256; a later preview fetches it again.
    let (cache_file, sidecar) = cache_paths(&layout.cache_dir, &s.url);
    for p in [cache_file, sidecar, run_file] {
        if let Err(e) = fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            problems.push(io_failed(&p)(e));
        }
    }
    if work_made
        && let Err(e) = fs::remove_dir_all(&work)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        problems.push(io_failed(&work)(e));
    }
    if problems.is_empty()
        && let Err(n) = journal(Phase::Done, None, None)
    {
        problems.push(Refusal::new(
            n.code,
            format!(
                "the script ran, but the outcome could not be journaled: {}",
                n.detail
            ),
        ));
    }
    let stopped = ranked(problems).map(|mut e| {
        if let Err(n) = journal(Phase::Failed, None, Some(bounded_visible(&e.detail, 1024))) {
            e = Refusal::new(
                e.code,
                format!(
                    "{}; the outcome could not be journaled: {}",
                    e.detail, n.detail
                ),
            );
        }
        e.after_mutation()
    });
    let has_binary = record.script.as_ref().is_some_and(|sc| sc.binary.is_some());
    let notes: Vec<String> = [
        (!has_binary).then(|| format!("the tool {tool} is not on PATH or cannot be hashed")),
        (written && shrunk.any()).then(|| {
            format!(
                "the window was shrunk to fit the record: {} entries dropped{}; {} recorded",
                shrunk.dropped_entries,
                if shrunk.dropped_before {
                    ", before-states removed"
                } else {
                    ""
                },
                if has_binary {
                    "the exit and the binary are"
                } else {
                    "the exit is"
                }
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let note = (!notes.is_empty()).then(|| format!("note: {}", notes.join("; ")));
    conclude(
        ctx,
        &store,
        Outcome {
            trial_id: &trial_id,
            op_id: &op_id,
            adapter: ADAPTER,
            status: if stopped.is_none() {
                Status::Updated
            } else {
                Status::Failed
            },
            started,
            stopped,
            binary,
            stamp,
            digest,
            note,
        },
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfm_model::state::SCRIPT_UNKNOWN;

    fn runner() -> Runner {
        Runner {
            search_path: vec!["/x/bin".into(), "/y/bin".into()],
            home: "/h".into(),
            cwd: "/nonexistent-run".into(),
            uid: 0,
            grace: Duration::from_millis(1),
            detach: false,
        }
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn a_stop_request_then_a_tracked_change_outranks_a_failed_script() {
        let r = |c: Code, d: &str| Refusal::new(c, d);
        let exited = r(Code::IoFailed, "the script exited 4");
        let tracked = r(
            Code::Conflict,
            "the script changed the tracked file /r/f ( M)",
        );
        let got = ranked(vec![exited.clone(), tracked.clone()]).unwrap();
        assert_eq!(got.code, Code::Conflict);
        assert_eq!(got.detail, format!("{}; {}", tracked.detail, exited.detail));
        let stop = r(Code::Interrupted, "SIGINT received");
        let got = ranked(vec![exited, tracked, stop]).unwrap();
        assert_eq!(got.code, Code::Interrupted);
        assert!(
            got.detail
                .starts_with("SIGINT received; the script changed")
        );
        assert!(ranked(vec![]).is_none());
    }

    #[test]
    fn unknown_is_the_trial_list_then_the_script_list() {
        assert_eq!(UNKNOWN[..3], super::super::UNKNOWN);
        assert_eq!(UNKNOWN[3..], SCRIPT_UNKNOWN);
    }

    #[test]
    fn system_tools_puts_usr_bin_first_and_keeps_the_rest() {
        let mut r = runner();
        r.search_path = vec!["/opt/homebrew/bin".into(), "/usr/bin".into(), "/bin".into()];
        let order = |r: &Runner| r.joined_path();
        assert_eq!(order(&system_tools(&r)), "/usr/bin:/opt/homebrew/bin:/bin");
        // The test provider path has no /usr/bin, so planted fakes stay first.
        assert_eq!(order(&system_tools(&runner())), "/x/bin:/y/bin");
    }

    #[test]
    fn hints_match_plain_text() {
        assert_eq!(hints(b"x\n cat < /dev/tty"), [HINT_TTY]);
        assert_eq!(hints(b"echo >> ~/.zshrc"), [HINT_STARTUP]);
        assert_eq!(
            hints(b"read x </dev/tty; echo >> ~/.profile"),
            [HINT_TTY, HINT_STARTUP]
        );
        assert!(hints(b"nothing").is_empty());
    }

    #[test]
    fn cache_paths_are_keyed_by_the_url_hash() {
        let (f, s) = cache_paths(Path::new("/c"), "https://a/b");
        assert_eq!(f, Path::new("/c/scripts").join(sha256_hex(b"https://a/b")));
        assert_eq!(s.extension().unwrap(), "json");
        assert_eq!(s.with_extension(""), f);
    }

    #[test]
    fn script_env_is_the_fixed_list_sorted_and_tty_adds_term() {
        let dfm = pairs(&[
            ("USER", "me"),
            ("TMPDIR", "/nonexistent-tmp/"),
            ("SHELL", "/bin/zsh"),
            ("ZDOTDIR", "/h/.config/zsh"),
            ("TERM", "xterm-256color"),
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", ""),
            ("SECRET_TOKEN", "s3cret"),
            ("PATH", "/owner/bin"),
            ("HOME", "/elsewhere"),
        ]);
        let extra = pairs(&[("FOO", "1")]);
        let env = script_env(&runner(), &dfm, false, &extra, |_| None).unwrap();
        assert_eq!(
            env,
            pairs(&[
                ("FOO", "1"),
                ("HOME", "/h"),
                ("LANG", "en_US.UTF-8"),
                ("LOGNAME", "me"),
                ("PATH", "/x/bin:/y/bin"),
                ("SHELL", "/bin/zsh"),
                ("TMPDIR", "/nonexistent-tmp/"),
                ("USER", "me"),
                ("ZDOTDIR", "/h/.config/zsh"),
            ])
        );
        let with_tty = script_env(&runner(), &dfm, true, &extra, |_| None).unwrap();
        let names: Vec<&str> = with_tty.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.is_sorted() && names.contains(&"TERM"), "{names:?}");
        assert_eq!(with_tty.len(), env.len() + 1);

        // LOGNAME stands in for USER; a relative SHELL is replaced.
        let dfm = pairs(&[("LOGNAME", "ln"), ("TMPDIR", "/t"), ("SHELL", "zsh")]);
        let env = script_env(&runner(), &dfm, false, &[], |_| None).unwrap();
        assert!(env.contains(&("USER".into(), "ln".into())), "{env:?}");
        assert!(env.contains(&("SHELL".into(), "/bin/sh".into())), "{env:?}");

        let bad = pairs(&[("USER", "me"), ("TMPDIR", "/t"), ("LANG", "a\nb")]);
        assert_eq!(
            script_env(&runner(), &bad, false, &[], |_| None)
                .unwrap_err()
                .code,
            Code::Usage
        );
        let bad_extra = pairs(&[("FOO", "a\nb")]);
        assert_eq!(
            script_env(&runner(), &dfm, false, &bad_extra, |_| None)
                .unwrap_err()
                .code,
            Code::Usage
        );
    }

    #[test]
    fn shell_is_the_login_shell_then_an_absolute_dfm_shell_then_bin_sh() {
        let shell = |dfm_shell: Option<&str>, lookup: Option<&str>| {
            let mut dfm = pairs(&[("USER", "me"), ("TMPDIR", "/t")]);
            if let Some(s) = dfm_shell {
                dfm.push(("SHELL".into(), s.into()));
            }
            let env = script_env(&runner(), &dfm, false, &[], |u| {
                assert_eq!(u, "me");
                lookup.map(String::from)
            })
            .unwrap();
            env.into_iter().find(|(n, _)| n == "SHELL").unwrap().1
        };
        assert_eq!(shell(Some("/bin/bash"), Some("/bin/zsh")), "/bin/zsh");
        assert_eq!(shell(Some("/bin/bash"), None), "/bin/bash");
        assert_eq!(shell(None, Some("/bin/zsh")), "/bin/zsh");
        assert_eq!(shell(None, None), "/bin/sh");
        assert_eq!(shell(Some("bash"), None), "/bin/sh");
    }

    #[test]
    fn the_digest_covers_the_script_bytes_the_environment_tty_and_the_profile() {
        let d = |sha: &str, env: &[(&str, &str)], tty: bool, profile: &str| {
            Decision {
                trial_id: "trial-boat".into(),
                tool: "boat".into(),
                url: "https://boat.dev/install".into(),
                fetched: Fetched {
                    path: "/c/scripts/x".into(),
                    sha256: sha.into(),
                    bytes: 1,
                    effective_url: "https://boat.dev/install".into(),
                    body: vec![],
                },
                shell: "sh".into(),
                args: vec![],
                env: pairs(env),
                tty,
                baseline: vec![],
                data: vec![],
                skipped: vec![],
                binary: Binary {
                    version: "0".into(),
                    sha256: "b".into(),
                },
                stamp: Stamp {
                    profile: "p".into(),
                    machine_id: "m".into(),
                    binding_digest: "d".into(),
                },
                profile_sha256: profile.into(),
            }
            .digest()
            .unwrap()
        };
        let home = [("HOME", "/h")];
        let base = d("s1", &home, false, "p1");
        assert_eq!(base, d("s1", &home, false, "p1"));
        assert_ne!(base, d("s2", &home, false, "p1"));
        assert_ne!(base, d("s1", &[("HOME", "/g")], false, "p1"));
        assert_ne!(base, d("s1", &home, true, "p1"));
        assert_ne!(base, d("s1", &home, false, "p2"));
    }

    #[test]
    fn the_profile_denies_trees_paths_ancestors_and_exec() {
        let p = Protected {
            home: "/h".into(),
            temp: vec!["/private/var/folders/x/T".into()],
            denied_trees: vec![
                "/h/.dotfiles".into(),
                "/h/.local/state/dotfiles-manager".into(),
            ],
            denied_paths: vec![
                "/h/.config/zsh/.zshrc".into(),
                "/h/.local/state/dotfiles-manager.lock".into(),
            ],
        };
        let text = profile(&p).unwrap();
        for want in [
            "(allow default)",
            "(deny file-write* (subpath \"/\"))",
            "(subpath \"/h\")",
            "(subpath \"/h/.dotfiles\")",
            "(literal \"/h/.config/zsh/.zshrc\")",
            "(literal \"/h/.config/zsh\")",
            "(literal \"/h/.config\")",
            "(literal \"/h/.local\")",
            "(literal \"/h/.local/state\")",
            "(literal \"/h/.local/state/dotfiles-manager.lock\")",
            "(literal \"/usr/bin/sudo\")",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        assert!(
            !text.contains("(literal \"/h\")"),
            "HOME itself is not denied"
        );
        let allow_at = text.find("(allow file-write*").unwrap();
        assert!(
            text.rfind("(deny file-write*").unwrap() > allow_at,
            "denies come after the allow"
        );
    }

    #[test]
    fn the_profile_lines_are_in_rule_order() {
        let p = Protected {
            home: "/h".into(),
            temp: vec!["/t".into()],
            denied_trees: vec!["/h/b".into(), "/h/a".into(), "/h/a".into()],
            denied_paths: vec!["/h/c/f".into(), "/outside/g".into()],
        };
        let text = profile(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "(version 1)",
                "(allow default)",
                "(deny file-write* (subpath \"/\"))",
                "(allow file-write* (subpath \"/h\") (subpath \"/t\") (subpath \"/private/tmp\") (subpath \"/dev\"))",
                "(deny file-write* (subpath \"/h/a\"))",
                "(deny file-write* (subpath \"/h/b\"))",
                "(deny file-write* (literal \"/h/c\") (literal \"/h/c/f\") (literal \"/outside/g\"))",
                "(deny process-exec (literal \"/usr/bin/sudo\") (literal \"/usr/bin/su\") (literal \"/usr/bin/osascript\"))",
            ]
        );
    }

    #[test]
    fn a_path_the_profile_cannot_hold_refuses() {
        for bad in ["/h/a\"b", "/h/a\\b", "/h/a\nb", "/h/a\u{7f}b"] {
            let p = Protected {
                home: "/h".into(),
                temp: vec![],
                denied_trees: vec![bad.into()],
                denied_paths: vec![],
            };
            assert_eq!(
                profile(&p).unwrap_err().code,
                Code::TargetInvalid,
                "{bad:?}"
            );
        }
        let p = Protected {
            home: "/h \u{e9}".into(),
            temp: vec![],
            denied_trees: vec![],
            denied_paths: vec!["/h \u{e9}/x y".into()],
        };
        assert!(profile(&p).unwrap().contains("(literal \"/h \u{e9}/x y\")"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dfm-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn canonical_follows_a_link_and_canonical_nofollow_keeps_it() {
        let dir = scratch("canonical");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(
            canonical(&link.join("missing/leaf")),
            dir.join("missing/leaf")
        );
        // `Path` equality ignores a trailing `/`; the profile text does not.
        assert_eq!(canonical(&link).as_os_str(), dir.as_os_str());
        // A deny on the target alone would let the script remove or replace the link.
        assert_eq!(canonical_nofollow(&link), link);
        assert_eq!(canonical_nofollow(&link.join("leaf")), dir.join("leaf"));
        assert_eq!(
            links_on(&dir, &link.join("leaf")),
            [canonical_nofollow(&link)]
        );
        assert!(links_on(&dir, &dir.join("leaf")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn links_on_follows_link_chains_and_stops_on_a_loop() {
        let h = scratch("chains");
        let link = |to: &str, at: &str| std::os::unix::fs::symlink(h.join(to), h.join(at)).unwrap();
        for d in ["a", "c", "n"] {
            fs::create_dir(h.join(d)).unwrap();
        }
        fs::write(h.join("c/zshrc"), b"z").unwrap();
        fs::write(h.join("n/vimrc"), b"v").unwrap();
        // Two hops: repointing the middle link redirects the first without writing to it.
        link("a/zshrc", ".zshrc");
        link("c/zshrc", "a/zshrc");
        // A link in the middle of the target's path.
        link("m/vimrc", ".vimrc");
        link("n", "m");
        link("y", "x");
        link("x", "y");
        let mut got = links_on(&h, &h.join(".zshrc"));
        got.sort();
        assert_eq!(got, [h.join(".zshrc"), h.join("a/zshrc")]);
        let mut got = links_on(&h, &h.join(".vimrc"));
        got.sort();
        assert_eq!(got, [h.join(".vimrc"), h.join("m")]);
        let mut got = links_on(&h, &h.join("x/f"));
        got.sort();
        got.dedup();
        assert_eq!(got, [h.join("x"), h.join("y")]);
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn linked_targets_the_dfm_link_and_a_linked_tree_are_denied_in_both_forms() {
        use dfm_model::state::{Class, LedgerResource, OwnedOutput};
        let h = scratch("protected");
        let mkdir = |d: &str| fs::create_dir_all(h.join(d)).unwrap();
        let link = |to: &str, at: &str| std::os::unix::fs::symlink(h.join(to), h.join(at)).unwrap();
        for d in [
            "src/dotfiles",
            "store/zsh",
            ".config/zsh",
            ".local/bin",
            ".local/share/dfm/bin",
        ] {
            mkdir(d);
        }
        fs::write(h.join("store/zsh/zshrc"), b"z").unwrap();
        fs::write(h.join(".local/share/dfm/bin/dfm"), b"x").unwrap();
        link("src/dotfiles", ".dotfiles");
        link("store/zsh/zshrc", ".config/zsh/.zshrc");
        link(".local/share/dfm/bin/dfm", ".local/bin/dfm");
        let layout = Layout {
            home: h.clone(),
            config_dir: h.join(".config/dfm"),
            state_root: h.join(".local/state/dfm"),
            lock_file: h.join(".local/state/dfm.lock"),
            data_dir: h.join(".local/share/dfm"),
            cache_dir: h.join(".cache/dfm"),
        };
        let mut ledger = Ledger::empty(Stamp {
            profile: "p".into(),
            machine_id: "m".into(),
            binding_digest: "d".into(),
        });
        ledger.resources.push(LedgerResource {
            resource: "zsh".into(),
            adapter: "link".into(),
            class: Class::Normal,
            op_id: "op".into(),
            owned_outputs: vec![OwnedOutput {
                path: h.join(".config/zsh/.zshrc").display().to_string(),
                mode: 0o644,
                fingerprint: None,
                coowned: None,
            }],
        });
        let p = protected_set(
            &layout,
            vec![h.join(".dotfiles")],
            &ledger,
            &h.join(".local/bin/dfm"),
            &h.join("tmp"),
        );
        let text = profile(&p).unwrap();
        let lit = |rel: &str| format!("(literal \"{}\")", h.join(rel).display());
        let sub = |rel: &str| format!("(subpath \"{}\")", h.join(rel).display());
        let want = [
            // The ledger link and the file it points to, with the ancestors of both.
            lit(".config/zsh/.zshrc"),
            lit(".config/zsh"),
            lit(".config"),
            lit("store/zsh/zshrc"),
            lit("store/zsh"),
            lit("store"),
            // The dfm command link and dfm itself; its real directory is the install.
            lit(".local/bin/dfm"),
            lit(".local/bin"),
            lit(".local"),
            lit(".local/share/dfm/bin/dfm"),
            sub(".local/share/dfm/bin"),
            // A linked checkout: the tree by its real path, the link as a literal.
            sub("src/dotfiles"),
            lit(".dotfiles"),
            lit("src"),
        ];
        for w in &want {
            assert!(text.contains(w.as_str()), "{w}\n{text}");
        }
        assert!(!text.contains(&sub(".dotfiles")), "{text}");
        fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn write_private_creates_a_0600_file_and_refuses_an_existing_entry() {
        let dir = scratch("write-private");
        let p = dir.join("run");
        write_private(&p, b"approved").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"approved");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A second writer, or a planted link, is a conflict and the entry stays as it was.
        let e = write_private(&p, b"other").unwrap_err();
        assert_eq!(e.code, Code::Conflict, "{}", e.detail);
        assert_eq!(fs::read(&p).unwrap(), b"approved");
        let link = dir.join("link");
        std::os::unix::fs::symlink(dir.join("target"), &link).unwrap();
        let e = write_private(&link, b"other").unwrap_err();
        assert_eq!(e.code, Code::Conflict, "{}", e.detail);
        assert!(!dir.join("target").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn on_path_finds_executables_and_ignores_the_rest() {
        let dir = scratch("on-path");
        let mode =
            |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        for (d, m) in [("bin", 0o755), ("bin2", 0o644)] {
            fs::create_dir(dir.join(d)).unwrap();
            fs::write(dir.join(d).join("t"), b"#!/bin/sh\n").unwrap();
            mode(&dir.join(d).join("t"), m);
        }
        fs::create_dir(dir.join("bin3")).unwrap();
        std::os::unix::fs::symlink(dir.join("bin/t"), dir.join("bin3/t")).unwrap();
        fs::create_dir(dir.join("bin4")).unwrap();
        std::os::unix::fs::symlink(dir.join("bin2/t"), dir.join("bin4/t")).unwrap();
        let path = std::env::join_paths(
            ["bin", "bin2", "bin3", "bin4", "bin", "absent"].map(|d| dir.join(d)),
        )
        .unwrap();
        let home = dir.join("home");
        assert_eq!(
            on_path(&home, &path, "t"),
            [dir.join("bin/t"), dir.join("bin3/t")]
        );
        // A relative entry resolves against the working directory and is skipped, also when
        // it reaches the executable `bin/t`; so is an empty entry.
        let cwd = std::env::current_dir().unwrap();
        let up: PathBuf = cwd.components().skip(1).map(|_| "..").collect();
        let rel_bin = up.join(dir.join("bin").strip_prefix("/").unwrap());
        assert!(fs::metadata(rel_bin.join("t")).is_ok());
        let rel = std::env::join_paths([PathBuf::new(), rel_bin]).unwrap();
        assert!(on_path(&home, &rel, "t").is_empty());
        assert!(on_path(&home, &path, "dfm-no-such-tool").is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cached_script_must_match_its_sidecar() {
        let dir = std::env::temp_dir().join(format!("dfm-script-cache-{}", std::process::id()));
        let url = "https://a/b";
        let (file, side) = cache_paths(&dir, url);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        assert!(cached(&dir, url).unwrap().is_none());
        fs::write(&file, b"echo hi\n").unwrap();
        assert!(cached(&dir, url).unwrap().is_none());
        let sidecar = |sha: &str| {
            json!({"url": url, "effective_url": url, "sha256": sha, "bytes": 8}).to_string()
        };
        fs::write(&side, sidecar(&sha256_hex(b"echo hi\n"))).unwrap();
        let got = cached(&dir, url).unwrap().unwrap();
        assert_eq!((got.bytes, got.body.as_slice()), (8, &b"echo hi\n"[..]));
        fs::write(&file, b"echo HI\n").unwrap();
        assert_eq!(cached(&dir, url).unwrap_err().code, Code::PlanStale);
        fs::remove_dir_all(&dir).unwrap();
    }
}
