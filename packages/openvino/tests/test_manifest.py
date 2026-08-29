from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from packages.openvino import manifest
from packages.openvino import legal


def compact(value) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


class ManifestTests(unittest.TestCase):
    def fixture(self, root: Path) -> Path:
        artifact_dir = root / "artifact"
        artifact_dir.mkdir()
        contents = {
            "embeddinggemma.xml": b"xml",
            "embeddinggemma.bin": b"bin",
            "tokenizer.json": b"tokenizer fixture",
            **{name: f"legal fixture {name}".encode() for name in legal.LEGAL_FILES},
        }
        for name, value in contents.items():
            (artifact_dir / name).write_bytes(value)
        file_entries = []
        for name, value in contents.items():
            digest = hashlib.sha256(value).hexdigest()
            if name == "tokenizer.json":
                digest = manifest.PINNED_SOURCE_FILE_SHA256["tokenizer.json"]
            elif name in legal.LEGAL_FILES:
                digest = legal.PINNED_LEGAL_SHA256[name]
            file_entries.append({"path": name, "sha256": digest, "bytes": len(value)})
        artifact_document = {
            "schema_version": 1,
            "artifact_format": "openvino-ir-dynamic-sequence-static-buckets-v1",
            "source": {
                "model": manifest.MODEL,
                "revision": manifest.MODEL_REVISION,
                "acquisition": {
                    "repository": manifest.SOURCE_MIRROR,
                    "revision": manifest.SOURCE_MIRROR_REVISION,
                    "mode": "public-byte-identical-mirror",
                },
                "files": manifest.PINNED_SOURCE_FILE_SHA256,
            },
            "semantic_pipeline": {
                "dimensions": 768,
                "pooling": "attention-mask-weighted-mean-include-prompt",
                "dense_2": "linear-768x3072-identity",
                "dense_3": "linear-3072x768-identity",
                "normalization": "l2",
                "truncation": "disabled",
                "padding": "right-attention-mask-excludes-padding",
            },
            "sequence_buckets": list(manifest.SEQUENCE_BUCKETS),
            "graph": {
                "xml": "embeddinggemma.xml",
                "bin": "embeddinggemma.bin",
                "input_ids": "input_ids",
                "attention_mask": "attention_mask",
                "output": "embedding",
            },
            "tokenizer": {
                "json": "tokenizer.json",
                "sha256": manifest.PINNED_SOURCE_FILE_SHA256["tokenizer.json"],
                "pad_token": "<pad>",
                "pad_token_id": 0,
                "bos_token": "<bos>",
                "bos_token_id": 2,
                "eos_token": "<eos>",
                "eos_token_id": 1,
                "add_bos_token": True,
                "add_eos_token": True,
            },
            "legal": {
                "terms_url": legal.TERMS_URL,
                "prohibited_use_policy_url": legal.PROHIBITED_USE_URL,
                "terms_file": "GEMMA_TERMS.txt",
                "prohibited_use_policy_file": "GEMMA_PROHIBITED_USE_POLICY.txt",
                "use_restrictions_file": "MODEL_USE_RESTRICTIONS.txt",
                "modifications_file": "MODEL_MODIFICATIONS.txt",
                "notice_file": "NOTICE",
            },
            "files": file_entries,
            "conversion": {
                "recipe": "packages/openvino/convert.py",
                "export": "torch-export-bounded-dynamic-sequence-1-to-2048",
                "weight_storage": "f16",
                "openvino": "test",
                "safetensors": "test",
                "torch": "test",
                "transformers": "test",
            },
        }
        artifact_raw = compact(artifact_document)
        (artifact_dir / "artifact-manifest.json").write_bytes(artifact_raw)
        for suffix, digit in (("npu", "1"), ("gpu", "2"), ("cpu", "3")):
            (root / f"scope-{suffix}.key").write_text(
                digit * 64 + "\n", encoding="ascii"
            )
        npu_scope = {
            "scope_id": "intel-test-npu",
            "backend": "openvino",
            "transport": "supervised-local",
            "runtime": "openvino test",
            "compiler": "openvino static test",
            "package_target": "linux-x86_64",
            "artifact_source": f"{manifest.MODEL}@{manifest.MODEL_REVISION}",
            "artifact_sha256": hashlib.sha256(artifact_raw).hexdigest(),
            "internal_precision": "fp16-hardware-compute",
            "device_class": "npu",
            "device": "test-intel-npu",
            "openvino_device": "NPU",
            "openvino_compile_config": {},
            "required_openvino_properties": {
                "FULL_DEVICE_NAME": "Test Intel NPU",
                "DEVICE_ARCHITECTURE": "test-npu-architecture",
                "NPU_DRIVER_VERSION": 1,
                "NPU_COMPILER_VERSION": 2,
            },
            "required_execution_devices": ["NPU"],
            "required_host": {
                "system": "Linux",
                "machine": "x86_64",
                "kernel_release": "test-kernel",
                "files": [
                    {"path": "/usr/lib/test-npu-driver.so", "sha256": "8" * 64}
                ],
            },
            "placement_evidence_sha256": "2" * 64,
            "supported_max_tokens": 2048,
            "supported_sequence_buckets": list(manifest.SEQUENCE_BUCKETS),
            "supported_max_batch_size": 64,
            "sequence_capability_evidence_sha256": "3" * 64,
            "performance_evidence_sha256": "4" * 64,
            "compatibility_report_sha256": None,
            "attestation_public_key": "5" * 64,
            "attestation_private_key_file": "scope-npu.key",
            "accelerated_placement": True,
        }
        gpu_scope = dict(npu_scope)
        gpu_scope.update(
            {
                "scope_id": "intel-test-gpu",
                "device_class": "gpu",
                "device": "test-intel-gpu",
                "openvino_device": "GPU",
                "required_openvino_properties": {
                    "FULL_DEVICE_NAME": "Test Intel GPU",
                    "DEVICE_ARCHITECTURE": "test-gpu-architecture",
                    "GPU_UARCH_VERSION": "test-uarch",
                    "GPU_DEVICE_ID": "0x0000",
                },
                "required_execution_devices": ["GPU"],
                "attestation_public_key": "6" * 64,
                "attestation_private_key_file": "scope-gpu.key",
            }
        )
        cpu_scope = dict(npu_scope)
        cpu_scope.update(
            {
                "scope_id": "intel-test-cpu",
                "device_class": "cpu",
                "device": "test-intel-cpu",
                "openvino_device": "CPU",
                "required_openvino_properties": {
                    "FULL_DEVICE_NAME": "Test Intel CPU",
                    "DEVICE_ARCHITECTURE": "intel64",
                },
                "required_execution_devices": ["CPU"],
                "attestation_public_key": "7" * 64,
                "attestation_private_key_file": "scope-cpu.key",
            }
        )
        package_document = {
            "schema_version": 1,
            "package_state": "candidate",
            "profile_id": manifest.PROFILE_ID,
            "profile_manifest_sha256": manifest.PROFILE_MANIFEST_SHA256,
            "admission_policy_sha256": manifest.ADMISSION_POLICY_SHA256,
            "model": manifest.MODEL,
            "model_revision": manifest.MODEL_REVISION,
            "artifact_manifest": "artifact/artifact-manifest.json",
            "artifact_manifest_sha256": hashlib.sha256(artifact_raw).hexdigest(),
            "runtime_manifest_sha256": "9" * 64,
            "dependency_versions": {
                "cryptography": "test",
                "numpy": "test",
                "openvino": "test",
                "tokenizers": "test",
            },
            "scopes": [npu_scope, gpu_scope, cpu_scope],
        }
        path = root / "package-manifest.json"
        path.write_bytes(compact(package_document))
        return path

    def test_exact_scope_is_the_only_device_authority(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            path = self.fixture(root)
            real_hash = manifest._sha256_file

            def hash_fixture(file_path: Path) -> str:
                if file_path.name == "tokenizer.json":
                    return manifest.PINNED_SOURCE_FILE_SHA256["tokenizer.json"]
                if file_path.name in legal.LEGAL_FILES:
                    return legal.PINNED_LEGAL_SHA256[file_path.name]
                return real_hash(file_path)

            with mock.patch.object(manifest, "_sha256_file", side_effect=hash_fixture):
                package = manifest.load_package_manifest(path)
            selected = package.scope("intel-test-npu")
            self.assertEqual(selected.openvino_device, "NPU")
            self.assertEqual(package.package_state, "candidate")
            self.assertEqual(
                selected.required_openvino_properties["NPU_DRIVER_VERSION"], 1
            )
            with self.assertRaisesRegex(manifest.ManifestError, "not present"):
                package.scope("intel-test-unknown")

    def test_aggregate_or_wrong_class_device_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            path = self.fixture(root)
            document = json.loads(path.read_text())
            document["scopes"][0]["openvino_device"] = "AUTO:NPU,GPU,CPU"
            path.write_bytes(compact(document))
            real_hash = manifest._sha256_file

            def hash_fixture(file_path: Path) -> str:
                if file_path.name == "tokenizer.json":
                    return manifest.PINNED_SOURCE_FILE_SHA256["tokenizer.json"]
                if file_path.name in legal.LEGAL_FILES:
                    return legal.PINNED_LEGAL_SHA256[file_path.name]
                return real_hash(file_path)

            with mock.patch.object(manifest, "_sha256_file", side_effect=hash_fixture):
                with self.assertRaisesRegex(manifest.ManifestError, "exactly NPU"):
                    manifest.load_package_manifest(path)

    def test_scope_requires_exact_allowlisted_openvino_properties(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            path = self.fixture(root)
            document = json.loads(path.read_text())
            del document["scopes"][0]["required_openvino_properties"][
                "NPU_DRIVER_VERSION"
            ]
            document["scopes"][0]["required_openvino_properties"][
                "DEVICE_UUID"
            ] = "not-allowlisted"
            path.write_bytes(compact(document))
            real_hash = manifest._sha256_file

            def hash_fixture(file_path: Path) -> str:
                if file_path.name == "tokenizer.json":
                    return manifest.PINNED_SOURCE_FILE_SHA256["tokenizer.json"]
                if file_path.name in legal.LEGAL_FILES:
                    return legal.PINNED_LEGAL_SHA256[file_path.name]
                return real_hash(file_path)

            with mock.patch.object(manifest, "_sha256_file", side_effect=hash_fixture):
                with self.assertRaisesRegex(
                    manifest.ManifestError, "missing fields: NPU_DRIVER_VERSION"
                ):
                    manifest.load_package_manifest(path)

    def test_artifact_digest_change_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            path = self.fixture(root)
            (root / "artifact/embeddinggemma.xml").write_bytes(b"changed")
            with self.assertRaisesRegex(manifest.ManifestError, "size mismatch"):
                manifest.load_package_manifest(path)

    def test_package_state_controls_pending_and_release_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            path = self.fixture(root)
            document = json.loads(path.read_text())
            document["package_state"] = "physical-probe"
            for scope in document["scopes"]:
                scope["placement_evidence_sha256"] = None
                scope["sequence_capability_evidence_sha256"] = None
                scope["performance_evidence_sha256"] = None
                scope["compatibility_report_sha256"] = None
            path.write_bytes(compact(document))
            real_hash = manifest._sha256_file

            def hash_fixture(file_path: Path) -> str:
                if file_path.name == "tokenizer.json":
                    return manifest.PINNED_SOURCE_FILE_SHA256["tokenizer.json"]
                if file_path.name in legal.LEGAL_FILES:
                    return legal.PINNED_LEGAL_SHA256[file_path.name]
                return real_hash(file_path)

            with mock.patch.object(manifest, "_sha256_file", side_effect=hash_fixture):
                package = manifest.load_package_manifest(path)
            execution = package.scope("intel-test-npu").execution_document()
            self.assertEqual(execution["package_state"], "physical-probe")
            self.assertIsNone(execution["placement_evidence_sha256"])
            self.assertIsNone(execution["compatibility_report_sha256"])

            document["package_state"] = "release"
            path.write_bytes(compact(document))
            with self.assertRaisesRegex(manifest.ManifestError, "release requires"):
                with mock.patch.object(
                    manifest, "_sha256_file", side_effect=hash_fixture
                ):
                    manifest.load_package_manifest(path)


if __name__ == "__main__":
    unittest.main()
