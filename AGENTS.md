# Development workflow

- Read `README.md` and `DESIGN.md` for the current scope and implementation status.
- Commit at completed, verified logical boundaries throughout execution, not only at the end of the project. Keep each commit focused and include its relevant tests and documentation.
- Review the staged diff and run checks relevant to the change before committing. `just check` runs separately named steps: design fixtures, script checks, `cargo fmt`/`clippy`, and `cargo test`. The fixture step does not execute resolver expectations. No step verifies a working manager on a native device.
- Install Lefthook with `lefthook install`. The commit hook runs the fast steps (fixtures, scripts, Rust style) on the staged tree. Hosted CI runs the tests on every pull request and gates merge: the `gate` job must pass. Run `just check` before publication when a change touches Rust. Keep `scripts/check_all.sh` shared by the hook and CI.
- Stage explicit paths and preserve unrelated work. Use Conventional Commit messages without generated-by or co-author attribution.
- Use Worktrunk (`wt`) when worktree isolation is useful. Preserve the existing configured sibling-directory naming; do not substitute manual worktree creation or temporary paths.
- Do not push, publish, or deploy live configuration without separate authorization.
