#!/usr/bin/env python3
"""Emit canonical cfetch KAT vectors for one ORT runtime/provider."""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path

import numpy as np
import onnxruntime as ort
from transformers import AutoTokenizer

from profile_data import DIMENSIONS, DOCUMENT_PREFIX, KAT_CASES, QUERY_PREFIX, SEQUENCE_BUCKETS

def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_vector(vector: np.ndarray) -> bytes:
    vector = np.asarray(vector, dtype=np.float32)
    if vector.shape != (DIMENSIONS,) or not np.all(np.isfinite(vector)):
        raise ValueError(f"invalid embedding output shape/content: {vector.shape}")
    maximum = np.max(np.abs(vector)).astype(np.float32)
    if maximum <= 0:
        raise ValueError("model emitted an all-zero vector")
    return np.rint(
        np.clip(vector / maximum * np.float32(127.0), -127.0, 127.0)
    ).astype(np.int8).tobytes()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--tokenizer", required=True, type=Path)
    parser.add_argument("--provider", default="CPUExecutionProvider")
    parser.add_argument(
        "--output",
        type=Path,
        help="write the full report here and print only its summary",
    )
    parser.add_argument("--batch-size", type=int, default=1)
    parser.add_argument(
        "--pad-to",
        type=int,
        help="pad every input to this fixed sequence bucket instead of batch-longest",
    )
    parser.add_argument(
        "--fix-session-dimensions",
        action="store_true",
        help="create one ORT session per bucket with batch/sequence dimensions overridden",
    )
    args = parser.parse_args()
    if args.batch_size < 1:
        raise SystemExit("--batch-size must be at least 1")

    tokenizer = AutoTokenizer.from_pretrained(
        args.tokenizer, local_files_only=True
    )
    texts = [
        (QUERY_PREFIX if case.kind == "query" else DOCUMENT_PREFIX) + case.text
        for case in KAT_CASES
    ]
    groups: dict[int, list[int]] = {}
    for index, text in enumerate(texts):
        token_count = len(
            tokenizer(
                text,
                add_special_tokens=True,
                padding=False,
                truncation=True,
                max_length=2048,
            )["input_ids"]
        )
        bucket = args.pad_to or next(
            (item for item in SEQUENCE_BUCKETS if token_count <= item),
            SEQUENCE_BUCKETS[-1],
        )
        groups.setdefault(bucket, []).append(index)
    def open_session(bucket: int | None) -> ort.InferenceSession:
        options = ort.SessionOptions()
        options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        options.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
        options.intra_op_num_threads = 1
        options.inter_op_num_threads = 1
        options.use_deterministic_compute = True
        # ORT's default U8S8 AVX2/AVX-512 fast path can saturate paired
        # products to 16 bits. V1 instead freezes the non-saturating U8U8
        # lowering so pre-VNNI and VNNI x86 CPUs share one reference.
        options.add_session_config_entry("session.x64quantprecision", "1")
        if bucket is not None:
            options.add_free_dimension_override_by_name("batch_size", 1)
            options.add_free_dimension_override_by_name("sequence_length", bucket)
        return ort.InferenceSession(
            str(args.model), options, providers=[args.provider]
        )

    dynamic_session = None if args.fix_session_dimensions else open_session(None)
    active_providers: set[str] = set()
    outputs: list[np.ndarray | None] = [None] * len(texts)
    for bucket, indices in groups.items():
        # Static-shape providers need one compiled session per bucket. Build
        # them sequentially here: retaining seven optimized copies of this
        # graph consumed roughly 8 GiB in the adverse probe and proves nothing
        # extra about the output contract.
        session = open_session(bucket) if args.fix_session_dimensions else dynamic_session
        assert session is not None
        active_providers.update(session.get_providers())
        for start in range(0, len(indices), args.batch_size):
            batch_indices = indices[start : start + args.batch_size]
            encoded = tokenizer(
                [texts[index] for index in batch_indices],
                padding="max_length",
                truncation=True,
                max_length=bucket,
                return_tensors="np",
            )
            batch = session.run(
                ["sentence_embedding"],
                {
                    "input_ids": np.asarray(encoded["input_ids"], dtype=np.int64),
                    "attention_mask": np.asarray(
                        encoded["attention_mask"], dtype=np.int64
                    ),
                },
            )[0]
            for index, vector in zip(batch_indices, batch, strict=True):
                outputs[index] = vector
    if any(vector is None for vector in outputs):
        raise RuntimeError("known-answer execution did not fill every vector")
    output_matrix = np.stack(outputs)
    vectors = [canonical_vector(vector) for vector in output_matrix]
    report = {
        "schema": 2,
        "model_sha256": sha256_file(args.model),
        "onnxruntime": importlib.metadata.version("onnxruntime"),
        "requested_provider": args.provider,
        "active_providers": sorted(active_providers),
        "batch_size": args.batch_size,
        "ort_intra_threads": 1,
        "ort_inter_threads": 1,
        "ort_execution_mode": "sequential",
        "ort_deterministic_compute": True,
        "ort_precise_qmm": True,
        "sequence_buckets": list(SEQUENCE_BUCKETS),
        "fixed_padding_override": args.pad_to,
        "fixed_session_dimensions": args.fix_session_dimensions,
        "graph_optimization": "ORT_ENABLE_ALL",
        "known_answers": [
            {
                "label": case.label,
                "kind": case.kind,
                "text": case.text,
                "expected_bucket": case.expected_bucket,
                "vector_sha256": hashlib.sha256(vector).hexdigest(),
                "vector_hex": vector.hex(),
            }
            for case, vector in zip(KAT_CASES, vectors, strict=True)
        ],
    }
    if args.output is None:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    else:
        if args.output.exists():
            raise SystemExit(f"refusing to overwrite existing report: {args.output}")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        )
        print(
            json.dumps(
                {
                    "report": str(args.output),
                    "model_sha256": report["model_sha256"],
                    "onnxruntime": report["onnxruntime"],
                    "active_providers": report["active_providers"],
                    "known_answer_vectors": len(report["known_answers"]),
                    "known_answer_vector_sha256": [
                        answer["vector_sha256"]
                        for answer in report["known_answers"]
                    ],
                },
                indent=2,
            )
        )


if __name__ == "__main__":
    main()
