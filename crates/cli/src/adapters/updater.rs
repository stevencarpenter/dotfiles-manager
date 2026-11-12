//! The updaters a profile declares (docs/upgrade.md): each one runs through its own owner's
//! updater, detached, and ends in exactly one status. Nothing here returns an error: a missing
//! tool, a failed command or a stop request is a row.

use dfm_engine::proc::{End, Output, Request, Runner};
use dfm_model::lock::{Grants, NPM, NPM_REGISTRY, git_identity, trust_command};
use dfm_model::state::Status;
use dfm_model::updater::Updater;
use dfm_model::{Code, Refusal};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
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
    /// The declared `git-checkout/v1` checkouts that the `checkouts` updater fast-forwards. An
    /// `Err` is the note for a declaration that is not valid (`<id>: <reason>`).
    pub checkouts: &'a [Result<Checkout, String>],
    /// The profile declares `agents`, which pulls its managed copy itself, so `checkouts`
    /// leaves that copy out.
    pub agents_declared: bool,
}

/// A declared `git-checkout/v1` checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The resource id.
    pub id: String,
    pub url: String,
    /// The absolute checkout directory.
    pub path: PathBuf,
    /// The bound checkout declared it and its url (`dfm_model::resolve::owner_declared`): then
    /// its url needs no grant.
    pub owner_declared: bool,
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
        Updater::Rustup => Some(".cargo/bin"),
        Updater::Agents
        | Updater::Brew
        | Updater::Checkouts
        | Updater::Mise
        | Updater::Npm
        | Updater::SideChannels
        | Updater::Skills
        | Updater::Uv => None,
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

/// Why a command did not succeed, or `None`. `ok` is the exit codes that are a success: a
/// listing that exits nonzero when there is something to upgrade (`npm outdated` 1, `rustup
/// check` 100) is still a list. Provider standard error is withheld (docs/secrets.md); its
/// size is shown.
fn trouble(what: &str, out: &Output, timeout: Duration, ok: &[i32]) -> Option<String> {
    match &out.end {
        End::Exited(c) if ok.contains(c) => None,
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
    call_ok(r, program, a, env, timeout, &[0])
}

/// [`call`] with the exit codes that count as success.
fn call_ok(
    r: &Runner,
    program: &str,
    a: &[String],
    env: &[(String, String)],
    timeout: Duration,
    ok: &[i32],
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
    match trouble(&what, &out, timeout, ok) {
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
        Updater::Checkouts => checkouts(d, env),
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
                &npm_env(&[]),
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
        Updater::Mise => mise(d, env),
        Updater::Npm => npm(d, env),
        Updater::Rustup => rustup(d, env),
        Updater::SideChannels => side_channels(d, env),
        Updater::Uv => uv(d, env),
        Updater::Claude | Updater::Codex | Updater::Opencode => native(d, env),
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
        Updater::Agents
        | Updater::Brew
        | Updater::Checkouts
        | Updater::Mise
        | Updater::Npm
        | Updater::Rustup
        | Updater::SideChannels
        | Updater::Skills
        | Updater::Uv => (None, None),
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

// ---- the providers with an outdated list: mise, rustup, npm ---------------------------------

/// One outdated name of a provider: the installed version and the one the upgrade reaches.
#[derive(Debug, PartialEq, Eq)]
struct Item {
    name: String,
    installed: String,
    newest: String,
}

/// A command's output, or the note of one that did not run or did not succeed, with whether a
/// stop request ended it.
type Called = Result<Output, (String, bool)>;

/// The brew pattern for a provider that lists what is outdated: `current` on an empty list,
/// else `upgrade`, then `list` again; `updated` when every name left the list, `failed` when
/// one stays. `before` is the list before the upgrade, so the caller can hold names first.
fn roll(
    mut d: Draft,
    what: &str,
    before: Vec<Item>,
    upgrade: &dyn Fn(&[Item]) -> Called,
    list: &dyn Fn() -> Result<Vec<Item>, (String, bool)>,
) -> Row {
    if before.is_empty() {
        return d.end(Status::Current);
    }
    if let Err(e) = upgrade(&before) {
        return d.stop(e);
    }
    let after = match list() {
        Ok(l) => l,
        Err(e) => return d.stop(e),
    };
    let mut left = false;
    for o in &before {
        match after.iter().find(|x| x.name == o.name) {
            Some(x) => {
                left = true;
                d.notes.push(format!(
                    "{} {} still outdated ({})",
                    o.name, x.installed, x.newest
                ));
            }
            None => d.notes.push(format!(
                "{} {} -> {} upgraded",
                o.name, o.installed, o.newest
            )),
        }
    }
    if left {
        d.notes
            .push(format!("{what} exited 0 but left names outdated"));
        d.end(Status::Failed)
    } else {
        d.end(Status::Updated)
    }
}

/// A string field of a JSON object, or empty.
fn field(v: &serde_json::Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `mise outdated --json`: an object keyed by tool name, with `current` and `latest`. The
/// global configuration applies, because the updater runs outside any project directory.
fn mise_outdated(env: &Env) -> Result<Vec<Item>, (String, bool)> {
    let out = call(
        env.provider,
        "mise",
        &args(&["outdated", "--json"]),
        &[],
        env.timeouts.short,
    )?;
    let bad = |why: String| (format!("mise outdated --json: {why}"), false);
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| bad(format!("not JSON ({e})")))?;
    let tools = v.as_object().ok_or_else(|| bad("not an object".into()))?;
    Ok(tools
        .iter()
        .map(|(name, t)| Item {
            name: name.clone(),
            installed: field(t, "current"),
            newest: field(t, "latest"),
        })
        .collect())
}

/// Each installed `(tool, version)`, from `mise ls --installed --json`: an object keyed by tool
/// name, each an array of records with `version` (and `installed`, never `false` here). It
/// reads the install directory and fetches nothing.
fn mise_installed(env: &Env) -> Result<BTreeSet<(String, String)>, (String, bool)> {
    let out = call(
        env.provider,
        "mise",
        &args(&["ls", "--installed", "--json"]),
        &[],
        env.timeouts.short,
    )?;
    let bad = |why: &str| (format!("mise ls --installed --json: {why}"), false);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| bad("not JSON"))?;
    let mut set = BTreeSet::new();
    for (tool, versions) in v.as_object().ok_or_else(|| bad("not an object"))? {
        for r in versions
            .as_array()
            .ok_or_else(|| bad("a tool is not a list"))?
        {
            if r.get("installed").and_then(|i| i.as_bool()) == Some(false) {
                continue;
            }
            let version = r
                .get("version")
                .and_then(|x| x.as_str())
                .ok_or_else(|| bad("a record has no version"))?;
            set.insert((tool.clone(), version.to_string()));
        }
    }
    Ok(set)
}

/// `mise install` for the configured tools that are missing, then `mise upgrade` without
/// `--bump`: the newest version inside each configured range, so no configuration file is
/// written. Each tool version the install adds is a note, and makes the row `updated`. mise
/// removes the replaced version after its own grace period (`upgrade.prune_after`).
fn mise(mut d: Draft, env: &Env) -> Row {
    if let Err(e) = env.provider.resolve("mise") {
        d.notes.push(e.detail);
        return d.end(Status::Blocked);
    }
    let had = match mise_installed(env) {
        Ok(s) => s,
        Err(e) => return d.stop(e),
    };
    if let Err(e) = call(
        env.provider,
        "mise",
        &args(&["install"]),
        &[],
        env.timeouts.long,
    ) {
        return d.stop(e);
    }
    let has = match mise_installed(env) {
        Ok(s) => s,
        Err(e) => return d.stop(e),
    };
    let added = has.difference(&had).count();
    d.notes.extend(
        has.difference(&had)
            .map(|(tool, version)| format!("{tool} {version} installed")),
    );
    let list = || mise_outdated(env);
    let before = match list() {
        Ok(l) => l,
        Err(e) => return d.stop(e),
    };
    let upgrade = |_: &[Item]| {
        call(
            env.provider,
            "mise",
            &args(&["upgrade"]),
            &[],
            env.timeouts.long,
        )
    };
    let mut row = roll(d, "mise upgrade", before, &upgrade, &list);
    if added > 0 && row.status == Status::Current {
        row.status = Status::Updated;
    }
    row
}

/// `rustup check`: one line per toolchain and one for rustup itself, `<name> - update
/// available: <installed> -> <newest>` or `<name> - up to date: <version>`. It exits 100 when
/// an update is available, and 0 when none is.
fn rustup_check(r: &Runner, env: &Env) -> Result<Vec<Item>, (String, bool)> {
    let out = call_ok(
        r,
        "rustup",
        &args(&["check"]),
        &[],
        env.timeouts.short,
        &[0, 100],
    )?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once(" - ")?;
            let (_, versions) = rest.split_once("update available")?;
            let (installed, newest) = versions.trim_start_matches([' ', ':']).split_once("->")?;
            Some(Item {
                name: name.trim().to_string(),
                installed: installed.trim().to_string(),
                newest: newest.trim().to_string(),
            })
        })
        .collect())
}

/// `rustup update`: every installed toolchain and rustup itself. No toolchain is removed.
fn rustup(mut d: Draft, env: &Env) -> Row {
    let r = match native_runner(Updater::Rustup, env) {
        Ok(r) => r,
        Err(why) => {
            d.notes.push(why);
            return d.end(Status::Blocked);
        }
    };
    let list = || rustup_check(&r, env);
    let before = match list() {
        Ok(l) => l,
        Err(e) => return d.stop(e),
    };
    let upgrade = |_: &[Item]| call(&r, "rustup", &args(&["update"]), &[], env.timeouts.long);
    roll(d, "rustup update", before, &upgrade, &list)
}

/// The global npm packages except `npm`, from `npm ls -g --depth=0 --json` (`dependencies`
/// keys). The list exits 1 when npm finds a problem in the tree; the names are still read.
fn npm_names(env: &Env) -> Result<Vec<String>, String> {
    let why = |n: String| format!("cannot list the global npm packages: {n}");
    let out = call_ok(
        env.provider,
        "npm",
        &args(&["ls", "-g", "--depth=0", "--json"]),
        &npm_env(&[]),
        env.timeouts.short,
        &[0, 1],
    )
    .map_err(|(n, _)| why(n))?;
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| why(format!("not JSON ({e})")))?;
    // `npm` itself is held (see `npm_outdated`), so it needs no grant.
    Ok(v.get("dependencies")
        .and_then(|d| d.as_object())
        .map(|d| d.keys().filter(|n| *n != "npm").cloned().collect())
        .unwrap_or_default())
}

/// `npm outdated -g --json`: an object keyed by package name with `current`, `wanted` and
/// `latest`. It exits 1 when a package is outdated. The rolling items and the held notes:
/// `npm` itself is held, because Homebrew's node formula owns it, and a package already at
/// its `wanted` version is held there, because `npm update` reaches no further.
fn npm_outdated(env: &Env, names: &[String]) -> Result<(Vec<Item>, Vec<String>), (String, bool)> {
    let out = call_ok(
        env.provider,
        "npm",
        &args(&["outdated", "-g", "--json"]),
        &npm_env(names),
        env.timeouts.short,
        &[0, 1],
    )?;
    let bad = |why: String| (format!("npm outdated -g --json: {why}"), false);
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok((vec![], vec![]));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| bad(format!("not JSON ({e})")))?;
    let packages = v.as_object().ok_or_else(|| bad("not an object".into()))?;
    // A failed registry request is `{"error": {"code": ..., "summary": ...}}` with exit 1, not
    // a listing.
    if let Some(e) = packages.get("error") {
        return Err(bad(format!(
            "npm reported an error ({})",
            e.get("code").and_then(|c| c.as_str()).unwrap_or("no code")
        )));
    }
    let (mut items, mut held) = (Vec::new(), Vec::new());
    for (name, p) in packages {
        let text = |k: &str| {
            p.get(k)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .ok_or_else(|| bad(format!("the entry {name} has no {k}")))
        };
        let (current, wanted) = (text("current")?, text("wanted")?);
        let latest = field(p, "latest");
        if name == "npm" {
            held.push(format!(
                "npm {current} held (Homebrew's node formula owns it; latest {latest})"
            ));
        } else if current == wanted {
            held.push(format!("{name} {current} held at wanted (latest {latest})"));
        } else {
            items.push(Item {
                name: name.clone(),
                installed: current,
                newest: wanted,
            });
        }
    }
    Ok((items, held))
}

/// `npm update -g <names>` of every outdated global package that is not held, on the default
/// registry that the `npm:<name>` grants cover.
fn npm(mut d: Draft, env: &Env) -> Row {
    if let Err(e) = env.provider.resolve("npm") {
        d.notes.push(e.detail);
        return d.end(Status::Blocked);
    }
    // One listing: the names give the grants to check and the scopes to pin.
    let names = match npm_names(env) {
        Ok(n) => n,
        Err(why) => {
            d.notes.push(why);
            return d.end(Status::Blocked);
        }
    };
    let untrusted = untrusted(
        names.iter().map(|n| Ok(format!("{NPM}{n}"))).collect(),
        env.grants,
    );
    if !untrusted.is_empty() {
        d.notes.extend(untrusted);
        return d.end(Status::Blocked);
    }
    let (before, held) = match npm_outdated(env, &names) {
        Ok(l) => l,
        Err(e) => return d.stop(e),
    };
    d.notes.extend(held);
    // Only a name the checked listing holds is updated: the grants were checked against it.
    let unlisted: Vec<&str> = before
        .iter()
        .filter(|i| !names.contains(&i.name))
        .map(|i| i.name.as_str())
        .collect();
    if !unlisted.is_empty() {
        d.notes.push(format!(
            "outdated but not in the global listing, so not checked and not updated: {}",
            unlisted.join(", ")
        ));
        return d.end(Status::Blocked);
    }
    let list = || npm_outdated(env, &names).map(|(l, _)| l);
    let upgrade = |items: &[Item]| {
        let mut a = args(&["update", "-g"]);
        a.extend(items.iter().map(|i| i.name.clone()));
        call(env.provider, "npm", &a, &npm_env(&names), env.timeouts.long)
    };
    roll(d, "npm update -g", before, &upgrade, &list)
}

// ---- uv tools ------------------------------------------------------------------------------

/// Where uv keeps its tools, relative to HOME: `<name>/uv-receipt.toml` records each tool's
/// source. The runner clears the environment, so `UV_TOOL_DIR` and `XDG_DATA_HOME`, which
/// would move it, are never set.
const UV_TOOLS: &str = ".local/share/uv/tools";

fn uv_env() -> Vec<(String, String)> {
    vec![("UV_PYTHON_DOWNLOADS".into(), "never".into())]
}

/// The installed uv tools by name, from the tools directory. A missing directory is no tool.
fn uv_tool_names(env: &Env) -> Result<Vec<String>, String> {
    let dir = env.home.join(UV_TOOLS);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let mut names = Vec::new();
    for e in entries {
        let e = e.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        if e.path().join("uv-receipt.toml").is_file() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The identities the uv tools are built from, read from each receipt: a `git` requirement
/// (its `?rev=` query aside) is mapped as it is, so a value that maps to no git origin never
/// reaches git, and then as git fetches it (`git ls-remote --get-url` applies the user's
/// `url.*.insteadOf` rewrites, locally); a directory, an editable path or an index name needs
/// none; any other source (a `url` archive) maps to no identity, and so does a legacy string
/// requirement or an unreadable receipt.
fn uv_identities(env: &Env) -> Vec<Result<String, String>> {
    let names = match uv_tool_names(env) {
        Ok(n) => n,
        Err(why) => return vec![Err(why)],
    };
    let mut ids = Vec::new();
    for name in names {
        let receipt = env.home.join(UV_TOOLS).join(&name).join("uv-receipt.toml");
        let why = |n: String| format!("uv tool {name}: {n}");
        let doc: toml::Table = match std::fs::read_to_string(&receipt)
            .map_err(|e| format!("cannot read {}: {e}", receipt.display()))
            .and_then(|s| {
                s.parse()
                    .map_err(|e: toml::de::Error| format!("{}: {e}", receipt.display()))
            }) {
            Ok(d) => d,
            Err(n) => {
                ids.push(Err(why(n)));
                continue;
            }
        };
        // Every list under `[tool]` but the entry points is a source list an upgrade resolves
        // again: `requirements`, `constraints`, `overrides`, and any list a newer uv records.
        let lists = doc
            .get("tool")
            .and_then(|t| t.as_table())
            .into_iter()
            .flat_map(|t| t.iter())
            .filter(|(k, _)| k.as_str() != "entrypoints")
            .filter_map(|(k, v)| v.as_array().map(|a| (k.clone(), a.clone())))
            .collect::<Vec<_>>();
        for (list, entries) in lists {
            for r in entries {
                // uv's legacy receipt form is one PEP 508 string per requirement, whose source
                // these keys cannot show (as `package::uv_receipt_source` holds it).
                if !r.is_table() {
                    ids.push(Err(why(format!(
                    "a legacy string entry in the receipt's {list}, whose source cannot be checked; reinstall the tool with uv tool install"
                ))));
                    continue;
                }
                if let Some(git) = r.get("git").and_then(|g| g.as_str()) {
                    let url = git.split(['?', '#']).next().unwrap_or_default();
                    ids.push(
                        git_id(url)
                            .and_then(|_| fetch_url(env, url))
                            .and_then(|u| git_id(&u))
                            .map_err(why),
                    );
                } else if r.get("directory").is_none()
                    && r.get("editable").is_none()
                    && r.get("path").is_none()
                    && r.get("url").is_some()
                {
                    ids.push(Err(why(format!(
                        "the source {} maps to no identity; upgrade it by hand",
                        field_toml(&r, "url")
                    ))));
                }
            }
        }
    }
    ids
}

fn field_toml(v: &toml::Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `uv tool list`: `<name> v<version>` lines, each followed by its `- <entrypoint>` lines.
fn uv_tool_list(env: &Env) -> Result<Vec<(String, String)>, (String, bool)> {
    let out = call(
        env.provider,
        "uv",
        &args(&["tool", "list"]),
        &uv_env(),
        env.timeouts.short,
    )?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with("- ") && !l.trim().is_empty())
        .filter_map(|l| {
            let (name, version) = l.rsplit_once(" v")?;
            Some((name.trim().to_string(), version.trim().to_string()))
        })
        .collect())
}

/// `uv tool upgrade <name>` for each tool, one command each, so one failure leaves the others
/// their upgrade. The listing decides the status: a changed version is `updated`.
fn uv(mut d: Draft, env: &Env) -> Row {
    if let Err(e) = env.provider.resolve("uv") {
        d.notes.push(e.detail);
        return d.end(Status::Blocked);
    }
    let untrusted = trust_notes(Updater::Uv, env);
    if !untrusted.is_empty() {
        d.notes.extend(untrusted);
        return d.end(Status::Blocked);
    }
    let names = match uv_tool_names(env) {
        Ok(n) => n,
        Err(why) => return d.stop((why, false)),
    };
    if names.is_empty() {
        d.notes.push("no uv tool is installed".into());
        return d.end(Status::Current);
    }
    let before = match uv_tool_list(env) {
        Ok(l) => l,
        Err(e) => return d.stop(e),
    };
    let mut failure = None;
    for name in &names {
        match call(
            env.provider,
            "uv",
            &args(&["tool", "upgrade", name]),
            &uv_env(),
            env.timeouts.long,
        ) {
            // A stop ends the updater here: no other uv command starts.
            Err(e) if e.1 => return d.stop(e),
            Err(e) if failure.is_none() => failure = Some(e),
            Err(e) => d.notes.push(e.0),
            Ok(_) => {}
        }
    }
    let after = match uv_tool_list(env) {
        Ok(l) => l,
        Err(e) => return d.stop(e),
    };
    let mut changed = false;
    for (name, v) in &before {
        if let Some((_, now)) = after.iter().find(|(n, _)| n == name)
            && now != v
        {
            changed = true;
            d.notes.push(format!("{name} {v} -> {now} upgraded"));
        }
    }
    match failure {
        Some(e) => d.stop(e),
        None if changed => d.end(Status::Updated),
        None => d.end(Status::Current),
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

/// The environment of `skills update` and of every `npm` command: the default npm registry, so
/// a registry that an npmrc names cannot serve a package that an `npm:` grant covers. npm
/// resolves a scoped package through `@<scope>:registry`, which `npm_config_registry` does
/// not override, so each scope among `names` is pinned to the default registry too.
fn npm_env(names: &[String]) -> Vec<(String, String)> {
    let mut v = vec![
        ("npm_config_registry".to_string(), NPM_REGISTRY.to_string()),
        // An npmrc `link=true` limits `npm ls` to linked packages; the listing must be whole.
        ("npm_config_link".to_string(), "false".to_string()),
    ];
    let scopes: BTreeSet<&str> = names
        .iter()
        .filter_map(|n| n.split_once('/').map(|(scope, _)| scope))
        .filter(|scope| scope.starts_with('@'))
        .collect();
    v.extend(scopes.into_iter().map(|scope| {
        (
            format!("npm_config_{scope}:registry"),
            NPM_REGISTRY.to_string(),
        )
    }));
    v
}

/// The git sources that `skills update` fetches: `source` and `sourceUrl` of each entry of the
/// `skills` CLI's record (version 3:
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
    let mut values = Vec::new();
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

/// The identity of a git remote url, or its note.
fn git_id(url: &str) -> Result<String, String> {
    git_identity(url, cfg!(feature = "test-isolation"))
        .map_err(|e| format!("{}: {}", e.code.as_str(), e.detail))
}

/// The url git fetches from for `url`: `git ls-remote --get-url` applies `url.*.insteadOf`
/// from the user's git config and makes no network call.
fn fetch_url(env: &Env, url: &str) -> Result<String, String> {
    let why = |n: String| {
        format!("cannot resolve the source {url} with git ls-remote --get-url: {n}; nothing ran")
    };
    env.provider.resolve("git").map_err(|e| why(e.detail))?;
    let out = call(
        env.provider,
        "git",
        &args(&["ls-remote", "--get-url", url]),
        &[],
        env.timeouts.short,
    )
    .map_err(|(n, _)| why(n))?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "" => Err(why("it printed no url".into())),
        u => Ok(u.to_string()),
    }
}

/// The identities `skills update` fetches: `npm:skills` for the installer, then each record
/// source as git clones it. A value is mapped before git sees it, so a value that maps to no
/// git origin never reaches git; then its fetch url is mapped again.
fn skills_identities(env: &Env, values: &[String]) -> Vec<Result<String, String>> {
    let mut ids = vec![Ok(format!("{NPM}{SKILLS_PACKAGE}"))];
    ids.extend(values.iter().map(|v| {
        let id = git_id(v)?;
        // The `skills` CLI clones a GitHub shorthand as `https://github.com/<o>/<r>.git`.
        let clone = if v.contains(':') {
            v.clone()
        } else {
            format!("{id}.git")
        };
        git_id(&fetch_url(env, &clone)?)
    }));
    ids
}

/// The notes for the identities one updater fetches: each note as it is, and one per identity
/// without a grant, with its `--trust-origin` command.
fn untrusted(ids: Vec<Result<String, String>>, grants: &Grants) -> Vec<String> {
    let mut notes = Vec::new();
    let mut seen = BTreeSet::new();
    for id in ids {
        match id {
            Ok(id) => {
                seen.insert(id);
            }
            Err(note) => notes.push(note),
        }
    }
    notes.extend(seen.iter().filter(|id| !grants.trusts(id)).map(|id| {
        format!(
            "{}: no grant for {id}; review it, then run {}",
            Code::SourceUntrusted.as_str(),
            trust_command(id)
        )
    }));
    notes
}

/// Why the updater must not run because of its sources, one note each, or nothing. `skills`
/// and the `agents` managed copy fetch extension sources, each global npm package runs its
/// install scripts, and a uv tool from a git source is built from what it fetches: each such
/// source needs a grant (DFM-25, C04), except an `agents` managed copy that is a declared
/// checkout of the same repository. The `agents` working copy is never fetched, and the
/// other updaters update a binary or packages from their owner: none of them is gated. An
/// unreadable source record is one note. An updater with a note runs no command that fetches
/// or installs.
pub fn trust_notes(u: Updater, env: &Env) -> Vec<String> {
    match u {
        Updater::Skills => match skills_sources(env) {
            Ok(v) => untrusted(skills_identities(env, &v), env.grants),
            Err(why) => vec![why],
        },
        Updater::Npm => match npm_names(env) {
            Ok(names) => untrusted(
                names.iter().map(|n| Ok(format!("{NPM}{n}"))).collect(),
                env.grants,
            ),
            Err(why) => vec![why],
        },
        Updater::Uv => untrusted(uv_identities(env), env.grants),
        Updater::Agents => {
            let (copy, working) = agents_copy(env);
            if working || std::fs::symlink_metadata(copy.join(".git")).is_err() {
                return vec![];
            }
            match agents_upstream(env, &copy, AGENTS_NOUN) {
                Ok(Ok(up)) => agents_gate(env, &copy, &up).into_iter().collect(),
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

/// Reads the upstream with local git commands only; nothing is fetched. `noun` names the copy
/// in the notes. `Ok(Err)` is the blocked reason (no upstream branch, or a remote url git
/// cannot give); `Err` is a command that did not run or was stopped.
fn agents_upstream(
    env: &Env,
    copy: &Path,
    noun: &str,
) -> Result<Result<Upstream, String>, (String, bool)> {
    let none = || {
        format!(
            "the {noun} {} has no upstream branch; not pulled",
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
/// The agent registry's managed copy in the notes.
const AGENTS_NOUN: &str = "managed copy";

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

/// The bound checkout's recipe that the `side-channels` updater runs.
const SIDE_CHANNELS_RECIPE: &str = "sync-side-channels";

/// The bound checkout's `Justfile`, once `just` resolves on the provider path. `Err` is the
/// blocked reason.
fn side_channels_justfile(env: &Env) -> Result<std::path::PathBuf, String> {
    let j = env.root.join("Justfile");
    if !j.is_file() {
        return Err(format!("no Justfile at {}", j.display()));
    }
    env.provider.resolve("just").map_err(|e| e.detail)?;
    Ok(j)
}

/// The checkout's side channels: `just sync-side-channels` in the bound checkout. The recipe is
/// fixed here and the checkout is the one sync fetched from a granted origin, as for the
/// `agents` install script. It reports no version, so a success is `current`.
fn side_channels(mut d: Draft, env: &Env) -> Row {
    let j = match side_channels_justfile(env) {
        Ok(j) => j,
        Err(why) => {
            d.notes.push(why);
            return d.end(Status::Blocked);
        }
    };
    let a = vec![
        "--justfile".to_string(),
        j.display().to_string(),
        "--working-directory".to_string(),
        env.root.display().to_string(),
        SIDE_CHANNELS_RECIPE.to_string(),
    ];
    match call(env.provider, "just", &a, &[], env.timeouts.long) {
        Ok(_) => {
            d.notes
                .push(format!("{SIDE_CHANNELS_RECIPE} ran; it reports no version"));
            d.end(Status::Current)
        }
        Err(e) => d.stop(e),
    }
}

/// `git -C <dir> <a>`; its trimmed standard output. As in the `git-checkout/v1` probe, git must
/// not walk up from a `.git` that is not a repository to an enclosing one.
fn git(env: &Env, dir: &Path, a: &[&str]) -> Result<String, (String, bool)> {
    let mut all = args(&["-C", &dir.display().to_string()]);
    all.extend(args(a));
    let ceiling: Vec<(String, String)> = dir
        .parent()
        .map(|p| p.canonicalize().unwrap_or(p.into()))
        .map(|c| {
            (
                "GIT_CEILING_DIRECTORIES".to_string(),
                c.display().to_string(),
            )
        })
        .into_iter()
        .collect();
    let out = call(env.provider, "git", &all, &ceiling, env.timeouts.short)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The note that blocks the pull of the `agents` managed copy, or none. The upstream's grant
/// covers the pull and the registry's own install code. A checkout that the bound checkout
/// declared at the copy with the upstream's identity needs no grant.
fn agents_gate(env: &Env, copy: &Path, up: &Upstream) -> Option<String> {
    let id = git_id(&up.url);
    if let Ok(i) = &id
        && env
            .checkouts
            .iter()
            .flatten()
            .any(|c| c.owner_declared && c.path == copy && git_id(&c.url).as_ref() == Ok(i))
    {
        return None;
    }
    // One identity gives at most one note.
    untrusted(vec![id], env.grants).into_iter().next()
}

/// Fast-forwards the clean copy at `copy` from its upstream, after `gate` passed for that
/// upstream. `noun` names the copy in the notes. `before` gets the revision that is read
/// first. `Ok(Ok(after))` is the revision after the pull. `Ok(Err)` is the blocked note
/// (dirty, no upstream, the gate, or diverged), and nothing changed. `Err` is a command that
/// did not succeed. Nothing here merges, resets or clones.
fn fast_forward(
    env: &Env,
    copy: &Path,
    noun: &str,
    before: &mut Option<String>,
    gate: impl Fn(&Upstream) -> Option<String>,
) -> Result<Result<String, String>, (String, bool)> {
    // The copy's own configuration first, by sync's checkout rule (docs/sync.md, "Checkout
    // configuration"): a key there can run a command (`core.sshCommand`, `core.fsmonitor`) or
    // send the fetch elsewhere. `--local` reads no include, so an `include.path` is refused too.
    let listing = git(
        env,
        copy,
        &["config", "--list", "--local", "--show-origin", "-z"],
    )?;
    if let Some(key) = crate::sync_cmd::refused_config_key(&listing) {
        return Ok(Err(format!(
            "{noun} {} configures {key}, which git would read during the fetch or the pull; remove it from the copy's own configuration; not pulled",
            copy.display()
        )));
    }
    // Read-only checks next: the revision and the dirty state.
    *before = Some(git(env, copy, &["rev-parse", "--short", "HEAD"])?);
    if !git(env, copy, &["status", "--porcelain"])?.is_empty() {
        return Ok(Err(format!(
            "{noun} {} has uncommitted changes; not pulled",
            copy.display()
        )));
    }
    // No upstream is a precondition, like a missing copy: nothing to fast-forward from.
    let up = match agents_upstream(env, copy, noun)? {
        Ok(up) => up,
        Err(why) => return Ok(Err(why)),
    };
    if let Some(why) = gate(&up) {
        return Ok(Err(why));
    }
    // The fetch and the pull name the checked remote, so git cannot pick another one. `fetch`
    // moves only the remote-tracking refs. A local commit that is not on the upstream means the
    // copy diverged: `pull --ff-only` is not tried.
    git(env, copy, &["fetch", "--quiet", &up.remote])?;
    let n = git(env, copy, &["rev-list", "--count", "@{u}..HEAD"])?;
    if n != "0" {
        return Ok(Err(format!(
            "{noun} {} has {n} local commits that are not on its upstream; not pulled",
            copy.display()
        )));
    }
    git(
        env,
        copy,
        &["pull", "--ff-only", "--quiet", &up.remote, &up.merge],
    )?;
    git(env, copy, &["rev-parse", "--short", "HEAD"]).map(Ok)
}

/// The declared checkouts that the `checkouts` updater pulls. When the profile declares
/// `agents`, its managed copy is left out: the `agents` updater pulls it before its install.
fn declared_checkouts<'e>(env: &'e Env) -> impl Iterator<Item = &'e Checkout> {
    let agents = env.agents_declared.then(|| agents_copy(env).0);
    env.checkouts
        .iter()
        .flatten()
        .filter(move |c| agents.as_ref() != Some(&c.path))
}

/// Fast-forwards each declared checkout, one note each. A blocked or failed checkout does not
/// stop the others. The row is the worst of them: `failed`, then `blocked`, then `updated`,
/// then `current`. A checkout that the bound checkout declared needs no grant; one that a nested
/// source declared needs a grant for its url. Its upstream must have the declared url's
/// identity, else it is blocked.
fn checkouts(mut d: Draft, env: &Env) -> Row {
    if let Err(e) = env.provider.resolve("git") {
        d.notes.push(e.detail);
        return d.end(Status::Blocked);
    }
    let rank = |s: Status| match s {
        Status::Failed => 3,
        Status::Blocked => 2,
        Status::Updated => 1,
        _ => 0,
    };
    // A declaration that is not valid is noted; the others still run.
    let invalid: Vec<&String> = env
        .checkouts
        .iter()
        .filter_map(|c| c.as_ref().err())
        .collect();
    let mut status = if invalid.is_empty() {
        Status::Current
    } else {
        Status::Blocked
    };
    d.notes.extend(invalid.into_iter().cloned());
    for c in declared_checkouts(env) {
        if std::fs::symlink_metadata(c.path.join(".git")).is_err() {
            d.notes.push(format!(
                "{} is not cloned yet; the packages scope clones it",
                c.path.display()
            ));
            continue;
        }
        let mut before = None;
        // The upstream must fetch from the declared url, which needs a grant unless the bound
        // checkout declared it. Both go through the owner's `url.*.insteadOf` rewrites: `git
        // ls-remote --get-url` in the copy rewrites the declared url as `remote get-url` did
        // the upstream, and makes no network call.
        let declared = |up: &Upstream| {
            let rewritten =
                git(env, &c.path, &["ls-remote", "--get-url", &c.url]).map_err(|(why, _)| why);
            match (git_id(&up.url), rewritten.and_then(|u| git_id(&u))) {
                (Ok(a), Ok(b)) if a == b => (!c.owner_declared)
                    .then(|| {
                        untrusted(vec![git_id(&c.url)], env.grants)
                            .into_iter()
                            .next()
                    })
                    .flatten(),
                _ => Some(format!(
                    "{}: {} fetches {}, not the declared {}; not pulled",
                    c.id,
                    c.path.display(),
                    up.url,
                    c.url
                )),
            }
        };
        let (note, s) = match fast_forward(env, &c.path, "checkout", &mut before, declared) {
            Ok(Ok(after)) => match before {
                Some(b) if b != after => (format!("{} {b} -> {after}", c.id), Status::Updated),
                _ => (format!("{} current at {after}", c.id), Status::Current),
            },
            Ok(Err(why)) => (why, Status::Blocked),
            // A stop request starts no more commands.
            Err((why, true)) => return d.stop((why, true)),
            Err((why, false)) => (why, Status::Failed),
        };
        d.notes.push(note);
        if rank(s) > rank(status) {
            status = s;
        }
    }
    d.end(status)
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
    if working {
        match git(env, &copy, &["rev-parse", "--short", "HEAD"]) {
            Ok(v) => d.before = Some(v),
            Err(e) => return d.stop(e),
        }
        let dirty = match git(env, &copy, &["status", "--porcelain"]) {
            Ok(s) => !s.is_empty(),
            Err(e) => return d.stop(e),
        };
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
        match fast_forward(env, &copy, AGENTS_NOUN, &mut d.before, |up| {
            agents_gate(env, &copy, up)
        }) {
            Ok(Ok(after)) => d.after = Some(after),
            Ok(Err(why)) => {
                d.notes.push(why);
                return d.end(Status::Blocked);
            }
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
        Updater::Checkouts => match env.provider.resolve("git") {
            Err(e) => format!("blocked: {}", e.detail),
            Ok(_) => {
                let paths: Vec<String> = declared_checkouts(env)
                    .map(|c| c.path.display().to_string())
                    .collect();
                // An invalid declaration blocks the row, so the preview counts it.
                let invalid = env.checkouts.iter().filter(|c| c.is_err()).count();
                let plural = |n: usize| if n == 1 { "" } else { "s" };
                let would = match paths.len() {
                    0 if invalid == 0 => return "nothing declared".into(),
                    0 => "would fast-forward no checkout".to_string(),
                    n => format!(
                        "would fast-forward {n} checkout{} ({})",
                        plural(n),
                        paths.join(", ")
                    ),
                };
                match invalid {
                    0 => would,
                    n => format!("{would}; {n} declaration{} invalid", plural(n)),
                }
            }
        },
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
        Updater::Mise => match env.provider.resolve("mise") {
            Ok(p) => format!(
                "would run mise ls --installed --json, mise install, mise outdated --json, mise upgrade ({})",
                p.display()
            ),
            Err(e) => format!("blocked: {}", e.detail),
        },
        Updater::Npm => match env.provider.resolve("npm") {
            Ok(p) => format!(
                "would run npm outdated -g, npm update -g of every outdated global package except npm ({})",
                p.display()
            ),
            Err(e) => format!("blocked: {}", e.detail),
        },
        Updater::Rustup => match native_runner(u, env) {
            Ok(r) => format!(
                "would run rustup check, rustup update ({})",
                r.resolve("rustup")
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
            Err(why) => format!("blocked: {why}"),
        },
        Updater::SideChannels => match side_channels_justfile(env) {
            Ok(j) => format!(
                "would run just {SIDE_CHANNELS_RECIPE} in {} ({})",
                env.root.display(),
                j.display()
            ),
            Err(why) => format!("blocked: {why}"),
        },
        Updater::Uv => match env.provider.resolve("uv") {
            Ok(p) => format!(
                "would run uv tool upgrade of each tool under {} ({})",
                env.home.join(UV_TOOLS).display(),
                p.display()
            ),
            Err(e) => format!("blocked: {}", e.detail),
        },
        Updater::Claude | Updater::Codex | Updater::Opencode => match native_runner(u, env) {
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
        checkouts: Vec<Result<Checkout, String>>,
        agents_declared: bool,
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
            // `git ls-remote --get-url <url>` prints `st/mirror` when it exists (an insteadOf
            // rewrite), else the url; `st/lsremote-fail` makes it fail. It logs nothing;
            // `agents` replaces it with its own look-alike.
            let st = root.join("st");
            fs::create_dir_all(&st).unwrap();
            fs::write(
                bin.join("git"),
                format!(
                    "#!/bin/sh\nst={}\n[ \"$1 $2\" = 'ls-remote --get-url' ] || exit 64\n[ -f \"$st/lsremote-fail\" ] && exit 2\nif [ -f \"$st/mirror\" ]; then cat \"$st/mirror\"; else echo \"$3\"; fi\n",
                    st.display()
                ),
            )
            .unwrap();
            fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
            T {
                dots: root.join("dots"),
                root,
                home,
                bin,
                runner,
                grants,
                checkouts: vec![],
                agents_declared: false,
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
                checkouts: &self.checkouts,
                agents_declared: self.agents_declared,
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
                "case \"$3\" in\n  rev-parse) if [ \"$4\" = --abbrev-ref ]; then echo main; else cat \"$st/rev\"; fi ;;\n  config) [ \"$4\" = --list ] && exit 0; [ -f \"$st/no-upstream\" ] && exit 1; case \"$5\" in *.remote) cat \"$st/remote\" 2>/dev/null || echo origin ;; *) echo refs/heads/main ;; esac ;;\n  remote) cat \"$st/url-$5\" ;;\n  status) cat \"$st/porcelain\" 2>/dev/null || true ;;\n  fetch) exit 0 ;;\n  rev-list) cat \"$st/ahead\" 2>/dev/null || echo 0 ;;\n  pull) cp \"$st/rev-after\" \"$st/rev\" ;;\n  *) exit 64 ;;\nesac",
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
            format!("git -C {c} config --list --local --show-origin -z"),
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
                "E_SOURCE_UNTRUSTED: no grant for {REGISTRY_ID}; review it, then run dfm sync --trust-origin '{REGISTRY_ID}'"
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
                "E_SOURCE_UNTRUSTED: no grant for ssh://git@github.com/evil/agents; review it, then run dfm sync --trust-origin 'ssh://git@github.com/evil/agents'"
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

    impl T {
        /// `git` for declared checkouts: each one is the directory `<home>/src/<id>`, and its
        /// state is under `st/<id>`: `rev`, `rev-after` (copied over `rev` by `pull`),
        /// `porcelain`, `ahead` (the `rev-list` count, 0 when absent), `no-upstream`, and
        /// `fail-<command>` that makes that command exit 2. The upstream is
        /// `origin`, `refs/heads/main`, at `https://github.com/o/<id>.git`.
        fn checkouts_git(&self) {
            self.script(
                &self.bin.join("git"),
                "d=$(basename \"$2\")\n[ -f \"$st/$d/fail-$3\" ] && exit 2\ncase \"$3\" in\n  rev-parse) if [ \"$4\" = --abbrev-ref ]; then echo main; else cat \"$st/$d/rev\"; fi ;;\n  config) [ \"$4\" = --list ] && exit 0; [ -f \"$st/$d/no-upstream\" ] && exit 1; case \"$5\" in *.remote) echo origin ;; *) echo refs/heads/main ;; esac ;;\n  remote) echo \"https://github.com/o/$d.git\" ;;\n  ls-remote) echo \"$5\" ;;\n  status) cat \"$st/$d/porcelain\" 2>/dev/null || true ;;\n  fetch) exit 0 ;;\n  rev-list) cat \"$st/$d/ahead\" 2>/dev/null || echo 0 ;;\n  pull) cp \"$st/$d/rev-after\" \"$st/$d/rev\" ;;\n  *) exit 64 ;;\nesac",
            );
        }

        /// A declared checkout `id`, cloned at `rev`, whose pull moves it to `rev_after`.
        fn checkout(&mut self, id: &str, rev: &str, rev_after: &str) -> PathBuf {
            let st = self.root.join("st").join(id);
            fs::create_dir_all(&st).unwrap();
            fs::write(st.join("rev"), rev).unwrap();
            fs::write(st.join("rev-after"), rev_after).unwrap();
            let path = self.home.join("src").join(id);
            fs::create_dir_all(path.join(".git")).unwrap();
            self.checkouts.push(Ok(Checkout {
                id: id.into(),
                url: format!("https://github.com/o/{id}.git"),
                path: path.clone(),
                owner_declared: true,
            }));
            path
        }
    }

    /// The calls that fast-forward the checkout at `c` from `origin`.
    fn checkout_calls(c: &Path) -> Vec<String> {
        let c = c.display().to_string();
        let mut v = upstream_calls(&c, "origin");
        let id = c.rsplit('/').next().unwrap_or_default();
        v.extend([
            format!("git -C {c} ls-remote --get-url https://github.com/o/{id}.git"),
            format!("git -C {c} fetch --quiet origin"),
            format!("git -C {c} rev-list --count @{{u}}..HEAD"),
            format!("git -C {c} pull --ff-only --quiet origin refs/heads/main"),
            format!("git -C {c} rev-parse --short HEAD"),
        ]);
        v
    }

    #[test]
    fn checkouts_fast_forwards_each_declared_checkout() {
        let mut t = T::new();
        t.checkouts_git();
        // A declared checkout needs no grant.
        t.grants = Grants::default();
        let tpm = t.checkout("tpm", "a1b2c3d", "d4e5f6a");
        let other = t.checkout("other", "a1b2c3d", "a1b2c3d");
        assert_eq!(
            preview(Updater::Checkouts, &t.env()),
            format!(
                "would fast-forward 2 checkouts ({}, {})",
                tpm.display(),
                other.display()
            )
        );
        assert_eq!(t.log(), "");
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(
            r.notes,
            ["tpm a1b2c3d -> d4e5f6a", "other current at a1b2c3d"]
        );
        let mut want = checkout_calls(&tpm);
        want.extend(checkout_calls(&other));
        assert_eq!(t.calls(), want);
    }

    /// A checkout that a nested source declares needs a grant for its url (docs/sync.md,
    /// "Source identities"); the other checkouts still run.
    #[test]
    fn checkouts_needs_a_grant_for_a_nested_declaration() {
        let mut t = T::new();
        t.checkouts_git();
        t.grants = Grants::default();
        t.checkout("tpm", "a1b2c3d", "d4e5f6a");
        let ext = t.checkout("ext", "a1b2c3d", "d4e5f6a");
        t.checkouts[1].as_mut().unwrap().owner_declared = false;
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                "tpm a1b2c3d -> d4e5f6a".to_string(),
                format!(
                    "E_SOURCE_UNTRUSTED: no grant for https://github.com/o/ext; review it, then run {}",
                    trust_command("https://github.com/o/ext")
                )
            ]
        );
        // Nothing was fetched for the nested declaration.
        assert!(
            !t.calls()
                .iter()
                .any(|c| c.starts_with(&format!("git -C {} fetch", ext.display()))),
            "{:?}",
            t.calls()
        );
        t.grants
            .grant("https://github.com/o/ext", "2026-11-12T00:00:00Z");
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(r.notes[1], "ext a1b2c3d -> d4e5f6a", "{r:?}");
    }

    #[test]
    fn checkouts_with_nothing_declared_is_current() {
        let t = T::new();
        assert_eq!(preview(Updater::Checkouts, &t.env()), "nothing declared");
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(t.log(), "");
    }

    #[test]
    fn one_dirty_checkout_does_not_stop_the_others() {
        let mut t = T::new();
        t.checkouts_git();
        let dirty = t.checkout("dirty", "a1b2c3d", "d4e5f6a");
        fs::write(t.root.join("st/dirty/porcelain"), " M x\n").unwrap();
        let moved = t.checkout("moved", "a1b2c3d", "d4e5f6a");
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                format!(
                    "checkout {} has uncommitted changes; not pulled",
                    dirty.display()
                ),
                "moved a1b2c3d -> d4e5f6a".to_string(),
            ]
        );
        let d = dirty.display().to_string();
        let mut want = upstream_calls(&d, "origin")[..3].to_vec();
        want.extend(checkout_calls(&moved));
        assert_eq!(t.calls(), want);
    }

    #[test]
    fn a_checkout_not_cloned_yet_is_a_note() {
        let mut t = T::new();
        t.checkouts_git();
        let path = t.checkout("tpm", "a1b2c3d", "d4e5f6a");
        fs::remove_dir(path.join(".git")).unwrap();
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(
            r.notes,
            [format!(
                "{} is not cloned yet; the packages scope clones it",
                path.display()
            )]
        );
        assert_eq!(t.log(), "");
    }

    #[test]
    fn checkouts_skips_the_agents_managed_copy() {
        let mut t = T::new();
        t.checkouts_git();
        t.agents_declared = true;
        let copy = t.managed();
        t.checkouts.push(Ok(Checkout {
            id: "agent-registry".into(),
            url: REGISTRY.into(),
            path: copy,
            owner_declared: true,
        }));
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert!(r.notes.is_empty(), "{r:?}");
        assert_eq!(t.log(), "");
        assert_eq!(preview(Updater::Checkouts, &t.env()), "nothing declared");
    }

    #[test]
    fn checkouts_pulls_the_managed_copy_when_agents_is_not_declared() {
        let mut t = T::new();
        t.checkouts_git();
        let copy = t.managed();
        let st = t.root.join("st/agent-registry");
        fs::create_dir_all(&st).unwrap();
        fs::write(st.join("rev"), "a1b2c3d").unwrap();
        fs::write(st.join("rev-after"), "d4e5f6a").unwrap();
        t.checkouts.push(Ok(Checkout {
            id: "agent-registry".into(),
            url: "https://github.com/o/agent-registry.git".into(),
            path: copy.clone(),
            owner_declared: true,
        }));
        assert_eq!(
            preview(Updater::Checkouts, &t.env()),
            format!("would fast-forward 1 checkout ({})", copy.display())
        );
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(r.notes, ["agent-registry a1b2c3d -> d4e5f6a"]);
        assert_eq!(t.calls(), checkout_calls(&copy));
    }

    #[test]
    fn git_for_a_copy_stops_at_the_copy_parent() {
        let mut t = T::new();
        let ceiling = t.root.join("st/ceiling");
        t.script(
            &t.bin.join("git"),
            "printf '%s\\n' \"$GIT_CEILING_DIRECTORIES\" >> \"$st/ceiling\"; exit 2",
        );
        // `agents` runs its git commands through the same `git` helper.
        let path = t.checkout("tpm", "a1b2c3d", "a1b2c3d");
        run(Updater::Checkouts, &t.env());
        assert_eq!(
            fs::read_to_string(&ceiling).unwrap(),
            format!("{}\n", path.parent().unwrap().display())
        );
    }

    #[test]
    fn checkouts_failed_outranks_blocked_and_updated() {
        let mut t = T::new();
        t.checkouts_git();
        t.checkout("broken", "a1b2c3d", "d4e5f6a");
        fs::write(t.root.join("st/broken/fail-fetch"), "").unwrap();
        let dirty = t.checkout("dirty", "a1b2c3d", "d4e5f6a");
        fs::write(t.root.join("st/dirty/porcelain"), " M x\n").unwrap();
        t.checkout("moved", "a1b2c3d", "d4e5f6a");
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes[0].contains("fetch --quiet origin exited 2"),
            "{r:?}"
        );
        assert_eq!(
            r.notes[1..],
            [
                format!(
                    "checkout {} has uncommitted changes; not pulled",
                    dirty.display()
                ),
                "moved a1b2c3d -> d4e5f6a".to_string(),
            ]
        );
    }

    #[test]
    fn a_checkout_that_fetches_an_undeclared_url_is_blocked_before_any_fetch() {
        let mut t = T::new();
        t.checkouts_git();
        let fork = t.checkout("fork", "a1b2c3d", "d4e5f6a");
        // The directory tracks `https://github.com/o/fork.git`; the declaration names another.
        t.checkouts[0].as_mut().unwrap().url = "https://github.com/o/other.git".into();
        let moved = t.checkout("moved", "a1b2c3d", "d4e5f6a");
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                format!(
                    "fork: {} fetches https://github.com/o/fork.git, not the declared https://github.com/o/other.git; not pulled",
                    fork.display()
                ),
                "moved a1b2c3d -> d4e5f6a".to_string(),
            ]
        );
        let f = fork.display().to_string();
        let mut want = upstream_calls(&f, "origin");
        want.push(format!(
            "git -C {f} ls-remote --get-url https://github.com/o/other.git"
        ));
        want.extend(checkout_calls(&moved));
        assert_eq!(t.calls(), want);
    }

    #[test]
    fn checkouts_diverged_or_without_an_upstream_are_blocked() {
        let mut t = T::new();
        t.checkouts_git();
        let ahead = t.checkout("ahead", "a1b2c3d", "d4e5f6a");
        fs::write(t.root.join("st/ahead/ahead"), "2\n").unwrap();
        let loose = t.checkout("loose", "a1b2c3d", "d4e5f6a");
        fs::write(t.root.join("st/loose/no-upstream"), "").unwrap();
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                format!(
                    "checkout {} has 2 local commits that are not on its upstream; not pulled",
                    ahead.display()
                ),
                format!(
                    "the checkout {} has no upstream branch; not pulled",
                    loose.display()
                ),
            ]
        );
        assert!(!t.log().contains(" pull "), "{}", t.log());
        assert!(!t.log().contains("loose fetch"), "{}", t.log());
    }

    #[test]
    fn an_invalid_checkout_declaration_is_a_note_and_the_others_run() {
        let mut t = T::new();
        t.checkouts_git();
        t.checkouts.push(Err(
            "extra.bad: url o/r is not an https:// or ssh remote".into()
        ));
        t.checkout("moved", "a1b2c3d", "d4e5f6a");
        let moved = t.home.join("src/moved");
        assert_eq!(
            preview(Updater::Checkouts, &t.env()),
            format!(
                "would fast-forward 1 checkout ({}); 1 declaration invalid",
                moved.display()
            )
        );
        let r = run(Updater::Checkouts, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                "extra.bad: url o/r is not an https:// or ssh remote",
                "moved a1b2c3d -> d4e5f6a"
            ]
        );
    }

    #[test]
    fn agents_needs_no_grant_for_a_declared_copy() {
        let mut t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        t.grants = Grants::default();
        // Another spelling of the upstream url: the identities match.
        t.checkouts.push(Ok(Checkout {
            id: "agent-registry".into(),
            url: "ssh://git@github.com/o/agents.git".into(),
            path: copy.clone(),
            owner_declared: true,
        }));
        assert!(trust_notes(Updater::Agents, &t.env()).is_empty());
        assert!(preview(Updater::Agents, &t.env()).starts_with("would fast-forward "));
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        // The same path declared with another repository keeps the grant check.
        t.checkouts[0].as_mut().unwrap().url = "https://github.com/o/other.git".into();
        t.agents("abc1234", "def5678");
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(r.notes[0].starts_with("E_SOURCE_UNTRUSTED: "), "{r:?}");
    }

    /// The `agents` grant skip holds only for a copy the bound checkout declared.
    #[test]
    fn agents_needs_a_grant_for_a_copy_a_nested_source_declares() {
        let mut t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        t.grants = Grants::default();
        t.checkouts.push(Ok(Checkout {
            id: "agent-registry".into(),
            url: "ssh://git@github.com/o/agents.git".into(),
            path: copy,
            owner_declared: false,
        }));
        let notes = trust_notes(Updater::Agents, &t.env());
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("E_SOURCE_UNTRUSTED: "), "{notes:?}");
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
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
                "E_SOURCE_UNTRUSTED: no grant for npm:skills; review it, then run dfm sync --trust-origin 'npm:skills'"
            ]
        );
        assert_eq!(t.log(), "");
    }

    #[test]
    fn side_channels_runs_the_checkout_recipe() {
        let t = T::new();
        t.script(
            &t.bin.join("just"),
            "exit $(cat \"$st/just-exit\" 2>/dev/null || echo 0)",
        );
        fs::create_dir_all(&t.dots).unwrap();
        let j = t.dots.join("Justfile");
        fs::write(&j, "").unwrap();
        let want = format!(
            "just --justfile {} --working-directory {} sync-side-channels",
            j.display(),
            t.dots.display()
        );
        assert!(
            preview(Updater::SideChannels, &t.env())
                .starts_with("would run just sync-side-channels"),
        );
        let r = run(Updater::SideChannels, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(t.calls(), std::slice::from_ref(&want));

        fs::create_dir_all(t.root.join("st")).unwrap();
        fs::write(t.root.join("st/just-exit"), "3").unwrap();
        let r = run(Updater::SideChannels, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert_eq!(t.calls(), [want.clone(), want]);
    }

    #[test]
    fn side_channels_without_a_justfile_is_blocked() {
        let t = T::new();
        t.script(&t.bin.join("just"), "exit 0");
        assert!(preview(Updater::SideChannels, &t.env()).starts_with("blocked: no Justfile at "));
        let r = run(Updater::SideChannels, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
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
            assert!(p.contains(&trust_command(id)), "{p}");
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
    fn skills_checks_the_url_git_rewrites_to() {
        let t = T::new();
        t.script(&t.bin.join("mise"), "exit 0");
        // The literal source is granted, but insteadOf sends the clone to a mirror.
        fs::write(
            t.root.join("st/mirror"),
            "https://mirror.invalid/o/skill.git\n",
        )
        .unwrap();
        let r = run(Updater::Skills, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            [
                "E_SOURCE_UNTRUSTED: no grant for https://mirror.invalid/o/skill; review it, then run dfm sync --trust-origin 'https://mirror.invalid/o/skill'"
            ]
        );
        assert!(preview(Updater::Skills, &t.env()).starts_with("blocked: E_SOURCE_UNTRUSTED"));
        assert_eq!(t.log(), "");
    }

    #[test]
    fn skills_whose_url_git_cannot_resolve_is_blocked() {
        let t = T::new();
        t.script(&t.bin.join("mise"), "exit 0");
        fs::write(t.root.join("st/lsremote-fail"), "").unwrap();
        let r = run(Updater::Skills, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes[0].starts_with("cannot resolve the source "),
            "{r:?}"
        );
        assert!(r.notes[0].ends_with("; nothing ran"), "{r:?}");
        assert_eq!(t.log(), "");
    }

    #[test]
    fn agents_with_an_npm_upstream_is_blocked_although_npm_is_granted() {
        let t = T::new();
        t.agents("abc1234", "def5678");
        let copy = t.managed();
        // Git reads `npm:skills` as host `npm`; the npm:skills grant must not cover it.
        fs::write(t.root.join("st/url-origin"), "npm:skills").unwrap();
        assert!(t.grants.trusts("npm:skills"));
        let r = run(Updater::Agents, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes[0].starts_with(
                "E_SOURCE_UNTRUSTED: git remote \"npm:skills\" names an npm: identity"
            ),
            "{r:?}"
        );
        assert_eq!(
            t.calls(),
            upstream_calls(&copy.display().to_string(), "origin")
        );
    }

    #[test]
    fn binaries_and_package_managers_are_not_gated() {
        let mut t = T::new();
        t.grants = Grants::default();
        for u in [
            Updater::Claude,
            Updater::Codex,
            Updater::Opencode,
            Updater::Brew,
            Updater::Mise,
            Updater::Rustup,
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
        assert_eq!(t.calls(), upstream_calls(&c, "origin")[3..]);
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
    fn skills_runs_against_the_default_npm_registry() {
        let t = T::new();
        let seen = t.root.join("registry");
        t.script(
            &t.bin.join("mise"),
            &format!("printf '%s' \"$npm_config_registry\" > {}", seen.display()),
        );
        let r = run(Updater::Skills, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(
            fs::read_to_string(&seen).unwrap(),
            "https://registry.npmjs.org/"
        );
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

    #[test]
    fn a_trust_command_quotes_the_identity_as_one_shell_word() {
        let id = "https://h/x$(touch /nonexistent/p)'`id`;\" $HOME";
        let cmd = trust_command(id);
        let quoted = cmd.strip_prefix("dfm sync --trust-origin '").unwrap();
        assert_eq!(cmd, format!("dfm sync --trust-origin '{}", quoted));
        // The shell reads the quoted identity back as one argument, byte for byte.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- '{quoted}; printf '%s|%s' \"$#\" \"$1\""))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("1|{id}"));
        assert_eq!(
            trust_command("npm:skills"),
            "dfm sync --trust-origin 'npm:skills'"
        );
    }

    // ---- mise, rustup, npm, uv (DFM-85) ------------------------------------------------------

    impl T {
        /// `mise` whose `ls --installed --json` prints `st/mise-ls` (`{}` when absent), whose
        /// bare `install` copies `st/mise-ls-after` over it when that exists, whose `outdated
        /// --json` prints `st/mise-outdated` and whose bare `upgrade` replaces it with
        /// `st/mise-after`. Any other upgrade or install form (`--bump`, a name) exits 65.
        fn mise(&self, outdated: &str, after: &str) {
            fs::write(self.root.join("st/mise-outdated"), outdated).unwrap();
            fs::write(self.root.join("st/mise-after"), after).unwrap();
            self.script(
                &self.bin.join("mise"),
                "case \"$1\" in\n  ls) [ \"$2 $3\" = '--installed --json' ] || exit 65; cat \"$st/mise-ls\" 2>/dev/null || echo '{}' ;;\n  install) [ -z \"$2\" ] || exit 65; [ -f \"$st/mise-ls-after\" ] && cp \"$st/mise-ls-after\" \"$st/mise-ls\"; exit 0 ;;\n  outdated) cat \"$st/mise-outdated\" ;;\n  upgrade) [ -z \"$2\" ] || exit 65; cp \"$st/mise-after\" \"$st/mise-outdated\" ;;\n  *) exit 64 ;;\nesac",
            );
        }

        /// `rustup` in `~/.cargo/bin` whose `check` prints `st/rustup-check` and exits 100 when
        /// a line says `update available`, as the real one does; `update` replaces the text
        /// with `st/rustup-after`.
        fn rustup(&self, check: &str, after: &str) {
            fs::write(self.root.join("st/rustup-check"), check).unwrap();
            fs::write(self.root.join("st/rustup-after"), after).unwrap();
            self.script(
                &self.home.join(".cargo/bin/rustup"),
                "case \"$1\" in\n  check) cat \"$st/rustup-check\"; grep -q 'update available' \"$st/rustup-check\" && exit 100; exit 0 ;;\n  update) cp \"$st/rustup-after\" \"$st/rustup-check\" ;;\n  *) exit 64 ;;\nesac",
            );
        }

        /// `npm` whose `ls` prints `st/npm-ls`, whose `outdated` prints `st/npm-outdated` and
        /// exits 1 when it names a package, as the real one does, and whose `update` replaces
        /// it with `st/npm-after`. Each call appends its registry to `st/npm-env`.
        fn npm(&self, ls: &str, outdated: &str, after: &str) {
            fs::write(self.root.join("st/npm-ls"), ls).unwrap();
            fs::write(self.root.join("st/npm-outdated"), outdated).unwrap();
            fs::write(self.root.join("st/npm-after"), after).unwrap();
            self.script(
                &self.bin.join("npm"),
                "env | grep '^npm_config_' | sort | tr '\\n' ' ' >> \"$st/npm-env\"; echo >> \"$st/npm-env\"\ncase \"$1\" in\n  ls) cat \"$st/npm-ls\" ;;\n  outdated) cat \"$st/npm-outdated\"; grep -q current \"$st/npm-outdated\" && exit 1; exit 0 ;;\n  update) cp \"$st/npm-after\" \"$st/npm-outdated\" ;;\n  *) exit 64 ;;\nesac",
            );
        }

        /// `uv` whose `tool list` prints `st/uv-list` and whose `tool upgrade <name>` replaces
        /// it with `st/uv-after`, or exits 1 with the `st/uv-fail-<name>` marker. Each
        /// `(name, requirement)` is a receipt under the tools directory.
        fn uv(&self, tools: &[(&str, &str)], list: &str, after: &str) {
            fs::write(self.root.join("st/uv-list"), list).unwrap();
            fs::write(self.root.join("st/uv-after"), after).unwrap();
            for (name, req) in tools {
                let dir = self.home.join(UV_TOOLS).join(name);
                fs::create_dir_all(&dir).unwrap();
                fs::write(
                    dir.join("uv-receipt.toml"),
                    format!("[tool]\nrequirements = [{{ name = \"{name}\", {req} }}]\nentrypoints = []\n"),
                )
                .unwrap();
            }
            self.script(
                &self.bin.join("uv"),
                "case \"$1 $2\" in\n  'tool list') cat \"$st/uv-list\" ;;\n  'tool upgrade') [ -f \"$st/uv-fail-$3\" ] && exit 1; cp \"$st/uv-after\" \"$st/uv-list\" ;;\n  *) exit 64 ;;\nesac",
            );
        }
    }

    const MISE_TWO: &str = r#"{"go":{"name":"go","requested":"1.27","current":"1.27.3","latest":"1.27.4"},"usage":{"name":"usage","requested":"latest","current":"2.0.0","latest":"2.1.0"}}"#;
    const MISE_GO: &str =
        r#"{"go":{"name":"go","requested":"1.27","current":"1.27.3","latest":"1.27.4"}}"#;

    #[test]
    fn mise_upgrades_without_bump_and_accounts_each_name() {
        let t = T::new();
        t.mise(MISE_TWO, "{}");
        let r = run(Updater::Mise, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(
            r.notes,
            vec![
                "go 1.27.3 -> 1.27.4 upgraded".to_string(),
                "usage 2.0.0 -> 2.1.0 upgraded".to_string()
            ]
        );
        assert_eq!(
            t.calls(),
            vec![
                "mise ls --installed --json".to_string(),
                "mise install".to_string(),
                "mise ls --installed --json".to_string(),
                "mise outdated --json".to_string(),
                "mise upgrade".to_string(),
                "mise outdated --json".to_string()
            ]
        );
        assert!(
            preview(Updater::Mise, &t.env()).starts_with(
                "would run mise ls --installed --json, mise install, mise outdated --json, mise upgrade"
            ),
            "{}",
            preview(Updater::Mise, &t.env())
        );
    }

    #[test]
    fn mise_installing_a_missing_tool_is_updated_although_nothing_is_outdated() {
        let t = T::new();
        t.mise("{}", "{}");
        fs::write(
            t.root.join("st/mise-ls"),
            r#"{"go":[{"version":"1.27.3","installed":true}]}"#,
        )
        .unwrap();
        fs::write(
            t.root.join("st/mise-ls-after"),
            r#"{"go":[{"version":"1.27.3","installed":true}],"node":[{"version":"22.1.0","installed":true}]}"#,
        )
        .unwrap();
        let r = run(Updater::Mise, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(r.notes, ["node 22.1.0 installed"]);
        // A listing that is not JSON stops the row as failed, before the install.
        let t = T::new();
        t.mise("{}", "{}");
        fs::write(t.root.join("st/mise-ls"), "mise ERROR").unwrap();
        let r = run(Updater::Mise, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes[0].starts_with("mise ls --installed --json: not JSON"),
            "{r:?}"
        );
        assert!(
            !t.calls().contains(&"mise install".to_string()),
            "{:?}",
            t.calls()
        );
    }

    #[test]
    fn mise_with_nothing_outdated_is_current_and_runs_no_upgrade() {
        let t = T::new();
        t.mise("{}", "{}");
        let r = run(Updater::Mise, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(
            t.calls(),
            vec![
                "mise ls --installed --json".to_string(),
                "mise install".to_string(),
                "mise ls --installed --json".to_string(),
                "mise outdated --json".to_string()
            ]
        );
    }

    #[test]
    fn mise_name_that_stays_outdated_is_failed() {
        let t = T::new();
        t.mise(MISE_TWO, MISE_GO);
        let r = run(Updater::Mise, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes
                .contains(&"go 1.27.3 still outdated (1.27.4)".to_string()),
            "{r:?}"
        );
        assert!(
            r.notes
                .contains(&"usage 2.0.0 -> 2.1.0 upgraded".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn mise_missing_is_blocked_and_invalid_json_is_failed() {
        let t = T::new();
        let r = run(Updater::Mise, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(preview(Updater::Mise, &t.env()).starts_with("blocked: "));
        t.mise("mise ERROR", "{}");
        let r = run(Updater::Mise, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes[0].starts_with("mise outdated --json: not JSON"),
            "{r:?}"
        );
    }

    const RUSTUP_TWO: &str = "stable-aarch64-apple-darwin - update available: 1.98.1 (48a229cea 2026-09-01) -> 1.99.1 (58cf8f9c3 2026-10-13)\n1.98-aarch64-apple-darwin - up to date: 1.98.1 (48a229cea 2026-09-01)\nrustup - update available : 1.29.2 -> 1.29.3\n";
    const RUSTUP_NONE: &str = "stable-aarch64-apple-darwin - up to date: 1.99.1 (58cf8f9c3 2026-10-13)\n1.98-aarch64-apple-darwin - up to date: 1.98.1 (48a229cea 2026-09-01)\nrustup - up to date : 1.29.3\n";

    #[test]
    fn rustup_reads_exit_100_as_the_list_and_updates() {
        let t = T::new();
        t.rustup(RUSTUP_TWO, RUSTUP_NONE);
        let r = run(Updater::Rustup, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(
            r.notes,
            vec![
                "stable-aarch64-apple-darwin 1.98.1 (48a229cea 2026-09-01) -> 1.99.1 (58cf8f9c3 2026-10-13) upgraded".to_string(),
                "rustup 1.29.2 -> 1.29.3 upgraded".to_string()
            ]
        );
        assert_eq!(
            t.calls(),
            vec![
                "rustup check".to_string(),
                "rustup update".to_string(),
                "rustup check".to_string()
            ]
        );
    }

    #[test]
    fn rustup_up_to_date_is_current_and_a_stale_toolchain_is_failed() {
        let t = T::new();
        t.rustup(RUSTUP_NONE, RUSTUP_NONE);
        let r = run(Updater::Rustup, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(t.calls(), vec!["rustup check".to_string()]);
        t.rustup(RUSTUP_TWO, RUSTUP_TWO);
        let r = run(Updater::Rustup, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            r.notes
                .contains(&"rustup 1.29.2 still outdated (1.29.3)".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn rustup_missing_from_cargo_bin_is_blocked_with_the_path() {
        let t = T::new();
        let r = run(Updater::Rustup, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(r.notes[0].contains(".cargo/bin/rustup"), "{r:?}");
        assert!(preview(Updater::Rustup, &t.env()).contains(".cargo/bin/rustup"));
    }

    const NPM_LS: &str = r#"{"name":"lib","dependencies":{"@webos-tools/cli":{"version":"3.2.5"},"npm":{"version":"11.20.0"}}}"#;
    const NPM_OUTDATED: &str = r#"{"@webos-tools/cli":{"current":"3.2.5","wanted":"3.2.6","latest":"3.2.6"},"npm":{"current":"11.20.0","wanted":"12.2.2","latest":"12.2.2"}}"#;
    const NPM_ONLY: &str = r#"{"npm":{"current":"11.20.0","wanted":"12.2.2","latest":"12.2.2"}}"#;

    #[test]
    fn npm_without_a_grant_for_each_package_is_blocked_before_any_outdated() {
        let t = T::new();
        t.npm(NPM_LS, NPM_OUTDATED, NPM_ONLY);
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            vec![format!(
                "E_SOURCE_UNTRUSTED: no grant for npm:@webos-tools/cli; review it, then run {}",
                trust_command("npm:@webos-tools/cli")
            )]
        );
        // npm itself is held, so it needs no grant.
        assert_eq!(t.calls(), vec!["npm ls -g --depth=0 --json".to_string()]);
        assert!(preview(Updater::Npm, &t.env()).starts_with("blocked: E_SOURCE_UNTRUSTED"));
    }

    #[test]
    fn npm_updates_the_granted_packages_and_holds_npm_on_the_default_registry() {
        let mut t = T::new();
        t.grants
            .grant("npm:@webos-tools/cli", "2026-11-06T00:00:00Z");
        t.npm(NPM_LS, NPM_OUTDATED, NPM_ONLY);
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(
            r.notes,
            vec![
                "npm 11.20.0 held (Homebrew's node formula owns it; latest 12.2.2)".to_string(),
                "@webos-tools/cli 3.2.5 -> 3.2.6 upgraded".to_string()
            ]
        );
        assert_eq!(
            t.calls(),
            vec![
                "npm ls -g --depth=0 --json".to_string(),
                "npm outdated -g --json".to_string(),
                "npm update -g @webos-tools/cli".to_string(),
                "npm outdated -g --json".to_string()
            ]
        );
        // The listing, before the names are known, pins the registry; every later command
        // pins the scope's registry too.
        let env = fs::read_to_string(t.root.join("st/npm-env")).unwrap();
        let lines: Vec<&str> = env.lines().collect();
        assert_eq!(
            lines[0], "npm_config_link=false npm_config_registry=https://registry.npmjs.org/ ",
            "{env}"
        );
        assert_eq!(lines.len(), 4, "{env}");
        assert!(
            lines[1..].iter().all(|l| *l
                == "npm_config_@webos-tools:registry=https://registry.npmjs.org/ npm_config_link=false npm_config_registry=https://registry.npmjs.org/ "),
            "{env}"
        );
    }

    #[test]
    fn npm_package_at_wanted_is_held_and_nothing_outdated_is_current() {
        let mut t = T::new();
        t.grants
            .grant("npm:@webos-tools/cli", "2026-11-06T00:00:00Z");
        let at_wanted =
            r#"{"@webos-tools/cli":{"current":"3.2.6","wanted":"3.2.6","latest":"4.0.0"}}"#;
        t.npm(NPM_LS, at_wanted, at_wanted);
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(
            r.notes,
            vec!["@webos-tools/cli 3.2.6 held at wanted (latest 4.0.0)".to_string()]
        );
        assert!(!t.log().contains("npm update"), "{}", t.log());
        t.npm(NPM_LS, "{}", "{}");
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert!(r.notes.is_empty(), "{r:?}");
    }

    const UV_LIST: &str = "a v1.0\n- a\nb v2.0\n- b\n- b-cli\n";
    const UV_AFTER: &str = "a v1.0\n- a\nb v3.0\n- b\n- b-cli\n";

    #[test]
    fn uv_upgrades_each_tool_from_a_local_or_granted_source() {
        let mut t = T::new();
        t.grants
            .grant("https://github.com/o/b", "2026-11-06T00:00:00Z");
        t.uv(
            &[
                ("a", "directory = \"/x/a\""),
                ("b", "git = \"https://github.com/o/b.git?rev=2026.10.17\""),
            ],
            UV_LIST,
            UV_AFTER,
        );
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Updated, "{r:?}");
        assert_eq!(r.notes, vec!["b 2.0 -> 3.0 upgraded".to_string()]);
        assert_eq!(
            t.calls(),
            vec![
                "uv tool list".to_string(),
                "uv tool upgrade a".to_string(),
                "uv tool upgrade b".to_string(),
                "uv tool list".to_string()
            ]
        );
    }

    #[test]
    fn uv_with_an_ungranted_git_source_is_blocked_before_any_command() {
        let t = T::new();
        t.uv(
            &[
                ("a", "editable = \"/x/a\""),
                ("b", "git = \"ssh://git@github.com/o/b\""),
            ],
            UV_LIST,
            UV_AFTER,
        );
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            vec![format!(
                "E_SOURCE_UNTRUSTED: no grant for ssh://git@github.com/o/b; review it, then run {}",
                trust_command("ssh://git@github.com/o/b")
            )]
        );
        assert_eq!(t.calls(), Vec::<String>::new());
        assert!(preview(Updater::Uv, &t.env()).starts_with("blocked: E_SOURCE_UNTRUSTED"));
    }

    #[test]
    fn uv_checks_the_url_git_rewrites_to() {
        let mut t = T::new();
        t.grants
            .grant("https://github.com/o/b", "2026-11-06T00:00:00Z");
        t.uv(
            &[("b", "git = \"https://github.com/o/b.git?rev=1\"")],
            UV_LIST,
            UV_AFTER,
        );
        // The literal source is granted, but insteadOf sends the fetch to a mirror.
        fs::write(t.root.join("st/mirror"), "https://mirror.invalid/o/b.git\n").unwrap();
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            vec![format!(
                "E_SOURCE_UNTRUSTED: no grant for https://mirror.invalid/o/b; review it, then run {}",
                trust_command("https://mirror.invalid/o/b")
            )]
        );
        assert_eq!(t.calls(), Vec::<String>::new());
        fs::write(t.root.join("st/lsremote-fail"), "").unwrap();
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes[0]
                .starts_with("uv tool b: cannot resolve the source https://github.com/o/b.git"),
            "{r:?}"
        );
    }

    #[test]
    fn uv_legacy_string_requirement_is_blocked() {
        let t = T::new();
        t.uv(&[("a", "directory = \"/x/a\"")], UV_LIST, UV_AFTER);
        fs::write(
            t.home.join(UV_TOOLS).join("a/uv-receipt.toml"),
            "[tool]\nrequirements = [\"a @ git+https://example.invalid/o/a\"]\n",
        )
        .unwrap();
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes[0]
                .starts_with("uv tool a: a legacy string entry in the receipt's requirements"),
            "{r:?}"
        );
        assert_eq!(t.calls(), Vec::<String>::new());
    }

    #[test]
    fn npm_error_document_and_bad_entry_are_failed_not_current() {
        let mut t = T::new();
        t.grants
            .grant("npm:@webos-tools/cli", "2026-11-06T00:00:00Z");
        let error = r#"{"error":{"code":"E403","summary":"forbidden"}}"#;
        t.npm(NPM_LS, error, error);
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert_eq!(
            r.notes,
            vec!["npm outdated -g --json: npm reported an error (E403)".to_string()]
        );
        assert!(!t.log().contains("npm update"), "{}", t.log());
        // An error after the upgrade is a failed row, not evidence of an upgrade.
        t.npm(NPM_LS, NPM_OUTDATED, error);
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(
            t.log().contains("npm update -g @webos-tools/cli"),
            "{}",
            t.log()
        );
        t.npm(NPM_LS, r#"{"@webos-tools/cli":{"latest":"3.2.6"}}"#, "{}");
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert_eq!(
            r.notes,
            vec!["npm outdated -g --json: the entry @webos-tools/cli has no current".to_string()]
        );
    }

    #[test]
    fn uv_constraint_sources_need_a_grant_too() {
        let t = T::new();
        t.uv(&[("a", "directory = \"/x/a\"")], UV_LIST, UV_AFTER);
        fs::write(
            t.home.join(UV_TOOLS).join("a/uv-receipt.toml"),
            "[tool]\nrequirements = [{ name = \"a\", directory = \"/x/a\" }]\nconstraints = [{ name = \"dep\", git = \"https://github.com/o/dep\" }]\noverrides = [\"x @ git+https://example.invalid/o/x\"]\nentrypoints = [{ name = \"a\", install-path = \"/x/bin/a\", from = \"a\" }]\n",
        )
        .unwrap();
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("no grant for https://github.com/o/dep")),
            "{r:?}"
        );
        assert!(
            r.notes
                .iter()
                .any(|n| n
                    .starts_with("uv tool a: a legacy string entry in the receipt's overrides")),
            "{r:?}"
        );
        assert_eq!(t.calls(), Vec::<String>::new());
    }

    #[test]
    fn npm_updates_only_the_names_the_checked_listing_holds() {
        let mut t = T::new();
        t.grants
            .grant("npm:@webos-tools/cli", "2026-11-06T00:00:00Z");
        // A filtered listing (an npmrc `link=true`) names no package; outdated still does.
        t.npm(
            r#"{"name":"lib","dependencies":{}}"#,
            NPM_OUTDATED,
            NPM_ONLY,
        );
        let r = run(Updater::Npm, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(
            r.notes.iter().any(|n| n
                == "outdated but not in the global listing, so not checked and not updated: @webos-tools/cli"),
            "{r:?}"
        );
        assert!(!t.log().contains("npm update"), "{}", t.log());
        let env = fs::read_to_string(t.root.join("st/npm-env")).unwrap();
        assert!(
            env.lines().all(|l| l.contains("npm_config_link=false ")),
            "{env}"
        );
    }

    #[test]
    fn uv_url_source_and_unreadable_receipt_are_blocked() {
        let t = T::new();
        t.uv(&[("a", "url = \"https://h/a.tar.gz\"")], UV_LIST, UV_AFTER);
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert_eq!(
            r.notes,
            vec![
                "uv tool a: the source https://h/a.tar.gz maps to no identity; upgrade it by hand"
                    .to_string()
            ]
        );
        fs::write(t.home.join(UV_TOOLS).join("a/uv-receipt.toml"), "[tool\n").unwrap();
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Blocked, "{r:?}");
        assert!(r.notes[0].starts_with("uv tool a: "), "{r:?}");
    }

    #[test]
    fn uv_failed_tool_still_upgrades_the_others_and_is_failed() {
        let t = T::new();
        t.uv(
            &[("a", "directory = \"/x/a\""), ("b", "directory = \"/x/b\"")],
            UV_LIST,
            UV_AFTER,
        );
        fs::write(t.root.join("st/uv-fail-a"), "").unwrap();
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Failed, "{r:?}");
        assert!(t.log().contains("uv tool upgrade b"), "{}", t.log());
        assert!(
            r.notes
                .iter()
                .any(|n| n.starts_with("uv tool upgrade a exited 1")),
            "{r:?}"
        );
        assert!(
            r.notes.contains(&"b 2.0 -> 3.0 upgraded".to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn uv_same_listing_is_current_and_no_tool_is_current() {
        let t = T::new();
        t.uv(&[("a", "directory = \"/x/a\"")], UV_LIST, UV_LIST);
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert!(t.log().contains("uv tool upgrade a"), "{}", t.log());
        fs::remove_dir_all(t.home.join(UV_TOOLS)).unwrap();
        let r = run(Updater::Uv, &t.env());
        assert_eq!(r.status, Status::Current, "{r:?}");
        assert_eq!(r.notes, vec!["no uv tool is installed".to_string()]);
    }

    #[test]
    fn uv_env_never_downloads_a_python() {
        assert_eq!(
            uv_env(),
            vec![("UV_PYTHON_DOWNLOADS".to_string(), "never".to_string())]
        );
    }
}
