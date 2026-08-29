#!/usr/bin/env python3
"""Create and verify a content-bound Linux x86_64 PyInstaller runtime."""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import sys
from typing import Any, Sequence


SCHEMA_VERSION = 1
TARGET = "linux-x86_64-glibc"
PYTHON_ABI = "cp312"
LAUNCHER = "cfetch-openvino-adapter"
DISPATCHER = "cfetch-openvino-adapter-runtime"
MANIFEST_NAME = "runtime-manifest.json"
RUNTIME_DISTRIBUTIONS = ("cryptography", "numpy", "openvino", "tokenizers")
MAX_MANIFEST_BYTES = 16 * 1024 * 1024
MAX_BUNDLE_FILES = 50_000
MAX_BUNDLE_BYTES = 4 * 1024 * 1024 * 1024
DIGEST_RE = re.compile(r"[0-9a-f]{64}")
VERSION_RE = re.compile(r"[0-9]+(?:\.[0-9]+)+")
LAUNCHER_DIGEST_PLACEHOLDER = b"0" * 64


class RuntimeBundleError(ValueError):
    """A frozen runtime does not satisfy its target or integrity contract."""


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise RuntimeBundleError(f"runtime manifest contains duplicate key {key!r}")
        result[key] = value
    return result


def _bounded_read(path: Path, limit: int, label: str) -> bytes:
    try:
        metadata = path.stat()
    except OSError as error:
        raise RuntimeBundleError(f"cannot inspect {label}: {error}") from error
    if path.is_symlink() or not path.is_file():
        raise RuntimeBundleError(f"{label} must be a regular non-symlink file")
    if metadata.st_size < 1 or metadata.st_size > limit:
        raise RuntimeBundleError(f"{label} must contain 1..{limit} bytes")
    try:
        with path.open("rb") as source:
            raw = source.read(limit + 1)
    except OSError as error:
        raise RuntimeBundleError(f"cannot read {label}: {error}") from error
    if not raw or len(raw) > limit:
        raise RuntimeBundleError(f"{label} changed size while it was read")
    return raw


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _version_tuple(value: Any, label: str) -> tuple[int, ...]:
    if not isinstance(value, str) or VERSION_RE.fullmatch(value) is None:
        raise RuntimeBundleError(f"{label} must be a dotted numeric version")
    return tuple(int(piece) for piece in value.split("."))


def _require_build_target(minimum_glibc: str) -> None:
    if sys.platform != "linux" or platform.machine() not in ("x86_64", "AMD64"):
        raise RuntimeBundleError("runtime bundle must be built on Linux x86_64")
    if sys.implementation.name != "cpython" or sys.version_info[:2] != (3, 12):
        raise RuntimeBundleError("runtime bundle must be built with CPython 3.12")
    libc_name, libc_version = platform.libc_ver()
    if libc_name != "glibc" or not libc_version:
        raise RuntimeBundleError("runtime bundle requires a glibc build host")
    if _version_tuple(libc_version, "build-host glibc") != _version_tuple(
        minimum_glibc, "minimum glibc"
    ):
        raise RuntimeBundleError(
            "build-host glibc must exactly equal the declared conservative minimum; "
            f"found {libc_version}, declared {minimum_glibc}"
        )


def dependency_versions() -> dict[str, str]:
    versions: dict[str, str] = {}
    for distribution in RUNTIME_DISTRIBUTIONS:
        try:
            versions[distribution] = importlib.metadata.version(distribution)
        except importlib.metadata.PackageNotFoundError as error:
            raise RuntimeBundleError(
                f"runtime dependency {distribution} is not installed in the build environment"
            ) from error
    return versions


def _bundle_files(root: Path) -> list[dict[str, Any]]:
    root = root.resolve()
    files: list[dict[str, Any]] = []
    total_bytes = 0
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        current = Path(directory)
        for name in directory_names:
            path = current / name
            if path.is_symlink():
                raise RuntimeBundleError(f"runtime bundle directory is a symlink: {path}")
        for name in file_names:
            path = current / name
            relative = path.relative_to(root).as_posix()
            if relative in (MANIFEST_NAME, LAUNCHER):
                continue
            if path.is_symlink() or not path.is_file():
                raise RuntimeBundleError(
                    f"runtime bundle entry is not a regular file: {relative}"
                )
            metadata = path.stat()
            if metadata.st_size < 1:
                raise RuntimeBundleError(f"runtime bundle file is empty: {relative}")
            total_bytes += metadata.st_size
            if total_bytes > MAX_BUNDLE_BYTES:
                raise RuntimeBundleError(
                    f"runtime bundle exceeds the {MAX_BUNDLE_BYTES}-byte limit"
                )
            files.append(
                {
                    "path": relative,
                    "sha256": _sha256_file(path),
                    "bytes": metadata.st_size,
                    "executable": bool(metadata.st_mode & 0o111),
                }
            )
            if len(files) > MAX_BUNDLE_FILES:
                raise RuntimeBundleError(
                    f"runtime bundle exceeds the {MAX_BUNDLE_FILES}-file limit"
                )
    files.sort(key=lambda entry: entry["path"])
    if not files:
        raise RuntimeBundleError("runtime bundle is empty")
    return files


def create_manifest(root: Path, minimum_glibc: str) -> tuple[Path, str]:
    root = root.resolve()
    launcher = root / LAUNCHER
    dispatcher = root / DISPATCHER
    for label, executable in (("launcher", launcher), ("dispatcher", dispatcher)):
        if executable.is_symlink() or not executable.is_file():
            raise RuntimeBundleError(
                f"runtime bundle must contain regular {label} {executable.name}"
            )
        if not executable.stat().st_mode & 0o111:
            raise RuntimeBundleError(f"runtime {label} must be executable")
    launcher_raw = _bounded_read(launcher, 16 * 1024 * 1024, "runtime launcher")
    if launcher_raw.count(LAUNCHER_DIGEST_PLACEHOLDER) != 1:
        raise RuntimeBundleError(
            "runtime launcher must contain exactly one unpatched inventory binding"
        )
    launcher_digest_offset = launcher_raw.index(LAUNCHER_DIGEST_PLACEHOLDER)
    _require_build_target(minimum_glibc)
    files = _bundle_files(root)
    dispatcher_entries = [entry for entry in files if entry["path"] == DISPATCHER]
    if len(dispatcher_entries) != 1 or dispatcher_entries[0]["executable"] is not True:
        raise RuntimeBundleError("runtime manifest did not bind one executable dispatcher")
    document = {
        "schema_version": SCHEMA_VERSION,
        "target": TARGET,
        "python_abi": PYTHON_ABI,
        "minimum_glibc": minimum_glibc,
        "launcher": LAUNCHER,
        "launcher_template_sha256": hashlib.sha256(launcher_raw).hexdigest(),
        "launcher_digest_offset": launcher_digest_offset,
        "dispatcher": DISPATCHER,
        "dependency_versions": dependency_versions(),
        "builder": {
            "python": platform.python_version(),
            "pyinstaller": importlib.metadata.version("pyinstaller"),
        },
        "external_prerequisites": {
            "npu": "matching Intel NPU kernel driver and firmware",
            "gpu": "matching Intel GPU kernel and user-mode compute drivers",
            "cpu": "admitted x86_64 accelerated CPU family",
        },
        "files": files,
    }
    raw = (
        json.dumps(document, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n"
    ).encode("utf-8")
    if len(raw) > MAX_MANIFEST_BYTES:
        raise RuntimeBundleError("runtime manifest exceeds its byte limit")
    path = root / MANIFEST_NAME
    if path.exists():
        raise RuntimeBundleError(f"refusing to overwrite existing {MANIFEST_NAME}")
    path.write_bytes(raw)
    return path, hashlib.sha256(raw).hexdigest()


def _safe_relative_file(value: Any, label: str) -> str:
    if not isinstance(value, str):
        raise RuntimeBundleError(f"{label} must be a string")
    pure = PurePosixPath(value)
    if (
        pure.is_absolute()
        or not pure.parts
        or any(part in ("", ".", "..") for part in pure.parts)
        or value in (MANIFEST_NAME, LAUNCHER)
    ):
        raise RuntimeBundleError(f"{label} is not a safe bundle-relative file")
    return value


def _actual_files(root: Path) -> set[str]:
    result: set[str] = set()
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        current = Path(directory)
        for name in directory_names:
            path = current / name
            if path.is_symlink():
                raise RuntimeBundleError(
                    f"runtime directory contains symlink: {path.relative_to(root)}"
                )
        for name in file_names:
            path = current / name
            relative = path.relative_to(root).as_posix()
            if path.is_symlink() or not path.is_file():
                raise RuntimeBundleError(
                    f"runtime directory entry is not regular: {relative}"
                )
            if relative not in (MANIFEST_NAME, LAUNCHER):
                result.add(relative)
            if len(result) > MAX_BUNDLE_FILES:
                raise RuntimeBundleError(
                    f"runtime directory exceeds the {MAX_BUNDLE_FILES}-file limit"
                )
    return result


def load_and_verify(
    root: Path,
    expected_sha256: str | None = None,
    allowed_unbound_files: Sequence[str] = (),
) -> dict[str, Any]:
    root = root.resolve()
    manifest_path = root / MANIFEST_NAME
    raw = _bounded_read(manifest_path, MAX_MANIFEST_BYTES, "runtime manifest")
    actual_sha256 = hashlib.sha256(raw).hexdigest()
    if expected_sha256 is not None:
        if DIGEST_RE.fullmatch(expected_sha256) is None:
            raise RuntimeBundleError(
                "expected runtime manifest digest must be 64 lowercase hexadecimal characters"
            )
        if actual_sha256 != expected_sha256:
            raise RuntimeBundleError(
                "runtime manifest digest mismatch: "
                f"expected {expected_sha256}, found {actual_sha256}"
            )
    try:
        document = json.loads(raw, object_pairs_hook=_reject_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeBundleError(f"runtime manifest is invalid JSON: {error}") from error
    required = {
        "schema_version",
        "target",
        "python_abi",
        "minimum_glibc",
        "launcher",
        "launcher_template_sha256",
        "launcher_digest_offset",
        "dispatcher",
        "dependency_versions",
        "builder",
        "external_prerequisites",
        "files",
    }
    if not isinstance(document, dict) or set(document) != required:
        raise RuntimeBundleError("runtime manifest fields do not match schema version 1")
    if (
        document["schema_version"] != SCHEMA_VERSION
        or document["target"] != TARGET
        or document["python_abi"] != PYTHON_ABI
        or document["launcher"] != LAUNCHER
        or document["dispatcher"] != DISPATCHER
    ):
        raise RuntimeBundleError("runtime manifest target identity is invalid")
    launcher_template_sha256 = document["launcher_template_sha256"]
    launcher_digest_offset = document["launcher_digest_offset"]
    if (
        not isinstance(launcher_template_sha256, str)
        or DIGEST_RE.fullmatch(launcher_template_sha256) is None
        or type(launcher_digest_offset) is not int
        or launcher_digest_offset < 0
        or launcher_digest_offset > 16 * 1024 * 1024 - 64
    ):
        raise RuntimeBundleError("runtime launcher binding is invalid")
    launcher_raw = _bounded_read(
        root / LAUNCHER, 16 * 1024 * 1024, "runtime launcher"
    )
    if launcher_digest_offset + 64 > len(launcher_raw):
        raise RuntimeBundleError("runtime launcher binding offset is outside the file")
    embedded = launcher_raw[launcher_digest_offset : launcher_digest_offset + 64]
    if embedded != LAUNCHER_DIGEST_PLACEHOLDER and DIGEST_RE.fullmatch(
        embedded.decode("ascii", errors="ignore")
    ) is None:
        raise RuntimeBundleError("runtime launcher has an invalid inventory binding")
    normalized_launcher = (
        launcher_raw[:launcher_digest_offset]
        + LAUNCHER_DIGEST_PLACEHOLDER
        + launcher_raw[launcher_digest_offset + 64 :]
    )
    if hashlib.sha256(normalized_launcher).hexdigest() != launcher_template_sha256:
        raise RuntimeBundleError("runtime launcher template digest mismatch")
    _version_tuple(document.get("minimum_glibc"), "minimum glibc")
    dependencies = document["dependency_versions"]
    if not isinstance(dependencies, dict) or set(dependencies) != set(
        RUNTIME_DISTRIBUTIONS
    ):
        raise RuntimeBundleError("runtime manifest dependency versions are incomplete")
    if any(not isinstance(value, str) or not value for value in dependencies.values()):
        raise RuntimeBundleError("runtime manifest has an invalid dependency version")
    builder = document["builder"]
    if not isinstance(builder, dict) or set(builder) != {"python", "pyinstaller"}:
        raise RuntimeBundleError("runtime manifest builder identity is invalid")
    _version_tuple(builder["python"], "builder Python")
    _version_tuple(builder["pyinstaller"], "builder PyInstaller")
    prerequisites = document["external_prerequisites"]
    if not isinstance(prerequisites, dict) or set(prerequisites) != {
        "npu",
        "gpu",
        "cpu",
    }:
        raise RuntimeBundleError("runtime external prerequisites are incomplete")
    if any(
        not isinstance(value, str)
        or not value
        or len(value) > 512
        or any(character in value for character in ("\x00", "\r", "\n"))
        for value in prerequisites.values()
    ):
        raise RuntimeBundleError("runtime external prerequisite text is invalid")
    entries = document["files"]
    if not isinstance(entries, list) or not entries or len(entries) > MAX_BUNDLE_FILES:
        raise RuntimeBundleError("runtime manifest files array is invalid")
    seen: set[str] = set()
    total_bytes = 0
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict) or set(entry) != {
            "path",
            "sha256",
            "bytes",
            "executable",
        }:
            raise RuntimeBundleError(f"runtime files[{index}] is invalid")
        relative = _safe_relative_file(
            entry["path"], f"runtime files[{index}].path"
        )
        pure = PurePosixPath(relative)
        if (
            relative in seen
        ):
            raise RuntimeBundleError(f"runtime files[{index}].path is unsafe or duplicate")
        seen.add(relative)
        if not isinstance(entry["sha256"], str) or DIGEST_RE.fullmatch(
            entry["sha256"]
        ) is None:
            raise RuntimeBundleError(f"runtime files[{index}].sha256 is invalid")
        if type(entry["bytes"]) is not int or entry["bytes"] < 1:
            raise RuntimeBundleError(f"runtime files[{index}].bytes is invalid")
        if type(entry["executable"]) is not bool:
            raise RuntimeBundleError(f"runtime files[{index}].executable is invalid")
        total_bytes += entry["bytes"]
        if total_bytes > MAX_BUNDLE_BYTES:
            raise RuntimeBundleError("runtime manifest exceeds its total byte limit")
        path = root.joinpath(*pure.parts)
        try:
            path.resolve().relative_to(root)
        except ValueError as error:
            raise RuntimeBundleError(f"runtime files[{index}].path escapes bundle") from error
        if path.is_symlink() or not path.is_file():
            raise RuntimeBundleError(f"runtime file is missing or not regular: {relative}")
        metadata = path.stat()
        if metadata.st_size != entry["bytes"]:
            raise RuntimeBundleError(f"runtime file size mismatch: {relative}")
        if bool(metadata.st_mode & 0o111) != entry["executable"]:
            raise RuntimeBundleError(f"runtime file mode mismatch: {relative}")
        if _sha256_file(path) != entry["sha256"]:
            raise RuntimeBundleError(f"runtime file digest mismatch: {relative}")
    if DISPATCHER not in seen:
        raise RuntimeBundleError("runtime manifest omits its dispatcher")
    allowed = {
        _safe_relative_file(value, f"allowed_unbound_files[{index}]")
        for index, value in enumerate(allowed_unbound_files)
    }
    if seen & allowed:
        raise RuntimeBundleError("an allowed package file is already bound by the runtime")
    if _actual_files(root) != seen | allowed:
        raise RuntimeBundleError("runtime directory contains unbound files")
    document["runtime_manifest_sha256"] = actual_sha256
    return document


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    subcommands = result.add_subparsers(dest="command", required=True)
    create = subcommands.add_parser("create")
    create.add_argument("--directory", required=True, type=Path)
    create.add_argument("--minimum-glibc", required=True)
    verify = subcommands.add_parser("verify")
    verify.add_argument("--directory", required=True, type=Path)
    verify.add_argument("--manifest-sha256")
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "create":
            path, digest = create_manifest(args.directory, args.minimum_glibc)
            output = {
                "schema_version": 1,
                "runtime_manifest": str(path),
                "runtime_manifest_sha256": digest,
            }
        else:
            document = load_and_verify(args.directory, args.manifest_sha256)
            output = {
                "schema_version": 1,
                "runtime_manifest": str(args.directory / MANIFEST_NAME),
                "runtime_manifest_sha256": document["runtime_manifest_sha256"],
            }
    except (OSError, RuntimeBundleError) as error:
        print(f"OpenVINO runtime bundle refused: {error}", file=sys.stderr)
        return 1
    print(json.dumps(output, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
