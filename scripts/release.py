#!/usr/bin/env python3
"""Read and validate the canonical release.toml automation metadata."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import tomllib

REQUIRED_KEYS = (
    "owner",
    "repository",
    "package",
    "binary",
    "description",
    "homepage",
    "license",
    "release_branch",
    "distribution_repository",
    "dispatch_event",
)
REQUIRED_TOKENS = (
    "windows_amd64",
    "windows_arm64",
    "darwin_amd64",
    "darwin_arm64",
    "linux_amd64",
    "linux_arm64",
)


def load_metadata(path: Path) -> dict:
    metadata = tomllib.loads(path.read_text(encoding="utf-8"))
    for key in REQUIRED_KEYS:
        if not isinstance(metadata.get(key), str) or not metadata[key]:
            raise ValueError(f"missing release metadata value: {key}")
    targets = metadata.get("targets")
    if not isinstance(targets, list):
        raise ValueError("release metadata must declare targets")
    tokens = tuple(target.get("token") for target in targets)
    if tokens != REQUIRED_TOKENS:
        raise ValueError(f"release target tokens must be exactly {REQUIRED_TOKENS!r}")
    for target in targets:
        for key in ("runner", "rust_target", "archive", "executable"):
            if not isinstance(target.get(key), str) or not target[key]:
                raise ValueError(f"target {target['token']} is missing {key}")
        if target["archive"] not in ("zip", "tar.gz"):
            raise ValueError(f"target {target['token']} has unsupported archive format")
    return metadata


def write_github_outputs(metadata: dict, output: Path) -> None:
    values = {key: metadata[key] for key in REQUIRED_KEYS}
    values["github_repository"] = f"{metadata['owner']}/{metadata['repository']}"
    values["matrix"] = json.dumps({"include": metadata["targets"]}, separators=(",", ":"))
    values["metadata"] = json.dumps(metadata, separators=(",", ":"))
    with output.open("a", encoding="utf-8", newline="\n") as destination:
        for key, value in values.items():
            destination.write(f"{key}={value}\n")


def expected_assets(metadata: dict, version: str) -> list[str]:
    names = []
    for target in metadata["targets"]:
        suffix = ".zip" if target["archive"] == "zip" else ".tar.gz"
        names.append(f"{metadata['package']}_{version}_{target['token']}{suffix}")
    return sorted(names)


def verify_assets(metadata: dict, version: str, directory: Path) -> None:
    actual = sorted(path.name for path in directory.iterdir() if path.is_file())
    expected = expected_assets(metadata, version)
    if actual != expected:
        raise ValueError(f"release assets differ: expected {expected!r}, got {actual!r}")


def test_self() -> None:
    metadata = load_metadata(Path("release.toml"))
    assert metadata["package"] == "azc"
    assert expected_assets(metadata, "1.2.3") == sorted(
        [
            "azc_1.2.3_windows_amd64.zip",
            "azc_1.2.3_windows_arm64.zip",
            "azc_1.2.3_darwin_amd64.tar.gz",
            "azc_1.2.3_darwin_arm64.tar.gz",
            "azc_1.2.3_linux_amd64.tar.gz",
            "azc_1.2.3_linux_arm64.tar.gz",
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", type=Path, default=Path("release.toml"))
    subparsers = parser.add_subparsers(dest="command", required=True)
    outputs = subparsers.add_parser("github-outputs")
    outputs.add_argument("--output", type=Path, required=True)
    expected = subparsers.add_parser("expected-assets")
    expected.add_argument("--version", required=True)
    verify = subparsers.add_parser("verify-assets")
    verify.add_argument("--version", required=True)
    verify.add_argument("--directory", type=Path, required=True)
    subparsers.add_parser("self-test")

    arguments = parser.parse_args()
    metadata = load_metadata(arguments.metadata)
    if arguments.command == "github-outputs":
        write_github_outputs(metadata, arguments.output)
    elif arguments.command == "expected-assets":
        print("\n".join(expected_assets(metadata, arguments.version)))
    elif arguments.command == "verify-assets":
        verify_assets(metadata, arguments.version, arguments.directory)
    else:
        test_self()


if __name__ == "__main__":
    main()
