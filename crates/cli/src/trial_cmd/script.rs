//! The `--script` trial: fetch an install script over HTTPS and run it in a recorded window.

use super::{Args, ScriptArgs, candidates, data_candidates, observe, print_observed};
use crate::apply_cmd::stale;
use crate::read_fs::read_regular_bounded;
use crate::{Ctx, read_existing_binding, state_cmd, usage};
use dfm_engine::proc::{End, Request};
use dfm_engine::{Layout, Runner, Store, signal};
use dfm_model::digest::sha256_hex;
use dfm_model::display::{bounded_visible, visible};
use dfm_model::plan::Binary;
use dfm_model::state::{BaselinePath, Ledger, Stamp, valid_id};
use dfm_model::{Code, Refusal};
use serde_json::json;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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

/// The largest script dfm fetches, as curl's `--max-filesize` enforces it.
const MAX_SCRIPT: u64 = 1_048_576;

/// A script in the cache, as the preview showed it and the run uses it.
#[derive(Debug)]
pub(super) struct Fetched {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub effective_url: String,
    pub body: Vec<u8>,
}

/// `<cache_dir>/scripts/<sha256(url)>` and its sidecar `<...>.json`
/// `{"url","effective_url","sha256","bytes"}`.
pub(super) fn cache_paths(cache_dir: &Path, url: &str) -> (PathBuf, PathBuf) {
    let file = cache_dir.join("scripts").join(sha256_hex(url.as_bytes()));
    let sidecar = file.with_extension("json");
    (file, sidecar)
}

fn io_failed(path: &Path) -> impl Fn(std::io::Error) -> Refusal + '_ {
    move |e| Refusal::new(Code::IoFailed, format!("{}: {}", path.display(), e.kind()))
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
pub(super) fn fetch(runner: &Runner, cache_dir: &Path, url: &str) -> Result<Fetched, Refusal> {
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
pub(super) fn cached(cache_dir: &Path, url: &str) -> Result<Option<Fetched>, Refusal> {
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

/// The script's whole environment, sorted by name: a fixed list taken from `dfm_env` (dfm's
/// own environment) and the runner, so the script picks the same files as a `curl | sh` in the
/// owner's shell without seeing dfm's other variables. `tty` adds `TERM`; `extra` is `--env`.
pub(super) fn script_env(
    runner: &Runner,
    dfm_env: &[(String, String)],
    tty: bool,
    extra: &[(String, String)],
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
    // shortcut: SHELL falls back to /bin/sh; the login shell from the directory service is not
    // read. Upgrade when a script picks its startup file from SHELL and dfm runs without one.
    let shell = get("SHELL")
        .filter(|s| s.starts_with('/'))
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

/// Programs the script may not start, so it cannot leave the sandbox through another user or
/// through Apple Events to an unsandboxed app.
#[cfg_attr(not(test), expect(dead_code, reason = "wired by the script run"))]
pub(super) const DENIED_EXEC: [&str; 3] = ["/usr/bin/sudo", "/usr/bin/su", "/usr/bin/osascript"];

/// What the script's sandbox lets it write: `home` and `temp` (canonical, from the caller), less
/// the denied trees and paths.
#[derive(Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "wired by the script run"))]
pub(super) struct Protected {
    pub home: PathBuf,
    pub temp: Vec<PathBuf>,
    /// Guarded checkouts, the state root, the config, data and cache dirs, dfm's install dir.
    pub denied_trees: Vec<PathBuf>,
    /// Ledger targets, the lock file and dfm's own executable path.
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
#[cfg_attr(not(test), expect(dead_code, reason = "wired by the script run"))]
pub(super) fn profile(p: &Protected) -> Result<String, Refusal> {
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
#[cfg_attr(not(test), expect(dead_code, reason = "wired by the script run"))]
fn canonical(p: &Path) -> PathBuf {
    for a in p.ancestors() {
        if let Ok(real) = fs::canonicalize(a) {
            // `a` is an ancestor of `p`, so the prefix always strips.
            return p
                .strip_prefix(a)
                .map_or_else(|_| p.to_path_buf(), |rest| real.join(rest));
        }
    }
    p.to_path_buf()
}

/// The protected set for a run: the binding root's guarded checkouts, dfm's own directories,
/// install directory and executable, the lock file, and every ledger target.
#[expect(dead_code, reason = "wired by the script run")]
pub(super) fn protected(
    layout: &Layout,
    root: &Path,
    ledger: &Ledger,
    temp: &Path,
) -> Result<Protected, Refusal> {
    let io = |e: std::io::Error| Refusal::new(Code::IoFailed, format!("dfm's own path: {e}"));
    // The invoked path is a link on an installed machine; its real directory is the install.
    let exe = std::env::current_exe().map_err(io)?;
    let install = fs::canonicalize(&exe).map_err(io)?;
    let mut trees = crate::sync_cmd::guarded_checkouts(root)?;
    trees.extend([
        layout.state_root.clone(),
        layout.config_dir.clone(),
        layout.data_dir.clone(),
        layout.cache_dir.clone(),
    ]);
    trees.extend(install.parent().map(Path::to_path_buf));
    let mut paths: Vec<PathBuf> = ledger
        .resources
        .iter()
        .flat_map(|r| r.owned_outputs.iter().map(|o| PathBuf::from(&o.path)))
        .collect();
    paths.extend([layout.lock_file.clone(), exe]);
    Ok(Protected {
        home: canonical(&layout.home),
        temp: vec![canonical(temp)],
        denied_trees: trees.iter().map(|t| canonical(t)).collect(),
        denied_paths: paths.iter().map(|p| canonical(p)).collect(),
    })
}

/// The executables named `tool` in `path_var` and `provider_path(home)`, in search order: a
/// regular file with an execute bit, or a link to one. An unreadable entry counts as absent;
/// the sandbox, not this check, is what protects the machine.
#[cfg_attr(not(test), expect(dead_code, reason = "wired by the script run"))]
pub(super) fn on_path(home: &Path, path_var: &OsStr, tool: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in std::env::split_paths(path_var).chain(dfm_platform_macos::provider_path(home)) {
        if !dirs.contains(&d) {
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
    // The fetch runs before anything is decided, so a stop request ends curl instead of
    // leaving it running.
    signal::install()?;
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let runner = ctx.runner(&sys);
    let fail = state_cmd::fail_from_env()?;
    let readable = Store::open_read(sys, &layout, fail)?.ok_or_else(crate::state_lost_refusal)?;
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
    // An approval runs the bytes the preview cached; it never fetches again.
    let fetched = match &args.yes_plan {
        None => fetch(&runner, &layout.cache_dir, &s.url)?,
        Some(_) => cached(&layout.cache_dir, &s.url)?.ok_or_else(|| {
            Refusal::new(
                Code::PlanStale,
                format!(
                    "the script cache for {} is missing; run the preview again",
                    visible(&s.url)
                ),
            )
        })?,
    };
    if let Some(stop) = signal::requested() {
        return Err(stop);
    }
    let dfm_env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let env = script_env(&runner, &dfm_env, s.tty, &s.env)?;
    let baseline = paths.iter().map(|p| observe(p)).collect::<Result<_, _>>()?;
    let mut seen = Vec::new();
    for p in &data {
        match observe(p) {
            Ok(b) => seen.push(b),
            Err(e) => skipped.push(format!("{p} not observed ({})", e.detail)),
        }
    }
    let decision = Decision {
        trial_id,
        tool,
        url: s.url.clone(),
        fetched,
        shell: s.shell.clone(),
        args: s.args.clone(),
        env,
        tty: s.tty,
        baseline,
        data: seen,
        skipped,
        binary,
        stamp,
    };
    let digest = decision.digest()?;
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    decision.print(out, &digest).map_err(w)?;
    match &args.yes_plan {
        Some(approved) if *approved != digest => Err(stale(approved, &digest)),
        Some(_) => Err(usage("the script run is not implemented yet")),
        None if args.preview => Ok(()),
        None => Err(Refusal::new(
            Code::ApprovalRequired,
            format!(
                "nothing was changed. Fix: read the trial above, then run again with --yes-plan {digest}"
            ),
        )),
    }
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
        let env = script_env(&runner(), &dfm, false, &extra).unwrap();
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
        let with_tty = script_env(&runner(), &dfm, true, &extra).unwrap();
        let names: Vec<&str> = with_tty.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.is_sorted() && names.contains(&"TERM"), "{names:?}");
        assert_eq!(with_tty.len(), env.len() + 1);

        // LOGNAME stands in for USER; a relative SHELL is replaced.
        let dfm = pairs(&[("LOGNAME", "ln"), ("TMPDIR", "/t"), ("SHELL", "zsh")]);
        let env = script_env(&runner(), &dfm, false, &[]).unwrap();
        assert!(env.contains(&("USER".into(), "ln".into())), "{env:?}");
        assert!(env.contains(&("SHELL".into(), "/bin/sh".into())), "{env:?}");

        let bad = pairs(&[("USER", "me"), ("TMPDIR", "/t"), ("LANG", "a\nb")]);
        assert_eq!(
            script_env(&runner(), &bad, false, &[]).unwrap_err().code,
            Code::Usage
        );
        let bad_extra = pairs(&[("FOO", "a\nb")]);
        assert_eq!(
            script_env(&runner(), &dfm, false, &bad_extra)
                .unwrap_err()
                .code,
            Code::Usage
        );
    }

    #[test]
    fn the_digest_covers_the_script_bytes_the_environment_and_tty() {
        let d = |sha: &str, env: &[(&str, &str)], tty: bool| {
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
            }
            .digest()
            .unwrap()
        };
        let base = d("s1", &[("HOME", "/h")], false);
        assert_eq!(base, d("s1", &[("HOME", "/h")], false));
        assert_ne!(base, d("s2", &[("HOME", "/h")], false));
        assert_ne!(base, d("s1", &[("HOME", "/g")], false));
        assert_ne!(base, d("s1", &[("HOME", "/h")], true));
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
    fn canonical_keeps_a_missing_leaf_under_the_real_parent() {
        let dir = scratch("canonical");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(
            canonical(&link.join("missing/leaf")),
            dir.join("missing/leaf")
        );
        assert_eq!(canonical(&link), dir);
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
