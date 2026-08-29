#!/usr/bin/env python3
"""Fetch only the pinned gated EmbeddingGemma files and verify every byte."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import sys
from typing import Sequence

if __package__:
    from .convert import ConversionError, verify_source_files
    from .manifest import MODEL, MODEL_REVISION, PINNED_SOURCE_FILE_SHA256
else:
    from convert import ConversionError, verify_source_files  # type: ignore[no-redef]
    from manifest import (  # type: ignore[no-redef]
        MODEL,
        MODEL_REVISION,
        PINNED_SOURCE_FILE_SHA256,
    )


class SourceFetchError(ValueError):
    """The gated source could not be resolved to the pinned immutable bytes."""


def fetch(output_dir: Path, cache_dir: Path, token_environment: str) -> dict[str, object]:
    try:
        from huggingface_hub import HfApi, snapshot_download
    except ImportError as error:
        raise SourceFetchError("huggingface-hub is not installed") from error

    token = os.environ.get(token_environment)
    if not token:
        raise SourceFetchError(
            f"gated source credential is absent from {token_environment}"
        )
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
            repo_id=MODEL,
            revision=MODEL_REVISION,
            token=token,
        )
    except Exception as error:
        raise SourceFetchError("pinned upstream revision lookup failed") from error
    if info.sha != MODEL_REVISION:
        raise SourceFetchError(
            "Hugging Face did not resolve the request to the pinned model commit"
        )
    try:
        snapshot = Path(
            snapshot_download(
                repo_id=MODEL,
                repo_type="model",
                revision=MODEL_REVISION,
                token=token,
                cache_dir=cache_dir,
                allow_patterns=sorted(PINNED_SOURCE_FILE_SHA256),
            )
        )
    except Exception as error:
        raise SourceFetchError("pinned gated snapshot download failed") from error
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
        "files": dict(PINNED_SOURCE_FILE_SHA256),
    }


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--output-dir", required=True, type=Path)
    result.add_argument("--cache-dir", required=True, type=Path)
    result.add_argument("--token-environment", default="HF_TOKEN")
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        report = fetch(args.output_dir, args.cache_dir, args.token_environment)
    except (ConversionError, OSError, SourceFetchError) as error:
        # The credential is deliberately never interpolated into diagnostics.
        print(f"pinned EmbeddingGemma source fetch refused: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
