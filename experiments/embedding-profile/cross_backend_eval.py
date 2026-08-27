#!/usr/bin/env python3
"""Gate an NPU-first INT8 embedding space across mixed local backends."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
from datasets import load_dataset

DATASET = "mteb/scifact"
DATASET_REVISION = "cf10ab6856b15b0e670ef8ae5dae4e266c12d035"
PROFILE_ID = "cfetch-embedding-v1"
MODEL_REVISION = "7b5b24595322ab0ea4d08827066860a6df8cb0aa"
VECTOR_ENCODING = "signed-int8x768"
REQUIRED_CLASSES = {"npu", "gpu", "cpu"}

# These absolute floors are fixed quality requirements, not a runtime oracle.
# They are the pinned source-model SciFact measurements minus the maximum
# accepted loss. An NPU anchor that misses them cannot define the shared space.
ANCHOR_MINIMUM = {
    "ndcg_at_10": 0.767907905520953,
    "recall_at_100": 0.970,
    "mrr_at_10": 0.7305529100529101,
}
MAX_PAIR_REGRESSION = {
    "ndcg_at_10": 0.01,
    "recall_at_100": 0.005,
    "mrr_at_10": 0.01,
}


def parse_backend(value: str) -> tuple[str, Path]:
    label, separator, raw_path = value.partition("=")
    if not separator or not label or not raw_path:
        raise argparse.ArgumentTypeError("backend must be LABEL=PATH")
    return label, Path(raw_path)


def load_cache(path: Path) -> tuple[dict[str, object], np.ndarray, np.ndarray]:
    with np.load(path, allow_pickle=False) as cached:
        required = {"metadata", "queries", "documents", "queries_repeat", "documents_repeat"}
        missing = required.difference(cached.files)
        if missing:
            raise ValueError(f"{path}: missing arrays {sorted(missing)}")
        metadata = json.loads(str(cached["metadata"].item()))
        queries = np.asarray(cached["queries"], dtype=np.int8)
        documents = np.asarray(cached["documents"], dtype=np.int8)
        queries_repeat = np.asarray(cached["queries_repeat"], dtype=np.int8)
        documents_repeat = np.asarray(cached["documents_repeat"], dtype=np.int8)

    expected = {
        "schema_version": 1,
        "profile_id": PROFILE_ID,
        "model_revision": MODEL_REVISION,
        "vector_encoding": VECTOR_ENCODING,
        "dataset": DATASET,
        "dataset_revision": DATASET_REVISION,
    }
    for key, value in expected.items():
        if metadata.get(key) != value:
            raise ValueError(f"{path}: metadata {key}={metadata.get(key)!r}, expected {value!r}")
    for key in ("backend", "runtime", "artifact_sha256", "device", "placement_evidence"):
        if not isinstance(metadata.get(key), str) or not metadata[key]:
            raise ValueError(f"{path}: metadata {key} must be a non-empty string")
    if metadata.get("device_class") not in REQUIRED_CLASSES:
        raise ValueError(f"{path}: device_class must be npu, gpu, or cpu")
    if metadata.get("accelerated_placement") is not True:
        raise ValueError(f"{path}: accelerated_placement must be true")
    if queries.ndim != 2 or documents.ndim != 2:
        raise ValueError(f"{path}: vectors must be rank-two arrays")
    if queries.shape[1] != 768 or documents.shape[1] != 768:
        raise ValueError(f"{path}: vectors must have 768 components")
    if queries_repeat.shape != queries.shape or documents_repeat.shape != documents.shape:
        raise ValueError(f"{path}: repeat arrays have different shapes")
    if not np.array_equal(queries, queries_repeat) or not np.array_equal(
        documents, documents_repeat
    ):
        raise ValueError(f"{path}: backend is not byte-repeatable on the same runtime/artifact/device")
    if not np.any(queries, axis=1).all() or not np.any(documents, axis=1).all():
        raise ValueError(f"{path}: cache contains an all-zero vector")
    return metadata, queries, documents


def scores(queries: np.ndarray, documents: np.ndarray) -> np.ndarray:
    query_float = queries.astype(np.float64)
    document_float = documents.astype(np.float64)
    query_float /= np.linalg.norm(query_float, axis=1, keepdims=True)
    document_float /= np.linalg.norm(document_float, axis=1, keepdims=True)
    return query_float @ document_float.T


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


def corresponding_vector_diagnostics(left: np.ndarray, right: np.ndarray) -> dict[str, float]:
    left_float = left.astype(np.float64)
    right_float = right.astype(np.float64)
    cosine = np.sum(left_float * right_float, axis=1) / (
        np.linalg.norm(left_float, axis=1) * np.linalg.norm(right_float, axis=1)
    )
    return {
        "exact_record_fraction": float(np.mean(np.all(left == right, axis=1))),
        "mean_cosine": float(np.mean(cosine)),
        "minimum_cosine": float(np.min(cosine)),
    }


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Evaluate every query-backend/document-backend pairing against an NPU anchor"
    )
    parser.add_argument(
        "--backend",
        action="append",
        type=parse_backend,
        required=True,
        metavar="LABEL=PATH",
        help="repeat for every NPU, GPU, and CPU cache",
    )
    parser.add_argument("--npu-anchor", required=True, help="label of the NPU reference cache")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    paths = dict(args.backend)
    if len(paths) != len(args.backend):
        raise SystemExit("backend labels must be unique")
    if args.npu_anchor not in paths:
        raise SystemExit("--npu-anchor must name one --backend label")

    loaded = {label: load_cache(path) for label, path in paths.items()}
    anchor_metadata = loaded[args.npu_anchor][0]
    if anchor_metadata["device_class"] != "npu":
        raise SystemExit("--npu-anchor cache must declare device_class=npu")
    classes = {metadata["device_class"] for metadata, _, _ in loaded.values()}

    qrel_rows = load_dataset(DATASET, revision=DATASET_REVISION, split="test")
    corpus = load_dataset(DATASET, "corpus", revision=DATASET_REVISION, split="corpus")
    queries = load_dataset(DATASET, "queries", revision=DATASET_REVISION, split="queries")
    qrels: dict[str, set[str]] = {}
    for row in qrel_rows:
        if row["score"] > 0:
            qrels.setdefault(row["query-id"], set()).add(row["corpus-id"])
    query_ids = sorted(qrels, key=lambda item: int(item))
    document_ids = [row["_id"] for row in corpus]
    expected_shape = (len(query_ids), len(document_ids))
    for label, (_, query_vectors, document_vectors) in loaded.items():
        if (len(query_vectors), len(document_vectors)) != expected_shape:
            raise SystemExit(
                f"{label}: cache has {len(query_vectors)} queries/{len(document_vectors)} documents; "
                f"pinned SciFact requires {expected_shape[0]}/{expected_shape[1]}"
            )

    pair_metrics: dict[str, dict[str, object]] = {}
    for query_label, (_, query_vectors, _) in loaded.items():
        for document_label, (_, _, document_vectors) in loaded.items():
            key = f"{query_label}__queries--{document_label}__documents"
            pair_metrics[key] = {
                "query_backend": query_label,
                "document_backend": document_label,
                **metrics(scores(query_vectors, document_vectors), query_ids, document_ids, qrels),
            }

    anchor_key = f"{args.npu_anchor}__queries--{args.npu_anchor}__documents"
    anchor = pair_metrics[anchor_key]
    anchor_checks = {
        metric: anchor[metric] >= minimum for metric, minimum in ANCHOR_MINIMUM.items()
    }
    pair_checks: dict[str, dict[str, bool]] = {}
    for key, result in pair_metrics.items():
        pair_checks[key] = {
            metric: result[metric] >= anchor[metric] - MAX_PAIR_REGRESSION[metric]
            for metric in MAX_PAIR_REGRESSION
        }

    diagnostics: dict[str, dict[str, object]] = {}
    labels = list(loaded)
    for offset, left_label in enumerate(labels):
        _, left_queries, left_documents = loaded[left_label]
        for right_label in labels[offset + 1 :]:
            _, right_queries, right_documents = loaded[right_label]
            diagnostics[f"{left_label}--{right_label}"] = {
                "queries": corresponding_vector_diagnostics(left_queries, right_queries),
                "documents": corresponding_vector_diagnostics(left_documents, right_documents),
            }

    complete_classes = REQUIRED_CLASSES.issubset(classes)
    passed = (
        complete_classes
        and all(anchor_checks.values())
        and all(all(checks.values()) for checks in pair_checks.values())
    )
    report = {
        "schema_version": 1,
        "profile_id": PROFILE_ID,
        "model_revision": MODEL_REVISION,
        "dataset": DATASET,
        "dataset_revision": DATASET_REVISION,
        "npu_anchor": args.npu_anchor,
        "backends": {label: metadata for label, (metadata, _, _) in loaded.items()},
        "pair_metrics": pair_metrics,
        "cross_backend_vector_diagnostics_not_gates": diagnostics,
        "release_gate": {
            "required_device_classes": sorted(REQUIRED_CLASSES),
            "present_device_classes": sorted(classes),
            "all_required_device_classes_present": complete_classes,
            "npu_anchor_minimum": ANCHOR_MINIMUM,
            "npu_anchor_checks": anchor_checks,
            "max_pair_regression_from_npu_anchor": MAX_PAIR_REGRESSION,
            "pair_checks": pair_checks,
            "passed": passed,
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, indent=2, sort_keys=True))
    if not passed:
        raise SystemExit("mixed-backend release gate failed")


if __name__ == "__main__":
    main()
