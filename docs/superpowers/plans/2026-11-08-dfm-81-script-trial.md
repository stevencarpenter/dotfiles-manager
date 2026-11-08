# DFM-81 script trial implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `dfm trial <tool> --script <url>` installs a tool through its downloaded script under a `sandbox-exec` profile, observes a fixed watch set before and after, and records everything in a schema 5 trial record.

**Architecture:** The script path is a second branch of the `trial` command in a new `trial_cmd/script.rs` module, with the watch-set observation in `trial_cmd/window.rs`. The record types live in `dfm-model` (`state.rs`). The engine gains one new way to run a child with the terminal (`proc::Runner::run_shown`) and two terminal helpers in `signal.rs` (the only module with `unsafe`). Everything else (lock, record-first, journal, receipt, tracked-file guard) reuses what `trial`, `sync` and the store already do.

**Tech Stack:** Rust 2024 workspace (`dfm-model`, `dfm-engine`, `dfm-cli`), clap builder, serde_json, `libc` (signal.rs only), macOS `/usr/bin/sandbox-exec`, `/usr/bin/curl`. Tests: the `test-isolation` build with look-alike `curl` and `sandbox-exec` wrapper scripts on `DFM_PROVIDER_PATH`.

**Spec:** `docs/superpowers/specs/2026-11-03-dfm-78-script-trials-design.md` (main 427ed2b). Kaneo DFM-81 holds the criteria C01 to C11. Slice 1 of 4: purge (DFM-82), adopt (DFM-83) and doctor (DFM-84) come later and are refused for a script trial here.

## Global constraints

- `unsafe` appears only in `crates/engine/src/signal.rs`, each block with `#[expect(unsafe_code, reason = ...)]` directly on the function (`crates/cli/tests/gate.rs`). Update the gate tests that enumerate the functions and the `signal::install(` callers.
- Nothing is written before the record. Under the lock: record (`create_trial`) first, then the `trial-script` journal intent, then the script, then one `write_trial`, then the `done` or `failed` entry, then the receipt.
- Every path in the profile and the watch set is checked against the store's claim rules where it is a claim, and the profile uses canonical (realpath) paths, because the sandbox matches the real path (`/var/folders/...` is `/private/var/folders/...`).
- Later SBPL rules win over earlier ones (verified on Darwin 27.2.0 on 2026-11-08): `(allow default)`, then `(deny file-write* (subpath "/"))`, then the HOME and temp allows, then every specific deny.
- A literal deny on a directory blocks its rename, removal and mode change, and still allows new entries inside it (verified). Use it for every ancestor of a protected path below HOME.
- Secrets never reach the script: the environment is built from the fixed list only (spec, "The script's environment"). The script's output is shown live and never captured by dfm.
- Conventional Commits, no AI attribution, explicit `git add` paths. The gate is `bash scripts/check_all.sh`; the hosted `gate` job gates the merge.
- Prose in docs and messages follows the repo style: no em dash, plain sentences.

## Review focus

1. A profile string that a path breaks: a path with `"` or `\` or a control character must refuse the run (`E_TARGET_INVALID`) before any write; Task 5 tests `profile()` with such a path.
2. HOME reached through a link (`/var/...` style) or a watched directory that is itself a link: observation and profile must use canonical paths consistently, or a denied directory is not denied. Task 5 tests that the profile holds the canonical form; Task 7 tests a HOME under `/private/tmp` (the test TempDir is already canonical).
3. The script exits 0 but the cache file changed between preview and run: the digest compare catches it only because the sha256 of the cached bytes is recomputed under the lock. Task 8 tests `E_PLAN_STALE` for a changed and for a missing cache file.
4. A stop request while the script runs with `--tty`: dfm must take the terminal back before it prints anything, or its output stops dfm with SIGTTOU. Task 3 blocks SIGTTOU around `tcsetpgrp`; Task 8 runs the stop test under `script` (a pty) only when one is available, else it skips with a note.
5. A watched directory with thousands of entries (`/opt/homebrew/bin`): the observation must stay bounded. Task 6 caps entries per directory and total, and tests the cap.

---

### Task 1: Record types for a script trial (schema 5)

**Files:**
- Modify: `crates/model/src/state.rs` (trials section, after `TrialAdopted`; `TrialRecord`; `parse`; `validate`)
- Test: `crates/model/src/state.rs` (`#[cfg(test)] mod tests`, next to the existing trial tests)

**Interfaces:**
- Produces:
  ```rust
  pub const TRIAL_SCHEMA_SCRIPT: i64 = 5;
  pub struct Observed { pub path: String, pub kind: String, pub sha256: Option<String>, pub target: Option<String> }
  pub struct WindowEntry { pub path: String, pub change: String, pub kind: String, pub sha256: Option<String>, pub target: Option<String>, pub before: Option<Observed> }
  pub struct ScriptExit { pub code: Option<i32>, pub signal: Option<String> }
  pub struct ScriptBinary { pub path: String, pub real_path: String, pub sha256: String }
  pub struct TrialScript { pub url: String, pub effective_url: String, pub sha256: String, pub bytes: u64, pub shell: String, pub args: Vec<String>, pub env: Vec<(String, String)>, pub tty: bool, pub before: Vec<Observed>, pub exit: Option<ScriptExit>, pub window: Option<Vec<WindowEntry>>, pub binary: Option<ScriptBinary> }
  // TrialRecord gains: #[serde(default, skip_serializing_if = "Option::is_none")] pub script: Option<TrialScript>
  pub const SCRIPT_UNKNOWN: [&str; 4] = ["script-downloads", "later-self-updates", "keychain", "remote-effects"];
  ```
  All structs: `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] #[serde(deny_unknown_fields)]`. `Observed.kind` and `WindowEntry.kind` are one of `"file"`, `"dir"`, `"link"`, `"other"`. `WindowEntry.change` is one of `"created"`, `"modified"`, `"removed"`, `"kind-changed"`. `env` serializes as a JSON array of `[name, value]` pairs (serde's default for a `Vec<(String, String)>`), sorted by name by the writer.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_script_record_is_schema_5_and_round_trips() {
    let mut t = sample_trial(); // the existing helper, or build one as the harness does
    t.schema = TRIAL_SCHEMA_SCRIPT;
    t.package = None;
    t.script = Some(TrialScript {
        url: "https://example.test/install".into(),
        effective_url: "https://example.test/install".into(),
        sha256: "a".repeat(64),
        bytes: 10,
        shell: "sh".into(),
        args: vec!["--no-modify-path".into()],
        env: vec![("HOME".into(), "/h".into())],
        tty: false,
        before: vec![Observed { path: "/h/.zshenv".into(), kind: "link".into(), sha256: None, target: Some("/h/.dotfiles/zshenv".into()) }],
        exit: None,
        window: None,
        binary: None,
    });
    t.unknown = SCRIPT_UNKNOWN.iter().map(|s| s.to_string()).collect();
    let bytes = serde_json::to_vec(&t).unwrap();
    assert_eq!(TrialRecord::parse(&bytes).unwrap(), t);
}

#[test]
fn a_script_record_needs_schema_5_and_schema_5_needs_a_script() {
    let mut t = sample_trial();
    t.script = Some(minimal_script());
    for s in [SCHEMA, TRIAL_SCHEMA_ADOPTED, TRIAL_SCHEMA_DATA] {
        t.schema = s;
        assert!(t.validate().is_err(), "schema {s}");
    }
    let mut plain = sample_trial();
    plain.schema = TRIAL_SCHEMA_SCRIPT;
    assert!(plain.validate().is_err());
}

#[test]
fn a_script_record_with_data_or_adoption_stays_schema_5() {
    // data and adopted ride along on schema 5: the newest schema wins.
    let mut t = sample_trial();
    t.schema = TRIAL_SCHEMA_SCRIPT;
    t.script = Some(minimal_script());
    t.baseline.data = vec![BaselinePath { path: "/h/.cache/x".into(), existed: false, sha256: None }];
    assert!(t.validate().is_ok());
    t.adopted = Some(TrialAdopted { op_id: "op".into(), module: "m".into(), at_unix_ns: 1 });
    assert!(t.validate().is_ok());
}

#[test]
fn script_fields_are_validated() {
    // each of these makes validate() fail
    // url not https; effective_url not https; sha256 not 64 hex; shell not sh|bash|zsh;
    // an env name that is empty or holds '='; a before path that is not valid_path;
    // an observed kind outside the four; a window change outside the four;
    // exit with both code and signal, or neither; binary.sha256 not 64 hex;
    // an unknown token that is not a valid_id
}

#[test]
fn a_schema_6_record_is_newer() {
    let mut v: serde_json::Value = serde_json::to_value(sample_trial()).unwrap();
    v["schema"] = 6.into();
    assert_eq!(TrialRecord::parse(&serde_json::to_vec(&v).unwrap()).unwrap_err().code, Code::StateNewer);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p dfm-model state::tests::a_script_record -- --nocapture`
Expected: compile errors for the missing types.

- [ ] **Step 3: Add the types and the schema rule**

In `validate()`, the expected schema becomes: `script.is_some()` means `TRIAL_SCHEMA_SCRIPT`; otherwise the existing three-way match. `parse` accepts `&[SCHEMA, TRIAL_SCHEMA_ADOPTED, TRIAL_SCHEMA_DATA, TRIAL_SCHEMA_SCRIPT]`. Add `fn validate(&self) -> Result<(), Refusal>` on `TrialScript` with the checks listed in `script_fields_are_validated` (`valid_sha256`, `valid_path`, `valid_id` already exist in this file). Shell values: `["sh", "bash", "zsh"]` as `pub const SHELLS: [&str; 3]`.

- [ ] **Step 4: Run the model tests**

Run: `cargo test -p dfm-model`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/model/src/state.rs
git commit -m "feat(state): add the schema 5 script trial record (DFM-81)"
```

---

### Task 2: Running a shown child, with or without the terminal

**Files:**
- Modify: `crates/engine/src/signal.rs` (two new functions, after `new_session`)
- Modify: `crates/engine/src/proc.rs` (new `Shown` request and `Runner::run_shown`)
- Modify: `crates/engine/src/lib.rs` (re-export `proc::Shown` if the crate re-exports `Request`; keep the existing style)
- Modify: `crates/cli/tests/gate.rs` (`the_unsafe_lint_is_named_only_in_the_crate_roots_and_on_the_three_signal_functions`: add the two functions to `functions`, rename the test to say five)
- Test: `crates/engine/src/proc.rs` (`#[cfg(test)]`), `crates/cli/tests/runner.rs` if an integration-level check fits better

**Interfaces:**
- Produces, in `signal.rs`:
  ```rust
  /// pre_exec hook: block SIGTTOU, setpgid(0, 0), tcsetpgrp(0, getpid()). The child becomes the terminal's foreground group before exec, so its first read does not stop it.
  pub(crate) fn foreground_group(cmd: &mut std::process::Command);
  /// In dfm after the child ended: block SIGTTOU, tcsetpgrp(0, getpgrp()), restore the mask. Err(E_IO_FAILED) when tcsetpgrp fails.
  pub(crate) fn take_terminal() -> Result<(), Refusal>;
  ```
- Produces, in `proc.rs`:
  ```rust
  pub struct Shown<'a> {
      /// An absolute program path the caller already resolved (`Runner::resolve`).
      pub program: &'a Path,
      pub args: &'a [String],
      /// The whole environment; nothing is added.
      pub env: &'a [(String, String)],
      /// true: stdin is dfm's, the child is its own foreground process group. false: stdin is /dev/null and the child starts in a new session.
      pub tty: bool,
      pub timeout: Duration,
  }
  impl Runner {
      /// stdout and stderr are inherited. Returns how the child ended; a stop request or the timeout ends the child's group (SIGTERM, grace, SIGKILL) and is reported as End::Stopped / End::TimedOut. With tty, the terminal is taken back before returning, whatever happened.
      pub fn run_shown(&self, req: &Shown<'_>) -> Result<End, Refusal>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

```rust
// proc.rs tests
#[test]
fn run_shown_without_tty_reads_dev_null_and_reports_the_exit_code() {
    let r = test_runner(); // as the existing proc tests build one
    let program = PathBuf::from("/bin/sh");
    let args = vec!["-c".to_string(), "read x && exit 7; exit 3".to_string()];
    let end = r.run_shown(&Shown { program: &program, args: &args, env: &[("PATH".into(), "/usr/bin:/bin".into())], tty: false, timeout: Duration::from_secs(5) }).unwrap();
    assert!(matches!(end, End::Exited(3)), "{end:?}"); // read fails on /dev/null
}

#[test]
fn run_shown_passes_only_the_given_environment() {
    // sh -c 'test -z "$SECRET" && test "$ONLY" = 1' with env [("ONLY","1")] while the test process has SECRET set
    // Expected End::Exited(0)
}

#[test]
fn run_shown_ends_the_group_on_timeout() {
    // sh -c 'sleep 30 & sleep 30' with timeout 200 ms and grace 100 ms: End::TimedOut within 2 s, and `pgrep -g <pid>` finds nothing afterwards
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p dfm-engine run_shown`
Expected: compile error (`Shown` missing).

- [ ] **Step 3: Implement `foreground_group` and `take_terminal` in `signal.rs`**

Each gets `#[expect(unsafe_code, reason = "...")]` directly on the `fn`. The hook: `sigprocmask(SIG_BLOCK, {SIGTTOU})`, `setpgid(0, 0)`, `tcsetpgrp(0, getpid())`, then `sigprocmask(SIG_SETMASK, old)`; an error returns `Err(last_os_error())` from the hook. `take_terminal`: same mask dance around `tcsetpgrp(0, getpgrp())`. Both use only async-signal-safe calls in the hook.

- [ ] **Step 4: Implement `Runner::run_shown` in `proc.rs`**

Reuse `ensure_cwd`, the NUL check, `wait`, `Target` and `signal::kill_child`. Build the `Command` with `env_clear()` then `envs(req.env)`, `stdin(Stdio::null())` or `Stdio::inherit()`, `stdout`/`stderr` `Stdio::inherit()`. `tty: false` calls `signal::new_session(&mut cmd)`; `tty: true` calls `signal::foreground_group(&mut cmd)`. After `wait`, if the end is `TimedOut` or `Stopped`, wait the grace period for the reap (`try_wait` poll) and `kill(false)` if still there; then with `tty` call `signal::take_terminal()` and fold its error into the result only when the run itself was `Ok`.

- [ ] **Step 5: Update the gate test and run the engine tests**

Run: `cargo test -p dfm-engine && cargo test -p dfm-cli --features test-isolation --test gate`
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/signal.rs crates/engine/src/proc.rs crates/engine/src/lib.rs crates/cli/tests/gate.rs
git commit -m "feat(proc): run a child with inherited output and an optional terminal (DFM-81)"
```

---

### Task 3: Command line: `--script`, `--shell`, `--arg`, `--env`, `--tty`

**Files:**
- Modify: `crates/cli/src/cli.rs` (`trial` subcommand)
- Modify: `crates/cli/src/trial_cmd.rs` (`Args`, `parse_args`, the dispatch at the top of `run`; make `UNKNOWN`, `observe`, `candidates`, `data_candidates`, `Decision`-independent helpers `pub(super)`)
- Create: `crates/cli/src/trial_cmd/script.rs` (an empty `pub fn run(ctx, args, out) -> Result<(), Refusal>` that returns `todo!()`-free `Err(usage("not yet"))` for now) and `crates/cli/src/trial_cmd/window.rs` (empty)
- Test: `crates/cli/src/trial_cmd.rs` unit tests; `crates/cli/tests/cli.rs` if a completions or help test lists flags

**Interfaces:**
- Produces:
  ```rust
  pub struct Args {
      pub package: String,          // the tool name with --script
      pub id: Option<String>,
      pub provider: Option<String>,
      pub configs: Vec<String>,
      pub preview: bool,
      pub yes_plan: Option<String>,
      pub script: Option<ScriptArgs>,
  }
  pub struct ScriptArgs { pub url: String, pub shell: String /* sh|bash|zsh, default sh */, pub args: Vec<String>, pub env: Vec<(String, String)>, pub tty: bool }
  pub(super) const FIXED_ENV: [&str; 15] = ["HOME","USER","LOGNAME","TMPDIR","SHELL","PATH","LANG","LC_ALL","TERM","ZDOTDIR","XDG_CONFIG_HOME","XDG_DATA_HOME","XDG_STATE_HOME","XDG_CACHE_HOME","XDG_BIN_HOME"];
  ```

- [ ] **Step 1: Write the failing unit tests in `trial_cmd.rs`**

```rust
#[test]
fn script_arguments_parse_and_refuse_the_wrong_combinations() {
    let a = |v: &[&str]| crate::cli::parse("trial", v).and_then(|m| parse_args(&m));
    let ok = a(&["boat", "--script", "https://boat.dev/install", "--shell", "bash", "--arg", "--no-modify-path", "--env", "FOO=1", "--tty", "--preview"]).unwrap();
    let s = ok.script.unwrap();
    assert_eq!((s.url.as_str(), s.shell.as_str(), s.args.as_slice(), s.tty), ("https://boat.dev/install", "bash", &["--no-modify-path".to_string()][..], true));
    assert_eq!(s.env, vec![("FOO".to_string(), "1".to_string())]);
    assert_eq!(a(&["boat", "--script", "https://x/i"]).unwrap().script.unwrap().shell, "sh");
    for bad in [
        vec!["boat", "--script", "https://x/i", "--provider", "homebrew/v1"],
        vec!["boat", "--script", "http://x/i"],
        vec!["boat", "--script", "https://x/i", "--shell", "fish"],
        vec!["boat", "--script", "https://x/i", "--env", "HOME=/x"],
        vec!["boat", "--script", "https://x/i", "--env", "NOEQUALS"],
        vec!["boat", "--script", "https://x/i", "--env", "=v"],
        vec!["boat", "--script", "https://x/i", "--env", "A=1", "--env", "A=2"],
        vec!["homebrew/core/boat", "--script", "https://x/i"],
        vec!["boat", "--shell", "bash"],   // --shell, --arg, --env, --tty need --script
        vec!["boat", "--tty"],
    ] {
        assert_eq!(a(&bad).unwrap_err().code, Code::Usage, "{bad:?}");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p dfm-cli script_arguments`
Expected: compile error (`script` field missing).

- [ ] **Step 3: Add the flags and the parsing**

`cli.rs`: `.arg(value("script", "url")).arg(value("shell", "sh|bash|zsh")).arg(many("arg", "value")).arg(many("env", "NAME=VALUE")).arg(switch("tty"))`. `parse_args`: the URL must start with `https://`; the tool name must be a `valid_id` (no `/`, no `:`); `--env` splits at the first `=`, the name must be a plain identifier (`[A-Za-z_][A-Za-z0-9_]*`), not in `FIXED_ENV`, and unique. `run` starts with `if let Some(s) = &args.script { return script::run(ctx, args, s, out); }` placed after the binding check so the gate test's `signal::install(` ordering in `trial_cmd.rs` is unchanged.

- [ ] **Step 4: Run the cli unit tests and the completions test**

Run: `cargo test -p dfm-cli --features test-isolation trial_cmd && cargo test -p dfm-cli --features test-isolation --test cli`
Expected: pass. If a completion snapshot changed, update it as the test instructs.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/cli.rs crates/cli/src/trial_cmd.rs crates/cli/src/trial_cmd/script.rs crates/cli/src/trial_cmd/window.rs
git commit -m "feat(trial): parse the --script flags (DFM-81)"
```

---

### Task 4: Fetch, cache, hints and the script environment

**Files:**
- Modify: `crates/cli/src/trial_cmd/script.rs`
- Test: `crates/cli/src/trial_cmd/script.rs` unit tests (pure functions) and `crates/cli/tests/trial_script.rs` (new test binary, registered in `crates/cli/Cargo.toml` with `required-features = ["test-isolation"]`) for the fetch through the fake `curl`
- Modify: `crates/cli/tests/common/providers.rs` (add `FAKE_CURL`, `FAKE_SANDBOX_EXEC`, `FAKE_GETCONF`)

**Interfaces:**
- Produces:
  ```rust
  pub(super) struct Fetched { pub path: PathBuf, pub sha256: String, pub bytes: u64, pub effective_url: String, pub body: Vec<u8> }
  /// `<cache_dir>/scripts/<sha256(url)>` and its sidecar `<...>.json` {"url","effective_url","sha256","bytes"}.
  pub(super) fn cache_paths(cache_dir: &Path, url: &str) -> (PathBuf, PathBuf);
  /// curl through the runner: `curl --proto =https --proto-redir =https --tlsv1.2 -fsSL --max-filesize 1048576 -o <tmp> -w %{url_effective} -- <url>`; writes the cache file (0600) and the sidecar; E_SOURCE_UNTRUSTED when curl fails or the effective URL is not https; E_LIMIT_EXCEEDED above 1048576 bytes.
  pub(super) fn fetch(runner: &Runner, cache_dir: &Path, url: &str) -> Result<Fetched, Refusal>;
  /// Reads the cache file and sidecar without fetching. None when either is missing. E_PLAN_STALE when the sidecar sha256 differs from the bytes.
  pub(super) fn cached(cache_dir: &Path, url: &str) -> Result<Option<Fetched>, Refusal>;
  /// The two preview notes from plain text matches.
  pub(super) fn hints(body: &[u8]) -> Vec<&'static str>;
  pub(super) const HINT_TTY: &str = "note: the script reads /dev/tty; pass --tty to answer its prompts";
  pub(super) const HINT_STARTUP: &str = "note: the script edits shell startup files; files in the dotfiles repo are write-protected during the run; pass the script's own opt-out if it has one";
  /// The script's whole environment, sorted by name (spec, "The script's environment"). `tty` adds TERM. `extra` is the --env list.
  pub(super) fn script_env(runner: &Runner, tty: bool, extra: &[(String, String)]) -> Result<Vec<(String, String)>, Refusal>;
  ```
  `script_env` sources: `HOME` = runner.home; `USER` = dfm's `USER`, else `LOGNAME`, else `id -un` through the runner; `LOGNAME` = dfm's `LOGNAME` else `USER`; `TMPDIR` = dfm's `TMPDIR` when absolute, else `getconf DARWIN_USER_TEMP_DIR` through the runner, canonicalized; `SHELL` = dfm's `SHELL` when absolute, else `/bin/sh` (shortcut: the login shell from the directory service is not read); `PATH` = the runner's joined search path (expose `Runner::joined_path` as `pub`); `LANG`, `LC_ALL`, `ZDOTDIR`, `XDG_*` from dfm's environment when set; `TERM` from dfm's environment only with `tty`.

- [ ] **Step 1: Write the fakes in `providers.rs`**

```sh
# FAKE_CURL: logs "curl <args>" to ../log; the body comes from ../curl-body, the effective URL from ../curl-effective (default: the URL argument), the exit code from ../curl-exit (default 0). Writes the body to the -o path and prints the effective URL (the -w format) on stdout.
# FAKE_SANDBOX_EXEC: `exec /usr/bin/sandbox-exec "$@"` after logging "sandbox-exec <profile text> | <command>" to ../log; with the marker ../sandbox-missing it exits 127 (the test deletes the file instead to simulate absence, see Task 8).
# FAKE_GETCONF: prints the content of ../tmpdir, else `exec /usr/bin/getconf "$@"`.
```

- [ ] **Step 2: Write the failing tests**

Unit (`script.rs`):
```rust
#[test] fn hints_match_plain_text() { assert_eq!(hints(b"x\n cat < /dev/tty"), [HINT_TTY]); assert_eq!(hints(b"echo >> ~/.zshrc"), [HINT_STARTUP]); assert!(hints(b"nothing").is_empty()); }
#[test] fn cache_paths_are_keyed_by_the_url_hash() { let (f, s) = cache_paths(Path::new("/c"), "https://a/b"); assert_eq!(f, Path::new("/c/scripts").join(sha256_hex(b"https://a/b"))); assert_eq!(s.extension().unwrap(), "json"); }
#[test] fn script_env_is_the_fixed_list_sorted_and_tty_adds_term() { /* set ZDOTDIR and TERM in a scoped way via a helper that passes an explicit env map; assert names == the expected subset, sorted; TERM present only with tty */ }
```
Make `script_env` take the dfm environment as `&[(String, String)]` (`std::env::vars()` collected by the caller) so the unit test needs no process-global state.

Integration (`tests/trial_script.rs`, fixture `Fx` copied from `tests/trial.rs` plus the three fakes):
```rust
#[test] fn preview_fetches_once_caches_the_script_and_prints_the_path_and_hints() {
    // curl-body: "#!/bin/sh\nread x < /dev/tty\n"; preview exit 0; stdout has "script file: <home>/.cache/dotfiles-manager/scripts/<sha(url)>", "unknown: ... script-downloads ...", HINT_TTY; log has exactly one curl line with the five fixed options; the cache file mode is 0600; the sidecar holds the body sha256.
}
#[test] fn a_redirect_to_http_is_refused() { /* curl-effective: http://x/i -> exit 3, E_SOURCE_UNTRUSTED, no cache file */ }
#[test] fn a_failed_fetch_is_refused() { /* curl-exit: 22 -> exit 3, E_SOURCE_UNTRUSTED */ }
#[test] fn a_script_over_the_size_cap_is_refused() { /* body 1048577 bytes -> E_LIMIT_EXCEEDED */ }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p dfm-cli --features test-isolation --test trial_script`
Expected: failures (`script::run` returns the placeholder).

- [ ] **Step 4: Implement `fetch`, `cached`, `hints`, `script_env`, and enough of `script::run` to print the preview**

`script::run` at this point: binding, tool and id checks (as `trial_cmd::run`), `signal::install()`, the PATH pre-existing check (Task 5 adds it; stub as no-op here), candidates and data candidates through `trial_cmd::candidates` and `data_candidates`, `fetch` (preview or no `--yes-plan`) or `cached` (with `--yes-plan`), `script_env`, then print the preview lines (exact format in Task 7) and `plan digest:`. The curl temp file is `<cache_dir>/scripts/.fetch-<pid>`; the directory is created 0700; the file is renamed over `<sha(url)>` and the sidecar written with `fs::write` then `set_permissions(0o600)`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p dfm-cli --features test-isolation --test trial_script`
Expected: the four tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/trial_cmd/script.rs crates/cli/tests/trial_script.rs crates/cli/tests/common/providers.rs crates/cli/Cargo.toml crates/engine/src/proc.rs
git commit -m "feat(trial): fetch and cache a script and build its environment (DFM-81)"
```

---

### Task 5: The sandbox profile and the pre-existing tool check

**Files:**
- Modify: `crates/cli/src/trial_cmd/script.rs`
- Modify: `crates/cli/src/sync_cmd.rs` (`guarded_checkouts`, `tracked_status` and `Git::new` become `pub(crate)`)
- Test: `crates/cli/src/trial_cmd/script.rs` unit tests

**Interfaces:**
- Produces:
  ```rust
  pub(super) struct Protected { pub home: PathBuf, pub temp: Vec<PathBuf> /* canonical TMPDIR, /private/tmp, /dev */, pub denied_trees: Vec<PathBuf> /* guarded checkouts, state root, config dir, data dir, cache dir, dfm's install dir */, pub denied_paths: Vec<PathBuf> /* ledger targets, lock file, dfm's own executable path */ }
  /// The SBPL text. Every path is canonicalized where it exists (a missing path keeps its parent's canonical form), then checked: printable ASCII or UTF-8 letters, no `"`, no `\`, no control character; a path that fails is E_TARGET_INVALID. Ancestors of every denied path and tree, strictly below HOME, get a literal deny.
  pub(super) fn profile(p: &Protected) -> Result<String, Refusal>;
  pub(super) const DENIED_EXEC: [&str; 3] = ["/usr/bin/sudo", "/usr/bin/su", "/usr/bin/osascript"];
  /// The executables named `tool` on the dfm process PATH plus `provider_path(home)`: a regular file with an execute bit, or a link to one. Empty when none.
  pub(super) fn on_path(home: &Path, path_var: &OsStr, tool: &str) -> Vec<PathBuf>;
  /// `Protected` from the layout, the binding root's guarded checkouts, the ledger and `std::env::current_exe()`.
  pub(super) fn protected(ctx: &Ctx, layout: &Layout, root: &Path, ledger: &Ledger, temp: &Path) -> Result<Protected, Refusal>;
  ```
  Profile shape (one rule per line):
  ```
  (version 1)
  (allow default)
  (deny file-write* (subpath "/"))
  (allow file-write* (subpath "<home>") (subpath "<tmpdir>") (subpath "/private/tmp") (subpath "/dev"))
  (deny file-write* (subpath "<each denied tree>"))
  (deny file-write* (literal "<each denied path>") (literal "<each ancestor below home>"))
  (deny process-exec (literal "/usr/bin/sudo") (literal "/usr/bin/su") (literal "/usr/bin/osascript"))
  ```

- [ ] **Step 1: Write the failing unit tests**

```rust
#[test] fn the_profile_denies_trees_paths_ancestors_and_exec() {
    let p = Protected { home: "/h".into(), temp: vec!["/private/var/folders/x/T".into()], denied_trees: vec!["/h/.dotfiles".into(), "/h/.local/state/dotfiles-manager".into()], denied_paths: vec!["/h/.config/zsh/.zshrc".into(), "/h/.local/state/dotfiles-manager.lock".into()] };
    let text = profile(&p).unwrap();
    for want in ["(allow default)", "(deny file-write* (subpath \"/\"))", "(subpath \"/h\")", "(subpath \"/h/.dotfiles\")", "(literal \"/h/.config/zsh/.zshrc\")", "(literal \"/h/.config/zsh\")", "(literal \"/h/.config\")", "(literal \"/h/.local\")", "(literal \"/h/.local/state\")", "(literal \"/h/.local/state/dotfiles-manager.lock\")", "(literal \"/usr/bin/sudo\")"] {
        assert!(text.contains(want), "{want}\n{text}");
    }
    assert!(!text.contains("(literal \"/h\")"), "HOME itself is not denied");
    let allow_at = text.find("(allow file-write*").unwrap();
    assert!(text.rfind("(deny file-write*").unwrap() > allow_at, "denies come after the allow");
}
#[test] fn a_path_the_profile_cannot_hold_refuses() { for bad in ["/h/a\"b", "/h/a\\b", "/h/a\nb"] { let p = Protected { home: "/h".into(), temp: vec![], denied_trees: vec![bad.into()], denied_paths: vec![] }; assert_eq!(profile(&p).unwrap_err().code, Code::TargetInvalid, "{bad}"); } }
#[test] fn on_path_finds_executables_and_ignores_the_rest() { /* TempDir with bin/t (0755), bin2/t (0644), bin3/t -> bin/t link; PATH=bin:bin2:bin3; expect [bin/t, bin3/t] plus nothing from provider_path */ }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p dfm-cli --features test-isolation trial_cmd::script`
Expected: compile errors.

- [ ] **Step 3: Implement `profile`, `on_path`, `protected`**

`protected`: trees = `sync_cmd::guarded_checkouts(root)?` plus `layout.state_root`, `layout.config_dir`, `layout.data_dir`, `layout.cache_dir`, and the parent of `fs::canonicalize(std::env::current_exe()?)`; paths = every `owned_outputs[].path` of every ledger resource, `layout.lock_file`, and `current_exe()` as returned (the link, when it is one). Canonicalize with a helper that canonicalizes the deepest existing ancestor and re-appends the rest. Dedup and sort. Ancestors: for each denied tree and path, every ancestor that `starts_with(home)` and is not `home`.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p dfm-cli --features test-isolation trial_cmd::script`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/trial_cmd/script.rs crates/cli/src/sync_cmd.rs
git commit -m "feat(trial): generate the script sandbox profile and refuse a tool already on PATH (DFM-81)"
```

---

### Task 6: The observation window

**Files:**
- Create/Modify: `crates/cli/src/trial_cmd/window.rs`
- Test: `crates/cli/src/trial_cmd/window.rs` unit tests over a `TempDir`

**Interfaces:**
- Produces:
  ```rust
  pub(super) const MAX_DIR_ENTRIES: usize = 10_000;
  pub(super) const MAX_OBSERVED: usize = 50_000;
  pub(super) const HASH_CAP: u64 = 16 * 1024 * 1024;
  pub(super) enum Watch { File(PathBuf), Dir(PathBuf) /* entries with hashes */, Listing(PathBuf) /* entries, name and kind only */ }
  /// The watch set of the spec table, from HOME and the script environment (ZDOTDIR when set).
  pub(super) fn watch_set(home: &Path, env: &[(String, String)]) -> Vec<Watch>;
  /// One pass over the watch set. A watched path that is absent yields nothing. E_LIMIT_EXCEEDED above the caps. An unreadable entry is recorded with kind "other" and no hash (never a failure: the window is evidence, not authority).
  pub(super) fn observe(watch: &[Watch]) -> Result<Vec<Observed>, Refusal>;
  /// Sorted by path. created: in after only. removed: in before only. kind-changed: kinds differ. modified: same kind and (sha256 or target) differ.
  pub(super) fn diff(before: &[Observed], after: &[Observed]) -> Vec<WindowEntry>;
  /// The first executable named `tool` on the PATH entries (dfm's PATH plus provider_path): path, canonical real_path, sha256 of the real file (None when over HASH_CAP or unreadable, with a note).
  pub(super) fn binary(home: &Path, path_var: &OsStr, tool: &str) -> Option<ScriptBinary>;
  ```
  Watch set: files `~/.profile ~/.bash_profile ~/.bash_login ~/.bashrc ~/.zshenv ~/.zprofile ~/.zshrc ~/.zlogin ~/.zlogout`, the same zsh names under `$ZDOTDIR` when set and different from HOME, `~/.config/fish/config.fish`; `Dir` for `~/.config/fish/conf.d ~/.local/bin ~/bin /usr/local/bin /opt/homebrew/bin`; `Listing` for `HOME ~/.config ~/.local ~/.local/share ~/.local/state ~/.cache ~/Library/Application Support ~/Library/LaunchAgents`. An `Observed` for a `File` watch is the file itself (kind through `symlink_metadata`; for a link, `target` and the sha256 of the content through the link when it is a regular file under the cap). A `Dir` entry: kind by `symlink_metadata`, `target` for a link, sha256 for a regular file under the cap. A `Listing` entry: name and kind only.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn the_watch_set_follows_home_and_zdotdir() { let w = watch_set(Path::new("/h"), &[("ZDOTDIR".into(), "/h/.config/zsh".into())]); assert!(w.iter().any(|x| matches!(x, Watch::File(p) if p == Path::new("/h/.config/zsh/.zshenv")))); assert!(w.iter().any(|x| matches!(x, Watch::Listing(p) if p == Path::new("/h/Library/LaunchAgents")))); assert!(w.iter().any(|x| matches!(x, Watch::Dir(p) if p == Path::new("/opt/homebrew/bin")))); }
#[test] fn observe_and_diff_report_each_change_kind() {
    // TempDir home: .zshrc (file), .local/bin/{a (exec file), l -> a}, .config/x (dir)
    // before = observe(); then: append to .zshrc (modified), create .local/bin/b (created), remove l (removed), replace .config/x dir with a file (kind-changed), create .config/newdir (created in listing)
    // diff must hold exactly those five with the right change, kind, sha256/target, and `before` on the modified entry
}
#[test] fn a_listing_change_inside_an_entry_is_not_seen() { /* a new file inside an existing ~/.config/x is not in the diff */ }
#[test] fn too_many_entries_is_a_limit_error() { /* a Dir watch with MAX_DIR_ENTRIES + 1 files -> E_LIMIT_EXCEEDED */ }
#[test] fn binary_follows_links_and_hashes_the_real_file() { /* bin/t -> real/t; binary().unwrap() has path bin/t, real_path real/t, sha256 of real/t */ }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p dfm-cli --features test-isolation trial_cmd::window`
Expected: compile errors.

- [ ] **Step 3: Implement `window.rs`**

Reuse `crate::read_fs::read_regular_bounded` for hashing and `dfm_model::digest::sha256_hex`. Sort entries by path before returning.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p dfm-cli --features test-isolation trial_cmd::window`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/trial_cmd/window.rs
git commit -m "feat(trial): observe the script trial watch set and diff it (DFM-81)"
```

---

### Task 7: The run: decision, digest, lock, record, sandbox, guard, receipt

**Files:**
- Modify: `crates/cli/src/trial_cmd/script.rs` (`Decision`, `run`)
- Modify: `crates/cli/src/doctor_cmd.rs` (`trial_rows`: a script trial's `how` is `"{tool} installed by the trial through {url}"`)
- Modify: `crates/cli/src/purge_cmd.rs` and `crates/cli/src/adopt_cmd.rs` (`trial_record`: a record with `script` is `E_CONFLICT` "purge of a script trial is not implemented yet (DFM-82)" / "adopt of a script trial is not implemented yet (DFM-83)")
- Modify: `crates/cli/tests/gate.rs` (the `signal::install(` callers list gains `crates/cli/src/trial_cmd/script.rs`; its call must come before the first `fetch(` or `on_path(`)
- Test: `crates/cli/tests/trial_script.rs`

**Interfaces:**
- Consumes: Tasks 1 to 6.
- Produces, in `script.rs`:
  ```rust
  struct Decision { trial_id, tool, fetched: Fetched, shell: String, args: Vec<String>, env: Vec<(String, String)>, tty: bool, baseline: Vec<BaselinePath>, data: Vec<BaselinePath>, skipped: Vec<String>, binary: Binary /* dfm's own identity */, stamp: Stamp }
  impl Decision { fn digest(&self) -> Result<String, Refusal>; fn record(&self, created: u64, before: Vec<Observed>) -> TrialRecord; fn print(&self, out, digest, hints: &[&str]) -> io::Result<()> }
  pub(super) fn run(ctx: &Ctx, args: &Args, s: &ScriptArgs, out: &mut dyn Write) -> Result<(), Refusal>;
  pub(super) const UNKNOWN: [&str; 7] = ["services", "paths-outside-baseline", "provider-caches", "script-downloads", "later-self-updates", "keychain", "remote-effects"];
  const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60 * 60); // shortcut: one fixed hour, upgrade when a flag asks for it
  ```
  Digest document: `{"trial_id","tool","script":{"url","effective_url","sha256","bytes","shell","args","env","tty"},"baseline","data","data_skipped","unknown","binary":{"version","sha256"},"binding":{...}}`, canonical through `serde_json::Value`.

  Preview lines, in order:
  ```
  trial: <id> tool=<tool>
  script: <url> -> <effective_url> sha256:<sha> bytes=<n>
  script file: <cache path>
  run: sandbox-exec /bin/<shell> <script file> <args...> tty=<true|false>
  env: NAME=VALUE NAME=VALUE ...            (sorted; the whole list)
  note: ...                                  (zero, one or two hint lines)
  baseline: ... / data: ...                  (as the provider trial prints them)
  watch: shell startup files, bin directories and top-level listings are observed before and after the run; they are not in the digest
  cleanup scope: dfm purge of a script trial is not available yet (DFM-82); the window in the record lists what changed
  unknown: services paths-outside-baseline provider-caches script-downloads later-self-updates keychain remote-effects (candidates for review, never owned cleanup scope)
  plan digest: <digest>
  ```
  Result lines after the run follow `trial_cmd::run`: `  <label> <trial_id> <op_id>[: error]`, `trial: state/trials/<file>`, `receipt: <name>`, `result: <outcome>`. Between the preview and the result lines, the script's own output appears (inherited).

- [ ] **Step 1: Write the failing integration tests (the happy path and the record)**

Fixture additions in `tests/trial_script.rs`: `Fx::script(body)` writes `curl-body`; `Fx::trial_script(tool, extra_args)` runs `trial <tool> --script https://example.test/<tool>`; `Fx::record(id) -> serde_json::Value`; `Fx::bind_with_links()` binds the common profile and runs `apply` so `~/.config/git/config`, `~/.ssh/config`, `~/.config/tmux/tmux.conf` and `~/.zshrc` are dfm links into the source tree (reuse `common::Fx::new` plus `apply --yes-plan`), and makes the source tree a git checkout (`git init`, commit) so `tracked_status` works. The fake PATH for the dfm process in tests: `PATH=<home>/.local/bin:/usr/bin:/bin` passed through `cmd.env("PATH", ...)`.

```rust
#[test] fn a_script_trial_records_before_runs_sandboxed_and_rewrites_the_record() {
    // body: creates ~/.local/bin/boat (exec), ~/.ascii/ (dir with a file), appends to ~/.zshrc? no: ~/.zshrc is a dfm link here -> that is the denied test. Use a body that creates ~/.local/bin/boat, ~/.ascii/bin/boat, ~/.config/ascii/config and a link ~/.local/bin/boat -> ~/.ascii/bin/boat, then exits 0.
    // preview -> digest; run with -y: exit 0; record schema 5; script.before non-empty; script.exit == {"code":0}; window has created entries for ~/.local/bin/boat (kind link, target), ~/.ascii (listing of HOME: created dir), ~/.config/ascii (listing: created dir); binary.path == ~/.local/bin/boat, real_path == ~/.ascii/bin/boat; the journal has trial-script intent and done with the same op_id; a receipt with command "trial" exists; the sandbox-exec log line holds "(deny process-exec" and the state root path.
}
#[test] fn a_nonzero_script_exit_is_exit_1_after_the_record() { /* body: `exit 9`; dfm exit 1; record exit {"code":9}; journal failed */ }
#[test] fn the_run_never_fetches_again_and_a_changed_or_missing_cache_is_stale() {
    // preview (1 curl line); edit the cache file -> run -y: exit 3 E_PLAN_STALE, still 1 curl line, no record; remove the cache file -> E_PLAN_STALE; restore via a new preview -> run succeeds
}
#[test] fn a_pre_existing_tool_refuses_the_trial() { /* ~/.local/bin/boat exists and is executable -> preview exit 3 E_CONFLICT naming the path; nothing fetched */ }
#[test] fn the_tracked_file_guard_fails_the_trial_when_a_tracked_file_changed() {
    // The sandbox makes this impossible through the script, so the test plants the change with the `trial-script-after-run` failpoint hook: DFM_FAIL=... as other tests use fail.hit(); simplest: the script writes to a path that the test maps to a tracked file through a git worktree trick is not possible; instead the test modifies a tracked source file from outside while the script sleeps 1 s. Expected: exit 1, E_CONFLICT naming the file, the change left in place, record written with the window.
}
#[test] fn a_dev_tty_read_fails_without_tty() { /* body: `read x < /dev/tty || exit 5`; expect record exit {"code":5} */ }
#[test] fn doctor_shows_the_script_trial_and_purge_and_adopt_refuse_it() { /* doctor row text contains "through https://"; purge -> exit 3 E_CONFLICT "DFM-82"; adopt --module extra -> exit 3 E_CONFLICT "DFM-83" */ }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p dfm-cli --features test-isolation --test trial_script`
Expected: the new tests fail (the run is not implemented).

- [ ] **Step 3: Implement `run`**

Order (mirrors `trial_cmd::run` and the spec "Order of one run"):
1. binding; tool `valid_id`; trial id; `binary_identity`; stamp; `signal::install()`.
2. `on_path(home, PATH, tool)` non-empty: `E_CONFLICT` "an executable named <tool> is already on PATH: <paths>; remove or purge it first". Before any fetch.
3. `Store::open_read`; identity; candidates and claims; data candidates (as `trial_cmd::run`).
4. `fetch` (no `--yes-plan`) or `cached` (`--yes-plan`; `None` is `E_PLAN_STALE` "the script cache for <url> is missing; run the preview again").
5. `script_env`; `temp` = the TMPDIR value from the env list; `protected(ctx, &layout, root, &ledger_lenient, temp)`; `profile(&p)?` (so a path the profile cannot hold refuses before the preview prints).
6. `runner.resolve("sandbox-exec")` -> `E_PLATFORM_UNSUPPORTED` "sandbox-exec is not available; a script trial needs it". Then a dry run `Runner::run(Request{program:"sandbox-exec", args:["-p", profile, "/usr/bin/true"], ...})` whose nonzero exit is `E_PLATFORM_UNSUPPORTED` "the sandbox profile was not accepted: <stderr>".
7. The decision, digest, `print`; `--preview` stops; no `--yes-plan` is `E_APPROVAL_REQUIRED`; a different digest is `E_PLAN_STALE`.
8. `--tty` requires `std::io::stdin().is_terminal()`, else usage error "--tty needs a terminal on standard input".
9. Existing record check, `drop(readable)`, `fail.hit("trial-before-lock")`.
10. `Store::open_write_with`; identity; `read_ledger`; recompute the decision (cached bytes, env, protected, profile) and compare the digest (`E_PLAN_STALE`).
11. `tracked_status` before (`Git::new(ctx, &sys)`, `guarded_checkouts(root)`); `window::observe(&watch_set)` before; `create_trial(record with before)`; `fail.hit("trial-script-after-record")`.
12. journal `trial-script` intent (`resource` = trial id, `adapter` = `"script/v1"`, empty recovery).
13. `runner.run_shown(Shown{ program: &sandbox_exec, args: ["-p", profile, "/bin/<shell>", cache_file, args...], env, tty, timeout: SCRIPT_TIMEOUT })`.
14. `observe` after; `diff`; `binary`; `tracked_status` after and compare (as `sync_cmd` does): a difference is `E_CONFLICT` "the script changed the tracked file <path> (<code>); it is left in place, not reverted, staged or committed".
15. `write_trial(record with exit, window, binary)`; the cache file is removed after a run that reached this point (shortcut: no cache retention policy; the record keeps the sha256).
16. journal `done` (exit 0, no guard finding, no stop) or `failed` (reason); status `Updated` or `Failed`; the receipt and result lines as `trial_cmd::run`. A nonzero exit, a guard finding or a stop request is exit 1 (or 130/143 for the stop) after the record.

`End::Stopped(r)`: `script.exit = {signal: "SIGTERM"}` style (`"stopped"` when dfm's own stop request ended it), the window as observed, journal failed with the stop detail, exit with `r.exit()`. `End::TimedOut`: `signal: "timeout"`, exit 1. `End::Signaled`: `signal: "signal"`.

- [ ] **Step 4: Run the focused tests, then the gate test and the trial tests**

Run: `cargo test -p dfm-cli --features test-isolation --test trial_script && cargo test -p dfm-cli --features test-isolation --test gate --test trial --test doctor --test adopt`
Expected: pass. Sandbox tests print `SKIPPED: sandbox-exec missing` through `common::skip` and return when `/usr/bin/sandbox-exec` is absent (hosted Linux runners); `common::is_root()` runs are skipped too, because the sandbox does not confine root the same way.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/trial_cmd/script.rs crates/cli/src/doctor_cmd.rs crates/cli/src/purge_cmd.rs crates/cli/src/adopt_cmd.rs crates/cli/tests/gate.rs crates/cli/tests/trial_script.rs
git commit -m "feat(trial): run a script trial under sandbox-exec with an observation window (DFM-81)"
```

---

### Task 8: Sandbox denial tests and the `--tty` stop test

**Files:**
- Test: `crates/cli/tests/trial_script.rs`

**Interfaces:**
- Consumes: Task 7's fixture.

- [ ] **Step 1: Write the denial tests (C08)**

Each test plants a script body, runs the trial with `-y`, and asserts on the exit, the record and the file system. All skip with a note when `sandbox-exec` is missing.

```rust
#[test] fn a_write_through_a_dfm_link_is_denied_and_the_repo_file_is_unchanged() { /* body: `echo x >> ~/.zshrc; echo rc=$? > ~/result`; ~/result holds rc=1; the source file bytes are unchanged; the tracked guard finds nothing; dfm exit 0 */ }
#[test] fn replacing_or_removing_a_dfm_link_is_denied() { /* body: `mv ~/.zshrc ~/.zshrc.bak; unlink ~/.ssh/config; echo done > ~/result`; both links still there with their targets */ }
#[test] fn sudo_su_and_osascript_are_denied() { /* body runs each with `|| echo denied-<name> >> ~/result`; all three lines present */ }
#[test] fn a_write_outside_home_is_denied() { /* body: `echo x > /usr/local/bin/zz || echo d1 >> ~/result; echo x > /private/var/tmp/zz || echo d2 >> ~/result`; also a write into TMPDIR succeeds */ }
#[test] fn the_binding_and_the_lock_are_protected() { /* body: `echo x > ~/.config/dotfiles-manager/machine.toml || echo d1 >> ~/result; unlink ~/.local/state/dotfiles-manager.lock || echo d2 >> ~/result`; binding bytes unchanged; the lock file still exists */ }
#[test] fn renaming_an_ancestor_of_a_protected_path_is_denied() { /* body: `mv ~/.config ~/.config2 || echo d >> ~/result`; ~/.config still exists */ }
#[test] fn created_directory_bin_file_and_link_are_recorded() { /* covered by Task 7's happy path; assert the three window entries here with their kinds and that the bin file entry has a sha256 */ }
```

- [ ] **Step 2: Write the `--tty` stop test**

Run dfm under `/usr/bin/script -q /dev/null <dfm ...>` so it has a pty, with a body that writes `started` to `~/result`, then `sleep 30`. The test waits for `~/result`, sends SIGTERM to the dfm process (its pid from the fixture, through `script`'s child; use `pgrep -P`), and asserts: dfm exits 143 within 15 s, the record has a `window` (not null) and `exit.signal == "stopped"`, the journal has a `failed` entry, and `pgrep -f sleep 30` under the test HOME finds nothing. Skip with a note when `/usr/bin/script` is missing or the test has no pty.

- [ ] **Step 3: Run the tests**

Run: `cargo test -p dfm-cli --features test-isolation --test trial_script`
Expected: pass on the M5; the stop test may need `--test-threads 1` only for itself, in which case mark it `#[ignore]` with a reason and add it to `step_network_tests`'s selection in the gate script with a note, or make it robust with a unique marker directory. Prefer robust.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/tests/trial_script.rs
git commit -m "test(trial): cover the script sandbox denials and the --tty stop (DFM-81)"
```

---

### Task 9: Documentation

**Files:**
- Modify: `docs/trial.md` (new section "Script trials" after "The record": the command, the environment, the sandbox, the window, the record fields, the exit codes, the known limits; update the Part table with `trial_cmd/script.rs`, `trial_cmd/window.rs`, `tests/trial_script.rs`)
- Modify: `docs/state.md` ("Documents": the trial record is schema 5 when it carries `script`; the `trial-script` journal operation; the `scripts/` cache directory under the cache root)
- Modify: `docs/purge.md` and `docs/adopt.md` (one line each: a script trial is refused until DFM-82 / DFM-83)
- Modify: `README.md` if it lists the trial command's flags

- [ ] **Step 1: Write the docs**

Record every builder decision from the spec's "Left to the builder" list in `docs/trial.md`: cache keyed by the URL hash with a sidecar, removed after a run; the profile text and the path rule (`"`, `\`, control characters refused; ancestors denied as literals; later rules win); the caps (`MAX_DIR_ENTRIES`, `MAX_OBSERVED`, 1 MiB script, 16 MiB hash cap); `SHELL`, `USER`, `LOGNAME`, `TMPDIR` sources; the one-hour timeout; the test skip rule.

- [ ] **Step 2: Run the fixtures and scripts steps**

Run: `bash scripts/check_all.sh fixtures scripts`
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add docs/trial.md docs/state.md docs/purge.md docs/adopt.md README.md
git commit -m "docs(trial): describe script trials, the sandbox and record schema 5 (DFM-81)"
```

---

### Task 10: Gate, review and PR

- [ ] **Step 1: Full gate**

Run: `bash scripts/check_all.sh`
Expected: exit 0, no `SKIPPED` line other than the documented sandbox skips (none on the M5).

- [ ] **Step 2: Local Codex review**

Run: `codex review --base main`
Fix every P1 and P2 finding, commit, rerun the affected tests.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feat/dfm-81-script-trial
gh pr create --title "feat(trial): install a tool through its script in a sandboxed trial (DFM-81)" --body "<summary, builder decisions, test evidence, Closes DFM-81 criteria C01 to C09; C10 and C11 wait for the gate and the merge>"
```

- [ ] **Step 4: Wait for the hosted `gate` and the Codex review; fix findings; merge on the thumbs-up**

Then record in Kaneo: C01 to C09 with the PR head SHA, C10 with the gate run URL, C11 with the merge commit, and move the task to done.
