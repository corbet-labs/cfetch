from __future__ import annotations

import hashlib
import http.client
import io
import json
from dataclasses import replace
from pathlib import Path
import tempfile
import threading
import unittest

from packages.openvino import adapter
from packages.openvino.manifest import (
    Artifact,
    HostBinding,
    HostFileBinding,
    PackageManifest,
    Scope,
)


class FakeTokenizer:
    pad_token_id = 0

    def encode(self, text: str) -> list[int]:
        if text == "over-limit":
            return list(range(2049))
        return list(range(33 if text == "bucket-64" else 3))


class FakeEngine:
    def __init__(self) -> None:
        self.calls: list[tuple[list[int], list[int], int]] = []

    def embed(self, input_ids, attention_mask, bucket):
        self.calls.append((list(input_ids), list(attention_mask), bucket))
        return [1.0] + [0.0] * 767

    def runtime_evidence(self, bucket):
        return {
            "bucket": bucket,
            "requested_device": "NPU",
            "execution_devices": ["NPU"],
            "execution_devices_source": "compiled_model.get_property(EXECUTION_DEVICES)",
            "device_properties": {},
            "device_properties_source": "core.get_property",
        }

    def host_evidence(self):
        return {
            "system": "Linux",
            "machine": "x86_64",
            "kernel_release": "test-kernel",
            "files": [],
        }


class FakeSigner:
    public_key_hex = "f" * 64

    def sign(self, message: bytes) -> bytes:
        return hashlib.sha512(message).digest()


def scope() -> Scope:
    return Scope(
        package_state="candidate",
        scope_id="intel-test-npu",
        backend="openvino",
        transport="supervised-local",
        runtime="openvino test",
        compiler="openvino test static buckets",
        package_target="linux-x86_64",
        artifact_source="google/embeddinggemma-300m@57c266a740f537b4dc058e1b0cda161fd15afa75",
        artifact_sha256="1" * 64,
        internal_precision="fp16-hardware-compute",
        device_class="npu",
        device="test-intel-npu",
        openvino_device="NPU",
        openvino_compile_config={},
        required_openvino_properties={
            "FULL_DEVICE_NAME": "Test Intel NPU",
            "DEVICE_ARCHITECTURE": "test-npu-architecture",
            "NPU_DRIVER_VERSION": 1,
            "NPU_COMPILER_VERSION": 2,
        },
        required_execution_devices=("NPU",),
        required_host=HostBinding(
            system="Linux",
            machine="x86_64",
            kernel_release="test-kernel",
            files=(HostFileBinding(Path("/usr/lib/test-driver.so"), "8" * 64),),
        ),
        placement_evidence_sha256="2" * 64,
        sequence_capability_evidence_sha256="3" * 64,
        performance_evidence_sha256="4" * 64,
        compatibility_report_sha256=None,
        attestation_public_key="5" * 64,
        attestation_private_key_file=Path("unused.key"),
        accelerated_placement=True,
    )


def package(selected_scope: Scope) -> PackageManifest:
    artifact = Artifact(
        root=Path("."),
        manifest_path=Path("artifact-manifest.json"),
        manifest_sha256=selected_scope.artifact_sha256,
        graph_xml=Path("model.xml"),
        graph_bin=Path("model.bin"),
        tokenizer_json=Path("tokenizer.json"),
        input_ids_name="input_ids",
        attention_mask_name="attention_mask",
        output_name="embedding",
        pad_token_id=0,
        bos_token_id=2,
        eos_token_id=1,
        files=(
            Path("model.xml"),
            Path("model.bin"),
            Path("tokenizer.json"),
        ),
        conversion_versions={
            "openvino": "test",
            "safetensors": "test",
            "torch": "test",
            "transformers": "test",
        },
    )
    return PackageManifest(
        path=Path("package-manifest.json"),
        artifact=artifact,
        scopes={selected_scope.scope_id: selected_scope},
        dependency_versions={},
        runtime_manifest_sha256="9" * 64,
        package_state="candidate",
    )


class AdapterContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.scope = scope()
        self.engine = FakeEngine()
        self.package = package(self.scope)
        self.signer = FakeSigner()
        self.service = adapter.EmbeddingService(
            self.package,
            FakeTokenizer(),
            lambda _package, _scope: self.engine,
            {self.scope.scope_id: self.signer},
        )

    @staticmethod
    def request_body(inputs: list[str], requested_scope: str = "intel-test-npu") -> bytes:
        return json.dumps(
            {
                "model": "google/embeddinggemma-300m",
                "dimensions": 768,
                "input": inputs,
                "cfetch_requested_scope_id": requested_scope,
            },
            separators=(",", ":"),
        ).encode()

    def test_service_right_pads_to_smallest_bucket_without_truncation(self) -> None:
        response_body, signer = self.service.response_for(
            self.request_body(["short", "bucket-64"])
        )
        response = json.loads(response_body)
        self.assertIs(signer, self.signer)
        self.assertEqual([row["token_count"] for row in response["data"]], [3, 33])
        self.assertEqual([row["sequence_bucket"] for row in response["data"]], [32, 64])
        self.assertEqual([call[2] for call in self.engine.calls], [32, 64])
        self.assertEqual(self.engine.calls[0][1], [1, 1, 1] + [0] * 29)
        self.assertTrue(all(row["truncated"] is False for row in response["data"]))
        runtime = response["cfetch_runtime_evidence"]
        self.assertEqual(set(runtime), {
            "schema_version",
            "provider",
            "scope_id",
            "host",
            "host_source",
            "bucket_results",
        })
        self.assertEqual(
            [result["bucket"] for result in runtime["bucket_results"]], [32, 64]
        )
        self.assertEqual(response["cfetch_execution"]["scope_id"], "intel-test-npu")
        self.assertEqual(
            response["cfetch_execution"]["transport"], "supervised-local"
        )
        self.assertEqual(response["cfetch_execution"]["package_state"], "candidate")
        self.assertIsNone(
            response["cfetch_execution"]["compatibility_report_sha256"]
        )

    def test_wrong_requested_scope_is_rejected(self) -> None:
        with self.assertRaisesRegex(adapter.RequestError, "exact scope"):
            self.service.response_for(
                self.request_body(["short"], requested_scope="intel-test-cpu")
            )

    def test_overlength_input_is_rejected_not_truncated(self) -> None:
        with self.assertRaisesRegex(adapter.RequestError, "truncation is forbidden"):
            self.service.response_for(self.request_body(["over-limit"]))
        self.assertEqual(self.engine.calls, [])

    def test_auth_stdin_is_one_exact_32_byte_hex_credential(self) -> None:
        bearer = "a" * 64
        self.assertEqual(
            adapter.parse_auth_line(io.BytesIO(f'{{"bearer":"{bearer}"}}\n'.encode())),
            bearer,
        )
        with self.assertRaises(adapter.RequestError):
            adapter.parse_auth_line(io.BytesIO(b'{"bearer":"ABC"}\n'))

    def test_http_response_signature_binds_nonce_and_exact_bodies(self) -> None:
        bearer = "a" * 64
        signer = FakeSigner()
        server = adapter.AdapterServer(
            ("127.0.0.1", 0), self.service, bearer
        )
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            body = self.request_body(["short"])
            nonce_hex = "b" * 64
            connection = http.client.HTTPConnection(
                "127.0.0.1", server.server_address[1], timeout=2
            )
            connection.request(
                "POST",
                "/v1/embeddings",
                body=body,
                headers={
                    "Authorization": f"Bearer {bearer}",
                    "Content-Type": "application/json",
                    "X-Cfetch-Attestation-Nonce": nonce_hex,
                },
            )
            response = connection.getresponse()
            response_body = response.read()
            self.assertEqual(response.status, 200)
            expected = hashlib.sha512(
                adapter.attestation_message(bytes.fromhex(nonce_hex), body, response_body)
            ).hexdigest()
            self.assertEqual(
                response.getheader("X-Cfetch-Attestation-Signature"), expected
            )
            self.assertEqual(json.loads(response_body)["data"][0]["index"], 0)
            connection.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_http_requires_bearer_and_nonce(self) -> None:
        server = adapter.AdapterServer(
            ("127.0.0.1", 0), self.service, "a" * 64
        )
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            connection = http.client.HTTPConnection(
                "127.0.0.1", server.server_address[1], timeout=2
            )
            connection.request(
                "POST",
                "/v1/embeddings",
                body=self.request_body(["short"]),
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(connection.getresponse().status, 401)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_readiness_is_bounded_and_names_all_plan_scopes_in_order(self) -> None:
        gpu = replace(
            self.scope,
            scope_id="intel-test-gpu",
            device_class="gpu",
            openvino_device="GPU",
            required_openvino_properties={
                "FULL_DEVICE_NAME": "Test Intel GPU",
                "DEVICE_ARCHITECTURE": "test-gpu-architecture",
                "GPU_UARCH_VERSION": "test-uarch",
                "GPU_DEVICE_ID": "0x0000",
            },
        )
        cpu = replace(
            self.scope,
            scope_id="intel-test-cpu",
            device_class="cpu",
            openvino_device="CPU",
            required_openvino_properties={
                "FULL_DEVICE_NAME": "Test Intel CPU",
                "DEVICE_ARCHITECTURE": "intel64",
            },
        )
        plan_package = replace(
            self.package,
            scopes={
                self.scope.scope_id: self.scope,
                gpu.scope_id: gpu,
                cpu.scope_id: cpu,
            },
        )
        server = adapter.AdapterServer(
            ("127.0.0.1", 0), self.service, "a" * 64
        )
        try:
            ready = adapter.readiness_document(server, plan_package)
            self.assertEqual(ready["schema_version"], 1)
            self.assertRegex(ready["url"], r"^http://127\.0\.0\.1:[1-9][0-9]*/v1$")
            self.assertEqual(
                ready["scope_ids"],
                ["intel-test-npu", "intel-test-gpu", "intel-test-cpu"],
            )
            self.assertLess(len(json.dumps(ready)), 512)
        finally:
            server.server_close()

    def test_scope_initialization_failure_has_the_only_retryable_shape(self) -> None:
        def unavailable(_package, _scope):
            raise RuntimeError("vendor detail must stay on stderr")

        service = adapter.EmbeddingService(
            self.package,
            FakeTokenizer(),
            unavailable,
            {self.scope.scope_id: FakeSigner()},
        )
        server = adapter.AdapterServer(("127.0.0.1", 0), service, "a" * 64)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            body = self.request_body(["short"])
            connection = http.client.HTTPConnection(
                "127.0.0.1", server.server_address[1], timeout=2
            )
            connection.request(
                "POST",
                "/v1/embeddings",
                body=body,
                headers={
                    "Authorization": f"Bearer {'a' * 64}",
                    "Content-Type": "application/json",
                    "X-Cfetch-Attestation-Nonce": "b" * 64,
                },
            )
            response = connection.getresponse()
            response_body = response.read()
            self.assertEqual(response.status, 503)
            self.assertEqual(
                response_body,
                b'{"error":{"code":"scope_unavailable","scope_id":"intel-test-npu",'
                b'"message":"requested admitted scope could not initialize or execute"}}',
            )
            connection.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_physical_properties_must_exactly_match_admitted_scope(self) -> None:
        class FakeCore:
            def __init__(self, values):
                self.values = values
                self.calls = []

            def get_property(self, device, name):
                self.calls.append((device, name))
                if name == "SUPPORTED_PROPERTIES":
                    return list(self.values)
                return self.values[name]

        matching = FakeCore(dict(self.scope.required_openvino_properties))
        adapter.validate_openvino_properties(matching, self.scope)
        self.assertEqual(
            matching.calls,
            [("NPU", "SUPPORTED_PROPERTIES")]
            + [("NPU", name) for name in self.scope.required_openvino_properties],
        )

        wrong = dict(self.scope.required_openvino_properties)
        wrong["NPU_DRIVER_VERSION"] = 99
        with self.assertRaisesRegex(RuntimeError, "did not match the admitted"):
            adapter.validate_openvino_properties(FakeCore(wrong), self.scope)

    def test_host_binding_requires_exact_kernel_and_file_bytes(self) -> None:
        with unittest.mock.patch.object(
            adapter.platform, "system", return_value="Linux"
        ), unittest.mock.patch.object(
            adapter.platform, "machine", return_value="x86_64"
        ), unittest.mock.patch.object(
            adapter.platform, "release", return_value="test-kernel"
        ), unittest.mock.patch.object(
            adapter, "_host_file_sha256", return_value="8" * 64
        ):
            adapter.validate_host_binding(self.scope)

        with unittest.mock.patch.object(
            adapter.platform, "system", return_value="Linux"
        ), unittest.mock.patch.object(
            adapter.platform, "machine", return_value="x86_64"
        ), unittest.mock.patch.object(
            adapter.platform, "release", return_value="wrong-kernel"
        ):
            with self.assertRaisesRegex(RuntimeError, "host identity did not match"):
                adapter.validate_host_binding(self.scope)


if __name__ == "__main__":
    unittest.main()
