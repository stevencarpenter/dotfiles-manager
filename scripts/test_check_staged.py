"""Prove the commit gate reads the index and preserves local changes."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


def clean_env() -> dict[str, str]:
    """Environment without the repository-local variables git exports to hooks."""
    names = subprocess.check_output(
        ["git", "rev-parse", "--local-env-vars"], text=True, env=None
    ).split()
    return {k: v for k, v in os.environ.items() if k not in names}


def poisoned_env(decoy: Path) -> dict[str, str]:
    """Environment as a hook sets it, pointing at a repository that must stay untouched."""
    return clean_env() | {
        "GIT_DIR": str(decoy / ".git"),
        "GIT_INDEX_FILE": str(decoy / ".git" / "index"),
    }


class StagedGateTest(unittest.TestCase):
    def test_staged_contents(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="dotfiles-manager-gate-test-"
        ) as folder:
            root = Path(folder)
            env = clean_env()
            subprocess.run(["git", "init", "--quiet", folder], check=True, env=env)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(
                Path(__file__).with_name("check_staged.sh"), scripts / "check_staged.sh"
            )
            (scripts / "check_all.sh").write_text('test "$(cat fixture)" = valid\n')
            fixture = root / "fixture"
            # The stub ignores this file; only `git diff --cached --check` reads it.
            notes = root / "notes"
            for staged, working, succeeds, note in [
                ("valid", "valid", True, "clean"),
                ("invalid", "valid", False, "clean"),
                ("valid", "invalid", True, "clean"),
                ("valid ", "valid", False, "clean"),
                ("valid", "valid", False, "trailing "),
            ]:
                with self.subTest(staged=staged, working=working, note=note):
                    fixture.write_text(staged + "\n")
                    notes.write_text(note + "\n")
                    subprocess.run(["git", "add", "."], cwd=root, check=True, env=env)
                    fixture.write_text(working + "\n")
                    before = subprocess.check_output(
                        ["git", "write-tree"], cwd=root, env=env
                    )
                    result = subprocess.run(
                        ["bash", "scripts/check_staged.sh"],
                        cwd=root,
                        capture_output=True,
                        env=env,
                    )
                    self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                    if note != "clean":
                        # The stub ignores notes, so only the diff check can fail here.
                        output = result.stdout + result.stderr
                        self.assertIn(b"trailing whitespace", output)
                    self.assertEqual(fixture.read_text(), working + "\n")
                    self.assertEqual(
                        subprocess.check_output(
                            ["git", "write-tree"], cwd=root, env=env
                        ),
                        before,
                    )

    def test_inherited_git_env_is_isolated(self) -> None:
        with tempfile.TemporaryDirectory(prefix="dotfiles-manager-decoy-") as folder:
            decoy = Path(folder)
            env = clean_env()
            subprocess.run(["git", "init", "--quiet", folder], check=True, env=env)
            scripts = decoy / "scripts"
            scripts.mkdir()
            shutil.copyfile(
                Path(__file__).with_name("check_staged.sh"), scripts / "check_staged.sh"
            )
            # The default index holds a failing gate; only the alternate index passes.
            (scripts / "check_all.sh").write_text(
                'echo "validated the default index" >&2\nexit 1\n'
            )
            subprocess.run(["git", "add", "."], cwd=decoy, check=True, env=env)
            before = subprocess.check_output(["git", "write-tree"], cwd=decoy, env=env)
            # The alternate index (as a hook's GIT_INDEX_FILE) must be the one validated,
            # and check_all.sh must see no local git variables.
            alternate = decoy / ".git" / "alternate-index"
            shutil.copyfile(decoy / ".git" / "index", alternate)
            (scripts / "check_all.sh").write_text(
                "for v in $(git rev-parse --local-env-vars); do\n"
                '  [ -z "${!v+x}" ] || { echo "leaked $v" >&2; exit 1; }\n'
                "done\n"
            )
            subprocess.run(
                ["git", "add", "scripts/check_all.sh"],
                cwd=decoy,
                check=True,
                env=env | {"GIT_INDEX_FILE": str(alternate)},
            )
            poisoned = poisoned_env(decoy) | {"GIT_INDEX_FILE": str(alternate)}
            result = subprocess.run(
                ["bash", "scripts/check_staged.sh"],
                cwd=decoy,
                capture_output=True,
                env=poisoned,
            )
            self.assertEqual(result.returncode, 0, result.stderr)

            # The contents test must not touch the caller repository either.
            result = subprocess.run(
                [sys.executable, __file__, "StagedGateTest.test_staged_contents"],
                capture_output=True,
                env=poisoned,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                subprocess.check_output(["git", "write-tree"], cwd=decoy, env=env),
                before,
            )
            self.assertEqual(
                subprocess.check_output(
                    ["git", "config", "core.bare"], cwd=decoy, env=env, text=True
                ).strip(),
                "false",
            )


if __name__ == "__main__":
    unittest.main()
