# Development workflow

- Read `README.md` and `DESIGN.md` for the current scope and implementation status.
- Commit at completed, verified logical boundaries throughout execution, not only at the end of the project. Keep each commit focused and include its relevant tests and documentation.
- Review the staged diff and run checks relevant to the change before committing. `just check` runs the steps of `scripts/check_all.sh`: `fixtures`, `scripts`, `rust-style` and `rust-tests` (network install tests included). The fixture step does not execute resolver expectations. No step verifies a working manager on a native device.
- Install the hooks with `lefthook install`. The commit hook runs `fixtures`, `scripts` and `rust-style` on the staged tree. The push hook runs `rust-tests` on every pushed commit, so the tests always run locally before a push. Hosted CI runs the same script on every pull request and gates merge: the `gate` job must pass. Keep `scripts/check_all.sh` shared by the hooks, `just` and CI.
- Stage explicit paths and preserve unrelated work. Use Conventional Commit messages without generated-by or co-author attribution.
- Use Worktrunk (`wt`) when worktree isolation is useful. Preserve the existing configured sibling-directory naming; do not substitute manual worktree creation or temporary paths.
- Do not push, publish, or deploy live configuration without separate authorization.
