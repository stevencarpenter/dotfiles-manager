"""Prove the commit gate reads the index, the push gate reads the pushed commit, and both preserve local changes."""

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


class PushGateTest(unittest.TestCase):
    def test_every_pushed_commit_not_working_tree(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="dotfiles-manager-push-test-"
        ) as folder:
            root = Path(folder)
            env = clean_env() | {
                "GIT_AUTHOR_NAME": "t",
                "GIT_AUTHOR_EMAIL": "t@example.com",
                "GIT_COMMITTER_NAME": "t",
                "GIT_COMMITTER_EMAIL": "t@example.com",
            }
            subprocess.run(["git", "init", "--quiet", folder], check=True, env=env)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(
                Path(__file__).with_name("check_push.sh"), scripts / "check_push.sh"
            )
            # The stub passes only for the committed content "valid"; a commit that carries
            # the marker `expect-clean` also requires the dotfile of an earlier commit gone.
            (scripts / "check_all.sh").write_text(
                'test "$(cat fixture)" = valid || exit 1\n'
                "[ ! -e expect-clean ] || [ ! -e .hidden ]\n"
            )
            fixture = root / "fixture"

            def commit(files: dict[str, str | None]) -> str:
                for name, text in files.items():
                    if text is None:
                        (root / name).unlink()
                    else:
                        (root / name).write_text(text + "\n")
                subprocess.run(["git", "add", "--all"], cwd=root, check=True, env=env)
                subprocess.run(
                    ["git", "commit", "--quiet", "-m", "step"],
                    cwd=root,
                    check=True,
                    env=env,
                )
                return subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], cwd=root, env=env, text=True
                ).strip()

            good = commit({"fixture": "valid"})
            bad = commit({"fixture": "invalid"})
            fixed = commit({"fixture": "valid"})
            commit({".hidden": "stale"})  # the commit below `clean`
            clean = commit({".hidden": None, "expect-clean": ""})
            zero = "0" * 40
            tracking = "refs/remotes/origin/main"
            for tip, remote, working, tested, succeeds in [
                # The pushed commit is tested, not the working tree.
                (good, None, "invalid", 1, True),
                (bad, None, "valid", 2, False),
                # Every commit the push introduces, oldest first, stopping at the first
                # broken one: `bad` sits below the tip `fixed`.
                (fixed, None, "valid", 2, False),
                # What a remote-tracking ref already has is not tested again.
                (fixed, bad, "valid", 1, True),
                # Each commit gets a new tree: the dotfile below `clean` must not reach it.
                (clean, fixed, "valid", 2, True),
                # A deletion-only push tests nothing.
                (zero, None, "invalid", 0, True),
            ]:
                with self.subTest(
                    tip=tip[:7], remote=(remote or "")[:7], working=working
                ):
                    if remote is None:
                        args = ["git", "update-ref", "-d", tracking]
                    else:
                        args = ["git", "update-ref", tracking, remote]
                    subprocess.run(args, cwd=root, check=True, env=env)
                    fixture.write_text(working + "\n")
                    result = subprocess.run(
                        ["bash", "scripts/check_push.sh"],
                        cwd=root,
                        capture_output=True,
                        env=env,
                        input=f"refs/heads/main {tip} refs/heads/main {zero}\n",
                        text=True,
                    )
                    self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                    self.assertEqual(result.stdout.count("pre-push: testing "), tested)
                    self.assertEqual(fixture.read_text(), working + "\n")


if __name__ == "__main__":
    unittest.main()
