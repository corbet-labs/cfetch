#!/usr/bin/env python3
"""Focused tests for final packaged-adapter conformance replay."""

from __future__ import annotations

from collections.abc import Sequence
import hashlib
import json
from pathlib import Path
import stat
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch
import warnings
import zipfile

import numpy as np
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from admission_evidence import (
    DIMENSIONS,
    SEQUENCE_BUCKETS,
    SEQUENCE_SEMANTIC_FIXTURE_ID,
    SEQUENCE_SEMANTIC_FIXTURE_SHA256,
    SUPPORTED_MAX_BATCH_SIZE,
    WIRE_BATCH_INPUT_SELECTION,
    ordered_input_json_sha256,
    sequence_semantic_probe_inputs,
)
from export_adapter_cache import SEQUENCE_PROBE_ARRAY_NAMES, canonical_i8, utf8_sha256
from final_package_conformance import (
    ADMISSION_IMPLEMENTATION_BUNDLE_SHA256,
    _extract_exact_package,
    build_parser,
    launch_exact_package,
    replay_retained_outputs,
    run_final_package_conformance,
    write_conformance_receipt,
)
from profile_identity import ADMISSION_POLICY_SHA256


REPORT_SHA256 = "4" * 64
ATTESTATION_PUBLIC_KEY = "a" * 64


def final_execution(scope_id: str) -> dict[str, object]:
    device_class = scope_id.removeprefix("synthetic-")
    return {
        "scope_id": scope_id,
        "transport": "supervised-local",
        "backend": "synthetic-adapter",
        "runtime": "synthetic-runtime-1",
        "compiler": "synthetic-compiler-1",
        "package_target": "synthetic-target",
        "artifact_source": "synthetic-source@revision/model",
        "device_class": device_class,
        "device": f"synthetic-{device_class}-family",
        "artifact_sha256": "1" * 64,
        "internal_precision": "target-native",
        "placement_evidence_sha256": "2" * 64,
        "supported_max_tokens": 2048,
        "supported_sequence_buckets": SEQUENCE_BUCKETS,
        "supported_max_batch_size": SUPPORTED_MAX_BATCH_SIZE,
        "sequence_capability_evidence_sha256": "3" * 64,
        "performance_evidence_sha256": "5" * 64,
        "accelerated_placement": True,
        "compatibility_report_sha256": REPORT_SHA256,
    }


def deterministic_floats(texts: Sequence[str]) -> np.ndarray:
    values = np.zeros((len(texts), DIMENSIONS), dtype=np.float32)
    for index, text in enumerate(texts):
        values[index, 0] = -1.0 if "music" in text else 1.0
        values[index, 1] = (sum(text.encode("utf-8")) % 29) / 100.0
    return values


def sequence_report(
    wire_inputs: Sequence[str], wire_outputs: np.ndarray
) -> dict[str, object]:
    input_digest = ordered_input_json_sha256(wire_inputs)
    output_digest = hashlib.sha256(
        np.ascontiguousarray(wire_outputs).tobytes()
    ).hexdigest()
    bucket_results = []
    for bucket in SEQUENCE_BUCKETS:
        inputs = sequence_semantic_probe_inputs(bucket)
        outputs = canonical_i8(deterministic_floats(inputs))
        labels = ("query", "relevant_document", "irrelevant_document")
        semantic_probe = {
            "fixture_id": SEQUENCE_SEMANTIC_FIXTURE_ID,
            "fixture_sha256": SEQUENCE_SEMANTIC_FIXTURE_SHA256,
            "canonical_repeatability": True,
            "self_relevant_before_irrelevant": True,
        }
        for index, (label, text) in enumerate(zip(labels, inputs, strict=True)):
            semantic_probe[f"{label}_input_utf8_sha256"] = utf8_sha256(text)
            semantic_probe[f"{label}_token_count"] = bucket
            semantic_probe[f"{label}_canonical_output_bytes_sha256"] = (
                hashlib.sha256(
                    np.ascontiguousarray(outputs[index]).tobytes()
                ).hexdigest()
            )
        bucket_results.append({"bucket": bucket, "semantic_probe": semantic_probe})
    return {
        "supported_sequence_buckets": SEQUENCE_BUCKETS,
        "supported_max_batch_size": SUPPORTED_MAX_BATCH_SIZE,
        "wire_batch_results": [
            {
                "batch_size": batch_size,
                "input_count": SUPPORTED_MAX_BATCH_SIZE,
                "request_count": (
                    SUPPORTED_MAX_BATCH_SIZE + batch_size - 1
                )
                // batch_size,
                "response_row_count": SUPPORTED_MAX_BATCH_SIZE,
                "ordered_input_json_sha256": input_digest,
                "canonical_output_bytes_sha256": output_digest,
                "signed_transactions_sha256": hashlib.sha256(
                    f"wire-transactions-{batch_size}".encode()
                ).hexdigest(),
            }
            for batch_size in range(1, SUPPORTED_MAX_BATCH_SIZE + 1)
        ],
        "grouping_invariance": {
            "batch_sizes": list(range(1, SUPPORTED_MAX_BATCH_SIZE + 1)),
            "input_selection": WIRE_BATCH_INPUT_SELECTION,
            "same_inputs_in_same_order": True,
            "canonical_output_bytes_equal": True,
        },
        "bucket_results": bucket_results,
    }


def retained_probe_outputs() -> tuple[np.ndarray, ...]:
    by_kind: list[list[np.ndarray]] = [[], [], []]
    for bucket in SEQUENCE_BUCKETS:
        outputs = canonical_i8(
            deterministic_floats(sequence_semantic_probe_inputs(bucket))
        )
        for index in range(3):
            by_kind[index].append(outputs[index])
    primary = tuple(np.stack(rows) for rows in by_kind)
    return (*primary, *(array.copy() for array in primary))


class FinalPackageConformanceTests(unittest.TestCase):
    @staticmethod
    def write_launchable_package(
        path: Path,
        *,
        requested_scope_overrides: dict[str, object] | None = None,
        manifest_overrides: dict[str, object] | None = None,
    ) -> tuple[str, str, str]:
        dispatcher_name = "cfetch-inference"
        dispatcher = b"""#!/usr/bin/env python3
import json
import socket
import sys

json.loads(sys.stdin.buffer.readline())
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen()
port = server.getsockname()[1]
print(json.dumps({"schema_version": 1, "url": f"http://127.0.0.1:{port}/v1", "scope_ids": ["synthetic-npu", "synthetic-gpu", "synthetic-cpu"]}, separators=(",", ":")), flush=True)
while sys.stdin.buffer.read(4096):
    pass
server.close()
"""
        scopes = []
        for scope_id in ("synthetic-npu", "synthetic-gpu", "synthetic-cpu"):
            scope = {
                **final_execution(scope_id),
                "attestation_public_key": ATTESTATION_PUBLIC_KEY,
            }
            if scope_id == "synthetic-npu" and requested_scope_overrides is not None:
                scope.update(requested_scope_overrides)
            scopes.append(scope)
        manifest_document = {
            "schema_version": 1,
            "package_state": "release",
            "profile_id": "cfetch-embedding-v1",
            "profile_manifest_sha256": (
                "59210a333494f788eb8e607fe38cabb6af1a7aa7cdf604ddf52e3fa6004b5afb"
            ),
            "admission_policy_sha256": ADMISSION_POLICY_SHA256,
            "model": "google/embeddinggemma-300m",
            "model_revision": "57c266a740f537b4dc058e1b0cda161fd15afa75",
            "scopes": scopes,
        }
        if manifest_overrides is not None:
            manifest_document.update(manifest_overrides)
        manifest = (
            json.dumps(
                manifest_document,
                sort_keys=True,
                separators=(",", ":"),
            )
            + "\n"
        ).encode("utf-8")
        with zipfile.ZipFile(path, "w") as archive:
            for name, payload, permissions in (
                (dispatcher_name, dispatcher, 0o755),
                ("package-manifest.json", manifest, 0o644),
            ):
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | permissions) << 16
                archive.writestr(info, payload)
        return (
            dispatcher_name,
            hashlib.sha256(dispatcher).hexdigest(),
            hashlib.sha256(manifest).hexdigest(),
        )

    def test_receipt_launcher_executes_the_dispatcher_from_exact_zip(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory) / "package.zip"
            dispatcher, dispatcher_sha256, manifest_sha256 = self.write_launchable_package(package)
            digest = hashlib.sha256(package.read_bytes()).hexdigest()
            with warnings.catch_warnings(record=True) as caught:
                warnings.simplefilter("always", ResourceWarning)
                with launch_exact_package(
                    package,
                    digest,
                    dispatcher,
                    dispatcher_sha256,
                    manifest_sha256,
                    ["synthetic-npu", "synthetic-gpu", "synthetic-cpu"],
                    "synthetic-npu",
                    final_execution("synthetic-npu"),
                    ATTESTATION_PUBLIC_KEY,
                ) as (endpoint, bearer):
                    self.assertRegex(endpoint, r"^http://127\.0\.0\.1:[0-9]+/v1/embeddings$")
                    self.assertEqual(len(bearer), 64)
            self.assertFalse(
                [warning for warning in caught if warning.category is ResourceWarning]
            )

    def test_raw_scifact_snapshot_is_explicit_and_optional(self) -> None:
        action = next(
            item for item in build_parser()._actions if item.dest == "scifact_snapshot"
        )
        self.assertFalse(action.required)
        self.assertIsNone(action.default)
        self.assertEqual(action.type, Path)

    @patch("final_package_conformance.load_scifact_inputs")
    @patch("final_package_conformance.load_retained_conformance")
    def test_final_replay_loads_the_explicit_snapshot(
        self, load_retained: object, load_inputs: object
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            snapshot = Path(directory) / "scifact"
            load_retained.return_value = (
                {
                    "scope_id": "synthetic-npu",
                    "attestation_public_key": ATTESTATION_PUBLIC_KEY,
                },
                {},
                np.zeros((1, 1), dtype=np.int8),
                (),
            )
            load_inputs.side_effect = RuntimeError("snapshot loaded")
            args = SimpleNamespace(
                endpoint="http://127.0.0.1:1234/v1/embeddings",
                cache=Path(directory) / "cache.npz",
                cache_sha256="1" * 64,
                bearer_token_env=None,
                scifact_snapshot=snapshot,
            )
            with self.assertRaisesRegex(RuntimeError, "snapshot loaded"):
                run_final_package_conformance(args)
            load_inputs.assert_called_once_with(snapshot)

    def test_receipt_launcher_rejects_package_manifest_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory) / "package.zip"
            dispatcher, dispatcher_sha256, manifest_sha256 = self.write_launchable_package(package)
            digest = hashlib.sha256(package.read_bytes()).hexdigest()
            wrong_manifest_sha256 = ("0" if manifest_sha256[0] != "0" else "1") + manifest_sha256[1:]
            with self.assertRaisesRegex(ValueError, "externally pinned"):
                with launch_exact_package(
                    package,
                    digest,
                    dispatcher,
                    dispatcher_sha256,
                    wrong_manifest_sha256,
                    ["synthetic-npu", "synthetic-gpu", "synthetic-cpu"],
                    "synthetic-npu",
                    final_execution("synthetic-npu"),
                    ATTESTATION_PUBLIC_KEY,
                ):
                    self.fail("manifest drift must fail before launch")

    def test_receipt_launcher_rejects_injected_report_cache_or_key_drift(self) -> None:
        cases = (
            ("newest report", {"compatibility_report_sha256": "6" * 64}, None),
            ("cache execution", {"runtime": "other-runtime"}, None),
            ("scope key", {"attestation_public_key": "b" * 64}, None),
            ("semantic profile", None, {"profile_id": "other-profile"}),
        )
        for label, scope_overrides, manifest_overrides in cases:
            with self.subTest(label=label), tempfile.TemporaryDirectory() as directory:
                package = Path(directory) / "package.zip"
                dispatcher, dispatcher_sha256, manifest_sha256 = (
                    self.write_launchable_package(
                        package,
                        requested_scope_overrides=scope_overrides,
                        manifest_overrides=manifest_overrides,
                    )
                )
                digest = hashlib.sha256(package.read_bytes()).hexdigest()
                with self.assertRaisesRegex(
                    ValueError,
                    "compatibility_report_sha256|runtime|attestation key|profile_id",
                ):
                    with launch_exact_package(
                        package,
                        digest,
                        dispatcher,
                        dispatcher_sha256,
                        manifest_sha256,
                        ["synthetic-npu", "synthetic-gpu", "synthetic-cpu"],
                        "synthetic-npu",
                        final_execution("synthetic-npu"),
                        ATTESTATION_PUBLIC_KEY,
                    ):
                        self.fail("manifest/cache/report/key drift must fail before launch")

    def test_receipt_launcher_rejects_a_scope_not_in_the_exact_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory) / "package.zip"
            dispatcher, dispatcher_sha256, manifest_sha256 = self.write_launchable_package(package)
            digest = hashlib.sha256(package.read_bytes()).hexdigest()
            with self.assertRaisesRegex(ValueError, "requested scope"):
                with launch_exact_package(
                    package,
                    digest,
                    dispatcher,
                    dispatcher_sha256,
                    manifest_sha256,
                    ["synthetic-npu", "synthetic-gpu", "synthetic-cpu"],
                    "unpackaged-scope",
                    final_execution("synthetic-npu"),
                    ATTESTATION_PUBLIC_KEY,
                ):
                    self.fail("an unpackaged requested scope must fail before launch")

    def test_exact_package_extraction_rejects_path_escape(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = root / "unsafe.zip"
            with zipfile.ZipFile(package, "w") as archive:
                info = zipfile.ZipInfo("../escape", (1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | 0o755) << 16
                archive.writestr(info, b"unsafe")
            destination = root / "output"
            destination.mkdir()
            with self.assertRaisesRegex(ValueError, "unsafe member"):
                _extract_exact_package(package, destination)
            self.assertFalse((root / "escape").exists())

    def test_writes_content_addressed_stage_and_package_bound_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory)
            private_key = Ed25519PrivateKey.generate()
            private_path = destination / "receipt.key"
            private_path.write_bytes(
                private_key.private_bytes(
                    encoding=serialization.Encoding.Raw,
                    format=serialization.PrivateFormat.Raw,
                    encryption_algorithm=serialization.NoEncryption(),
                )
            )
            args = SimpleNamespace(
                stage_id="1" * 64,
                package_id="synthetic-package",
                package_sha256="2" * 64,
                receipt_directory=destination,
                receipt_attestation_private_key=private_path,
                receipt_attestation_public_key=private_key.public_key()
                .public_bytes(
                    encoding=serialization.Encoding.Raw,
                    format=serialization.PublicFormat.Raw,
                )
                .hex(),
            )
            result = {
                "passed": True,
                "scope_id": "synthetic-scope",
                "cache_sha256": "3" * 64,
                "compatibility_report_sha256": "4" * 64,
                "wire_groupings": SUPPORTED_MAX_BATCH_SIZE,
                "sequence_buckets": len(SEQUENCE_BUCKETS),
                "signed_requests": 351,
            }
            receipt_path = write_conformance_receipt(result, args, 12345)
            raw = receipt_path.read_bytes()
            self.assertEqual(
                receipt_path.name, f"{hashlib.sha256(raw).hexdigest()}.json"
            )
            envelope = json.loads(raw)
            receipt = envelope["receipt"]
            private_key.public_key().verify(
                bytes.fromhex(envelope["signature"]),
                (
                    json.dumps(
                        receipt,
                        ensure_ascii=False,
                        sort_keys=True,
                        separators=(",", ":"),
                    )
                    + "\n"
                ).encode("utf-8"),
            )
            self.assertEqual(receipt["stage_id"], args.stage_id)
            self.assertEqual(receipt["package_sha256"], args.package_sha256)
            self.assertEqual(receipt["package_bytes"], 12345)
            self.assertEqual(
                receipt["admission_implementation_bundle_sha256"],
                ADMISSION_IMPLEMENTATION_BUNDLE_SHA256,
            )

    def test_replays_every_grouping_and_every_bucket_twice(self) -> None:
        wire_inputs = [f"canonical-{index}" for index in range(64)]
        one_wire_output = canonical_i8(deterministic_floats(wire_inputs))
        retained_wire = np.stack(
            [one_wire_output.copy() for _ in range(SUPPORTED_MAX_BATCH_SIZE)]
        )
        report = sequence_report(wire_inputs, one_wire_output)
        retained_probes = retained_probe_outputs()
        wire_calls: list[tuple[str, list[str]]] = []
        probe_calls: list[tuple[list[str], list[dict[str, object]]]] = []

        def wire_request(
            endpoint: str,
            requested_scope_id: str,
            texts: Sequence[str],
            timeout_seconds: float,
            bearer_token: str | None,
        ) -> np.ndarray:
            del endpoint, timeout_seconds, bearer_token
            wire_calls.append((requested_scope_id, list(texts)))
            return deterministic_floats(texts)

        def probe_request(
            texts: Sequence[str], metadata: Sequence[dict[str, object]]
        ) -> np.ndarray:
            probe_calls.append((list(texts), list(metadata)))
            return deterministic_floats(texts)

        replay_retained_outputs(
            "http://127.0.0.1:1234/embeddings",
            "exact-package-scope",
            wire_inputs,
            report,
            retained_wire,
            retained_probes,
            5.0,
            None,
            wire_request,
            probe_request,
        )

        self.assertEqual(
            len(wire_calls),
            sum(
                (SUPPORTED_MAX_BATCH_SIZE + batch_size - 1) // batch_size
                for batch_size in range(1, SUPPORTED_MAX_BATCH_SIZE + 1)
            ),
        )
        self.assertEqual(
            {requested_scope_id for requested_scope_id, _ in wire_calls},
            {"exact-package-scope"},
        )
        self.assertEqual(len(probe_calls), 2 * len(SEQUENCE_BUCKETS))
        for offset, bucket in enumerate(SEQUENCE_BUCKETS):
            first = probe_calls[offset * 2]
            repeat = probe_calls[offset * 2 + 1]
            self.assertEqual(first, repeat)
            self.assertEqual(first[0], list(sequence_semantic_probe_inputs(bucket)))
            self.assertTrue(
                all(row["sequence_bucket"] == bucket for row in first[1])
            )

    def test_rejects_drift_from_retained_wire_or_bucket_bytes(self) -> None:
        wire_inputs = [f"canonical-{index}" for index in range(64)]
        one_wire_output = canonical_i8(deterministic_floats(wire_inputs))
        retained_wire = np.stack(
            [one_wire_output.copy() for _ in range(SUPPORTED_MAX_BATCH_SIZE)]
        )
        report = sequence_report(wire_inputs, one_wire_output)
        retained_probes = retained_probe_outputs()

        def wire_request(
            endpoint: str,
            requested_scope_id: str,
            texts: Sequence[str],
            timeout_seconds: float,
            bearer_token: str | None,
        ) -> np.ndarray:
            del endpoint, requested_scope_id, timeout_seconds, bearer_token
            return deterministic_floats(texts)

        def probe_request(
            texts: Sequence[str], metadata: Sequence[dict[str, object]]
        ) -> np.ndarray:
            del metadata
            return deterministic_floats(texts)

        changed_wire = retained_wire.copy()
        changed_wire[63, 0, 1] += 1
        with self.assertRaisesRegex(ValueError, "wire-grouping outputs"):
            replay_retained_outputs(
                "http://127.0.0.1:1234/embeddings",
                "exact-package-scope",
                wire_inputs,
                report,
                changed_wire,
                retained_probes,
                5.0,
                None,
                wire_request,
                probe_request,
            )

        incomplete_report = json.loads(json.dumps(report))
        incomplete_report["supported_sequence_buckets"] = list(SEQUENCE_BUCKETS[:-1])
        incomplete_report["bucket_results"] = incomplete_report["bucket_results"][:-1]
        with self.assertRaisesRegex(ValueError, "every admitted sequence bucket"):
            replay_retained_outputs(
                "http://127.0.0.1:1234/embeddings",
                "exact-package-scope",
                wire_inputs,
                incomplete_report,
                retained_wire,
                retained_probes,
                5.0,
                None,
                wire_request,
                probe_request,
            )

        changed_probes = tuple(array.copy() for array in retained_probes)
        changed_probes[4][6, 1] += 1
        with self.assertRaisesRegex(
            ValueError, SEQUENCE_PROBE_ARRAY_NAMES[4]
        ):
            replay_retained_outputs(
                "http://127.0.0.1:1234/embeddings",
                "exact-package-scope",
                wire_inputs,
                report,
                retained_wire,
                changed_probes,
                5.0,
                None,
                wire_request,
                probe_request,
            )


if __name__ == "__main__":
    unittest.main()
