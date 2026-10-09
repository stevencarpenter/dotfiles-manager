# Validate design examples only. No live configuration operations.
check:
    uv run scripts/check_examples.py

# Full local validation, shared with the commit hook and hosted CI.
check-all:
    bash scripts/check_all.sh
