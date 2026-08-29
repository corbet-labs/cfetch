"""Candidate-only deterministic smoke for the pinned EmbeddingGemma Q8 GGUF."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import re
import struct
import subprocess
from collections.abc import Sequence
from pathlib import Path

LLAMA_CPP_REVISION = "b95502ba9aa0eb73a2f4fc8878d7fbe6a847a0b9"
ARTIFACT_REPOSITORY = "ggml-org/embeddinggemma-300M-GGUF"
ARTIFACT_REVISION = "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12"
ARTIFACT_FILE = "embeddinggemma-300M-Q8_0.gguf"
ARTIFACT_SHA256 = "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63"
TARGET_MODEL = "google/embeddinggemma-300m"
TARGET_MODEL_REVISION = "57c266a740f537b4dc058e1b0cda161fd15afa75"
TARGET_PROFILE_MANIFEST_SHA256 = (
    "59210a333494f788eb8e607fe38cabb6af1a7aa7cdf604ddf52e3fa6004b5afb"
)
DIMENSIONS = 768
QUERY_PREFIX = "task: search result | query: "
DOCUMENT_PREFIX = "title: none | text: "
SEPARATOR = "<#cfetch-gguf-candidate-separator#>"
PROMPTS = (
    QUERY_PREFIX + "Which planet is known as the Red Planet?",
    DOCUMENT_PREFIX
    + "Mars is known as the Red Planet because iron minerals in its soil oxidize.",
)


def f32(value: float) -> float:
    """Round one operation result to IEEE-754 binary32 like the Rust codec."""

    return struct.unpack("=f", struct.pack("=f", value))[0]


def canonical_i8(vector: Sequence[float]) -> bytes:
    """Apply cfetch's f32 max-absolute, ties-to-even signed INT8 codec."""

    values = [f32(value) for value in vector]
    if len(values) != DIMENSIONS:
        raise ValueError(f"expected {DIMENSIONS} components, found {len(values)}")
    if not all(math.isfinite(value) for value in values):
        raise ValueError("embedding contains a non-finite component")
    maximum = max(abs(value) for value in values)
    if maximum <= 0.0:
        raise ValueError("embedding is all zero")

    encoded = bytearray()
    for value in values:
        divided = f32(value / maximum)
        scaled = f32(divided * f32(127.0))
        quantized = min(127, max(-127, round(scaled)))
        encoded.append(quantized & 0xFF)
    return bytes(encoded)


def codec_self_test() -> None:
    fixture = [
        1.0,
        -1.0,
        f32(0.5 / 127.0),
        f32(1.5 / 127.0),
        f32(-0.5 / 127.0),
        f32(-1.5 / 127.0),
        *([0.0] * (DIMENSIONS - 6)),
    ]
    expected_signed = [127, -127, 0, 2, 0, -2, *([0] * (DIMENSIONS - 6))]
    expected = bytes(value & 0xFF for value in expected_signed)
    if canonical_i8(fixture) != expected:
        raise RuntimeError("canonical INT8 codec self-test failed")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def executable_version(executable: Path) -> str:
    process = subprocess.run(
        [str(executable), "--version"],
        check=True,
        capture_output=True,
        text=True,
    )
    version = "\n".join(
        part for part in (process.stdout, process.stderr) if part
    ).strip()
    if f"commit {LLAMA_CPP_REVISION[:7]}" not in version:
        raise RuntimeError(
            "llama-embedding does not attest pinned llama.cpp revision "
            f"{LLAMA_CPP_REVISION}"
        )
    return version


def command(executable: Path, model: Path, threads: int) -> list[str]:
    return [
        str(executable),
        "-m",
        str(model),
        "--offline",
        "--threads",
        str(threads),
        "--threads-batch",
        str(threads),
        "--ctx-size",
        "2048",
        "--batch-size",
        "2048",
        "--ubatch-size",
        "2048",
        "--no-warmup",
        "--embd-output-format",
        "json",
        "--embd-separator",
        SEPARATOR,
        "--log-verbosity",
        "3",
        "--prompt",
        SEPARATOR.join(PROMPTS),
    ]


def parse_vectors(stdout: str) -> list[list[float]]:
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError("llama-embedding returned invalid JSON") from error
    if not isinstance(payload, dict) or not isinstance(payload.get("data"), list):
        raise TypeError("llama-embedding response has no data array")
    entries = payload["data"]
    if len(entries) != len(PROMPTS):
        raise RuntimeError(f"expected {len(PROMPTS)} embeddings, found {len(entries)}")
    if any(not isinstance(entry, dict) for entry in entries):
        raise RuntimeError("llama-embedding data entry is not an object")
    indices = [entry.get("index") for entry in entries]
    if indices != list(range(len(PROMPTS))):
        raise RuntimeError(f"unexpected embedding indices: {indices!r}")

    vectors: list[list[float]] = []
    for entry in entries:
        embedding = entry.get("embedding")
        if not isinstance(embedding, list):
            raise TypeError("llama-embedding data entry has no embedding array")
        if any(
            isinstance(value, bool) or not isinstance(value, (int, float))
            for value in embedding
        ):
            raise RuntimeError("embedding contains a non-numeric component")
        vectors.append([f32(float(value)) for value in embedding])
    return vectors


def placement_evidence(stderr: str) -> tuple[str, list[str]]:
    lines = [line.strip() for line in stderr.splitlines() if "system_info:" in line]
    if len(lines) != 1:
        raise RuntimeError(
            f"expected one llama.cpp system_info line, found {len(lines)}"
        )
    line = lines[0]
    enabled = sorted(set(re.findall(r"\b([A-Z][A-Z0-9_]*)\s*=\s*1\b", line)))
    architecture = platform.machine().lower()
    if architecture in {"x86_64", "amd64"}:
        simd = sorted(set(enabled).intersection({"AVX", "AVX2", "AVX512"}))
    elif architecture in {"aarch64", "arm64"}:
        simd = sorted(set(enabled).intersection({"NEON", "SVE", "SVE2"}))
    else:
        simd = sorted(
            set(enabled).intersection(
                {"AVX", "AVX2", "AVX512", "NEON", "SVE", "SVE2", "VSX"}
            )
        )
    if not simd:
        raise RuntimeError(f"no SIMD CPU feature is enabled in: {line}")
    return line, simd


def run_once(
    executable: Path, model: Path, threads: int
) -> tuple[list[list[float]], str]:
    process = subprocess.run(
        command(executable, model, threads),
        check=True,
        capture_output=True,
        text=True,
    )
    return parse_vectors(process.stdout), process.stderr


def vector_evidence(
    role: str, vector: Sequence[float], encoded: bytes
) -> dict[str, object]:
    norm = math.sqrt(sum(float(value) * float(value) for value in vector))
    return {
        "canonical_int8_sha256": hashlib.sha256(encoded).hexdigest(),
        "dimensions": len(vector),
        "finite": all(math.isfinite(value) for value in vector),
        "float_nonzero": sum(value != 0.0 for value in vector),
        "l2_norm": norm,
        "role": role,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run a candidate-only deterministic SIMD CPU smoke for the pinned "
            "EmbeddingGemma Q8 GGUF. This never performs or claims backend admission."
        )
    )
    parser.add_argument("--llama-embedding", type=Path, required=True)
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument(
        "--threads",
        type=int,
        default=min(16, os.cpu_count() or 1),
        help="llama.cpp CPU thread count (default: min(16, detected CPUs))",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.threads < 1:
        raise SystemExit("--threads must be at least 1")
    executable = args.llama_embedding.resolve()
    model = args.model.resolve()
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise SystemExit(f"llama-embedding is not executable: {executable}")
    if not model.is_file():
        raise SystemExit(f"GGUF model does not exist: {model}")

    model_digest = sha256_file(model)
    if model_digest != ARTIFACT_SHA256:
        raise SystemExit(
            f"GGUF SHA-256 mismatch: expected {ARTIFACT_SHA256}, found {model_digest}"
        )
    codec_self_test()
    version = executable_version(executable)

    first_vectors, first_logs = run_once(executable, model, args.threads)
    repeat_vectors, repeat_logs = run_once(executable, model, args.threads)
    system_info, simd_features = placement_evidence(first_logs)
    placement_evidence(repeat_logs)

    first_encoded = [canonical_i8(vector) for vector in first_vectors]
    repeat_encoded = [canonical_i8(vector) for vector in repeat_vectors]
    repeatable = first_encoded == repeat_encoded
    if not repeatable:
        raise SystemExit("canonical INT8 output changed on the immediate repeat")

    evidence = {
        "admission_state": "candidate-not-admitted",
        "artifact_file": ARTIFACT_FILE,
        "artifact_lineage_to_target_revision_proven": False,
        "artifact_repository": ARTIFACT_REPOSITORY,
        "artifact_revision": ARTIFACT_REVISION,
        "artifact_sha256": model_digest,
        "candidate_only": True,
        "codec": "signed-int8x768-maxabs-rne",
        "codec_self_test": True,
        "global_all_pairs_admitted": False,
        "llama_cpp_revision": LLAMA_CPP_REVISION,
        "llama_cpp_version": version,
        "profile_id": "cfetch-embedding-v1",
        "prompts": {
            "document_prefix": DOCUMENT_PREFIX,
            "query_prefix": QUERY_PREFIX,
        },
        "same_scope_canonical_int8_repeatable": repeatable,
        "schema_version": 1,
        "simd_features": simd_features,
        "simd_placement_evidence": True,
        "system_info": system_info,
        "target_model": TARGET_MODEL,
        "target_model_revision": TARGET_MODEL_REVISION,
        "target_profile_manifest_sha256": TARGET_PROFILE_MANIFEST_SHA256,
        "threads": args.threads,
        "vectors": [
            vector_evidence("query", first_vectors[0], first_encoded[0]),
            vector_evidence("document", first_vectors[1], first_encoded[1]),
        ],
    }
    print(json.dumps(evidence, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
