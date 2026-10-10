//! Recording commands that drive the engine through the real binary. They exist only in the
//! `test-isolation` build. Each performs one tiny, fake operation (create a file in HOME) so the
//! lifecycle can be tested: intent first, the effect, an independent check, completion, then the
//! ledger and the receipt.

use super::super::{Ctx, bounded, flags, header, read_existing_binding, usage};
use dfm_engine::{Fail, Probe, Store, now_unix};
use dfm_model::binding::Binding;
use dfm_model::digest::sha256_hex;
use dfm_model::state::{
    Baseline, BaselinePath, BinaryId, Class, JournalEntry, LedgerResource, OperationRecord,
    OwnedOutput, Phase, ProfileRef, Receipt, Recovery, RecoveryOutput, RunResult, Stamp, Status,
    TrialRecord, count_statuses, redact, rfc3339,
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
        None => format!("run-{}-{}", now_unix()?, std::process::id()),
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
        Some(t) => t.parse().map_err(|_| usage("--at is a unix time"))?,
        None => now_unix()?,
    };
    if Path::new(&output).symlink_metadata().is_ok() {
        return Err(Refusal::new(
            Code::StateInvalid,
            "the output already exists; it is never claimed",
        ));
    }
    let digest = sha256_hex(content.as_bytes());
    let now = || now_unix().map(rfc3339);
    let entry = |phase, recovery, reason| -> Result<JournalEntry, Refusal> {
        Ok(JournalEntry {
            schema: 1,
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
            started_at: rfc3339(started),
            finished_at: now()?,
            before: None,
            after: (class == Class::Normal).then(|| fp(&digest)),
            owned_outputs: vec![output],
            error: note,
        }];
        let receipt = Receipt {
            schema: 1,
            op_id: id.clone(),
            command: "record-op".into(),
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
            result: RunResult {
                outcome: "ok".into(),
                exit_code: 0,
                counts: count_statuses(&ops),
            },
            operations: ops,
        };
        store.write_receipt(&receipt)?;
        Ok(())
    };
    rest().map_err(Refusal::after_mutation)?;
    writeln!(out, "recorded {id}").map_err(|e| io("output", &e))
}

/// `__record-trial`: write or replace an active trial record with a baseline of one path.
pub fn record_trial(ctx: &Ctx, args: &[String], out: &mut dyn Write) -> Result<(), Refusal> {
    let [id, tool, path] = flags(args, ["--id", "--tool", "--baseline-path"])?;
    let (Some(id), Some(tool), Some(path)) = (id, tool, path) else {
        return Err(usage("--id, --tool and --baseline-path are required"));
    };
    let (store, binding) = open_bound(ctx, super::fail_from_env()?, Vec::new())?;
    let existing = fs::read(&path).ok();
    let trial = TrialRecord {
        schema: 1,
        binding: Stamp::of(&binding),
        trial_id: id.clone(),
        tool,
        created_unix: now_unix()?,
        baseline: Baseline {
            tool_preexisting: false,
            paths: vec![BaselinePath {
                path,
                existed: existing.is_some(),
                sha256: existing.map(|b| sha256_hex(&b)),
            }],
        },
        owned_outputs: vec![],
    };
    store.write_trial(&trial)?;
    writeln!(out, "recorded trial {id}").map_err(|e| io("output", &e))
}

/// `__reconcile`: settle interrupted operations by checking their recorded outputs.
pub fn reconcile(ctx: &Ctx, out: &mut dyn Write) -> Result<(), Refusal> {
    let (store, _) = open_bound(ctx, super::fail_from_env()?, Vec::new())?;
    let probe = |e: &JournalEntry| {
        let outs = e
            .recovery
            .iter()
            .flat_map(|r| &r.outputs)
            .collect::<Vec<_>>();
        let seen: Vec<Option<Vec<u8>>> = outs.iter().map(|o| fs::read(&o.path).ok()).collect();
        if seen.iter().all(Option::is_none) {
            return Probe::NotDone;
        }
        let proven = outs.iter().zip(&seen).all(|(o, s)| {
            matches!((&o.expected_sha256, s),
                (Some(want), Some(b)) if &sha256_hex(b) == want)
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
    Ok(())
}
