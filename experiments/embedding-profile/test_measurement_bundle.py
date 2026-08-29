#!/usr/bin/env python3
"""Unit tests for deterministic measurement evidence packaging."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from measurement_bundle import build_measurement_bundle, file_sha256


class MeasurementBundleTests(unittest.TestCase):
    def test_same_inputs_produce_the_same_content_addressed_zip(self) -> None:
        with tempfile.TemporaryDirectory() as raw_name, tempfile.TemporaryDirectory() as out_name:
            raw = Path(raw_name)
            output = Path(out_name)
            profiler = raw / "profiler.txt"
            benchmark = raw / "benchmark.txt"
            wire = raw / "wire.txt"
            profiler.write_bytes(b"physical placement output\n")
            benchmark.write_bytes(b"physical benchmark output\n")
            wire.write_bytes(b"signed wire transaction output\n")
            profiler_digest = file_sha256(profiler)
            benchmark_digest = file_sha256(benchmark)
            wire_digest = file_sha256(wire)
            metadata = {
                "scope_id": "test-cpu-scope",
                "sequence_capability_evidence_sha256": "0" * 64,
                "placement_evidence_sha256": "1" * 64,
                "performance_evidence_sha256": "2" * 64,
            }
            reports = {
                "sequence": {
                    "wire_batch_results": [
                        {"signed_transactions_sha256": wire_digest}
                    ]
                },
                "placement": {
                    "bucket_results": [
                        {"profiler_output_sha256": profiler_digest}
                    ]
                },
                "performance": {
                    "bucket_results": [
                        {"benchmark_output_sha256": benchmark_digest}
                    ]
                },
            }
            with (
                patch("measurement_bundle.load_cache", return_value=(metadata, None, None)),
                patch(
                    "measurement_bundle.load_embedded_evidence_reports",
                    return_value=reports,
                ),
                patch("measurement_bundle.validate_measurement_bundle") as validate,
            ):
                first = build_measurement_bundle(Path("cache.npz"), raw, output)
                first_bytes = first.read_bytes()
                second = build_measurement_bundle(Path("cache.npz"), raw, output)
            self.assertEqual(first, second)
            self.assertEqual(first_bytes, second.read_bytes())
            self.assertEqual(first.name, f"{file_sha256(first)}.zip")
            validate.assert_called()

    def test_missing_raw_digest_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_name, tempfile.TemporaryDirectory() as out_name:
            raw = Path(raw_name)
            (raw / "unrelated.txt").write_text("not the required bytes")
            metadata = {
                "scope_id": "test-cpu-scope",
                "sequence_capability_evidence_sha256": "0" * 64,
                "placement_evidence_sha256": "1" * 64,
                "performance_evidence_sha256": "2" * 64,
            }
            reports = {
                "sequence": {
                    "wire_batch_results": [
                        {"signed_transactions_sha256": "c" * 64}
                    ]
                },
                "placement": {
                    "bucket_results": [{"profiler_output_sha256": "a" * 64}]
                },
                "performance": {
                    "bucket_results": [{"benchmark_output_sha256": "b" * 64}]
                },
            }
            with (
                patch("measurement_bundle.load_cache", return_value=(metadata, None, None)),
                patch(
                    "measurement_bundle.load_embedded_evidence_reports",
                    return_value=reports,
                ),
            ):
                with self.assertRaisesRegex(ValueError, "missing evidence bytes"):
                    build_measurement_bundle(
                        Path("cache.npz"), raw, Path(out_name)
                    )

    def test_unreferenced_raw_bytes_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_name, tempfile.TemporaryDirectory() as out_name:
            raw = Path(raw_name)
            required = raw / "required.txt"
            required.write_bytes(b"required raw output")
            (raw / "unreferenced.txt").write_bytes(b"not declared by evidence")
            digest = file_sha256(required)
            metadata = {
                "scope_id": "test-cpu-scope",
                "sequence_capability_evidence_sha256": "0" * 64,
                "placement_evidence_sha256": "1" * 64,
                "performance_evidence_sha256": "2" * 64,
            }
            reports = {
                "sequence": {
                    "wire_batch_results": [
                        {"signed_transactions_sha256": digest}
                    ]
                },
                "placement": {
                    "bucket_results": [{"profiler_output_sha256": digest}]
                },
                "performance": {
                    "bucket_results": [{"benchmark_output_sha256": digest}]
                },
            }
            with (
                patch("measurement_bundle.load_cache", return_value=(metadata, None, None)),
                patch(
                    "measurement_bundle.load_embedded_evidence_reports",
                    return_value=reports,
                ),
            ):
                with self.assertRaisesRegex(ValueError, "unreferenced evidence"):
                    build_measurement_bundle(Path("cache.npz"), raw, Path(out_name))


if __name__ == "__main__":
    unittest.main()
