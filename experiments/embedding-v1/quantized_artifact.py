#!/usr/bin/env python3
"""Build and audit a candidate quantized cfetch EmbeddingGemma artifact."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
from typing import Iterator

import numpy as np
import onnx
import onnxruntime as ort
from onnxruntime.quantization import CalibrationDataReader
from quark.onnx import CalibrationMethod, ModelQuantizer, QConfig
from transformers import AutoTokenizer

from profile_data import (
    calibration_text,
    DIMENSIONS,
    DOCUMENT_PREFIX,
    KAT_CASES,
    QUERY_PREFIX,
    SEQUENCE_BUCKETS,
)

def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


class TextCalibrationReader(CalibrationDataReader):
    def __init__(
        self,
        tokenizer: object,
        samples: int,
        max_tokens: int,
        fixed_tensor_shape: bool,
    ):
        lengths = tuple(length for length in (16, 32, 64, 128, 256, 512, 1024, 2048) if length <= max_tokens)
        if not lengths:
            raise ValueError("max_tokens must be at least 16")
        self._rows: list[dict[str, np.ndarray]] = []
        for index in range(samples):
            target = lengths[index % len(lengths)]
            tensor_length = max_tokens if fixed_tensor_shape else target
            encoded = tokenizer(
                calibration_text(index, target),
                add_special_tokens=True,
                max_length=tensor_length,
                padding="max_length",
                truncation=True,
                return_tensors="np",
            )
            self._rows.append(
                {
                    "input_ids": np.asarray(encoded["input_ids"], dtype=np.int64),
                    "attention_mask": np.asarray(encoded["attention_mask"], dtype=np.int64),
                }
            )
        self._iterator: Iterator[dict[str, np.ndarray]] | None = None

    def get_next(self) -> dict[str, np.ndarray] | None:
        if self._iterator is None:
            self._iterator = iter(self._rows)
        return next(self._iterator, None)

    def rewind(self) -> None:
        self._iterator = None


def canonical_vector(vector: np.ndarray) -> bytes:
    # Match Rust's index::vec_to_blob exactly: f32 arithmetic, per-vector
    # max-abs scale, IEEE round-to-nearest-even, signed [-127, 127]. The graph
    # output is already L2-normalized and a second normalization would add
    # avoidable floating-point drift before the byte contract.
    vector = np.asarray(vector, dtype=np.float32)
    if not np.all(np.isfinite(vector)):
        raise ValueError("vector contains a non-finite component")
    maximum = np.max(np.abs(vector)).astype(np.float32)
    if maximum <= 0:
        raise ValueError(f"degenerate vector maximum: {maximum}")
    scaled = vector / maximum * np.float32(127.0)
    encoded = np.rint(np.clip(scaled, -127.0, 127.0)).astype(np.int8)
    if encoded.shape != (DIMENSIONS,):
        raise ValueError(f"expected {DIMENSIONS} dimensions, got {encoded.shape}")
    return encoded.tobytes()


def run_outputs(model: Path, tokenizer: object) -> np.ndarray:
    session_options = ort.SessionOptions()
    # FastEmbed configures ORT Level 3. Q/DQ fusion changes this graph's
    # numerical execution materially, so a no-optimization reference would
    # certify vectors the shipping runtime never produces.
    session_options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    session_options.intra_op_num_threads = 1
    session_options.inter_op_num_threads = 1
    session = ort.InferenceSession(str(model), session_options, providers=["CPUExecutionProvider"])
    texts = [
        (QUERY_PREFIX if case.kind == "query" else DOCUMENT_PREFIX) + case.text
        for case in KAT_CASES
    ]
    outputs = []
    for text in texts:
        token_count = len(
            tokenizer(
                text,
                add_special_tokens=True,
                padding=False,
                truncation=True,
                max_length=2048,
            )["input_ids"]
        )
        bucket = next(
            (item for item in SEQUENCE_BUCKETS if token_count <= item),
            SEQUENCE_BUCKETS[-1],
        )
        encoded = tokenizer(
            [text],
            padding="max_length",
            truncation=True,
            max_length=bucket,
            return_tensors="np",
        )
        outputs.append(
            session.run(
                ["sentence_embedding"],
                {
                    "input_ids": np.asarray(encoded["input_ids"], dtype=np.int64),
                    "attention_mask": np.asarray(
                        encoded["attention_mask"], dtype=np.int64
                    ),
                },
            )[0]
        )
    return np.concatenate(outputs)


def run_kat(
    model: Path, tokenizer: object, reference_model: Path | None = None
) -> tuple[list[dict[str, object]], dict[str, object]]:
    outputs = run_outputs(model, tokenizer)
    canonical = [canonical_vector(output) for output in outputs]
    if len(set(canonical)) != len(canonical):
        raise ValueError("candidate collapses distinct known-answer inputs to identical vectors")

    answers = [
        {
            "label": case.label,
            "kind": case.kind,
            "text": case.text,
            "expected_bucket": case.expected_bucket,
            "vector_sha256": hashlib.sha256(encoded).hexdigest(),
            "vector_hex": encoded.hex(),
        }
        for case, encoded in zip(KAT_CASES, canonical, strict=True)
    ]
    if reference_model is None:
        return answers, {
            "known_answer_vectors": len(canonical),
            "known_answer_vectors_distinct": len(set(canonical)),
            "source_inference": "not run; full-precision export is not a cfetch runtime",
        }

    references = run_outputs(reference_model, tokenizer)
    cosines = []
    exact_matches = 0
    for output, reference in zip(outputs, references, strict=True):
        denominator = np.linalg.norm(output) * np.linalg.norm(reference)
        cosines.append(float(np.dot(output, reference) / denominator))
        exact_matches += canonical_vector(output) == canonical_vector(reference)

    for answer, cosine in zip(answers, cosines, strict=True):
        answer["reference_cosine"] = cosine
    return answers, {
        "minimum_reference_cosine": min(cosines),
        "mean_reference_cosine": float(np.mean(cosines)),
        "canonical_exact_matches": exact_matches,
    }


def scale_audit(model: onnx.ModelProto) -> tuple[int, int, list[dict[str, object]]]:
    invalid: list[dict[str, object]] = []
    count = 0
    power_of_two = 0
    for initializer in model.graph.initializer:
        if not initializer.name.endswith("_scale"):
            continue
        values = onnx.numpy_helper.to_array(initializer).astype(np.float64).reshape(-1)
        for value in values:
            count += 1
            valid = value > 0 and math.isfinite(float(value))
            exponent = math.log2(float(value)) if valid else math.nan
            if not valid:
                invalid.append({"initializer": initializer.name, "value": float(value)})
            elif abs(exponent - round(exponent)) <= 1e-6:
                power_of_two += 1
    return count, power_of_two, invalid


def int8_qdq_audit(
    model: onnx.ModelProto, selected_nodes: list[str]
) -> dict[str, object]:
    """Prove that every selected learned input is signed symmetric INT8 Q/DQ."""
    initializers = {item.name: item for item in model.graph.initializer}
    nodes = {node.name: node for node in model.graph.node}
    producers = {
        output: node for node in model.graph.node for output in node.output
    }
    zero_point_types: Counter[str] = Counter()
    zero_point_ranges: Counter[tuple[int, int]] = Counter()
    qdq_nodes = 0
    for node in model.graph.node:
        if node.op_type not in {"QuantizeLinear", "DequantizeLinear"}:
            continue
        qdq_nodes += 1
        if len(node.input) < 3 or node.input[2] not in initializers:
            raise ValueError(f"{node.name} has no static INT8 zero point")
        zero_point = initializers[node.input[2]]
        data_type = onnx.TensorProto.DataType.Name(zero_point.data_type)
        values = onnx.numpy_helper.to_array(zero_point).reshape(-1)
        zero_point_types[data_type] += 1
        zero_point_ranges[(int(values.min()), int(values.max()))] += 1
    if set(zero_point_types) != {"INT8"} or set(zero_point_ranges) != {(0, 0)}:
        raise ValueError(
            "Q/DQ graph is not uniformly signed symmetric INT8: "
            f"types={dict(zero_point_types)}, ranges={dict(zero_point_ranges)}"
        )

    learned_ops: Counter[str] = Counter()
    for name in selected_nodes:
        node = nodes.get(name)
        if node is None:
            raise ValueError(f"selected learned node disappeared after quantization: {name}")
        learned_ops[node.op_type] += 1
        # MatMul has two numerical inputs; Gather's learned embedding table is
        # input zero while its token indices intentionally remain INT64.
        required = range(2) if node.op_type == "MatMul" else range(1)
        for index in required:
            producer = producers.get(node.input[index])
            if producer is None or producer.op_type != "DequantizeLinear":
                raise ValueError(
                    f"selected {node.op_type} node {name} input {index} is not INT8 Q/DQ"
                )

    return {
        "format": "ONNX opset-18 QuantizeLinear/DequantizeLinear",
        "qdq_nodes": qdq_nodes,
        "zero_point_type": "INT8",
        "zero_point_value": 0,
        "learned_nodes": len(selected_nodes),
        "learned_ops": dict(sorted(learned_ops.items())),
        "learned_inputs_qdq_covered": True,
    }


def standardize_qdq_domains(model: onnx.ModelProto) -> int:
    """Replace Quark's legacy Microsoft Q/DQ spelling with portable ONNX ops.

    Quark emits otherwise-standard INT8 QuantizeLinear/DequantizeLinear nodes
    in the ``com.microsoft`` domain. Opset 18 supports the same per-axis
    contract in the default ONNX domain, and ONNX's full checker accepts the
    rewritten graph. Keeping the vendor domain would make provider matching
    needlessly depend on a Microsoft compatibility alias before the graph
    ever reaches Core ML, OpenVINO, QNN, or Vitis AI.
    """
    initializers = {item.name: item for item in model.graph.initializer}
    changed = 0
    for node in model.graph.node:
        if node.domain == "com.microsoft" and node.op_type in {
            "QuantizeLinear",
            "DequantizeLinear",
        }:
            if len(node.input) < 3 or node.input[1] not in initializers or node.input[2] not in initializers:
                raise ValueError(f"cannot standardize dynamic Q/DQ parameters on {node.name}")
            scale = initializers[node.input[1]]
            zero_point = initializers[node.input[2]]
            if scale.data_type != onnx.TensorProto.FLOAT or zero_point.data_type not in {
                onnx.TensorProto.INT8,
                onnx.TensorProto.UINT8,
            }:
                raise ValueError(f"cannot standardize non-INT8 Q/DQ node {node.name}")
            if any(attribute.name != "axis" for attribute in node.attribute):
                raise ValueError(f"cannot standardize vendor Q/DQ attributes on {node.name}")
            node.domain = ""
            changed += 1
    used_domains = {node.domain for node in model.graph.node}
    imports = [
        item
        for item in model.opset_import
        if item.domain == "" or item.domain in used_domains
    ]
    del model.opset_import[:]
    model.opset_import.extend(imports)
    onnx.checker.check_model(model, full_check=True)
    return changed


def repair_quantizer_shapes(
    model: onnx.ModelProto, source: onnx.ModelProto
) -> dict[str, int]:
    """Restore the interface and remove bogus optional Quark shapes.

    Quark duplicates many optional ``value_info`` entries, and its
    AdaRound/AdaQuant pass additionally records dynamic batch and sequence
    dimensions as literal zeroes on graph outputs and intermediates. The graph
    dataflow remains dynamic, but ORT trusts those annotations and rejects a
    later ``Reshape([-1, ...])`` or ``Expand``. Restore the exact pinned source
    interface and discard only optional annotations that are invalid,
    duplicated, or shadow that interface.
    """
    source_inputs = {value.name: value for value in source.graph.input}
    source_outputs = {value.name: value for value in source.graph.output}
    restored = 0
    for values, references in (
        (model.graph.input, source_inputs),
        (model.graph.output, source_outputs),
    ):
        for value in values:
            if reference := references.get(value.name):
                value.type.CopyFrom(reference.type)
                restored += 1

    interface_names = {
        value.name for value in (*model.graph.input, *model.graph.output)
    }
    retained = []
    removed = 0
    seen: set[str] = set()
    for value in model.graph.value_info:
        dimensions = value.type.tensor_type.shape.dim
        invalid = any(
            dimension.HasField("dim_value") and dimension.dim_value == 0
            for dimension in dimensions
        )
        if value.name in interface_names or value.name in seen or invalid:
            removed += 1
        else:
            retained.append(value)
            seen.add(value.name)
    del model.graph.value_info[:]
    model.graph.value_info.extend(retained)
    return {
        "restored_interface_shapes": restored,
        "discarded_value_info": removed,
    }


def stamp_distribution_metadata(model: onnx.ModelProto) -> None:
    """Carry the Gemma derivative notice inside the modified ONNX file."""
    additions = {
        "cfetch.artifact": "cfetch-embeddinggemma-300m-a8w8-v1",
        "cfetch.modified": (
            "Modified by cfetch contributors: exported from the pinned "
            "EmbeddingGemma checkpoint to ONNX opset 18, statically quantized "
            "to the cfetch v1 signed W8A8 Q/DQ profile, and exported with its "
            "masked-mean pooling, projection, and L2-normalization pipeline."
        ),
        "cfetch.source_model": "google/embeddinggemma-300m-qat-q8_0-unquantized",
        "cfetch.source_revision": "7b5b24595322ab0ea4d08827066860a6df8cb0aa",
        "gemma.terms": "https://ai.google.dev/gemma/terms",
    }
    retained = [item for item in model.metadata_props if item.key not in additions]
    del model.metadata_props[:]
    model.metadata_props.extend(retained)
    for key, value in sorted(additions.items()):
        entry = model.metadata_props.add()
        entry.key = key
        entry.value = value


def transformer_quantization_nodes(
    model: onnx.ModelProto, selection: str
) -> list[str]:
    """Select numerical transformer regions without quantizing shape/control data."""
    initializers = {initializer.name: initializer for initializer in model.graph.initializer}
    float_types = {
        onnx.TensorProto.FLOAT,
        onnx.TensorProto.FLOAT16,
        onnx.TensorProto.BFLOAT16,
        onnx.TensorProto.DOUBLE,
    }
    selected: list[str] = []
    for node in model.graph.node:
        has_float_initializer = any(
            (initializer := initializers.get(name)) is not None
            and initializer.data_type in float_types
            for name in node.input
        )
        quantize = node.op_type == "MatMul" and (
            selection == "all-matmul" or has_float_initializer
        )
        if node.op_type == "Gather" and selection != "weighted-matmul-no-embedding":
            quantize = has_float_initializer
        if quantize:
            if not node.name:
                raise ValueError(f"selected {node.op_type} node has no stable name")
            selected.append(node.name)
    if len(selected) != len(set(selected)):
        raise ValueError("selected quantization node names are not unique")
    if not selected:
        raise ValueError("model contains no transformer nodes to quantize")
    return selected


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--tokenizer", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--samples", type=int, default=128)
    parser.add_argument("--max-tokens", type=int, default=2048)
    parser.add_argument(
        "--compare-source-kat",
        action="store_true",
        help="adverse build-time diagnostic only; never used by a release/runtime",
    )
    parser.add_argument(
        "--quantization",
        choices=(
            "A8W8",
            "A8W8_ADAROUND",
            "A8W8_ADAQUANT",
            "XINT8",
            "XINT8_ADAROUND",
            "XINT8_ADAQUANT",
        ),
        default="A8W8",
    )
    parser.add_argument(
        "--calibration",
        choices=("minmax", "percentile", "entropy"),
        default="minmax",
        help="activation range estimator; release recipe uses deterministic Entropy plus SmoothQuant",
    )
    parser.add_argument(
        "--fast-finetune-iterations",
        type=int,
        default=1000,
        help="deterministic AdaRound/AdaQuant refinement steps",
    )
    parser.add_argument(
        "--fast-finetune-memory-level",
        type=int,
        choices=(0, 1, 2),
        default=1,
        help=(
            "Quark activation-cache strategy: 0 caches reference activations in RAM, "
            "1 recomputes them per layer, and 2 spills them to disk"
        ),
    )
    parser.add_argument(
        "--fixed-calibration-shape",
        action="store_true",
        help=(
            "pad varied calibration content into one max-token tensor shape; "
            "required by Quark AdaRound/AdaQuant"
        ),
    )
    parser.add_argument(
        "--per-channel-weights",
        action="store_true",
        help="use one INT8 scale per weight output channel instead of per tensor",
    )
    parser.add_argument(
        "--hardware-optimize",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="apply Quark's NPU graph rewrites; off keeps the canonical ONNX Q/DQ portable",
    )
    parser.add_argument(
        "--smooth-alpha",
        type=float,
        help="enable SmoothQuant with this deterministic activation/weight tradeoff",
    )
    parser.add_argument(
        "--node-selection",
        choices=("weighted-matmul", "weighted-matmul-no-embedding", "all-matmul"),
        default="weighted-matmul",
        help="INT8 learned matrices only, or learned plus activation-only attention MatMuls",
    )
    args = parser.parse_args()

    if args.output.exists():
        raise SystemExit(f"refusing to overwrite existing artifact: {args.output}")
    model_path = args.model.resolve()
    tokenizer_path = args.tokenizer.resolve()
    output_path = args.output.resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)

    if ("ADAROUND" in args.quantization or "ADAQUANT" in args.quantization) and not args.fixed_calibration_shape:
        raise SystemExit(
            "AdaRound/AdaQuant requires --fixed-calibration-shape; Quark otherwise "
            "skips heterogeneous transformer layers instead of failing the build"
        )
    tokenizer = AutoTokenizer.from_pretrained(tokenizer_path, local_files_only=True)
    calibration = TextCalibrationReader(
        tokenizer,
        args.samples,
        args.max_tokens,
        args.fixed_calibration_shape,
    )
    source_graph = onnx.load(model_path, load_external_data=False)
    selected_nodes = transformer_quantization_nodes(source_graph, args.node_selection)
    config = QConfig.get_default_config(args.quantization)
    config.global_quant_config.per_channel = args.per_channel_weights
    config.global_quant_config.optimize_model = args.hardware_optimize
    if args.smooth_alpha is not None:
        if not 0.0 <= args.smooth_alpha <= 1.0:
            raise SystemExit("--smooth-alpha must be between 0 and 1")
        config.global_quant_config.include_sq = True
        config.global_quant_config.extra_options["SmoothAlpha"] = args.smooth_alpha
    config.global_quant_config.calibrate_method = {
        "minmax": CalibrationMethod.MinMax,
        "percentile": CalibrationMethod.Percentile,
        "entropy": CalibrationMethod.Entropy,
    }[args.calibration]
    # The deployment presets are deliberately broad enough for CNN exports.
    # Applying them to every numerically typed Gemma edge also quantizes Shape,
    # rotary Sin/Cos, Softmax, normalization, and mask sentinels. That destroys
    # the encoder. Quantize every attention/linear MatMul plus the one learned
    # embedding Gather, while leaving shape/control and nonlinear operators in
    # their canonical form.
    config.global_quant_config.nodes_to_quantize = selected_nodes
    if args.quantization.startswith("A8W8"):
        # Quark's concat-alignment refinement assumes each Concat output has a
        # single consumer. The portable Gemma export legitimately reuses a
        # shape tensor, so retain its independently calibrated Q/DQ edges.
        config.global_quant_config.extra_options["AlignConcat"] = False
    fast_finetune = config.global_quant_config.extra_options.get("FastFinetune")
    if fast_finetune is not None:
        fast_finetune["DataSize"] = args.samples
        fast_finetune["NumIterations"] = args.fast_finetune_iterations
        fast_finetune["MemOptLevel"] = args.fast_finetune_memory_level
    quantizer = ModelQuantizer(config)
    previous_directory = Path.cwd()
    try:
        # Quark writes its audit CSV to the process working directory.
        os.chdir(output_path.parent)
        quantizer.quantize_model(str(model_path), str(output_path), calibration)
    finally:
        os.chdir(previous_directory)

    quantized = onnx.load(output_path, load_external_data=False)
    shape_repair = repair_quantizer_shapes(quantized, source_graph)
    standardized_qdq_nodes = standardize_qdq_domains(quantized)
    stamp_distribution_metadata(quantized)
    onnx.save(quantized, output_path)
    quantized = onnx.load(output_path, load_external_data=False)
    scale_count, power_of_two_scales, invalid_scales = scale_audit(quantized)
    if scale_count == 0:
        raise SystemExit("quantized graph contains no scale initializers")
    if invalid_scales:
        preview = json.dumps(invalid_scales[:10], indent=2)
        raise SystemExit(f"quantized graph contains invalid scales:\n{preview}")
    if args.quantization.startswith("XINT8") and power_of_two_scales != scale_count:
        raise SystemExit("XINT8 graph contains non-power-of-two scales")
    int8_audit = int8_qdq_audit(quantized, selected_nodes)

    kat, quality = run_kat(
        output_path,
        tokenizer,
        model_path if args.compare_source_kat else None,
    )
    report = {
        "schema": 1,
        "artifact": f"cfetch-embeddinggemma-300m-{args.quantization.lower()}-v1-candidate",
        "source_model_sha256": sha256_file(model_path),
        "artifact_sha256": sha256_file(output_path),
        "calibration": {
            "generator": "cfetch-int8-calibration-v1",
            "samples": args.samples,
            "max_tokens": args.max_tokens,
            "fixed_tensor_shape": args.fixed_calibration_shape,
            "method": args.calibration,
            "per_channel_weights": args.per_channel_weights,
            "hardware_optimize": args.hardware_optimize,
            "smooth_alpha": args.smooth_alpha,
            "fast_finetune_iterations": (
                args.fast_finetune_iterations if fast_finetune is not None else None
            ),
            "fast_finetune_memory_level": (
                args.fast_finetune_memory_level if fast_finetune is not None else None
            ),
            "query_prefix": QUERY_PREFIX,
            "document_prefix": DOCUMENT_PREFIX,
        },
        "model_quantization": args.quantization,
        "toolchain": {
            package: importlib.metadata.version(package)
            for package in ("amd-quark", "onnx", "onnxruntime", "transformers", "numpy", "torch")
        },
        "quantizer_config": repr(config),
        "quantized_nodes": {
            "selection": args.node_selection,
            "count": len(selected_nodes),
        },
        "onnx": {
            "opsets": {item.domain: item.version for item in quantized.opset_import},
            "runtime_graph_optimization": "ORT_ENABLE_ALL (FastEmbed Level 3)",
            "sequence_buckets": list(SEQUENCE_BUCKETS),
            "inference_batch_size": 1,
            "ort_intra_threads": 1,
            "ort_inter_threads": 1,
            "ort_execution_mode": "sequential",
            "nodes": len(quantized.graph.node),
            "initializers": len(quantized.graph.initializer),
            "standardized_qdq_nodes": standardized_qdq_nodes,
            "shape_repair": shape_repair,
            "scale_values": scale_count,
            "power_of_two_scale_values": power_of_two_scales,
            "int8_audit": int8_audit,
        },
        "quality": quality,
        "known_answers": kat,
    }
    report_path = output_path.with_suffix(output_path.suffix + ".build.json")
    report_path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"artifact": str(output_path), "report": str(report_path), **report["onnx"]}, indent=2))


if __name__ == "__main__":
    main()
