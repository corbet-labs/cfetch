#!/usr/bin/env python3
"""Fetch a pinned public mirror of EmbeddingGemma and verify every source byte."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import sys
from typing import Sequence

if __package__:
    from .convert import ConversionError, verify_source_files
    from .manifest import (
        MODEL,
        MODEL_REVISION,
        PINNED_SOURCE_FILE_SHA256,
        SOURCE_MIRROR,
        SOURCE_MIRROR_REVISION,
    )
else:
    from convert import ConversionError, verify_source_files  # type: ignore[no-redef]
    from manifest import (  # type: ignore[no-redef]
        MODEL,
        MODEL_REVISION,
        PINNED_SOURCE_FILE_SHA256,
        SOURCE_MIRROR,
        SOURCE_MIRROR_REVISION,
    )


class SourceFetchError(ValueError):
    """The public source mirror could not be resolved to the pinned bytes."""


def fetch(output_dir: Path, cache_dir: Path) -> dict[str, object]:
    try:
        from huggingface_hub import HfApi, snapshot_download
    except ImportError as error:
        raise SourceFetchError("huggingface-hub is not installed") from error

    output_dir = output_dir.resolve()
    cache_dir = cache_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise SourceFetchError(f"source output must be absent or empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    cache_dir.mkdir(parents=True, exist_ok=True)

    # Resolve the requested Git commit first.  A mutable tag, branch, or
    # server-side substitution is not accepted even if individual files later
    # happen to match.
    try:
        info = HfApi().model_info(
            repo_id=SOURCE_MIRROR,
            revision=SOURCE_MIRROR_REVISION,
            # Do not send a cached or ambient credential to a public mirror.
            token=False,
        )
    except Exception as error:
        raise SourceFetchError("pinned mirror revision lookup failed") from error
    if info.sha != SOURCE_MIRROR_REVISION:
        raise SourceFetchError(
            "Hugging Face did not resolve the request to the pinned mirror commit"
        )
    try:
        snapshot = Path(
            snapshot_download(
                repo_id=SOURCE_MIRROR,
                repo_type="model",
                revision=SOURCE_MIRROR_REVISION,
                token=False,
                cache_dir=cache_dir,
                allow_patterns=sorted(PINNED_SOURCE_FILE_SHA256),
            )
        )
    except Exception as error:
        raise SourceFetchError("pinned public mirror snapshot download failed") from error
    for relative in sorted(PINNED_SOURCE_FILE_SHA256):
        source = snapshot.joinpath(*relative.split("/"))
        if not source.is_file():
            raise SourceFetchError(f"pinned snapshot omitted {relative}")
        destination = output_dir.joinpath(*relative.split("/"))
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
    verify_source_files(output_dir, PINNED_SOURCE_FILE_SHA256)
    return {
        "schema_version": 1,
        "model": MODEL,
        "revision": MODEL_REVISION,
        "acquisition": {
            "repository": SOURCE_MIRROR,
            "revision": SOURCE_MIRROR_REVISION,
            "mode": "public-byte-identical-mirror",
        },
        "files": dict(PINNED_SOURCE_FILE_SHA256),
    }


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--output-dir", required=True, type=Path)
    result.add_argument("--cache-dir", required=True, type=Path)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        report = fetch(args.output_dir, args.cache_dir)
    except (ConversionError, OSError, SourceFetchError) as error:
        print(f"pinned EmbeddingGemma source fetch refused: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
