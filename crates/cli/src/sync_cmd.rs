//! `sync`: one invocation that fetches the declared sources into staging, validates the
//! composition they produce against this machine, takes one approval, publishes the sources into
//! the user's checkouts and then runs the four scopes in order: configuration, packages, secrets
//! and services (docs/sync.md).
//!
//! Git runs through the bounded runner with an environment built from empty (docs/sync.md,
//! "Fetch environment"). The staging directory is the only thing a preview writes.
//! Publication is a fast-forward of each checkout and nothing else: never a reset, clean, stash,
//! rebase, commit or push. A nested source is pinned to an exact commit, and the lock that holds
//! the pin is never rewritten.

use crate::adapters::{secret, updater};
use crate::plan_cmd::{plan_for, plan_with_outputs, render_body};
use crate::read_fs::read_regular_bounded;
use crate::source_fs::load_scoped;
use crate::{
    Ctx, Loaded, apply_cmd, builtin_catalog, canonical_root, cli, paint, require_binding,
    state_cmd, usage,
};
use dfm_engine::{
    Approval as Allowance, Dir, End, Lock as FileLock, ProcRequest, Runner, Store, Sys, now_ns,
};
use dfm_model::binding::Binding;
use dfm_model::digest::sha256_hex;
use dfm_model::display::{visible, visible_for_paste};
use dfm_model::lock::{Grants, Lock, LockSource, MAX_BYTES as LOCK_MAX_BYTES, check_identity};
use dfm_model::plan::{Action, Blocker, Ownership, Plan, Scope, Status as PlanStatus};
use dfm_model::resolve::{Resolved, resolve};
use dfm_model::state::{self, Class, JournalEntry, Phase, Recovery, rfc3339};
use dfm_model::updater::Updater;
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_FILE: &str = "dotfiles-manager.lock";
/// The default bound of one external process. A fetch gets the whole bound.
const FETCH_TIMEOUT: Duration = Duration::from_secs(600);
/// A git command that touches no network.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_STDOUT: usize = 16 << 20;
/// Settings every git call carries (docs/sync.md): no hooks, no fsmonitor, objects checked on
/// fetch, no replacement refs, no line-ending rewrite, and no HTTP redirect: a redirect would
/// send the request to a host the grant never named, so the fetch fails instead. The same pin
/// drops every `objects/info/http-alternates` and `objects/info/alternates` entry of a dumb-HTTP
/// server (git's `is_alternate_allowed`, http-walker.c, admits one only under
/// `followRedirects=true`), so a server without smart HTTP cannot send the fetch elsewhere.
const GIT_SETTINGS: [&str; 14] = [
    "-c",
    "http.followRedirects=false",
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
    /// Identities to grant before the fetch (`--trust-origin`): canonical origins and
    /// `npm:<package>` identities (docs/sync.md, "Source identities").
    trust: Vec<String>,
    /// `--restart <resource-id>`, for the services scope.
    restart: BTreeSet<String>,
    /// False with `--no-upgrade`: the profile's updaters do not run (docs/upgrade.md).
    upgrade: bool,
}

/// `sync [--root <dir>] [--preview] [--yes-plan <digest>] [--allow-disruptive <id>]...
/// [--allow-adopt <id>]... [--restart <resource-id>]... [--trust-origin <origin>]...`
pub fn parse_args(m: &clap::ArgMatches) -> Result<Args, Refusal> {
    let (trust, restart) = (
        cli::all(m, "trust-origin"),
        cli::all(m, "restart").into_iter().collect(),
    );
    let upgrade = !cli::flag(m, "no-upgrade");
    let apply = apply_cmd::parse_args(m, Scope::Config)?;
    if apply.preview && !trust.is_empty() {
        return Err(usage(
            "--preview writes nothing, so it cannot grant an origin; run sync --trust-origin without --preview",
        ));
    }
    // One command never both widens trust and publishes: the grant is reviewed first, then the
    // plan it allows (--yes-plan can never grant a new origin).
    if apply.yes_plan.is_some() && !trust.is_empty() {
        return Err(usage(
            "--trust-origin cannot be combined with --yes-plan; grant first, review the plan, then approve it",
        ));
    }
    Ok(Args {
        apply,
        trust,
        restart,
        upgrade,
    })
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
/// environment of docs/sync.md. `remote` allows `https` and `ssh` (and `file` in the isolated
/// build, for the tests' local remotes); `local` allows only the `file` transport, which the
/// fetches between the staging checkout and the user's checkout use.
pub(crate) struct Git {
    runner: Runner,
    remote: Vec<(String, String)>,
    local: Vec<(String, String)>,
}

impl Git {
    pub(crate) fn new(ctx: &Ctx, sys: &Sys) -> Git {
        let mut runner = ctx.runner(sys);
        // A manager-owned empty directory as HOME. `ssh` still reads `~/.ssh` through the
        // password database (docs/sync.md, "Known limits").
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

    pub(crate) fn output(
        &self,
        local: bool,
        args: &[&str],
    ) -> Result<dfm_engine::ProcOutput, Refusal> {
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
    pub(crate) fn run(
        &self,
        local: bool,
        code: Code,
        what: &str,
        args: &[&str],
    ) -> Result<String, Refusal> {
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
    /// `sweep` is for the run that holds the writer lock: only it removes
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

    /// Removes the staging directories of runs that are gone: an entry named
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
    /// That head or pin when the checkout already holds it under local commits; `new` is then
    /// the checkout's HEAD, which stays where it is.
    ahead_of: Option<String>,
    /// The branch the root checkout has out (`refs/heads/...`), `None` when its HEAD is detached
    /// or the source is nested. Shown on the `source:` line only, not in the digest.
    branch: Option<String>,
}

impl Staged {
    fn change(&self) -> String {
        match (&self.old, &self.ahead_of) {
            (None, _) => format!("new checkout at {}", self.new),
            (Some(_), Some(a)) => format!("current at {} (ahead of {a})", self.new),
            (Some(o), None) if *o == self.new => format!("current at {}", self.new),
            (Some(o), None) => format!("{o}..{}", self.new),
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
    // Grafts and a shallow boundary rewrite the ancestry git reports: a graft can join
    // unrelated commits so that `merge --ff-only` moves HEAD over local-only history, and
    // `GIT_NO_REPLACE_OBJECTS` does not cover them. Both must be absent before any ancestry
    // answer is trusted.
    for file in ["info/grafts", "shallow"] {
        let path = git.run(
            true,
            Code::SourceUntrusted,
            "git-path ancestry file",
            &[
                "-C",
                live,
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                file,
            ],
        )?;
        if fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            return Err(untrusted(format!(
                "the checkout at {} has .git/{file}, which rewrites ancestry; remove it first",
                visible(live)
            )));
        }
    }
    Ok(())
}

/// A configuration key a checkout may carry (docs/sync.md, "Checkout configuration"): what
/// `git init` and `git clone` write, plus the identity keys. Every fetch names its URL on the
/// command line, and git matches that argument against the configured remote nicknames before
/// it treats it as a URL (`[remote "https://host/x"] url = elsewhere` redirects a fetch of
/// `https://host/x`; a nickname beginning with `/` is the one shape git refuses). So a remote
/// section is allowed only under a plain nickname, with no `/` and no `:`, which no canonical
/// origin (`scheme://`) and no staging path (`/...`) can equal. `branch.*` reaches no fetch,
/// fast-forward or detached checkout.
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
    let Some((nick, name)) = rest.rsplit_once('.') else {
        return false;
    };
    let plain_nick = !nick.contains('/') && !nick.contains(':');
    // Not branch.<name>.mergeOptions or .rebase: mergeOptions reaches `merge`.
    match (section, name) {
        ("remote", "url" | "fetch" | "pushurl") => plain_nick,
        ("branch", "remote" | "merge" | "vscode-merge-base") => true,
        _ => false,
    }
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

/// Whether `a` is an ancestor of `b` (or `b` itself) in the repository at `repo`, both commits
/// known to it. git answers with exit 0 or 1; any other end is a refusal, not a "no".
fn is_ancestor(git: &Git, repo: &str, a: &str, b: &str) -> Result<bool, Refusal> {
    match git
        .output(true, &["-C", repo, "merge-base", "--is-ancestor", a, b])?
        .end
    {
        End::Exited(0) => Ok(true),
        End::Exited(1) => Ok(false),
        End::Stopped(r) => Err(r),
        _ => Err(Refusal::new(
            Code::SourceMissing,
            format!("git cannot tell whether {a} is an ancestor of {b} in the staged copy"),
        )),
    }
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
            // Not `is_ancestor`: a pin the copy has never seen makes git exit 128, which is
            // this same answer.
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
    // A checkout that already holds `new` under local commits stays at its HEAD: sync never
    // moves a checkout back, and `merge --ff-only` would only answer "Already up to date".
    let (new, ahead_of) = match old.as_deref() {
        Some(o) if o != new && is_ancestor(git, &repo_s, &new, o)? => (o.to_string(), Some(new)),
        _ => (new, None),
    };
    if old.as_deref() != Some(new.as_str()) {
        // The objects are already in the copy (the fetch above), so the fast-forward is local.
        advance(git, id, &repo_s, old.as_deref(), &new, None)?;
    }
    let branch = if src.is_root() {
        let out = git.output(true, &["-C", &repo_s, "symbolic-ref", "-q", "HEAD"])?;
        match out.end {
            End::Exited(0) => Some(String::from_utf8_lossy(&out.stdout).trim().to_string()),
            End::Exited(1) => None,
            End::Stopped(r) => return Err(r),
            _ => return Err(untrusted(format!("{id}: git symbolic-ref HEAD failed"))),
        }
    } else {
        None
    };
    Ok(Staged {
        src: src.clone(),
        repo,
        live,
        old,
        new,
        ahead_of,
        branch,
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

/// The digest the owner approves: the combined plan's digest, the secrets the run skips for want
/// of a session and, per source, what moves from where to where. A plan digest covers the loaded
/// source tree, not the commits behind it, so a commit that changes only files outside the
/// scoped roots would otherwise pass.
fn sync_digest(review: &Review, staged: &[Staged]) -> String {
    let mut text = format!("plan {} {}\n", Scope::Sync.as_str(), review.digest);
    for r in &review.excluded.skipped {
        text.push_str(&format!("skip {} {r}\n", Scope::Secrets.as_str()));
    }
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

/// Records one grant in the journal around its write: a
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

/// The parts of one run, in run order (docs/sync.md).
const SCOPES: [Scope; 4] = [
    Scope::Config,
    Scope::Packages,
    Scope::Secrets,
    Scope::Services,
];

/// The combined plan of the staged tree (`Scope::Sync`) and what this run leaves out of it.
struct Review {
    plan: Plan,
    digest: String,
    /// The part (scope) of each operation id (`parts`).
    parts: BTreeMap<String, Scope>,
    excluded: Excluded,
}

impl Review {
    fn part(&self, id: &str) -> Scope {
        apply_cmd::part_of(&self.parts, id)
    }

    /// The plan the run approves and executes: without the excluded operations, and without the
    /// blockers of the parts that do not run. A configuration blocker stays and refuses the run.
    fn runnable(&self) -> Plan {
        self.runnable_with(&self.excluded)
    }

    /// [`Review::runnable`] with the exclusions `ex`.
    fn runnable_with(&self, ex: &Excluded) -> Plan {
        let ids = ex.ids(self);
        let mut plan = apply_cmd::without(&self.plan, &ids);
        // An operation that runs here after a left-out one does not wait for it: a withheld
        // adoption leaves HOME as it is, and a part that does not run changes nothing.
        for o in &mut plan.operations {
            o.after.retain(|a| !ids.contains(a));
        }
        plan.blockers.retain(|b| {
            !ex.blocked
                .contains(&blocker_part(&self.plan, &self.parts, b))
        });
        plan
    }

    /// Each part as the owner reviews it, with its own operations and blockers.
    fn split(&self) -> Vec<ScopePlan> {
        SCOPES
            .iter()
            .map(|&scope| {
                let mut plan = self.plan.clone();
                plan.scope = scope.as_str().into();
                plan.operations.retain(|o| self.part(&o.id) == scope);
                plan.blockers
                    .retain(|b| blocker_part(&self.plan, &self.parts, b) == scope);
                plan.blockers.extend(
                    self.excluded
                        .waiting
                        .iter()
                        .filter(|_| scope == Scope::Services)
                        .cloned(),
                );
                let ex = &self.excluded;
                let mine = |ids: &BTreeSet<String>| -> BTreeSet<String> {
                    ids.iter()
                        .filter(|id| self.part(id) == scope)
                        .cloned()
                        .collect()
                };
                ScopePlan {
                    scope,
                    withheld: mine(&ex.withheld),
                    plan,
                    skipped: match scope {
                        Scope::Secrets => ex.skipped.clone(),
                        _ => Vec::new(),
                    },
                    no_session: match scope {
                        Scope::Secrets => ex.no_session.clone(),
                        _ => Vec::new(),
                    },
                    blocked: ex.blocked.contains(&scope),
                }
            })
            .collect()
    }
}

/// The part a blocker belongs to: that of its resource's kind, else configuration.
fn blocker_part(plan: &Plan, parts: &BTreeMap<String, Scope>, b: &Blocker) -> Scope {
    plan.operations
        .iter()
        .find(|o| o.resource == b.resource)
        .map_or(Scope::Config, |o| apply_cmd::part_of(parts, &o.id))
}

/// The part of each operation of a combined plan: the scope of its resource's kind, and
/// configuration for a removal (its resource is no longer declared). A directory belongs to the
/// earliest part that needs it, so it is left out only with every part that needs it; the plan
/// lists it before the operations that run after it.
fn parts(plan: &Plan, resolved: &Resolved) -> BTreeMap<String, Scope> {
    let mut out: BTreeMap<String, Scope> = BTreeMap::new();
    for o in plan.operations.iter().rev() {
        let own = resolved
            .resources
            .get(&o.resource)
            .and_then(|r| Scope::of(&r.kind))
            .unwrap_or(Scope::Config);
        let part = match o.action {
            Action::CreateDir => plan
                .operations
                .iter()
                .filter(|d| d.after.contains(&o.id))
                .filter_map(|d| out.get(&d.id).copied())
                .min_by_key(|s| s.rank())
                .unwrap_or(own),
            _ => own,
        };
        out.insert(o.id.clone(), part);
    }
    out
}

/// What a run of the combined plan leaves out (docs/sync.md, "Approval" and "The four scopes").
#[derive(Default)]
struct Excluded {
    /// The adoptions and takeovers without their `--allow-adopt` id, and what runs after them.
    /// A later part does not wait for a withheld adoption: HOME already holds exactly what it
    /// would record.
    withheld: BTreeSet<String>,
    /// The planned secret resources, when no 1Password session exists: this run skips them.
    skipped: Vec<String>,
    /// The accounts without a session, each with what `op whoami` answered.
    no_session: Vec<(String, String)>,
    /// The packages, secrets or services parts with a blocker that this run cannot clear: the
    /// part does not run and the other parts do. A configuration blocker refuses the run.
    blocked: Vec<Scope>,
    /// A service whose prerequisite file only an operation this run leaves out (skipped or
    /// blocked, not withheld) publishes: it blocks the services part.
    waiting: Vec<Blocker>,
}

impl Excluded {
    /// `sessions` answers the session check for the secrets part without its withheld
    /// operations (`secret::missing_sessions`). The review asks it only when that part does
    /// more than render again (`rerenders_only`); the run, which renders them, always asks.
    fn of(
        plan: &Plan,
        parts: &BTreeMap<String, Scope>,
        allow_adopt: &BTreeSet<String>,
        rerenders: bool,
        sessions: impl FnOnce(&Plan) -> Result<Vec<(String, String)>, Refusal>,
    ) -> Result<Excluded, Refusal> {
        let part = |id: &str| apply_cmd::part_of(parts, id);
        let action = |id: &str| {
            plan.operations
                .iter()
                .find(|o| o.id == id)
                .map(|o| o.action)
        };
        let mut ex = Excluded::default();
        for b in &plan.blockers {
            let p = blocker_part(plan, parts, b);
            if p != Scope::Config && !ex.blocked.contains(&p) {
                ex.blocked.push(p);
            }
        }
        // The plan lists an operation after the ones it runs after, so one pass takes the chains.
        for o in plan
            .operations
            .iter()
            .filter(|o| o.status == PlanStatus::Planned)
        {
            let unapproved =
                Allowance::of(o) == Some(Allowance::Adopt) && !allow_adopt.contains(&o.id);
            let after = o.after.iter().any(|a| {
                ex.withheld.contains(a)
                    && (part(a) == part(&o.id) || action(a) != Some(Action::Adopt))
            });
            if unapproved || after {
                ex.withheld.insert(o.id.clone());
            }
        }
        let mut secrets = apply_cmd::without(plan, &ex.withheld);
        secrets.operations.retain(|o| part(&o.id) == Scope::Secrets);
        secrets.blockers.retain(|b| {
            plan.operations
                .iter()
                .any(|o| o.resource == b.resource && part(&o.id) == Scope::Secrets)
        });
        // Re-renders alone are not worth a vault contact on a run that changes nothing else.
        if rerenders || !rerenders_only(&secrets) {
            ex.no_session = sessions(&secrets)?;
        }
        if !ex.no_session.is_empty() {
            ex.skipped = planned_secrets(&secrets);
        }
        // A planned service whose missing file only a skipped or blocked operation publishes
        // cannot start.
        let gone: BTreeSet<&str> = plan
            .operations
            .iter()
            .filter(|o| {
                (part(&o.id) == Scope::Secrets && !ex.skipped.is_empty())
                    || ex.blocked.contains(&part(&o.id))
            })
            .map(|o| o.id.as_str())
            .collect();
        for o in plan.operations.iter().filter(|o| {
            o.status == PlanStatus::Planned
                && part(&o.id) == Scope::Services
                && !ex.blocked.contains(&Scope::Services)
        }) {
            // Only a file absent at plan time waits (`produced-by:`); a file in place
            // (`present-or-produced-by:`) stands when its producer does not run.
            for pc in &o.preconditions {
                let Some(by) = pc.state.strip_prefix(dfm_model::plan::PRODUCED_BY) else {
                    continue;
                };
                if gone.contains(by) {
                    let path = pc.subject.split_once(':').map(|(_, p)| p.to_string());
                    ex.waiting.push(Blocker {
                        code: Code::Conflict,
                        resource: o.resource.clone(),
                        source: String::new(),
                        field: None,
                        detail: format!(
                            "{} depends on {}, which only {by} publishes, and this run does not run it",
                            o.resource,
                            path.as_deref().unwrap_or("a file")
                        ),
                        path,
                        correction: format!(
                            "let {by} run (dfm sync or dfm {}), then run dfm sync again",
                            part(by).as_str()
                        ),
                    });
                }
            }
        }
        if !ex.waiting.is_empty() && !ex.blocked.contains(&Scope::Services) {
            ex.blocked.push(Scope::Services);
        }
        Ok(ex)
    }

    /// Every operation id the run leaves out.
    fn ids(&self, review: &Review) -> BTreeSet<String> {
        let mut ids = self.withheld.clone();
        ids.extend(
            review
                .plan
                .operations
                .iter()
                .filter(|o| {
                    let p = review.part(&o.id);
                    self.blocked.contains(&p) || (p == Scope::Secrets && !self.skipped.is_empty())
                })
                .map(|o| o.id.clone()),
        );
        ids
    }
}

/// One part of the combined plan as the owner reviews it.
struct ScopePlan {
    scope: Scope,
    /// The part's operations and blockers.
    plan: Plan,
    /// The planned secret resources, when no 1Password session exists: this run skips them.
    skipped: Vec<String>,
    /// The accounts without a session, each with what `op whoami` answered.
    no_session: Vec<(String, String)>,
    /// A packages, secrets or services part with a blocker that this run cannot clear.
    blocked: bool,
    /// The part's withheld operations (`Excluded::withheld`).
    withheld: BTreeSet<String>,
}

impl ScopePlan {
    /// The plan as the owner reviews it: the part's own operations (a `not_in_scope` one is in
    /// no part's scope).
    fn reviewed(&self) -> Plan {
        let mut own = self.plan.clone();
        own.operations
            .retain(|o| o.status != PlanStatus::NotInScope);
        own
    }
    /// Each blocked operation with its code, or the resource when a blocker names no operation.
    fn blocked_ops(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for b in &self.plan.blockers {
            let mut ids: Vec<String> = self
                .plan
                .operations
                .iter()
                .filter(|o| o.resource == b.resource && o.status == PlanStatus::Blocked)
                .map(|o| format!("{} {}", o.id, b.code.as_str()))
                .collect();
            if ids.is_empty() {
                ids.push(format!("{} {}", b.resource, b.code.as_str()));
            }
            for id in ids {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    /// The part without its withheld operations: what it runs.
    fn runnable(&self) -> Plan {
        apply_cmd::without(&self.plan, &self.withheld)
    }

    /// Whether the scope has nothing to run, withheld operations aside.
    fn changes_nothing(&self) -> bool {
        self.runnable().changes_nothing()
    }

    /// The summary status of a scope with nothing to run.
    fn idle(&self) -> String {
        match self.withheld.is_empty() {
            true => "current".into(),
            false => format!("withheld ({})", visible(&self.withheld_ids().join(", "))),
        }
    }

    /// The withheld operations in plan order.
    fn withheld_ids(&self) -> Vec<String> {
        self.plan
            .operations
            .iter()
            .filter(|o| self.withheld.contains(&o.id))
            .map(|o| o.id.clone())
            .collect()
    }

    /// The end-of-output warnings of a skipped, blocked or withheld scope (docs/sync.md). A
    /// secrets scope can be both blocked and skipped: the blocked resources, with their
    /// corrections, and the secrets skipped for want of a session each get a warning, since
    /// unlocking clears only the second.
    fn warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = self.blocked_warning().into_iter().collect();
        out.extend(self.withheld_warning());
        if !self.skipped.is_empty() {
            out.push(skipped_warning(
                &self.skipped,
                &self.no_session,
                "run dfm sync or dfm secrets again",
            ));
        }
        out
    }

    /// The warning of a scope that runs without its withheld operations, if any.
    fn withheld_warning(&self) -> Option<String> {
        if self.blocked || self.withheld.is_empty() {
            return None;
        }
        let flags: Vec<String> = self
            .plan
            .operations
            .iter()
            .filter(|o| self.withheld.contains(&o.id) && Allowance::of(o) == Some(Allowance::Adopt))
            .map(|o| dfm_engine::flag(Allowance::Adopt, &o.id))
            .collect();
        let ids = visible(&self.withheld_ids().join(", "));
        Some(match flags.is_empty() {
            // Each runs after an operation an earlier scope withheld; that scope's warning
            // names its flag.
            true => format!(
                "{} withheld: {ids}, after a withheld operation of an earlier scope. Fix: approve that operation, then run dfm sync again",
                self.scope.as_str()
            ),
            false => format!(
                "{} withheld: {ids}. Fix: review each, then run dfm sync {}",
                self.scope.as_str(),
                visible_for_paste(&flags.join(" "))
            ),
        })
    }

    fn blocked_warning(&self) -> Option<String> {
        if !self.blocked {
            return None;
        }
        let mut fixes: Vec<&str> = Vec::new();
        for b in &self.plan.blockers {
            if !fixes.contains(&b.correction.as_str()) {
                fixes.push(&b.correction);
            }
        }
        Some(format!(
            "{} blocked: {}. Fix: {}, or run dfm {} on its own",
            self.scope.as_str(),
            visible(&self.blocked_ops().join(", ")),
            visible(&fixes.join("; ")),
            self.scope.as_str()
        ))
    }
}

/// The warning of skipped secrets: the resources, each account with what `op whoami` answered,
/// and the command that renders them once 1Password is unlocked.
fn skipped_warning(names: &[String], no_session: &[(String, String)], then: &str) -> String {
    let accounts: Vec<String> = no_session
        .iter()
        .map(|(a, why)| format!("{a} ({why})"))
        .collect();
    format!(
        "secrets skipped, no 1Password session for {}: {}. Fix: unlock 1Password, then {then}",
        visible(&accounts.join(", ")),
        visible(&names.join(", "))
    )
}

/// Whether `id` is a planned operation of `plan` that needs this kind of approval.
fn needs(plan: &Plan, kind: Allowance, id: &str) -> bool {
    plan.operations
        .iter()
        .any(|o| o.id == id && o.status == PlanStatus::Planned && Allowance::of(o) == Some(kind))
}

/// The arguments of the combined run: the sync flags, with only the approvals that name an
/// operation of `plan` (an id outside it is a usage error in `check_before_lock`), as a composed
/// run (`apply_cmd::Args::composed`).
fn run_args(args: &Args, plan: &Plan) -> apply_cmd::Args {
    let own = |given: &BTreeSet<String>, kind| {
        given
            .iter()
            .filter(|id| needs(plan, kind, id))
            .cloned()
            .collect()
    };
    apply_cmd::Args {
        yes_plan: None,
        allow: own(&args.apply.allow, Allowance::Disruptive),
        allow_adopt: own(&args.apply.allow_adopt, Allowance::Adopt),
        scope: Scope::Sync,
        restart: args.restart.clone(),
        composed: true,
        unattended: args.apply.yes_plan.is_none(),
        ..args.apply.clone()
    }
}

/// Resolves and plans the staged tree `tree` as the tree `root` will hold after publication, as
/// one combined plan (`Scope::Sync`): the sources are read from `tree`, the plan names `root`. A
/// service definition is read where the configuration part leaves it: at the target of its
/// planned link, or from the bytes a planned operation generates, unless that operation is
/// withheld. Then the secrets part runs the session check of `secrets`: without a session its
/// planned resources are skipped, not refused. After publication `tree` is `root`.
fn plan_staged(
    ctx: &Ctx,
    binding: &Binding,
    (root, rel): (&Path, &str),
    tree: &Path,
    args: &Args,
) -> Result<(Loaded, Review), Refusal> {
    let sources = load_scoped(tree, rel)?;
    let resolved = resolve(&sources, rel, &builtin_catalog())?;
    let mut loaded = Loaded {
        binding: binding.clone(),
        root: root.to_path_buf(),
        staged: Some(tree.to_path_buf()),
        links: BTreeMap::new(),
        generated: BTreeMap::new(),
        sources,
        resolved,
    };
    if let Some(d) = crate::machine_mismatch(ctx, &loaded) {
        return Err(Refusal::new(Code::PlatformMismatch, d));
    }
    let sys = ctx.sys()?;
    // What the configuration part leaves in HOME, for the services probe. A withheld file
    // operation writes nothing: the probe reads HOME at its path. A withheld link is an adoption
    // of the exact link that HOME already holds, so its target stays in `links`.
    let (config, mut generated) =
        plan_with_outputs(ctx, &loaded, Scope::Config, Default::default())?;
    let withheld = apply_cmd::withheld(&run_args(args, &config), &config);
    let unwritten: BTreeSet<String> = config
        .operations
        .iter()
        .filter(|o| withheld.contains(&o.id))
        .filter_map(|o| o.path.as_deref().map(dfm_model::fold::fold))
        .collect();
    generated.retain(|path, _| !unwritten.contains(path));
    loaded.generated = generated;
    // A planning input, not a post-run retarget: the services probe reads each planned link's
    // staged target before anything runs.
    loaded.links = config
        .operations
        .iter()
        .filter(|o| o.status == PlanStatus::Planned)
        .filter_map(|o| Some((dfm_model::fold::fold(o.path.as_ref()?), o.link.clone()?)))
        .collect();
    let probed = apply_cmd::probe(ctx, &sys, &run_args(args, &config), &loaded)?;
    let plan = plan_for(ctx, &loaded, Scope::Sync, probed)?;
    apply_cmd::check_restart(&args.restart, &plan)?;
    let parts = parts(&plan, &loaded.resolved);
    let runner = ctx.runner(&sys);
    // After publication `tree` is `root`: the run renders what the review only re-renders.
    let excluded = Excluded::of(
        &plan,
        &parts,
        &args.apply.allow_adopt,
        tree == root,
        |secrets| secret::missing_sessions(secrets, &loaded.resolved, &runner),
    )?;
    let review = Review {
        digest: plan.digest()?,
        plan,
        parts,
        excluded,
    };
    Ok((loaded, review))
}

/// Whether every planned operation of a secrets plan only renders again a destination the
/// ledger proves the manager wrote: the plan cannot tell an unchanged result from a changed one
/// before the render (docs/secrets.md). `sync` approves nothing for such a plan alone. A
/// destination whose observed mode is not the declared mode is a change the plan can see.
fn rerenders_only(plan: &Plan) -> bool {
    plan.blockers.is_empty()
        && plan.operations.iter().all(|o| {
            o.status != PlanStatus::Planned
                || (o.adapter == secret::ADAPTER
                    && o.action == Action::UpdateFile
                    && o.ownership == Ownership::Owned
                    && !o.takeover
                    && o.path.as_ref().is_some_and(|path| {
                        let want = format!("file:{:o}", o.mode.unwrap_or(0o600));
                        o.preconditions
                            .iter()
                            .any(|pc| pc.subject == format!("path:{path}") && pc.state == want)
                    }))
        })
}

/// The planned secret resources of a secrets plan, each once.
fn planned_secrets(plan: &Plan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for o in plan
        .operations
        .iter()
        .filter(|o| o.status == PlanStatus::Planned && o.adapter == secret::ADAPTER)
    {
        if !out.contains(&o.resource) {
            out.push(o.resource.clone());
        }
    }
    out
}

/// The combined preview: each part's plan under its scope heading, the secrets this run skips
/// and the parts it does not run, then the one digest that approves the whole run.
fn show(
    review: &Review,
    parts: &[ScopePlan],
    digest: &str,
    color: bool,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    for p in parts {
        writeln!(
            out,
            "{} {}",
            paint::label(color, "scope:"),
            p.scope.as_str()
        )?;
        render_body(&p.reviewed(), color, out)?;
        for r in &p.skipped {
            writeln!(
                out,
                "{} {}",
                paint::style(color, "33", "skipped: no session"),
                visible(r)
            )?;
        }
        if p.blocked {
            writeln!(
                out,
                "{} this scope does not run: {}",
                paint::style(color, "1;31", "blocked:"),
                visible(&p.blocked_ops().join(", "))
            )?;
        }
        for o in p
            .plan
            .operations
            .iter()
            .filter(|o| p.withheld.contains(&o.id))
        {
            // The withheld operations of an earlier part that this one runs after.
            let by: Vec<&str> = o
                .after
                .iter()
                .filter(|a| review.excluded.withheld.contains(*a) && review.part(a) != p.scope)
                .map(String::as_str)
                .collect();
            let why = match Allowance::of(o) {
                Some(Allowance::Adopt) => "needs --allow-adopt".to_string(),
                _ if !by.is_empty() => format!("runs after withheld {}", by.join(", ")),
                _ => "runs after a withheld operation".to_string(),
            };
            writeln!(
                out,
                "{} {} ({why})",
                paint::style(color, "33", "withheld:"),
                visible(&o.id)
            )?;
        }
    }
    writeln!(
        out,
        "{} {}",
        paint::label(color, "plan digest:"),
        paint::style(color, "35", digest)
    )
}

/// The checks `apply` runs before its lock (`apply_cmd::check_before_lock`) on the operations
/// this run will run, with their approvals: one plan, so the journal capacity covers them all.
/// An approval id that names no operation of the combined plan is a usage error. Returns whether
/// the journal holds open work.
fn check(
    ctx: &Ctx,
    args: &Args,
    review: &Review,
    loaded: &Loaded,
    out: &mut dyn Write,
) -> Result<bool, Refusal> {
    for kind in Allowance::ALL {
        let given = match kind {
            Allowance::Disruptive => &args.apply.allow,
            Allowance::Adopt => &args.apply.allow_adopt,
        };
        if let Some(bad) = given.iter().find(|id| !needs(&review.plan, kind, id)) {
            return Err(usage(format!(
                "{} {bad} names no planned {} in any scope of this sync",
                kind.flag(),
                kind.noun()
            )));
        }
    }
    let sys = ctx.sys()?;
    let runnable = review.runnable();
    let generator = apply_cmd::generators_for(ctx, &sys, Scope::Sync, loaded);
    apply_cmd::check_before_lock(
        ctx,
        &run_args(args, &runnable),
        &runnable,
        &review.digest,
        &generator,
        out,
    )
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

/// What was printed so far, so a refusal prints its `result:` line exactly once, and the
/// warnings of the scopes the run skipped or did not run, which end the output.
#[derive(Default)]
struct Printed {
    plan: bool,
    result: bool,
    warnings: Vec<String>,
}

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let mut printed = Printed::default();
    let r = run_inner(ctx, args, out, &mut printed);
    if let Err(e) = &r
        && printed.plan
        && !printed.result
    {
        let _ = writeln!(
            out,
            "{} {}",
            paint::label(ctx.color, "result:"),
            paint::status(ctx.color, e.exit().name())
        );
    }
    // Last, so the owner sees them without scrolling (docs/sync.md, "The four scopes"). A
    // refused run ends with its refusal instead. A stopped run still names what did not finish.
    let stopped = r.as_ref().err().is_some_and(|e| {
        matches!(
            e.exit(),
            dfm_model::Exit::Interrupted | dfm_model::Exit::Terminated
        )
    });
    for warning in printed.warnings.iter().filter(|_| r.is_ok() || stopped) {
        let _ = writeln!(
            out,
            "{} {warning}",
            paint::style(ctx.color, "1;33", "warning:")
        );
    }
    r
}

/// The per-scope lines and the `result:` line that end a run.
fn finish(
    ctx: &Ctx,
    lines: &[(Scope, String)],
    upgrade: Option<&str>,
    result: Result<(), Refusal>,
    out: &mut dyn Write,
    printed: &mut Printed,
) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    writeln!(out, "{}", paint::label(ctx.color, "scopes:")).map_err(w)?;
    for (scope, status) in lines {
        writeln!(out, "  {}: {status}", scope.as_str()).map_err(w)?;
    }
    if let Some(u) = upgrade {
        writeln!(out, "  upgrade: {u}").map_err(w)?;
    }
    let exit = result
        .as_ref()
        .err()
        .map_or(dfm_model::Exit::Ok, Refusal::exit);
    writeln!(
        out,
        "{} {}",
        paint::label(ctx.color, "result:"),
        paint::status(ctx.color, exit.name())
    )
    .map_err(w)?;
    printed.result = true;
    result
}

/// The summary status of a scope that does not run, if it does not.
fn not_run(p: &ScopePlan) -> Option<String> {
    if p.blocked {
        return Some(format!(
            "blocked ({})",
            visible(&p.blocked_ops().join(", "))
        ));
    }
    (!p.skipped.is_empty())
        .then(|| format!("skipped: no session ({})", visible(&p.skipped.join(", "))))
}

fn run_inner(
    ctx: &Ctx,
    args: &Args,
    out: &mut dyn Write,
    printed: &mut Printed,
) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = require_binding(ctx)?;
    let (root, rel) = canonical_root(&binding, args.apply.root.as_deref().map(Path::new))?;
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    let live_lock = read_lock(&root)?;
    let root_src = live_lock
        .root()
        .ok_or_else(|| untrusted(format!("{LOCK_FILE} declares no root source")))?;

    // Trust first, before any fetch (docs/sync.md). A grant is written only for an
    // origin the owner names on the command line, under the writer lock and journaled.
    let readable = Store::open_read(sys, &layout, state_cmd::fail_from_env()?)?
        .ok_or_else(crate::state_lost_refusal)?;
    readable.require_identity(&binding)?;
    let mut grants = readable.read_grants()?;
    drop(readable);
    for o in &args.trust {
        check_identity(o, cfg!(feature = "test-isolation"))?;
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

    // The review pass holds the shared lock while its staging directory exists, so
    // a writer that starts meanwhile is refused and no writer sweeps this run.
    let shared = Store::probe_writer(&sys, &layout)?;
    // Copy the checkout into staging, rehearse the fetch and fast-forward there, then plan the
    // staged tree as the tree --root will hold.
    let git = Git::new(ctx, &sys);
    let staging = Staging::create(ctx, &sys, false)?;
    let previewed = stage_all(&git, root_src, &grants, &root, &staging.tree())?;
    for s in &previewed {
        // A root checkout on another branch is fast-forwarded where it is; say so.
        let checkout = match &s.branch {
            _ if !s.src.is_root() => String::new(),
            Some(b) if *b == s.src.reference => String::new(),
            Some(b) => format!("; checkout on {}", visible(b)),
            None => "; detached".into(),
        };
        writeln!(
            out,
            "{} {} {} {} {} ({}{checkout})",
            paint::label(ctx.color, "source:"),
            s.src.id,
            s.src.origin,
            s.src.reference,
            s.change(),
            s.kind()
        )
        .map_err(w)?;
    }
    let (loaded, review) = plan_staged(ctx, &binding, (&root, &rel), &staging.tree(), args)?;
    let digest = sync_digest(&review, &previewed);
    let parts = review.split();
    printed.warnings = parts.iter().flat_map(ScopePlan::warnings).collect();
    show(&review, &parts, &digest, ctx.color, out).map_err(w)?;
    printed.plan = true;
    // A digest that no longer matches is the first thing to report: the reviewer approved a
    // different run.
    if let Some(approved) = &args.apply.yes_plan
        && *approved != digest
    {
        return Err(apply_cmd::stale(approved, &digest));
    }
    // The checks apply runs before its lock (docs/apply.md), so a plan that the run would
    // refuse is refused here before any source is published.
    let journal_open = check(ctx, args, &review, &loaded, out)?;
    // The updaters, a blocked one included, are listed before any part runs (DFM-31.C02).
    let updaters = &loaded.resolved.updaters;
    if args.apply.preview || (args.upgrade && !updaters.is_empty()) {
        upgrade_preview(ctx, &sys, (&root, &grants), args, updaters, out)?;
    }
    if args.apply.preview {
        return Ok(());
    }
    // Without a digest the run approves the plan it printed (DFM-76). A run that grants an
    // identity stops here, also with nothing to change: the owner reviews what the new source
    // brings first, the plan and the updater listing above, and no updater runs (DFM-25).
    if args.apply.yes_plan.is_none() && !args.trust.is_empty() {
        return Err(Refusal::new(
            Code::ApprovalRequired,
            "nothing was published and no updater ran. Fix: read the plan above, then run dfm sync",
        ));
    }
    // A source that moves is a publication, so only a run with every source current, and no
    // part that would run with a change, can end here (docs/apply.md, step 4).
    if args.apply.yes_plan.is_none()
        && !journal_open
        && previewed.iter().all(|s| s.old.as_ref() == Some(&s.new))
        && parts.iter().all(|p| {
            not_run(p).is_some()
                || p.changes_nothing()
                || (p.scope == Scope::Secrets && rerenders_only(&p.runnable()))
        })
    {
        writeln!(
            out,
            "{}",
            paint::style(ctx.color, "1;32", "nothing to change: no approval needed")
        )
        .map_err(w)?;
        let lines: Vec<(Scope, String)> = parts
            .iter()
            .map(|p| {
                let status = match not_run(p) {
                    Some(s) => s,
                    None if p.changes_nothing() => p.idle(),
                    None => "current, not rendered again (dfm secrets renders them)".into(),
                };
                (p.scope, status)
            })
            .collect();
        // The review's shared lock and staging end here: the upgrade step takes the writer lock.
        drop(staging);
        drop(shared);
        let (up, result) = match upgrade(
            ctx,
            &sys,
            &binding,
            (&root, &rel),
            args,
            &loaded.resolved.updaters,
            out,
            printed,
        ) {
            // A stop request during the updaters decides the exit (docs/upgrade.md).
            Ok(up) => (up, dfm_engine::signal::requested().map_or(Ok(()), Err)),
            Err(e) => (Some(format!("failed ({})", e.code.as_str())), Err(e)),
        };
        return finish(ctx, &lines, up.as_deref(), result, out, printed);
    }
    drop(staging);
    drop(shared);
    state_cmd::fail_from_env()?.hit("sync-before-lock")?;

    // One writer lock for the rest of the run: fetch and plan again, so the sources and the
    // checkouts are rechecked immediately before anything is published, fast-forward each
    // checkout, plan the published tree, and run the plan.
    let fail = state_cmd::fail_from_env()?;
    let store = Store::open_write_with(sys, &layout, fail.clone(), false)?;
    if !store.is_initialized() {
        return Err(crate::state_lost_refusal());
    }
    store.require_identity(&binding)?;
    let second = Staging::create(ctx, &sys, true)?;
    let staged = stage_all(&git, root_src, &grants, &root, &second.tree())?;
    let (_, locked) = plan_staged(ctx, &binding, (&root, &rel), &second.tree(), args)?;
    let locked_digest = sync_digest(&locked, &staged);
    if locked_digest != digest {
        return Err(match &args.apply.yes_plan {
            Some(approved) => apply_cmd::stale(approved, &locked_digest),
            None => Refusal::new(
                Code::PlanStale,
                format!(
                    "the plan changed after it was printed: its digest was {digest} and is {locked_digest} now; nothing was published. Fix: run dfm sync again"
                ),
            ),
        });
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
        match fail
            .hit(&format!("sync-before-publish-{}", s.src.id))
            .and_then(|()| publish(&git, s))
        {
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
                    "sync: published {}; {} was not published; no scope was run",
                    published.join(", "),
                    s.src.id
                )
                .map_err(w)?;
                return Err(e.after_mutation());
            }
        }
    }
    drop(second);
    let changed = !published.is_empty();
    let after = |e: Refusal| if changed { e.after_mutation() } else { e };

    // The published tree must give the approved run (docs/sync.md, step 6). Then the session
    // check runs again: a session that ended since the review skips the secrets now.
    let (now_loaded, now) =
        plan_staged(ctx, &binding, (&root, &rel), &root, args).map_err(after)?;
    // A part that does not run may say why in other words once its files are published (a
    // definition the copy could not read is not published now): what runs must be equal.
    let what_runs = now
        .runnable_with(&locked.excluded)
        .digest()
        .map_err(after)?;
    if what_runs != locked.runnable().digest().map_err(after)? {
        return Err(after(Refusal::new(
            Code::PlanStale,
            "the plan of the published tree is not the approved one; nothing was run. Fix: run dfm sync again",
        )));
    }
    // The approved run, with the secrets skipped now when the session ended since the review. A
    // session that started since does not widen it.
    let mut run = locked;
    if run.excluded.skipped.is_empty() && !now.excluded.skipped.is_empty() {
        // A plain sync would not render these again (`rerenders_only`), so the fix names
        // `secrets`.
        printed.warnings.push(skipped_warning(
            &now.excluded.skipped,
            &now.excluded.no_session,
            "run dfm secrets, which renders them",
        ));
        run.excluded = now.excluded;
    }
    let parts = run.split();
    let runnable = run.runnable();
    // A run that changes nothing in any part (an approved repeat) writes no receipt.
    let runs = |p: &ScopePlan| not_run(p).is_none() && (!p.changes_nothing() || journal_open);
    let mut failure: Option<Refusal> = dfm_engine::signal::requested();
    let mut statuses: BTreeMap<String, state::Status> = BTreeMap::new();
    if failure.is_none() && parts.iter().any(runs) {
        let ran = apply_cmd::execute_locked(
            ctx,
            &run_args(args, &runnable),
            &binding,
            &apply_cmd::Locked {
                store: &store,
                fail: &fail,
                loaded: &now_loaded,
                plan: &runnable,
                digest: &run.digest,
                parts: Some(&run.parts),
            },
            out,
        );
        match ran {
            Ok(r) => {
                statuses.extend(r.statuses);
                failure = r.outcome.err();
            }
            Err(e) => failure = Some(e),
        }
    }
    drop(store);
    // A stop request decides the exit, also one that came during the report.
    if let Some(r) = dfm_engine::signal::requested() {
        failure = Some(r);
    }
    let lines = summary(
        &parts,
        &statuses,
        failure.as_ref(),
        journal_open,
        &run.parts,
    );
    let changed = changed || statuses.values().any(|s| *s == state::Status::Updated);
    // The updaters run after every part that ran, and not after a refusal (docs/upgrade.md).
    let up = match failure {
        Some(_) => None,
        None => match upgrade(
            ctx,
            &sys,
            &binding,
            (&root, &rel),
            args,
            &now_loaded.resolved.updaters,
            out,
            printed,
        ) {
            Ok(up) => {
                failure = dfm_engine::signal::requested();
                up
            }
            // The parts already ran: the summary still prints, and the exit is the error's.
            Err(e) => {
                let up = format!("failed ({})", e.code.as_str());
                failure = Some(e);
                Some(up)
            }
        },
    };
    let result = match failure {
        Some(e) if changed => Err(e.after_mutation()),
        Some(e) => Err(e),
        None => Ok(()),
    };
    finish(ctx, &lines, up.as_deref(), result, out, printed)
}

/// The `scopes:` lines of a run: per part, why it did not run, or how its operations ended. The
/// part with the first operation that did not complete reports the run's failure; a part whose
/// own operations did not all complete did not start. `of` is the part of each operation id.
fn summary(
    parts: &[ScopePlan],
    statuses: &BTreeMap<String, state::Status>,
    failure: Option<&Refusal>,
    journal_open: bool,
    of: &BTreeMap<String, Scope>,
) -> Vec<(Scope, String)> {
    use state::Status as S;
    let part = |id: &str| apply_cmd::part_of(of, id);
    let failed_part = failure.and_then(|_| {
        let first = |want: &dyn Fn(S) -> bool| {
            statuses
                .iter()
                .filter(|(_, s)| want(**s))
                .map(|(id, _)| part(id))
                .min_by_key(|s| s.rank())
        };
        first(&|s| {
            matches!(
                s,
                S::Failed | S::Blocked | S::Unknown | S::Pending | S::BlockedPending
            )
        })
        .or_else(|| first(&|s| s == S::NotStarted))
    });
    // The run's failure is reported once, by the part it came from.
    let mut reported = false;
    let ran = statuses.values().any(|s| *s != S::NotStarted);
    parts
        .iter()
        .map(|p| {
            let mine: Vec<S> = statuses
                .iter()
                .filter(|(id, _)| part(id) == p.scope)
                .map(|(_, s)| *s)
                .collect();
            let none_started = !mine.is_empty() && mine.iter().all(|s| *s == S::NotStarted);
            let status = if let Some(s) = not_run(p) {
                s
            } else if p.changes_nothing() && !journal_open {
                p.idle()
            } else if let Some(e) =
                failure.filter(|_| !reported && failed_part.is_none_or(|f| f == p.scope))
            {
                reported = true;
                if none_started && ran {
                    "not_started".into()
                } else {
                    // `blocked` is the word for a part that did not run (C10), not a failure.
                    let word = match e.exit() {
                        dfm_model::Exit::Blocked => "failed",
                        exit => exit.name(),
                    };
                    format!("{word} ({})", e.code.as_str())
                }
            } else if failure.is_some()
                && !mine
                    .iter()
                    .all(|s| matches!(s, S::Updated | S::Current | S::Held | S::NotInScope))
            {
                // Its operations did not start, or waited for one that did not complete.
                "not_started".into()
            } else {
                "ok".into()
            };
            (p.scope, status)
        })
        .collect()
}

// ---- the upgrade step (docs/upgrade.md) ------------------------------------------------------

fn status_name(s: state::Status) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The updater environment: the provider runner, HOME, the bound checkout and the grants.
fn updater_env<'a>(
    ctx: &'a Ctx,
    runner: &'a Runner,
    root: &'a Path,
    grants: &'a Grants,
) -> updater::Env<'a> {
    updater::Env {
        provider: runner,
        home: ctx.platform.home(),
        root,
        timeouts: updater::Timeouts::DEFAULT,
        grants,
    }
}

/// `--preview`: what the upgrade step would run. Nothing runs and nothing is written.
fn upgrade_preview(
    ctx: &Ctx,
    sys: &Sys,
    (root, grants): (&Path, &Grants),
    args: &Args,
    updaters: &[Updater],
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    if !args.upgrade {
        return writeln!(out, "upgrade: skipped (--no-upgrade)").map_err(w);
    }
    if updaters.is_empty() {
        return writeln!(out, "upgrade: none declared in the profile").map_err(w);
    }
    let runner = ctx.runner(sys);
    let env = updater_env(ctx, &runner, root, grants);
    writeln!(out, "{}", paint::label(ctx.color, "upgrade:")).map_err(w)?;
    for u in updaters {
        writeln!(
            out,
            "  {}: {}",
            u.name(),
            visible(&updater::preview(*u, &env))
        )
        .map_err(w)?;
    }
    Ok(())
}

/// Runs the profile's updaters in order under the writer lock, prints one row each, writes one
/// receipt and adds a warning per updater that did not succeed. `reviewed` is the list the
/// review resolved. Another sync may change the profile before the lock, so under the lock the
/// profile in `root` is resolved again, and an updater runs only when the review listed it and
/// the profile still declares it: one added meanwhile runs in the next sync, after its review.
/// Returns the summary for the `scopes:` block, or `None` when the step printed only why it did
/// not run.
#[expect(
    clippy::too_many_arguments,
    reason = "the step needs the binding, the root to read the profile again, and the run's output"
)]
fn upgrade(
    ctx: &Ctx,
    sys: &Sys,
    binding: &Binding,
    (root, rel): (&Path, &str),
    args: &Args,
    reviewed: &[Updater],
    out: &mut dyn Write,
    printed: &mut Printed,
) -> Result<Option<String>, Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    if !args.upgrade {
        writeln!(out, "upgrade: skipped (--no-upgrade)").map_err(w)?;
        return Ok(Some("skipped (--no-upgrade)".into()));
    }
    // A profile that declares none takes no lock.
    if reviewed.is_empty() {
        writeln!(out, "upgrade: none declared in the profile").map_err(w)?;
        return Ok(None);
    }
    state_cmd::fail_from_env()?.hit("sync-before-upgrade-lock")?;
    // A sync with nothing to change ran no scope, so nothing armed the handlers yet.
    if !dfm_engine::signal::installed() {
        dfm_engine::signal::install()?;
    }
    let layout = ctx.layout();
    let store = Store::open_write_with(*sys, &layout, state_cmd::fail_from_env()?, false)?;
    if !store.is_initialized() {
        return Err(crate::state_lost_refusal());
    }
    store.require_identity(binding)?;
    let declared = resolve(&load_scoped(root, rel)?, rel, &builtin_catalog())?.updaters;
    let updaters: Vec<Updater> = reviewed
        .iter()
        .filter(|u| declared.contains(u))
        .copied()
        .collect();
    if updaters.is_empty() {
        writeln!(out, "upgrade: none declared in the profile").map_err(w)?;
        return Ok(None);
    }
    // The grants as they are under the lock: the source check runs again just before each
    // updater, because a grant or the tool's sources may have changed since the review.
    let grants = store.read_grants()?;
    let started = now_ns()?;
    let mut runner = ctx.runner(sys);
    runner.detach = true;
    let env = updater_env(ctx, &runner, root, &grants);
    writeln!(out, "{}", paint::label(ctx.color, "upgrade:")).map_err(w)?;
    let mut ops = Vec::new();
    for u in &updaters {
        let row = updater::run(*u, &env);
        let status = status_name(row.status);
        let versions = match (&row.before, &row.after) {
            (Some(b), Some(a)) if a != b => format!(" {} -> {}", visible(b), visible(a)),
            (Some(b), _) => format!(" {}", visible(b)),
            _ => String::new(),
        };
        writeln!(
            out,
            "  {} {}{versions}",
            paint::status(ctx.color, &status),
            u.name()
        )
        .map_err(w)?;
        for n in &row.notes {
            writeln!(out, "    note: {}", visible(n)).map_err(w)?;
        }
        let problem = row.status.is_problem();
        if problem {
            printed.warnings.push(format!(
                "upgrade {} {status}: {}",
                u.name(),
                visible(&row.notes.join("; "))
            ));
        }
        let mut error = problem.then(|| row.notes.join("; "));
        if let Some(e) = &mut error
            && e.len() > state::MAX_ERROR_BYTES
        {
            let mut cut = state::MAX_ERROR_BYTES;
            while !e.is_char_boundary(cut) {
                cut -= 1;
            }
            e.truncate(cut);
        }
        ops.push(state::OperationRecord {
            id: format!("updater-{}", u.name()),
            resource: format!("updater:{}", u.name()),
            adapter: "updater/v1".into(),
            class: Class::Normal,
            status: row.status,
            started_at: Some(row.started_at),
            finished_at: Some(row.finished_at),
            before: None,
            after: None,
            owned_outputs: vec![],
            error,
            reload: None,
        });
    }
    // After the loop, so a copy that brew upgraded shows the version brew left. A stopped run
    // starts no more commands.
    let stopped = dfm_engine::signal::requested().is_some();
    for u in updaters.iter().filter(|_| !stopped) {
        for n in updater::second_copies(*u, &env) {
            writeln!(out, "  note: {} {}", u.name(), visible(&n)).map_err(w)?;
        }
    }
    let binary = crate::plan_cmd::binary_identity()?;
    let stamp = state::Stamp::of(binding);
    let receipt = state::Receipt {
        schema: state::SCHEMA,
        op_id: format!("upgrade-{started}"),
        command: "upgrade".into(),
        update: false,
        started_at: rfc3339(started),
        finished_at: rfc3339(now_ns()?),
        binary: state::BinaryId {
            version: binary.version,
            commit: option_env!("DFM_BUILD_COMMIT").unwrap_or("unknown").into(),
            sha256: binary.sha256,
        },
        profile: state::ProfileRef {
            id: stamp.profile,
            binding_digest: stamp.binding_digest,
        },
        machine_id: stamp.machine_id,
        plan_digest: None,
        approval: None,
        sources: vec![],
        approvals: vec![],
        result: state::RunResult::from_operations(&ops),
        operations: ops,
    };
    let name = store.write_receipt(&receipt)?;
    writeln!(out, "receipt: {}", visible(&name)).map_err(w)?;
    let counts: Vec<String> = receipt
        .result
        .counts
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(s, n)| format!("{}={n}", status_name(*s)))
        .collect();
    Ok(Some(counts.join(" ")))
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
    fn redirects_and_http_alternates_stay_disabled() {
        // Any value but `true` also disables the dumb-HTTP alternates (is_alternate_allowed).
        let follow: Vec<&&str> = GIT_SETTINGS
            .iter()
            .filter(|s| s.starts_with("http.followRedirects="))
            .collect();
        assert_eq!(follow, [&"http.followRedirects=false"]);
    }

    #[test]
    fn trust_origin_cannot_be_combined_with_preview() {
        let p = |v: &[&str]| crate::cli::parse("sync", v).and_then(|m| parse_args(&m));
        let d = "0".repeat(64);
        let usage = Some(Code::Usage);
        let args = ["--root", "/r", "--preview", "--trust-origin", "https://h/x"];
        assert_eq!(p(&args).err().map(|e| e.code), usage);
        let args = [
            "--root",
            "/r",
            "--yes-plan",
            &d,
            "--trust-origin",
            "https://h/x",
        ];
        assert_eq!(p(&args).err().map(|e| e.code), usage);
        let args = ["-y", &d, "--trust-origin", "https://h/x"];
        assert_eq!(p(&args).err().map(|e| e.code), usage);
        let args = ["--root", "/r", "--trust-origin", "https://h/x"];
        assert_eq!(p(&args).unwrap().trust, ["https://h/x"]);
    }
}
