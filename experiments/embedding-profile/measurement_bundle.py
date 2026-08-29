#!/usr/bin/env python3
"""Build deterministic, content-addressed raw measurement evidence bundles."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import tempfile
import zipfile
from pathlib import Path

from cross_backend_eval import (
    MAX_MEASUREMENT_BUNDLE_BYTES,
    expected_measurement_roles,
    load_cache,
    load_embedded_evidence_reports,
    validate_measurement_bundle,
)

ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def raw_files_by_digest(root: Path) -> dict[str, Path]:
    if not root.is_dir() or root.is_symlink():
        raise ValueError("raw measurement input must be a real directory")
    result: dict[str, Path] = {}
    total = 0
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"raw measurement input contains a symlink: {path}")
        if not path.is_file():
            continue
        size = path.stat().st_size
        if size < 1:
            raise ValueError(f"raw measurement input is empty: {path}")
        total += size
        if total > MAX_MEASUREMENT_BUNDLE_BYTES:
            raise ValueError("raw measurement inputs exceed the bundle byte bound")
        digest = file_sha256(path)
        previous = result.get(digest)
        if previous is not None:
            raise ValueError(
                f"raw measurement input repeats digest {digest}: {previous} and {path}"
            )
        result[digest] = path
    if not result:
        raise ValueError("raw measurement input directory contains no files")
    return result


def canonical_zip_info(name: str) -> zipfile.ZipInfo:
    info = zipfile.ZipInfo(name, ZIP_TIMESTAMP)
    info.compress_type = zipfile.ZIP_DEFLATED
    info.create_system = 3
    info.external_attr = 0o100644 << 16
    return info


def build_measurement_bundle(
    cache_path: Path,
    raw_root: Path,
    output_directory: Path,
) -> Path:
    metadata, _, _ = load_cache(cache_path)
    scope_id = str(metadata["scope_id"])
    evidence_reports = load_embedded_evidence_reports(cache_path)
    roles = expected_measurement_roles(evidence_reports)
    available = raw_files_by_digest(raw_root)
    missing = sorted(set(roles).difference(available))
    if missing:
        raise ValueError(
            "raw measurement directory is missing evidence bytes: " + ", ".join(missing)
        )
    unexpected = sorted(set(available).difference(roles))
    if unexpected:
        raise ValueError(
            "raw measurement directory contains unreferenced evidence bytes: "
            + ", ".join(unexpected)
        )

    manifest = {
        "schema_version": 1,
        "scope_id": scope_id,
        "placement_evidence_sha256": metadata["placement_evidence_sha256"],
        "performance_evidence_sha256": metadata["performance_evidence_sha256"],
        "files": [
            {
                "path": f"raw/{digest}.bin",
                "sha256": digest,
                "roles": roles[digest],
            }
            for digest in sorted(roles)
        ],
    }
    manifest_bytes = (
        json.dumps(manifest, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n"
    ).encode("utf-8")

    output_directory.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        prefix=f".{scope_id}-", suffix=".zip", dir=output_directory, delete=False
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        with zipfile.ZipFile(
            temporary_path,
            mode="w",
            compression=zipfile.ZIP_DEFLATED,
            compresslevel=9,
            strict_timestamps=True,
        ) as archive:
            archive.writestr(
                canonical_zip_info("measurement-manifest.json"), manifest_bytes
            )
            for digest in sorted(roles):
                archive.writestr(
                    canonical_zip_info(f"raw/{digest}.bin"),
                    available[digest].read_bytes(),
                )
        digest = file_sha256(temporary_path)
        destination = output_directory / f"{digest}.zip"
        if destination.exists():
            if destination.read_bytes() != temporary_path.read_bytes():
                raise ValueError(f"content-addressed output collision at {destination}")
            temporary_path.unlink()
        else:
            os.replace(temporary_path, destination)
        entry = {
            "placement_evidence_sha256": metadata["placement_evidence_sha256"],
            "performance_evidence_sha256": metadata["performance_evidence_sha256"],
        }
        validate_measurement_bundle(destination, scope_id, entry, evidence_reports)
        return destination
    except Exception:
        temporary_path.unlink(missing_ok=True)
        raise


def main() -> None:
    parser = argparse.ArgumentParser(
        description="build one deterministic content-addressed measurement ZIP"
    )
    parser.add_argument("--cache", required=True, type=Path)
    parser.add_argument("--raw-directory", required=True, type=Path)
    parser.add_argument("--output-directory", required=True, type=Path)
    args = parser.parse_args()
    try:
        output = build_measurement_bundle(
            args.cache, args.raw_directory, args.output_directory
        )
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error
    print(output)


if __name__ == "__main__":
    main()
