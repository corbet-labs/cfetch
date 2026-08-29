#!/usr/bin/env python3
"""One pinned, ordered SciFact dataset contract shared by export and evaluation."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from dataclasses import dataclass
from pathlib import Path
import tempfile
from typing import Sequence

DATASET = "mteb/scifact"
DATASET_REVISION = "cf10ab6856b15b0e670ef8ae5dae4e266c12d035"
EXPECTED_QREL_ROWS = 339
EXPECTED_QUERIES = 300
EXPECTED_DOCUMENTS = 5183


@dataclass(frozen=True)
class SciFactContract:
    query_ids: list[str]
    document_ids: list[str]
    qrels: dict[str, set[str]]
    query_texts: list[str]
    document_texts: list[str]


def wire_probe_document(contract: SciFactContract) -> dict[str, object]:
    """Build the one canonical offline input manifest used by the collector."""

    # Import here so the dataset contract remains independently importable by
    # admission_evidence.py users and cannot introduce an import cycle.
    from admission_evidence import (
        WIRE_BATCH_INPUT_SELECTION,
        wire_batch_inputs,
    )

    return {
        "schema_version": 1,
        "dataset": DATASET,
        "dataset_revision": DATASET_REVISION,
        "selection": WIRE_BATCH_INPUT_SELECTION,
        "inputs": wire_batch_inputs(contract.query_texts, contract.document_texts),
    }


def write_wire_probe_inputs(output: Path, contract: SciFactContract) -> tuple[Path, str]:
    """Atomically create, but never replace, a canonical wire-input manifest."""

    output = output.resolve()
    parent = output.parent
    if parent.is_symlink() or not parent.is_dir():
        raise ValueError("wire-input output parent must be an existing real directory")
    if output.exists() or output.is_symlink():
        raise ValueError(f"refusing to overwrite wire-input manifest: {output}")
    raw = (
        json.dumps(
            wire_probe_document(contract),
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n"
    ).encode("utf-8")
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            prefix=f".{output.name}-", suffix=".tmp", dir=parent, delete=False
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(raw)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.chmod(temporary_path, 0o644)
        # A same-directory hard link is an atomic no-replace publication.  A
        # competing writer therefore fails instead of replacing trusted bytes.
        os.link(temporary_path, output)
        temporary_path.unlink()
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
    return output, hashlib.sha256(raw).hexdigest()


def load_scifact_contract(
    query_prefix: str = "", document_prefix: str = ""
) -> SciFactContract:
    from datasets import load_dataset

    qrel_rows = load_dataset(DATASET, revision=DATASET_REVISION, split="test")
    corpus = load_dataset(DATASET, "corpus", revision=DATASET_REVISION, split="corpus")
    queries = load_dataset(DATASET, "queries", revision=DATASET_REVISION, split="queries")

    if len(qrel_rows) != EXPECTED_QREL_ROWS:
        raise ValueError(
            f"pinned SciFact has {len(qrel_rows)} test qrels, expected {EXPECTED_QREL_ROWS}"
        )
    qrels: dict[str, set[str]] = {}
    for row in qrel_rows:
        if row["score"] > 0:
            qrels.setdefault(row["query-id"], set()).add(row["corpus-id"])
    query_ids = sorted(qrels, key=lambda item: int(item))
    if len(query_ids) != EXPECTED_QUERIES:
        raise ValueError(
            f"pinned SciFact has {len(query_ids)} test queries, expected {EXPECTED_QUERIES}"
        )

    query_text_by_id = {row["_id"]: row["text"] for row in queries}
    if len(query_text_by_id) != len(queries):
        raise ValueError("pinned SciFact query ids are not unique")
    missing_queries = [item for item in query_ids if item not in query_text_by_id]
    if missing_queries:
        raise ValueError(f"pinned SciFact is missing query ids {missing_queries}")

    document_ids = [row["_id"] for row in corpus]
    if len(document_ids) != EXPECTED_DOCUMENTS:
        raise ValueError(
            f"pinned SciFact has {len(document_ids)} documents, expected {EXPECTED_DOCUMENTS}"
        )
    if len(set(document_ids)) != len(document_ids):
        raise ValueError("pinned SciFact document ids are not unique")
    document_id_set = set(document_ids)
    missing_documents = sorted(
        {item for relevant in qrels.values() for item in relevant} - document_id_set
    )
    if missing_documents:
        raise ValueError(f"pinned SciFact is missing relevant documents {missing_documents}")

    query_texts = [query_prefix + query_text_by_id[item] for item in query_ids]
    document_texts = [
        document_prefix + ((row["title"] + "\n") if row["title"] else "") + row["text"]
        for row in corpus
    ]
    if any(not text for text in query_texts) or any(not text for text in document_texts):
        raise ValueError("pinned SciFact contains an empty prepared input")
    return SciFactContract(
        query_ids=query_ids,
        document_ids=document_ids,
        qrels=qrels,
        query_texts=query_texts,
        document_texts=document_texts,
    )


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument(
        "--wire-inputs-output",
        type=Path,
        help=(
            "create the canonical prefixed 64-input physical-collector manifest "
            "at a new path"
        ),
    )
    return result


def main(argv: Sequence[str] | None = None) -> None:
    args = parser().parse_args(argv)
    if args.wire_inputs_output is not None:
        from admission_evidence import DOCUMENT_PREFIX, QUERY_PREFIX

        contract = load_scifact_contract(QUERY_PREFIX, DOCUMENT_PREFIX)
        try:
            output, digest = write_wire_probe_inputs(args.wire_inputs_output, contract)
        except (OSError, ValueError) as error:
            raise SystemExit(f"wire-input generation refused: {error}") from error
        print(
            json.dumps(
                {
                    "schema_version": 1,
                    "wire_inputs": str(output),
                    "wire_inputs_sha256": digest,
                    "inputs": 64,
                    "dataset": DATASET,
                    "dataset_revision": DATASET_REVISION,
                },
                sort_keys=True,
            )
        )
        return

    contract = load_scifact_contract()
    print(
        json.dumps(
            {
                "dataset": DATASET,
                "dataset_revision": DATASET_REVISION,
                "documents": len(contract.document_ids),
                "queries": len(contract.query_ids),
                "qrel_rows": sum(len(items) for items in contract.qrels.values()),
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
