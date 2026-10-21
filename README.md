# Dotfiles manager: contract draft

A proposed manager for one profile per machine, an authoritative shared core, and explicitly selected modules.

**Status:** design and example fixtures, plus a Rust CLI that can be built, installed in isolation and bound to one profile ([bootstrap and binding](docs/bootstrap.md)) and a durable [state store](docs/state.md) (ownership ledger, receipts, journal, trial records). A pure configuration resolver ([resolver notes](docs/resolver.md)) and a read-only `explain` command exist. A read-only `plan` command ([planner notes](docs/planner.md)) validates claims, topology and ownership and detects stale plans. An `apply` command ([apply notes](docs/apply.md)) publishes file links and removes previously owned outputs from an approved plan. It also publishes generated files from the `git-config/v1` render and `text-fragments/v1` assemble adapters ([adapter notes](docs/adapters.md)). The `claude-settings/v1` sensitive merge adapter plans and applies a co-owned `.claude/settings.json`; it is the only merge adapter, and no merge has run on a native device yet. Read-only `doctor` and `status` commands ([doctor notes](docs/doctor.md)) report evidence-backed health of the binding, state and owned files. They do not yet probe tools, versions or PATH. An existing link whose text already matches is adopted (recorded as owned, nothing written) with an explicit `--allow-adopt` flag. A `secrets` command ([secrets notes](docs/secrets.md)) renders `op-template/v1` secret files through the 1Password CLI on explicit request, with a session gate before any write and a salted ledger record; no secret value or unsalted hash is printed or stored. A `packages` command ([packages notes](docs/packages.md)) probes the `homebrew/v1`, `mise/v1` and `uv-tool/v1` providers' own listings and installs what is missing; nothing is upgraded, downgraded or removed, and `apt/v1` is probe-only. `packages --update` ([toolchains notes](docs/toolchains.md)) upgrades the installed mise and uv tools declared rolling to the newest version their provider resolves, keeps every installed version, and holds a held tool or a local build. A `trial` command ([trial notes](docs/trial.md)) tries a tool without a module: it asks those two providers which one owns or can install it, records a baseline of the tool, its owners and its configuration candidates before any change, installs it with `--yes-plan` and keeps the record in `state/trials/`; `adopt` and `purge` are not built. A `services` command ([lifecycle notes](docs/lifecycle.md)) asks launchd and `brew services` what they hold for each declared `launchd-agent/v1` and `brew-service/v1` service, starts what is not loaded, and restarts only with `--allow-disruptive`; a request the launcher accepted without observed completion is `pending`, never a success. A `sync` command ([sync notes](docs/sync.md)) fetches the sources that `dotfiles-manager.lock` declares into a staged copy of the checkout, plans the result, and with one approval fast-forwards the checkout (the root tracks its ref, nested sources stay at their exact pins, nothing is reset or stashed) and runs `apply` once; a new origin needs an explicit `--trust-origin` grant before any fetch. The shortest safe path to a first live run is in the [first-run runbook](docs/first-run.md). `dotfiles-manager` is a working name. Licensed under the [MIT License](LICENSE).

- [Reviewed delivery roadmap](docs/roadmap.json) and [review record](docs/roadmap-review.md): M5/M3 task contracts and evidence gates. Kaneo DFM is the authoritative backlog.
- [Checklist completion contract](docs/checklist-state.md): live Kaneo acceptance checkboxes are the sole completion state. Tasks and epics derive completion from their canonical requirements.
- [Design contract](DESIGN.md): agreed requirements, proposed manifest semantics, lifecycle, and migration boundaries.
- [Minimum implementation contract (proposed, owner review pending)](docs/architecture.md): language and toolchain, formats, CLI exit semantics, adapter boundary, trust and bootstrap.
- [Initial inventory classification](INVENTORY.md): actual source areas proposed as invariants, defaults, modules, or unmanaged state.
- [Example profiles](examples/profiles/): personal development, synthetic work, homelab (illustrative), and server (illustrative) compositions.
- [Public example bundle](examples/base/bundle.toml): the mandatory core and reusable modules.
- [Synthetic external bundle](examples/external/bundle.toml): adds a module without making the public bundle reference it.
- [Resolver acceptance fixtures](examples/cases.toml): expected results and errors. `cargo test -p dfm-cli --test resolver` executes all 16 against in-memory copies. `scripts/check_examples.py` still checks only their shape.

## First delivery scope

First delivery requires only the personal M5 and the work M3, both native Apple Silicon macOS. This supersedes the earlier three-machine requirement. All four workflows below and full replacement remain mandatory for these two machines.

The Intel i9 remains independently owned by the homelab repository and GNU Stow. The Linux and homelab examples are illustrative schema fixtures, not supported-target or migration promises. Future Linux and desktop Mac (Mac Studio, Mac Mini) use is limited to avoiding OS-bound core semantics, mandatory GUI assumptions, and hard-coded model or prefix paths. It adds no platform implementation, portability framework, or delivery gate.

## Required in the very first implementation

These workflows are user requirements, not features deferred until after a read-only implementation. The command names are proposals, not installed commands.

1. **`dfm update`:** update every eligible managed version across all sources and selected modules, including external module repositories, package/input pins, runtimes, plugins/extensions, skills, and native applications. One combined preview and approval; explicit accounting for holds, failures, and pending native updates.
2. **`dfm sync`:** pull the latest declared GitHub configuration and required module revisions, validate them, and apply them in one invocation. Preserve local work and do not independently upgrade unrelated floating versions.
3. **`dfm try <tool>`**, then **`adopt <trial> --module <id>`** or **`purge <trial>`:** test a tool without authoring a module first, keep a recoverable baseline, and either promote its selected configuration/install declarations or confirm targeted cleanup. Preserve pre-existing tools, shared dependencies, credentials, and unrelated data.
4. **`dfm doctor`:** a read-only diagnostic table showing tool/resource health, wanted/installed/selected versions, executable and config paths, source/module ownership, drift, and actionable findings. Include per-tool detail and `--json`; expose unknowns and failed checks without running repairs. [Doctor contract](DESIGN.md#doctor-read-only-machine-diagnostics).

Read-only resolution can be built first internally, but does not satisfy this first-implementation scope. [Workflow acceptance requirements](DESIGN.md#first-implementation-workflow-acceptance) cover the required end-to-end behavior; the existing 16 resolver definitions are not substitutes.

## Check the draft

Install Git hooks once per clone with `lefthook install`. Git, Bash, Lefthook and uv must be available on PATH.

The pre-commit hook runs the fast steps of the local gate against a temporary copy of the staged files: fixtures, pinned Ruff lint/format, shell syntax, the staged-gate regression test, `cargo fmt --check` and `cargo clippy -D warnings`. It does not run `cargo test`, and it does not modify or restage files. The copy builds into `target/gate`, so a hook never waits for a build in `target`. The pre-push hook runs the tests (`cargo test`, the network install tests included) on every commit the push introduces: each commit reachable from a pushed tip and not yet on the destination remote (its tracking refs and the remote sha git reports), oldest first, written out with `git checkout-index` into its own temporary tree with the same `target/gate` build directory. A push that only deletes a branch, or whose commits are all on the remote already, tests nothing. Run `just check` for the full gate on the working tree.

Hosted CI is the merge gate since 2026-10-20: branch protection on `main` requires the `gate` job of [Contract CI](.github/workflows/contract-check.yml). The tests run locally before any push (the pre-push hook, owner rule of 2026-10-20); keep evidence for the exact source revision. Merge and publication still require authorization.

```sh
just check            # the full gate; just check-all is the same
just check-fixtures   # documentation fixtures only
just check-scripts    # Ruff, shell syntax, staged-gate regression test
just check-style      # cargo fmt --check, cargo clippy -D warnings
just check-tests      # cargo test: implementation, CLI, bootstrap and build-controls tests
just check-network    # the network install tests alone
just check-commit     # what the commit hook runs, on the working tree
# Or, without just, the fixture checker alone:
uv run scripts/check_examples.py
```

`scripts/check_all.sh` is the single shared gate and accepts the step names `fixtures`, `scripts`, `rust-style`, `rust-tests` and `network-tests`. The CLI and differential tests need the isolated build, so the gate runs `cargo test --locked --features dfm-cli/test-isolation`; a plain `cargo test` skips those test targets. `DFM_TEST_SELECT` replaces the default `--workspace --features dfm-cli/test-isolation` selection with cargo package, target and feature flags; CI uses it to run one shard of test binaries per job. The install tests need crates.io, so they are `#[ignore]`d and the `network-tests` step runs them explicitly with `-- --ignored`; `rust-tests` runs that step itself. `DFM_NETWORK_TESTS` limits that step to the named tests of `crates/cli/tests/bootstrap.rs`; CI uses it because each of those tests builds the manager in release mode. Set `DFM_SKIP_NETWORK_TESTS=1` to skip it; the gate then prints `SKIPPED: network install test`. The `scripts` step fails when `shellcheck` is missing. Set `DFM_SKIP_SHELLCHECK=1` to skip it; the gate then prints `SKIPPED: shellcheck`.

The fixture checker below is unchanged and still checks documentation fixtures only.

The checker uses Python 3.14 and the standard library. It checks TOML syntax, basic document shapes, local references, source containment, resource IDs, policy pointers, and acceptance-fixture structure. It also checks rejection of nine malformed example variants, including wrong document-kind references, and acceptance of a profile without optional overrides.

It does **not** implement composition, evaluate the acceptance expectations, contact package managers or vaults, or inspect/write a live HOME. Python is a documentation-check dependency, not a decision about the manager's implementation or target-machine requirements.

The checker reports 15 manifests, 8 modules, 16 unexecuted acceptance-case definitions, nine malformed-variant rejections, and one accepted profile without overrides. Earlier draft verification rejected the original five malformed variants. Ruff lint/format, hook shell syntax, and authored-source Gitleaks scans passed. The draft 0.2 review HTML passed layout checks at 1280 and 390 CSS pixels in both themes; those checks predate the doctor table added in draft 0.3. These checks do not establish implementation of the required workflows.

[Contract CI](.github/workflows/contract-check.yml) runs the same script in stages. A pull request runs fixtures and scripts on Linux, and Rust style, four test shards and four install-test shards on macOS in parallel; the Rust jobs run only when a Rust input changed, the install tests only when a bootstrap input changed, and a new push cancels the older run. A push to `main` runs every step. Every macOS job restores the cargo dependency cache and the first to finish saves it. The `gate` job passes when every job passed or was skipped, and branch protection requires it. The checks run no manager workflow (sync, update, try, adopt, purge) and no native-device proof.

All names, account references, and external configuration in the fixtures are synthetic. The examples demonstrate the proposed format; they are not a complete migration of the existing dotfiles inventory and must not be deployed.
