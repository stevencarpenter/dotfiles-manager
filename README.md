# dotfiles-manager

`dfm` binds one macOS machine to one profile, resolves an authoritative shared core plus explicitly selected modules, and publishes the result through approved, journaled changes. Licensed under the [MIT License](LICENSE).

## Status

The Rust CLI builds, installs in isolation with `scripts/bootstrap.sh`, and runs as `dfm`. The project, the lock file `dotfiles-manager.lock` and the data, cache, config and state directories keep the name `dotfiles-manager`. First delivery targets two native Apple Silicon macOS machines: the personal M5 and the work M3.

| Command | Purpose | Notes |
| --- | --- | --- |
| `dfm bind`, `dfm binding` | Record (or show) the profile, the machine identity and the checkout root. Every command that reads sources uses that root when `--root` is not given. | [bootstrap](docs/bootstrap.md) |
| `dfm explain <resource-or-target>` | Read-only. Print ownership, permitted changes, provenance and native precedence for one resource or output path. | [resolver](docs/resolver.md) |
| `dfm plan` | Read-only. Validate claims, topology and ownership, and print the typed plan with its digest. `--against <saved plan>` exits 3 when any input changed. | [planner](docs/planner.md) |
| `dfm apply` | Publish file links and generated files (`git-config/v1`, `text-fragments/v1`, and the `claude-settings/v1` merge of `.claude/settings.json`). Remove outputs the manager owned before. | [apply](docs/apply.md), [adapters](docs/adapters.md) |
| `dfm doctor`, `dfm status` | Read-only. Report evidence-backed health of the binding, the state store, the resolved profile, the plan and each owned output. `--json` prints one document. | [doctor](docs/doctor.md) |
| `dfm secrets` | Render `op-template/v1` files through the 1Password CLI. A session gate runs before any write. No secret value or unsalted hash is printed or stored. | [secrets](docs/secrets.md) |
| `dfm packages` | Install missing `homebrew/v1`, `mise/v1` and `uv-tool/v1` packages. Nothing is upgraded, downgraded or removed. `apt/v1` is probe-only. | [packages](docs/packages.md) |
| `dfm packages --update` | Upgrade each installed mise or uv tool declared rolling to the newest version its provider resolves. Installed versions stay. A held tool or a local build is kept. | [toolchains](docs/toolchains.md) |
| `dfm services` | Start declared `launchd-agent/v1` and `brew-service/v1` services that the launcher does not hold. Restart only on `--restart` with `--allow-disruptive`. An unconfirmed request is `pending` (exit 5). | [lifecycle](docs/lifecycle.md) |
| `dfm trial <package>` | Try a tool without a module. Ask `homebrew/v1` and `mise/v1` which one can install it, record a baseline in `state/trials/`, then install it when no provider has it. | [trial](docs/trial.md) |
| `dfm sync` | The one daily command, and the only one that runs without a digest. Fetch the sources that `dotfiles-manager.lock` declares into staging and plan four scopes: configuration, packages, secrets and services. Print the plans, fast-forward the checkouts, then run the scopes in that order, each with its own receipt. A failed scope stops the later ones. Without a 1Password session the secrets are skipped, a blocked packages, secrets or services plan does not run, and an adoption or takeover without `--allow-adopt` is withheld; each is warned at the end of the output. A restart runs only on `--restart`. A new origin needs `--trust-origin` before any fetch. Then the updaters the profile declares run (brew, claude, codex, opencode, skills), unless `--no-upgrade` is given. | [sync](docs/sync.md), [upgrade](docs/upgrade.md) |
| `dfm state check`, `recover`, `resolve` | Check the state store and settle journal entries that ended unknown. | [state](docs/state.md) |
| `dfm self-update` | Rebuild and install the manager from the HEAD (or `--commit`) of a local checkout through that commit's `scripts/bootstrap.sh`. It never pulls. | [bootstrap](docs/bootstrap.md#self-update) |

Approval rules of `apply`, `secrets`, `packages`, `services` and `trial`:

- A run prints the plan and its digest. It changes nothing without `-y <digest>` (long form `--yes-plan`), and exits 3 (`E_APPROVAL_REQUIRED`).
- A plan that changes nothing needs no digest: `apply`, `secrets`, `packages` and `services` exit 0.
- A removal or a restart also needs `--allow-disruptive <operation-id>`. Adopting an existing file needs `--allow-adopt <operation-id>`.

Approval rules of `sync`, the daily command:

- A plain `dfm sync` needs no digest. It prints the plans and one sync digest for the sources and the four scope plans, then runs. Under the writer lock it plans again, and a plan that differs from the printed one refuses with `E_PLAN_STALE` before anything is published.
- `-y <sync digest>` is optional. It pins the run to the plan a `--preview` printed: a run whose plan has another digest refuses with `E_PLAN_STALE`.
- A removal of an output that the ledger proves the manager owns needs no flag.
- Three kinds of change never run on their own. A new origin needs `--trust-origin <origin>` before any fetch; that run grants it, prints the plan and stops (exit 3). An adoption or takeover of a file the manager does not own yet needs `--allow-adopt <operation-id>`: without it, that operation is withheld, the rest runs, and the warning at the end gives the flag. A restart of a running service needs `--restart <resource-id>`.

Rules for every command:

- `--preview` prints the plan and changes nothing.
- Exit codes: 0 ok, 1 failed, 2 usage, 3 blocked before mutation, 4 declined, 5 pending, 130 interrupted, 143 terminated.

`dfm --help` prints the full command reference.

### Required workflows

The owner requires four workflows for first delivery on both machines, plus full replacement of the current dotfiles setup. [Workflow acceptance requirements](DESIGN.md#first-implementation-workflow-acceptance) define the end-to-end behavior. Read-only resolution and the 16 resolver cases do not satisfy them.

| Workflow | Requirement | State at HEAD |
| --- | --- | --- |
| Update | One preview and one approval update every eligible managed version: external module repositories, package and input pins, runtimes, plugins and extensions, skills and native applications. Holds, failures and pending native updates are accounted for. | Not built as one command (`dfm update`). `dfm packages --update` covers rolling mise and uv tools. `dfm self-update` rebuilds the manager. |
| Sync | Pull the declared GitHub configuration and module revisions, validate them and apply them in one invocation. Local work is kept. Then the updaters the profile declares upgrade the tools (`--no-upgrade` skips them). | `dfm sync` |
| Try, adopt, purge | Test a tool without a module, keep a recoverable baseline, then promote its declarations or clean up only what the trial added. | `dfm trial` is built. `adopt` (DFM-33) and `purge` (DFM-34) are not built. |
| Doctor | A read-only table of tool and resource health: wanted, installed and selected versions, executable and config paths, ownership, drift and findings, with per-tool detail and `--json`. See the [doctor contract](DESIGN.md#doctor-read-only-machine-diagnostics). | `dfm doctor` reports the binding, state, plan and owned outputs. It runs no program and does not probe tools, versions or PATH. |

`claude-settings/v1` is the only merge adapter.

Native evidence on the personal M5 (receipts in `~/.local/state/dotfiles-manager/state/receipts/`):

- `apply` ended `ok` on 2026-10-16, 10-17, 10-18, 10-22 (three runs) and 10-23. The runs published `fs-link/v1` and `text-fragments/v1` outputs and ran the `claude-settings/v1` merge of `~/.claude/settings.json`. The ledger holds the co-owned record of that file.
- `secrets` ended `ok` on 2026-10-18 and rendered one `op-template/v1` output.
- No receipt on the M5 exists for `packages`, `services` or `trial`, and no receipt contains a `git-config/v1` operation. A run that changes nothing writes no receipt, so this does not prove that those commands never ran.

### Scope limits

The Intel i9 stays with the homelab repository and GNU Stow. The Linux, homelab and server examples are illustrative schema fixtures, not supported targets or migration promises. Future Linux and desktop Mac (Mac Studio, Mac Mini) use only constrains the design: no OS-bound core semantics, no mandatory GUI, and no hard-coded model or prefix paths. It adds no platform implementation, portability framework or delivery gate.

## Install and bind

The [first-run runbook](docs/first-run.md) is the shortest safe path to a first live `apply`. It starts with placing the pinned toolchain, which `install` requires. [Bootstrap and binding](docs/bootstrap.md) documents every check, upgrade, rollback and removal.

```sh
/bin/sh scripts/bootstrap.sh install --source "$PWD"   # builds the checkout's HEAD; --commit <sha> picks another
dfm bind --profile <profile.toml> --machine-id <id>     # root: the nearest directory above the profile that holds .git
dfm plan
dfm apply                                               # prints the plan and its digest
dfm apply -y <digest>
dfm self-update --source "$PWD"                         # later reinstalls; after the first run, plain dfm self-update
```

## Check the repository

Install the Git hooks once per clone with `lefthook install`. The checks need Git, Bash, Lefthook, uv, `shellcheck` and the Rust toolchain pinned in `rust-toolchain.toml`. `just` is optional.

```sh
just check            # the full gate (just check-all is the same)
just check-fixtures   # documentation fixtures only
just check-scripts    # Ruff, shell syntax, shellcheck, staged-gate regression test
just check-style      # cargo fmt --check, cargo clippy -D warnings
just check-tests      # cargo test, network install tests included
just check-network    # the network install tests alone
just check-commit     # what the commit hook runs, on the working tree
uv run scripts/check_examples.py   # the fixture checker without just
```

`scripts/check_all.sh` is the single gate for the hooks, `just` and CI. Its steps are `fixtures`, `scripts`, `rust-style`, `rust-tests` and `network-tests`; `rust-tests` runs `network-tests` itself.

- **Commit hook:** runs `fixtures`, `scripts` and `rust-style` on a temporary copy of the staged files. It does not run `cargo test` and does not modify or restage files.
- **Push hook:** runs `rust-tests` on each pushed tip that the remote does not have, in its own tree written with `git checkout-index`. Commits below a tip are not tested, because the tip supersedes them. A push of tips the remote already has, or a branch deletion, tests nothing.
- Both hooks build into `target/gate`, so a hook never waits for a build in `target`.

Environment variables of the gate:

- The gate runs `cargo test --locked --features dfm-cli/test-isolation`. The CLI and differential test targets need that feature; a plain `cargo test` skips them.
- `DFM_TEST_SELECT` replaces the default `--workspace --features dfm-cli/test-isolation` selection. CI uses it to run one shard per job.
- The install tests need crates.io, so they are `#[ignore]`d and `network-tests` runs them with `-- --ignored`. `DFM_NETWORK_TESTS` limits that step to named tests of `crates/cli/tests/bootstrap.rs`. `DFM_SKIP_NETWORK_TESTS=1` skips it and prints `SKIPPED: network install test`.
- The `scripts` step fails when `shellcheck` is missing. `DFM_SKIP_SHELLCHECK=1` skips it and prints `SKIPPED: shellcheck`.

### Fixture checker

`scripts/check_examples.py` uses Python 3.14 and the standard library. It checks TOML syntax, document shapes, local references, source containment, resource IDs, policy pointers and acceptance-fixture structure. It reports 15 manifests, 8 modules and 16 acceptance-case definitions, rejects nine malformed variants and accepts one profile without overrides. It does not run composition, evaluate the case expectations, contact package managers or vaults, or touch a live HOME. `cargo test -p dfm-cli --test resolver` executes the 16 cases against in-memory copies.

### Hosted CI

[Contract CI](.github/workflows/contract-check.yml) runs `scripts/check_all.sh` in stages and gates merge: branch protection on `main` requires its `gate` job. A pull request runs fixtures and scripts on Linux, and Rust style, four test shards and four install-test shards on macOS. The Rust jobs run only when a Rust input changed, and the install tests only when a bootstrap input changed. A new push to a pull request cancels its older run. A push to `main` runs every step. The `gate` job passes when every job passed or was skipped. No check runs a manager workflow against a live HOME or proves behavior on a native device.

## Documentation

- [Design contract](DESIGN.md): requirements, manifest semantics, lifecycle and migration boundaries.
- [Repo map](docs/repo-map-v7.html): a generated HTML page of the crates, the command flows, the recovery flows and what is built. Open the file in a browser.
- [Delivery roadmap](docs/roadmap.json) and [review record](docs/roadmap-review.md): M5 and M3 task contracts and evidence gates. Kaneo DFM is the authoritative backlog.
- [Checklist completion contract](docs/checklist-state.md): live Kaneo acceptance checkboxes are the sole completion state.
- [Inventory classification](INVENTORY.md) and [generated-dotfiles audit](docs/generated-dotfiles-audit.md): the existing dotfiles sorted into invariants, defaults, modules and unmanaged state.
- [Example profiles](examples/profiles/), the [public example bundle](examples/base/bundle.toml) (mandatory core and reusable modules), the [synthetic external bundle](examples/external/bundle.toml) (a module the public bundle does not reference) and the [resolver acceptance cases](examples/cases.toml).

All names, account references and external configuration in the examples are synthetic. The examples show the format. They are not a migration of the existing dotfiles and must not be deployed.
