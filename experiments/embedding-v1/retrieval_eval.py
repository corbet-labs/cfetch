#!/usr/bin/env python3
"""Compare candidate and source cfetch INT8 vectors on pinned SciFact retrieval."""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import importlib.metadata
import json
from pathlib import Path

import numpy as np
import onnxruntime as ort
from datasets import load_dataset
from transformers import AutoTokenizer

from profile_data import DOCUMENT_PREFIX, QUERY_PREFIX, SEQUENCE_BUCKETS

DATASET = "mteb/scifact"
DATASET_REVISION = "cf10ab6856b15b0e670ef8ae5dae4e266c12d035"
# Frozen before inspecting the full 128-sample candidate. A release artifact
# may not lose more than one absolute point of top-rank quality or more than
# half a point of broad recall. Top-10 overlap is retained as a diagnostic,
# not a gate: a different relevant document is not a quality regression.
MAX_ABSOLUTE_REGRESSION = {
    "ndcg_at_10": 0.01,
    "recall_at_100": 0.005,
    "mrr_at_10": 0.01,
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def cache_path(
    cache_dir: Path,
    model_sha256: str,
    runtime_version: str,
    batch_size: int,
    workers: int,
    max_tokens: int,
) -> Path:
    return cache_dir / (
        f"scifact-seqp2t1-{DATASET_REVISION[:12]}-{model_sha256}-"
        f"ort{runtime_version}-b{batch_size}-w{workers}-t{max_tokens}.npz"
    )


def load_cache(path: Path, expected: dict[str, object]) -> tuple[np.ndarray, np.ndarray]:
    with np.load(path, allow_pickle=False) as cached:
        metadata = json.loads(str(cached["metadata"].item()))
        if metadata != expected:
            raise ValueError(f"retrieval cache metadata mismatch in {path}")
        queries = np.asarray(cached["queries"], dtype=np.int8)
        documents = np.asarray(cached["documents"], dtype=np.int8)
    if (
        queries.shape != (expected["queries"], 768)
        or documents.shape != (expected["documents"], 768)
    ):
        raise ValueError(f"retrieval cache has invalid vector shapes in {path}")
    return queries, documents


def save_cache(
    path: Path,
    metadata: dict[str, object],
    queries: np.ndarray,
    documents: np.ndarray,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("wb") as output:
        np.savez(output, metadata=json.dumps(metadata, sort_keys=True), queries=queries, documents=documents)
    temporary.replace(path)


def write_checkpoint(path: Path, metadata: dict[str, object], done: int) -> None:
    state_path = path.with_suffix(path.suffix + ".json")
    temporary = state_path.with_suffix(state_path.suffix + ".tmp")
    temporary.write_text(
        json.dumps({"metadata": metadata, "done": done}, sort_keys=True) + "\n"
    )
    temporary.replace(state_path)


def session(path: Path) -> ort.InferenceSession:
    options = ort.SessionOptions()
    options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    options.intra_op_num_threads = 1
    options.inter_op_num_threads = 1
    return ort.InferenceSession(str(path), options, providers=["CPUExecutionProvider"])


def run_batch(
    model: ort.InferenceSession,
    input_ids: np.ndarray,
    attention_mask: np.ndarray,
) -> np.ndarray:
    return model.run(
        ["sentence_embedding"],
        {"input_ids": input_ids, "attention_mask": attention_mask},
    )[0]


def embed(
    model: ort.InferenceSession,
    tokenizer: object,
    texts: list[str],
    batch_size: int,
    workers: int,
    max_tokens: int,
    checkpoint: Path | None = None,
    checkpoint_metadata: dict[str, object] | None = None,
) -> np.ndarray:
    # v1 assigns a fixed sequence shape from each input's own token count and
    # admits exactly one item per inference batch. Independent batch-one calls
    # may run concurrently in this CPU-only quality evaluation.
    tokenized = tokenizer(
        texts,
        add_special_tokens=True,
        padding=False,
        truncation=True,
        max_length=max_tokens,
    )["input_ids"]
    buckets = tuple(bucket for bucket in SEQUENCE_BUCKETS if bucket <= max_tokens)
    if not buckets or buckets[-1] != max_tokens:
        buckets = (*buckets, max_tokens)
    groups: dict[int, list[int]] = {bucket: [] for bucket in buckets}
    for index, tokens in enumerate(tokenized):
        bucket = next((item for item in buckets if len(tokens) <= item), buckets[-1])
        groups[bucket].append(index)
    work = [
        (bucket, indices[start : start + batch_size])
        for bucket, indices in groups.items()
        for start in range(0, len(indices), batch_size)
    ]
    start_at = 0
    if checkpoint is None:
        outputs = np.empty((len(texts), 768), dtype=np.int8)
    else:
        if checkpoint_metadata is None:
            raise ValueError("checkpoint metadata is required with a checkpoint path")
        checkpoint.parent.mkdir(parents=True, exist_ok=True)
        state_path = checkpoint.with_suffix(checkpoint.suffix + ".json")
        if checkpoint.exists() != state_path.exists():
            raise ValueError(f"incomplete retrieval checkpoint metadata at {checkpoint}")
        if checkpoint.exists():
            state = json.loads(state_path.read_text())
            if state.get("metadata") != checkpoint_metadata:
                raise ValueError(f"retrieval checkpoint metadata mismatch at {checkpoint}")
            start_at = int(state.get("done", -1))
            if not 0 <= start_at <= len(work):
                raise ValueError(f"invalid retrieval checkpoint offset at {checkpoint}")
            outputs = np.lib.format.open_memmap(checkpoint, mode="r+")
            if outputs.shape != (len(texts), 768) or outputs.dtype != np.int8:
                raise ValueError(f"invalid retrieval checkpoint shape at {checkpoint}")
            print(f"resuming {checkpoint.name} at batch {start_at}/{len(work)}", flush=True)
        else:
            outputs = np.lib.format.open_memmap(
                checkpoint, mode="w+", dtype=np.int8, shape=(len(texts), 768)
            )
            write_checkpoint(checkpoint, checkpoint_metadata, 0)
    completed = sum(len(indices) for _, indices in work[:start_at])
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
        for wave_start in range(start_at, len(work), workers):
            pending = []
            for work_index in range(wave_start, min(wave_start + workers, len(work))):
                bucket, indices = work[work_index]
                encoded = tokenizer(
                    [texts[index] for index in indices],
                    padding="max_length",
                    truncation=True,
                    max_length=bucket,
                    return_tensors="np",
                )
                future = executor.submit(
                    run_batch,
                    model,
                    np.asarray(encoded["input_ids"], dtype=np.int64),
                    np.asarray(encoded["attention_mask"], dtype=np.int64),
                )
                pending.append((indices, future))
            previous = completed
            for indices, future in pending:
                batch = canonical_i8(future.result())
                for index, vector in zip(indices, batch, strict=True):
                    outputs[index] = vector
                completed += len(indices)
            done = min(wave_start + workers, len(work))
            if checkpoint is not None:
                outputs.flush()
                write_checkpoint(checkpoint, checkpoint_metadata, done)
            if completed // 256 > previous // 256 or completed == len(texts):
                print(f"embedded {completed}/{len(texts)}", flush=True)
    return np.asarray(outputs)


def canonical_i8(vectors: np.ndarray) -> np.ndarray:
    vectors = np.asarray(vectors, dtype=np.float32)
    if not np.all(np.isfinite(vectors)):
        raise ValueError("model emitted a non-finite component")
    maximum = np.max(np.abs(vectors), axis=1, keepdims=True)
    if np.any(maximum <= 0):
        raise ValueError("model emitted an all-zero vector")
    return np.rint(np.clip(vectors / maximum * np.float32(127.0), -127.0, 127.0)).astype(
        np.int8
    )


def scores(query: np.ndarray, documents: np.ndarray) -> np.ndarray:
    # This is the same cosine over signed INT8 components that cfetch ranks.
    q = query.astype(np.float64)
    d = documents.astype(np.float64)
    q /= np.linalg.norm(q, axis=1, keepdims=True)
    d /= np.linalg.norm(d, axis=1, keepdims=True)
    return q @ d.T


def metrics(
    similarities: np.ndarray,
    query_ids: list[str],
    document_ids: list[str],
    qrels: dict[str, set[str]],
) -> dict[str, float]:
    document_index = {document_id: index for index, document_id in enumerate(document_ids)}
    ndcg10: list[float] = []
    recall100: list[float] = []
    mrr10: list[float] = []
    for row, query_id in enumerate(query_ids):
        relevant = {document_index[item] for item in qrels[query_id] if item in document_index}
        order = np.argsort(-similarities[row], kind="stable")[:100]
        gains = np.asarray([1.0 if index in relevant else 0.0 for index in order[:10]])
        discounts = 1.0 / np.log2(np.arange(2, 2 + len(gains)))
        dcg = float(np.sum(gains * discounts))
        ideal = float(np.sum(discounts[: min(len(relevant), 10)]))
        ndcg10.append(dcg / ideal if ideal else 0.0)
        recall100.append(len(relevant.intersection(order)) / len(relevant) if relevant else 0.0)
        first = next((rank for rank, index in enumerate(order[:10], 1) if index in relevant), None)
        mrr10.append(1.0 / first if first is not None else 0.0)
    return {
        "ndcg_at_10": float(np.mean(ndcg10)),
        "recall_at_100": float(np.mean(recall100)),
        "mrr_at_10": float(np.mean(mrr10)),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--tokenizer", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument(
        "--batch-size",
        type=int,
        choices=(1,),
        default=1,
        help="v1 inference batch is immutable; this option exists only to stamp the report",
    )
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--max-tokens", type=int, default=512)
    parser.add_argument(
        "--cache-dir",
        type=Path,
        help="reuse canonical source/candidate vectors by model digest for quantizer comparisons",
    )
    args = parser.parse_args()
    if args.batch_size != 1:
        raise SystemExit("cfetch embedding v1 retrieval evaluation requires --batch-size 1")
    if args.workers < 1:
        raise SystemExit("--workers must be at least 1")

    qrel_rows = load_dataset(DATASET, revision=DATASET_REVISION, split="test")
    corpus = load_dataset(DATASET, "corpus", revision=DATASET_REVISION, split="corpus")
    queries = load_dataset(DATASET, "queries", revision=DATASET_REVISION, split="queries")
    qrels: dict[str, set[str]] = {}
    for row in qrel_rows:
        if row["score"] > 0:
            qrels.setdefault(row["query-id"], set()).add(row["corpus-id"])
    query_text = {row["_id"]: row["text"] for row in queries}
    query_ids = sorted(qrels, key=lambda item: int(item))
    document_ids = [row["_id"] for row in corpus]
    query_inputs = [QUERY_PREFIX + query_text[query_id] for query_id in query_ids]
    document_inputs = [
        DOCUMENT_PREFIX + ((row["title"] + "\n") if row["title"] else "") + row["text"]
        for row in corpus
    ]
    tokenizer = AutoTokenizer.from_pretrained(args.tokenizer, local_files_only=True)
    runtime_version = importlib.metadata.version("onnxruntime")

    results: dict[str, object] = {}
    encoded: dict[str, tuple[np.ndarray, np.ndarray]] = {}
    for name, path in (("source", args.source), ("candidate", args.candidate)):
        model_sha256 = sha256_file(path)
        cache_metadata = {
            "schema": 1,
            "dataset": DATASET,
            "dataset_revision": DATASET_REVISION,
            "model_sha256": model_sha256,
            "onnxruntime": runtime_version,
            "graph_optimization": "ORT_ENABLE_ALL (FastEmbed Level 3)",
            "batch_size": args.batch_size,
            "evaluation_workers": args.workers,
            "profile_inference_batch_size": 1,
            "ort_intra_threads": 1,
            "sequence_buckets": list(SEQUENCE_BUCKETS),
            "max_tokens": args.max_tokens,
            "queries": len(query_ids),
            "documents": len(document_ids),
            "vector_encoding": "signed-int8x768",
        }
        cached_path = (
            cache_path(
                args.cache_dir,
                model_sha256,
                runtime_version,
                args.batch_size,
                args.workers,
                args.max_tokens,
            )
            if args.cache_dir is not None
            else None
        )
        if cached_path is not None and cached_path.exists():
            print(f"loading cached {name} vectors: {cached_path}", flush=True)
            query_vectors, document_vectors = load_cache(cached_path, cache_metadata)
        else:
            print(f"evaluating {name}: {path}", flush=True)
            model = session(path)
            query_checkpoint = (
                cached_path.with_name(cached_path.name + ".queries.partial.npy")
                if cached_path is not None
                else None
            )
            document_checkpoint = (
                cached_path.with_name(cached_path.name + ".documents.partial.npy")
                if cached_path is not None
                else None
            )
            query_vectors = embed(
                model,
                tokenizer,
                query_inputs,
                args.batch_size,
                args.workers,
                args.max_tokens,
                query_checkpoint,
                {**cache_metadata, "split": "queries"},
            )
            document_vectors = embed(
                model,
                tokenizer,
                document_inputs,
                args.batch_size,
                args.workers,
                args.max_tokens,
                document_checkpoint,
                {**cache_metadata, "split": "documents"},
            )
            if cached_path is not None:
                save_cache(cached_path, cache_metadata, query_vectors, document_vectors)
                for partial in (query_checkpoint, document_checkpoint):
                    partial.unlink()
                    partial.with_suffix(partial.suffix + ".json").unlink()
        encoded[name] = (query_vectors, document_vectors)
        results[name] = {
            "model_sha256": model_sha256,
            **metrics(
                scores(query_vectors, document_vectors),
                query_ids,
                document_ids,
                qrels,
            ),
        }

    source = results["source"]
    candidate = results["candidate"]
    deltas = {
        metric: candidate[metric] - source[metric]
        for metric in ("ndcg_at_10", "recall_at_100", "mrr_at_10")
    }
    source_scores = scores(*encoded["source"])
    candidate_scores = scores(*encoded["candidate"])
    source_top10 = np.argsort(-source_scores, axis=1, kind="stable")[:, :10]
    candidate_top10 = np.argsort(-candidate_scores, axis=1, kind="stable")[:, :10]
    overlap = [
        len(set(left).intersection(right)) / 10
        for left, right in zip(source_top10, candidate_top10, strict=True)
    ]
    mean_overlap = float(np.mean(overlap))
    gate_checks = {
        metric: delta >= -MAX_ABSOLUTE_REGRESSION[metric]
        for metric, delta in deltas.items()
    }
    report = {
        "schema": 1,
        "dataset": DATASET,
        "dataset_revision": DATASET_REVISION,
        "queries": len(query_ids),
        "documents": len(document_ids),
        "vector_encoding": "signed-int8x768",
        "onnxruntime": runtime_version,
        "graph_optimization": "ORT_ENABLE_ALL (FastEmbed Level 3)",
        "batch_size": args.batch_size,
        "evaluation_workers": args.workers,
        "profile_inference_batch_size": 1,
        "ort_intra_threads": 1,
        "sequence_buckets": list(SEQUENCE_BUCKETS),
        "max_tokens": args.max_tokens,
        "results": results,
        "candidate_minus_source": deltas,
        "mean_source_candidate_top10_overlap": mean_overlap,
        "release_quality_gate": {
            "max_absolute_regression": MAX_ABSOLUTE_REGRESSION,
            "checks": gate_checks,
            "passed": all(gate_checks.values()),
        },
    }
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    if not report["release_quality_gate"]["passed"]:
        raise SystemExit("candidate failed the frozen retrieval-quality gate")


if __name__ == "__main__":
    main()
