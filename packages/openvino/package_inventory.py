#!/usr/bin/env python3
"""Create and verify the final package inventory trusted by the launcher."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import tempfile
from typing import Any, Sequence


INVENTORY_NAME = "package-inventory.v1"
LAUNCHER = "cfetch-openvino-adapter"
RUNTIME_DISPATCHER = "cfetch-openvino-adapter-runtime"
HEADER = "cfetch-package-inventory-v1"
MAX_INVENTORY_BYTES = 32 * 1024 * 1024
MAX_FILES = 50_000
MAX_BYTES = 4 * 1024 * 1024 * 1024
MAX_LAUNCHER_BYTES = 16 * 1024 * 1024
MAX_RUNTIME_MANIFEST_BYTES = 16 * 1024 * 1024
MAX_PACKAGE_MANIFEST_BYTES = 1024 * 1024
DIGEST_RE = re.compile(r"[0-9a-f]{64}")
LAUNCHER_DIGEST_PLACEHOLDER = "0" * 64


class InventoryError(ValueError):
    """A package inventory is unsafe, incomplete, or does not match bytes."""


@dataclass(frozen=True)
class RebindingProjection:
    """Deterministic bytes and identities for one manifest-only lifecycle step."""

    inventory_bytes: bytes
    inventory_sha256: str
    launcher_bytes: bytes
    launcher_sha256: str


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise InventoryError(f"JSON object contains duplicate key {key!r}")
        result[key] = value
    return result


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _bounded_read(path: Path, limit: int, label: str) -> bytes:
    try:
        metadata = path.stat()
    except OSError as error:
        raise InventoryError(f"cannot inspect {label}: {error}") from error
    if path.is_symlink() or not path.is_file():
        raise InventoryError(f"{label} must be a regular non-symlink file")
    if not 1 <= metadata.st_size <= limit:
        raise InventoryError(f"{label} has an invalid size")
    with path.open("rb") as source:
        raw = source.read(limit + 1)
    if not raw or len(raw) > limit:
        raise InventoryError(f"{label} changed size while read")
    return raw


def _safe_path(value: str, label: str) -> str:
    pure = PurePosixPath(value)
    if (
        not value
        or len(value) > 4096
        or pure.is_absolute()
        or any(part in ("", ".", "..") for part in pure.parts)
        or any(character in value for character in ("\x00", "\r", "\n", "\t", "\\"))
        or value in (INVENTORY_NAME, LAUNCHER)
    ):
        raise InventoryError(f"{label} is not a normalized safe relative path")
    return value


def _files(root: Path) -> list[tuple[str, Path]]:
    root = root.resolve()
    result: list[tuple[str, Path]] = []
    total_bytes = 0
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        current = Path(directory)
        for name in directory_names:
            path = current / name
            if path.is_symlink():
                raise InventoryError(
                    f"package inventory cannot include symlink directory {path}"
                )
        for name in file_names:
            path = current / name
            relative = path.relative_to(root).as_posix()
            if relative in (INVENTORY_NAME, LAUNCHER):
                if path.is_symlink() or not path.is_file():
                    raise InventoryError(f"excluded package root file is unsafe: {relative}")
                continue
            _safe_path(relative, "package file")
            if path.is_symlink() or not path.is_file():
                raise InventoryError(f"package entry is not regular: {relative}")
            size = path.stat().st_size
            total_bytes += size
            if total_bytes > MAX_BYTES:
                raise InventoryError(f"package exceeds the {MAX_BYTES}-byte inventory limit")
            result.append((relative, path))
            if len(result) > MAX_FILES:
                raise InventoryError(f"package exceeds the {MAX_FILES}-file inventory limit")
    result.sort(key=lambda item: item[0])
    return result


def _serialize(entries: Sequence[tuple[str, int, bool, str]]) -> bytes:
    lines = [HEADER]
    for digest, size, executable, relative in entries:
        lines.append(
            "\t".join(
                (digest, str(size), "1" if executable else "0", relative)
            )
        )
    raw = ("\n".join(lines) + "\n").encode("utf-8")
    if len(raw) > MAX_INVENTORY_BYTES:
        raise InventoryError("package inventory exceeds its byte limit")
    return raw


def create(root: Path) -> tuple[Path, str]:
    root = root.resolve()
    inventory = root / INVENTORY_NAME
    launcher = root / LAUNCHER
    runtime_dispatcher = root / RUNTIME_DISPATCHER
    if inventory.exists():
        raise InventoryError(f"refusing to overwrite existing {INVENTORY_NAME}")
    for path in (launcher, runtime_dispatcher):
        if path.is_symlink() or not path.is_file() or not path.stat().st_mode & 0o111:
            raise InventoryError(f"package requires executable regular file {path.name}")
    entries: list[tuple[str, int, bool, str]] = []
    for relative, path in _files(root):
        metadata = path.stat()
        entries.append(
            (
                _sha256_file(path),
                metadata.st_size,
                bool(metadata.st_mode & 0o111),
                relative,
            )
        )
    raw = _serialize(entries)
    inventory.write_bytes(raw)
    return inventory, hashlib.sha256(raw).hexdigest()


def parse(raw: bytes) -> list[tuple[str, int, bool, str]]:
    if not raw or len(raw) > MAX_INVENTORY_BYTES or not raw.endswith(b"\n"):
        raise InventoryError("package inventory is empty, oversized, or unterminated")
    try:
        lines = raw.decode("utf-8").splitlines()
    except UnicodeDecodeError as error:
        raise InventoryError("package inventory is not UTF-8") from error
    if not lines or lines[0] != HEADER:
        raise InventoryError("package inventory header is invalid")
    if not 1 <= len(lines) - 1 <= MAX_FILES:
        raise InventoryError("package inventory file count is invalid")
    entries: list[tuple[str, int, bool, str]] = []
    previous = ""
    total_bytes = 0
    for index, line in enumerate(lines[1:]):
        fields = line.split("\t")
        if len(fields) != 4:
            raise InventoryError(f"package inventory line {index + 2} is invalid")
        digest, size_text, executable_text, relative = fields
        if DIGEST_RE.fullmatch(digest) is None:
            raise InventoryError(f"package inventory line {index + 2} has invalid SHA-256")
        if (
            not size_text.isascii()
            or not size_text.isdecimal()
            or (size_text.startswith("0") and size_text != "0")
        ):
            raise InventoryError(f"package inventory line {index + 2} has invalid size")
        size = int(size_text)
        total_bytes += size
        if total_bytes > MAX_BYTES:
            raise InventoryError("package inventory exceeds its total byte limit")
        if executable_text not in ("0", "1"):
            raise InventoryError(f"package inventory line {index + 2} has invalid mode")
        relative = _safe_path(relative, f"package inventory line {index + 2} path")
        if relative <= previous:
            raise InventoryError("package inventory paths are duplicate or not sorted")
        previous = relative
        entries.append((digest, size, executable_text == "1", relative))
    return entries


def _read_inventory(path: Path) -> bytes:
    return _bounded_read(path, MAX_INVENTORY_BYTES, "package inventory")


def verify(root: Path, expected_sha256: str) -> str:
    root = root.resolve()
    if DIGEST_RE.fullmatch(expected_sha256) is None:
        raise InventoryError("expected inventory SHA-256 is invalid")
    raw = _read_inventory(root / INVENTORY_NAME)
    actual_inventory_sha256 = hashlib.sha256(raw).hexdigest()
    if actual_inventory_sha256 != expected_sha256:
        raise InventoryError("package inventory digest does not match the launcher binding")
    entries = parse(raw)
    actual = {relative: path for relative, path in _files(root)}
    expected_paths = {entry[3] for entry in entries}
    if set(actual) != expected_paths:
        raise InventoryError("package files do not exactly match the bound inventory")
    for digest, size, executable, relative in entries:
        path = actual[relative]
        metadata = path.stat()
        if metadata.st_size != size:
            raise InventoryError(f"package file size mismatch: {relative}")
        if bool(metadata.st_mode & 0o111) != executable:
            raise InventoryError(f"package file mode mismatch: {relative}")
        if _sha256_file(path) != digest:
            raise InventoryError(f"package file digest mismatch: {relative}")
    return actual_inventory_sha256


def _launcher_template_binding(root: Path) -> tuple[bytes, int, str]:
    raw = _bounded_read(
        root / "runtime-manifest.json",
        MAX_RUNTIME_MANIFEST_BYTES,
        "runtime manifest",
    )
    try:
        document = json.loads(raw, object_pairs_hook=_reject_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise InventoryError(f"runtime manifest is invalid JSON: {error}") from error
    if not isinstance(document, dict) or document.get("launcher") != LAUNCHER:
        raise InventoryError("runtime manifest does not bind the package launcher")
    offset = document.get("launcher_digest_offset")
    template_sha256 = document.get("launcher_template_sha256")
    if (
        type(offset) is not int
        or offset < 0
        or offset > MAX_LAUNCHER_BYTES - 64
        or not isinstance(template_sha256, str)
        or DIGEST_RE.fullmatch(template_sha256) is None
    ):
        raise InventoryError("runtime manifest launcher template binding is invalid")
    launcher_raw = _bounded_read(
        root / LAUNCHER, MAX_LAUNCHER_BYTES, "package launcher"
    )
    if offset + 64 > len(launcher_raw):
        raise InventoryError("runtime manifest launcher offset is outside the file")
    normalized = (
        launcher_raw[:offset]
        + LAUNCHER_DIGEST_PLACEHOLDER.encode("ascii")
        + launcher_raw[offset + 64 :]
    )
    if hashlib.sha256(normalized).hexdigest() != template_sha256:
        raise InventoryError("package launcher does not match its runtime template")
    return launcher_raw, offset, template_sha256


def verify_bound(root: Path, expected_sha256: str) -> str:
    """Verify both the complete inventory and its exact launcher embedding."""

    root = root.resolve()
    actual = verify(root, expected_sha256)
    launcher_raw, offset, _template_sha256 = _launcher_template_binding(root)
    embedded = launcher_raw[offset : offset + 64]
    if embedded != expected_sha256.encode("ascii"):
        raise InventoryError("package launcher does not embed the expected inventory")
    if launcher_raw.count(embedded) != 1:
        raise InventoryError("package launcher inventory binding is ambiguous")
    return actual


def patch_launcher(root: Path, inventory_sha256: str) -> str:
    if DIGEST_RE.fullmatch(inventory_sha256) is None:
        raise InventoryError("inventory SHA-256 is invalid for launcher binding")
    launcher = root.resolve() / LAUNCHER
    if launcher.is_symlink() or not launcher.is_file():
        raise InventoryError("launcher must be a regular non-symlink file")
    raw = _bounded_read(launcher, MAX_LAUNCHER_BYTES, "package launcher")
    placeholder = LAUNCHER_DIGEST_PLACEHOLDER.encode("ascii")
    if raw.count(placeholder) != 1:
        raise InventoryError("launcher does not contain one inventory digest placeholder")
    patched = raw.replace(placeholder, inventory_sha256.encode("ascii"), 1)
    launcher.write_bytes(patched)
    os.chmod(launcher, 0o755)
    return _sha256_file(launcher)


def _validate_new_package_manifest(raw: bytes) -> None:
    if not 1 <= len(raw) <= MAX_PACKAGE_MANIFEST_BYTES or not raw.endswith(b"\n"):
        raise InventoryError(
            "replacement package manifest must be bounded and newline terminated"
        )
    try:
        document = json.loads(raw, object_pairs_hook=_reject_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise InventoryError(f"replacement package manifest is invalid JSON: {error}") from error
    if not isinstance(document, dict):
        raise InventoryError("replacement package manifest must contain a JSON object")


def project_package_manifest_rebinding(
    root: Path,
    expected_old_inventory_sha256: str,
    new_package_manifest: bytes,
) -> RebindingProjection:
    """Project a manifest-only lifecycle rebind without modifying the package.

    Admission can use this on a candidate package with reconstructed probe
    manifest bytes to recover the exact probe inventory and launcher SHA-256.
    """

    root = root.resolve()
    _validate_new_package_manifest(new_package_manifest)
    verify_bound(root, expected_old_inventory_sha256)
    old_inventory = _read_inventory(root / INVENTORY_NAME)
    entries = parse(old_inventory)
    replacement: list[tuple[str, int, bool, str]] = []
    found = False
    for digest, size, executable, relative in entries:
        if relative == "package-manifest.json":
            if found or executable:
                raise InventoryError("package manifest inventory entry is unsafe")
            found = True
            replacement.append(
                (
                    hashlib.sha256(new_package_manifest).hexdigest(),
                    len(new_package_manifest),
                    False,
                    relative,
                )
            )
        else:
            replacement.append((digest, size, executable, relative))
    if not found:
        raise InventoryError("package inventory does not contain package-manifest.json")
    inventory_bytes = _serialize(replacement)
    inventory_sha256 = hashlib.sha256(inventory_bytes).hexdigest()
    launcher_raw, offset, _template_sha256 = _launcher_template_binding(root)
    old_digest = expected_old_inventory_sha256.encode("ascii")
    if launcher_raw[offset : offset + 64] != old_digest:
        raise InventoryError("launcher changed after old inventory verification")
    launcher_bytes = (
        launcher_raw[:offset]
        + inventory_sha256.encode("ascii")
        + launcher_raw[offset + 64 :]
    )
    return RebindingProjection(
        inventory_bytes=inventory_bytes,
        inventory_sha256=inventory_sha256,
        launcher_bytes=launcher_bytes,
        launcher_sha256=hashlib.sha256(launcher_bytes).hexdigest(),
    )


def _atomic_write(path: Path, raw: bytes, mode: int) -> None:
    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            prefix=".cfetch-rebind-", dir=path.parent, delete=False
        ) as output:
            temporary = Path(output.name)
            output.write(raw)
            output.flush()
            os.fsync(output.fileno())
        os.chmod(temporary, mode)
        temporary.replace(path)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def rebind_package_manifest(
    root: Path,
    expected_old_inventory_sha256: str,
    new_package_manifest: bytes,
) -> tuple[str, str]:
    """Transactionally rebind a verified package after a manifest-only phase change.

    Returns `(new_inventory_sha256, new_launcher_sha256)`. On any failed final
    verification, the prior manifest, inventory, and launcher bytes are
    restored and reverified before the error is returned.
    """

    root = root.resolve()
    projection = project_package_manifest_rebinding(
        root, expected_old_inventory_sha256, new_package_manifest
    )
    manifest_path = root / "package-manifest.json"
    inventory_path = root / INVENTORY_NAME
    launcher_path = root / LAUNCHER
    old_manifest = _bounded_read(
        manifest_path, MAX_PACKAGE_MANIFEST_BYTES, "package manifest"
    )
    old_inventory = _read_inventory(inventory_path)
    old_launcher = _bounded_read(
        launcher_path, MAX_LAUNCHER_BYTES, "package launcher"
    )
    manifest_mode = manifest_path.stat().st_mode & 0o777
    inventory_mode = inventory_path.stat().st_mode & 0o777
    launcher_mode = launcher_path.stat().st_mode & 0o777
    try:
        _atomic_write(manifest_path, new_package_manifest, manifest_mode)
        _atomic_write(inventory_path, projection.inventory_bytes, inventory_mode)
        _atomic_write(launcher_path, projection.launcher_bytes, launcher_mode)
        verify_bound(root, projection.inventory_sha256)
    except Exception as error:
        try:
            _atomic_write(manifest_path, old_manifest, manifest_mode)
            _atomic_write(inventory_path, old_inventory, inventory_mode)
            _atomic_write(launcher_path, old_launcher, launcher_mode)
            verify_bound(root, expected_old_inventory_sha256)
        except Exception as rollback_error:
            raise InventoryError(
                "package manifest rebind failed and rollback verification also failed"
            ) from rollback_error
        if isinstance(error, InventoryError):
            raise
        raise InventoryError(f"package manifest rebind failed: {error}") from error
    return projection.inventory_sha256, projection.launcher_sha256


def inventory_digest_from_environment(environment: dict[str, str] | None = None) -> str:
    values = os.environ if environment is None else environment
    digest = values.pop("CFETCH_PACKAGE_INVENTORY_SHA256", "")
    if DIGEST_RE.fullmatch(digest) is None:
        raise InventoryError("trusted launcher inventory binding is missing")
    return digest
