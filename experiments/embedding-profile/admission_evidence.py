#!/usr/bin/env python3
"""Shared schema validation for target-native embedding admission evidence."""

from __future__ import annotations

import hashlib
import json
import math
import re
from collections.abc import Mapping
from collections.abc import Sequence

DIMENSIONS = 768
MAX_TOKENS = 2048
SEQUENCE_BUCKETS = [32, 64, 128, 256, 512, 1024, 2048]
SUPPORTED_MAX_BATCH_SIZE = 64
QUERY_PREFIX = "task: search result | query: "
DOCUMENT_PREFIX = "title: none | text: "
WIRE_BATCH_INPUT_SELECTION = (
    "first-32-pinned-scifact-queries-then-first-32-pinned-scifact-documents"
)
WIRE_BATCH_QUERY_ITEMS = 32
WIRE_BATCH_DOCUMENT_ITEMS = 32
SEQUENCE_SEMANTIC_FIXTURE_ID = "cfetch-sequence-semantic-v1-cat-vs-music"
SEQUENCE_SEMANTIC_FIXTURE_SHA256 = (
    "fccd9309f8e97f4f4750ea0d733670ded08e7cc6824da4f6aa66616cd402c417"
)

EVIDENCE_IDENTITY_FIELDS = (
    "scope_id",
    "backend",
    "artifact_source",
    "artifact_sha256",
    "internal_precision",
    "runtime",
    "compiler",
    "package_target",
    "device",
    "device_class",
)


def sequence_semantic_probe_inputs(bucket: int) -> tuple[str, str, str]:
    repetitions = max(1, bucket * 5 // 8 - 4)
    query_topic = " ".join(["cat"] * repetitions)
    irrelevant_topic = " ".join(["music"] * repetitions)
    return (
        QUERY_PREFIX + query_topic,
        DOCUMENT_PREFIX + query_topic,
        DOCUMENT_PREFIX + irrelevant_topic,
    )


def utf8_sha256(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def wire_batch_inputs(
    query_inputs: Sequence[str], document_inputs: Sequence[str]
) -> list[str]:
    if len(query_inputs) < WIRE_BATCH_QUERY_ITEMS:
        raise ValueError(
            f"wire-batch probe needs at least {WIRE_BATCH_QUERY_ITEMS} pinned queries"
        )
    if len(document_inputs) < WIRE_BATCH_DOCUMENT_ITEMS:
        raise ValueError(
            "wire-batch probe needs at least "
            f"{WIRE_BATCH_DOCUMENT_ITEMS} pinned documents"
        )
    return [
        *query_inputs[:WIRE_BATCH_QUERY_ITEMS],
        *document_inputs[:WIRE_BATCH_DOCUMENT_ITEMS],
    ]


def ordered_input_json_sha256(texts: Sequence[str]) -> str:
    encoded = json.dumps(
        list(texts), ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def sequence_semantic_fixture_sha256() -> str:
    manifest = {
        "fixture_id": SEQUENCE_SEMANTIC_FIXTURE_ID,
        "buckets": [
            {
                "bucket": bucket,
                "query": sequence_semantic_probe_inputs(bucket)[0],
                "relevant_document": sequence_semantic_probe_inputs(bucket)[1],
                "irrelevant_document": sequence_semantic_probe_inputs(bucket)[2],
            }
            for bucket in SEQUENCE_BUCKETS
        ],
    }
    encoded = json.dumps(
        manifest, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _reject_duplicate_json_keys(
    pairs: list[tuple[str, object]],
) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON object key {key!r}")
        result[key] = value
    return result


def _reject_nonfinite_json_number(value: str) -> object:
    raise ValueError(f"non-finite JSON number {value}")


def parse_evidence_json(data: bytes, context: str) -> dict[str, object]:
    try:
        report = json.loads(
            data.decode("utf-8"),
            object_pairs_hook=_reject_duplicate_json_keys,
            parse_constant=_reject_nonfinite_json_number,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise ValueError(f"{context} must be UTF-8 JSON") from error
    if not isinstance(report, dict):
        raise ValueError(f"{context} must contain a JSON object")
    return report


def positive_measurement(value: object) -> bool:
    return (
        type(value) in {int, float}
        and math.isfinite(float(value))
        and float(value) > 0.0
    )


def selected_sequence_bucket(
    token_count: object, supported_buckets: list[int]
) -> int | None:
    if type(token_count) is not int or token_count < 1:
        return None
    return next((bucket for bucket in supported_buckets if bucket >= token_count), None)


def bucket_records(
    report: dict[str, object], expected_buckets: list[int], evidence_kind: str
) -> list[dict[str, object]]:
    records = report.get("bucket_results")
    if not isinstance(records, list) or any(not isinstance(row, dict) for row in records):
        raise ValueError(f"{evidence_kind} evidence bucket_results must be an array of objects")
    if [row.get("bucket") for row in records] != expected_buckets:
        raise ValueError(
            f"{evidence_kind} evidence bucket_results must cover exactly {expected_buckets}"
        )
    return records


def validate_wire_batch_evidence(report: dict[str, object]) -> None:
    if report.get("supported_max_batch_size") != SUPPORTED_MAX_BATCH_SIZE:
        raise ValueError(
            "sequence evidence supported_max_batch_size must be "
            f"{SUPPORTED_MAX_BATCH_SIZE}"
        )
    records = report.get("wire_batch_results")
    expected_batch_sizes = list(range(1, SUPPORTED_MAX_BATCH_SIZE + 1))
    if not isinstance(records, list) or any(not isinstance(row, dict) for row in records):
        raise ValueError(
            "sequence evidence wire_batch_results must be an array of objects"
        )
    if [row.get("batch_size") for row in records] != expected_batch_sizes:
        raise ValueError(
            "sequence evidence wire_batch_results must cover exactly batch sizes "
            f"{expected_batch_sizes}"
        )
    input_digests: set[str] = set()
    output_digests: set[str] = set()
    for row, batch_size in zip(records, expected_batch_sizes, strict=True):
        expected_fields = {
            "batch_size",
            "input_count",
            "request_count",
            "response_row_count",
            "ordered_input_json_sha256",
            "canonical_output_bytes_sha256",
        }
        if set(row) != expected_fields:
            raise ValueError(
                f"sequence evidence batch {batch_size} must contain exactly "
                f"{sorted(expected_fields)}"
            )
        expected = {
            "input_count": SUPPORTED_MAX_BATCH_SIZE,
            "request_count": (SUPPORTED_MAX_BATCH_SIZE + batch_size - 1) // batch_size,
            "response_row_count": SUPPORTED_MAX_BATCH_SIZE,
        }
        for field, value in expected.items():
            if row.get(field) != value:
                raise ValueError(
                    f"sequence evidence batch {batch_size} {field} must be {value}"
                )
        input_digest = row.get("ordered_input_json_sha256")
        output_digest = row.get("canonical_output_bytes_sha256")
        for field, digest in (
            ("ordered_input_json_sha256", input_digest),
            ("canonical_output_bytes_sha256", output_digest),
        ):
            if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
                raise ValueError(
                    f"sequence evidence batch {batch_size} needs {field}"
                )
        input_digests.add(input_digest)
        output_digests.add(output_digest)
    grouping = report.get("grouping_invariance")
    if not isinstance(grouping, dict):
        raise ValueError("sequence evidence grouping_invariance must be an object")
    grouping_fields = {
        "batch_sizes",
        "input_selection",
        "same_inputs_in_same_order",
        "canonical_output_bytes_equal",
    }
    if set(grouping) != grouping_fields:
        raise ValueError(
            "sequence evidence grouping_invariance must contain exactly "
            f"{sorted(grouping_fields)}"
        )
    for field, expected in {
        "batch_sizes": expected_batch_sizes,
        "input_selection": WIRE_BATCH_INPUT_SELECTION,
        "same_inputs_in_same_order": True,
        "canonical_output_bytes_equal": True,
    }.items():
        if grouping.get(field) != expected:
            raise ValueError(
                f"sequence evidence grouping_invariance {field} must be {expected!r}"
            )
    if len(input_digests) != 1:
        raise ValueError(
            "sequence evidence batch sizes 1 through 64 use different ordered inputs"
        )
    if len(output_digests) != 1:
        raise ValueError(
            "sequence evidence batch sizes 1 through 64 canonical outputs differ"
        )


def _source_value(source: object, name: str) -> object:
    if isinstance(source, Mapping):
        return source.get(name)
    return getattr(source, name, None)


def _supported_buckets(source: object) -> list[int]:
    value = _source_value(source, "supported_sequence_buckets")
    if value is None:
        value = _source_value(source, "supported_sequence_bucket")
    if not isinstance(value, list) or any(type(item) is not int for item in value):
        raise ValueError("admission evidence scope needs supported sequence buckets")
    return sorted(set(value))


def validate_evidence_reports(
    scope: object,
    sequence_report: dict[str, object],
    placement_report: dict[str, object],
    performance_report: dict[str, object],
) -> None:
    """Validate every embedded evidence report against one exact cache scope."""
    evidence_scope = {field: _source_value(scope, field) for field in EVIDENCE_IDENTITY_FIELDS}
    if any(value is None for value in evidence_scope.values()):
        raise ValueError("admission evidence scope identity is incomplete")
    for kind, report in (
        ("sequence", sequence_report),
        ("placement", placement_report),
        ("performance", performance_report),
    ):
        for field, expected in evidence_scope.items():
            if report.get(field) != expected:
                raise ValueError(
                    f"{kind} evidence {field}={report.get(field)!r}, expected {expected!r}"
                )

    sequence_fields = {
        *EVIDENCE_IDENTITY_FIELDS,
        "supported_max_tokens",
        "supported_sequence_buckets",
        "supported_max_batch_size",
        "wire_batch_results",
        "grouping_invariance",
        "bucket_results",
    }
    placement_fields = {
        *EVIDENCE_IDENTITY_FIELDS,
        "accelerated_placement",
        "accelerator_execution_confirmed",
        "fallback_disclosure_complete",
        "unexpected_fallback_detected",
        "bucket_results",
    }
    performance_fields = {*EVIDENCE_IDENTITY_FIELDS, "bucket_results"}
    for kind, report, fields in (
        ("sequence", sequence_report, sequence_fields),
        ("placement", placement_report, placement_fields),
        ("performance", performance_report, performance_fields),
    ):
        if set(report) != fields:
            raise ValueError(
                f"{kind} evidence must contain exactly {sorted(fields)}"
            )

    supported_max_tokens = _source_value(scope, "supported_max_tokens")
    supported_buckets = _supported_buckets(scope)
    if sequence_report.get("supported_max_tokens") != supported_max_tokens:
        raise ValueError(
            "sequence evidence supported_max_tokens does not match the scope attestation"
        )
    if sequence_report.get("supported_sequence_buckets") != supported_buckets:
        raise ValueError(
            "sequence evidence supported_sequence_buckets do not match the scope attestation"
        )
    validate_wire_batch_evidence(sequence_report)
    for row in bucket_records(sequence_report, supported_buckets, "sequence"):
        bucket = row["bucket"]
        sequence_bucket_fields = {
            "bucket",
            "requested_tokens",
            "tokenized_tokens",
            "executed_shape_tokens",
            "output_dimensions",
            "finite_output",
            "nonzero_output",
            "truncated",
            "semantic_probe",
        }
        if set(row) != sequence_bucket_fields:
            raise ValueError(
                f"sequence evidence bucket {bucket} must contain exactly "
                f"{sorted(sequence_bucket_fields)}"
            )
        expected = {
            "requested_tokens": bucket,
            "tokenized_tokens": bucket,
            "executed_shape_tokens": bucket,
            "output_dimensions": DIMENSIONS,
            "finite_output": True,
            "nonzero_output": True,
            "truncated": False,
        }
        for field, value in expected.items():
            if row.get(field) != value:
                raise ValueError(
                    f"sequence evidence bucket {bucket} {field}={row.get(field)!r}, "
                    f"expected {value!r}"
                )
        probe = row.get("semantic_probe")
        probe_fields = {
            "fixture_id",
            "fixture_sha256",
            "query_input_utf8_sha256",
            "relevant_document_input_utf8_sha256",
            "irrelevant_document_input_utf8_sha256",
            "query_token_count",
            "relevant_document_token_count",
            "irrelevant_document_token_count",
            "query_canonical_output_bytes_sha256",
            "relevant_document_canonical_output_bytes_sha256",
            "irrelevant_document_canonical_output_bytes_sha256",
            "canonical_repeatability",
            "self_relevant_before_irrelevant",
        }
        if not isinstance(probe, dict) or set(probe) != probe_fields:
            raise ValueError(
                f"sequence evidence bucket {bucket} semantic_probe must contain "
                f"exactly {sorted(probe_fields)}"
            )
        if probe["fixture_id"] != SEQUENCE_SEMANTIC_FIXTURE_ID:
            raise ValueError(
                f"sequence evidence bucket {bucket} semantic_probe fixture_id "
                "does not match the pinned fixture"
            )
        if probe["fixture_sha256"] != SEQUENCE_SEMANTIC_FIXTURE_SHA256:
            raise ValueError(
                f"sequence evidence bucket {bucket} semantic_probe fixture_sha256 "
                "does not match the pinned fixture"
            )
        for label, text in zip(
            ("query", "relevant_document", "irrelevant_document"),
            sequence_semantic_probe_inputs(bucket),
            strict=True,
        ):
            if probe.get(f"{label}_input_utf8_sha256") != utf8_sha256(text):
                raise ValueError(
                    f"sequence evidence bucket {bucket} {label} input digest "
                    "does not match the pinned semantic probe"
                )
            token_count = probe.get(f"{label}_token_count")
            if selected_sequence_bucket(token_count, supported_buckets) != bucket:
                raise ValueError(
                    f"sequence evidence bucket {bucket} {label}_token_count must "
                    "select this bucket under the smallest-fitting-bucket rule"
                )
            output_digest = probe.get(f"{label}_canonical_output_bytes_sha256")
            if not isinstance(output_digest, str) or re.fullmatch(
                r"[0-9a-f]{64}", output_digest
            ) is None:
                raise ValueError(
                    f"sequence evidence bucket {bucket} {label} needs a canonical "
                    "output digest"
                )
        if probe["canonical_repeatability"] is not True:
            raise ValueError(
                f"sequence evidence bucket {bucket} semantic probe must attest "
                "canonical_repeatability=true"
            )
        if probe["self_relevant_before_irrelevant"] is not True:
            raise ValueError(
                f"sequence evidence bucket {bucket} semantic probe must attest "
                "self_relevant_before_irrelevant=true"
            )

    for field, expected in {
        "accelerated_placement": True,
        "accelerator_execution_confirmed": True,
        "fallback_disclosure_complete": True,
        "unexpected_fallback_detected": False,
        "device": evidence_scope["device"],
        "device_class": evidence_scope["device_class"],
        "runtime": evidence_scope["runtime"],
    }.items():
        if placement_report.get(field) != expected:
            raise ValueError(
                f"placement evidence {field}={placement_report.get(field)!r}, "
                f"expected {expected!r}"
            )
    for row in bucket_records(placement_report, supported_buckets, "placement"):
        bucket = row["bucket"]
        placement_bucket_fields = {
            "bucket",
            "accelerator_execution_confirmed",
            "fallback_disclosure_complete",
            "unexpected_fallback_detected",
            "fallback_summary",
            "profiler_output_sha256",
        }
        if set(row) != placement_bucket_fields:
            raise ValueError(
                f"placement evidence bucket {bucket} must contain exactly "
                f"{sorted(placement_bucket_fields)}"
            )
        if row.get("accelerator_execution_confirmed") is not True:
            raise ValueError(
                f"placement evidence bucket {bucket} does not confirm accelerator execution"
            )
        if row.get("fallback_disclosure_complete") is not True:
            raise ValueError(
                f"placement evidence bucket {bucket} has incomplete fallback disclosure"
            )
        if row.get("unexpected_fallback_detected") is not False:
            raise ValueError(
                f"placement evidence bucket {bucket} records unexpected fallback"
            )
        fallback_summary = row.get("fallback_summary")
        if not isinstance(fallback_summary, str) or not fallback_summary.strip():
            raise ValueError(
                f"placement evidence bucket {bucket} needs a fallback_summary"
            )
        profiler_digest = row.get("profiler_output_sha256")
        if not isinstance(profiler_digest, str) or re.fullmatch(
            r"[0-9a-f]{64}", profiler_digest
        ) is None:
            raise ValueError(
                f"placement evidence bucket {bucket} needs profiler_output_sha256"
            )

    for row in bucket_records(performance_report, supported_buckets, "performance"):
        bucket = row["bucket"]
        measured_fields = {
            "bucket",
            "sample_count",
            "benchmark_output_sha256",
            "latency_ms_p50",
            "latency_ms_p95",
            "peak_memory_bytes",
            "energy_joules",
            "average_power_watts",
        }
        unmeasured_fields = {
            "bucket",
            "sample_count",
            "benchmark_output_sha256",
            "latency_ms_p50",
            "latency_ms_p95",
            "peak_memory_bytes",
            "energy_measurement",
            "energy_not_measured_reason",
        }
        if frozenset(row) not in {
            frozenset(measured_fields),
            frozenset(unmeasured_fields),
        }:
            raise ValueError(
                f"performance evidence bucket {bucket} energy_joules and related "
                "fields must use exactly the measured or explicitly-unmeasured schema"
            )
        benchmark_digest = row.get("benchmark_output_sha256")
        if not isinstance(benchmark_digest, str) or re.fullmatch(
            r"[0-9a-f]{64}", benchmark_digest
        ) is None:
            raise ValueError(
                f"performance evidence bucket {bucket} needs benchmark_output_sha256"
            )
        for field in ("latency_ms_p50", "latency_ms_p95"):
            if not positive_measurement(row.get(field)):
                raise ValueError(
                    f"performance evidence bucket {bucket} {field} must be finite and positive"
                )
        if type(row.get("peak_memory_bytes")) is not int or row["peak_memory_bytes"] < 1:
            raise ValueError(
                f"performance evidence bucket {bucket} peak_memory_bytes must be a "
                "positive integer"
            )
        if float(row["latency_ms_p95"]) < float(row["latency_ms_p50"]):
            raise ValueError(
                f"performance evidence bucket {bucket} p95 latency is below p50"
            )
        if type(row.get("sample_count")) is not int or row["sample_count"] < 1:
            raise ValueError(
                f"performance evidence bucket {bucket} sample_count must be positive"
            )
        energy_joules = row.get("energy_joules")
        average_power_watts = row.get("average_power_watts")
        measured = positive_measurement(energy_joules) and positive_measurement(
            average_power_watts
        )
        explicitly_unmeasured = row.get("energy_measurement") == "not_measured"
        if measured:
            if explicitly_unmeasured:
                raise ValueError(
                    f"performance evidence bucket {bucket} has ambiguous energy evidence"
                )
        elif explicitly_unmeasured:
            reason = row.get("energy_not_measured_reason")
            if not isinstance(reason, str) or not reason.strip():
                raise ValueError(
                    f"performance evidence bucket {bucket} needs a nonempty "
                    "energy_not_measured_reason"
                )
            if "energy_joules" in row or "average_power_watts" in row:
                raise ValueError(
                    f"performance evidence bucket {bucket} mixes measured and "
                    "not_measured energy evidence"
                )
        else:
            raise ValueError(
                f"performance evidence bucket {bucket} must contain positive "
                "energy_joules and average_power_watts or explicitly record "
                "energy_measurement=not_measured with a reason"
            )


if sequence_semantic_fixture_sha256() != SEQUENCE_SEMANTIC_FIXTURE_SHA256:
    raise RuntimeError("sequence semantic fixture digest is stale")
