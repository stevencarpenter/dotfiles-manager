"""Prove the commit gate reads the index and preserves local changes."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class StagedGateTest(unittest.TestCase):
    def test_staged_contents(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="dotfiles-manager-gate-test-"
        ) as folder:
            root = Path(folder)
            subprocess.run(["git", "init", "--quiet", folder], check=True)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(
                Path(__file__).with_name("check_staged.sh"), scripts / "check_staged.sh"
            )
            (scripts / "check_all.sh").write_text('test "$(cat fixture)" = valid\n')
            fixture = root / "fixture"
            for staged, working, succeeds in [
                ("valid", "valid", True),
                ("invalid", "valid", False),
                ("valid", "invalid", True),
                ("valid ", "valid", False),
            ]:
                with self.subTest(staged=staged, working=working):
                    fixture.write_text(staged + "\n")
                    subprocess.run(["git", "add", "."], cwd=root, check=True)
                    fixture.write_text(working + "\n")
                    before = subprocess.check_output(["git", "write-tree"], cwd=root)
                    result = subprocess.run(
                        ["bash", "scripts/check_staged.sh"],
                        cwd=root,
                        capture_output=True,
                    )
                    self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                    self.assertEqual(fixture.read_text(), working + "\n")
                    self.assertEqual(
                        subprocess.check_output(["git", "write-tree"], cwd=root), before
                    )


if __name__ == "__main__":
    unittest.main()
