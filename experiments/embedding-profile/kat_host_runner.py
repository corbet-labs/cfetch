#!/usr/bin/env python3
"""Reproduce the network-major-1 runtime KAT on one host with ONNX Runtime.

This is the community harness behind third-party hardware reports on the
certification issue. It downloads nothing and trusts nothing: it reads the
released model bundle, replays the recorded session contract (deterministic
compute, intra/inter threads 1/1, sequential execution, ORT_ENABLE_ALL,
batch 1, per-case bucket padding, frozen tokenizer with the profile
prefixes), applies the canonical signed max-abs RNE INT8x768 codec, and
compares SHA-256 against the bundle's schema-2 known answers by default.

Requires the bundle unpacked locally plus `onnxruntime` and `tokenizers`;
neither is imported at module load, so unit tests run without them.

Pitfalls this runner refuses to hide:

- A GPU execution-provider factory failure silently leaves the session on
  CPU. `--require-provider` asserts the requested provider is ACTIVE before
  any byte comparison runs.
- `--strict` sets `session.disable_cpu_ep_fallback`, so any CPU remainder
  fails session creation instead of quietly joining the computation.
- `--baseline-schema1` compares against the superseded schema-1 known
  answers embedded in `model.onnx.build.json`. That baseline is diagnostic
  only: it exists to detect hosts that deterministically reproduce the
  old saturating-kernel bytes (a real rejected-producer class).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

import numpy as np

QUERY_PREFIX = "task: search result | query: "
DOCUMENT_PREFIX = "title: none | text: "
PAD_ID = 0  # Gemma <pad>
DIMENSIONS = 768


def canonical_int8(vec: np.ndarray) -> np.ndarray:
    """l2-source-output-then-i8-maxabs-rne-storage codec.

    L2-normalizes, scales by 127/max-abs, rounds to nearest-even, and clips
    to [-127, 127]. Never emits -128; the max-abs element lands on +/-127.
    """
    vec = np.asarray(vec, dtype=np.float64)
    norm = np.linalg.norm(vec)
    if norm > 0:
        vec = vec / norm
    max_abs = np.max(np.abs(vec))
    if max_abs <= 0:
        raise ValueError("cannot encode a zero vector")
    q = np.rint(vec * (127.0 / max_abs))
    return np.clip(q, -127, 127).astype(np.int8)


def pad_to_bucket(ids: list[int], bucket: int, pad_id: int = PAD_ID):
    """Right-pads token ids to the fixed bucket; returns (input_ids, mask)."""
    if len(ids) > bucket:
        raise ValueError(f"{len(ids)} tokens exceed bucket {bucket}")
    pad = bucket - len(ids)
    input_ids = np.array([[*ids, *[pad_id] * pad]], dtype=np.int64)
    attention_mask = np.array([[*[1] * len(ids), *[0] * pad]], dtype=np.int64)
    return input_ids, attention_mask


def prefixed_text(kind: str, text: str) -> str:
    if kind == "query":
        return QUERY_PREFIX + text
    if kind == "document":
        return DOCUMENT_PREFIX + text
    raise ValueError(f"unknown KAT kind: {kind!r}")


def load_cases(bundle: Path, schema1: bool) -> list[dict]:
    if schema1:
        doc = json.loads((bundle / "model.onnx.build.json").read_text(encoding="utf-8"))
    else:
        doc = json.loads((bundle / "runtime-kat.json").read_text(encoding="utf-8"))
    cases = doc["known_answers"]
    if len(cases) != 11:
        raise ValueError(f"expected 11 known answers, found {len(cases)}")
    return cases


def compare_vector(q: np.ndarray, case: dict) -> tuple[bool, int]:
    digest = hashlib.sha256(q.tobytes()).hexdigest()
    expected = np.frombuffer(bytes.fromhex(case["vector_hex"]), dtype=np.int8)
    return digest == case["vector_sha256"], int(np.count_nonzero(q != expected))


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("bundle", type=Path, help="unpacked bundle directory containing model.onnx")
    parser.add_argument("provider", help="e.g. CPUExecutionProvider, DmlExecutionProvider, CUDAExecutionProvider")
    parser.add_argument("--strict", action="store_true", help="disable CPU EP fallback (session creation fails on any CPU remainder)")
    parser.add_argument("--require-provider", action="store_true", help="fail if the requested provider is not active (catches silent CPU fallback)")
    parser.add_argument("--no-u8s8", action="store_true", help="control: session.qdqisint8allowed=0")
    parser.add_argument("--baseline-schema1", action="store_true", help="compare against the superseded schema-1 known answers (diagnostic)")
    args = parser.parse_args(argv)

    import onnxruntime as ort
    from tokenizers import Tokenizer

    kat_settings = json.loads((args.bundle / "runtime-kat.json").read_text(encoding="utf-8"))
    if not args.baseline_schema1 and ort.__version__ != kat_settings["onnxruntime"]:
        print(f"warning: bundle pinned ORT {kat_settings['onnxruntime']}, running {ort.__version__}", file=sys.stderr)

    so = ort.SessionOptions()
    so.use_deterministic_compute = True
    so.intra_op_num_threads = int(kat_settings["ort_intra_threads"])
    so.inter_op_num_threads = int(kat_settings["ort_inter_threads"])
    so.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    so.log_severity_level = 3
    if args.strict:
        so.add_session_config_entry("session.disable_cpu_ep_fallback", "1")
    if args.no_u8s8:
        so.add_session_config_entry("session.qdqisint8allowed", "0")

    sess = ort.InferenceSession(str(args.bundle / "model.onnx"), sess_options=so, providers=[args.provider])
    active = sess.get_providers()
    print(f"ort={ort.__version__} requested={args.provider} active={active}")
    if args.require_provider and args.provider not in active:
        print(f"REFUSED: {args.provider} is not active — this session would silently measure a different route", file=sys.stderr)
        return 2

    tok = Tokenizer.from_file(str(args.bundle / "tokenizer.json"))
    cases = load_cases(args.bundle, schema1=args.baseline_schema1)

    passed = 0
    deterministic = True
    for case in cases:
        enc = tok.encode(prefixed_text(case["kind"], case["text"]))
        bucket = int(case["expected_bucket"])
        input_ids, attention_mask = pad_to_bucket(enc.ids, bucket)
        feed = {"input_ids": input_ids, "attention_mask": attention_mask}
        r1 = canonical_int8(sess.run(["sentence_embedding"], feed)[0][0])
        r2 = canonical_int8(sess.run(["sentence_embedding"], feed)[0][0])
        deterministic = deterministic and np.array_equal(r1, r2)
        ok, diff = compare_vector(r1, case)
        passed += ok
        print(f"{'PASS' if ok else 'FAIL'}  {case['label']:<20} bucket={bucket:<5} diff={diff}/768")

    baseline = "schema-1 (SUPERSEDED, diagnostic)" if args.baseline_schema1 else "corrected v1"
    print(f"\n{passed}/11 exact vs {baseline} | deterministic={deterministic}")
    return 0 if passed == len(cases) else 1


if __name__ == "__main__":
    sys.exit(main())
