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

use crate::adapters::secret;
use crate::plan_cmd::{plan_for, render_body};
use crate::read_fs::read_regular_bounded;
use crate::source_fs::load_scoped;
use crate::{
    Ctx, Loaded, apply_cmd, builtin_catalog, canonical_root, paint, require_binding, state_cmd,
    usage,
};
use dfm_engine::{
    Approval as Allowance, Dir, End, Lock as FileLock, ProcRequest, Runner, Store, Sys, now_ns,
};
use dfm_model::binding::Binding;
use dfm_model::digest::sha256_hex;
use dfm_model::display::{visible, visible_for_paste};
use dfm_model::lock::{Grants, Lock, LockSource, MAX_BYTES as LOCK_MAX_BYTES, check_origin};
use dfm_model::plan::{
    Action, Blocker, LinkTo, Node, Operation, Ownership, Plan, Scope, Seen, Status as PlanStatus,
    node_state,
};
use dfm_model::resolve::resolve;
use dfm_model::state::{self, Class, JournalEntry, Phase, Recovery, rfc3339};
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
    /// Origins to grant before the fetch (`--trust-origin`), each named in the lock.
    trust: Vec<String>,
    /// `--restart <resource-id>`, for the services scope.
    restart: BTreeSet<String>,
}

/// `sync [--root <dir>] [--preview] [--yes-plan <digest>] [--allow-disruptive <id>]...
/// [--allow-adopt <id>]... [--restart <resource-id>]... [--trust-origin <origin>]...`
pub fn parse_args(args: &[String]) -> Result<Args, Refusal> {
    let (mut trust, mut restart, mut rest) = (Vec::new(), BTreeSet::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--trust-origin" || a == "--restart" {
            let v = it
                .next()
                .ok_or_else(|| usage(format!("{a} needs a value")))?;
            if a == "--restart" {
                restart.insert(v.clone());
            } else {
                trust.push(v.clone());
            }
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
    Ok(Staged {
        src: src.clone(),
        repo,
        live,
        old,
        new,
        ahead_of,
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

/// The digest the owner approves: each scope's plan digest, the secrets the run skips for want
/// of a session and, per source, what moves from where to where. A plan digest covers the
/// loaded source tree, not the commits behind it, so a commit that changes only files outside
/// the scoped roots would otherwise pass.
fn sync_digest(plans: &[ScopePlan], staged: &[Staged]) -> String {
    let mut text = String::new();
    for p in plans {
        text.push_str(&format!("plan {} {}\n", p.scope.as_str(), p.digest));
        for r in &p.skipped {
            text.push_str(&format!("skip {} {r}\n", p.scope.as_str()));
        }
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

/// The scopes of one run, in run order (docs/sync.md).
const SCOPES: [Scope; 4] = [
    Scope::Config,
    Scope::Packages,
    Scope::Secrets,
    Scope::Services,
];

/// One scope's plan of the staged tree.
struct ScopePlan {
    scope: Scope,
    plan: Plan,
    digest: String,
    /// The planned secret resources, when no 1Password session exists: this run skips them.
    skipped: Vec<String>,
    /// The accounts without a session, each with what `op whoami` answered.
    no_session: Vec<(String, String)>,
    /// A packages, secrets or services plan with a blocker that this run cannot clear: the
    /// scope does not run and the other scopes do. A blocked configuration plan refuses the run.
    blocked: bool,
    /// Services blocked only because their definition or configuration file is not published
    /// yet, while the configuration plan publishes it: they are planned after that scope. Each
    /// is reviewed as a start, or as a restart when `--restart` names it.
    deferred: BTreeMap<String, Action>,
}

impl ScopePlan {
    /// The plan as the owner reviews it: the scope's own operations (a `not_in_scope` one
    /// belongs to another scope), and each deferred service shown as the start or the requested
    /// restart it becomes.
    fn reviewed(&self, config: &Plan) -> Plan {
        let mut own = self.plan.clone();
        own.operations
            .retain(|o| o.status != PlanStatus::NotInScope);
        for o in own
            .operations
            .iter_mut()
            .filter(|o| self.deferred.contains_key(&o.id))
        {
            let by: Vec<String> = self
                .plan
                .blockers
                .iter()
                .filter(|b| b.resource == o.resource)
                .filter_map(|b| Some(format!("{} ({})", b.path.as_ref()?, publisher(config, b)?)))
                .collect();
            o.status = PlanStatus::Planned;
            o.action = self.deferred[&o.id];
            o.effect = match o.action {
                Action::Restart => format!(
                    "restart {} on request (--restart) after the configuration scope publishes {}; disruptive; planned again from the published files before it restarts",
                    o.resource,
                    by.join(", ")
                ),
                _ => format!(
                    "start {} after the configuration scope publishes {}; planned again from the published files before it starts",
                    o.resource,
                    by.join(", ")
                ),
            };
        }
        own.blockers.retain(|b| publisher(config, b).is_none());
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

    /// The end-of-output warning of a skipped or blocked scope, if any (docs/sync.md).
    fn warning(&self) -> Option<String> {
        if !self.skipped.is_empty() {
            return Some(skipped_warning(
                &self.skipped,
                &self.no_session,
                "run dfm sync or dfm secrets again",
            ));
        }
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

/// The planned configuration operation that publishes the path of a service's "not published"
/// blocker, if one does.
fn publisher<'a>(config: &'a Plan, b: &Blocker) -> Option<&'a str> {
    let path = b.path.as_deref()?;
    (b.code == Code::Conflict)
        .then(|| {
            config
                .operations
                .iter()
                .find(|o| o.status == PlanStatus::Planned && o.path.as_deref() == Some(path))
        })
        .flatten()
        .map(|o| o.id.as_str())
}

/// The arguments of one scope's run: the sync flags, with only the approvals that name an
/// operation of this scope's plan (an id outside it is a usage error there), and `--restart`
/// for services only.
fn scope_args(args: &Args, scope: Scope, plan: Option<&Plan>) -> apply_cmd::Args {
    let own = |given: &BTreeSet<String>, kind| {
        given
            .iter()
            .filter(|id| plan.is_some_and(|p| needs(p, kind, id)))
            .cloned()
            .collect()
    };
    apply_cmd::Args {
        yes_plan: None,
        allow: own(&args.apply.allow, Allowance::Disruptive),
        allow_adopt: own(&args.apply.allow_adopt, Allowance::Adopt),
        scope,
        restart: match scope {
            Scope::Services => args.restart.clone(),
            _ => BTreeSet::new(),
        },
        composed: true,
        ..args.apply.clone()
    }
}

/// Whether `id` is a planned operation of `plan` that needs this kind of approval.
fn needs(plan: &Plan, kind: Allowance, id: &str) -> bool {
    plan.operations
        .iter()
        .any(|o| o.id == id && o.status == PlanStatus::Planned && Allowance::of(o) == Some(kind))
}

/// Plans one scope of the tree `loaded` holds as its own command plans it, with the providers
/// probed read-only.
fn plan_scope(
    ctx: &Ctx,
    sys: &Sys,
    args: &Args,
    scope: Scope,
    loaded: &Loaded,
) -> Result<Plan, Refusal> {
    let probed = apply_cmd::probe(ctx, sys, &scope_args(args, scope, None), loaded)?;
    let plan = plan_for(ctx, loaded, scope, probed)?;
    if scope == Scope::Services {
        apply_cmd::check_restart(&args.restart, &plan)?;
    }
    Ok(plan)
}

/// Resolves and plans the staged tree as the tree `root` will hold after publication: the
/// sources are read from `tree`, the plans name `root`. The secrets scope then runs the session
/// check of `secrets`: without a session its planned resources are skipped, not refused.
fn plan_staged(
    ctx: &Ctx,
    binding: &Binding,
    root: &Path,
    rel: &str,
    tree: &Path,
    args: &Args,
) -> Result<(Loaded, Vec<ScopePlan>), Refusal> {
    let sources = load_scoped(tree, rel)?;
    let resolved = resolve(&sources, rel, &builtin_catalog())?;
    let mut loaded = Loaded {
        binding: binding.clone(),
        root: root.to_path_buf(),
        staged: Some(tree.to_path_buf()),
        links: BTreeMap::new(),
        sources,
        resolved,
    };
    if let Some(d) = crate::machine_mismatch(ctx, &loaded) {
        return Err(Refusal::new(Code::PlatformMismatch, d));
    }
    let sys = ctx.sys()?;
    let mut plans: Vec<ScopePlan> = Vec::new();
    for scope in SCOPES {
        let plan = plan_scope(ctx, &sys, args, scope, &loaded)?;
        if scope == Scope::Config {
            loaded.links = plan
                .operations
                .iter()
                .filter(|o| o.status == PlanStatus::Planned)
                .filter_map(|o| Some((o.path.clone()?, o.link.clone()?)))
                .collect();
        }
        // Re-renders alone are not worth a vault contact on a run that changes nothing else.
        let no_session = match scope {
            Scope::Secrets if !rerenders_only(&plan) => {
                secret::missing_sessions(&plan, &loaded.resolved, &ctx.runner(&sys))?
            }
            _ => Vec::new(),
        };
        let skipped = match no_session.is_empty() {
            true => Vec::new(),
            false => planned_secrets(&plan),
        };
        let (mut blocked, mut deferred) = (false, BTreeMap::new());
        if let (Scope::Packages | Scope::Secrets | Scope::Services, Some(config)) =
            (scope, plans.first())
        {
            for b in &plan.blockers {
                match publisher(&config.plan, b) {
                    Some(_) if scope == Scope::Services => {
                        let action = match args.restart.contains(&b.resource) {
                            true => Action::Restart,
                            false => Action::Start,
                        };
                        deferred.insert(format!("{}@service", b.resource), action);
                    }
                    _ => blocked = true,
                }
            }
            if blocked {
                deferred.clear();
            }
        }
        plans.push(ScopePlan {
            scope,
            digest: plan.digest()?,
            plan,
            skipped,
            no_session,
            blocked,
            deferred,
        });
    }
    Ok((loaded, plans))
}

/// Whether every planned operation of a secrets plan only renders again a destination the
/// ledger proves the manager wrote: the plan cannot tell an unchanged result from a changed one
/// before the render (docs/secrets.md). `sync` approves nothing for such a plan alone.
fn rerenders_only(plan: &Plan) -> bool {
    plan.blockers.is_empty()
        && plan.operations.iter().all(|o| {
            o.status != PlanStatus::Planned
                || (o.adapter == secret::ADAPTER
                    && o.action == Action::UpdateFile
                    && o.ownership == Ownership::Owned
                    && !o.takeover)
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

/// The combined preview: each scope's reviewed plan under its heading, the secrets this run
/// skips and the scopes it does not run, then the one digest that approves the whole run.
fn show(
    plans: &[ScopePlan],
    digest: &str,
    color: bool,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let config = &plans[0].plan;
    for p in plans {
        writeln!(
            out,
            "{} {}",
            paint::label(color, "scope:"),
            p.scope.as_str()
        )?;
        render_body(&p.reviewed(config), color, out)?;
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
    }
    writeln!(
        out,
        "{} {}",
        paint::label(color, "plan digest:"),
        paint::style(color, "35", digest)
    )
}

/// The checks `apply` runs before its lock (`apply_cmd::check_before_lock`), per scope with that
/// scope's approvals, for the scopes that will run (a deferred service is checked when its scope
/// plans it again; only the approval of a deferred restart is checked here, before publication).
/// An approval id that names no operation of any reviewed plan is a usage error. Missing
/// approvals of every scope are printed as one list before the refusal. Returns whether the
/// journal holds open work.
fn check_scopes(
    ctx: &Ctx,
    args: &Args,
    plans: &[ScopePlan],
    loaded: &Loaded,
    out: &mut dyn Write,
) -> Result<bool, Refusal> {
    for kind in Allowance::ALL {
        let given = match kind {
            Allowance::Disruptive => &args.apply.allow,
            Allowance::Adopt => &args.apply.allow_adopt,
        };
        if let Some(bad) = given.iter().find(|id| {
            !plans
                .iter()
                .any(|p| needs(&p.reviewed(&plans[0].plan), kind, id))
        }) {
            return Err(usage(format!(
                "{} {bad} names no planned {} in any scope of this sync",
                kind.flag(),
                kind.noun()
            )));
        }
    }
    let sys = ctx.sys()?;
    let (mut open, mut missing) = (false, None);
    for p in plans.iter().filter(|p| !p.blocked && p.skipped.is_empty()) {
        let mut plan = p.plan.clone();
        plan.operations.retain(|o| !p.deferred.contains_key(&o.id));
        if !p.deferred.is_empty() {
            plan.blockers.clear();
        }
        let generator = apply_cmd::generators_for(ctx, &sys, p.scope, loaded);
        let scoped = scope_args(args, p.scope, Some(&p.plan));
        let restarts: Vec<String> = p
            .deferred
            .iter()
            .filter(|(_, a)| **a == Action::Restart)
            .map(|(id, _)| id.clone())
            .collect();
        let approved: BTreeSet<String> = args
            .apply
            .allow
            .iter()
            .filter(|id| restarts.contains(id))
            .cloned()
            .collect();
        match apply_cmd::check_before_lock(ctx, &scoped, &plan, &p.digest, &generator, out) {
            Ok(o) => open |= o,
            Err(e) if matches!(e.code, Code::DisruptiveUnapproved | Code::AdoptUnapproved) => {
                missing.get_or_insert(e);
            }
            Err(e) => return Err(e),
        }
        if let Err(e) = dfm_engine::check_allow(Allowance::Disruptive, &restarts, &approved) {
            writeln!(out, "{}", visible_for_paste(&e.detail)).map_err(|e| {
                Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()))
            })?;
            missing.get_or_insert(e);
        }
    }
    missing.map_or(Ok(open), Err)
}

/// Whether `fresh`, the plan of scope `p` after the earlier scopes of this run, is the reviewed
/// plan with the effects of those scopes' approved operations (`done`, by folded path) applied:
/// a directory an earlier scope created drops out of the reviewed plan, and a `path:` or
/// `sensitive-path:` precondition on a path an earlier scope wrote must state what that
/// operation leaves there (`left_by`); so must a `content:` precondition on a file it rendered.
/// The reviewed preconditions on those paths described them before the earlier scopes. Each
/// deferred service is now planned as a start (or current, when the launcher already holds it;
/// or a restart, when the owner requested and approved the restart before publication). Every
/// other operation and precondition, the sources and the model are unchanged, and nothing
/// blocks. The observations are left out: each operation's preconditions carry the states it
/// depends on.
fn as_reviewed(
    p: &ScopePlan,
    fresh: &Plan,
    done: &BTreeMap<String, Operation>,
    home: &Path,
) -> bool {
    let started = p.deferred.iter().all(|(id, reviewed)| {
        fresh.operations.iter().any(|o| {
            o.id == *id
                && (o.status == PlanStatus::Current
                    || (o.status == PlanStatus::Planned
                        && (o.action == Action::Start || o.action == *reviewed)))
        })
    });
    // The earlier operation that changed the subject of this precondition, if one did. The
    // content of a link is the file it points at, which the review already read at the new
    // target (`Staged`), so a `content:` precondition through a link stays as reviewed.
    fn changed_by<'a>(
        done: &'a BTreeMap<String, Operation>,
        subject: &'a str,
    ) -> Option<(&'a str, &'a Operation)> {
        let (kind, path) = subject.split_once(':')?;
        let op = done.get(&dfm_model::fold::fold(path))?;
        match kind {
            "path" | "sensitive-path" => Some((kind, op)),
            "content" if op.link.is_none() => Some((kind, op)),
            _ => None,
        }
    }
    let created = |o: &Operation| {
        o.action == Action::CreateDir
            && o.path.as_deref().is_some_and(|path| {
                done.get(&dfm_model::fold::fold(path))
                    .is_some_and(|d| d.action == Action::CreateDir)
            })
    };
    let rest = |plan: &Plan, fresh: bool| {
        let mut c = plan.clone();
        let gone: BTreeSet<String> = match fresh {
            true => BTreeSet::new(),
            false => c
                .operations
                .iter()
                .filter(|o| created(o))
                .map(|o| o.id.clone())
                .collect(),
        };
        c.operations
            .retain(|o| !p.deferred.contains_key(&o.id) && !gone.contains(&o.id));
        for o in &mut c.operations {
            o.after.retain(|id| !gone.contains(id));
            o.preconditions
                .retain(|pc| match changed_by(done, &pc.subject) {
                    None => true,
                    Some(_) if !fresh => false,
                    Some((kind, op)) => !left_by(op, kind, &pc.state, home),
                });
        }
        c.blockers.clear();
        c.observations = Default::default();
        c
    };
    started && fresh.blockers.is_empty() && rest(&p.plan, false) == rest(fresh, true)
}

/// Whether `state`, a precondition of kind `kind` (`path`, `sensitive-path` or `content`) on
/// the path of `op`, is what the approved operation `op` of an earlier scope leaves there: the
/// node apply makes (a directory of mode 700 or the declared mode, the absence it removed to,
/// the link it published, the file it rendered with the declared mode or 600), formatted by
/// `node_state` as observe records it. The rendered content is read again from `home`, because
/// a precondition records a different hash of it, and must be the approved content.
fn left_by(op: &Operation, kind: &str, state: &str, home: &Path) -> bool {
    let writes = matches!(op.action, Action::CreateFile | Action::UpdateFile);
    if kind == "content" {
        return writes
            && op
                .output_sha256
                .as_ref()
                .is_some_and(|h| state == format!("sha256:{h}"));
    }
    let is = |node| state == node_state(&Seen { node, alias: None }, kind == "sensitive-path");
    let file = |sha256, size| Node::File {
        mode: op.mode.unwrap_or(0o600),
        own: true,
        size,
        sha256,
    };
    match (op.action, op.link.as_deref()) {
        (Action::CreateDir, _) => is(Node::Dir {
            mode: op.mode.unwrap_or(0o700),
            own: true,
        }),
        (Action::Remove, _) => is(Node::Missing {}),
        (_, Some(link)) => link_to(Path::new(link)).is_some_and(|to| {
            is(Node::Link {
                target: link.into(),
                to,
                own: true,
            })
        }),
        _ if !writes => false,
        // A sensitive path is recorded by mode only: its content is never read.
        _ if kind == "sensitive-path" => is(file(None, 0)),
        // Hashed, or by size when observe was not asked for a hash.
        _ => rendered(op, home)
            .is_some_and(|(fp, size)| is(file(Some(fp), size)) || is(file(None, size))),
    }
}

/// What observe records a link to the absolute `target` points at (`fsys::Peek`).
/// shortcut: a link loop is not told apart from another error and never matches, upgrade when
/// a published link to a loop must count as the approved effect.
fn link_to(target: &Path) -> Option<LinkTo> {
    match fs::metadata(target) {
        Ok(m) if m.is_dir() => Some(LinkTo::Dir),
        Ok(m) if m.is_file() => Some(LinkTo::File),
        Ok(_) => Some(LinkTo::Other),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Some(LinkTo::Dangling)
        }
        Err(_) => None,
    }
}

/// The fingerprint (`file_fingerprint`) and size of the file at the path of `op` under `home`,
/// if its content is the approved content: the SHA-256 the plan recorded for it.
fn rendered(op: &Operation, home: &Path) -> Option<(String, u64)> {
    let (path, sha, limit) = (
        op.path.as_ref()?,
        op.output_sha256.as_ref()?,
        op.output_bytes?,
    );
    let bytes = read_regular_bounded(
        &home.join(path),
        limit,
        |e| io_failed("read", &e),
        || conflict("not a regular file"),
    )
    .ok()?;
    (sha256_hex(&bytes) == *sha).then(|| {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        (dfm_model::plan::file_fingerprint(&bytes), size)
    })
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
    // refused run ends with its refusal instead.
    for warning in printed.warnings.iter().filter(|_| r.is_ok()) {
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

    // The review pass holds the shared lock while its staging directory exists, so
    // a writer that starts meanwhile is refused and no writer sweeps this run.
    let shared = Store::probe_writer(&sys, &layout)?;
    // Copy the checkout into staging, rehearse the fetch and fast-forward there, then plan the
    // staged tree as the tree --root will hold.
    let git = Git::new(ctx, &sys);
    let staging = Staging::create(ctx, &sys, false)?;
    let previewed = stage_all(&git, root_src, &grants, &root, &staging.tree())?;
    for s in &previewed {
        writeln!(
            out,
            "{} {} {} {} {} ({})",
            paint::label(ctx.color, "source:"),
            s.src.id,
            s.src.origin,
            s.src.reference,
            s.change(),
            s.kind()
        )
        .map_err(w)?;
    }
    let (loaded, plans) = plan_staged(ctx, &binding, &root, &rel, &staging.tree(), args)?;
    let digest = sync_digest(&plans, &previewed);
    printed.warnings = plans.iter().filter_map(ScopePlan::warning).collect();
    show(&plans, &digest, ctx.color, out).map_err(w)?;
    printed.plan = true;
    // A digest that no longer matches is the first thing to report: the reviewer approved a
    // different run.
    if let Some(approved) = &args.apply.yes_plan
        && *approved != digest
    {
        return Err(apply_cmd::stale(approved, &digest));
    }
    // The checks apply runs before its lock (docs/apply.md), per scope, so a plan that a scope
    // would refuse is refused here before any source is published.
    let journal_open = check_scopes(ctx, args, &plans, &loaded, out)?;
    if args.apply.preview {
        return Ok(());
    }
    // A source that moves is a publication, so only a run with every source current, and no
    // scope that would run with a change, can end here (docs/apply.md, step 4).
    if args.apply.yes_plan.is_none()
        && !journal_open
        && previewed.iter().all(|s| s.old.as_ref() == Some(&s.new))
        && plans.iter().all(|p| {
            not_run(p).is_some()
                || p.plan.changes_nothing()
                || (p.scope == Scope::Secrets && rerenders_only(&p.plan))
        })
    {
        writeln!(
            out,
            "{}",
            paint::style(ctx.color, "1;32", "nothing to change: no approval needed")
        )
        .map_err(w)?;
        let lines: Vec<(Scope, String)> = plans
            .iter()
            .map(|p| {
                let status = match not_run(p) {
                    Some(s) => s,
                    None if p.plan.changes_nothing() => "current".into(),
                    None => "current, not rendered again (dfm secrets renders them)".into(),
                };
                (p.scope, status)
            })
            .collect();
        return finish(ctx, &lines, Ok(()), out, printed);
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
    let (locked_loaded, locked) = plan_staged(ctx, &binding, &root, &rel, &second.tree(), args)?;
    let locked_digest = sync_digest(&locked, &staged);
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
    drop(store);

    // Each scope runs as its own command would, with its plan digest, which the sync digest
    // covers: it plans --root (now the published tree), compares, takes the lock, plans again,
    // runs and writes its own receipt (docs/apply.md). A scope that fails stops the scopes after
    // it. A skipped or blocked scope does not run and stops nothing.
    let mut failure: Option<Refusal> = None;
    let mut changed = !published.is_empty();
    let mut lines: Vec<(Scope, String)> = Vec::new();
    // The approved operations of the scopes that ran, by folded path: a later scope is checked
    // against their effects.
    let mut done = BTreeMap::new();
    let fail = state_cmd::fail_from_env()?;
    for p in &locked {
        // A stop request between two scopes ends the run: no later scope starts.
        if failure.is_none()
            && let Some(r) = dfm_engine::signal::requested()
        {
            failure = Some(r);
        }
        let status = if failure.is_some() {
            "not_started".to_string()
        } else if let Some(s) = not_run(p) {
            s
        } else if p.plan.changes_nothing() && !journal_open {
            "current".into()
        } else {
            writeln!(
                out,
                "{} {}",
                paint::label(ctx.color, "scope:"),
                p.scope.as_str()
            )
            .map_err(w)?;
            let earlier = Earlier {
                done: &done,
                home: &layout.home,
            };
            let ran = run_scope(ctx, &sys, args, p, &locked_loaded, &earlier, out)
                .and_then(|ran| fail.hit("sync-after-scope").map(|()| ran));
            match ran {
                Ok(None) => {
                    changed = true;
                    for o in p.plan.operations.iter() {
                        if let (PlanStatus::Planned, Some(path)) = (o.status, &o.path) {
                            done.insert(dfm_model::fold::fold(path), o.clone());
                        }
                    }
                    "ok".into()
                }
                // The session ended after the review. A plain sync would not render these again
                // (`rerenders_only`), so the fix names `secrets`.
                Ok(Some(no_session)) => {
                    let names = planned_secrets(&p.plan);
                    printed.warnings.push(skipped_warning(
                        &names,
                        &no_session,
                        "run dfm secrets, which renders them",
                    ));
                    format!("skipped: no session ({})", visible(&names.join(", ")))
                }
                Err(e) => {
                    // `blocked` is the word for a scope that did not run (C10), not a failure.
                    let word = match e.exit() {
                        dfm_model::Exit::Blocked => "failed",
                        exit => exit.name(),
                    };
                    let status = format!("{word} ({})", e.code.as_str());
                    failure = Some(e);
                    status
                }
            }
        };
        lines.push((p.scope, status));
    }
    // A stop request decides the exit, also one that came during the last scope's report.
    if let Some(r) = dfm_engine::signal::requested() {
        failure = Some(r);
    }
    let result = match failure {
        Some(e) if changed => Err(e.after_mutation()),
        Some(e) => Err(e),
        None => Ok(()),
    };
    finish(ctx, &lines, result, out, printed)
}

/// What the scopes that already ran in this sync did (see `run_inner`).
struct Earlier<'a> {
    done: &'a BTreeMap<String, Operation>,
    home: &'a Path,
}

/// Runs one approved scope through its own command (`apply_cmd::run`) with its plan digest.
/// Returns the accounts without a session when the secrets scope finds none now: it is skipped.
/// A scope after an earlier scope that changed something is planned again first, from the
/// published tree: it runs only when that plan is the reviewed one with the earlier scopes'
/// effects applied (`as_reviewed`), with the digest of that plan.
fn run_scope(
    ctx: &Ctx,
    sys: &Sys,
    args: &Args,
    p: &ScopePlan,
    loaded: &Loaded,
    earlier: &Earlier<'_>,
    out: &mut dyn Write,
) -> Result<Option<Vec<(String, String)>>, Refusal> {
    if p.scope == Scope::Secrets {
        let no_session = secret::missing_sessions(&p.plan, &loaded.resolved, &ctx.runner(sys))?;
        if !no_session.is_empty() {
            return Ok(Some(no_session));
        }
    }
    // A service deferred to this point always follows a configuration scope that ran.
    let fresh = match p.scope {
        Scope::Config => None,
        _ if earlier.done.is_empty() => None,
        _ => {
            let now = crate::load_resolved(ctx, args.apply.root.as_deref().map(Path::new))?;
            let scoped = scope_args(args, p.scope, None);
            let probed = apply_cmd::probe(ctx, sys, &scoped, &now)?;
            let fresh = plan_for(ctx, &now, p.scope, probed)?;
            if !as_reviewed(p, &fresh, earlier.done, earlier.home) {
                return Err(Refusal::new(
                    Code::PlanStale,
                    format!(
                        "the {} plan after the earlier scopes of this sync is not the reviewed one. Fix: run dfm sync again and approve the new plan",
                        p.scope.as_str()
                    ),
                ));
            }
            Some(fresh)
        }
    };
    // A deferred restart is an operation of the new plan only: its approval goes with it.
    let approvals = fresh.as_ref().unwrap_or(&p.plan);
    let scoped = apply_cmd::Args {
        yes_plan: Some(match &fresh {
            Some(f) => f.digest()?,
            None => p.digest.clone(),
        }),
        ..scope_args(args, p.scope, Some(approvals))
    };
    // The scope's first look is the recheck just made: the plan under its lock is compared
    // with that digest.
    let scoped = apply_cmd::Args {
        planned: fresh,
        ..scoped
    };
    apply_cmd::run(ctx, &scoped, out).map(|()| None)
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
    use dfm_model::plan::{Binary, Observations, Operation, Precondition};

    fn pre(subject: &str, state: &str) -> Precondition {
        Precondition {
            subject: subject.into(),
            state: state.into(),
        }
    }

    /// A services plan with one start of `life.agent`, whose definition is `PLIST`.
    fn services_plan(preconditions: Vec<Precondition>) -> Plan {
        Plan {
            schema: 1,
            command: "plan".into(),
            scope: "services".into(),
            binary: Binary {
                version: "0".into(),
                sha256: "0".repeat(64),
            },
            profile: "/p".into(),
            machine_id: "m".into(),
            model_digest: "0".repeat(64),
            sources: vec![],
            observations: Observations::default(),
            operations: vec![Operation {
                id: "life.agent@service".into(),
                journal_id: "j".into(),
                resource: "life.agent".into(),
                module: "life".into(),
                adapter: "launchd-agent/v1".into(),
                class: dfm_model::state::Class::Normal,
                action: Action::Start,
                status: PlanStatus::Planned,
                path: None,
                mode: None,
                link: None,
                privilege: "user".into(),
                effect: "start org.example.agent".into(),
                ownership: Ownership::Absent,
                inputs: vec![],
                fragments: vec![],
                fragment_dir: None,
                output_sha256: None,
                managed_sha256: None,
                takeover: false,
                output_bytes: None,
                excluded: None,
                sets: vec![],
                after: vec![],
                preconditions,
                postcondition: String::new(),
            }],
            blockers: vec![],
        }
    }

    const PLIST: &str = "Library/LaunchAgents/org.example.agent.plist";

    fn reviewed(plan: Plan) -> ScopePlan {
        ScopePlan {
            scope: Scope::Services,
            digest: plan.digest().unwrap(),
            plan,
            skipped: vec![],
            no_session: vec![],
            blocked: false,
            deferred: BTreeMap::new(),
        }
    }

    fn before() -> Vec<Precondition> {
        vec![
            pre(&format!("path:{PLIST}"), "link:/r/old.plist"),
            pre(&format!("content:{PLIST}"), "sha256:aa"),
            pre(
                "probe:launchd-agent/v1:org.example.agent",
                "stopped:not loaded",
            ),
        ]
    }

    /// An approved operation of an earlier scope at `path`.
    fn earlier(path: &str, action: Action, link: Option<&str>) -> (String, Operation) {
        let mut o = services_plan(vec![]).operations.remove(0);
        o.id = format!("x@{path}");
        o.action = action;
        o.path = Some(path.into());
        o.link = link.map(String::from);
        (dfm_model::fold::fold(path), o)
    }

    #[test]
    fn a_later_scope_is_checked_against_the_approved_effects_of_the_earlier_scopes() {
        let home = Path::new("/nonexistent-home");
        let review = reviewed(services_plan(before()));
        let done: BTreeMap<String, Operation> =
            [earlier(PLIST, Action::ReplaceLink, Some("/r/new.plist"))].into();
        let mut after = before();
        // `/r/new.plist` does not exist here, so observe records the link as dangling.
        after[0].state = "link:/r/new.plist:dangling".into();
        let fresh = services_plan(after.clone());
        // 1. The definition link is the one the configuration scope published: accepted.
        assert!(as_reviewed(&review, &fresh, &done, home));
        let mut kind = after.clone();
        kind[0].state = "link:/r/new.plist:file".into();
        assert!(!as_reviewed(&review, &services_plan(kind), &done, home));
        // 2. Another target than the approved one, or no earlier write of the path: refused.
        let mut other = after.clone();
        other[0].state = "link:/r/other.plist:file".into();
        assert!(!as_reviewed(&review, &services_plan(other), &done, home));
        assert!(!as_reviewed(&review, &fresh, &BTreeMap::new(), home));
        let elsewhere: BTreeMap<String, Operation> = [earlier(
            ".config/other",
            Action::ReplaceLink,
            Some("/r/new.plist"),
        )]
        .into();
        assert!(!as_reviewed(&review, &fresh, &elsewhere, home));
        // 3. The content through a link stays as reviewed (read at the new target).
        let mut content = after.clone();
        content[1].state = "sha256:bb".into();
        assert!(!as_reviewed(&review, &services_plan(content), &done, home));
        // 4. Any other difference is refused, even with the definition written.
        let mut probe = after.clone();
        probe[2].state = "current:loaded".into();
        assert!(!as_reviewed(&review, &services_plan(probe), &done, home));
        let mut status = services_plan(after.clone());
        status.operations[0].status = PlanStatus::Current;
        assert!(!as_reviewed(&review, &status, &done, home));
        let mut action = services_plan(after.clone());
        action.operations[0].action = Action::Restart;
        assert!(!as_reviewed(&review, &action, &done, home));
        let mut effect = services_plan(after.clone());
        effect.operations[0].effect = "start another program".into();
        assert!(!as_reviewed(&review, &effect, &done, home));
        let mut extra = services_plan(after.clone());
        extra.operations[0]
            .preconditions
            .push(pre(&format!("path:{PLIST}.d"), "missing"));
        assert!(!as_reviewed(&review, &extra, &done, home));
        // 5. A deferred service may come back as a restart only when it was reviewed as one.
        let mut deferred = reviewed(services_plan(before()));
        let id = deferred.plan.operations[0].id.clone();
        deferred.deferred.insert(id.clone(), Action::Start);
        assert!(!as_reviewed(&deferred, &action, &done, home));
        deferred.deferred.insert(id, Action::Restart);
        assert!(as_reviewed(&deferred, &action, &done, home));
    }

    #[test]
    fn a_directory_an_earlier_scope_created_drops_out_of_the_reviewed_plan() {
        let home = Path::new("/nonexistent-home");
        let dir = ".config/example";
        let mut review = services_plan(vec![pre(".config", "dir:755")]);
        let mut mkdir = earlier(dir, Action::CreateDir, None).1;
        mkdir.id = format!("mkdir@{dir}");
        mkdir.preconditions = vec![pre(&format!("path:{dir}"), "missing")];
        review.operations[0].after = vec![mkdir.id.clone()];
        review.operations.insert(0, mkdir.clone());
        let review = reviewed(review);
        let done: BTreeMap<String, Operation> = [(dfm_model::fold::fold(dir), mkdir)].into();
        let fresh = |state: &str| {
            services_plan(vec![
                pre(".config", "dir:755"),
                pre(&format!("path:{dir}"), state),
            ])
        };
        // apply creates a directory with mode 700.
        assert!(as_reviewed(&review, &fresh("dir:700"), &done, home));
        // The directory is not there now, or not as apply made it: the earlier effect did not
        // hold.
        assert!(!as_reviewed(&review, &fresh("missing"), &done, home));
        assert!(!as_reviewed(&review, &fresh("link:/x:dir"), &done, home));
        assert!(!as_reviewed(&review, &fresh("dir:755"), &done, home));
        // Without the earlier mkdir, the reviewed mkdir is still expected.
        assert!(!as_reviewed(
            &review,
            &fresh("dir:700"),
            &BTreeMap::new(),
            home
        ));
    }

    #[test]
    fn a_file_an_earlier_scope_rendered_must_hold_the_approved_content_and_mode() {
        let home = TempDirLite::new();
        let path = ".config/bar/barrc";
        let bytes = b"height=1\n";
        fs::create_dir_all(home.0.join(".config/bar")).unwrap();
        fs::write(home.0.join(path), bytes).unwrap();
        let (_, mut op) = earlier(path, Action::UpdateFile, None);
        op.mode = Some(0o644);
        op.output_sha256 = Some(sha256_hex(bytes));
        op.output_bytes = Some(bytes.len() as u64);
        let fp = dfm_model::plan::file_fingerprint(bytes);
        let ok = format!("file:644:sha256:{fp}");
        assert!(left_by(&op, "path", &ok, &home.0));
        assert!(!left_by(
            &op,
            "path",
            &format!("file:600:sha256:{fp}"),
            &home.0
        ));
        assert!(!left_by(&op, "path", "file:644:sha256:00", &home.0));
        assert!(left_by(
            &op,
            "content",
            &format!("sha256:{}", sha256_hex(bytes)),
            &home.0
        ));
        assert!(!left_by(&op, "content", "sha256:00", &home.0));
        assert!(left_by(&op, "sensitive-path", "file:644", &home.0));
        assert!(!left_by(&op, "sensitive-path", "file:600", &home.0));
        // A path observed without a hash records the size: the size of the approved content.
        assert!(left_by(&op, "path", "file:644:size:9", &home.0));
        assert!(!left_by(&op, "path", "file:644:size:10", &home.0));
        assert!(!left_by(&op, "path", "file:600:size:9", &home.0));
        // Another hand rewrote the file after the scope: the precondition is not the approved one.
        fs::write(home.0.join(path), b"height=2\n").unwrap();
        assert!(!left_by(&op, "path", &ok, &home.0));
    }

    /// A scratch directory removed on drop.
    struct TempDirLite(PathBuf);

    impl TempDirLite {
        fn new() -> Self {
            let d = std::env::temp_dir().join(format!(
                "dfm-sync-unit-{}-{}",
                std::process::id(),
                now_ns().unwrap()
            ));
            fs::create_dir_all(&d).unwrap();
            TempDirLite(d)
        }
    }

    impl Drop for TempDirLite {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
        let args: Vec<String> = ["-y", &"0".repeat(64), "--trust-origin", "https://h/x"]
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
