#!/usr/bin/env bash
set -euo pipefail

uv run scripts/check_examples.py
uvx --python 3.14 ruff@0.14.14 check scripts
uvx --python 3.14 ruff@0.14.14 format --check scripts
bash -n scripts/check_all.sh scripts/check_staged.sh examples/external/modules/work/files/session-hook.sh
uv run --python 3.14 scripts/test_check_staged.py
