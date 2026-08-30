from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import scifact_contract

from admission_evidence import (
    DOCUMENT_PREFIX,
    QUERY_PREFIX,
    WIRE_BATCH_INPUT_SELECTION,
)
from scifact_contract import (
    DATASET,
    DATASET_REVISION,
    RAW_SNAPSHOT_FILES,
    SciFactContract,
    _contract_from_rows,
    load_scifact_contract,
    wire_probe_document,
    write_wire_probe_inputs,
)


def contract() -> SciFactContract:
    return SciFactContract(
        query_ids=[str(index) for index in range(40)],
        document_ids=[str(index) for index in range(40)],
        qrels={},
        query_texts=[f"{QUERY_PREFIX}query {index}" for index in range(40)],
        document_texts=[f"{DOCUMENT_PREFIX}document {index}" for index in range(40)],
    )


def raw_rows() -> tuple[
    list[dict[str, object]],
    list[dict[str, object]],
    list[dict[str, object]],
]:
    return (
        [
            {"query-id": "2", "corpus-id": "11", "score": "1"},
            {"query-id": "1", "corpus-id": "10", "score": "1"},
        ],
        [
            {"_id": "12", "title": "Third", "text": "document three"},
            {"_id": "10", "title": "First", "text": "document one"},
            {"_id": "11", "title": "", "text": "document two"},
        ],
        [
            {"_id": "2", "text": "query two"},
            {"_id": "9", "text": "unused query"},
            {"_id": "1", "text": "query one"},
        ],
    )


def write_raw_snapshot(root: Path) -> dict[str, tuple[int, str]]:
    qrels, corpus, queries = raw_rows()
    documents = {
        "corpus.jsonl": corpus,
        "queries.jsonl": queries,
        "qrels/test.jsonl": qrels,
    }
    specifications: dict[str, tuple[int, str]] = {}
    for relative, rows in documents.items():
        raw = b"".join(
            (json.dumps(row, separators=(",", ":")) + "\n").encode("utf-8")
            for row in rows
        )
        path = root.joinpath(*relative.split("/"))
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)
        specifications[relative] = (len(raw), hashlib.sha256(raw).hexdigest())
    return specifications


def synthetic_contract_pins(specifications: dict[str, tuple[int, str]]):
    return patch.multiple(
        scifact_contract,
        RAW_SNAPSHOT_FILES=specifications,
        EXPECTED_QREL_ROWS=2,
        EXPECTED_QUERIES=2,
        EXPECTED_QUERY_ROWS=3,
        EXPECTED_DOCUMENTS=3,
    )


class WireProbeInputTests(unittest.TestCase):
    def test_document_selects_exactly_first_queries_then_documents(self) -> None:
        document = wire_probe_document(contract())
        self.assertEqual(
            set(document),
            {"schema_version", "dataset", "dataset_revision", "selection", "inputs"},
        )
        self.assertEqual(document["dataset"], DATASET)
        self.assertEqual(document["dataset_revision"], DATASET_REVISION)
        self.assertEqual(document["selection"], WIRE_BATCH_INPUT_SELECTION)
        self.assertEqual(
            document["inputs"],
            [
                *[f"{QUERY_PREFIX}query {index}" for index in range(32)],
                *[f"{DOCUMENT_PREFIX}document {index}" for index in range(32)],
            ],
        )

    def test_writer_is_canonical_atomic_and_refuses_overwrite(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "wire-inputs.json"
            path, digest = write_wire_probe_inputs(output, contract())
            raw = output.read_bytes()
            expected = (
                json.dumps(
                    wire_probe_document(contract()),
                    ensure_ascii=False,
                    allow_nan=False,
                    sort_keys=True,
                    separators=(",", ":"),
                )
                + "\n"
            ).encode("utf-8")
            self.assertEqual(path, output.resolve())
            self.assertEqual(raw, expected)
            self.assertEqual(digest, hashlib.sha256(expected).hexdigest())
            with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
                write_wire_probe_inputs(output, contract())

    def test_writer_requires_an_existing_real_parent(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "missing" / "wire-inputs.json"
            with self.assertRaisesRegex(ValueError, "existing real directory"):
                write_wire_probe_inputs(output, contract())


class RawSnapshotTests(unittest.TestCase):
    def test_production_snapshot_pins_exact_upstream_bytes(self) -> None:
        self.assertEqual(
            RAW_SNAPSHOT_FILES,
            {
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
            },
        )

    def test_raw_snapshot_preserves_dataset_contract_order_and_semantics(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "snapshot"
            root.mkdir()
            specifications = write_raw_snapshot(root)
            with synthetic_contract_pins(specifications):
                loaded = load_scifact_contract(QUERY_PREFIX, DOCUMENT_PREFIX, root)

            self.assertEqual(loaded.query_ids, ["1", "2"])
            self.assertEqual(loaded.document_ids, ["12", "10", "11"])
            self.assertEqual(loaded.qrels, {"1": {"10"}, "2": {"11"}})
            self.assertEqual(
                loaded.query_texts,
                [f"{QUERY_PREFIX}query one", f"{QUERY_PREFIX}query two"],
            )
            self.assertEqual(
                loaded.document_texts,
                [
                    f"{DOCUMENT_PREFIX}Third\ndocument three",
                    f"{DOCUMENT_PREFIX}First\ndocument one",
                    f"{DOCUMENT_PREFIX}document two",
                ],
            )

    def test_raw_snapshot_rejects_digest_and_size_drift_before_parsing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "snapshot"
            root.mkdir()
            specifications = write_raw_snapshot(root)
            corpus = root / "corpus.jsonl"
            raw = corpus.read_bytes()
            corpus.write_bytes(raw.replace(b"Third", b"Other", 1))
            with synthetic_contract_pins(specifications), self.assertRaisesRegex(
                ValueError, "sha256"
            ):
                load_scifact_contract(snapshot_directory=root)

            corpus.write_bytes(raw + b" ")
            with synthetic_contract_pins(specifications), self.assertRaisesRegex(
                ValueError, "byte count"
            ):
                load_scifact_contract(snapshot_directory=root)

    def test_normalized_contract_rejects_schema_and_count_drift(self) -> None:
        qrels, corpus, queries = raw_rows()
        with patch.multiple(
            scifact_contract,
            EXPECTED_QREL_ROWS=2,
            EXPECTED_QUERIES=2,
            EXPECTED_QUERY_ROWS=3,
            EXPECTED_DOCUMENTS=3,
        ):
            loader_qrels = [dict(row) for row in qrels]
            loader_qrels[0]["score"] = 1.0
            self.assertEqual(
                _contract_from_rows(loader_qrels, corpus, queries, "", "").qrels,
                {"1": {"10"}, "2": {"11"}},
            )

            changed_queries = [dict(row) for row in queries]
            changed_queries[0]["unexpected"] = True
            with self.assertRaisesRegex(ValueError, "contain exactly"):
                _contract_from_rows(qrels, corpus, changed_queries, "", "")

            with self.assertRaisesRegex(ValueError, "2 documents, expected 3"):
                _contract_from_rows(qrels, corpus[:-1], queries, "", "")

    def test_raw_snapshot_rejects_symlink_and_noncanonical_path_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            root = parent / "snapshot"
            root.mkdir()
            specifications = write_raw_snapshot(root)

            root_link = parent / "snapshot-link"
            root_link.symlink_to(root, target_is_directory=True)
            with synthetic_contract_pins(specifications), self.assertRaisesRegex(
                ValueError, "without symlinks"
            ):
                load_scifact_contract(snapshot_directory=root_link)

            noncanonical = root / ".." / "snapshot"
            with synthetic_contract_pins(specifications), self.assertRaisesRegex(
                ValueError, "without symlinks"
            ):
                load_scifact_contract(snapshot_directory=noncanonical)

            corpus = root / "corpus.jsonl"
            moved = parent / "corpus.jsonl"
            os.replace(corpus, moved)
            corpus.symlink_to(moved)
            with synthetic_contract_pins(specifications), self.assertRaisesRegex(
                ValueError, "regular non-symlink"
            ):
                load_scifact_contract(snapshot_directory=root)


if __name__ == "__main__":
    unittest.main()
