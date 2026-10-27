# Sync upgrades Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A plain `dfm sync` runs the updaters the profile declares (from the fixed set `brew`, `claude`, `codex`, `opencode`, `skills`) after its four scopes, and reports one status per updater.

**Architecture:** The profile gets an optional `updaters` list that the resolver validates and carries on `Resolved`. A new adapter module runs one updater through the existing `Runner` and returns a row. `sync_cmd.rs` runs the declared updaters after the scopes (also after "nothing to change"), prints the rows and an `upgrade:` summary line, writes one `command = "upgrade"` receipt and adds warnings. Doctor reports the latest upgrade receipt in its own row.

**Tech Stack:** Rust workspace (`dfm-model`, `dfm-engine`, `dfm-cli`), integration tests with shell look-alikes under the `test-isolation` build.

**Spec:** `docs/superpowers/specs/2026-10-27-sync-upgrades-design.md`

## Global Constraints

- The fixed set is exactly `brew`, `claude`, `codex`, `opencode`, `skills`. No free command string from any document.
- Every `dfm sync` runs the declared updaters, also after "nothing to change". `--no-upgrade` skips them. `--preview` lists them and runs none.
- A refused scope (an error exit) stops the run before the upgrade step. A blocked or skipped scope does not.
- Statuses are the existing `Status` values only: `updated`, `current`, `held`, `pending`, `failed`, `blocked`. No new receipt field and no new status.
- `updated` needs an observed version change. Exit 0 with the same version is `current`.
- Sync exits 0 when an updater is `failed`, `blocked` or `pending`, and prints a warning for each at the end of stdout.
- Updaters run detached: stdin closed, no terminal (`Runner` as it is). `Runner::resolve`'s link check is not changed.
- Owner paths: `claude` in `~/.local/bin`; `codex` in `~/.codex/packages/standalone/current/bin`; `opencode` in `~/.opencode/bin`; `brew` and `mise` through the provider search path (`provider_path`, overridden by `DFM_PROVIDER_PATH` in the isolated build). `skills` runs `mise exec -- npx skills update`.
- Homebrew variables for the upgrade: only `HOMEBREW_NO_ANALYTICS=1` and `HOMEBREW_NO_ENV_HINTS=1`. Do not reuse `brew_env()`.
- Timeouts: 30 minutes for `brew upgrade`, 10 minutes for every other command.
- No journal intent for an updater. The receipt is the record.
- No em dash or en dash in code, docs or commit messages. Conventional Commit messages with no attribution. Stage explicit paths.
- Run the full gate (`just check`) on the branch head only, never on intermediate commits.

## Review Focus

- A declared updater whose owner path holds a link that `Runner::resolve` refuses (for example a codex install outside `current`): expect `blocked` with the path, not a crash or a fallback to `PATH`.
- `--version` output that contains extra lines or ANSI color: the before and after strings must be compared as the tool printed them, trimmed, so the same version is `current`.
- `brew outdated --json=v2` that returns invalid JSON or a non-zero exit after `brew update` succeeded: expect `failed` with the reason, and the other updaters still run.
- A profile that declares `updaters` on a machine whose sync has a scope blocked by a locked 1Password: the upgrade still runs and both warnings appear.
- SIGINT during `brew upgrade`: the rest are `pending`, the receipt is written, and the next sync runs every updater again (no journal entry blocks it).

---

### Task 1: The `updaters` profile field

**Files:**
- Modify: `crates/model/src/resolve/docs.rs` (`ProfileDoc`, `profile()` at about lines 395-460, `only_keys` hint at about line 228)
- Modify: `crates/model/src/resolve/mod.rs` (`Resolved` at about line 100)
- Modify: `crates/model/src/resolve/compose.rs` (the `Ok(Resolved { .. })` at about line 241)
- Create: `crates/model/src/updater.rs` (the fixed set), and export it from `crates/model/src/lib.rs`
- Test: the resolver unit tests next to `profile()` (follow the existing profile tests in `crates/model/src/resolve/`), and `crates/cli/tests/resolver/` if profile fixtures live there
- Docs: `docs/resolver.md` (profile keys)

**Interfaces:**
- Produces: `pub enum Updater { Brew, Claude, Codex, Opencode, Skills }` in `dfm_model::updater`, with `pub const ALL: [Updater; 5]`, `pub fn name(self) -> &'static str` (the lowercase names above) and `pub fn parse(s: &str) -> Option<Updater>`.
- Produces: `ProfileDoc.updaters: Vec<Updater>` and `Resolved.updaters: Vec<Updater>`, in declared order. Absent key means an empty vector.

- [ ] **Step 1: Write the failing tests**
  - `profile_updaters_keep_the_declared_order`: `updaters = ["codex", "brew"]` resolves to `vec![Updater::Codex, Updater::Brew]`.
  - `profile_without_updaters_has_none`: no key gives an empty vector.
  - `profile_unknown_updater_is_manifest_invalid`: `updaters = ["npm"]` gives `Code::ManifestInvalid`, field `/updaters/0`.
  - `profile_duplicate_updater_is_manifest_invalid`: `updaters = ["brew", "brew"]` gives `Code::ManifestInvalid`, field `/updaters/1`.
  - `profile_updaters_must_be_strings`: `updaters = [1]` gives the same code the `modules` list gives for a non-string item.
  - `unknown_key_hint_names_optional_keys`: an unknown profile key's hint is "remove the key; a new required key needs a new schema version, an optional key needs a manager that knows it".

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p dfm-model profile_ -- --nocapture`
Expected: FAIL (no `updaters` field, no `Updater` type).

- [ ] **Step 3: Implement**
  - Add `"updaters"` to the `only_keys` list in `profile()`. Parse it with the same string-list reader `modules` uses, then map each item through `Updater::parse`. Unknown name: `invalid(file, "unknown updater `<name>`", "use one of: brew, claude, codex, opencode, skills")` with `.field(format!("/updaters/{i}"))`.
  - Carry the vector to `Resolved` in `compose.rs`.
  - Change the `only_keys` hint to the text in the last test.

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo test -p dfm-model`
Expected: PASS, and no other model test changes.

- [ ] **Step 5: Update `docs/resolver.md`** with the `updaters` key, its values, its order rule and its errors.

- [ ] **Step 6: Commit**

```bash
git add crates/model/src/updater.rs crates/model/src/lib.rs crates/model/src/resolve/docs.rs crates/model/src/resolve/mod.rs crates/model/src/resolve/compose.rs docs/resolver.md <the test files>
git commit -m "feat(model): accept an updaters list in the profile (DFM-31)"
```

---

### Task 2: Run one updater

**Files:**
- Create: `crates/cli/src/adapters/updater.rs`, and register it in `crates/cli/src/adapters.rs`
- Test: unit tests in `crates/cli/src/adapters/updater.rs` that write look-alike scripts into a `TempDir` and point a `Runner` at it (follow how existing adapter unit tests build a `Runner`, if any; otherwise build `Runner { search_path, home, cwd, uid, grace }` directly)

**Interfaces:**
- Consumes: `dfm_model::updater::Updater` (Task 1), `dfm_engine::proc::{Runner, Request, Output, End}`.
- Produces:

```rust
pub struct Row {
    pub updater: Updater,
    pub status: Status,                 // dfm_model::state::Status
    pub before: Option<String>,         // version text, trimmed
    pub after: Option<String>,
    pub notes: Vec<String>,             // second copies, per-package lines, blocked reason, last stderr line
    pub started_at: String,             // rfc3339, as the other receipts write it
    pub finished_at: String,
}

pub struct Env<'a> {
    pub provider: &'a Runner,           // ctx.runner(sys): brew and mise
    pub home: &'a Path,
    pub held: &'a BTreeSet<String>,     // Homebrew names of active trials
}

pub fn run(u: Updater, env: &Env) -> Row;
pub fn preview(u: Updater, env: &Env) -> String; // "would run <commands> (<resolved path or blocked reason>)"
```

`run` never returns an error. Every failure is a row.

- [ ] **Step 1: Write the failing tests** (one look-alike per updater, each logging its arguments to a `log` file)
  - `claude_with_a_new_version_is_updated`: `claude --version` prints `2.1.311 (Claude Code)` before and `2.1.312 (Claude Code)` after `claude update`. Row: `Updated`, before and after as printed.
  - `claude_with_the_same_version_is_current`: the version does not change. Row: `Current`.
  - `codex_missing_is_blocked_with_the_path`: no file at `<home>/.codex/packages/standalone/current/bin/codex`. Row: `Blocked`, a note that names that path. The log shows no call.
  - `a_failing_update_is_failed_with_the_last_stderr_line`: `opencode upgrade` exits 1 and prints `network down` on stderr. Row: `Failed`, the note `network down`.
  - `a_timeout_is_failed`: the `Runner` returns `End::TimedOut` (use a short timeout in the test through a test-only constructor). Row: `Failed`, note `timed out after N minutes`.
  - `brew_upgrades_only_the_outdated_names`: `brew outdated --json=v2` lists formula `uv` (0.13.5 to 0.13.6) and cask `codex`. The log shows `brew update`, then `brew upgrade uv codex`. After the upgrade the list is empty. Row: `Updated`, notes `uv 0.13.5 -> 0.13.6 upgraded` and `codex ... upgraded`.
  - `brew_with_nothing_outdated_is_current_and_runs_no_upgrade`: empty list. Row: `Current`. The log has no `brew upgrade`.
  - `brew_partial_upgrade_is_failed_and_lists_each_package`: `brew upgrade` exits 1, and `uv` is still outdated afterwards. Row: `Failed`, notes `uv still outdated` and `codex upgraded`.
  - `brew_leaves_an_active_trial_out`: `held = {"uv"}`, `uv` outdated. The log shows `brew upgrade codex` only. Notes include `uv held (active trial)`.
  - `brew_env_keeps_homebrew_defaults`: the look-alike logs `HOMEBREW_NO_AUTO_UPDATE` and `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK` as `unset`, and `HOMEBREW_NO_ANALYTICS=1`.
  - `brew_invalid_outdated_json_is_failed`: the list is `not json`. Row: `Failed`, a note that names the parse error.
  - `skills_exit_0_is_current_with_a_note`: `mise exec -- npx skills update` exits 0. Row: `Current`, note `skills update ran; it reports no version`. The log shows exactly `exec -- npx skills update`.
  - `codex_second_copy_is_reported`: `<provider dir>/../Caskroom/codex` cannot be faked under `/opt/homebrew`, so the second-copy check asks `brew list --cask --versions codex` through the provider runner. The look-alike answers `codex 0.164.0`. Row notes include `second copy: Homebrew cask codex 0.164.0`.
  - `version_text_is_trimmed`: `--version` prints `codex-cli 0.164.0\n\n` before and after. Row: `Current`.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p dfm-cli --lib updater`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement `run` and `preview`**
  - Per-updater commands and owner directory as in Global Constraints. Resolve the native tools with a `Runner` whose `search_path` is only that tool's owner directory, so nothing falls back to `PATH`. A `resolve` refusal is `Blocked` with its detail.
  - Read the version with `<tool> --version` before and after. A version read that fails before the update is `Blocked`. One that fails after a successful update is `Pending` with a note.
  - `brew`: `brew update` (10 min), `brew outdated --json=v2` (10 min, parse `formulae[].name`, `installed_versions`, `current_version` and `casks[].name`, `installed_versions`, `current_version` with `serde_json`), `brew upgrade <names>` (30 min, raised `max_stdout`), `brew outdated --json=v2` again.
  - Second copies: for `claude`, `codex`, `opencode`, ask `brew list --versions <name>` and `brew list --cask --versions <name>` through the provider runner. A non-empty answer is a note. A failure of this check is a note, not a status change.
  - A stop request (`dfm_engine::signal::requested()`) before a command makes the row `Pending`.

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo test -p dfm-cli --lib updater`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/adapters/updater.rs crates/cli/src/adapters.rs
git commit -m "feat(cli): run one declared updater and report its status (DFM-31)"
```

---

### Task 3: The upgrade step in `dfm sync`

**Files:**
- Modify: `crates/cli/src/sync_cmd.rs` (`Args` and `parse_args` at about line 80, `run_inner`'s preview return at about line 1953, the "nothing to change" return at about line 1958, the normal end that calls `finish`, `finish` at about line 1817)
- Modify: `crates/cli/src/lib.rs` (usage text for `sync`)
- Create: `crates/cli/tests/upgrade.rs` (integration tests with look-alikes, built like `crates/cli/tests/toolchains.rs`: `--home <temp>`, empty environment, `DFM_PROVIDER_PATH` to the look-alike directory, the profile fixture with `updaters`)
- Create: `docs/upgrade.md`
- Modify: `docs/sync.md`, `README.md`

**Interfaces:**
- Consumes: `Resolved.updaters` (Task 1), `adapters::updater::{run, preview, Row, Env}` (Task 2), `Store::load()` (`crates/engine/src/store.rs:1213`) for the trial records (`TrialRecord.package` with `adapter == "homebrew/v1"`), `store.write_receipt`.
- Produces: `Args.upgrade: bool` (true unless `--no-upgrade`); `fn upgrade(ctx, args, resolved, out, printed) -> Result<Option<String>, Refusal>` in `sync_cmd.rs`, which returns the `upgrade:` summary text for `finish`.

- [ ] **Step 1: Write the failing integration tests** in `crates/cli/tests/upgrade.rs`
  - `sync_runs_the_declared_updaters_in_order`: profile `updaters = ["claude", "brew"]`. The log shows the claude calls before the brew calls. Stdout has an `upgrade:` section with one row per updater and the line `  upgrade: updated=2`.
  - `sync_runs_updaters_when_nothing_changes`: a fully current tree. Stdout has `nothing to change: no approval needed`, then the upgrade rows.
  - `no_upgrade_skips_every_updater`: `dfm sync --no-upgrade`. The log is empty and stdout has `upgrade: skipped (--no-upgrade)`.
  - `preview_lists_updaters_and_runs_none`: `dfm sync --preview`. Stdout names each updater with `would run`, and the log is empty.
  - `profile_without_updaters_says_so`: no key. Stdout has `upgrade: none declared in the profile`, and no receipt with `command = "upgrade"` is written.
  - `a_failed_updater_warns_and_exits_0`: `codex update` exits 1. Exit code 0. The last stdout lines include `warning: upgrade codex failed: <stderr line>`. The other updaters ran.
  - `a_blocked_scope_still_runs_the_upgrade`: the secrets scope is skipped (no `op` session, as in the existing sync tests). The upgrade runs, and both warnings appear.
  - `a_refused_sync_runs_no_updater`: a sync refused with `E_SOURCE_UNTRUSTED` (as in the existing sync tests). The log is empty.
  - `the_upgrade_receipt_records_each_updater`: the receipt with `command = "upgrade"` has one operation per updater (`resource = "updater:<name>"`, `adapter = "updater/v1"`, `class` the non-sensitive class, the row's status), and `result.outcome = "failed"`, `exit_code = 1` when any row is a problem, while the process exits 0.
  - `an_interrupted_upgrade_does_not_block_the_next_sync`: SIGINT while the brew look-alike sleeps. The next `dfm sync` runs every updater again and exits 0.
  - `no_upgrade_with_preview_is_allowed` and `no_upgrade_twice_is_a_usage_error`.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p dfm-cli --test upgrade --features test-isolation`
Expected: FAIL.

- [ ] **Step 3: Implement**
  - `parse_args`: `--no-upgrade` sets `upgrade = false`; a second one is `usage("--no-upgrade given twice")`.
  - Preview: after the plans print, print `upgrade:` and `preview(u, env)` per updater. No receipt.
  - The "nothing to change" path and the normal path both call `upgrade(...)` before `finish`. A refusal from a scope returns before it.
  - `upgrade(...)`: if `!args.upgrade`, print `upgrade: skipped (--no-upgrade)` and return. If `resolved.updaters` is empty, print `upgrade: none declared in the profile` and return. Take the writer lock with `Store::open_write_with` as `trial_cmd.rs:517` does, collect the Homebrew trial names from `store.load()`, run each updater in order, print each row (`  <status> <name> <before> -> <after>`, then `    note: <note>` lines, in the style of the plan rows), write one receipt (`command: "upgrade"`, `plan_digest: None`, `approval: None`, `result: RunResult::from_operations(&ops)`), print `receipt: <name>`, and push one warning per problem row into `printed.warnings`.
  - `finish` prints `  upgrade: <counts>` after the scope lines when `upgrade` returned a summary.
  - Install the signal handlers before the first updater if no scope installed them (the gate test allows `signal::install` only in `apply_cmd.rs` and `trial_cmd.rs`; add `sync_cmd.rs` to that list in `crates/cli/tests/gate.rs` with a one-line reason).

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo test -p dfm-cli --test upgrade --test sync --features test-isolation`
Expected: PASS, and the existing sync tests still pass. Where an existing sync test's profile has no `updaters`, its output gains only the `upgrade: none declared in the profile` line. Update those expectations, and do not change anything else in them.

- [ ] **Step 5: Write `docs/upgrade.md`** (the fixed set table, statuses, owner paths, detached runs, receipt, trials, second copies, known limits: no candidate version shown, `sudo` casks fail, skills reports no version) and update `docs/sync.md` ("The four scopes" gains the upgrade step and `--no-upgrade`) and `README.md` (the command table).

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/sync_cmd.rs crates/cli/src/lib.rs crates/cli/tests/upgrade.rs crates/cli/tests/sync.rs crates/cli/tests/gate.rs docs/upgrade.md docs/sync.md README.md
git commit -m "feat(sync): run the profile's updaters after the scopes (DFM-31)"
```

---

### Task 4: Doctor reports the latest upgrade

**Files:**
- Modify: `crates/model/src/doctor.rs` (new `DoctorCode::Upgrade`, `D_UPGRADE`, its summary)
- Modify: `crates/cli/src/doctor_cmd.rs` (the last-run row at about lines 586-596)
- Test: `crates/cli/tests/doctor.rs`
- Docs: `docs/doctor.md`

**Interfaces:**
- Consumes: receipts with `command = "upgrade"` (Task 3).
- Produces: `DoctorCode::Upgrade` with the string `D_UPGRADE` and the summary "The last upgrade did not end with `ok`."

- [ ] **Step 1: Write the failing tests**
  - `last_run_skips_upgrade_receipts`: an `ok` apply receipt, then a `failed` upgrade receipt. The `D_LAST_RUN` row is `ok`.
  - `a_failed_upgrade_is_a_warning_row`: the same state. A `D_UPGRADE` row at `warn`, fix text `Run dfm sync.`
  - `no_upgrade_receipt_means_no_upgrade_row`.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p dfm-cli --test doctor --features test-isolation`
Expected: FAIL.

- [ ] **Step 3: Implement** the filter on `command != "upgrade"` for the last-run row and a separate row for the latest upgrade receipt. Add `DoctorCode::Upgrade` to every list that enumerates the codes in `doctor.rs`.

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo test -p dfm-cli --test doctor --features test-isolation && cargo test -p dfm-model`
Expected: PASS.

- [ ] **Step 5: Update `docs/doctor.md`** (the code table) and commit.

```bash
git add crates/model/src/doctor.rs crates/cli/src/doctor_cmd.rs crates/cli/tests/doctor.rs docs/doctor.md
git commit -m "feat(doctor): report the latest upgrade apart from the last run (DFM-31)"
```

---

### Task 5: Branch gate and review

- [ ] **Step 1:** Run `just check` on the branch head. Expected: exit 0.
- [ ] **Step 2:** Run `codex review --base main` and fix every finding of medium or higher with a test, then run the gate again on the new head only.
- [ ] **Step 3:** Push and open the PR. Merge only after the GitHub Codex review gives its thumbs-up.
