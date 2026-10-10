# Dotfiles manager: contract draft

A proposed manager for one profile per machine, an authoritative shared core, and explicitly selected modules.

**Status:** design and example fixtures, plus a Rust CLI that can be built, installed in isolation and bound to one profile ([bootstrap and binding](docs/bootstrap.md)). There is no configuration resolver, provider implementation, or deployment engine. `dotfiles-manager` is a working name. No license has been selected.

- [Reviewed delivery roadmap](docs/roadmap.json) and [review record](docs/roadmap-review.md): M5/M3 task contracts and evidence gates. Kaneo DFM is the authoritative backlog.
- [Checklist completion contract](docs/checklist-state.md): live Kaneo acceptance checkboxes are the sole completion state. Tasks and epics derive completion from their canonical requirements.
- [Design contract](DESIGN.md): agreed requirements, proposed manifest semantics, lifecycle, and migration boundaries.
- [Minimum implementation contract (proposed, owner review pending)](docs/architecture.md): language and toolchain, formats, CLI exit semantics, adapter boundary, trust and bootstrap.
- [Initial inventory classification](INVENTORY.md): actual source areas proposed as invariants, defaults, modules, or unmanaged state.
- [Example profiles](examples/profiles/): personal development, synthetic work, homelab (illustrative), and server (illustrative) compositions.
- [Public example bundle](examples/base/bundle.toml): the mandatory core and reusable modules.
- [Synthetic external bundle](examples/external/bundle.toml): adds a module without making the public bundle reference it.
- [Resolver acceptance fixtures](examples/cases.toml): expected results and errors for the future resolver. These expectations are not executed yet.

## First delivery scope

First delivery requires only the personal M5 and the work M3, both native Apple Silicon macOS. This supersedes the earlier three-machine requirement. All four workflows below and full replacement remain mandatory for these two machines.

The Intel i9 remains independently owned by the homelab repository and GNU Stow. The Linux and homelab examples are illustrative schema fixtures, not supported-target or migration promises. Future Linux and desktop Mac (Mac Studio, Mac Mini) use is limited to avoiding OS-bound core semantics, mandatory GUI assumptions, and hard-coded model or prefix paths. It adds no platform implementation, portability framework, or delivery gate.

## Required in the very first implementation

These workflows are user requirements, not features deferred until after a read-only implementation. The command names are proposals, not installed commands.

1. **`dotfiles-manager update`:** update every eligible managed version across all sources and selected modules, including external module repositories, package/input pins, runtimes, plugins/extensions, skills, and native applications. One combined preview and approval; explicit accounting for holds, failures, and pending native updates.
2. **`dotfiles-manager sync`:** pull the latest declared GitHub configuration and required module revisions, validate them, and apply them in one invocation. Preserve local work and do not independently upgrade unrelated floating versions.
3. **`dotfiles-manager try <tool>`**, then **`adopt <trial> --module <id>`** or **`purge <trial>`:** test a tool without authoring a module first, keep a recoverable baseline, and either promote its selected configuration/install declarations or confirm targeted cleanup. Preserve pre-existing tools, shared dependencies, credentials, and unrelated data.
4. **`dotfiles-manager doctor`:** a read-only diagnostic table showing tool/resource health, wanted/installed/selected versions, executable and config paths, source/module ownership, drift, and actionable findings. Include per-tool detail and `--json`; expose unknowns and failed checks without running repairs. [Doctor contract](DESIGN.md#doctor-read-only-machine-diagnostics).

Read-only resolution can be built first internally, but does not satisfy this first-implementation scope. [Workflow acceptance requirements](DESIGN.md#first-implementation-workflow-acceptance) cover the required end-to-end behavior; the existing 16 resolver definitions are not substitutes.

## Check the draft

Install Git hooks once per clone with `lefthook install`. Git, Bash, Lefthook and uv must be available on PATH.

The pre-commit hook runs the full local gate against a temporary copy of the staged files. It checks fixtures, pinned Ruff lint/format, shell syntax, the staged-gate regression test, `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`. It does not modify or restage files. Run `just check-all` to check the working tree manually.

Passing local validation is the gate for GitHub publication and merge. Hosted CI is informational; account billing and unavailable runners do not block delivery. Keep evidence for the exact source revision. Merge and publication still require authorization.

```sh
just check            # runs the four named checks below, in order
just check-fixtures   # documentation fixtures only
just check-scripts    # Ruff, shell syntax, staged-gate regression test
just check-style      # cargo fmt --check, cargo clippy -D warnings
just check-tests      # cargo test: implementation, CLI, bootstrap and build-controls tests
# Or, without just, the fixture checker alone:
uv run scripts/check_examples.py
```

`scripts/check_all.sh` is the single shared gate and accepts the step names `fixtures`, `scripts`, `rust-style` and `rust-tests`. `just check-tests` includes an install test that fetches crates from crates.io; set `DFM_SKIP_NETWORK_TESTS=1` to skip that one test (the skip is printed).

The fixture checker below is unchanged and still checks documentation fixtures only.

The checker uses Python 3.14 and the standard library. It checks TOML syntax, basic document shapes, local references, source containment, resource IDs, policy pointers, and acceptance-fixture structure. It also checks rejection of nine malformed example variants, including wrong document-kind references, and acceptance of a profile without optional overrides.

It does **not** implement composition, evaluate the acceptance expectations, contact package managers or vaults, or inspect/write a live HOME. Python is a documentation-check dependency, not a decision about the manager's implementation or target-machine requirements.

The checker reports 14 manifests, seven modules, 16 unexecuted acceptance-case definitions, nine malformed-variant rejections, and one accepted profile without overrides. Earlier draft verification rejected the original five malformed variants. Ruff lint/format, hook shell syntax, and authored-source Gitleaks scans passed. The draft 0.2 review HTML passed layout checks at 1280 and 390 CSS pixels in both themes; those checks predate the doctor table added in draft 0.3. These checks do not establish implementation of the required workflows.

[Contract CI](.github/workflows/contract-check.yml) mirrors the local gate on pushes and pull requests: fixtures, Ruff, hook and bootstrap script syntax, Rust style and Rust tests. Hosted Actions are disabled for this repository, so the local gate is the authority. The checks run no manager workflow (sync, update, try, adopt, purge) and no native-device proof.

All names, account references, and external configuration in the fixtures are synthetic. The examples demonstrate the proposed format; they are not a complete migration of the existing dotfiles inventory and must not be deployed.
