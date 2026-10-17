//! The `op-template/v1` secret adapter (docs/secrets.md): a 1Password CLI template rendered
//! with `op inject` on explicit request (`secrets`), never during `plan` or `apply`.
//!
//! Plan time reads the destination (read-only, in memory) and reports a verdict and nothing
//! else. Apply time checks the session with `op whoami` before any mutation, then renders the
//! template through standard input and gets the bytes on standard output. The bytes reach the
//! destination file and nowhere else. The provider's standard error is never printed or
//! persisted: it could repeat a reference or a value.

use dfm_engine::{Dir, End, Live, ProcRequest, Runner, Store, Sys};
use dfm_model::merge::salted;
use dfm_model::plan::{MAX_HASHED_BYTES, Owned, Plan, Scope, SecretVerdict, Status};
use dfm_model::resolve::{Resolved, Value};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

pub const ADAPTER: &str = "op-template/v1";
/// `op whoami` answers from the local session; this is generous.
const WHOAMI_TIMEOUT: Duration = Duration::from_secs(30);
/// `op inject` resolves each reference against the vault.
const INJECT_TIMEOUT: Duration = Duration::from_secs(120);
/// A rendered secret file is bounded like any published file.
const MAX_RENDERED: usize = 16 << 20;
/// The caller's variables that reach `op`: a session or service-account token that is already
/// available, and the Connect settings. Nothing else of the environment does.
const OP_ENV_PREFIXES: [&str; 3] = ["OP_SESSION_", "OP_SERVICE_ACCOUNT_TOKEN", "OP_CONNECT_"];

/// The account a resource names, checked as a parameter (docs/architecture.md section 8 item 3):
/// letters, digits, `.`, `_`, `-` and `@`, not starting with `-`, at most 128 characters.
fn account(value: &Value) -> Result<String, Refusal> {
    let a = value
        .get("account")
        .and_then(Value::as_str)
        .ok_or_else(|| Refusal::new(Code::ManifestInvalid, "op-template/v1 needs account"))?;
    let ok = !a.is_empty()
        && a.len() <= 128
        && !a.starts_with('-')
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
    if !ok {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            "op-template/v1 account must be letters, digits, '.', '_', '-' or '@', not start with '-', and be at most 128 characters",
        ));
    }
    Ok(a.to_string())
}

fn request<'a>(
    args: &'a [String],
    env: &'a [(String, String)],
    stdin: Option<&'a [u8]>,
    timeout: Duration,
) -> ProcRequest<'a> {
    ProcRequest {
        program: "op",
        args,
        env,
        stdin,
        timeout,
        max_stdout: MAX_RENDERED,
    }
}

/// Whether `op` has a session for `account`: `op whoami --account <account>` exits 0. Nothing
/// else is read from it. A failure is `E_CREDENTIAL_UNAVAILABLE` (exit 3, before mutation).
pub fn credential(runner: &Runner, account: &str) -> Result<(), Refusal> {
    let args = ["whoami".to_string(), "--account".into(), account.into()];
    let env = dfm_engine::proc::passthrough(&OP_ENV_PREFIXES);
    let out = runner
        .run(&request(&args, &env, None, WHOAMI_TIMEOUT))
        .map_err(|e| Refusal::new(Code::CredentialUnavailable, e.detail))?;
    if out.success() {
        return Ok(());
    }
    let why = match out.end {
        End::Exited(c) => format!("op whoami exited {c}"),
        End::TimedOut => "op whoami did not answer in time".to_string(),
        End::Signaled => "op whoami was ended by a signal".to_string(),
        End::Stopped(r) => return Err(r),
    };
    Err(Refusal::new(
        Code::CredentialUnavailable,
        format!(
            "no 1Password session for account {account} ({why}); nothing was changed. Fix: run `op signin --account {account}` (or unlock the 1Password app), then run secrets again"
        ),
    ))
}

/// The credential gate for a secrets plan: every planned secret's account must have a session.
/// Accounts are checked once each, in order. Nothing is rendered here.
pub fn credentials(plan: &Plan, resolved: &Resolved, runner: &Runner) -> Result<(), Refusal> {
    let mut seen = BTreeSet::new();
    for op in plan
        .operations
        .iter()
        .filter(|o| o.status == Status::Planned && o.adapter == ADAPTER)
    {
        let Some(r) = resolved.resources.get(&op.resource) else {
            continue;
        };
        let a = account(&r.value)?;
        if seen.insert(a.clone()) {
            credential(runner, &a)?;
        }
    }
    Ok(())
}

/// Renders `template` for `account`: `op inject --account <account>` with the template on
/// standard input. The output bytes are returned to the caller and nowhere else. A provider
/// that ran and failed is `E_RENDER_FAILED`; its standard error is not repeated.
pub fn render(runner: &Runner, value: &Value, template: &[u8]) -> Result<Vec<u8>, Refusal> {
    let account = account(value)?;
    let args = ["inject".to_string(), "--account".into(), account.clone()];
    let env = dfm_engine::proc::passthrough(&OP_ENV_PREFIXES);
    let out = runner.run(&request(&args, &env, Some(template), INJECT_TIMEOUT))?;
    match out.end {
        End::Exited(0) => Ok(out.stdout),
        End::Exited(c) => Err(Refusal::new(
            Code::RenderFailed,
            format!(
                "op inject exited {c} for account {account}; its {} bytes of standard error are withheld because they can repeat a reference or a value. Run `op inject --account {account}` on the template yourself to see them",
                out.stderr.len()
            ),
        )),
        End::TimedOut => Err(Refusal::new(
            Code::RenderFailed,
            format!("op inject did not finish within {INJECT_TIMEOUT:?}"),
        )),
        End::Signaled => Err(Refusal::new(
            Code::RenderFailed,
            "op inject was ended by a signal",
        )),
        End::Stopped(r) => Err(r),
    }
}

fn blocked(code: Code, detail: &str, correction: &str) -> SecretVerdict {
    SecretVerdict::Blocked {
        code,
        detail: detail.into(),
        correction: correction.into(),
    }
}

/// The verdict on each selected secret resource's destination, by resource id: the destination
/// is read (in memory, never hashed unsalted) and compared with the salted hash the ledger
/// records for this resource. Absent, or equal: `Owned`. Present with no entry, another
/// resource's entry, or a different hash: `Unowned` (a takeover that needs `--allow-adopt`).
pub fn secret_facts(
    resolved: &Resolved,
    store: &Store,
    sys: &Sys,
    home: &Dir,
    home_path: &Path,
    ledger: &[Owned],
) -> BTreeMap<String, SecretVerdict> {
    let mut out = BTreeMap::new();
    for r in resolved
        .resources
        .values()
        .filter(|r| Scope::Secrets.covers(r) && r.adapter == ADAPTER)
    {
        let [claim] = r.claims.as_slice() else {
            continue;
        };
        if let Err(e) = account(&r.value) {
            out.insert(
                r.id.clone(),
                blocked(
                    e.code,
                    &e.detail,
                    &format!("correct the resource value in {}", r.declared_in),
                ),
            );
            continue;
        }
        out.insert(
            r.id.clone(),
            verdict(&r.id, claim, store, sys, home, home_path, ledger),
        );
    }
    out
}

fn verdict(
    resource: &str,
    claim: &str,
    store: &Store,
    sys: &Sys,
    home: &Dir,
    home_path: &Path,
    ledger: &[Owned],
) -> SecretVerdict {
    let comps: Vec<&str> = claim.split('/').collect();
    let Some((name, parents)) = comps.split_last() else {
        return blocked(Code::TargetInvalid, "the target is empty", "name a file");
    };
    let live = match home.open_below_opt(sys, parents) {
        Ok(None) => Live::Missing,
        Ok(Some(dir)) => match dir.read_live(sys, name, MAX_HASHED_BYTES) {
            Ok(l) => l,
            Err(e) => return blocked(e.code, &e.detail, "make the path readable, then plan again"),
        },
        Err(e) => {
            return blocked(
                e.code,
                &e.detail,
                "make the parent a real directory of yours",
            );
        }
    };
    let bytes = match live {
        Live::Missing => return SecretVerdict::Owned,
        Live::Unusable(why) => {
            return blocked(
                Code::Conflict,
                &format!("{claim} {why}"),
                "replace it with a plain regular file of yours, or remove it",
            );
        }
        Live::TooBig => {
            return blocked(
                Code::LimitExceeded,
                &format!("{claim} exceeds 16 MiB"),
                "move the file aside",
            );
        }
        Live::File { mode, .. } if mode & 0o077 != 0 => {
            return blocked(
                Code::Conflict,
                &format!("{claim} is mode {mode:o}, open to group or others, and holds a secret"),
                "run chmod 600 on the file, then plan again",
            );
        }
        Live::File { bytes, .. } => bytes,
    };
    // Another resource's entry is the planner's conflict; here it is simply not ours.
    let abs = home_path.join(claim);
    let recorded = ledger
        .iter()
        .find(|o| o.resource == resource && Path::new(&abs).ends_with(&o.path));
    let hash = match store.read_ledger_lenient() {
        Ok(l) => l
            .resources
            .iter()
            .filter(|x| x.resource == resource)
            .flat_map(|x| &x.owned_outputs)
            .find(|o| Path::new(&o.path) == abs)
            .and_then(|o| o.coowned.as_ref().map(|c| c.managed_hash.clone())),
        Err(e) => return blocked(e.code, &e.detail, "repair the state store first"),
    };
    let (Some(_), Some(hash)) = (recorded, hash) else {
        return SecretVerdict::Unowned;
    };
    match store.salt() {
        Ok(Some(salt)) if salted(&salt, &bytes) == hash => SecretVerdict::Owned,
        Ok(Some(_)) => SecretVerdict::Unowned,
        // A recorded hash with no salt cannot be checked: the store's own fix text applies.
        Ok(None) => blocked(
            Code::StateInvalid,
            "state/salt does not exist but the ledger records a salted hash for this secret",
            "restore state/salt from a copy, or delete the coowned object of this output in state/ledger.json and approve the takeover with --allow-adopt",
        ),
        Err(e) => blocked(e.code, &e.detail, "repair state/salt, then plan again"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_is_a_checked_parameter() {
        let v = |a: &str| {
            let mut t = dfm_model::resolve::Table::new();
            t.insert("account".into(), Value::String(a.into()));
            Value::Table(t)
        };
        assert_eq!(account(&v("my.1password.com")).unwrap(), "my.1password.com");
        assert!(account(&v("me@example.com")).is_ok());
        for bad in ["", "-x", "a b", "a;b", &"a".repeat(129)] {
            assert_eq!(
                account(&v(bad)).unwrap_err().code,
                Code::ManifestInvalid,
                "{bad:?}"
            );
        }
        assert!(account(&Value::Table(dfm_model::resolve::Table::new())).is_err());
    }
}
