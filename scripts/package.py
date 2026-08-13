#!/usr/bin/env python3
"""Create, inspect, and checksum azc release archives without dependencies."""

from __future__ import annotations

import argparse
import datetime
import gzip
import hashlib
from pathlib import Path, PurePosixPath
import stat
import tarfile
import tempfile
import zipfile


def archive_name(package: str, version: str, token: str, kind: str) -> str:
    suffix = ".zip" if kind == "zip" else ".tar.gz"
    return f"{package}_{version}_{token}{suffix}"


def validate_entry(name: str, expected: str) -> None:
    path = PurePosixPath(name)
    if name != expected or path.is_absolute() or ".." in path.parts or "\\" in name:
        raise ValueError(f"unsafe or unexpected archive entry: {name!r}")


def create_archive(binary: Path, output: Path, executable: str, kind: str, epoch: int) -> None:
    if not binary.is_file():
        raise FileNotFoundError(f"release binary not found: {binary}")
    output.parent.mkdir(parents=True, exist_ok=True)

    if kind == "zip":
        timestamp = max(epoch, 315532800)
        date_time = datetime.datetime.fromtimestamp(timestamp, datetime.UTC).timetuple()[:6]
        info = zipfile.ZipInfo(executable, date_time=date_time)
        info.compress_type = zipfile.ZIP_DEFLATED
        info.external_attr = (stat.S_IFREG | 0o755) << 16
        with zipfile.ZipFile(output, "w") as archive:
            archive.writestr(info, binary.read_bytes())
        return

    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                info = archive.gettarinfo(str(binary), arcname=executable)
                info.mode = 0o755
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                info.mtime = epoch
                with binary.open("rb") as source:
                    archive.addfile(info, source)


def inspect_archive(archive_path: Path, executable: str) -> None:
    if archive_path.suffix == ".zip":
        with zipfile.ZipFile(archive_path) as archive:
            entries = archive.infolist()
            if len(entries) != 1:
                raise ValueError(f"archive must contain exactly one entry: {archive_path}")
            entry = entries[0]
            validate_entry(entry.filename, executable)
            mode = entry.external_attr >> 16
            if not stat.S_ISREG(mode):
                raise ValueError(f"archive entry is not a regular file: {entry.filename}")
            with archive.open(entry) as source:
                if not source.read(1):
                    raise ValueError(f"archive executable is empty: {entry.filename}")
        return

    with tarfile.open(archive_path, mode="r:gz") as archive:
        entries = archive.getmembers()
        if len(entries) != 1:
            raise ValueError(f"archive must contain exactly one entry: {archive_path}")
        entry = entries[0]
        validate_entry(entry.name, executable)
        if not entry.isfile() or entry.issym() or entry.islnk():
            raise ValueError(f"archive entry is not a regular file: {entry.name}")
        if entry.mode & 0o111 == 0:
            raise ValueError(f"archive executable bit is missing: {entry.name}")
        source = archive.extractfile(entry)
        if source is None or not source.read(1):
            raise ValueError(f"archive executable is empty: {entry.name}")


def release_assets(directory: Path) -> list[Path]:
    return sorted(
        path
        for path in directory.iterdir()
        if path.is_file()
        and path.name != "checksums.txt"
        and not path.name.endswith((".sig", ".asc"))
    )


def write_checksums(directory: Path) -> None:
    assets = release_assets(directory)
    if not assets:
        raise ValueError(f"no release assets found in {directory}")
    lines = [f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in assets]
    (directory / "checksums.txt").write_text("".join(lines), encoding="utf-8", newline="\n")


def verify_checksums(directory: Path) -> None:
    checksum_path = directory / "checksums.txt"
    raw = checksum_path.read_bytes()
    if b"\r" in raw or not raw.endswith(b"\n"):
        raise ValueError("checksums.txt must use LF line endings and end with a newline")
    lines = raw.decode("utf-8").splitlines()
    expected_names = [path.name for path in release_assets(directory)]
    actual_names: list[str] = []
    for line in lines:
        if len(line) < 67 or line[64:66] != "  ":
            raise ValueError(f"malformed checksum line: {line!r}")
        digest, name = line[:64], line[66:]
        if any(character not in "0123456789abcdef" for character in digest):
            raise ValueError(f"malformed SHA-256 digest for {name!r}")
        validate_entry(name, name)
        asset = directory / name
        actual = hashlib.sha256(asset.read_bytes()).hexdigest()
        if actual != digest:
            raise ValueError(f"checksum mismatch for {name}")
        actual_names.append(name)
    if actual_names != sorted(actual_names) or actual_names != expected_names:
        raise ValueError("checksums.txt must be sorted and cover every release asset exactly once")


def test_self() -> None:
    import io

    def rejects(operation, message: str) -> None:
        try:
            operation()
        except (FileNotFoundError, ValueError):
            return
        raise AssertionError(message)

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        binary = root / "azc"
        binary.write_bytes(b"release-binary")
        archives: list[tuple[Path, str]] = []
        for kind, executable in (("zip", "azc.exe"), ("tar.gz", "azc")):
            archive = root / archive_name("azc", "1.2.3", f"test_{kind}", kind)
            create_archive(binary, archive, executable, kind, 1_786_579_200)
            inspect_archive(archive, executable)
            archives.append((archive, executable))
            first = archive.read_bytes()
            create_archive(binary, archive, executable, kind, 1_786_579_200)
            assert archive.read_bytes() == first

        write_checksums(root)
        verify_checksums(root)
        checksum_bytes = (root / "checksums.txt").read_bytes()
        assert b"\r" not in checksum_bytes
        checksum_names = [line[66:] for line in checksum_bytes.decode("utf-8").splitlines()]
        assert checksum_names == sorted(checksum_names)

        bad = root / "bad.tar.gz"
        with tarfile.open(bad, "w:gz") as archive:
            info = tarfile.TarInfo("../azc")
            info.size = 1
            archive.addfile(info, io.BytesIO(b"x"))
        rejects(lambda: inspect_archive(bad, "azc"), "unsafe archive path was accepted")

        duplicate = root / "duplicate.zip"
        with zipfile.ZipFile(duplicate, "w") as archive:
            archive.writestr("azc.exe", b"x")
            archive.writestr("README", b"x")
        rejects(
            lambda: inspect_archive(duplicate, "azc.exe"),
            "unexpected archive entry was accepted",
        )

        tampered = archives[0][0]
        tampered.write_bytes(tampered.read_bytes() + b"x")
        rejects(lambda: verify_checksums(root), "checksum mismatch was accepted")


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    create = subparsers.add_parser("create")
    create.add_argument("--binary", type=Path, required=True)
    create.add_argument("--output", type=Path, required=True)
    create.add_argument("--executable", required=True)
    create.add_argument("--kind", choices=("zip", "tar.gz"), required=True)
    create.add_argument("--epoch", type=int, required=True)

    inspect = subparsers.add_parser("inspect")
    inspect.add_argument("--archive", type=Path, required=True)
    inspect.add_argument("--executable", required=True)

    checksums = subparsers.add_parser("checksums")
    checksums.add_argument("--directory", type=Path, required=True)
    verify = subparsers.add_parser("verify-checksums")
    verify.add_argument("--directory", type=Path, required=True)
    subparsers.add_parser("self-test")

    arguments = parser.parse_args()
    if arguments.command == "create":
        create_archive(arguments.binary, arguments.output, arguments.executable, arguments.kind, arguments.epoch)
    elif arguments.command == "inspect":
        inspect_archive(arguments.archive, arguments.executable)
    elif arguments.command == "checksums":
        write_checksums(arguments.directory)
    elif arguments.command == "verify-checksums":
        verify_checksums(arguments.directory)
    else:
        test_self()


if __name__ == "__main__":
    main()
