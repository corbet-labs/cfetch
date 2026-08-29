from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from packages.openvino import archive
from packages.openvino import fetch_source
from packages.openvino import legal
from packages.openvino import package_inventory
from packages.openvino import smoke_parity
from packages.openvino import runtime_bundle


class PackagingTests(unittest.TestCase):
    def test_fetch_cli_keeps_safe_failure_out_of_result_stdout(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with (
            mock.patch.object(
                fetch_source,
                "fetch",
                side_effect=fetch_source.SourceFetchError("safe lookup failure"),
            ),
            redirect_stdout(stdout),
            redirect_stderr(stderr),
        ):
            status = fetch_source.main(
                ["--output-dir", "unused-output", "--cache-dir", "unused-cache"]
            )
        self.assertEqual(status, 1)
        self.assertEqual(stdout.getvalue(), "")
        self.assertIn("safe lookup failure", stderr.getvalue())

    def test_public_mirror_fetch_uses_exact_commit_allowlist_without_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            snapshot = root / "snapshot"
            snapshot.mkdir()
            payload = b"pinned-source"
            expected = {"nested/source.bin": hashlib.sha256(payload).hexdigest()}
            source = snapshot / "nested/source.bin"
            source.parent.mkdir()
            source.write_bytes(payload)
            calls: dict[str, object] = {}

            class FakeApi:
                def model_info(self, **kwargs):
                    calls["model_info"] = kwargs
                    return SimpleNamespace(sha="test-revision")

            def fake_snapshot_download(**kwargs):
                calls["snapshot_download"] = kwargs
                return str(snapshot)

            fake_module = SimpleNamespace(
                HfApi=FakeApi,
                snapshot_download=fake_snapshot_download,
            )
            with (
                mock.patch.dict(sys.modules, {"huggingface_hub": fake_module}),
                mock.patch.object(fetch_source, "MODEL", "test/model"),
                mock.patch.object(fetch_source, "MODEL_REVISION", "test-revision"),
                mock.patch.object(fetch_source, "SOURCE_MIRROR", "test/mirror"),
                mock.patch.object(
                    fetch_source, "SOURCE_MIRROR_REVISION", "test-revision"
                ),
                mock.patch.object(
                    fetch_source, "PINNED_SOURCE_FILE_SHA256", expected
                ),
            ):
                report = fetch_source.fetch(root / "output", root / "cache")
            self.assertEqual(report["revision"], "test-revision")
            self.assertEqual(
                report["acquisition"],
                {
                    "repository": "test/mirror",
                    "revision": "test-revision",
                    "mode": "public-byte-identical-mirror",
                },
            )
            self.assertIs(calls["model_info"]["token"], False)
            download = calls["snapshot_download"]
            self.assertEqual(download["revision"], "test-revision")
            self.assertIs(download["token"], False)
            self.assertEqual(download["allow_patterns"], ["nested/source.bin"])
            self.assertEqual((root / "output/nested/source.bin").read_bytes(), payload)

    def test_archive_is_deterministic_and_contains_regular_files_only(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            base = Path(raw_root)
            payload = base / "payload"
            (payload / "nested").mkdir(parents=True)
            (payload / "root.txt").write_bytes(b"root")
            executable = payload / "nested/tool"
            executable.write_bytes(b"#!/bin/sh\nexit 0\n")
            executable.chmod(0o755)
            first, first_digest = archive.create_archive(
                payload, base / "first", "test-package"
            )
            second, second_digest = archive.create_archive(
                payload, base / "second", "test-package"
            )
            self.assertEqual(first_digest, second_digest)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first, "r:gz") as bundle:
                members = bundle.getmembers()
            self.assertEqual(
                [member.name for member in members], ["nested/tool", "root.txt"]
            )
            self.assertTrue(all(member.isreg() for member in members))

    def test_gemma_archive_refuses_missing_redistribution_payload(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            base = Path(raw_root)
            payload = base / "payload"
            payload.mkdir()
            (payload / "model.bin").write_bytes(b"weights")
            with self.assertRaisesRegex(archive.ArchiveError, "legal payload"):
                archive.create_archive(
                    payload,
                    base / "output",
                    "gemma-artifact",
                    require_gemma_legal=True,
                )

    def test_native_launcher_rejects_post_assembly_native_file_tamper(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            launcher = root / package_inventory.LAUNCHER
            source = Path(package_inventory.__file__).with_name("launcher.c")
            subprocess.run(
                [
                    "cc",
                    "-std=c17",
                    "-O2",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    str(source),
                    "-o",
                    str(launcher),
                ],
                check=True,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            runtime = root / package_inventory.RUNTIME_DISPATCHER
            runtime.write_bytes(
                b"#!/bin/sh\nprintf '%s\\n' \"$CFETCH_PACKAGE_INVENTORY_SHA256\"\n"
            )
            runtime.chmod(0o755)
            native = root / "_internal/native.so"
            native.parent.mkdir()
            native.write_bytes(b"native-one")
            inventory, digest = package_inventory.create(root)
            self.assertEqual(
                hashlib.sha256(inventory.read_bytes()).hexdigest(), digest
            )
            package_inventory.patch_launcher(root, digest)
            package_inventory.verify(root, digest)
            accepted = subprocess.run(
                [str(launcher), "runtime-check"],
                check=False,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            self.assertEqual(accepted.stdout.strip(), digest)
            native.write_bytes(b"native-two")
            with self.assertRaisesRegex(
                package_inventory.InventoryError, "digest mismatch"
            ):
                package_inventory.verify(root, digest)
            refused = subprocess.run(
                [str(launcher), "runtime-check"],
                check=False,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            self.assertEqual(refused.returncode, 126)
            self.assertIn("inventory verification failed", refused.stderr)

    def test_runtime_manifest_normalizes_but_binds_patched_launcher(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            launcher = root / runtime_bundle.LAUNCHER
            source = Path(runtime_bundle.__file__).with_name("launcher.c")
            subprocess.run(
                [
                    "cc",
                    "-std=c17",
                    "-O2",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    str(source),
                    "-o",
                    str(launcher),
                ],
                check=True,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            dispatcher = root / runtime_bundle.DISPATCHER
            dispatcher.write_bytes(b"#!/bin/sh\nexit 0\n")
            dispatcher.chmod(0o755)
            native = root / "_internal/native.so"
            native.parent.mkdir()
            native.write_bytes(b"runtime-native")
            with (
                mock.patch.object(runtime_bundle, "_require_build_target"),
                mock.patch.object(
                    runtime_bundle,
                    "dependency_versions",
                    return_value={
                        name: "1.2.3"
                        for name in runtime_bundle.RUNTIME_DISTRIBUTIONS
                    },
                ),
                mock.patch.object(
                    runtime_bundle.importlib.metadata,
                    "version",
                    return_value="6.22.2",
                ),
            ):
                _, manifest_sha256 = runtime_bundle.create_manifest(root, "2.35")
            runtime_bundle.load_and_verify(root, manifest_sha256)
            package_manifest = root / "package-manifest.json"
            package_manifest.write_bytes(b'{"package_state":"candidate"}\n')
            _, inventory_sha256 = package_inventory.create(root)
            package_inventory.patch_launcher(root, inventory_sha256)
            package_inventory.verify_bound(root, inventory_sha256)
            runtime_bundle.load_and_verify(
                root,
                manifest_sha256,
                allowed_unbound_files=(
                    package_inventory.INVENTORY_NAME,
                    "package-manifest.json",
                ),
            )
            replacement = b'{"package_state":"physical-probe"}\n'
            projection = package_inventory.project_package_manifest_rebinding(
                root, inventory_sha256, replacement
            )
            self.assertEqual(
                package_manifest.read_bytes(), b'{"package_state":"candidate"}\n'
            )
            new_inventory, new_launcher = package_inventory.rebind_package_manifest(
                root, inventory_sha256, replacement
            )
            self.assertEqual(new_inventory, projection.inventory_sha256)
            self.assertEqual(new_launcher, projection.launcher_sha256)
            self.assertEqual(package_manifest.read_bytes(), replacement)
            package_inventory.verify_bound(root, new_inventory)
            native.write_bytes(b"runtime-tamper")
            with self.assertRaisesRegex(
                runtime_bundle.RuntimeBundleError, "digest mismatch"
            ):
                runtime_bundle.load_and_verify(
                    root,
                    manifest_sha256,
                    allowed_unbound_files=(
                        package_inventory.INVENTORY_NAME,
                        "package-manifest.json",
                    ),
                )

    def test_generated_legal_notices_have_pinned_bytes(self) -> None:
        generated = {
            "MODEL_USE_RESTRICTIONS.txt": legal.USE_RESTRICTIONS_BYTES,
            "MODEL_MODIFICATIONS.txt": legal.MODIFICATIONS_BYTES,
            "NOTICE": legal.NOTICE_BYTES,
        }
        for name, raw in generated.items():
            self.assertEqual(
                hashlib.sha256(raw).hexdigest(), legal.PINNED_LEGAL_SHA256[name]
            )
        self.assertEqual(
            legal.NOTICE_BYTES,
            b"Gemma is provided under and subject to the Gemma Terms of Use found at "
            b"ai.google.dev/gemma/terms\n",
        )

    def test_parity_gate_rejects_divergence_and_accepts_normalized_match(self) -> None:
        reference = [1.0] + [0.0] * 767
        _, _, cosine = smoke_parity.validate_pair(reference, reference)
        self.assertEqual(cosine, 1.0)
        with self.assertRaisesRegex(smoke_parity.ParityError, "cosine"):
            smoke_parity.validate_pair(reference, [-1.0] + [0.0] * 767)


if __name__ == "__main__":
    unittest.main()
