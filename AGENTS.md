# Development workflow

- Read `README.md` and `DESIGN.md` for the current scope and implementation status.
- Commit at completed, verified logical boundaries throughout execution, not only at the end of the project. Keep each commit focused and include its relevant tests and documentation.
- Review the staged diff and run checks relevant to the change before committing. `just check` validates design fixtures; it does not execute resolver expectations or verify a working manager.
- Stage explicit paths and preserve unrelated work. Use Conventional Commit messages without generated-by or co-author attribution.
- Use Worktrunk (`wt`) when worktree isolation is useful. Preserve the existing configured sibling-directory naming; do not substitute manual worktree creation or temporary paths.
- Do not push, publish, or deploy live configuration without separate authorization.
