"""Exercise release orchestration without downloading or compiling OpenSSL."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("build-release-openssl.sh")
SHA256 = "9bffaa1ad1e07b354c21bd3324ec02fa15579f45a7d0494b3e74bc449b7333ef"
MOCK = r'''
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["MOCK_LOG"], "a") as log:
    log.write(json.dumps({"name": name, "args": args, "makeflags": os.getenv("MAKEFLAGS"), "cflags": os.getenv("CFLAGS"), "cc": os.getenv("CC")}) + "\n")
if name == "curl":
    pathlib.Path(args[args.index("--output") + 1]).write_bytes(b"isolated test archive")
elif name == "sha256sum":
    digest = "0" * 64 if os.getenv("MOCK_BAD_CHECKSUM") else os.environ["MOCK_SHA256"]
    print(digest + "  " + args[0])
elif name == "tar":
    source = pathlib.Path(args[args.index("-C") + 1]) / "openssl-3.6.4"
    source.mkdir()
    configure = source / "Configure"
    configure.write_text(pathlib.Path(__file__).read_text())
    configure.chmod(0o755)
elif name == "Configure":
    prefix = next(arg.split("=", 1)[1] for arg in args if arg.startswith("--prefix="))
    pathlib.Path(".mock-prefix").write_text(prefix)
elif name == "make":
    if os.getenv("MOCK_FAIL_MAKE"):
        sys.exit(1)
    if "install_dev" in args:
        prefix = pathlib.Path(".mock-prefix").read_text()
        lib = pathlib.Path(prefix) / "lib"
        lib.mkdir(parents=True)
        for archive in ["libssl.a", "libcrypto.a"]:
            (lib / archive).write_bytes(b"mock static library")
        headers = pathlib.Path(prefix) / "include" / "openssl"
        headers.mkdir(parents=True)
        (headers / "opensslv.h").write_text('#define OPENSSL_VERSION_STR "3.6.4"\n')
        if os.getenv("MOCK_SHARED_LIB"):
            (lib / "libcrypto.dylib").write_bytes(b"unexpected shared library")
'''


class BuildReleaseOpenSSLTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="openssl-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for command in ["curl", "sha256sum", "tar", "make"]:
            path = self.bin / command
            path.write_text("#!" + sys.executable + "\n" + MOCK)
            path.chmod(0o755)
        self.log = self.root / "commands.jsonl"
        self.env_file = self.root / "github-env"
        self.env = os.environ.copy()
        self.env.update({
            "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "TMPDIR": str(self.root),
            "GITHUB_ENV": str(self.env_file),
            "MOCK_LOG": str(self.log),
            "MOCK_SHA256": SHA256,
            "OPENSSL_BUILD_JOBS": "2",
        })
        self.bash = shutil.which("bash")
        self.assertIsNotNone(self.bash)

    def run_helper(self, target="x86_64-unknown-linux-gnu", prefix=None, extra_env=None):
        env = self.env | (extra_env or {})
        return subprocess.run(
            [self.bash, str(SCRIPT), target, str(prefix or self.root / "install")],
            env=env, text=True, capture_output=True, timeout=15,
        )

    def commands(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_supported_targets_install_static_and_emit_target_environment(self):
        for target, configure_target in [
            ("x86_64-unknown-linux-gnu", "linux-x86_64"),
            ("x86_64-apple-darwin", "darwin64-x86_64-cc"),
            ("aarch64-apple-darwin", "darwin64-arm64-cc"),
        ]:
            with self.subTest(target=target):
                self.log.unlink(missing_ok=True)
                self.env_file.unlink(missing_ok=True)
                prefix = self.root / target
                result = self.run_helper(target, prefix)
                self.assertEqual(result.returncode, 0, result.stderr)
                configure = next(c for c in self.commands() if c["name"] == "Configure")
                self.assertEqual(configure["args"], [
                    configure_target, "no-shared", "no-module", "no-tests",
                    "--prefix=" + str(prefix), "--openssldir=/etc/ssl", "--libdir=lib",
                ])
                values = dict(line.split("=", 1) for line in self.env_file.read_text().splitlines())
                self.assertEqual(values["OPENSSL_DIR"], str(prefix))
                self.assertEqual(values["OPENSSL_STATIC"], "1")
                self.assertEqual(values["OPENSSL_NO_VENDOR"], "1")
                target_env = target.upper().replace("-", "_")
                self.assertEqual(values[target_env + "_OPENSSL_DIR"], str(prefix))
                self.assertEqual(values[target_env + "_OPENSSL_STATIC"], "1")
                self.assertEqual(values[target_env + "_OPENSSL_NO_VENDOR"], "1")
                self.assertTrue((prefix / "lib/libcrypto.a").is_file())

    def test_checksum_mismatch_stops_before_unpack_or_build(self):
        result = self.run_helper(extra_env={"MOCK_BAD_CHECKSUM": "1"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHA-256 mismatch", result.stderr)
        self.assertEqual([c["name"] for c in self.commands()], ["curl", "sha256sum"])
        self.assertFalse(self.env_file.exists())
        self.assertFalse((self.root / "install").exists())

    def test_unsupported_target_is_rejected_before_download(self):
        result = self.run_helper(target="x86_64-unknown-linux-musl")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported target", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_unsafe_inputs_cannot_expand_shell_or_environment_commands(self):
        marker = self.root / "injected"
        attacks = [
            {"prefix": self.root / ("$(touch " + str(marker) + ")")},
            {"prefix": self.root / "install\nUNEXPECTED_ENV=1"},
            {"target": "x86_64-unknown-linux-gnu;touch " + str(marker)},
            {"extra_env": {"OPENSSL_BUILD_JOBS": "2;touch " + str(marker)}},
            {"extra_env": {"TMPDIR": str(self.root / "$(touch-injected)")}},
        ]
        for attack in attacks:
            with self.subTest(attack=attack):
                result = self.run_helper(**attack)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(marker.exists())
                self.assertEqual(self.commands(), [])
                self.assertFalse(self.env_file.exists())

    def test_compiler_and_make_flags_are_not_inherited(self):
        result = self.run_helper(extra_env={
            "MAKEFLAGS": "--eval=$(shell false)",
            "CFLAGS": "$(shell false)",
            "CC": "sh -c false",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        for command in self.commands():
            if command["name"] in ("Configure", "make"):
                self.assertIsNone(command["makeflags"])
                self.assertIsNone(command["cflags"])
                self.assertIsNone(command["cc"])

    def test_failed_build_does_not_publish_environment(self):
        result = self.run_helper(extra_env={"MOCK_FAIL_MAKE": "1"})
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.env_file.exists())

    def test_shared_library_artifacts_are_rejected(self):
        result = self.run_helper(extra_env={"MOCK_SHARED_LIB": "1"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected shared library", result.stderr)
        self.assertFalse(self.env_file.exists())

    def test_existing_prefix_is_rejected_before_download(self):
        prefix = self.root / "install"
        prefix.mkdir()
        result = self.run_helper(prefix=prefix)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("prefix already exists", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_dangling_prefix_symlink_is_rejected_before_download(self):
        prefix = self.root / "install"
        prefix.symlink_to(self.root / "unexpected-destination")
        result = self.run_helper(prefix=prefix)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("prefix already exists", result.stderr)
        self.assertEqual(self.commands(), [])
        self.assertFalse((self.root / "unexpected-destination").exists())


if __name__ == "__main__":
    unittest.main()
