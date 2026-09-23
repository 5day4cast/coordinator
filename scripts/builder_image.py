"""Name the release builder image after the inputs that decide its contents (Python 3.11+).

The image holds the toolchain, static OpenSSL, and precompiled dependencies. A release
version bump changes only the workspace's own versions, so those are left out of the tag.
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tomllib


# Besides every Cargo.toml: files that change what the image installs or compiles.
INPUTS = (
    "Cargo.lock",
    ".cargo/config.toml",
    ".github/builder/Dockerfile",
    ".github/builder/Dockerfile.dockerignore",
    "scripts/build-release-openssl.sh",
    "scripts/release-cargo.sh",
)


def manifests(root):
    listed = subprocess.check_output(["git", "ls-files", "-z", "Cargo.toml", "*/Cargo.toml"], cwd=root)
    return sorted(name for name in listed.decode().split("\0") if name)


def normalized(root, name):
    text = (root / name).read_text()
    if name == "Cargo.lock":
        lock = tomllib.loads(text)
        for package in lock["package"]:
            if "source" not in package:
                package.pop("version")
        return json.dumps(lock, sort_keys=True)
    if name == "Cargo.toml":
        manifest = tomllib.loads(text)
        manifest["workspace"]["package"].pop("version", None)
        return json.dumps(manifest, sort_keys=True)
    return text


def tag(root):
    hasher = hashlib.sha256()
    for name in sorted({*INPUTS, *manifests(root)}):
        content = normalized(root, name).encode()
        hasher.update(f"{name}\0{len(content)}\0".encode())
        hasher.update(content)
    return "lock-" + hasher.hexdigest()[:32]


def wasm_bindgen_version(root):
    locked = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    return next(p["version"] for p in locked if p["name"] == "wasm-bindgen")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["tag", "wasm-bindgen-version"])
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    if args.command == "tag":
        print(tag(args.root))
    else:
        print(wasm_bindgen_version(args.root))


if __name__ == "__main__":
    main()
