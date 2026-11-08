//! `trial`: try a tool without authoring a module (docs/trial.md). The command asks the package
//! providers which one owns or can install the tool, records a baseline of the tool and its
//! configuration candidates before anything changes, installs the tool through that provider
//! with `--yes-plan`, and keeps the trial record in `state/trials/` for `adopt` (DFM-33) and
//! `purge` (DFM-34). Nothing here writes configuration: a candidate path is observed, never
//! changed.

use crate::adapters::package::{self, HOMEBREW, MISE};
use crate::apply_cmd::{is_digest, label, stale};
use crate::read_fs::read_regular_bounded;
use crate::{Ctx, bounded, cli, paint, read_existing_binding, state_cmd, usage};
use dfm_engine::{Runner, Store, now_ns, signal};
use dfm_model::digest::sha256_hex;
use dfm_model::display::{bounded_visible, visible};
use dfm_model::plan::{Binary, PackageState, file_fingerprint};
use dfm_model::resolve::{Table, Value};
use dfm_model::state::{
    Approval, Baseline, BaselinePath, BinaryId, Class, JournalEntry, MAX_BYTES, OperationRecord,
    Phase, ProfileRef, Receipt, Recovery, RunResult, SCHEMA, SHELLS, Stamp, Status,
    TRIAL_SCHEMA_DATA, TrialInstall, TrialPackage, TrialRecord, count_statuses, rfc3339, valid_id,
    valid_path,
};
use dfm_model::{Code, Exit, Refusal};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::Path;

mod script;
mod window;

/// Effects a trial does not instrument. They are recorded so `purge` treats them as candidates
/// for review and never as owned cleanup scope (DESIGN.md: the manager is not an OS sandbox).
pub(super) const UNKNOWN: [&str; 3] = ["services", "paths-outside-baseline", "provider-caches"];

#[derive(Debug)]
pub struct Args {
    /// The package as the provider names it.
    pub package: String,
    pub id: Option<String>,
    pub provider: Option<String>,
    pub configs: Vec<String>,
    pub preview: bool,
    pub yes_plan: Option<String>,
    pub script: Option<ScriptArgs>,
}

/// The `--script` flags: an install script fetched from `url` and run by `shell`.
#[derive(Debug)]
pub struct ScriptArgs {
    /// An `https://` URL.
    pub url: String,
    /// One of [`SHELLS`]; `sh` when `--shell` is absent.
    pub shell: String,
    pub args: Vec<String>,
    /// `--env NAME=VALUE` pairs: unique names, none from [`FIXED_ENV`].
    pub env: Vec<(String, String)>,
    pub tty: bool,
}

/// Variables the script run sets itself. `--env` cannot override them.
pub(super) const FIXED_ENV: [&str; 15] = [
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "SHELL",
    "PATH",
    "LANG",
    "LC_ALL",
    "TERM",
    "ZDOTDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_BIN_HOME",
];

fn parse_env(pairs: Vec<String>) -> Result<Vec<(String, String)>, Refusal> {
    let mut env: Vec<(String, String)> = Vec::new();
    for p in pairs {
        let (name, val) = p
            .split_once('=')
            .ok_or_else(|| usage("--env takes NAME=VALUE"))?;
        let mut chars = name.chars();
        let plain = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !plain {
            return Err(usage(format!(
                "--env name {} must match [A-Za-z_][A-Za-z0-9_]*",
                visible(name)
            )));
        }
        if FIXED_ENV.contains(&name) {
            return Err(usage(format!("--env cannot set {name}; the run fixes it")));
        }
        if val.contains(['\0', '\n']) {
            return Err(usage(format!("--env {name} holds a NUL byte or a newline")));
        }
        if env.iter().any(|(n, _)| n == name) {
            return Err(usage(format!("--env {name} is given twice")));
        }
        env.push((name.to_string(), val.to_string()));
    }
    Ok(env)
}

/// `trial <package> [--id <trial-id>] [--provider homebrew/v1|mise/v1] [--config <path>]...
/// [--preview] [-y|--yes-plan <digest>]`.
pub fn parse_args(m: &clap::ArgMatches) -> Result<Args, Refusal> {
    let (package, id, provider) = (
        cli::one(m, "package"),
        cli::one(m, "id"),
        cli::one(m, "provider"),
    );
    let (yes, preview, configs) = (
        cli::one(m, "yes-plan"),
        cli::flag(m, "preview"),
        cli::all(m, "config"),
    );
    if preview && yes.is_some() {
        return Err(usage("--preview and --yes-plan cannot be used together"));
    }
    if let Some(d) = &yes
        && !is_digest(d)
    {
        return Err(usage(
            "--yes-plan takes the 64 hex characters of a plan digest",
        ));
    }
    if let Some(p) = &provider
        && ![HOMEBREW, MISE].contains(&p.as_str())
    {
        return Err(usage(format!(
            "--provider is {HOMEBREW} or {MISE}; {p} cannot own a trial"
        )));
    }
    let package = package.ok_or_else(|| usage("trial needs a package name"))?;
    // A mise backend name (`npm:prettier`, `npm:@scope/pkg`) has no short name the record can
    // carry, and `leaf` would turn a scoped one into a plausible tool name.
    if package.contains(':') {
        return Err(usage(format!(
            "the tool name {} must be lowercase ASCII letters, digits, '.', '_' or '-' (1 to 64 characters); a backend-prefixed name cannot own a trial",
            visible(&package)
        )));
    }
    let (url, shell, script_args) = (
        cli::one(m, "script"),
        cli::one(m, "shell"),
        cli::all(m, "arg"),
    );
    let (env, tty) = (cli::all(m, "env"), cli::flag(m, "tty"));
    let script = match url {
        None => {
            if shell.is_some() || !script_args.is_empty() || !env.is_empty() || tty {
                return Err(usage("--shell, --arg, --env and --tty need --script"));
            }
            None
        }
        Some(url) => {
            if provider.is_some() {
                return Err(usage("--script and --provider cannot be used together"));
            }
            if !url.starts_with("https://") {
                return Err(usage("--script takes an https:// URL"));
            }
            if !valid_id(&package) {
                return Err(usage(format!(
                    "the tool name {} must be lowercase ASCII letters, digits, '.', '_' or '-' (1 to 64 characters)",
                    visible(&package)
                )));
            }
            let shell = shell.unwrap_or_else(|| "sh".to_string());
            if !SHELLS.contains(&shell.as_str()) {
                return Err(usage(format!(
                    "--shell is {}; {} cannot run a script",
                    SHELLS.join(", "),
                    visible(&shell)
                )));
            }
            Some(ScriptArgs {
                url,
                shell,
                args: script_args,
                env: parse_env(env)?,
                tty,
            })
        }
    };
    Ok(Args {
        package,
        id,
        provider,
        configs,
        preview,
        yes_plan: yes,
        script,
    })
}

fn package_value(name: &str) -> Value {
    let mut t = Table::new();
    t.insert("name".into(), Value::String(name.into()));
    // The trial's own plan only installs: the package adapters require a policy, and `hold` is
    // the one a baseline can stand behind. Updates come later, as for an adopted tool: `brew` in
    // sync (docs/upgrade.md) and `packages --update` for mise (docs/toolchains.md).
    t.insert("update_policy".into(), Value::String("hold".into()));
    Value::Table(t)
}

/// What the preview shows and the digest covers: the trial, its provider, what it will install
/// or found installed, the baseline observed before any change, and the identity of the binary
/// and the binding that decided it.
struct Decision {
    trial_id: String,
    tool: String,
    /// The provider that lists or will install the package. `None` when only another owner
    /// has it: no provider owns anything then.
    adapter: Option<String>,
    package: String,
    /// The dependencies the install adds, when the trial installs; `None` when the tool
    /// pre-exists and nothing is installed.
    install: Option<Vec<String>>,
    owners: Vec<String>,
    baseline: Vec<BaselinePath>,
    /// The data candidates ([`data_candidates`]), observed as the baseline is.
    data: Vec<BaselinePath>,
    /// Data candidates left out, with the reason: a claim or an observation failed. A path
    /// that is not recorded is never removed, so it blocks nothing.
    skipped: Vec<String>,
    binary: Binary,
    stamp: Stamp,
}

impl Decision {
    fn digest(&self) -> Result<String, Refusal> {
        let view = |ps: &[BaselinePath]| -> Vec<serde_json::Value> {
            ps.iter()
                .map(|p| json!({"path": p.path, "existed": p.existed, "sha256": p.sha256}))
                .collect()
        };
        let (paths, data) = (view(&self.baseline), view(&self.data));
        let doc = json!({
            "trial_id": self.trial_id,
            "tool": self.tool,
            "adapter": self.adapter,
            "package": self.package,
            "install": self.install,
            "owners": self.owners,
            "baseline": paths,
            "data": data,
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

    fn record(&self, created: u64, op_id: &str) -> TrialRecord {
        TrialRecord {
            schema: if self.data.is_empty() {
                SCHEMA
            } else {
                TRIAL_SCHEMA_DATA
            },
            binding: self.stamp.clone(),
            trial_id: self.trial_id.clone(),
            tool: self.tool.clone(),
            created_unix_ns: created,
            baseline: Baseline {
                data: self.data.clone(),
                tool_preexisting: self.install.is_none(),
                paths: self.baseline.clone(),
                owners: self.owners.clone(),
            },
            owned_outputs: vec![],
            package: self.adapter.as_ref().map(|adapter| TrialPackage {
                adapter: adapter.clone(),
                name: self.package.clone(),
                install: self.install.as_ref().map(|deps| TrialInstall {
                    op_id: op_id.into(),
                    dependencies: deps.clone(),
                }),
            }),
            unknown: UNKNOWN.iter().map(|u| u.to_string()).collect(),
            adopted: None,
            script: None,
        }
    }

    /// The adapter a journal entry or receipt operation names.
    fn adapter_name(&self) -> &str {
        self.adapter.as_deref().unwrap_or("other-owner")
    }

    fn print(&self, out: &mut dyn Write, digest: &str) -> std::io::Result<()> {
        writeln!(out, "trial: {} tool={}", self.trial_id, self.tool)?;
        writeln!(
            out,
            "provider: {} package={}",
            self.adapter
                .as_deref()
                .unwrap_or("none (no provider lists it)"),
            visible(&self.package)
        )?;
        match (&self.install, &self.adapter) {
            (Some(deps), Some(adapter)) if deps.is_empty() => {
                writeln!(out, "action: install {} with {adapter}", self.tool)?;
            }
            (Some(deps), Some(adapter)) => writeln!(
                out,
                "action: install {} with {adapter} (adds dependencies: {})",
                self.tool,
                visible(&deps.join(", "))
            )?,
            _ => writeln!(
                out,
                "action: nothing to install; {} pre-exists, owned by {}",
                self.tool,
                bounded(&self.owners.join("; "))
            )?,
        }
        print_observed(out, &self.baseline, &self.data, &self.skipped)?;
        writeln!(
            out,
            "cleanup scope: dfm purge removes the install above and every baseline or data path that was absent; a pre-existing path or tool is never removed"
        )?;
        writeln!(
            out,
            "unknown: {} (candidates for review, never owned cleanup scope)",
            UNKNOWN.join(" ")
        )?;
        writeln!(out, "plan digest: {digest}")
    }
}

/// The `baseline:` and `data:` preview lines, shared by the package and the script trial.
pub(super) fn print_observed(
    out: &mut dyn Write,
    baseline: &[BaselinePath],
    data: &[BaselinePath],
    skipped: &[String],
) -> std::io::Result<()> {
    for (what, ps) in [("baseline", baseline), ("data", data)] {
        for p in ps {
            let seen = match (&p.existed, &p.sha256) {
                (false, _) => "absent".to_string(),
                (true, Some(h)) => format!("file sha256:{h}"),
                (true, None) => "directory".to_string(),
            };
            writeln!(out, "{what}: {} {seen}", visible(&p.path))?;
        }
    }
    for s in skipped {
        writeln!(out, "data: {}", visible(s))?;
    }
    Ok(())
}

/// The code a probe answer blocks with, as the package planner maps it (docs/packages.md).
fn block_code(s: &PackageState) -> Code {
    match s {
        PackageState::Unknown(_) => Code::ProbeFailed,
        PackageState::Unofficial(_) => Code::SourceUntrusted,
        PackageState::SourceBuild(_) => Code::PlatformUnsupported,
        PackageState::Unsupported(_) => Code::PrivilegeUnsupported,
        PackageState::Refused(_) => Code::ManifestInvalid,
        _ => Code::Conflict,
    }
}

/// The providers' answer about a package.
struct Owner {
    /// The provider that lists it or will install it; `None` when only another owner has it.
    adapter: Option<String>,
    /// The dependencies the install adds; `None` when the tool pre-exists.
    install: Option<Vec<String>>,
    owners: Vec<String>,
}

/// Asks the providers about the package. The tool pre-exists when any provider lists it or an
/// executable of its name has another owner; then nothing is installed. Otherwise exactly one
/// provider must be able to install it, or the choice is ambiguous and `--provider` is required.
fn find_owner(runner: &Runner, pkg: &str, provider: Option<&str>) -> Result<Owner, Refusal> {
    let value = package_value(pkg);
    let adapters = provider.map_or(vec![HOMEBREW, MISE], |p| vec![p]);
    let mut states = Vec::new();
    for a in adapters {
        if let Some(stop) = signal::requested() {
            return Err(stop);
        }
        let s = match package::probe(runner, a, &value) {
            Ok(s) => s,
            Err(e) if e.code == Code::ManifestInvalid => return Err(usage(e.detail)),
            Err(e) => PackageState::Unknown(e.detail),
        };
        states.push((a, s));
    }
    let mut owners: Vec<String> = states
        .iter()
        .filter_map(|(a, s)| match s {
            PackageState::Installed(v) => Some(format!("{a} ({v})")),
            PackageState::OtherOwner(why) => Some(why.clone()),
            _ => None,
        })
        .collect();
    owners.dedup();
    if !owners.is_empty() {
        let adapter = states
            .iter()
            .find(|(_, s)| matches!(s, PackageState::Installed(_)))
            .map(|(a, _)| (*a).into());
        return Ok(Owner {
            adapter,
            install: None,
            owners,
        });
    }
    let missing: Vec<(&str, &Vec<String>)> = states
        .iter()
        .filter_map(|(a, s)| match s {
            PackageState::Missing(d) => Some((*a, d)),
            _ => None,
        })
        .collect();
    match missing.as_slice() {
        [(a, deps)] => Ok(Owner {
            adapter: Some((*a).into()),
            install: Some((*deps).clone()),
            owners: vec![],
        }),
        [] => {
            let (code, _) = states
                .iter()
                .map(|(_, s)| (block_code(s), s))
                .min_by_key(|(c, _)| u8::from(*c != Code::ProbeFailed))
                .unwrap_or((Code::ProbeFailed, &PackageState::Unknown(String::new())));
            let why: Vec<String> = states
                .iter()
                .map(|(a, s)| format!("{a}: {}", s.state()))
                .collect();
            Err(Refusal::new(
                code,
                format!(
                    "{pkg} is not installed and no provider can install it here ({}); nothing was changed",
                    why.join("; ")
                ),
            ))
        }
        many => Err(Refusal::new(
            Code::ApprovalRequired,
            format!(
                "{pkg} is not installed and more than one provider can install it ({}). Fix: choose one with --provider",
                many.iter().map(|(a, _)| *a).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

/// One baseline observation. A file is hashed, a directory and an absent path are recorded as
/// such, and anything else (a link, a special file, an unreadable entry) is a baseline failure:
/// recording it as absent would let a later purge delete what was there.
pub(super) fn observe(path: &str) -> Result<BaselinePath, Refusal> {
    let fail = |what: String| {
        Refusal::new(
            Code::StateInvalid,
            format!("baseline failure: {what}; nothing was written"),
        )
    };
    let seen = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BaselinePath {
                path: path.into(),
                existed: false,
                sha256: None,
            });
        }
        Err(e) => return Err(fail(format!("{path}: {}", e.kind()))),
    };
    let sha256 = if seen.is_dir() {
        None
    } else if seen.is_file() {
        // The same cap as the manager's own documents.
        let limit = u64::try_from(MAX_BYTES).unwrap_or(u64::MAX);
        let bytes = read_regular_bounded(
            Path::new(path),
            limit,
            |e| fail(format!("{path}: {}", e.kind())),
            || fail(format!("{path} changed while it was read")),
        )?;
        if bytes.len() > MAX_BYTES {
            return Err(fail(format!("{path} is larger than the read limit")));
        }
        Some(file_fingerprint(&bytes))
    } else {
        return Err(fail(format!(
            "{path} is a link or a special file; a baseline records only files, directories and absent paths"
        )));
    };
    Ok(BaselinePath {
        path: path.into(),
        existed: true,
        sha256,
    })
}

/// The data candidates: where a tool keeps its data, state and caches by convention (XDG and
/// macOS). `purge` removes the ones that were absent before the trial.
/// shortcut: a fixed list by tool name, upgrade when an adapter can declare its data paths
pub(super) fn data_candidates(home: &Path, tool: &str) -> Vec<String> {
    [
        ".local/share",
        ".local/state",
        ".cache",
        "Library/Application Support",
        "Library/Caches",
    ]
    .iter()
    .map(|d| home.join(d).join(tool).display().to_string())
    .collect()
}

/// The configuration candidates: the explicit `--config` paths, or `~/.config/<tool>` when
/// none is given. A relative path is inside HOME.
pub(super) fn candidates(
    home: &Path,
    tool: &str,
    configs: &[String],
) -> Result<Vec<String>, Refusal> {
    if configs.is_empty() {
        // shortcut: one XDG candidate for every tool, upgrade when an adapter can declare paths
        return Ok(vec![home.join(".config").join(tool).display().to_string()]);
    }
    let mut out = Vec::new();
    for c in configs {
        let p = if c.starts_with('/') {
            c.clone()
        } else {
            home.join(c).display().to_string()
        };
        if !valid_path(&p) {
            return Err(usage(format!(
                "--config {c} is not an absolute path without '.' or '..' components"
            )));
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    if let Some(s) = &args.script {
        return script::run(ctx, args, s, out);
    }
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })?;
    // The adapter's own name check, before any provider sees the name.
    let package = package::name(&package_value(&args.package)).map_err(|e| usage(e.detail))?;
    let tool = package::leaf(&package).to_string();
    if !valid_id(&tool) {
        return Err(usage(format!(
            "the tool name {} must be lowercase ASCII letters, digits, '.', '_' or '-' (1 to 64 characters)",
            visible(&tool)
        )));
    }
    let trial_id = args.id.clone().unwrap_or_else(|| format!("trial-{tool}"));
    if !valid_id(&trial_id) {
        return Err(usage(
            "--id must be lowercase ASCII letters, digits, '.', '_' or '-' (1 to 64 characters)",
        ));
    }
    // The binary identity is part of the digest and the receipt; read it before anything runs.
    let binary = crate::plan_cmd::binary_identity()?;
    let stamp = Stamp::of(&binding);
    // The providers run before anything is decided, so the handlers go in first: a stop request
    // ends the probe instead of leaving it running (docs/packages.md).
    signal::install()?;
    let stop = || signal::requested().map_or(Ok(()), Err);
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let runner = ctx.runner(&sys);
    let fail = state_cmd::fail_from_env()?;
    let readable =
        Store::open_read(sys, &layout, fail.clone())?.ok_or_else(crate::state_lost_refusal)?;
    readable.require_identity(&binding)?;
    let paths = candidates(&layout.home, &tool, &args.configs)?;
    // A data candidate that is also a configuration candidate is observed once, as configuration.
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
    let decide = || -> Result<Decision, Refusal> {
        let owner = find_owner(&runner, &package, args.provider.as_deref())?;
        let baseline = paths.iter().map(|p| observe(p)).collect::<Result<_, _>>()?;
        let (mut seen, mut skipped) = (Vec::new(), skipped.clone());
        for p in &data {
            match observe(p) {
                Ok(b) => seen.push(b),
                Err(e) => skipped.push(format!("{p} not observed ({})", e.detail)),
            }
        }
        Ok(Decision {
            trial_id: trial_id.clone(),
            tool: tool.clone(),
            adapter: owner.adapter,
            package: package.clone(),
            install: owner.install,
            owners: owner.owners,
            baseline,
            data: seen,
            skipped,
            binary: binary.clone(),
            stamp: stamp.clone(),
        })
    };
    let first = decide()?;
    stop()?;
    let digest = first.digest()?;
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    first.print(out, &digest).map_err(w)?;
    if let Some(approved) = &args.yes_plan
        && *approved != digest
    {
        return Err(stale(approved, &digest));
    }
    if args.preview {
        return Ok(());
    }
    let Some(approved) = &args.yes_plan else {
        return Err(Refusal::new(
            Code::ApprovalRequired,
            format!(
                "nothing was changed. Fix: read the trial above, then run again with --yes-plan {digest}"
            ),
        ));
    };
    let file = format!("{trial_id}.json");
    let conflict = || {
        Refusal::new(
            Code::Conflict,
            format!(
                "trial {trial_id} already exists in state/trials/; nothing was changed. Fix: pass another --id, or purge it with dfm purge"
            ),
        )
    };
    if readable.load().trials.iter().any(|(n, _)| *n == file) {
        return Err(conflict());
    }
    drop(readable);
    fail.hit("trial-before-lock")?;

    // Take the lock, then decide again: the providers or the baseline may have changed.
    let store = Store::open_write_with(sys, &layout, fail.clone(), false)?;
    if !store.is_initialized() {
        return Err(store.stamp(crate::state_lost_refusal()));
    }
    store
        .require_identity(&binding)
        .map_err(|r| store.stamp(r))?;
    // A damaged or newer ledger refuses the trial before any write.
    store.read_ledger().map_err(|r| store.stamp(r))?;
    let locked = decide().map_err(|r| store.stamp(r))?;
    stop().map_err(|r| store.stamp(r))?;
    let locked_digest = locked.digest().map_err(|r| store.stamp(r))?;
    if locked_digest != digest {
        return Err(store.stamp(stale(approved, &locked_digest)));
    }
    let started = now_ns().map_err(|r| store.stamp(r))?;
    let op_id = format!("trial-{started}-{}", std::process::id());
    // The baseline is durable before the provider changes anything (C02). The create is
    // exclusive, so a record another run wrote since the check above is a conflict, not
    // replaced.
    if !store.create_trial(&locked.record(started, &op_id))? {
        return Err(store.stamp(conflict()));
    }

    // From here on the record exists and every outcome, journaled or not, ends in a receipt and
    // a result line; a refusal is after a mutation, so it exits 1.
    let mut status = Status::Current;
    let mut stopped: Option<Refusal> = None;
    if locked.install.is_some() {
        let value = package_value(&package);
        let journal = |phase, recovery, reason| -> Result<(), Refusal> {
            store.append_journal(&JournalEntry {
                schema: SCHEMA,
                op_id: op_id.clone(),
                op: "install".into(),
                phase,
                at: rfc3339(now_ns()?),
                resource: trial_id.clone(),
                adapter: locked.adapter_name().into(),
                class: Class::Normal,
                recovery,
                token: None,
                reason,
            })
        };
        // The intent has no path: the provider's listing is the record, as for `packages`.
        let intent = journal(
            Phase::Intent,
            Some(Recovery {
                outputs: vec![],
                snapshot: None,
            }),
            None,
        );
        // The install and its postcondition, as `try_install` in the engine runs them for a
        // package (crates/engine/src/apply.rs).
        let result = intent.and_then(|()| {
            fail.hit("trial-after-intent")
                .and_then(|()| package::install(&runner, locked.adapter_name(), &value))
                .and_then(|()| {
                    match (
                        package::probe(&runner, locked.adapter_name(), &value)?,
                        signal::requested(),
                    ) {
                        (PackageState::Installed(_), _) => Ok(()),
                        (_, Some(stop)) => Err(Refusal::new(
                            stop.code,
                            "stopped before the postcondition was checked; the install may have completed, and the next trial for this tool is refused until this one is purged",
                        )),
                        (s, None) => Err(Refusal::new(
                            Code::IoFailed,
                            format!(
                                "postcondition failed: the provider reports {} after the install",
                                s.state()
                            ),
                        )),
                    }
                })
                // An effect whose outcome entry cannot be written is a failed operation, as
                // the engine records it; the text names both causes.
                .and_then(|()| {
                    journal(Phase::Done, None, None).map_err(|n| {
                        Refusal::new(
                            n.code,
                            format!(
                                "installed, but the outcome could not be journaled: {}",
                                n.detail
                            ),
                        )
                    })
                })
        });
        match result {
            Ok(()) => status = Status::Updated,
            Err(e) => {
                let why = bounded_visible(&e.detail, 1024);
                let e = match journal(Phase::Failed, None, Some(why)) {
                    Ok(()) => e,
                    Err(n) => Refusal::new(
                        e.code,
                        format!(
                            "{}; the outcome could not be journaled: {}",
                            e.detail, n.detail
                        ),
                    ),
                };
                status = Status::Failed;
                stopped = Some(e.after_mutation());
            }
        }
    }
    let finished = now_ns().unwrap_or(started);
    let exit = stopped.as_ref().map_or(Exit::Ok, Refusal::exit);
    let error = stopped.as_ref().map(|e| bounded_visible(&e.detail, 1024));
    // The one operation carries the journal id, as an apply operation that reached its intent
    // does, so the record, the journal and the receipt are joined by that id.
    let ops = vec![OperationRecord {
        id: op_id.clone(),
        resource: trial_id.clone(),
        adapter: locked.adapter_name().into(),
        class: Class::Normal,
        status,
        started_at: Some(rfc3339(started)),
        finished_at: Some(rfc3339(finished)),
        before: None,
        after: None,
        owned_outputs: vec![],
        error: error.clone(),
        reload: None,
    }];
    let receipt = Receipt {
        schema: SCHEMA,
        op_id: op_id.clone(),
        command: "trial".into(),
        update: false,
        started_at: rfc3339(started),
        finished_at: rfc3339(finished),
        binary: BinaryId {
            version: binary.version,
            commit: option_env!("DFM_BUILD_COMMIT").unwrap_or("unknown").into(),
            sha256: binary.sha256,
        },
        profile: ProfileRef {
            id: stamp.profile,
            binding_digest: stamp.binding_digest,
        },
        machine_id: stamp.machine_id,
        plan_digest: Some(digest.clone()),
        approval: Some(Approval {
            kind: "yes_plan".into(),
            plan_digest: digest,
            at: rfc3339(started),
        }),
        sources: vec![],
        approvals: vec![],
        result: RunResult {
            outcome: exit.name().into(),
            exit_code: exit.code(),
            counts: count_statuses(&ops),
        },
        operations: ops,
    };
    let receipt_name = store.write_receipt(&receipt);
    let error = error.map_or(String::new(), |e| format!(": {e}"));
    writeln!(out, "  {} {trial_id} {op_id}{error}", label(status)).map_err(w)?;
    writeln!(out, "trial: state/trials/{file}").map_err(w)?;
    let outcome = match (receipt_name, stopped) {
        (Ok(name), stopped) => {
            writeln!(out, "receipt: {}", visible(&name)).map_err(w)?;
            stopped.map_or(Ok(()), Err)
        }
        // The journal and the trial record stay the record; the higher-ranked exit wins and
        // the receipt failure wins a tie.
        (Err(e), stopped) => {
            let e = e.after_mutation();
            writeln!(out, "receipt: not written ({})", visible(&e.to_string())).map_err(w)?;
            match stopped {
                Some(r) if e.exit().combine(r.exit()) != e.exit() => Err(r),
                _ => Err(e),
            }
        }
    };
    let final_exit = outcome.as_ref().err().map_or(Exit::Ok, Refusal::exit);
    writeln!(
        out,
        "{} {}",
        paint::label(ctx.color, "result:"),
        paint::status(ctx.color, final_exit.name())
    )
    .map_err(w)?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_take_one_package_and_repeated_config_paths() {
        let a = |v: &[&str]| crate::cli::parse("trial", v).and_then(|m| parse_args(&m));
        let ok = a(&["atuin", "--config", "a", "--config", "/b", "--preview"]).unwrap();
        assert_eq!(
            (ok.package.as_str(), ok.configs.len(), ok.preview),
            ("atuin", 2, true)
        );
        assert_eq!(a(&[]).unwrap_err().code, Code::Usage);
        assert_eq!(a(&["a", "b"]).unwrap_err().code, Code::Usage);
        assert_eq!(a(&["npm:prettier"]).unwrap_err().code, Code::Usage);
        assert_eq!(a(&["npm:@scope/pkg"]).unwrap_err().code, Code::Usage);
        assert_eq!(
            a(&["a", "--provider", "apt/v1"]).unwrap_err().code,
            Code::Usage
        );
        assert_eq!(
            a(&["a", "--preview", "--yes-plan", &"0".repeat(64)])
                .unwrap_err()
                .code,
            Code::Usage
        );
        assert_eq!(a(&["a", "--yes-plan", "zz"]).unwrap_err().code, Code::Usage);
        // `-y` is the short form: the same value check, and one approval per run.
        let d = "0".repeat(64);
        assert_eq!(
            a(&["a", "-y", &d]).unwrap().yes_plan.as_deref(),
            Some(d.as_str())
        );
        for bad in [
            vec!["a", "-y", "zz"],
            vec!["a", "-y"],
            vec!["a", "--preview", "-y", &d],
            vec!["a", "-y", &d, "--yes-plan", &d],
        ] {
            assert_eq!(a(&bad).unwrap_err().code, Code::Usage, "{bad:?}");
        }
    }

    #[test]
    fn script_arguments_parse_and_refuse_the_wrong_combinations() {
        let a = |v: &[&str]| crate::cli::parse("trial", v).and_then(|m| parse_args(&m));
        let ok = a(&[
            "boat",
            "--script",
            "https://boat.dev/install",
            "--shell",
            "bash",
            "--arg",
            "--no-modify-path",
            "--env",
            "FOO=1",
            "--tty",
            "--preview",
        ])
        .unwrap();
        let s = ok.script.unwrap();
        assert_eq!(
            (s.url.as_str(), s.shell.as_str(), s.args.as_slice(), s.tty),
            (
                "https://boat.dev/install",
                "bash",
                &["--no-modify-path".to_string()][..],
                true
            )
        );
        assert_eq!(s.env, vec![("FOO".to_string(), "1".to_string())]);
        assert_eq!(
            a(&["boat", "--script", "https://x/i"])
                .unwrap()
                .script
                .unwrap()
                .shell,
            "sh"
        );
        for bad in [
            vec![
                "boat",
                "--script",
                "https://x/i",
                "--provider",
                "homebrew/v1",
            ],
            vec!["boat", "--script", "http://x/i"],
            vec!["boat", "--script", "https://x/i", "--shell", "fish"],
            vec!["boat", "--script", "https://x/i", "--env", "HOME=/x"],
            vec!["boat", "--script", "https://x/i", "--env", "NOEQUALS"],
            vec!["boat", "--script", "https://x/i", "--env", "=v"],
            vec!["boat", "--script", "https://x/i", "--env", "A=a\nb"],
            vec!["boat", "--script", "https://x/i", "--env", "A=a\0b"],
            vec![
                "boat",
                "--script",
                "https://x/i",
                "--env",
                "A=1",
                "--env",
                "A=2",
            ],
            vec!["homebrew/core/boat", "--script", "https://x/i"],
            vec!["boat", "--shell", "bash"],
            vec!["boat", "--arg", "x"],
            vec!["boat", "--env", "A=1"],
            vec!["boat", "--tty"],
        ] {
            assert_eq!(a(&bad).unwrap_err().code, Code::Usage, "{bad:?}");
        }
    }

    #[test]
    fn candidates_default_to_one_xdg_path_and_resolve_relative_paths_inside_home() {
        let home = Path::new("/h");
        assert_eq!(
            candidates(home, "atuin", &[]).unwrap(),
            ["/h/.config/atuin"]
        );
        let given = ["x/y".to_string(), "/h/z".to_string(), "x/y".to_string()];
        assert_eq!(
            candidates(home, "atuin", &given).unwrap(),
            ["/h/x/y", "/h/z"]
        );
        assert_eq!(
            candidates(home, "atuin", &["../e".to_string()])
                .unwrap_err()
                .code,
            Code::Usage
        );
    }

    #[test]
    fn the_digest_covers_the_baseline_the_install_the_binary_and_the_binding() {
        let d = |install: Option<Vec<String>>, existed: bool, sha: &str, machine: &str| {
            Decision {
                trial_id: "trial-a".into(),
                tool: "a".into(),
                adapter: Some(HOMEBREW.into()),
                package: "a".into(),
                install,
                owners: vec![],
                baseline: vec![BaselinePath {
                    path: "/h/.config/a".into(),
                    existed,
                    sha256: None,
                }],
                data: vec![],
                skipped: vec![],
                binary: Binary {
                    version: "0".into(),
                    sha256: sha.into(),
                },
                stamp: Stamp {
                    profile: "p".into(),
                    machine_id: machine.into(),
                    binding_digest: "d".into(),
                },
            }
            .digest()
            .unwrap()
        };
        let base = d(Some(vec![]), false, "s1", "m1");
        assert_eq!(base, d(Some(vec![]), false, "s1", "m1"));
        assert_ne!(base, d(Some(vec!["dep".into()]), false, "s1", "m1"));
        assert_ne!(base, d(Some(vec![]), true, "s1", "m1"));
        assert_ne!(base, d(None, false, "s1", "m1"));
        assert_ne!(base, d(Some(vec![]), false, "s2", "m1"));
        assert_ne!(base, d(Some(vec![]), false, "s1", "m2"));
    }
}
