"""Release regressions: version/source mismatch and incomplete browser packages."""

import hashlib
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import release


VERSION = "2.0.0"
SOURCE = "a" * 40
TARGET = "x86_64-unknown-linux-gnu"


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.put("Cargo.toml", '[workspace]\nmembers = ["crates/coordinator"]\n[workspace.package]\nversion = "2.0.0"\n')
        self.put("crates/coordinator/Cargo.toml", '[package]\nname = "coordinator"\nversion.workspace = true\n')
        self.put("Cargo.lock", '[[package]]\nname = "coordinator"\nversion = "2.0.0"\n')
        self.put("flake.lock", "{}")
        self.put("crates/coordinator-wasm/keymeld-trusted-pcrs.json", "{}")
        for chart in ("coordinator", "synth"):
            self.put(f"deploy/helm/{chart}/Chart.yaml", 'version: 2.0.0\nappVersion: "2.0.0"\n')
        self.put("docs/releases/v2.0.0.md", "Release migration instructions.")

    def put(self, name, text):
        destination = self.root / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text)
        return destination

    def wasm(self, source=SOURCE):
        self.put("wasm/coordinator_wasm.js", "export default function init() {}")
        self.put("wasm/coordinator_wasm_bg.wasm", "wasm fixture")
        return release.package_wasm(self.root, VERSION, source, self.root / "wasm", self.root / "wasm-release")

    def native(self, wasm):
        return release.package_native(
            self.root, VERSION, SOURCE, TARGET, "coordinator-x86_64-linux", wasm, self.root / "release"
        )

    def native_files(self):
        for binary in ("coordinator", "wallet-cli"):
            self.put(f"target/{TARGET}/release/{binary}", "binary fixture").chmod(0o755)
        self.put("crates/coordinator/migrations/competitions/001.sql", "SELECT 1;")

    def test_committed_versions_must_all_match(self):
        release.validate_version(self.root, VERSION)
        for filename, old, new in (
            ("Cargo.toml", VERSION, "0.8.2"),
            ("Cargo.lock", VERSION, "0.8.2"),
            ("deploy/helm/coordinator/Chart.yaml", 'appVersion: "2.0.0"', 'appVersion: "0.1.0"'),
            ("deploy/helm/synth/Chart.yaml", "version: 2.0.0", "version: 0.1.0"),
            ("crates/coordinator/Cargo.toml", "version.workspace = true", 'version = "0.8.2"'),
        ):
            with self.subTest(filename=filename):
                path = self.root / filename
                original = path.read_text()
                path.write_text(original.replace(old, new))
                with self.assertRaises(ValueError):
                    release.validate_version(self.root, VERSION)
                path.write_text(original)

    def test_invalid_input_cannot_become_an_output_or_shell_fragment(self):
        for version in ("v2.0.0", "02.0.0", "2.0", "2.0.0-rc1", "2.0.0\nsource=bad", "$(touch sentinel)"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.validate_version(self.root, version)

    def test_release_notes_are_required(self):
        (self.root / "docs/releases/v2.0.0.md").unlink()
        with self.assertRaisesRegex(ValueError, "missing or empty release asset"):
            release.validate_version(self.root, VERSION)

    def test_missing_attestation_provenance_fails_packaging(self):
        (self.root / "crates/coordinator-wasm/keymeld-trusted-pcrs.json").unlink()
        with self.assertRaises(FileNotFoundError):
            self.wasm()

    def test_dry_run_allows_an_untagged_source(self):
        with patch("release.subprocess.check_output", return_value=SOURCE + "\n") as git:
            self.assertEqual(release.validate_source(self.root, VERSION, "workflow_dispatch", "refs/heads/release", True), SOURCE)
            self.assertEqual(git.call_count, 1)

    def test_public_dispatch_requires_tag_at_exact_build_commit(self):
        with patch("release.subprocess.check_output", side_effect=[SOURCE, "b" * 40]):
            with self.assertRaisesRegex(ValueError, "existing version tag"):
                release.validate_source(self.root, VERSION, "workflow_dispatch", "refs/heads/release", False)
        with patch("release.subprocess.check_output", side_effect=[SOURCE, SOURCE]):
            self.assertEqual(release.validate_source(self.root, VERSION, "push", "refs/tags/v2.0.0", False), SOURCE)

    def test_tag_name_must_match_requested_version(self):
        with patch("release.subprocess.check_output", return_value=SOURCE):
            with self.assertRaisesRegex(ValueError, "tag does not match"):
                release.validate_source(self.root, VERSION, "push", "refs/tags/v1.20.0", False)

    def test_native_archive_contains_browser_assets_wallet_and_provenance(self):
        self.native_files()
        wasm = self.wasm()
        # The public sidecar must keep the documented filename, module path, and hash.
        module_hash = hashlib.sha256((self.root / "wasm/coordinator_wasm_bg.wasm").read_bytes()).hexdigest()
        archive_hash = hashlib.sha256(wasm.read_bytes()).hexdigest()
        self.assertEqual(
            (wasm.parent / f"coordinator-wasm-{VERSION}.sha256").read_text(),
            f"{module_hash}  coordinator-wasm-{VERSION}/coordinator_wasm_bg.wasm\n"
            f"{archive_hash}  coordinator-wasm-{VERSION}.tar.gz\n",
        )
        destination = self.native(wasm)
        first_hash = release.digest(destination)
        with tarfile.open(destination) as archive:
            prefix = "coordinator-x86_64-linux-2.0.0/"
            for name in ("bin/coordinator", "bin/wallet-cli", "ui/pkg/coordinator_wasm_bg.wasm", "ui/pkg/coordinator_wasm.js", "migrations/competitions/001.sql", "SHA256SUMS"):
                self.assertIn(prefix + name, archive.getnames())
            self.assertEqual(archive.getmember(prefix + "bin/coordinator").mode, 0o755)
            manifest = json.load(archive.extractfile(prefix + "RELEASE.json"))
            self.assertEqual(manifest["source_commit"], SOURCE)
            self.assertEqual(manifest["wasm_sha256"], release.digest(self.root / "wasm/coordinator_wasm_bg.wasm"))
            checksums = archive.extractfile(prefix + "SHA256SUMS").read().decode()
            self.assertIn("  ui/pkg/coordinator_wasm_bg.wasm\n", checksums)
        # Repackaging the same content produces identical bytes and an archive checksum.
        self.assertEqual(release.digest(self.native(wasm)), first_hash)
        self.assertEqual(destination.with_suffix(".gz.sha256").read_text(), f"{first_hash}  {destination.name}\n")

    def test_mixed_source_wasm_is_rejected(self):
        self.native_files()
        with self.assertRaisesRegex(ValueError, "different release source"):
            self.native(self.wasm(source="b" * 40))

    def test_ui_directory_holds_only_the_wasm_package(self):
        # Scripts and styles are embedded in the binary; nothing else ships beside it.
        self.native_files()
        destination = self.native(self.wasm())
        with tarfile.open(destination) as archive:
            prefix = "coordinator-x86_64-linux-2.0.0/ui/"
            names = [name[len(prefix):] for name in archive.getnames() if name.startswith(prefix)]
        self.assertTrue(names)
        self.assertTrue(all(name.startswith("pkg") for name in names), names)

    def test_swap_archive_contains_the_service_and_provenance(self):
        self.put(f"target/{TARGET}/release/ark-swapd", "binary fixture").chmod(0o755)
        destination = release.package_swap(self.root, VERSION, SOURCE, TARGET, self.root / "release")
        with tarfile.open(destination) as archive:
            prefix = f"ark-swapd-{VERSION}-{TARGET}/"
            for name in ("bin/ark-swapd", "RELEASE.json", "README.txt", "SHA256SUMS"):
                self.assertIn(prefix + name, archive.getnames())
            self.assertEqual(archive.getmember(prefix + "bin/ark-swapd").mode, 0o755)
            self.assertEqual(json.load(archive.extractfile(prefix + "RELEASE.json"))["source_commit"], SOURCE)

    def test_swap_binary_is_required(self):
        with self.assertRaisesRegex(ValueError, "missing or empty release asset"):
            release.package_swap(self.root, VERSION, SOURCE, TARGET, self.root / "release")

    def test_synth_archive_contains_the_service_and_provenance(self):
        self.put(f"target/{TARGET}/release/synth", "binary fixture").chmod(0o755)
        destination = release.package_synth(self.root, VERSION, SOURCE, TARGET, self.root / "release")
        with tarfile.open(destination) as archive:
            prefix = f"synth-{VERSION}-{TARGET}/"
            for name in ("bin/synth", "RELEASE.json", "README.txt", "SHA256SUMS"):
                self.assertIn(prefix + name, archive.getnames())
            self.assertEqual(archive.getmember(prefix + "bin/synth").mode, 0o755)

    def test_synth_binary_is_required(self):
        with self.assertRaisesRegex(ValueError, "missing or empty release asset"):
            release.package_synth(self.root, VERSION, SOURCE, TARGET, self.root / "release")

    def test_wasm_binary_is_required(self):
        self.put("wasm/coordinator_wasm.js", "binding fixture")
        with self.assertRaisesRegex(ValueError, "missing or empty release asset"):
            release.package_wasm(self.root, VERSION, SOURCE, self.root / "wasm", self.root / "release")


if __name__ == "__main__":
    unittest.main()
