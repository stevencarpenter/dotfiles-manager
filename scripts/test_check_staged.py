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

            def git(*args: str) -> None:
                subprocess.run(["git", *args], cwd=root, check=True, env=env)

            git("init", "--quiet", folder)
            # The hook bounds the range by the destination remote, so it must be configured.
            git("remote", "add", "origin", "git@example.com:origin.git")
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(
                Path(__file__).with_name("check_push.sh"), scripts / "check_push.sh"
            )
            # The stub passes only for the committed content "valid"; the marker
            # `expect-clean` also requires the dotfile of an earlier commit gone, and
            # the file `broken` (hidden from archives by export-ignore) must not exist.
            (scripts / "check_all.sh").write_text(
                'test "$(cat fixture)" = valid || exit 1\n'
                "[ ! -e expect-clean ] || [ ! -e .hidden ] || exit 1\n"
                "[ ! -e broken ]\n"
            )
            fixture = root / "fixture"

            def commit(files: dict[str, str | None]) -> str:
                for name, text in files.items():
                    if text is None:
                        (root / name).unlink()
                    else:
                        (root / name).write_text(text + "\n")
                git("add", "--all")
                git("commit", "--quiet", "-m", "step")
                return subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], cwd=root, env=env, text=True
                ).strip()

            good = commit({"fixture": "valid"})
            bad = commit({"fixture": "invalid"})
            fixed = commit({"fixture": "valid"})
            commit({".hidden": "stale"})  # the commit below `clean`
            clean = commit({".hidden": None, "expect-clean": ""})
            attrs = commit({"broken": "", ".gitattributes": "broken export-ignore"})
            zero = "0" * 40
            base = {
                "remote": "origin",
                "tracking": {},
                "over": zero,
                "working": "valid",
            }
            cases = [
                # The pushed commit is tested, not the working tree.
                {"tip": good, "working": "invalid", "tested": 1, "ok": True},
                {"tip": bad, "tested": 2, "ok": False},
                # Every commit the push introduces, oldest first, stopping at the first
                # broken one: `bad` sits below the tip `fixed`.
                {"tip": fixed, "tested": 2, "ok": False},
                # What the destination remote has is not tested again: its tracking ref...
                {"tip": fixed, "tracking": {"origin": bad}, "tested": 1, "ok": True},
                # ...or the remote sha git reports for the pushed ref.
                {"tip": fixed, "over": bad, "tested": 1, "ok": True},
                # Another remote's tracking ref does not bound a push to origin.
                {"tip": fixed, "tracking": {"backup": bad}, "tested": 2, "ok": False},
                # A push to a URL has no tracking refs; every remote bounds it then.
                {
                    "tip": fixed,
                    "remote": "git@example.com:other.git",
                    "tracking": {"backup": bad},
                    "tested": 1,
                    "ok": True,
                },
                # Each commit gets a new tree: the dotfile below `clean` must not reach it.
                {"tip": clean, "tracking": {"origin": fixed}, "tested": 2, "ok": True},
                # The commit's tree, not its archive: export-ignore hides nothing.
                {"tip": attrs, "tracking": {"origin": clean}, "tested": 1, "ok": False},
                # A deletion-only push tests nothing.
                {"tip": zero, "working": "invalid", "tested": 0, "ok": True},
            ]
            for case in cases:
                case = base | case
                tip, over, working = case["tip"], case["over"], case["working"]
                with self.subTest(
                    tip=tip[:7],
                    tracking={k: v[:7] for k, v in case["tracking"].items()},
                    over=over[:7],
                    remote=case["remote"],
                ):
                    for name in ("origin", "backup"):
                        git("update-ref", "-d", f"refs/remotes/{name}/main")
                    for name, sha in case["tracking"].items():
                        git("update-ref", f"refs/remotes/{name}/main", sha)
                    fixture.write_text(working + "\n")
                    result = subprocess.run(
                        ["bash", "scripts/check_push.sh", case["remote"]],
                        cwd=root,
                        capture_output=True,
                        env=env,
                        input=f"refs/heads/main {tip} refs/heads/main {over}\n",
                        text=True,
                    )
                    self.assertEqual(result.returncode == 0, case["ok"], result.stderr)
                    self.assertEqual(
                        result.stdout.count("pre-push: testing "), case["tested"]
                    )
                    self.assertEqual(fixture.read_text(), working + "\n")


if __name__ == "__main__":
    unittest.main()
