"""Validate release metadata and assemble complete archives (Python 3.11+)."""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


VERSION_PATTERN = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def validate_version(root, version):
    if not VERSION_PATTERN.fullmatch(version):
        raise ValueError("release version must be X.Y.Z without a v prefix or leading zeros")
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    if workspace["package"]["version"] != version:
        raise ValueError("release version does not match committed workspace version")
    locked = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    for member in workspace["members"]:
        for directory in root.glob(member):
            package = tomllib.loads((directory / "Cargo.toml").read_text())["package"]
            declared = package["version"]
            if declared != {"workspace": True} and declared != version:
                raise ValueError(f"{package['name']} does not use the release version")
            versions = [p["version"] for p in locked if p["name"] == package["name"] and "source" not in p]
            if versions != [version]:
                raise ValueError(f"Cargo.lock version mismatch for {package['name']}")
    for name in ("coordinator", "synth"):
        chart = root / f"deploy/helm/{name}/Chart.yaml"
        # These chart metadata fields are top-level YAML scalars.
        for field in ("version", "appVersion"):
            match = re.search(rf'^{field}:\s*[\"\']?([^\s\"\']+)', chart.read_text(), re.MULTILINE)
            if not match or match[1] != version:
                raise ValueError(f"{chart} {field} does not match the release")
    require_files(root, [f"docs/releases/v{version}.md"])


def validate_source(root, version, event, ref, dry_run):
    validate_version(root, version)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    tag = f"refs/tags/v{version}"
    if event != "workflow_dispatch" and ref != tag:
        raise ValueError("release tag does not match the requested version")
    if not dry_run:
        tagged = subprocess.check_output(
            ["git", "rev-parse", "--verify", f"{tag}^{{commit}}"], cwd=root, text=True
        ).strip()
        if tagged != source:
            raise ValueError("public release requires an existing version tag at the selected commit")
    return source


def metadata(root, version, source):
    return {
        "version": version,
        "source_commit": source,
        "source_sha256": {
            name: digest(root / name)
            for name in ("Cargo.lock", "flake.lock", "crates/coordinator-wasm/keymeld-trusted-pcrs.json")
        },
    }


def require_files(directory, names):
    for name in names:
        if not (directory / name).is_file() or not (directory / name).stat().st_size:
            raise ValueError(f"missing or empty release asset: {directory / name}")


def write_checksums(directory):
    paths = sorted(p for p in directory.rglob("*") if p.is_file() and p.name != "SHA256SUMS")
    (directory / "SHA256SUMS").write_text("".join(f"{digest(p)}  {p.relative_to(directory)}\n" for p in paths))


def archive(directory, output):
    write_checksums(directory)
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"{directory.name}.tar.gz"
    # Normalize tar/gzip metadata so identical files produce identical archives.
    with destination.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w") as tar:
            for path in [directory, *sorted(directory.rglob("*"))]:
                info = tar.gettarinfo(str(path), arcname=str(path.relative_to(directory.parent)))
                info.uid = info.gid = info.mtime = 0
                info.uname = info.gname = ""
                if path.is_file():
                    with path.open("rb") as file:
                        tar.addfile(info, file)
                else:
                    tar.addfile(info)
    destination.with_suffix(".gz.sha256").write_text(f"{digest(destination)}  {destination.name}\n")
    return destination


def package_wasm(root, version, source, wasm, output):
    require_files(wasm, ["coordinator_wasm.js", "coordinator_wasm_bg.wasm"])
    with tempfile.TemporaryDirectory() as temporary:
        package = Path(temporary) / f"coordinator-wasm-{version}"
        shutil.copytree(wasm, package)
        provenance = metadata(root, version, source)
        provenance["wasm_sha256"] = digest(package / "coordinator_wasm_bg.wasm")
        (package / "RELEASE.json").write_text(json.dumps(provenance, indent=2) + "\n")
        destination = archive(package, output)
        # Preserve the public sidecar used to verify the module served by a coordinator.
        (output / f"{package.name}.sha256").write_text(
            f"{provenance['wasm_sha256']}  {package.name}/coordinator_wasm_bg.wasm\n"
            f"{digest(destination)}  {destination.name}\n"
        )
        return destination


def package_native(root, version, source, target, asset, wasm_archive, output):
    binaries = root / "target" / target / "release"
    require_files(binaries, ["coordinator", "wallet-cli"])
    ui = root / "crates/public_ui"
    require_files(ui, ["asset-manifest.json", "loader.js", "app.min.js", "admin.min.js", "styles.min.css"])
    manifest = json.loads((ui / "asset-manifest.json").read_text())
    for name, extension in (("app", "js"), ("admin", "js"), ("styles", "css")):
        hash_value = manifest.get(name, "")
        if not re.fullmatch(r"[0-9a-f]{8}", hash_value):
            raise ValueError(f"missing or invalid {name} UI bundle hash")
        require_files(ui, [f"{name}.{hash_value}.min.{extension}"])
    with tempfile.TemporaryDirectory() as temporary:
        package = Path(temporary) / f"{asset}-{version}"
        (package / "bin").mkdir(parents=True)
        for binary in ("coordinator", "wallet-cli"):
            shutil.copy2(binaries / binary, package / "bin" / binary)
        shutil.copytree(root / "crates/coordinator/migrations", package / "migrations")
        shutil.copytree(ui, package / "ui", ignore=shutil.ignore_patterns("pkg"))
        with tarfile.open(wasm_archive) as tar:
            tar.extractall(Path(temporary) / "wasm", filter="data")
        wasm = Path(temporary) / "wasm" / f"coordinator-wasm-{version}"
        require_files(wasm, ["RELEASE.json", "coordinator_wasm.js", "coordinator_wasm_bg.wasm"])
        wasm_provenance = json.loads((wasm / "RELEASE.json").read_text())
        provenance = metadata(root, version, source)
        if any(wasm_provenance.get(key) != value for key, value in provenance.items()):
            raise ValueError("WASM artifact was built from a different release source")
        wasm_hash = digest(wasm / "coordinator_wasm_bg.wasm")
        if wasm_provenance.get("wasm_sha256") != wasm_hash:
            raise ValueError("WASM artifact hash does not match its release manifest")
        shutil.copytree(wasm, package / "ui/pkg")
        provenance.update(target=target, wasm_sha256=wasm_hash)
        (package / "RELEASE.json").write_text(json.dumps(provenance, indent=2) + "\n")
        (package / "README.txt").write_text(
            f"Coordinator v{version}\nSource: {source}\n\n"
            "Run ./bin/coordinator --help for configuration options.\n"
            "Set [ui_settings].ui_dir to the absolute path of this archive's ui/ directory.\n"
            "The UI and browser WASM are included. Migrations run automatically on startup.\n"
            "Use sha256sum -c SHA256SUMS (or shasum -a 256 -c SHA256SUMS on macOS) to verify files.\n"
            "Read the release migration guide before upgrading an existing database:\n"
            "https://github.com/5day4cast/coordinator/releases/tag/v" + version + "\n"
        )
        return archive(package, output)


def package_enclave(root, version, source, target, output):
    """Package the coordinator verifier enclave and its LNURL relay for a Keymeld host."""
    binaries = root / "target" / target / "release"
    names = ("coordinator-verifier-enclave", "coordinator-lnurl-relay")
    require_files(binaries, names)
    with tempfile.TemporaryDirectory() as temporary:
        package = Path(temporary) / f"coordinator-verifier-enclave-{version}-{target}"
        (package / "bin").mkdir(parents=True)
        for binary in names:
            shutil.copy2(binaries / binary, package / "bin" / binary)
        provenance = metadata(root, version, source)
        provenance.update(target=target, features=["lnurl"])
        (package / "RELEASE.json").write_text(json.dumps(provenance, indent=2) + "\n")
        (package / "README.txt").write_text(
            f"Coordinator verifier enclave v{version}\nSource: {source}\n\n"
            "bin/coordinator-verifier-enclave is the Keymeld enclave with the coordinator's\n"
            "verifier registered, built with Lightning Address (LNURL) support. Run it in place\n"
            "of the stock keymeld-enclave binary next to a Keymeld gateway of the version this\n"
            "release pins. It reads the same ENCLAVE_ID, VSOCK_PORT, TRANSPORT_MODE, and TCP_HOST\n"
            "environment as keymeld-enclave, plus COORDINATOR_ESCROW_LNURL_ENABLED and\n"
            "COORDINATOR_LNURL_RELAY_PORT.\n\n"
            "bin/coordinator-lnurl-relay is the host-side relay the enclave uses for LNURL\n"
            "traffic; run it on the relay port when LNURL is enabled.\n\n"
            "Use sha256sum -c SHA256SUMS to verify files. See docs/COORDINATOR_ENCLAVE.md:\n"
            "https://github.com/5day4cast/coordinator/blob/v" + version + "/docs/COORDINATOR_ENCLAVE.md\n"
        )
        return archive(package, output)


def package_swap(root, version, source, target, output):
    """Package ark-swapd, the service that swaps Lightning payments into Arkade escrows."""
    binaries = root / "target" / target / "release"
    require_files(binaries, ("ark-swapd",))
    with tempfile.TemporaryDirectory() as temporary:
        package = Path(temporary) / f"ark-swapd-{version}-{target}"
        (package / "bin").mkdir(parents=True)
        shutil.copy2(binaries / "ark-swapd", package / "bin" / "ark-swapd")
        provenance = metadata(root, version, source)
        provenance.update(target=target)
        (package / "RELEASE.json").write_text(json.dumps(provenance, indent=2) + "\n")
        (package / "README.txt").write_text(
            f"ark-swapd v{version}\nSource: {source}\n\n"
            "bin/ark-swapd takes a hold invoice on its LND node, pays the escrow from its own\n"
            "Arkade wallet, then settles. Run ./bin/ark-swapd --config ark-swapd.toml.\n"
            "Its database migrations are built in and run on startup.\n\n"
            "Use sha256sum -c SHA256SUMS to verify files. See crates/coordinator-ark-swap:\n"
            "https://github.com/5day4cast/coordinator/tree/v" + version + "/crates/coordinator-ark-swap\n"
        )
        return archive(package, output)


def package_synth(root, version, source, target, output):
    """Package synth, which runs synthetic competitions against a coordinator and serves a dashboard."""
    binaries = root / "target" / target / "release"
    require_files(binaries, ("synth",))
    with tempfile.TemporaryDirectory() as temporary:
        package = Path(temporary) / f"synth-{version}-{target}"
        (package / "bin").mkdir(parents=True)
        shutil.copy2(binaries / "synth", package / "bin" / "synth")
        provenance = metadata(root, version, source)
        provenance.update(target=target)
        (package / "RELEASE.json").write_text(json.dumps(provenance, indent=2) + "\n")
        (package / "README.txt").write_text(
            f"synth v{version}\nSource: {source}\n\n"
            "bin/synth runs synthetic competitions against a coordinator, paying for entries from an\n"
            "LND node, and serves a dashboard of how they fare. Run ./bin/synth synth.toml.\n\n"
            "Use sha256sum -c SHA256SUMS to verify files. See crates/synth:\n"
            "https://github.com/5day4cast/coordinator/tree/v" + version + "/crates/synth\n"
        )
        return archive(package, output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["validate", "wasm", "native", "enclave", "swap", "synth", "checksums"])
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--version")
    parser.add_argument("--source")
    parser.add_argument("--event", default="workflow_dispatch")
    parser.add_argument("--ref", default="")
    parser.add_argument("--dry-run", choices=["true", "false"], default="true")
    parser.add_argument("--wasm", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--asset")
    parser.add_argument("--output", type=Path, default=Path("release"))
    args = parser.parse_args()
    if args.command == "checksums":
        write_checksums(args.output)
        return
    if args.command == "validate":
        print(validate_source(args.root, args.version, args.event, args.ref, args.dry_run == "true"))
        return
    validate_version(args.root, args.version)
    if not args.source or not re.fullmatch(r"[0-9a-f]{40}", args.source):
        parser.error("--source must be the complete source commit SHA")
    if args.command == "wasm":
        print(package_wasm(args.root, args.version, args.source, args.wasm, args.output))
    elif args.command == "enclave":
        print(package_enclave(args.root, args.version, args.source, args.target, args.output))
    elif args.command == "swap":
        print(package_swap(args.root, args.version, args.source, args.target, args.output))
    elif args.command == "synth":
        print(package_synth(args.root, args.version, args.source, args.target, args.output))
    else:
        print(package_native(args.root, args.version, args.source, args.target, args.asset, args.wasm, args.output))


if __name__ == "__main__":
    main()
