//! The updaters a profile declares (docs/upgrade.md): each one runs through its own owner's
//! updater, detached, and ends in exactly one status. Nothing here returns an error: a missing
//! tool, a failed command or a stop request is a row.

use dfm_engine::proc::{End, Output, Request, Runner};
use dfm_model::lock::{Grants, NPM, source_identity};
use dfm_model::state::Status;
use dfm_model::updater::Updater;
use dfm_model::{Code, Refusal};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

/// How long each command may run.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// `brew upgrade`.
    pub long: Duration,
    /// Every other command.
    pub short: Duration,
}

impl Timeouts {
    pub const DEFAULT: Timeouts = Timeouts {
        long: Duration::from_secs(30 * 60),
        short: Duration::from_secs(10 * 60),
    };
}

pub struct Env<'a> {
    /// The provider runner (`brew`, `mise`).
    pub provider: &'a Runner,
    pub home: &'a Path,
    /// The bound checkout (`--root` or the binding's root), which holds
    /// `scripts/install-agent-registry.sh`.
    pub root: &'a Path,
    pub timeouts: Timeouts,
    /// The grants that the `skills` and `agents` sources must hold (docs/upgrade.md, "Source
    /// trust").
    pub grants: &'a Grants,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub updater: Updater,
    pub status: Status,
    pub before: Option<String>,
    pub after: Option<String>,
    pub notes: Vec<String>,
    pub started_at: String,
    pub finished_at: String,
}

/// Where each native tool's own installer puts it, relative to HOME.
fn owner_dir(u: Updater) -> Option<&'static str> {
    match u {
        Updater::Claude => Some(".local/bin"),
        Updater::Codex => Some(".codex/packages/standalone/current/bin"),
        Updater::Opencode => Some(".opencode/bin"),
        Updater::Agents | Updater::Brew | Updater::Skills => None,
    }
}

/// The program and the update arguments of a native tool.
fn native_update(u: Updater) -> &'static str {
    match u {
        Updater::Opencode => "upgrade",
        _ => "update",
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn brew_env(auto_update: bool) -> Vec<(String, String)> {
    let mut v = vec![
        ("HOMEBREW_NO_ANALYTICS".to_string(), "1".to_string()),
        ("HOMEBREW_NO_ENV_HINTS".to_string(), "1".to_string()),
    ];
    if !auto_update {
        v.push(("HOMEBREW_NO_AUTO_UPDATE".to_string(), "1".to_string()));
    }
    v
}

fn now() -> String {
    dfm_model::state::rfc3339(dfm_engine::store::now_ns().unwrap_or(0))
}

/// Why a command did not succeed, or `None`. Provider standard error is withheld
/// (docs/secrets.md); its size is shown.
fn trouble(what: &str, out: &Output, timeout: Duration) -> Option<String> {
    match &out.end {
        End::Exited(0) => None,
        End::Exited(c) => Some(format!(
            "{what} exited {c}; its {} bytes of standard error are withheld. Run it by hand to see them",
            out.stderr.len()
        )),
        End::TimedOut => Some(format!("{what} did not finish within {timeout:?}")),
        End::Signaled => Some(format!("{what} was ended by a signal")),
        End::Stopped(_) => Some(format!("{what} was stopped")),
    }
}

/// Runs one command. `Err` is the note for a command that did not run or did not succeed, with
/// whether a stop request ended it.
fn call(
    r: &Runner,
    program: &str,
    a: &[String],
    env: &[(String, String)],
    timeout: Duration,
) -> Result<Output, (String, bool)> {
    let what = std::iter::once(program)
        .chain(a.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let out = r
        .run(&Request {
            program,
            args: a,
            env,
            stdin: None,
            timeout,
            max_stdout: 16 << 20,
        })
        .map_err(|e| (format!("{what}: {}", e.detail), false))?;
    match trouble(&what, &out, timeout) {
        None => Ok(out),
        Some(n) => Err((n, matches!(out.end, End::Stopped(_)))),
    }
}

struct Draft {
    u: Updater,
    started_at: String,
    before: Option<String>,
    after: Option<String>,
    notes: Vec<String>,
}

impl Draft {
    fn end(self, status: Status) -> Row {
        Row {
            updater: self.u,
            status,
            before: self.before,
            after: self.after,
            notes: self.notes,
            started_at: self.started_at,
            finished_at: now(),
        }
    }

    /// A command that did not succeed: `pending` when a stop request ended it, else `failed`.
    fn stop(mut self, (note, stopped): (String, bool)) -> Row {
        self.notes.push(note);
        self.end(if stopped {
            Status::Pending
        } else {
            Status::Failed
        })
    }
}

/// Runs the updater and accounts for it in one row.
pub fn run(u: Updater, env: &Env) -> Row {
    let mut d = Draft {
        u,
        started_at: now(),
        before: None,
        after: None,
        notes: vec![],
    };
    if dfm_engine::signal::requested().is_some() {
        d.notes.push("not started: the run was stopped".into());
        return d.end(Status::Pending);
    }
    match u {
        Updater::Agents => agents(d, env),
        Updater::Brew => brew(d, env),
        Updater::Skills => {
            // As the preview says: a missing `mise` is blocked, not a failed command.
            if let Err(e) = env.provider.resolve("mise") {
                d.notes.push(e.detail);
                return d.end(Status::Blocked);
            }
            let untrusted = trust_notes(Updater::Skills, env);
            if !untrusted.is_empty() {
                d.notes.extend(untrusted);
                return d.end(Status::Blocked);
            }
            match call(
                env.provider,
                "mise",
                &args(&["exec", "--", "npx", "skills", "update"]),
                &[],
                env.timeouts.short,
            ) {
                Ok(_) => {
                    d.notes
                        .push("skills update ran; it reports no version".into());
                    d.end(Status::Current)
                }
                Err(e) => d.stop(e),
            }
        }
        _ => native(d, env),
    }
}

/// The runner for a native tool: its owner directory first, then the provider path so its
/// children find the system tools. `Err` is the blocked reason: the tool is missing from its
/// owner directory, and no other copy is used.
fn native_runner(u: Updater, env: &Env) -> Result<Runner, String> {
    let dir = env.home.join(owner_dir(u).unwrap_or_default());
    let path = dir.join(u.name());
    if std::fs::symlink_metadata(&path).is_err() {
        return Err(format!("not installed at {}", path.display()));
    }
    let mut r = env.provider.clone();
    r.search_path.insert(0, dir);
    r.resolve(u.name()).map_err(|e| e.detail)?;
    Ok(r)
}

fn version(r: &Runner, u: Updater, env: &Env) -> Result<String, (String, bool)> {
    let out = call(r, u.name(), &args(&["--version"]), &[], env.timeouts.short)?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        // No evidence of a version: neither `current` nor `updated` could be shown.
        return Err((format!("{} --version printed no version", u.name()), false));
    }
    Ok(v)
}

fn native(mut d: Draft, env: &Env) -> Row {
    let u = d.u;
    let r = match native_runner(u, env) {
        Ok(r) => r,
        Err(why) => {
            d.notes.push(why);
            return d.end(Status::Blocked);
        }
    };
    match version(&r, u, env) {
        Ok(v) => d.before = Some(v),
        Err((note, stopped)) => {
            d.notes.push(note);
            return d.end(if stopped {
                Status::Pending
            } else {
                Status::Blocked
            });
        }
    }
    if let Err(e) = call(
        &r,
        u.name(),
        &args(&[native_update(u)]),
        &[],
        env.timeouts.short,
    ) {
        return d.stop(e);
    }
    match version(&r, u, env) {
        Ok(v) => {
            let changed = d.before.as_deref() != Some(v.as_str());
            d.after = Some(v);
            d.end(if changed {
                Status::Updated
            } else {
                Status::Current
            })
        }
        Err((note, _)) => {
            d.notes.push(format!(
                "the update ran, but the new version is unknown: {note}"
            ));
            d.end(Status::Pending)
        }
    }
}

/// Notes for a Homebrew formula or cask of the same name as a native tool, read after every
/// updater ran so the versions are the ones brew left. The check never changes a row's status,
/// and finds nothing when brew is absent.
pub fn second_copies(u: Updater, env: &Env) -> Vec<String> {
    let mut notes = Vec::new();
    if env.provider.resolve("brew").is_err() {
        return notes;
    }
    // Homebrew's names for the same tool. The `claude` cask is the desktop app, not Claude Code.
    let (formula, cask) = match u {
        Updater::Claude => (None, Some("claude-code")),
        Updater::Codex => (Some("codex"), Some("codex")),
        Updater::Opencode => (Some("opencode"), None),
        Updater::Agents | Updater::Brew | Updater::Skills => (None, None),
    };
    let checks = formula
        .map(|n| ("formula", args(&["list", "--versions", n])))
        .into_iter()
        .chain(cask.map(|n| ("cask", args(&["list", "--cask", "--versions", n]))));
    for (kind, a) in checks {
        if let Ok(out) = call(
            env.provider,
            "brew",
            &a,
            &brew_env(false),
            env.timeouts.short,
        ) {
            let listed = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !listed.is_empty() {
                notes.push(format!("second copy: Homebrew {kind} {listed}"));
            }
        }
    }
    notes
}

/// One outdated formula or cask: kind, name, installed versions, newest version. A formula and a
/// cask can share a name, so the kind is part of the identity.
#[derive(Debug, PartialEq, Eq)]
struct Outdated {
    kind: &'static str,
    name: String,
    installed: String,
    current: String,
    /// Held by `brew pin`: left out of the upgrade.
    pinned: bool,
}

fn outdated(env: &Env) -> Result<Vec<Outdated>, (String, bool)> {
    let out = call(
        env.provider,
        "brew",
        &args(&["outdated", "--json=v2"]),
        &brew_env(false),
        env.timeouts.short,
    )?;
    let bad = |why: String| (format!("brew outdated --json=v2: {why}"), false);
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| bad(format!("not JSON ({e})")))?;
    let mut list = Vec::new();
    for (key, kind) in [("formulae", "formula"), ("casks", "cask")] {
        let items = v
            .get(key)
            .and_then(|x| x.as_array())
            .ok_or_else(|| bad(format!("no `{key}` array")))?;
        for i in items {
            let s = |k: &str| i.get(k).and_then(|x| x.as_str()).map(str::to_string);
            let name = s("name").ok_or_else(|| bad("an entry has no name".into()))?;
            let installed = i
                .get("installed_versions")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            list.push(Outdated {
                kind,
                name,
                installed,
                current: s("current_version").unwrap_or_default(),
                pinned: i.get("pinned").and_then(|x| x.as_bool()).unwrap_or(false),
            });
        }
    }
    Ok(list)
}

fn brew(mut d: Draft, env: &Env) -> Row {
    if let Err(e) = env.provider.resolve("brew") {
        d.notes.push(e.detail);
        return d.end(Status::Blocked);
    }
    if let Err(e) = call(
        env.provider,
        "brew",
        &args(&["update"]),
        &brew_env(true),
        env.timeouts.short,
    ) {
        return d.stop(e);
    }
    let (held, before): (Vec<_>, Vec<_>) = match outdated(env) {
        Ok(l) => l.into_iter().partition(|o| o.pinned),
        Err(e) => return d.stop(e),
    };
    for o in &held {
        d.notes
            .push(format!("{} {} held (brew pin)", o.kind, o.name));
    }
    if before.is_empty() {
        return d.end(Status::Current);
    }
    // One call per kind, so a formula and a cask of one name are each upgraded as themselves. A
    // failed kind does not keep the other from its upgrade; the first failure is kept.
    let mut upgrade = Ok(());
    for kind in ["formula", "cask"] {
        let names: Vec<String> = before
            .iter()
            .filter(|o| o.kind == kind)
            .map(|o| o.name.clone())
            .collect();
        if names.is_empty() {
            continue;
        }
        let mut a = args(&["upgrade", &format!("--{kind}")]);
        a.extend(names);
        let done = call(
            env.provider,
            "brew",
            &a,
            &brew_env(false),
            env.timeouts.long,
        );
        match done {
            // A stop ends the updater here: no other brew command starts.
            Err(e) if e.1 => return d.stop(e),
            Err(e) => {
                if upgrade.is_ok() {
                    upgrade = Err(e);
                } else {
                    d.notes.push(e.0);
                }
            }
            Ok(_) => {}
        }
    }
    let after = match outdated(env) {
        Ok(l) => l,
        Err(e) => return d.stop(e),
    };
    let mut left = false;
    for o in &before {
        match after.iter().find(|x| x.kind == o.kind && x.name == o.name) {
            Some(x) => {
                left = true;
                d.notes.push(format!(
                    "{} {} {} still outdated ({})",
                    o.kind, o.name, x.installed, x.current
                ));
            }
            None => d.notes.push(format!(
                "{} {} {} -> {} upgraded",
                o.kind, o.name, o.installed, o.current
            )),
        }
    }
    match upgrade {
        Err(e) => d.stop(e),
        Ok(_) if left => {
            d.notes
                .push("brew upgrade exited 0 but left packages outdated".into());
            d.end(Status::Failed)
        }
        Ok(_) => d.end(Status::Updated),
    }
}

// ---- source trust (docs/upgrade.md, "Source trust") ------------------------------------------

/// The `skills` CLI's record of the global skills it updates, relative to HOME. The runner
/// clears the environment, so `XDG_STATE_HOME`, which would move it, is never set.
const SKILLS_RECORD: &str = ".agents/.skill-lock.json";
/// The only record version this manager reads.
const SKILLS_RECORD_VERSION: u64 = 3;
/// The installer that `npx` fetches and runs.
const SKILLS_PACKAGE: &str = "skills";
/// The largest source record read.
const RECORD_MAX_BYTES: u64 = 1 << 20;

fn read_record(path: &Path) -> Result<String, String> {
    let why = |what: String| format!("cannot read {}: {what}; nothing ran", path.display());
    let bytes = crate::read_fs::read_regular_bounded(
        path,
        RECORD_MAX_BYTES,
        |e| Refusal::new(Code::IoFailed, e.kind().to_string()),
        || Refusal::new(Code::IoFailed, "not a regular file"),
    )
    .map_err(|e| why(e.detail))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > RECORD_MAX_BYTES {
        return Err(why("larger than 1 MiB".into()));
    }
    String::from_utf8(bytes).map_err(|_| why("not UTF-8".into()))
}

/// The sources that `skills update` fetches: `npm:skills` for the installer, then `source` and
/// `sourceUrl` of each entry of the `skills` CLI's record (version 3:
/// `{"version": 3, "skills": {<name>: {"source": .., "sourceUrl": .., ..}}}`). `Err` is why
/// the record cannot be read.
fn skills_sources(env: &Env) -> Result<Vec<String>, String> {
    let path = env.home.join(SKILLS_RECORD);
    let text = read_record(&path)?;
    let unknown = |what: &str| {
        format!(
            "{} is not a skills record this manager reads: {what}; nothing ran",
            path.display()
        )
    };
    let doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| unknown("not valid JSON"))?;
    if doc["version"].as_u64() != Some(SKILLS_RECORD_VERSION) {
        return Err(unknown("version is not 3"));
    }
    let skills = doc["skills"]
        .as_object()
        .ok_or_else(|| unknown("skills is not an object"))?;
    let mut values = vec![format!("{NPM}{SKILLS_PACKAGE}")];
    for (name, entry) in skills {
        let source = entry["source"]
            .as_str()
            .ok_or_else(|| unknown(&format!("skill {name:?} has no source")))?;
        values.push(source.to_string());
        match &entry["sourceUrl"] {
            serde_json::Value::Null => {}
            serde_json::Value::String(u) => values.push(u.clone()),
            _ => {
                return Err(unknown(&format!(
                    "skill {name:?} has a sourceUrl that is not text"
                )));
            }
        }
    }
    Ok(values)
}

/// The notes for `values`, the sources one updater fetches: one per source that maps to no
/// identity and one per identity without a grant, each with its `--trust-origin` command.
fn untrusted(values: &[String], grants: &Grants) -> Vec<String> {
    let (mut notes, mut ids) = (Vec::new(), BTreeSet::new());
    for v in values {
        match source_identity(v, cfg!(feature = "test-isolation")) {
            Ok(id) => {
                ids.insert(id);
            }
            Err(e) => notes.push(format!("{}: {}", e.code.as_str(), e.detail)),
        }
    }
    notes.extend(ids.iter().filter(|id| !grants.trusts(id)).map(|id| {
        format!(
            "{}: no grant for {id}; review it, then run dfm sync --trust-origin {id}",
            Code::SourceUntrusted.as_str()
        )
    }));
    notes
}

/// Why the updater must not run because of its sources, one note each, or nothing. `skills`
/// and the `agents` managed copy fetch extension sources, and each source needs a grant
/// (DFM-25, C04). The `agents` working copy is never fetched, and the other updaters update a
/// binary or packages from their owner: none of them is gated. An unreadable source record is
/// one note. An updater with a note runs no command that fetches or installs.
pub fn trust_notes(u: Updater, env: &Env) -> Vec<String> {
    match u {
        Updater::Skills => match skills_sources(env) {
            Ok(v) => untrusted(&v, env.grants),
            Err(why) => vec![why],
        },
        Updater::Agents => {
            let (copy, working) = agents_copy(env);
            if working || std::fs::symlink_metadata(copy.join(".git")).is_err() {
                return vec![];
            }
            match agents_upstream(env, &copy) {
                Ok(Ok(up)) => untrusted(&[up.url], env.grants),
                Ok(Err(why)) | Err((why, _)) => vec![why],
            }
        }
        _ => vec![],
    }
}

/// The managed copy's upstream: the remote and the branch that `branch.<HEAD>.remote` and
/// `branch.<HEAD>.merge` name, and the remote's url as git fetches from it (`git remote
/// get-url` applies `url.*.insteadOf`).
struct Upstream {
    remote: String,
    merge: String,
    url: String,
}

/// Reads the upstream with local git commands only; nothing is fetched. `Ok(Err)` is the
/// blocked reason (no upstream branch, or a remote url git cannot give); `Err` is a command
/// that did not run or was stopped.
fn agents_upstream(env: &Env, copy: &Path) -> Result<Result<Upstream, String>, (String, bool)> {
    let none = || {
        format!(
            "the managed copy {} has no upstream branch; not pulled",
            copy.display()
        )
    };
    let branch = git(env, copy, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        return Ok(Err(none()));
    }
    let key = |field: &str| match git(
        env,
        copy,
        &["config", "--get", &format!("branch.{branch}.{field}")],
    ) {
        Ok(v) if !v.is_empty() => Ok(Ok(v)),
        Err((n, true)) => Err((n, true)),
        _ => Ok(Err(none())),
    };
    let remote = match key("remote")? {
        Ok(r) => r,
        Err(why) => return Ok(Err(why)),
    };
    let merge = match key("merge")? {
        Ok(m) => m,
        Err(why) => return Ok(Err(why)),
    };
    let url = match git(env, copy, &["remote", "get-url", &remote]) {
        Ok(u) => u,
        Err((n, true)) => return Err((n, true)),
        Err((n, false)) => {
            return Ok(Err(format!(
                "cannot read the url of the remote {remote} of {}: {n}; nothing ran",
                copy.display()
            )));
        }
    };
    Ok(Ok(Upstream { remote, merge, url }))
}

/// The agent registry's working copy (never pulled) and managed copy (fast-forwarded), relative
/// to HOME.
const AGENTS_WORKING: &str = "projects/agents";
const AGENTS_MANAGED: &str = ".local/share/agent-registry";
const AGENTS_INSTALL: &str = "install-agent-registry.sh";

/// The copy to install from: the working copy when it holds `pyproject.toml`, else the managed
/// copy. `true` marks the working copy.
fn agents_copy(env: &Env) -> (std::path::PathBuf, bool) {
    let working = env.home.join(AGENTS_WORKING);
    if working.join("pyproject.toml").is_file() {
        (working, true)
    } else {
        (env.home.join(AGENTS_MANAGED), false)
    }
}

/// The runner for the install script: the bound checkout's `scripts` first, then the provider
/// path. `Err` is the blocked reason.
fn agents_runner(env: &Env) -> Result<Runner, String> {
    let dir = env.root.join("scripts");
    let path = dir.join(AGENTS_INSTALL);
    if std::fs::symlink_metadata(&path).is_err() {
        return Err(format!("no install script at {}", path.display()));
    }
    let mut r = env.provider.clone();
    r.search_path.insert(0, dir);
    r.resolve(AGENTS_INSTALL).map_err(|e| e.detail)?;
    Ok(r)
}

/// `git -C <dir> <a>`; its trimmed standard output.
fn git(env: &Env, dir: &Path, a: &[&str]) -> Result<String, (String, bool)> {
    let mut all = args(&["-C", &dir.display().to_string()]);
    all.extend(args(a));
    let out = call(env.provider, "git", &all, &[], env.timeouts.short)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The agent registry: install from the working copy as it is, or fast-forward the managed copy
/// and install from it. Nothing here merges, resets or clones: a missing, dirty or diverged
/// managed copy is `blocked` before any change.
fn agents(mut d: Draft, env: &Env) -> Row {
    let (copy, working) = agents_copy(env);
    let r = match agents_runner(env) {
        Ok(r) => r,
        Err(why) => {
            d.notes.push(why);
            return d.end(Status::Blocked);
        }
    };
    if let Err(e) = env.provider.resolve("git") {
        d.notes.push(e.detail);
        return d.end(Status::Blocked);
    }
    if !working && std::fs::symlink_metadata(copy.join(".git")).is_err() {
        d.notes.push(format!(
            "no managed copy at {}; clone it by hand",
            copy.display()
        ));
        return d.end(Status::Blocked);
    }
    // Read-only checks first: the revision and the dirty state.
    match git(env, &copy, &["rev-parse", "--short", "HEAD"]) {
        Ok(v) => d.before = Some(v),
        Err(e) => return d.stop(e),
    }
    let dirty = match git(env, &copy, &["status", "--porcelain"]) {
        Ok(s) => !s.is_empty(),
        Err(e) => return d.stop(e),
    };
    if working {
        d.notes.push(format!(
            "working copy {}{}; not pulled",
            copy.display(),
            if dirty {
                " has uncommitted changes"
            } else {
                ""
            }
        ));
        d.after = d.before.clone();
    } else {
        if dirty {
            d.notes.push(format!(
                "managed copy {} has uncommitted changes; not pulled",
                copy.display()
            ));
            return d.end(Status::Blocked);
        }
        // No upstream is a precondition, like a missing copy: nothing to fast-forward from.
        let up = match agents_upstream(env, &copy) {
            Ok(Ok(up)) => up,
            Ok(Err(why)) => {
                d.notes.push(why);
                return d.end(Status::Blocked);
            }
            Err(e) => return d.stop(e),
        };
        // The upstream's grant covers the pull and the registry's own install code. The
        // fetch and the pull name the checked remote, so git cannot pick another one.
        let notes = untrusted(std::slice::from_ref(&up.url), env.grants);
        if !notes.is_empty() {
            d.notes.extend(notes);
            return d.end(Status::Blocked);
        }
        // `fetch` moves only the remote-tracking refs. A local commit that is not on the
        // upstream means the copy diverged: `pull --ff-only` is not tried.
        if let Err(e) = git(env, &copy, &["fetch", "--quiet", &up.remote]) {
            return d.stop(e);
        }
        match git(env, &copy, &["rev-list", "--count", "@{u}..HEAD"]) {
            Ok(n) if n == "0" => {}
            Ok(n) => {
                d.notes.push(format!(
                    "managed copy {} has {n} local commits that are not on its upstream; not pulled",
                    copy.display()
                ));
                return d.end(Status::Blocked);
            }
            Err(e) => return d.stop(e),
        }
        if let Err(e) = git(
            env,
            &copy,
            &["pull", "--ff-only", "--quiet", &up.remote, &up.merge],
        ) {
            return d.stop(e);
        }
        match git(env, &copy, &["rev-parse", "--short", "HEAD"]) {
            Ok(v) => d.after = Some(v),
            Err(e) => return d.stop(e),
        }
    }
    if let Err(e) = call(
        &r,
        AGENTS_INSTALL,
        &[copy.display().to_string()],
        &[],
        env.timeouts.short,
    ) {
        return d.stop(e);
    }
    let changed = d.before != d.after;
    d.end(if changed {
        Status::Updated
    } else {
        Status::Current
    })
}

/// What `run` would do, without running anything.
pub fn preview(u: Updater, env: &Env) -> String {
    let would = preview_command(u, env);
    if would.starts_with("blocked: ") {
        return would;
    }
    match trust_notes(u, env) {
        n if n.is_empty() => would,
        n => format!("blocked: {}", n.join("; ")),
    }
}

fn preview_command(u: Updater, env: &Env) -> String {
    match u {
        Updater::Agents => {
            let (copy, working) = agents_copy(env);
            match agents_runner(env) {
                Ok(r) => format!(
                    "would {} {} and run {} ({})",
                    if working {
                        "install from the working copy"
                    } else {
                        "fast-forward"
                    },
                    copy.display(),
                    AGENTS_INSTALL,
                    r.resolve(AGENTS_INSTALL)
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                Err(why) => format!("blocked: {why}"),
            }
        }
        Updater::Brew => match env.provider.resolve("brew") {
            Ok(p) => format!(
                "would run brew update, brew upgrade of every outdated formula and cask ({})",
                p.display()
            ),
            Err(e) => format!("blocked: {}", e.detail),
        },
        Updater::Skills => match env.provider.resolve("mise") {
            Ok(p) => format!("would run mise exec -- npx skills update ({})", p.display()),
            Err(e) => format!("blocked: {}", e.detail),
        },
        _ => match native_runner(u, env) {
            Ok(r) => format!(
                "would run {} {} ({})",
                u.name(),
                native_update(u),
                r.resolve(u.name())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
            Err(why) => format!("blocked: {why}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    struct T {
        root: PathBuf,
        home: PathBuf,
        /// The bound checkout.
        dots: PathBuf,
        bin: PathBuf,
        runner: Runner,
        grants: Grants,
    }

    /// The registry remote that [`T::managed`] records, and its identity.
    const REGISTRY: &str = "git@github.com:o/agents.git";
    const REGISTRY_ID: &str = "ssh://git@github.com/o/agents";
    /// A skills record with one GitHub skill, whose identity is `https://github.com/o/skill`.
    const SKILL_RECORD: &str = r#"{"version":3,"skills":{"one":{"source":"o/skill","sourceType":"github","sourceUrl":"https://github.com/o/skill.git","skillPath":"skills/one/SKILL.md"}}}"#;

    impl T {
        fn new() -> T {
            // Tests run in parallel and the clock has microsecond steps, so a counter keeps
            // each root apart.
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "dfm-updater-unit-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let (home, bin) = (root.join("home"), root.join("bin"));
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&bin).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let (home, bin) = (root.join("home"), root.join("bin"));
            let uid = fs::metadata(&root).unwrap().uid();
            let runner = Runner {
                search_path: vec![bin.clone(), "/usr/bin".into(), "/bin".into()],
                home: home.clone(),
                cwd: root.join("run"),
                uid,
                grace: Duration::from_millis(200),
                detach: false,
            };
            let mut grants = Grants::default();
            for id in [REGISTRY_ID, "npm:skills", "https://github.com/o/skill"] {
                grants.grant(id, "2026-11-02T00:00:00Z");
            }
            fs::create_dir_all(home.join(".agents")).unwrap();
            fs::write(home.join(".agents/.skill-lock.json"), SKILL_RECORD).unwrap();
            T {
                dots: root.join("dots"),
                root,
                home,
                bin,
                runner,
                grants,
            }
        }

        /// A look-alike at `path` that logs `<name> <args> | auto=<HOMEBREW_NO_AUTO_UPDATE>
        /// deps=<HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK> analytics=<HOMEBREW_NO_ANALYTICS>`
        /// and then runs `body`.
        fn script(&self, path: &Path, body: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let name = path.file_name().unwrap().to_str().unwrap();
            let log = self.root.join("log");
            fs::write(
                path,
                format!(
                    "#!/bin/sh\nPATH=/usr/bin:/bin\nst={st}\nprintf '%s %s | auto=%s deps=%s analytics=%s\\n' {name} \"$*\" \"${{HOMEBREW_NO_AUTO_UPDATE:-unset}}\" \"${{HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK:-unset}}\" \"${{HOMEBREW_NO_ANALYTICS:-unset}}\" >> {log}\n{body}\n",
                    st = self.root.join("st").display(),
                    log = log.display(),
                ),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn log(&self) -> String {
            fs::read_to_string(self.root.join("log")).unwrap_or_default()
        }

        fn env(&self) -> Env<'_> {
            Env {
                provider: &self.runner,
                home: &self.home,
                root: &self.dots,
                timeouts: Timeouts::DEFAULT,
                grants: &self.grants,
            }
        }

        /// A native tool whose `--version` prints the content of `st/<name>` and whose update
        /// command writes `after` there.
        fn native(&self, rel: &str, update: &str, before: &str, after: &str) {
            let name = Path::new(rel).file_name().unwrap().to_str().unwrap();
            fs::create_dir_all(self.root.join("st")).unwrap();
            fs::write(self.root.join("st").join(name), before).unwrap();
            self.script(
                &self.home.join(rel),
                &format!(
                    "case \"$1\" in\n  --version) cat \"$st/{name}\" ;;\n  {update}) printf '%s' '{after}' > \"$st/{name}\" ;;\n  *) exit 64 ;;\nesac"
                ),
            );
        }

        /// `brew` whose `outdated --json=v2` prints `st/outdated` and whose `upgrade` replaces
        /// it with `st/after`, then exits with `st/upgrade-exit` (0 when absent).
        fn brew(&self, outdated: &str, after: &str) {
            fs::create_dir_all(self.root.join("st")).unwrap();
            fs::write(self.root.join("st/outdated"), outdated).unwrap();
            fs::write(self.root.join("st/after"), after).unwrap();
            self.script(
                &self.bin.join("brew"),
                "case \"$1\" in\n  update) exit 0 ;;\n  outdated) cat \"$st/outdated\" ;;\n  upgrade) cp \"$st/after\" \"$st/outdated\"; exit $(cat \"$st/upgrade-exit$2\" 2>/dev/null || cat \"$st/upgrade-exit\" 2>/dev/null || echo 0) ;;\n  list) [ -f \"$st/list-$4\" ] && cat \"$st/list-$4\" || exit 1 ;;\n  *) exit 64 ;;\nesac",
            );
        }
    }

    impl T {
        /// `git` whose `rev-parse` prints `st/rev` (`main` for `--abbrev-ref HEAD`), `config`
        /// names the remote in `st/remote` (`origin` when absent, nothing with
        /// `st/no-upstream`) and `refs/heads/main`, `remote get-url <r>` prints `st/url-<r>`,
        /// `status` prints `st/porcelain`, `rev-list`
        /// prints `st/ahead` (0 when absent) and `pull` copies `st/rev-after` over `st/rev`; the
        /// bound checkout's install script exits with `st/install-exit` (0 when absent).
        fn agents(&self, rev: &str, rev_after: &str) {
            let st = self.root.join("st");
            fs::create_dir_all(&st).unwrap();
            fs::write(st.join("rev"), rev).unwrap();
            fs::write(st.join("rev-after"), rev_after).unwrap();
            fs::write(st.join("url-origin"), REGISTRY).unwrap();
            self.script(
                &self.bin.join("git"),
                "case \"$3\" in\n  rev-parse) if [ \"$4\" = --abbrev-ref ]; then echo main; else cat \"$st/rev\"; fi ;;\n  config) [ -f \"$st/no-upstream\" ] && exit 1; case \"$5\" in *.remote) cat \"$st/remote\" 2>/dev/null || echo origin ;; *) echo refs/heads/main ;; esac ;;\n  remote) cat \"$st/url-$5\" ;;\n  status) cat \"$st/porcelain\" 2>/dev/null || true ;;\n  fetch) exit 0 ;;\n  rev-list) cat \"$st/ahead\" 2>/dev/null || echo 0 ;;\n  pull) cp \"$st/rev-after\" \"$st/rev\" ;;\n  *) exit 64 ;;\nesac",
            );
            self.script(
                &self.dots.join("scripts/install-agent-registry.sh"),
                "echo 'secret detail' >&2; exit $(cat \"$st/install-exit\" 2>/dev/null || echo 0)",
            );
        }

        fn managed(&self) -> PathBuf {
            let copy = self.home.join(".local/share/agent-registry");
            fs::create_dir_all(copy.join(".git")).unwrap();
            copy
        }

        /// The logged calls without the environment part.
        fn calls(&self) -> Vec<String> {
            self.log()
                .lines()
                .map(|l| l.split(" | ").next().unwrap().to_string())
                .collect()
        }
    }

    /// The read-only calls that find the upstream of the copy at `c`.
    fn upstream_calls(c: &str, remote: &str) -> Vec<String> {
        vec![
            format!("git -C {c} rev-parse --short HEAD"),
            format!("git -C {c} status --porcelain"),
            format!("git -C {c} rev-parse --abbrev-ref HEAD"),
            format!("git -C {c} config --get branch.main.remote"),
            format!("git -C {c} config --get branch.main.merge"),
            format!("git -C {c} remote get-url {remote}"),
        ]
    }

    #[test]
    fn agents_with_an_ungranted_upstream_is_blocked_before_any_fetch() {
        let mut t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        t.grants = Grants::default();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [format!(
                "E_SOURCE_UNTRUSTED: no grant for {REGISTRY_ID}; review it, then run dfm sync --trust-origin {REGISTRY_ID}"
            )]
        );
        assert_eq!(
            t.calls(),
            upstream_calls(&copy.display().to_string(), "origin")
        );
        assert_eq!(
            preview(Updater::Agents, &t.env()),
            format!("blocked: {}", r.notes[0])
        );
    }

    #[test]
    fn agents_checks_the_upstream_remote_not_origin() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        // origin is granted, but the branch tracks the remote `fork`, which is not.
        fs::write(t.root.join("st/remote"), "fork").unwrap();
        fs::write(t.root.join("st/url-fork"), "git@github.com:evil/agents.git").unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                "E_SOURCE_UNTRUSTED: no grant for ssh://git@github.com/evil/agents; review it, then run dfm sync --trust-origin ssh://git@github.com/evil/agents"
            ]
        );
        assert_eq!(
            t.calls(),
            upstream_calls(&copy.display().to_string(), "fork")
        );
        assert!(preview(Updater::Agents, &t.env()).starts_with("blocked: E_SOURCE_UNTRUSTED"));
    }

    #[test]
    fn agents_with_a_granted_origin_runs() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        t.managed();
        assert!(t.grants.trusts(REGISTRY_ID));
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
    }

    #[test]
    fn agents_with_an_unreadable_or_local_upstream_is_blocked() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        // `remote get-url` fails: the url git would fetch from is unknown.
        fs::remove_file(t.root.join("st/url-origin")).unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes[0].starts_with(&format!(
                "cannot read the url of the remote origin of {}: ",
                copy.display()
            )),
            "{r:?}"
        );
        // A local path is not a source identity.
        fs::write(t.root.join("st/url-origin"), "/some/local/path").unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(r.notes[0].starts_with("E_SOURCE_UNTRUSTED: "), "{r:?}");
        assert!(!t.log().contains(" fetch"), "{}", t.log());
        assert!(!t.log().contains("install-agent-registry"), "{}", t.log());
    }

    #[test]
    fn agents_working_copy_is_not_checked() {
        let mut t = T::new();
        t.agents("abc1234", "def5678");
        let working = t.home.join("projects/agents");
        fs::create_dir_all(&working).unwrap();
        fs::write(working.join("pyproject.toml"), "").unwrap();
        t.grants = Grants::default();
        assert!(trust_notes(Updater::Agents, &t.env()).is_empty());
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
    }

    #[test]
    fn skills_without_a_grant_for_npm_skills_is_blocked() {
        let mut t = T::new();
        t.script(&t.bin.join("mise"), "exit 0");
        t.grants = Grants::default();
        t.grants
            .grant("https://github.com/o/skill", "2026-11-02T00:00:00Z");
        let r = run(Updater::Skills, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                "E_SOURCE_UNTRUSTED: no grant for npm:skills; review it, then run dfm sync --trust-origin npm:skills"
            ]
        );
        assert_eq!(t.log(), "");
    }

    #[test]
    fn skills_lists_every_ungranted_skill_source() {
        let mut t = T::new();
        t.script(&t.bin.join("mise"), "exit 0");
        t.grants = Grants::default();
        let p = preview(Updater::Skills, &t.env());
        assert!(p.starts_with("blocked: "), "{p}");
        for id in ["npm:skills", "https://github.com/o/skill"] {
            assert!(p.contains(&format!("dfm sync --trust-origin {id}")), "{p}");
        }
        // A source that maps to no identity is untrusted, also with every grant held.
        let t = T::new();
        t.script(&t.bin.join("mise"), "exit 0");
        fs::write(
            t.home.join(".agents/.skill-lock.json"),
            r#"{"version":3,"skills":{"mine":{"source":"/Users/me/skills","sourceType":"local"}}}"#,
        )
        .unwrap();
        let r = run(Updater::Skills, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes[0].starts_with("E_SOURCE_UNTRUSTED: source "),
            "{r:?}"
        );
        assert_eq!(t.log(), "");
    }

    #[test]
    fn skills_with_an_unreadable_source_record_is_blocked() {
        let t = T::new();
        t.script(&t.bin.join("mise"), "exit 0");
        let record = t.home.join(".agents/.skill-lock.json");
        for (text, why) in [
            (None, "cannot read"),
            (Some("not json"), "not valid JSON"),
            (Some(r#"{"version":4,"skills":{}}"#), "version is not 3"),
            (Some(r#"{"version":3,"skills":{"x":{}}}"#), "has no source"),
        ] {
            match text {
                Some(t) => fs::write(&record, t).unwrap(),
                None => {
                    let _ = fs::remove_file(&record);
                }
            }
            let r = run(Updater::Skills, &t.env());
            assert_eq!(r.status, Status::Blocked, "{r:?}");
            assert!(r.notes[0].contains(why), "{why}: {r:?}");
            assert!(r.notes[0].ends_with("; nothing ran"), "{r:?}");
            assert!(preview(Updater::Skills, &t.env()).starts_with("blocked: "));
        }
        assert_eq!(t.log(), "");
    }

    #[test]
    fn only_skills_and_agents_are_gated() {
        let mut t = T::new();
        t.grants = Grants::default();
        for u in [
            Updater::Claude,
            Updater::Codex,
            Updater::Opencode,
            Updater::Brew,
        ] {
            assert!(trust_notes(u, &t.env()).is_empty(), "{u:?}");
        }
    }

    #[test]
    fn agents_working_copy_is_not_pulled_and_reports_dirty() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let working = t.home.join("projects/agents");
        fs::create_dir_all(&working).unwrap();
        fs::write(working.join("pyproject.toml"), "").unwrap();
        // A managed copy exists too; the working copy wins.
        t.managed();
        fs::write(t.root.join("st/porcelain"), " M agents/x.md\n").unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(r.before.as_deref(), Some("abc1234"));
        assert_eq!(r.after.as_deref(), Some("abc1234"));
        let w = working.display();
        assert_eq!(
            r.notes,
            [format!(
                "working copy {w} has uncommitted changes; not pulled"
            )]
        );
        assert_eq!(
            t.calls(),
            [
                format!("git -C {w} rev-parse --short HEAD"),
                format!("git -C {w} status --porcelain"),
                format!("install-agent-registry.sh {w}"),
            ]
        );
    }

    #[test]
    fn agents_managed_copy_is_fast_forwarded_and_installed() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(r.before.as_deref(), Some("abc1234"));
        assert_eq!(r.after.as_deref(), Some("def5678"));
        let c = copy.display();
        let mut want = upstream_calls(&c.to_string(), "origin");
        want.extend([
            format!("git -C {c} fetch --quiet origin"),
            format!("git -C {c} rev-list --count @{{u}}..HEAD"),
            format!("git -C {c} pull --ff-only --quiet origin refs/heads/main"),
            format!("git -C {c} rev-parse --short HEAD"),
            format!("install-agent-registry.sh {c}"),
        ]);
        assert_eq!(t.calls(), want);
    }

    #[test]
    fn agents_managed_copy_without_new_commits_is_current() {
        let t = T::new();
        t.agents("abc1234", "abc1234");
        t.managed();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
    }

    #[test]
    fn agents_diverged_copy_is_blocked_and_not_pulled() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        t.managed();
        fs::write(t.root.join("st/ahead"), "2\n").unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes.iter().any(
                |n| n.ends_with("has 2 local commits that are not on its upstream; not pulled")
            ),
            "{r:?}"
        );
        assert!(!t.log().contains(" pull "), "{}", t.log());
        assert!(!t.log().contains("install-agent-registry"), "{}", t.log());
    }

    #[test]
    fn agents_copy_without_an_upstream_is_blocked_before_fetch() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        fs::write(t.root.join("st/no-upstream"), "").unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [format!(
                "the managed copy {} has no upstream branch; not pulled",
                copy.display()
            )]
        );
        assert!(!t.log().contains(" fetch"), "{}", t.log());
        assert!(!t.log().contains("install-agent-registry"), "{}", t.log());
    }

    #[test]
    fn agents_dirty_managed_copy_is_blocked_and_not_fetched() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        t.managed();
        fs::write(t.root.join("st/porcelain"), "?? stray\n").unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes
                .iter()
                .any(|n| n.ends_with("has uncommitted changes; not pulled")),
            "{r:?}"
        );
        assert!(!t.log().contains(" fetch"), "{}", t.log());
        assert!(!t.log().contains("install-agent-registry"), "{}", t.log());
    }

    #[test]
    fn agents_missing_copy_is_blocked_and_never_cloned() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        let want = t.home.join(".local/share/agent-registry");
        assert_eq!(
            r.notes,
            [format!(
                "no managed copy at {}; clone it by hand",
                want.display()
            )]
        );
        assert_eq!(t.log(), "");
    }

    #[test]
    fn agents_failed_install_is_failed_and_withholds_stderr() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        t.managed();
        fs::write(t.root.join("st/install-exit"), "3").unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        let note = r.notes.join("\n");
        assert!(note.contains("install-agent-registry.sh"), "{note}");
        assert!(note.contains("exited 3"), "{note}");
        assert!(!note.contains("secret detail"), "{note}");
        // The pull happened before the install; the row still shows it.
        assert_eq!(r.after.as_deref(), Some("def5678"));
    }

    #[test]
    fn agents_without_the_install_script_is_blocked_before_any_git() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        t.managed();
        fs::remove_file(t.dots.join("scripts/install-agent-registry.sh")).unwrap();
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(r.notes[0].starts_with("no install script at "), "{r:?}");
        assert_eq!(t.log(), "");
        assert!(preview(Updater::Agents, &t.env()).starts_with("blocked: no install script at "));
    }

    #[test]
    fn agents_preview_names_the_copy_and_runs_nothing() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        let p = preview(Updater::Agents, &t.env());
        assert!(
            p.starts_with(&format!(
                "would fast-forward {} and run install-agent-registry.sh",
                copy.display()
            )),
            "{p}"
        );
        // Only the read-only calls that find the upstream for the source check.
        let c = copy.display().to_string();
        assert_eq!(t.calls(), upstream_calls(&c, "origin")[2..]);
    }

    impl Drop for T {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const NONE: &str = r#"{"formulae":[],"casks":[]}"#;
    const UV_CODEX: &str = r#"{"formulae":[{"name":"uv","installed_versions":["0.13.5"],"current_version":"0.13.6"}],"casks":[{"name":"codex","installed_versions":["0.164.0"],"current_version":"0.165.0"}]}"#;
    const UV_ONLY: &str = r#"{"formulae":[{"name":"uv","installed_versions":["0.13.5"],"current_version":"0.13.6"}],"casks":[]}"#;

    #[test]
    fn claude_with_a_new_version_is_updated() {
        let t = T::new();
        t.native(
            ".local/bin/claude",
            "update",
            "2.1.311 (Claude Code)",
            "2.1.312 (Claude Code)",
        );
        let r = run(Updater::Claude, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(r.before.as_deref(), Some("2.1.311 (Claude Code)"));
        assert_eq!(r.after.as_deref(), Some("2.1.312 (Claude Code)"));
    }

    #[test]
    fn claude_with_the_same_version_is_current() {
        let t = T::new();
        t.native(
            ".local/bin/claude",
            "update",
            "2.1.311 (Claude Code)",
            "2.1.311 (Claude Code)",
        );
        let r = run(Updater::Claude, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
    }

    #[test]
    fn codex_missing_is_blocked_with_the_path() {
        let t = T::new();
        // Another copy on the provider path is never a fallback.
        t.script(&t.bin.join("codex"), "exit 0");
        let r = run(Updater::Codex, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        let want = t.home.join(".codex/packages/standalone/current/bin/codex");
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains(&want.display().to_string())),
            "{r:?}"
        );
        assert!(!t.log().contains("codex"), "{}", t.log());
    }

    #[test]
    fn a_failing_update_is_failed_with_the_exit_code() {
        let t = T::new();
        t.script(
            &t.home.join(".opencode/bin/opencode"),
            "case \"$1\" in --version) echo 1.19.4 ;; upgrade) echo 'network down' >&2; exit 1 ;; esac",
        );
        let r = run(Updater::Opencode, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        let note = r.notes.join("\n");
        assert!(note.contains("exited 1"), "{note}");
        // Provider standard error is withheld (docs/secrets.md); its size is shown.
        assert!(!note.contains("network down"), "{note}");
        assert!(note.contains("13 bytes of standard error"), "{note}");
    }

    #[test]
    fn a_timeout_is_failed() {
        let t = T::new();
        t.script(
            &t.home.join(".opencode/bin/opencode"),
            "case \"$1\" in --version) echo 1.19.4 ;; upgrade) exec sleep 5 ;; esac",
        );
        let mut env = t.env();
        env.timeouts = Timeouts {
            long: Duration::from_secs(2),
            short: Duration::from_secs(2),
        };
        let r = run(Updater::Opencode, &env);
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes.join("\n").contains("did not finish within"),
            "{r:?}"
        );
    }

    #[test]
    fn brew_upgrades_only_the_outdated_names() {
        let t = T::new();
        t.brew(UV_CODEX, NONE);
        let r = run(Updater::Brew, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        let log = t.log();
        let calls: Vec<&str> = log
            .lines()
            .map(|l| l.split(" | ").next().unwrap())
            .collect();
        assert_eq!(
            calls,
            [
                "brew update",
                "brew outdated --json=v2",
                "brew upgrade --formula uv",
                "brew upgrade --cask codex",
                "brew outdated --json=v2"
            ],
            "{log}"
        );
        assert!(
            r.notes
                .contains(&"formula uv 0.13.5 -> 0.13.6 upgraded".to_string()),
            "{r:?}"
        );
        assert!(
            r.notes
                .contains(&"cask codex 0.164.0 -> 0.165.0 upgraded".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn brew_with_nothing_outdated_is_current_and_runs_no_upgrade() {
        let t = T::new();
        t.brew(NONE, NONE);
        let r = run(Updater::Brew, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert!(!t.log().contains("brew upgrade"), "{}", t.log());
    }

    #[test]
    fn brew_partial_upgrade_is_failed_and_lists_each_package() {
        let t = T::new();
        t.brew(UV_CODEX, UV_ONLY);
        fs::write(t.root.join("st/upgrade-exit"), "1").unwrap();
        let r = run(Updater::Brew, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes
                .contains(&"formula uv 0.13.5 still outdated (0.13.6)".to_string()),
            "{r:?}"
        );
        assert!(
            r.notes
                .contains(&"cask codex 0.164.0 -> 0.165.0 upgraded".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn brew_keeps_a_formula_and_a_cask_of_one_name_apart() {
        let t = T::new();
        let both = r#"{"formulae":[{"name":"codex","installed_versions":["0.1"],"current_version":"0.2"}],"casks":[{"name":"codex","installed_versions":["0.164.0"],"current_version":"0.165.0"}]}"#;
        let formula_left = r#"{"formulae":[{"name":"codex","installed_versions":["0.1"],"current_version":"0.2"}],"casks":[]}"#;
        t.brew(both, formula_left);
        fs::write(t.root.join("st/upgrade-exit"), "1").unwrap();
        let r = run(Updater::Brew, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes
                .contains(&"formula codex 0.1 still outdated (0.2)".to_string()),
            "{r:?}"
        );
        assert!(
            r.notes
                .contains(&"cask codex 0.164.0 -> 0.165.0 upgraded".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn a_failed_formula_upgrade_still_upgrades_the_casks() {
        let t = T::new();
        t.brew(UV_CODEX, UV_ONLY);
        fs::write(t.root.join("st/upgrade-exit--formula"), "1").unwrap();
        let r = run(Updater::Brew, &t.env());
        assert!(
            t.log().contains("brew upgrade --cask codex |"),
            "{}",
            t.log()
        );
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("brew upgrade --formula uv exited 1")),
            "{r:?}"
        );
        assert!(
            r.notes
                .contains(&"cask codex 0.164.0 -> 0.165.0 upgraded".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn brew_env_keeps_homebrew_defaults() {
        let t = T::new();
        t.brew(UV_ONLY, NONE);
        run(Updater::Brew, &t.env());
        let log = t.log();
        let line = |p: &str| log.lines().find(|l| l.starts_with(p)).unwrap().to_string();
        assert!(line("brew update").contains("auto=unset"), "{log}");
        let upgrade = line("brew upgrade");
        assert!(upgrade.contains("auto=1"), "{log}");
        assert!(upgrade.contains("deps=unset"), "{log}");
        assert!(upgrade.contains("analytics=1"), "{log}");
    }

    #[test]
    fn brew_invalid_outdated_json_is_failed() {
        let t = T::new();
        t.brew("not json", NONE);
        let r = run(Updater::Brew, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(r.notes.join("\n").contains("brew outdated"), "{r:?}");
        assert!(!t.log().contains("brew upgrade"), "{}", t.log());
    }

    #[test]
    fn skills_exit_0_is_current_with_a_note() {
        let t = T::new();
        t.script(&t.bin.join("mise"), "exit 0");
        let r = run(Updater::Skills, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert!(
            r.notes
                .contains(&"skills update ran; it reports no version".to_string()),
            "{r:?}"
        );
        assert!(
            t.log().starts_with("mise exec -- npx skills update |"),
            "{}",
            t.log()
        );
    }

    #[test]
    fn codex_second_copy_is_reported() {
        let t = T::new();
        t.native(
            ".codex/packages/standalone/current/bin/codex",
            "update",
            "codex-cli 0.164.0",
            "codex-cli 0.164.0",
        );
        t.brew(NONE, NONE);
        fs::write(t.root.join("st/list-codex"), "codex 0.164.0\n").unwrap();
        // The row itself does not look: sync reads the copies after every updater ran.
        let r = run(Updater::Codex, &t.env());
        assert!(!r.notes.iter().any(|n| n.contains("second copy")), "{r:?}");
        assert_eq!(
            second_copies(Updater::Codex, &t.env()),
            ["second copy: Homebrew cask codex 0.164.0"]
        );
    }

    #[test]
    fn claude_second_copy_is_the_claude_code_cask() {
        let t = T::new();
        t.native(".local/bin/claude", "update", "2.1.311", "2.1.311");
        t.brew(NONE, NONE);
        // The `claude` cask is the desktop app, not a copy of Claude Code.
        fs::write(t.root.join("st/list-claude"), "claude 1.0\n").unwrap();
        assert!(second_copies(Updater::Claude, &t.env()).is_empty());
        fs::write(t.root.join("st/list-claude-code"), "claude-code 2.1.300\n").unwrap();
        assert_eq!(
            second_copies(Updater::Claude, &t.env()),
            ["second copy: Homebrew cask claude-code 2.1.300"]
        );
    }

    #[test]
    fn an_update_with_an_unreadable_new_version_is_pending() {
        let t = T::new();
        t.native(".local/bin/claude", "update", "2.1.311", "");
        let r = run(Updater::Claude, &t.env());
        assert_eq!(r.status, Status::Pending, "{r:?}");
        assert!(
            r.notes
                .iter()
                .any(|n| n.starts_with("the update ran, but the new version is unknown")),
            "{r:?}"
        );
    }

    #[test]
    fn brew_missing_is_blocked() {
        let t = T::new();
        let r = run(Updater::Brew, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(!r.notes.is_empty(), "{r:?}");
    }

    #[test]
    fn skills_without_mise_is_blocked_as_the_preview_says() {
        let t = T::new();
        let r = run(Updater::Skills, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(preview(Updater::Skills, &t.env()).starts_with("blocked: "));
    }

    #[test]
    fn brew_leaves_a_pinned_formula_out() {
        let t = T::new();
        let pinned = r#"{"formulae":[{"name":"uv","installed_versions":["0.13.5"],"current_version":"0.13.6","pinned":false},{"name":"node","installed_versions":["22.1.0"],"current_version":"24.0.0","pinned":true,"pinned_version":"22.1.0"}],"casks":[]}"#;
        let left = r#"{"formulae":[{"name":"node","installed_versions":["22.1.0"],"current_version":"24.0.0","pinned":true,"pinned_version":"22.1.0"}],"casks":[]}"#;
        t.brew(pinned, left);
        let r = run(Updater::Brew, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert!(
            t.log().contains("brew upgrade --formula uv |"),
            "{}",
            t.log()
        );
        assert!(!t.log().contains("node"), "{}", t.log());
        assert!(
            r.notes
                .contains(&"formula node held (brew pin)".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn an_empty_version_is_blocked_before_the_update() {
        let t = T::new();
        t.native(".local/bin/claude", "update", "\n", "2.1.312");
        let r = run(Updater::Claude, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(!t.log().contains("claude update"), "{}", t.log());
        assert!(
            r.notes.iter().any(|n| n.contains("printed no version")),
            "{r:?}"
        );
    }

    #[test]
    fn version_text_is_trimmed() {
        let t = T::new();
        t.native(
            ".codex/packages/standalone/current/bin/codex",
            "update",
            "codex-cli 0.164.0\n\n",
            "codex-cli 0.164.0\n",
        );
        let r = run(Updater::Codex, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(r.before.as_deref(), Some("codex-cli 0.164.0"));
    }

    #[test]
    fn preview_names_the_commands_and_runs_nothing() {
        let t = T::new();
        t.brew(UV_ONLY, NONE);
        let p = preview(Updater::Brew, &t.env());
        assert!(p.starts_with("would run brew update, brew upgrade"), "{p}");
        assert_eq!(t.log(), "");
        let p = preview(Updater::Claude, &t.env());
        assert!(p.contains("not installed at"), "{p}");
    }
}
