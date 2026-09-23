"""Builder image tag regressions: a release version bump reuses the image, a dependency change does not."""

from pathlib import Path
import subprocess
import tempfile
import unittest

import builder_image


class BuilderImageTagTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.release("2.0.0", serde="1.0.200")
        for name in builder_image.INPUTS:
            if not (self.root / name).exists():
                self.put(name, f"{name} fixture\n")
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(["git", "add", "."], cwd=self.root, check=True)

    def put(self, name, text):
        destination = self.root / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text)

    def release(self, version, serde):
        self.put("Cargo.toml", f'[workspace]\nmembers = ["crates/coordinator"]\n[workspace.package]\nversion = "{version}"\n')
        self.put("crates/coordinator/Cargo.toml", '[package]\nname = "coordinator"\nversion.workspace = true\n')
        self.put(
            "Cargo.lock",
            f'[[package]]\nname = "coordinator"\nversion = "{version}"\n\n'
            f'[[package]]\nname = "serde"\nversion = "{serde}"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n',
        )

    def test_a_release_version_bump_keeps_the_tag(self):
        before = builder_image.tag(self.root)
        self.release("2.1.0", serde="1.0.200")
        self.assertEqual(builder_image.tag(self.root), before)

    def test_a_dependency_change_moves_the_tag(self):
        before = builder_image.tag(self.root)
        self.release("2.0.0", serde="1.0.201")
        self.assertNotEqual(builder_image.tag(self.root), before)

    def test_a_manifest_change_moves_the_tag(self):
        before = builder_image.tag(self.root)
        self.put("crates/coordinator/Cargo.toml", '[package]\nname = "coordinator"\nversion.workspace = true\n[features]\nx = []\n')
        self.assertNotEqual(builder_image.tag(self.root), before)

    def test_an_image_input_change_moves_the_tag(self):
        before = builder_image.tag(self.root)
        self.put(".github/builder/Dockerfile", "FROM changed\n")
        self.assertNotEqual(builder_image.tag(self.root), before)


if __name__ == "__main__":
    unittest.main()
