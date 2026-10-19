//! `sync`: one invocation that fetches the declared sources into staging, validates the
//! composition they produce against this machine, takes one approval, publishes the sources into
//! the user's checkouts and applies the configuration once (docs/sync.md).
//!
//! Git runs through the bounded runner with an environment built from empty (docs/architecture.md
//! section 8, "Fetch environment"). The staging directory is the only thing a preview writes.
//! Publication is a fast-forward of each checkout and nothing else: never a reset, clean, stash,
//! rebase, commit or push. A nested source is pinned to an exact commit, and the lock that holds
//! the pin is never rewritten.

use crate::adapters::Generators;
use crate::plan_cmd::{plan_for, render};
use crate::read_fs::read_regular_bounded;
use crate::source_fs::load_scoped;
use crate::{
    Ctx, Loaded, apply_cmd, builtin_catalog, canonical_root, require_binding, state_cmd, usage,
};
use dfm_engine::{Dir, End, Lock as FileLock, ProcRequest, Runner, Store, Sys, now_ns};
use dfm_model::binding::Binding;
use dfm_model::digest::sha256_hex;
use dfm_model::display::visible;
use dfm_model::lock::{Grants, Lock, LockSource, MAX_BYTES as LOCK_MAX_BYTES, check_origin};
use dfm_model::plan::{Plan, Scope};
use dfm_model::resolve::resolve;
use dfm_model::state::{self, Class, JournalEntry, Phase, Recovery, rfc3339};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_FILE: &str = "dotfiles-manager.lock";
/// Section 10 item 6: the default bound of one external process. A fetch gets the whole bound.
const FETCH_TIMEOUT: Duration = Duration::from_secs(600);
/// A git command that touches no network.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_STDOUT: usize = 16 << 20;
/// Settings every git call carries (section 8): no hooks, no fsmonitor, objects checked on
/// fetch, no replacement refs, no line-ending rewrite.
const GIT_SETTINGS: [&str; 12] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "transfer.fsckObjects=true",
    "-c",
    "core.useReplaceRefs=false",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.eol=lf",
];

pub struct Args {
    apply: apply_cmd::Args,
    /// Origins to grant before the fetch (`--trust-origin`), each named in the lock.
    trust: Vec<String>,
}

/// `sync --root <dir> [--preview] [--yes-plan <digest>] [--allow-disruptive <id>]...
/// [--allow-adopt <id>]... [--trust-origin <origin>]...`
pub fn parse_args(args: &[String]) -> Result<Args, Refusal> {
    let (mut trust, mut rest) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--trust-origin" {
            let v = it
                .next()
                .ok_or_else(|| usage("--trust-origin needs a value"))?;
            trust.push(v.clone());
        } else {
            rest.push(a.clone());
        }
    }
    let apply = apply_cmd::parse_args(&rest, Scope::Config)?;
    if apply.preview && !trust.is_empty() {
        return Err(usage(
            "--preview writes nothing, so it cannot grant an origin; run sync --trust-origin without --preview",
        ));
    }
    // One command never both widens trust and publishes: the grant is reviewed first, then the
    // plan it allows (section 8: --yes-plan can never grant a new origin).
    if apply.yes_plan.is_some() && !trust.is_empty() {
        return Err(usage(
            "--trust-origin cannot be combined with --yes-plan; grant first, review the plan, then approve it",
        ));
    }
    Ok(Args { apply, trust })
}

fn untrusted(d: impl Into<String>) -> Refusal {
    Refusal::new(Code::SourceUntrusted, d)
}

fn conflict(d: impl Into<String>) -> Refusal {
    Refusal::new(Code::Conflict, d)
}

fn io_failed(what: &str, e: &std::io::Error) -> Refusal {
    Refusal::new(Code::IoFailed, format!("{what}: {}", e.kind()))
}

fn path_str(p: &Path) -> Result<&str, Refusal> {
    p.to_str().ok_or_else(|| {
        usage(format!(
            "{} is not a UTF-8 path; sync passes paths to git as text",
            p.display()
        ))
    })
}

// ---- git ------------------------------------------------------------------------------------

/// Git through the runner: a fixed search path, a manager-owned empty HOME, and the fetch
/// environment of section 8. `remote` allows `https` and `ssh` (and `file` in the isolated
/// build, for the tests' local remotes); `local` allows only the `file` transport, which the
/// fetches between the staging checkout and the user's checkout use.
struct Git {
    runner: Runner,
    remote: Vec<(String, String)>,
    local: Vec<(String, String)>,
}

impl Git {
    fn new(ctx: &Ctx, sys: &Sys) -> Git {
        let mut runner = ctx.runner(sys);
        // Section 8: a manager-owned empty directory as HOME. `ssh` still reads `~/.ssh` through
        // the password database, which the contract records.
        runner.home = ctx.platform.cache_dir().join("sync-home");
        let env = |protocols: &str| {
            [
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_CONFIG_SYSTEM", "/dev/null"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_NO_REPLACE_OBJECTS", "1"),
                ("GIT_ALLOW_PROTOCOL", protocols),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
        };
        let remote = if cfg!(feature = "test-isolation") {
            "https:ssh:file"
        } else {
            "https:ssh"
        };
        Git {
            runner,
            remote: env(remote),
            local: env("file"),
        }
    }

    fn output(&self, local: bool, args: &[&str]) -> Result<dfm_engine::ProcOutput, Refusal> {
        let argv: Vec<String> = GIT_SETTINGS
            .iter()
            .chain(args.iter())
            .map(|s| s.to_string())
            .collect();
        let (env, timeout) = if local {
            (&self.local, LOCAL_TIMEOUT)
        } else {
            (&self.remote, FETCH_TIMEOUT)
        };
        self.runner.run(&ProcRequest {
            program: "git",
            args: &argv,
            env,
            stdin: None,
            timeout,
            max_stdout: MAX_STDOUT,
        })
    }

    /// Runs git and returns its trimmed standard output, or a refusal with `code` that names
    /// `what` and the first line git wrote to standard error (bounded, made visible).
    fn run(&self, local: bool, code: Code, what: &str, args: &[&str]) -> Result<String, Refusal> {
        let out = self.output(local, args)?;
        if out.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string());
        }
        let why = match out.end {
            End::Exited(n) => format!("git exit {n}"),
            End::Signaled => "git was killed".into(),
            End::TimedOut => "git timed out".into(),
            End::Stopped(r) => return Err(r),
        };
        // The first lines git wrote (bounded by the runner), without its hint lines when
        // anything else was said: a refused merge names the files on the lines after the first.
        let text = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let said: Vec<&str> = match lines.iter().any(|l| !l.starts_with("hint:")) {
            true => lines
                .into_iter()
                .filter(|l| !l.starts_with("hint:"))
                .collect(),
            false => lines,
        };
        let said: String = said
            .iter()
            .take(4)
            .map(|l| l.chars().take(200).collect::<String>())
            .collect::<Vec<_>>()
            .join(" | ");
        Err(Refusal::new(
            code,
            format!("{what}: {why}: {}", visible(&said)),
        ))
    }
}

// ---- staging --------------------------------------------------------------------------------

/// A directory the manager owns: created with mode 0700 when missing; an existing one must be a
/// real directory (not a link) owned by the current user, and is set to 0700 again (the rule
/// of the runner's working directory).
fn ensure_private_dir(dir: &Path, uid: u32) -> Result<(), Refusal> {
    let e = |e: std::io::Error| io_failed("staging directory", &e);
    match fs::symlink_metadata(dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(dir).map_err(e)?;
        }
        Err(err) => return Err(e(err)),
        Ok(m) if !m.is_dir() => {
            return Err(conflict(format!(
                "{} is not a plain directory",
                dir.display()
            )));
        }
        Ok(m) if m.uid() != uid => {
            return Err(conflict(format!(
                "{} is owned by another user",
                dir.display()
            )));
        }
        Ok(_) => {}
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(e)
}

/// A per-run staging directory under the cache directory, `<pid>-<start>`, removed on drop. A
/// lock file inside it is held for the run's lifetime: the kernel releases it when the process
/// dies, so a later writer can tell a dead run's directory from a live one and remove it.
struct Staging {
    dir: PathBuf,
    _held: FileLock,
}

const STAGING_LOCK: &str = "lock";

impl Staging {
    /// `sweep` is for the run that holds the writer lock (section 10 item 4): only it removes
    /// the directories of dead runs, so a review pass, which holds the shared lock, can never
    /// remove another review's directory between its creation and its lock.
    fn create(ctx: &Ctx, sys: &Sys, sweep: bool) -> Result<Staging, Refusal> {
        let root = ctx.platform.cache_dir().join("staging");
        ensure_private_dir(&ctx.platform.cache_dir(), sys.uid)?;
        ensure_private_dir(&root, sys.uid)?;
        if sweep {
            Self::sweep(&root, sys);
        }
        let dir = root.join(format!("{}-{}", std::process::id(), now_ns()?));
        ensure_private_dir(&dir, sys.uid)?;
        let held = FileLock::acquire(sys, &Dir::open_plain(&dir)?, STAGING_LOCK)?;
        Ok(Staging { dir, _held: held })
    }

    /// Removes the staging directories of runs that are gone (section 10 item 4): an entry named
    /// `<pid>-<start>`, a real directory owned by the current user, whose lock file nobody
    /// holds. An entry that cannot be classified, or whose lock is held, is skipped.
    fn sweep(root: &Path, sys: &Sys) {
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale_name = name.split_once('-').is_some_and(|(p, s)| {
                !p.is_empty()
                    && !s.is_empty()
                    && p.bytes().chain(s.bytes()).all(|b| b.is_ascii_digit())
            });
            let Ok(m) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !stale_name || !m.is_dir() || m.uid() != sys.uid {
                continue;
            }
            let Ok(dir) = Dir::open_plain(&entry.path()) else {
                continue;
            };
            // Acquiring the lock proves no live run holds this directory.
            if FileLock::acquire(sys, &dir, STAGING_LOCK).is_ok() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// The staged root checkout; nested sources are checked out at their path inside it.
    fn tree(&self) -> PathBuf {
        self.dir.join("tree")
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// One declared source, rehearsed in the staged copy, beside the checkout it is published into.
struct Staged {
    src: LockSource,
    /// The staged copy of the checkout, now at `new`.
    repo: PathBuf,
    /// The checkout under `--root` the source is published into.
    live: PathBuf,
    /// The checkout's HEAD when the copy was taken, or `None` when the checkout does not exist
    /// yet (nested only).
    old: Option<String>,
    /// The commit to publish: the fetched head of the root's ref, the pin of a nested source.
    new: String,
}

impl Staged {
    fn change(&self) -> String {
        match &self.old {
            None => format!("new checkout at {}", self.new),
            Some(o) if *o == self.new => format!("current at {}", self.new),
            Some(o) => format!("{o}..{}", self.new),
        }
    }

    fn kind(&self) -> &'static str {
        if self.src.is_root() {
            "tracking"
        } else {
            "exact"
        }
    }
}

/// Reads the lock at `root`. The isolated build accepts `file` origins for the tests' local
/// remotes. A `tree` digest is accepted by the parser and not compared by this build, so a lock
/// that carries one is refused rather than half-checked.
fn read_lock(root: &Path) -> Result<Lock, Refusal> {
    let bytes = read_regular_bounded(
        &root.join(LOCK_FILE),
        LOCK_MAX_BYTES as u64,
        |e| Refusal::new(Code::ManifestInvalid, format!("{LOCK_FILE}: {}", e.kind())),
        || {
            Refusal::new(
                Code::ManifestInvalid,
                format!("{LOCK_FILE} is not a regular file"),
            )
        },
    )?;
    let lock = Lock::parse(&bytes, cfg!(feature = "test-isolation"))?;
    if lock.root().is_none() {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            format!(
                "{LOCK_FILE} declares no [[sources]]; sync has nothing to fetch. Fix: declare the root checkout (path = \".\") with its origin and ref"
            ),
        ));
    }
    // shortcut: the lock's tree digest is not recomputed from the staged checkout; compare it
    // against a clean export of the pinned commit once update writes locks.
    if let Some(s) = std::iter::once(lock.root())
        .flatten()
        .chain(lock.nested())
        .find(|s| s.tree.is_some())
    {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            format!(
                "{LOCK_FILE}: [[sources]] {} carries a tree digest, which this build does not check; remove the field",
                s.id
            ),
        ));
    }
    Ok(lock)
}

/// Entries a staged copy may hold. A checkout past this is not copied.
const MAX_COPY_ENTRIES: usize = 1_000_000;

/// Copies the checkout at `from` to `to` as it is: directories, regular files with their mode,
/// and symlinks as symlinks (never followed). A socket or FIFO inside a `.git` directory (a file
/// monitor's socket) is skipped; anywhere else it is untrusted, as the source loader says.
fn copy_checkout(from: &Path, to: &Path) -> Result<(), Refusal> {
    fn go(from: &Path, to: &Path, in_git: bool, seen: &mut usize) -> Result<(), Refusal> {
        let e = |e: std::io::Error| io_failed("copy the checkout into staging", &e);
        fs::create_dir(to).map_err(e)?;
        for entry in fs::read_dir(from).map_err(e)? {
            let entry = entry.map_err(e)?;
            *seen += 1;
            if *seen > MAX_COPY_ENTRIES {
                return Err(Refusal::new(
                    Code::LimitExceeded,
                    format!("the checkout holds more than {MAX_COPY_ENTRIES} entries"),
                ));
            }
            let (src, dst) = (entry.path(), to.join(entry.file_name()));
            let meta = fs::symlink_metadata(&src).map_err(e)?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&src).map_err(e)?, &dst).map_err(e)?;
            } else if ft.is_dir() {
                let git_dir = in_git || entry.file_name().eq_ignore_ascii_case(".git");
                go(&src, &dst, git_dir, seen)?;
            } else if ft.is_file() {
                fs::copy(&src, &dst).map_err(e)?;
            } else if !in_git {
                return Err(untrusted(format!(
                    "{} is not a regular file, symlink or directory",
                    visible(&src.display().to_string())
                )));
            }
        }
        // The mode goes on last: a read-only directory must still take its children.
        fs::set_permissions(to, fs::symlink_metadata(from).map_err(e)?.permissions()).map_err(e)
    }
    let mut seen = 0;
    go(from, to, false, &mut seen)
}

/// A checkout the command drives (the staged copy or the live one) must be self-contained: its
/// git directory inside it and its top level equal to it, so a gitfile (a worktree checkout)
/// or `core.worktree` cannot send git's writes to another place. It must not configure
/// attribute files or filters either: a fast-forward writes files through them (the same rule
/// as `bootstrap.sh install`).
fn check_live(git: &Git, live: &str) -> Result<(), Refusal> {
    let where_is = |what: &str, flag: &str| {
        git.run(
            true,
            Code::SourceUntrusted,
            what,
            &["-C", live, "rev-parse", flag],
        )
    };
    let git_dir = where_is("find the git directory", "--absolute-git-dir")?;
    let top = where_is("find the top level", "--show-toplevel")?;
    if !Path::new(&git_dir).starts_with(live) || top != live {
        return Err(untrusted(format!(
            "the checkout at {} keeps its git directory at {} with top level {}; sync needs a plain checkout whose .git is inside it (not a worktree, gitfile or core.worktree)",
            visible(live),
            visible(&git_dir),
            visible(&top)
        )));
    }
    check_config(git, live)?;
    let attrs = git.run(
        true,
        Code::SourceUntrusted,
        "git-path info/attributes",
        &[
            "-C",
            live,
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/attributes",
        ],
    )?;
    if fs::metadata(&attrs).is_ok_and(|m| m.len() > 0) {
        return Err(untrusted(format!(
            "the checkout at {} has .git/info/attributes; remove it first",
            visible(live)
        )));
    }
    Ok(())
}

/// A configuration key a checkout may carry (docs/sync.md, "Checkout configuration"): what
/// `git init` and `git clone` write, plus the identity keys. Every fetch names its URL on the
/// command line, and a URL is never a remote nickname (a nickname holds no slash), so
/// `remote.*` and `branch.*` never reach a fetch, a fast-forward or a detached checkout.
fn config_key_allowed(key: &str) -> bool {
    const PLAIN: [&str; 8] = [
        "core.repositoryformatversion",
        "core.filemode",
        "core.bare",
        "core.logallrefupdates",
        "core.ignorecase",
        "core.precomposeunicode",
        "user.name",
        "user.email",
    ];
    if PLAIN.contains(&key) {
        return true;
    }
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    // Namespaces of tools that write into .git/config; git itself reads no key there.
    if matches!(section, "worktrunk" | "coderabbit") {
        return true;
    }
    let Some((_, name)) = rest.rsplit_once('.') else {
        return false;
    };
    // Not branch.<name>.mergeOptions or .rebase: mergeOptions reaches `merge`.
    matches!(
        (section, name),
        ("remote", "url" | "fetch" | "pushurl")
            | ("branch", "remote" | "merge" | "vscode-merge-base")
    )
}

/// Every git command in a checkout loads its `.git/config` (and `.git/config.worktree`, and
/// whatever `include.path` names), which the scrubbed environment cannot remove. A key there
/// can send a fetch elsewhere (`url.*.insteadOf`, `remote.*.proxy`, `http.*`), run a command
/// (`credential.helper`, `core.sshCommand`, `core.gitProxy`, `core.askPass`, `alias.*`), or
/// rewrite the files a fast-forward writes (`filter.*`, `core.attributesFile`, `core.symlinks`).
/// So the whole configuration is listed with its origins, the command-line pins are skipped,
/// and any other key outside [`config_key_allowed`] is refused by name: a value may hold a
/// credential, so it is never printed.
fn check_config(git: &Git, live: &str) -> Result<(), Refusal> {
    let o = git.output(
        true,
        &["-C", live, "config", "--list", "--show-origin", "-z"],
    )?;
    if !o.success() {
        return Err(untrusted(format!(
            "cannot read the git configuration of the checkout at {}",
            visible(live)
        )));
    }
    // `-z`: `<origin>\0<key>\n<value>\0` per entry; a `-c` pin's origin is `command line:`.
    let text = String::from_utf8_lossy(&o.stdout);
    let mut fields = text.split('\0');
    while let (Some(origin), Some(entry)) = (fields.next(), fields.next()) {
        let key = entry.split('\n').next().unwrap_or(entry);
        if origin == "command line:" || config_key_allowed(key) {
            continue;
        }
        // A subsection can hold a credential too (`url.https://user:token@host/.insteadOf`).
        let shown = match (key.split_once('.'), key.rsplit_once('.')) {
            (Some((section, rest)), Some((_, name))) if rest.contains('.') => {
                format!("{section}.*.{name}")
            }
            _ => key.to_string(),
        };
        return Err(untrusted(format!(
            "the checkout at {} configures {}, which sync does not allow because git would read it during the fetch or the fast-forward; remove it from the checkout's configuration (a global setting is not read)",
            visible(live),
            visible(&shown)
        )));
    }
    Ok(())
}

/// The HEAD commit of the checkout at `dir`, or `None` when there is no checkout there.
fn head_of(git: &Git, dir: &Path) -> Result<Option<String>, Refusal> {
    match fs::symlink_metadata(dir.join(".git")) {
        Ok(_) => {
            let dir = path_str(dir)?;
            check_live(git, dir)?;
            git.run(
                true,
                Code::SourceUntrusted,
                "rev-parse HEAD of the checkout",
                &[
                    "-C",
                    dir,
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    "HEAD^{commit}",
                ],
            )
            .map(Some)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match fs::symlink_metadata(dir) {
            Ok(_) => Err(conflict(format!(
                "{} exists and is not a git checkout; move it aside",
                visible(path_str(dir)?)
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_failed("checkout", &e)),
        },
        Err(e) => Err(io_failed("checkout", &e)),
    }
}

/// Moves the checkout at `dir` from `old` to `new` the one way sync ever moves a checkout: a
/// fast-forward (or a first checkout into a directory that does not exist). Divergence and a
/// local change the update would overwrite make git refuse, and nothing is reset, cleaned,
/// stashed or rebased. `from` is where the objects come from; `None` when `dir` holds them.
fn advance(
    git: &Git,
    id: &str,
    dir: &str,
    old: Option<&str>,
    new: &str,
    from: Option<&str>,
) -> Result<(), Refusal> {
    if let Some(from) = from {
        if old.is_none() {
            git.run(
                true,
                Code::IoFailed,
                &format!("{id}: init the checkout"),
                &["init", "-q", dir],
            )?;
        }
        git.run(
            true,
            Code::SourceMissing,
            &format!("{id}: fetch {new} into the checkout"),
            &[
                "-C",
                dir,
                "fetch",
                "-q",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                from,
                new,
            ],
        )?;
    }
    if old.is_none() {
        git.run(
            true,
            Code::Conflict,
            &format!("{id}: check out {new}"),
            &["-C", dir, "checkout", "-q", "--detach", "--end-of-options", new],
        )
    } else {
        git.run(
            true,
            Code::Conflict,
            &format!("{id}: fast-forward the checkout to {new} (sync never resets, rebases, stashes or cleans; commit or move your work aside, then sync again)"),
            &["-C", dir, "merge", "-q", "--ff-only", "--end-of-options", new],
        )
    }
    .map(|_| ())
}

/// Rehearses one source in the staged copy: fetches its ref from the origin, decides the commit
/// to publish, and fast-forwards the copy to it. The copy holds the user's local state, so what
/// git accepts here is what it will accept at publication.
fn stage_source(
    git: &Git,
    src: &LockSource,
    repo: PathBuf,
    live: PathBuf,
) -> Result<Staged, Refusal> {
    let id = src.id.as_str();
    let repo_s = path_str(&repo)?.to_string();
    let old = head_of(git, &repo)?;
    if src.is_root() && old.is_none() {
        return Err(untrusted(format!(
            "--root {} is not a git checkout; sync publishes into the checkout that holds the profile",
            visible(path_str(&live)?)
        )));
    }
    if old.is_none() {
        // A first checkout: the fetch needs a repository to land in.
        git.run(
            true,
            Code::IoFailed,
            &format!("{id}: init staging"),
            &["init", "-q", &repo_s],
        )?;
    }
    git.run(
        false,
        Code::SourceMissing,
        &format!("{id}: fetch {} from {}", src.reference, src.origin),
        &[
            "-C",
            &repo_s,
            "fetch",
            "-q",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            &src.origin,
            &src.reference,
        ],
    )?;
    let head = git.run(
        true,
        Code::SourceMissing,
        &format!("{id}: resolve the fetched head"),
        &[
            "-C",
            &repo_s,
            "rev-parse",
            "--verify",
            "--end-of-options",
            "FETCH_HEAD^{commit}",
        ],
    )?;
    let new = match &src.commit {
        None => head,
        Some(pin) => {
            let reachable = git
                .output(
                    true,
                    &[
                        "-C",
                        &repo_s,
                        "merge-base",
                        "--is-ancestor",
                        pin,
                        "FETCH_HEAD",
                    ],
                )?
                .success();
            if !reachable {
                return Err(Refusal::new(
                    Code::SourceMissing,
                    format!(
                        "{id}: the pinned commit {pin} is not in {} of {} (its head is {head}); the pin stays as declared and the head is not used in its place. Fix: push the pinned commit to that ref, or change the pin in {LOCK_FILE} yourself",
                        src.reference, src.origin
                    ),
                ));
            }
            pin.clone()
        }
    };
    if old.as_deref() != Some(new.as_str()) {
        // The objects are already in the copy (the fetch above), so the fast-forward is local.
        advance(git, id, &repo_s, old.as_deref(), &new, None)?;
    }
    Ok(Staged {
        src: src.clone(),
        repo,
        live,
        old,
        new,
    })
}

/// A nested source lands at `base/<path>`. Every component of that path that exists must be a
/// real directory: a symlink there would carry the checkout (and git's writes) outside `base`.
/// A component that does not exist yet is fine; git creates it.
fn check_nested_path(base: &Path, path: &str) -> Result<(), Refusal> {
    let mut at = base.to_path_buf();
    for component in path.split('/') {
        at.push(component);
        match fs::symlink_metadata(&at) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_failed("nested source path", &e)),
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Refusal::new(
                    Code::SourceEscape,
                    format!(
                        "{} is a symlink; a nested source path may not pass through one",
                        visible(&at.display().to_string())
                    ),
                ));
            }
            Ok(m) if !m.is_dir() => {
                return Err(conflict(format!(
                    "{} is not a directory; move it aside",
                    visible(&at.display().to_string())
                )));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Copies the checkout into staging and rehearses every source there: the root first, from the
/// lock at `root`; then the nested sources that the fetched root's lock declares, each origin
/// checked against the grants before its fetch. Returns the staged sources, root first.
fn stage_all(
    git: &Git,
    root_src: &LockSource,
    grants: &Grants,
    root: &Path,
    tree: &Path,
) -> Result<Vec<Staged>, Refusal> {
    // A checkout that holds the cache directory (a HOME that is itself the checkout) would be
    // copied into itself.
    if tree.starts_with(root) {
        return Err(conflict(format!(
            "--root {} contains the manager's cache directory, which sync stages into; the checkout cannot hold it",
            visible(path_str(root)?)
        )));
    }
    copy_checkout(root, tree)?;
    let staged_root = stage_source(git, root_src, tree.to_path_buf(), root.to_path_buf())?;
    let fetched = read_lock(tree)?;
    let fetched_root = fetched
        .root()
        .ok_or_else(|| untrusted(format!("the fetched {LOCK_FILE} declares no root source")))?;
    if fetched_root.origin != root_src.origin || fetched_root.reference != root_src.reference {
        return Err(untrusted(format!(
            "the fetched {LOCK_FILE} names {} {} for the root checkout, not the {} {} this checkout declares; a source may not redirect itself. Fix: change the checkout's lock yourself if the move is intended",
            fetched_root.origin, fetched_root.reference, root_src.origin, root_src.reference
        )));
    }
    let untrusted_origins: Vec<&str> = fetched
        .nested()
        .iter()
        .filter(|s| !grants.trusts(&s.origin))
        .map(|s| s.origin.as_str())
        .collect();
    if !untrusted_origins.is_empty() {
        return Err(untrusted_refusal(&untrusted_origins));
    }
    let mut staged = vec![Staged {
        src: fetched_root.clone(),
        ..staged_root
    }];
    for src in fetched.nested() {
        check_nested_path(root, &src.path)?;
        check_nested_path(tree, &src.path)?;
        staged.push(stage_source(
            git,
            src,
            tree.join(&src.path),
            root.join(&src.path),
        )?);
    }
    Ok(staged)
}

/// The digest the owner approves: the configuration plan's digest and, per source, what moves
/// from where to where. The plan digest covers the loaded source tree, not the commits behind
/// it, so a commit that changes only files outside the scoped roots would otherwise pass.
fn sync_digest(plan_digest: &str, staged: &[Staged]) -> String {
    let mut text = format!("plan {plan_digest}\n");
    for s in staged {
        text.push_str(&format!(
            "source {} {} {} {} {}\n",
            s.src.id,
            s.src.origin,
            s.src.reference,
            s.old.as_deref().unwrap_or("none"),
            s.new
        ));
    }
    sha256_hex(text.as_bytes())
}

/// Records one grant in the journal around its write (section 10, "Mixed results"): a
/// `grant-origin` intent, the rewrite of `grants.json`, then its `done` entry. A run that dies
/// in between leaves the intent open, which `doctor` and `state recover` report.
fn grant_journaled(
    store: &Store,
    grants: &Grants,
    origin: &str,
    id: &str,
    at: &str,
) -> Result<(), Refusal> {
    let entry = |phase: Phase| JournalEntry {
        schema: state::SCHEMA,
        op_id: id.to_string(),
        op: "grant-origin".into(),
        phase,
        at: at.to_string(),
        resource: origin.to_string(),
        adapter: "sync".into(),
        class: Class::Normal,
        recovery: (phase == Phase::Intent).then(|| Recovery {
            outputs: Vec::new(),
            snapshot: None,
        }),
        token: None,
        reason: Some(format!("grant origin {origin}")),
    };
    store.append_journal(&entry(Phase::Intent))?;
    store.write_grants(grants)?;
    store.append_journal(&entry(Phase::Done))
}

/// Resolves and plans the staged tree as the tree `root` will hold after publication: the
/// sources are read from `tree`, the plan names `root`.
fn plan_staged(
    ctx: &Ctx,
    binding: &Binding,
    root: &Path,
    rel: &str,
    tree: &Path,
) -> Result<(Loaded, Plan), Refusal> {
    let sources = load_scoped(tree, rel)?;
    let resolved = resolve(&sources, rel, &builtin_catalog())?;
    let loaded = Loaded {
        binding: binding.clone(),
        root: root.to_path_buf(),
        staged: Some(tree.to_path_buf()),
        sources,
        resolved,
    };
    if let Some(d) = crate::machine_mismatch(ctx, &loaded) {
        return Err(Refusal::new(Code::PlatformMismatch, d));
    }
    let plan = plan_for(ctx, &loaded, Scope::Config, BTreeMap::new())?;
    Ok((loaded, plan))
}

/// Publishes one staged source into its checkout exactly as the rehearsal moved the copy, after
/// rechecking that the checkout is where the copy saw it. `Ok(true)` when the checkout changed.
fn publish(git: &Git, s: &Staged) -> Result<bool, Refusal> {
    let id = s.src.id.as_str();
    let (repo_s, live_s) = (path_str(&s.repo)?, path_str(&s.live)?);
    let now = head_of(git, &s.live)?;
    if now != s.old {
        return Err(Refusal::new(
            Code::PlanStale,
            format!(
                "{id}: the checkout at {} is at {} now and was at {} when the plan was made. Fix: plan again",
                visible(live_s),
                now.as_deref().unwrap_or("no checkout"),
                s.old.as_deref().unwrap_or("no checkout")
            ),
        ));
    }
    if s.old.as_deref() == Some(s.new.as_str()) {
        return Ok(false);
    }
    advance(git, id, live_s, s.old.as_deref(), &s.new, Some(repo_s))?;
    // Postcondition: the checkout is at the published commit.
    let after = head_of(git, &s.live)?;
    if after.as_deref() != Some(s.new.as_str()) {
        return Err(Refusal::new(
            Code::IoUnconfirmed,
            format!(
                "{id}: the checkout at {} is at {} after publication, not {}",
                visible(live_s),
                after.as_deref().unwrap_or("no checkout"),
                s.new
            ),
        ));
    }
    Ok(true)
}

// ---- the command ----------------------------------------------------------------------------

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let mut plan_printed = false;
    let r = run_inner(ctx, args, out, &mut plan_printed);
    if let Err(e) = &r
        && plan_printed
    {
        let _ = writeln!(out, "result: {}", e.exit().name());
    }
    r
}

fn run_inner(
    ctx: &Ctx,
    args: &Args,
    out: &mut dyn Write,
    plan_printed: &mut bool,
) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = require_binding(ctx)?;
    let (root, rel) = canonical_root(&binding, Path::new(&args.apply.root))?;
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    let live_lock = read_lock(&root)?;
    let root_src = live_lock
        .root()
        .ok_or_else(|| untrusted(format!("{LOCK_FILE} declares no root source")))?;

    // Trust first, before any fetch (section 8, "Trust grant"). A grant is written only for an
    // origin the owner names on the command line, under the writer lock and journaled.
    let readable = Store::open_read(sys, &layout, state_cmd::fail_from_env()?)?
        .ok_or_else(crate::state_lost_refusal)?;
    readable.require_identity(&binding)?;
    let mut grants = readable.read_grants()?;
    drop(readable);
    for o in &args.trust {
        check_origin(o, cfg!(feature = "test-isolation"))?;
    }
    if !args.trust.is_empty() {
        state_cmd::fail_from_env()?.hit("sync-before-grant-lock")?;
        let store = Store::open_write_with(sys, &layout, state_cmd::fail_from_env()?, false)?;
        if !store.is_initialized() {
            return Err(crate::state_lost_refusal());
        }
        store.require_identity(&binding)?;
        // Read again under the lock: another sync may have granted since the read above, and a
        // rewrite from that snapshot would drop its grant.
        grants = store.read_grants()?;
        let at = rfc3339(now_ns()?);
        for o in &args.trust {
            if grants.trusts(o) {
                writeln!(out, "granted: {o} (already)").map_err(w)?;
                continue;
            }
            let id = grants.grant(o, &at);
            // shortcut: the grant reaches the journal and grants.json, not a receipt field;
            // origin_grants[] in the receipt waits for the sync receipt of DFM-31.
            grant_journaled(&store, &grants, o, &id, &at)?;
            writeln!(out, "granted: {o} {id}").map_err(w)?;
        }
    }
    // The root origin is checked here; the nested origins, which the fetched root's lock
    // declares, are checked after the root fetch and before their own.
    if !grants.trusts(&root_src.origin) {
        return Err(untrusted_refusal(&[root_src.origin.as_str()]));
    }

    // The review pass holds the shared lock while its staging directory exists (section 10
    // item 4), so a writer that starts meanwhile is refused and no writer sweeps this run.
    let shared = Store::probe_writer(&sys, &layout)?;
    // Copy the checkout into staging, rehearse the fetch and fast-forward there, then plan the
    // staged tree as the tree --root will hold.
    let git = Git::new(ctx, &sys);
    let staging = Staging::create(ctx, &sys, false)?;
    let previewed = stage_all(&git, root_src, &grants, &root, &staging.tree())?;
    for s in &previewed {
        writeln!(
            out,
            "source: {} {} {} {} ({})",
            s.src.id,
            s.src.origin,
            s.src.reference,
            s.change(),
            s.kind()
        )
        .map_err(w)?;
    }
    let (loaded, plan) = plan_staged(ctx, &binding, &root, &rel, &staging.tree())?;
    let plan_digest = plan.digest()?;
    let digest = sync_digest(&plan_digest, &previewed);
    // With an approval the apply below prints the plan it runs; otherwise this is the review.
    // The digest shown, and the one --yes-plan takes, is the sync digest.
    let applying = args.apply.yes_plan.is_some() && !args.apply.preview;
    if !applying {
        render(&plan, &digest, out).map_err(w)?;
        *plan_printed = true;
    }
    // The checks apply runs before its lock (docs/apply.md), against the sync digest, so a plan
    // that apply would refuse is refused here before any source is published.
    apply_cmd::check_before_lock(
        ctx,
        &args.apply,
        &plan,
        &digest,
        &Generators::new(&loaded.resolved),
        out,
    )?;
    if args.apply.preview {
        return Ok(());
    }
    let Some(approved) = &args.apply.yes_plan else {
        return Err(Refusal::new(
            Code::ApprovalRequired,
            format!(
                "nothing was changed. Fix: read the plan above, then run again with --yes-plan {digest}"
            ),
        ));
    };
    drop(staging);
    drop(shared);

    // Publication under the lock: fetch and plan again, so the sources and the checkouts are
    // rechecked immediately before anything is published, then fast-forward each checkout.
    let store = Store::open_write_with(sys, &layout, state_cmd::fail_from_env()?, false)?;
    if !store.is_initialized() {
        return Err(crate::state_lost_refusal());
    }
    store.require_identity(&binding)?;
    let second = Staging::create(ctx, &sys, true)?;
    let staged = stage_all(&git, root_src, &grants, &root, &second.tree())?;
    let (_, locked) = plan_staged(ctx, &binding, &root, &rel, &second.tree())?;
    let locked_plan_digest = locked.digest()?;
    let locked_digest = sync_digest(&locked_plan_digest, &staged);
    if locked_digest != digest {
        return Err(apply_cmd::stale(approved, &locked_digest));
    }
    // The digest already binds the commits; this is the same check in process, as a guard.
    let moved = |s: &Staged| (s.src.id.clone(), s.old.clone(), s.new.clone());
    if staged.len() != previewed.len()
        || staged
            .iter()
            .zip(&previewed)
            .any(|(a, b)| moved(a) != moved(b))
    {
        return Err(Refusal::new(
            Code::PlanStale,
            "a source moved since the reviewed run: the commits to publish are not the ones shown with this digest. Fix: read the new plan with --preview and approve its digest",
        ));
    }
    let mut published = Vec::new();
    for s in &staged {
        match publish(&git, s) {
            Ok(changed) => {
                writeln!(out, "published: {} {}", s.src.id, s.change()).map_err(w)?;
                if changed {
                    published.push(s.src.id.as_str());
                }
            }
            Err(e) if published.is_empty() => return Err(e),
            Err(e) => {
                // Honest partial publication: what changed is named, and the exit is 1.
                writeln!(
                    out,
                    "sync: published {}; {} was not published; the configuration was not applied",
                    published.join(", "),
                    s.src.id
                )
                .map_err(w)?;
                return Err(e.after_mutation());
            }
        }
    }
    drop(second);
    drop(store);

    // Apply once with the configuration plan's digest, which the sync digest covers: apply
    // plans --root (now the published tree), compares, takes the lock, plans again and runs
    // (docs/apply.md).
    let apply_args = apply_cmd::Args {
        yes_plan: Some(locked_plan_digest),
        ..args.apply.clone()
    };
    match apply_cmd::run(ctx, &apply_args, out) {
        Ok(()) => Ok(()),
        Err(e) if published.is_empty() => Err(e),
        Err(e) => {
            writeln!(
                out,
                "sync: published {}; the configuration was not applied (see the result above)",
                published.join(", ")
            )
            .map_err(w)?;
            Err(e.after_mutation())
        }
    }
}

fn untrusted_refusal(origins: &[&str]) -> Refusal {
    let fix: Vec<String> = origins
        .iter()
        .map(|o| format!("sync --trust-origin {o}"))
        .collect();
    untrusted(format!(
        "no grant for {}; nothing was fetched. Fix: review each origin, then run {}",
        origins.join(", "),
        fix.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_origin_cannot_be_combined_with_preview() {
        let args: Vec<String> = ["--root", "/r", "--preview", "--trust-origin", "https://h/x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_args(&args).err().map(|e| e.code), Some(Code::Usage));
        let args: Vec<String> = [
            "--root",
            "/r",
            "--yes-plan",
            &"0".repeat(64),
            "--trust-origin",
            "https://h/x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(parse_args(&args).err().map(|e| e.code), Some(Code::Usage));
        let args: Vec<String> = ["--root", "/r", "--trust-origin", "https://h/x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_args(&args).unwrap().trust, ["https://h/x"]);
    }
}
