from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from admission_evidence import (
    DOCUMENT_PREFIX,
    QUERY_PREFIX,
    WIRE_BATCH_INPUT_SELECTION,
)
from scifact_contract import (
    DATASET,
    DATASET_REVISION,
    SciFactContract,
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


if __name__ == "__main__":
    unittest.main()
