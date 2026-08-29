#!/usr/bin/env python3
"""Create a deterministic content-addressed tar.gz from a package directory."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import tarfile
import tempfile
from typing import Sequence

if __package__:
    from .legal import LegalError, validate_embedded_legal
else:
    from legal import LegalError, validate_embedded_legal  # type: ignore[no-redef]


PREFIX_RE = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
MAX_FILES = 100_000
MAX_BYTES = 8 * 1024 * 1024 * 1024


class ArchiveError(ValueError):
    """A directory cannot be represented as a safe deterministic package."""


def _entries(root: Path) -> list[Path]:
    entries: list[Path] = []
    total_bytes = 0
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        current = Path(directory)
        directory_names.sort()
        file_names.sort()
        for name in directory_names:
            path = current / name
            if path.is_symlink():
                raise ArchiveError(f"package directory contains symlink {path}")
        for name in file_names:
            path = current / name
            if path.is_symlink() or not path.is_file():
                raise ArchiveError(f"package entry is not a regular file: {path}")
            size = path.stat().st_size
            total_bytes += size
            if total_bytes > MAX_BYTES:
                raise ArchiveError(f"package exceeds the {MAX_BYTES}-byte limit")
            entries.append(path)
            if len(entries) > MAX_FILES:
                raise ArchiveError(f"package exceeds the {MAX_FILES}-file limit")
    entries.sort(key=lambda path: path.relative_to(root).as_posix())
    return entries


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def create_archive(
    root: Path,
    output_dir: Path,
    prefix: str,
    require_gemma_legal: bool = False,
) -> tuple[Path, str]:
    root = root.resolve()
    output_dir = output_dir.resolve()
    if not root.is_dir() or root.is_symlink():
        raise ArchiveError("archive input must be a regular directory")
    if PREFIX_RE.fullmatch(prefix) is None or len(prefix) > 128:
        raise ArchiveError("archive prefix must be a canonical lowercase dash slug")
    if require_gemma_legal:
        legal_root = root if (root / "NOTICE").exists() else root / "artifact"
        try:
            validate_embedded_legal(legal_root)
        except LegalError as error:
            raise ArchiveError(f"Gemma legal payload is invalid: {error}") from error
    entries = _entries(root)
    if not entries:
        raise ArchiveError("archive input directory is empty")
    output_dir.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            prefix=f".{prefix}-", suffix=".tar.gz.tmp", dir=output_dir, delete=False
        ) as temporary:
            temporary_path = Path(temporary.name)
            with gzip.GzipFile(
                filename="", mode="wb", fileobj=temporary, mtime=0
            ) as compressed:
                with tarfile.open(
                    fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT
                ) as archive:
                    for path in entries:
                        relative = path.relative_to(root).as_posix()
                        metadata = path.stat()
                        info = tarfile.TarInfo(relative)
                        info.uid = 0
                        info.gid = 0
                        info.uname = ""
                        info.gname = ""
                        info.mtime = 0
                        # Parent directories are implicit.  Release staging
                        # accepts regular tar members only, so emitting
                        # directory entries would make an otherwise valid
                        # package impossible to install.
                        info.type = tarfile.REGTYPE
                        info.mode = 0o755 if metadata.st_mode & 0o111 else 0o644
                        info.size = metadata.st_size
                        with path.open("rb") as source:
                            archive.addfile(info, source)
            temporary.flush()
            os.fsync(temporary.fileno())
        digest = _sha256(temporary_path)
        output = output_dir / f"{prefix}-{digest}.tar.gz"
        if output.exists():
            raise ArchiveError(f"refusing to overwrite existing archive {output}")
        temporary_path.replace(output)
        temporary_path = None
        return output, digest
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--directory", required=True, type=Path)
    result.add_argument("--output-dir", required=True, type=Path)
    result.add_argument("--prefix", required=True)
    result.add_argument("--require-gemma-legal", action="store_true")
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        path, digest = create_archive(
            args.directory,
            args.output_dir,
            args.prefix,
            args.require_gemma_legal,
        )
    except (ArchiveError, OSError) as error:
        print(f"content-addressed archive refused: {error}")
        return 1
    print(
        json.dumps(
            {"schema_version": 1, "path": str(path), "sha256": digest},
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
