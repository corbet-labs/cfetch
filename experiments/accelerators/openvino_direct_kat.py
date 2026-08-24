#!/usr/bin/env python3
"""Run the frozen cfetch v1 known-answer test through native OpenVINO.

This diagnostic deliberately bypasses ONNX Runtime.  It is useful when ORT's
OpenVINO execution provider cannot own the whole graph but the vendor runtime
can compile the unchanged ONNX model directly.  A device is compatible only
when every final INT8x768 vector hash matches exactly; close floating-point
outputs are failures.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import time
from collections import Counter
from pathlib import Path
from typing import Any, NamedTuple

import numpy as np
import openvino as ov
from tokenizers import Tokenizer


QUERY_PREFIX = "task: search result | query: "
DOCUMENT_PREFIX = "title: none | text: "
DIMENSIONS = 768
MODEL_SHA256 = "ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1"


class KnownAnswer(NamedTuple):
    label: str
    kind: str
    seed: str
    repeats: int
    bucket: int
    input_ids_sha256: str
    attention_mask_sha256: str
    vector_sha256: str


KNOWN_ANSWERS = (
    KnownAnswer(
        "short-query",
        "query",
        "Which files define cfetch's embedding compatibility boundary?",
        1,
        32,
        "1fb838a2dc546f7175329dae40c5e92df41d85bc8dcd5c24448657cbba8d6e66",
        "9b5ec5a4f100cf4aa2d5a985a66f90bda63382cbd084854cc16a116675663826",
        "20e164382888d264f9a8db999c8f375740c18f0df384ca4335a2d1b75e2971b1",
    ),
    KnownAnswer(
        "profile-document",
        "document",
        "The embedding profile pins the model, tokenizer, prompts, pooling, dimensions, quantization, and vector codec.",
        1,
        32,
        "deeac777342a4ca9fdfa1711b795994236bebf06268a3235d5bc2bcf78f953b1",
        "c0bd668d957abdb6b9e0b8573f93db3c76fe77c2b8cda80225d8e18301703d5a",
        "2d1b07a4baa02517f41d18204ff9c82b1ecc949d3b75afac6e0e79911ca0b7b8",
    ),
    KnownAnswer(
        "source-code",
        "document",
        'fn main() { println!("deterministic vectors"); }',
        1,
        32,
        "23cf572fe094acfcbeb8372eb0d10a7d733a509d8c7ca8842d52f26393ba80cc",
        "bc75fdec344caf5c4f2b3a9baba6230b3b64d71d676629a768035ba1b0977ca9",
        "b59d9d873849c7387ba4f153006a8a3b8ae69104b044623bd5949c578a5cf14a",
    ),
    KnownAnswer(
        "german-query",
        "query",
        "Wie werden inkompatible Vektoren im Netzwerk verhindert?",
        1,
        32,
        "bbd7c33253fcee3bd4c076a489c5ec282323224f324fefc9db5c6dbc05b881ed",
        "18efdb614783a912829ebc0a5f4f17ae5fcced6bddec3a0721420123f325dc61",
        "5720264eac4e9a977ad289109c11e9ab87699206e48f476fea992e293655fd9f",
    ),
    KnownAnswer(
        "japanese-document",
        "document",
        "同じコンテンツハッシュに異なるベクトルが届いた場合、保存を拒否します。",
        1,
        32,
        "65d14cbae5c7f3f54b1c712503b84edfc9f2005bcc3942f1e08bd8162eeb4a8f",
        "575b6b00d72d240caaa3ae73662214a0b15b04755fc2d949e38408d42e1612cd",
        "6a61828885789b40358a773d31deab1d21fbde927c66cce1b33fcbf674549f24",
    ),
    KnownAnswer(
        "bucket-64",
        "document",
        "The canonical vector store rejects conflicting bytes for identical content.",
        3,
        64,
        "5e61212aea07ecb1367261676b888249cc3924235cad0cc91521ec9ad294c6e8",
        "600ee5e5b094d85c2e281f5f89c853d7e41b00c9bf6732408c5aaf953b708643",
        "66e60d40b9e5153da1ee13b5da545522dd324f8600661b0f50be40021c765444",
    ),
    KnownAnswer(
        "bucket-128",
        "document",
        "fn verify(hash: &str, vector: &[i8]) { assert_eq!(vector.len(), 768); }",
        2,
        128,
        "55595a75fe1d2b5fdf59dc52ca2e2ca55c24a7a20f9ab042eb35bc30b8a70539",
        "aab707b95144f118119a1b405aa0e3fbb86d5acc7146a9d789b518419212e8ff",
        "a6a1c4f90acd55441fdf4e911967ae624eb876207d6a9aa9c1ebd4cd1c8ef59c",
    ),
    KnownAnswer(
        "bucket-256",
        "query",
        "Warum müssen alle Teilnehmer genau dasselbe Einbettungsprofil verwenden?",
        9,
        256,
        "97bfa231065a776cf3ab93c9637ff255d3a2aa6a5d5e899bf5ea4c25c817c41e",
        "f7bc8aa1ba021a460ebb88eeec782827ae3a74caa5632a8e8a95c25987dcabb8",
        "91e408fa2d0acebce0a1634336562c8dea6937511bf9835e88acb7e8b42ca1cf",
    ),
    KnownAnswer(
        "bucket-512",
        "document",
        "同じコンテンツハッシュに異なるベクトルが届いた場合、保存を拒否します。",
        14,
        512,
        "4ce829f06f3e2f0b2bf8a151024453bc0610eca03baa2680317d5d6f4d1713ab",
        "de2855a3584560aff0374555d9c05647913b795b8bc7b071e60af965575c4e53",
        "881ea45e551fbb696afa2bd4420a7dcb82f06e97a7a3cf04cd45bb3d54acd56a",
    ),
    KnownAnswer(
        "bucket-1024",
        "document",
        "يجب أن تستخدم جميع الأجهزة النموذج نفسه وخط المعالجة نفسه حتى تبقى المتجهات قابلة للتبادل.",
        19,
        1024,
        "770af9c85623eb023c57c789199073cb184c2e2a43c795bc8a6909b115a2a6f6",
        "41b257f21c3974d7459d5e20be94d5d8f378aaa8c1a493b8ce7d6ed5483dd592",
        "97ddc4bc0911156158f6333afcc51099da70690ad36c4451fe427cfc85c890dc",
    ),
    KnownAnswer(
        "bucket-2048",
        "document",
        '{"network_major":1,"dimensions":768,"precision":"int8","compatible":true}',
        45,
        2048,
        "fa9281575e69af98dc22ab86e14020a13a23482a871e28d5ad38bf06cafe40e9",
        "ecb2bdfef2ed90d076cae41ca6dacf201772bf85aabaea83c4f9c9ace97a9b0d",
        "a28785b70630e80a522ae66e7ed48c801ad1c780df4be40073097c1bdec8a348",
    ),
)


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_vector(vector: np.ndarray) -> bytes:
    value = np.asarray(vector, dtype=np.float32).reshape(-1)
    if value.shape != (DIMENSIONS,) or not np.all(np.isfinite(value)):
        raise ValueError(f"invalid embedding output shape/content: {value.shape}")
    maximum = np.max(np.abs(value)).astype(np.float32)
    if maximum <= 0:
        raise ValueError("model emitted an all-zero vector")
    scaled = value / maximum * np.float32(127.0)
    return np.clip(np.rint(scaled), -127.0, 127.0).astype(np.int8).tobytes()


def encode(tokenizer: Tokenizer, known: KnownAnswer) -> tuple[np.ndarray, np.ndarray]:
    prefix = QUERY_PREFIX if known.kind == "query" else DOCUMENT_PREFIX
    text = prefix + "\n".join([known.seed] * known.repeats)
    encoded = tokenizer.encode(text, add_special_tokens=True)
    if len(encoded.ids) > known.bucket:
        raise ValueError(
            f"{known.label} produced {len(encoded.ids)} tokens for bucket {known.bucket}"
        )
    padding = known.bucket - len(encoded.ids)
    ids = np.asarray([encoded.ids + [0] * padding], dtype="<i8")
    mask = np.asarray([encoded.attention_mask + [0] * padding], dtype="<i8")
    return ids, mask


def safe_property(target: Any, name: str) -> Any:
    try:
        value = target.get_property(name)
    except Exception as error:  # Vendor property support varies by device.
        return {"unavailable": str(error)}
    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    if isinstance(value, (list, tuple, set)):
        return [str(item) for item in value]
    return str(value)


def runtime_summary(compiled: ov.CompiledModel) -> dict[str, Any]:
    counts: dict[str, Counter[str]] = {
        "layerType": Counter(),
        "runtimePrecision": Counter(),
        "execType": Counter(),
    }
    nodes = compiled.get_runtime_model().get_ordered_ops()
    for node in nodes:
        info = node.get_rt_info()
        for key, counter in counts.items():
            if key in info:
                value = info[key]
                counter[str(getattr(value, "value", value))] += 1
    return {
        "node_count": len(nodes),
        "counts": {key: dict(sorted(counter.items())) for key, counter in counts.items()},
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-dir", required=True, type=Path)
    parser.add_argument("--device", required=True, choices=("CPU", "GPU", "NPU"))
    parser.add_argument(
        "--execution-mode-hint",
        choices=("ACCURACY", "PERFORMANCE"),
        default="ACCURACY",
        help="OpenVINO execution policy; exact certification defaults to ACCURACY",
    )
    parser.add_argument(
        "--inference-precision-hint",
        choices=("auto", "f32", "f16"),
        default="auto",
        help="optional diagnostic override; auto uses the device default",
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    model_path = args.model_dir / "model.onnx"
    tokenizer_path = args.model_dir / "tokenizer.json"
    model_digest = sha256_file(model_path)
    if model_digest != MODEL_SHA256:
        raise SystemExit(
            f"model digest {model_digest} does not match frozen v1 {MODEL_SHA256}"
        )

    tokenizer = Tokenizer.from_file(str(tokenizer_path))
    tokenizer.no_padding()
    tokenizer.enable_truncation(max_length=2048, direction="right")
    encoded: dict[str, tuple[np.ndarray, np.ndarray]] = {}
    token_failures: list[str] = []
    for known in KNOWN_ANSWERS:
        ids, mask = encode(tokenizer, known)
        encoded[known.label] = (ids, mask)
        if sha256_bytes(ids.tobytes()) != known.input_ids_sha256:
            token_failures.append(f"{known.label}:input_ids")
        if sha256_bytes(mask.tobytes()) != known.attention_mask_sha256:
            token_failures.append(f"{known.label}:attention_mask")
    if token_failures:
        raise SystemExit("tokenizer contract mismatch: " + ", ".join(token_failures))

    core = ov.Core()
    if args.device not in core.available_devices:
        raise SystemExit(
            f"requested {args.device} is unavailable; found {core.available_devices}"
        )
    supported = {
        str(item) for item in core.get_property(args.device, "SUPPORTED_PROPERTIES")
    }
    compile_config: dict[str, Any] = {}
    if "PERFORMANCE_HINT" in supported:
        compile_config["PERFORMANCE_HINT"] = "LATENCY"
    if "NUM_STREAMS" in supported:
        compile_config["NUM_STREAMS"] = "1"
    if "EXECUTION_MODE_HINT" in supported:
        compile_config["EXECUTION_MODE_HINT"] = args.execution_mode_hint
    if (
        args.inference_precision_hint != "auto"
        and "INFERENCE_PRECISION_HINT" in supported
    ):
        compile_config["INFERENCE_PRECISION_HINT"] = args.inference_precision_hint

    report: dict[str, Any] = {
        "schema": 1,
        "adapter": "native-openvino",
        "openvino": ov.__version__,
        "python": platform.python_version(),
        "platform": platform.platform(),
        "model_sha256": model_digest,
        "requested_device": args.device,
        "available_devices": list(core.available_devices),
        "device_full_name": None,
        "compile_config": compile_config,
        "buckets": [],
        "known_answers": [],
    }
    # Core.get_property takes the device separately; keep the generic helper
    # above for compiled-model properties and record this value explicitly.
    try:
        report["device_full_name"] = str(
            core.get_property(args.device, "FULL_DEVICE_NAME")
        )
    except Exception as error:
        report["device_full_name"] = {"unavailable": str(error)}

    by_bucket: dict[int, list[KnownAnswer]] = {}
    for known in KNOWN_ANSWERS:
        by_bucket.setdefault(known.bucket, []).append(known)

    started = time.monotonic()
    for bucket, known_answers in by_bucket.items():
        compile_started = time.monotonic()
        model = core.read_model(model_path)
        model.reshape(
            {"input_ids": [1, bucket], "attention_mask": [1, bucket]}
        )
        compiled = core.compile_model(model, args.device, compile_config)
        output = compiled.output("sentence_embedding")
        bucket_report = {
            "sequence_bucket": bucket,
            "compile_ms": round((time.monotonic() - compile_started) * 1000),
            "execution_devices": safe_property(compiled, "EXECUTION_DEVICES"),
            "inference_precision_hint": safe_property(
                compiled, "INFERENCE_PRECISION_HINT"
            ),
            "runtime_model": runtime_summary(compiled),
        }
        report["buckets"].append(bucket_report)
        for known in known_answers:
            ids, mask = encoded[known.label]
            run_started = time.monotonic()
            result = compiled(
                {"input_ids": ids, "attention_mask": mask}, share_inputs=False
            )[output]
            vector = np.asarray(result, dtype=np.float32).reshape(-1)
            vector_bytes = canonical_vector(vector)
            vector_digest = sha256_bytes(vector_bytes)
            report["known_answers"].append(
                {
                    "label": known.label,
                    "kind": known.kind,
                    "sequence_bucket": known.bucket,
                    "input_ids_sha256": sha256_bytes(ids.tobytes()),
                    "attention_mask_sha256": sha256_bytes(mask.tobytes()),
                    "model_output_f32_le_sha256": sha256_bytes(
                        np.asarray(vector, dtype="<f4").tobytes()
                    ),
                    "model_output_preview": [float(item) for item in vector[:8]],
                    "vector_sha256": vector_digest,
                    "expected_vector_sha256": known.vector_sha256,
                    "passed": vector_digest == known.vector_sha256,
                    "latency_ms": round((time.monotonic() - run_started) * 1000),
                }
            )
        del compiled
        del model

    report["elapsed_ms"] = round((time.monotonic() - started) * 1000)
    report["exact_vector_conformance"] = all(
        answer["passed"] for answer in report["known_answers"]
    )
    rendered = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output is None:
        print(rendered, end="")
    else:
        if args.output.exists():
            raise SystemExit(f"refusing to overwrite existing report: {args.output}")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered)
        print(
            json.dumps(
                {
                    "report": str(args.output),
                    "requested_device": args.device,
                    "exact_vector_conformance": report["exact_vector_conformance"],
                    "passed": sum(
                        answer["passed"] for answer in report["known_answers"]
                    ),
                    "total": len(report["known_answers"]),
                    "elapsed_ms": report["elapsed_ms"],
                },
                indent=2,
            )
        )
    raise SystemExit(0 if report["exact_vector_conformance"] else 1)


if __name__ == "__main__":
    main()
