#!/usr/bin/env python3
"""One pinned, ordered SciFact dataset contract shared by export and evaluation."""

from __future__ import annotations

import argparse
from collections.abc import Mapping, Sequence
import hashlib
import json
import math
import os
from dataclasses import dataclass
from pathlib import Path
import re
import tempfile

DATASET = "mteb/scifact"
DATASET_REVISION = "cf10ab6856b15b0e670ef8ae5dae4e266c12d035"
EXPECTED_QREL_ROWS = 339
EXPECTED_QUERIES = 300
EXPECTED_QUERY_ROWS = 1109
EXPECTED_DOCUMENTS = 5183
RAW_SNAPSHOT_FILES = {
    "corpus.jsonl": (
        8_023_638,
        "f0d32db0d156b526d75921ed7a76f2cb912902631c87248c5c97c617bad0b60c",
    ),
    "queries.jsonl": (
        129_085,
        "9db7df096f7414435d52bafcbacf814c30cea50eb565c1e8fa6d11440759bba8",
    ),
    "qrels/test.jsonl": (
        19_940,
        "da33fc0edc7447d43908ea92bf11de010e361ac43228295f09bdcd33ce14730c",
    ),
}
ID_RE = re.compile(r"0|[1-9][0-9]*")


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


def _reject_duplicate_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def _reject_nonfinite(value: str) -> object:
    raise ValueError(f"non-finite JSON number {value!r}")


def _verified_snapshot_file(root: Path, relative: str) -> bytes:
    expected_bytes, expected_sha256 = RAW_SNAPSHOT_FILES[relative]
    path = root.joinpath(*relative.split("/"))
    if path.is_symlink() or path.resolve() != path.absolute() or not path.is_file():
        raise ValueError(
            f"pinned SciFact snapshot member {relative!r} must be a regular non-symlink file"
        )
    if path.stat().st_size != expected_bytes:
        raise ValueError(
            f"pinned SciFact snapshot member {relative!r} has an unexpected byte count"
        )
    raw = path.read_bytes()
    if len(raw) != expected_bytes:
        raise ValueError(
            f"pinned SciFact snapshot member {relative!r} changed while it was read"
        )
    actual_sha256 = hashlib.sha256(raw).hexdigest()
    if actual_sha256 != expected_sha256:
        raise ValueError(
            f"pinned SciFact snapshot member {relative!r} has sha256 "
            f"{actual_sha256}, expected {expected_sha256}"
        )
    return raw


def _jsonl_rows(raw: bytes, label: str) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for line_number, line in enumerate(raw.splitlines(), start=1):
        if not line:
            raise ValueError(f"{label} line {line_number} is empty")
        try:
            row = json.loads(
                line,
                object_pairs_hook=_reject_duplicate_keys,
                parse_constant=_reject_nonfinite,
            )
        except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
            raise ValueError(
                f"{label} line {line_number} is not strict UTF-8 JSON"
            ) from error
        if not isinstance(row, dict):
            raise ValueError(f"{label} line {line_number} must contain one JSON object")
        rows.append(row)
    if not rows:
        raise ValueError(f"{label} must contain at least one row")
    return rows


def _raw_snapshot_rows(
    snapshot_directory: Path,
) -> tuple[list[dict[str, object]], list[dict[str, object]], list[dict[str, object]]]:
    root = snapshot_directory
    if root.is_symlink() or root.resolve() != root.absolute() or not root.is_dir():
        raise ValueError(
            "pinned SciFact snapshot must be an existing real directory without symlinks"
        )
    captured = {
        relative: _verified_snapshot_file(root, relative)
        for relative in RAW_SNAPSHOT_FILES
    }
    return (
        _jsonl_rows(captured["qrels/test.jsonl"], "SciFact qrels/test.jsonl"),
        _jsonl_rows(captured["corpus.jsonl"], "SciFact corpus.jsonl"),
        _jsonl_rows(captured["queries.jsonl"], "SciFact queries.jsonl"),
    )


def _canonical_id(value: object, label: str) -> str:
    if not isinstance(value, str) or ID_RE.fullmatch(value) is None:
        raise ValueError(f"{label} must be a canonical decimal string id")
    return value


def _positive_qrel_score(value: object, label: str) -> bool:
    if type(value) is int:
        return value > 0
    if type(value) is float and math.isfinite(value) and value.is_integer():
        return value > 0.0
    if isinstance(value, str) and re.fullmatch(r"-?(?:0|[1-9][0-9]*)", value):
        return int(value) > 0
    raise ValueError(
        f"{label} must be an integer-valued number or canonical integer string"
    )


def _require_schema(
    row: object, expected: set[str], label: str
) -> Mapping[str, object]:
    if not isinstance(row, Mapping) or set(row) != expected:
        raise ValueError(f"{label} must contain exactly {sorted(expected)}")
    return row


def _contract_from_rows(
    qrel_rows: Sequence[Mapping[str, object]],
    corpus: Sequence[Mapping[str, object]],
    queries: Sequence[Mapping[str, object]],
    query_prefix: str,
    document_prefix: str,
) -> SciFactContract:
    """Normalize either trusted loader into one strict ordered contract."""
    if len(qrel_rows) != EXPECTED_QREL_ROWS:
        raise ValueError(
            f"pinned SciFact has {len(qrel_rows)} test qrels, expected {EXPECTED_QREL_ROWS}"
        )
    qrels: dict[str, set[str]] = {}
    qrel_pairs: set[tuple[str, str]] = set()
    for index, unvalidated in enumerate(qrel_rows):
        row = _require_schema(
            unvalidated,
            {"query-id", "corpus-id", "score"},
            f"pinned SciFact qrel row {index}",
        )
        query_id = _canonical_id(row["query-id"], f"pinned SciFact qrel row {index} query-id")
        document_id = _canonical_id(
            row["corpus-id"], f"pinned SciFact qrel row {index} corpus-id"
        )
        if _positive_qrel_score(row["score"], f"pinned SciFact qrel row {index} score"):
            pair = (query_id, document_id)
            if pair in qrel_pairs:
                raise ValueError("pinned SciFact contains a duplicate positive qrel")
            qrel_pairs.add(pair)
            qrels.setdefault(query_id, set()).add(document_id)
    if len(qrel_pairs) != EXPECTED_QREL_ROWS:
        raise ValueError("pinned SciFact test qrels must all be unique and positive")
    query_ids = sorted(qrels, key=lambda item: int(item))
    if len(query_ids) != EXPECTED_QUERIES:
        raise ValueError(
            f"pinned SciFact has {len(query_ids)} test queries, expected {EXPECTED_QUERIES}"
        )

    if len(queries) != EXPECTED_QUERY_ROWS:
        raise ValueError(
            f"pinned SciFact has {len(queries)} query rows, expected {EXPECTED_QUERY_ROWS}"
        )
    query_text_by_id: dict[str, str] = {}
    for index, unvalidated in enumerate(queries):
        row = _require_schema(
            unvalidated, {"_id", "text"}, f"pinned SciFact query row {index}"
        )
        query_id = _canonical_id(row["_id"], f"pinned SciFact query row {index} _id")
        text = row["text"]
        if not isinstance(text, str):
            raise ValueError(f"pinned SciFact query row {index} text must be a string")
        if query_id in query_text_by_id:
            raise ValueError("pinned SciFact query ids are not unique")
        query_text_by_id[query_id] = text
    missing_queries = [item for item in query_ids if item not in query_text_by_id]
    if missing_queries:
        raise ValueError(f"pinned SciFact is missing query ids {missing_queries}")

    if len(corpus) != EXPECTED_DOCUMENTS:
        raise ValueError(
            f"pinned SciFact has {len(corpus)} documents, expected {EXPECTED_DOCUMENTS}"
        )
    document_ids: list[str] = []
    prepared_documents: list[str] = []
    for index, unvalidated in enumerate(corpus):
        row = _require_schema(
            unvalidated,
            {"_id", "title", "text"},
            f"pinned SciFact corpus row {index}",
        )
        document_id = _canonical_id(
            row["_id"], f"pinned SciFact corpus row {index} _id"
        )
        title = row["title"]
        text = row["text"]
        if not isinstance(title, str) or not isinstance(text, str):
            raise ValueError(
                f"pinned SciFact corpus row {index} title and text must be strings"
            )
        document_ids.append(document_id)
        prepared_documents.append(
            document_prefix + ((title + "\n") if title else "") + text
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
    document_texts = prepared_documents
    if any(not text for text in query_texts) or any(not text for text in document_texts):
        raise ValueError("pinned SciFact contains an empty prepared input")
    return SciFactContract(
        query_ids=query_ids,
        document_ids=document_ids,
        qrels=qrels,
        query_texts=query_texts,
        document_texts=document_texts,
    )


def load_scifact_contract(
    query_prefix: str = "",
    document_prefix: str = "",
    snapshot_directory: Path | None = None,
) -> SciFactContract:
    if snapshot_directory is None:
        from datasets import load_dataset

        qrel_rows = load_dataset(DATASET, revision=DATASET_REVISION, split="test")
        corpus = load_dataset(
            DATASET, "corpus", revision=DATASET_REVISION, split="corpus"
        )
        queries = load_dataset(
            DATASET, "queries", revision=DATASET_REVISION, split="queries"
        )
    else:
        qrel_rows, corpus, queries = _raw_snapshot_rows(snapshot_directory)
    return _contract_from_rows(
        qrel_rows,
        corpus,
        queries,
        query_prefix,
        document_prefix,
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
