//! Recording commands that drive the engine through the real binary. They exist only in the
//! `test-isolation` build. Each performs one tiny, fake operation (create a file in HOME) so the
//! lifecycle can be tested: intent first, the effect, an independent check, completion, then the
//! ledger and the receipt.

use super::super::{Ctx, bounded, flags, header, read_existing_binding, usage};
use dfm_engine::{Fail, Probe, Store, now_ns};
use dfm_model::binding::Binding;
use dfm_model::plan::file_fingerprint;
use dfm_model::state::{
    Baseline, BaselinePath, BinaryId, Class, JournalEntry, LedgerResource, OperationRecord,
    OwnedOutput, Phase, ProfileRef, Receipt, Recovery, RecoveryOutput, RunResult, SCHEMA, Stamp,
    Status, TrialRecord, redact, rfc3339,
};
use dfm_model::{Code, Refusal};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

fn open_bound(ctx: &Ctx, fail: Fail, secrets: Vec<String>) -> Result<(Store, Binding), Refusal> {
    let binding = read_existing_binding(ctx)?
        .ok_or_else(|| Refusal::new(Code::BindingInvalid, "this machine is not bound"))?;
    let store =
        Store::open_write_with(ctx.sys()?, &ctx.layout(), fail, false)?.with_secrets(secrets);
    if !store.is_initialized() {
        return Err(Refusal::new(
            Code::StateInvalid,
            "state is not initialized; run bind",
        ));
    }
    store.require_identity(&binding)?;
    // A missing, damaged or newer ledger refuses the operation before any write.
    store.read_ledger()?;
    Ok((store, binding))
}

fn io(what: &str, e: &std::io::Error) -> Refusal {
    Refusal::new(Code::IoFailed, format!("{what}: {}", e.kind()))
}

fn op_id(given: Option<String>) -> Result<String, Refusal> {
    Ok(match given {
        Some(g) => g,
        None => format!("run-{}-{}", now_ns()?, std::process::id()),
    })
}

/// `__record-op`: create `--output` with `--content` as one journaled operation.
pub fn record_op(ctx: &Ctx, args: &[String], out: &mut dyn Write) -> Result<(), Refusal> {
    let [
        resource,
        adapter,
        output,
        content,
        id,
        sensitive,
        note,
        secret,
        at,
    ] = flags(
        args,
        [
            "--resource",
            "--adapter",
            "--output",
            "--content",
            "--op-id",
            "--sensitive",
            "--note",
            "--secret",
            "--at",
        ],
    )?;
    let need = |v: Option<String>, n: &str| v.ok_or_else(|| usage(format!("{n} is required")));
    let (resource, output) = (need(resource, "--resource")?, need(output, "--output")?);
    let adapter = adapter.unwrap_or_else(|| "fake/v1".into());
    let content = content.unwrap_or_default();
    let class = if sensitive.as_deref() == Some("yes") {
        Class::Sensitive
    } else {
        Class::Normal
    };
    let id = op_id(id)?;
    let fail = super::fail_from_env()?;
    let (store, binding) = open_bound(ctx, fail.clone(), secret.iter().cloned().collect())?;
    header(out, ctx).map_err(|e| io("output", &e))?;
    // `--at` fixes the start time so a test can make two runs collide on one receipt name.
    let started = match at {
        Some(t) => t
            .parse()
            .map_err(|_| usage("--at is a unix time in nanoseconds"))?,
        None => now_ns()?,
    };
    if Path::new(&output).symlink_metadata().is_ok() {
        return Err(Refusal::new(
            Code::StateInvalid,
            "the output already exists; it is never claimed",
        ));
    }
    let digest = file_fingerprint(content.as_bytes());
    let now = || now_ns().map(rfc3339);
    let entry = |phase, recovery, reason| -> Result<JournalEntry, Refusal> {
        Ok(JournalEntry {
            schema: SCHEMA,
            op_id: id.clone(),
            op: "op1".into(),
            phase,
            at: now()?,
            resource: resource.clone(),
            adapter: adapter.clone(),
            class,
            recovery,
            token: None,
            reason,
        })
    };
    // 1. Intent and recovery metadata, durable before the side effect.
    store.append_journal(&entry(
        Phase::Intent,
        Some(Recovery {
            outputs: vec![RecoveryOutput {
                path: output.clone(),
                existed_before: false,
                expected_sha256: (class == Class::Normal).then(|| digest.clone()),
            }],
            snapshot: None,
        }),
        None,
    )?)?;
    // The intent is durable: any refusal from here on happened after a mutation began.
    let rest = || -> Result<(), Refusal> {
        fail.hit("after-intent")?;
        // 2. The side effect.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&output)
            .and_then(|mut f| f.write_all(content.as_bytes()).and_then(|()| f.sync_all()))
            .map_err(|e| io("write output", &e))?;
        fail.hit("after-effect")?;
        // 3. Independent verification: read the result back, do not trust the write call.
        let seen = fs::read(&output).map_err(|e| io("verify output", &e))?;
        let note = note.map(|n| redact(&n, &secret.iter().map(String::as_str).collect::<Vec<_>>()));
        if seen != content.as_bytes() {
            store.append_journal(&entry(
                Phase::Failed,
                None,
                Some("postcondition failed".into()),
            )?)?;
            return Err(Refusal::new(Code::IoFailed, "postcondition failed"));
        }
        store.append_journal(&entry(Phase::Done, None, note.clone())?)?;
        fail.hit("after-done")?;
        // 4. Ownership, only after the terminal journal entry.
        let mut ledger = store.read_ledger()?;
        ledger.record(LedgerResource {
            resource: resource.clone(),
            adapter: adapter.clone(),
            class,
            op_id: id.clone(),
            owned_outputs: vec![OwnedOutput {
                path: output.clone(),
                mode: 0o600,
                fingerprint: (class == Class::Normal).then(|| digest.clone()),
                coowned: None,
            }],
        })?;
        store.write_ledger(&ledger)?;
        // 5. The receipt.
        let stamp = Stamp::of(&binding);
        let fp = |d: &str| dfm_model::state::Fingerprint {
            fingerprint: d.into(),
        };
        let ops = vec![OperationRecord {
            id: "op1".into(),
            resource: resource.clone(),
            adapter: adapter.clone(),
            class,
            status: Status::Updated,
            started_at: Some(rfc3339(started)),
            finished_at: Some(now()?),
            before: None,
            after: (class == Class::Normal).then(|| fp(&digest)),
            owned_outputs: vec![output],
            error: note,
            reload: None,
        }];
        let receipt = Receipt {
            schema: SCHEMA,
            op_id: id.clone(),
            command: "record-op".into(),
            update: false,
            started_at: rfc3339(started),
            finished_at: now()?,
            binary: BinaryId {
                version: env!("CARGO_PKG_VERSION").into(),
                commit: "unknown".into(),
                sha256: "0".repeat(64),
            },
            profile: ProfileRef {
                id: stamp.profile,
                binding_digest: stamp.binding_digest,
            },
            machine_id: stamp.machine_id,
            plan_digest: None,
            approval: None,
            sources: vec![],
            approvals: vec![],
            result: RunResult::from_operations(&ops),
            operations: ops,
        };
        store.write_receipt(&receipt)?;
        Ok(())
    };
    rest().map_err(Refusal::after_mutation)?;
    writeln!(out, "recorded {id}").map_err(|e| io("output", &e))
}

/// The most a harness read takes from a file, as for the manager's own documents.
const MAX_READ: u64 = 16 << 20;

/// The bytes of a regular file, read without following a link. `None` only when the path does
/// not exist: any other failure (a link, a directory, a FIFO, a second hard link, no permission,
/// more than [`MAX_READ`] bytes) is an error, because recording it as "did not exist" would make
/// a later restore delete a file that was there.
fn read_regular(path: &str) -> Result<Option<Vec<u8>>, Refusal> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let seen = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io("stat path", &e)),
    };
    let refuse = |what: &str| Refusal::new(Code::StateInvalid, format!("the path {what}"));
    if !seen.is_file() {
        return Err(refuse("is a link or not a regular file"));
    }
    // No-follow and non-blocking, so a swap for a link or a FIFO after the check above fails
    // or is caught below instead of hanging the harness.
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(dfm_platform_macos::O_NOFOLLOW | dfm_platform_macos::O_NONBLOCK)
        .open(path)
        .map_err(|e| io("open path", &e))?;
    // The open file must be the file that was checked.
    let opened = f.metadata().map_err(|e| io("stat path", &e))?;
    if !opened.is_file() || (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
        return Err(refuse("changed while it was read"));
    }
    if opened.nlink() != 1 {
        return Err(refuse("has more than one hard link"));
    }
    let mut bytes = Vec::new();
    (&mut f)
        .take(MAX_READ + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io("read path", &e))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_READ {
        return Err(refuse("is larger than the read limit"));
    }
    Ok(Some(bytes))
}

/// `__record-trial`: write or replace an active trial record with a baseline of one path.
pub fn record_trial(ctx: &Ctx, args: &[String], out: &mut dyn Write) -> Result<(), Refusal> {
    let [id, tool, path] = flags(args, ["--id", "--tool", "--baseline-path"])?;
    let (Some(id), Some(tool), Some(path)) = (id, tool, path) else {
        return Err(usage("--id, --tool and --baseline-path are required"));
    };
    let (store, binding) = open_bound(ctx, super::fail_from_env()?, Vec::new())?;
    let existing = read_regular(&path)?;
    let trial = TrialRecord {
        schema: SCHEMA,
        binding: Stamp::of(&binding),
        trial_id: id.clone(),
        tool,
        created_unix_ns: now_ns()?,
        baseline: Baseline {
            tool_preexisting: false,
            paths: vec![BaselinePath {
                path,
                existed: existing.is_some(),
                sha256: existing.map(|b| file_fingerprint(&b)),
            }],
            owners: vec![],
        },
        owned_outputs: vec![],
        package: None,
        unknown: vec![],
    };
    store.write_trial(&trial)?;
    writeln!(out, "recorded trial {id}").map_err(|e| io("output", &e))
}

/// `__reconcile`: settle interrupted operations by checking their recorded outputs.
pub fn reconcile(ctx: &Ctx, out: &mut dyn Write) -> Result<(), Refusal> {
    let (store, _) = open_bound(ctx, super::fail_from_env()?, Vec::new())?;
    // The probe has no way to carry a cause, so the unreadable outputs are collected here.
    let unreadable = std::cell::RefCell::new(Vec::new());
    let probe = |e: &JournalEntry| {
        let outs = e
            .recovery
            .iter()
            .flat_map(|r| &r.outputs)
            .collect::<Vec<_>>();
        // An output that cannot be read as a plain file (a link, a directory, no permission) is
        // not absent: the probe cannot decide.
        let seen = match outs
            .iter()
            .map(|o| read_regular(&o.path))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(seen) => seen,
            Err(r) => {
                unreadable.borrow_mut().push((e.op_id.clone(), r.detail));
                return Probe::Unknown;
            }
        };
        if seen.iter().all(Option::is_none) {
            return Probe::NotDone;
        }
        let proven = outs.iter().zip(&seen).all(|(o, s)| {
            matches!((&o.expected_sha256, s),
                (Some(want), Some(b)) if &file_fingerprint(b) == want)
        });
        if proven { Probe::Done } else { Probe::Unknown }
    };
    for (op, step, phase) in store.reconcile(&probe)? {
        writeln!(
            out,
            "settled {} {} as {:?}",
            bounded(&op),
            bounded(&step),
            phase
        )
        .map_err(|e| io("output", &e))?;
    }
    for (op, why) in unreadable.into_inner() {
        writeln!(
            out,
            "cannot read an output of {}: {}",
            bounded(&op),
            bounded(&why)
        )
        .map_err(|e| io("output", &e))?;
    }
    Ok(())
}
