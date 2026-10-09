"""Behavior tests for the manual release version command."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("release_version.py")


class ReleaseVersionTests(unittest.TestCase):
    def test_bump_type_selects_expected_version(self) -> None:
        cases = (
            ("patch", "0.0.4"),
            ("minor", "0.1.0"),
            ("major", "1.0.0"),
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(
                '[package]\nname = "reef"\nversion = "0.0.3"\n'
            )

            for bump, expected in cases:
                with self.subTest(bump=bump):
                    result = subprocess.run(
                        [sys.executable, SCRIPT, "next", bump, "--root", root],
                        capture_output=True,
                        text=True,
                        check=True,
                    )
                    self.assertEqual(result.stdout.strip(), expected)

    def test_apply_updates_only_reef_version_and_marker(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(
                '[package]\nname = "reef"\nversion = "0.0.3"\n\n[dependencies]\nexample = "0.0.3"\n'
            )
            (root / "Cargo.lock").write_text(
                '[[package]]\nname = "other"\nversion = "0.0.3"\n\n'
                '[[package]]\nname = "reef"\nversion = "0.0.3"\n'
            )

            subprocess.run(
                [sys.executable, SCRIPT, "apply", "0.0.4", "--root", root],
                check=True,
            )

            self.assertEqual((root / ".release-version").read_text(), "0.0.4\n")
            self.assertIn('version = "0.0.4"', (root / "Cargo.toml").read_text())
            self.assertIn('example = "0.0.3"', (root / "Cargo.toml").read_text())
            self.assertIn(
                'name = "other"\nversion = "0.0.3"',
                (root / "Cargo.lock").read_text(),
            )
            self.assertIn(
                'name = "reef"\nversion = "0.0.4"',
                (root / "Cargo.lock").read_text(),
            )

    def test_invalid_lockfile_preserves_source(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cargo = '[package]\nname = "reef"\nversion = "0.0.3"\n'
            (root / "Cargo.toml").write_text(cargo)
            (root / "Cargo.lock").write_text('[[package]]\nname = "other"\n')

            result = subprocess.run(
                [sys.executable, SCRIPT, "apply", "0.0.4", "--root", root],
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((root / "Cargo.toml").read_text(), cargo)
            self.assertFalse((root / ".release-version").exists())


if __name__ == "__main__":
    unittest.main()
