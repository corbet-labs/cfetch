#!/usr/bin/env python3
"""Create the three distinct hex-encoded Ed25519 keys used by one OpenVINO package."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import tempfile
from typing import Sequence

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


SCOPE_ID_RE = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")


def generate_scope_keys(scope_ids: Sequence[str], output_directory: Path) -> Path:
    if len(scope_ids) != 3 or len(set(scope_ids)) != 3:
        raise ValueError("exactly three distinct scope IDs are required")
    if any(SCOPE_ID_RE.fullmatch(scope_id) is None for scope_id in scope_ids):
        raise ValueError("scope IDs must be canonical lowercase slugs")
    if output_directory.exists():
        raise ValueError("output directory must not already exist")

    output_directory.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(
        tempfile.mkdtemp(
            prefix=f".{output_directory.name}-", dir=output_directory.parent
        )
    )
    os.chmod(temporary, 0o700)
    try:
        rows: list[dict[str, str]] = []
        for scope_id in scope_ids:
            private_key = Ed25519PrivateKey.generate()
            private_bytes = private_key.private_bytes(
                encoding=serialization.Encoding.Raw,
                format=serialization.PrivateFormat.Raw,
                encryption_algorithm=serialization.NoEncryption(),
            )
            public_bytes = private_key.public_key().public_bytes(
                encoding=serialization.Encoding.Raw,
                format=serialization.PublicFormat.Raw,
            )
            filename = f"{scope_id}.key"
            descriptor = os.open(
                temporary / filename,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL,
                0o600,
            )
            with os.fdopen(descriptor, "wb") as destination:
                destination.write(private_bytes.hex().encode("ascii") + b"\n")
            rows.append(
                {
                    "scope_id": scope_id,
                    "attestation_public_key": public_bytes.hex(),
                    "attestation_private_key_file": filename,
                }
            )
        manifest = {"schema_version": 1, "keys": rows}
        manifest_path = temporary / "scope-keys.json"
        manifest_path.write_bytes(
            (
                json.dumps(
                    manifest,
                    ensure_ascii=False,
                    sort_keys=True,
                    separators=(",", ":"),
                )
                + "\n"
            ).encode("utf-8")
        )
        os.chmod(manifest_path, 0o600)
        os.replace(temporary, output_directory)
        return output_directory / "scope-keys.json"
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument(
        "--scope-id",
        action="append",
        required=True,
        help="repeat exactly three times, in NPU/GPU/CPU package order",
    )
    result.add_argument("--output-directory", required=True, type=Path)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        manifest = generate_scope_keys(args.scope_id, args.output_directory)
    except (OSError, ValueError) as error:
        print(f"OpenVINO scope-key generation refused: {error}", file=os.sys.stderr)
        return 1
    print(
        json.dumps(
            {"schema_version": 1, "public_manifest": str(manifest)},
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
