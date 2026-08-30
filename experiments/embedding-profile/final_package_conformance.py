#!/usr/bin/env python3
"""Replay one admission cache against the final packaged adapter endpoint."""

from __future__ import annotations

import argparse
from collections.abc import Mapping, Sequence
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import queue
import re
import secrets
import shutil
import stat
import subprocess
import tempfile
import threading
from typing import TYPE_CHECKING
import zipfile

from admission_evidence import (
    SEQUENCE_BUCKETS,
    SUPPORTED_MAX_BATCH_SIZE,
    ordered_input_json_sha256,
    parse_evidence_json,
    wire_batch_inputs,
)
from cross_backend_eval import (
    ADMISSION_IMPLEMENTATION_BUNDLE_SHA256,
    file_sha256,
    load_cache,
    validate_admission_cache_container,
    validate_wire_batch_output_cache,
    verify_implementation_bundle,
)
from export_adapter_cache import (
    MODEL,
    MODEL_REVISION,
    PROFILE_ID,
    PROFILE_MANIFEST_SHA256,
    RequestFunction,
    SEQUENCE_PROBE_ARRAY_NAMES,
    SequenceProbeRequestFunction,
    collect_sequence_probe_arrays,
    load_scifact_inputs,
    local_opener,
    positive_float,
    request_embeddings,
    sha256_value,
    validate_loopback_endpoint,
    verify_wire_batch_contract,
)
from profile_identity import ADMISSION_POLICY_SHA256

if TYPE_CHECKING:
    import numpy as np
    import urllib.request


FINAL_EXECUTION_FIELDS = (
    "scope_id",
    "transport",
    "backend",
    "runtime",
    "compiler",
    "package_target",
    "artifact_source",
    "device_class",
    "device",
    "artifact_sha256",
    "internal_precision",
    "placement_evidence_sha256",
    "supported_max_tokens",
    "supported_sequence_buckets",
    "supported_max_batch_size",
    "sequence_capability_evidence_sha256",
    "performance_evidence_sha256",
    "accelerated_placement",
)
SHA256_RE = re.compile(r"[0-9a-f]{64}")
PACKAGE_ID_RE = re.compile(r"[a-z0-9]+(?:[._-][a-z0-9]+)*")
MAX_RECEIPT_BYTES = 64 * 1024
MAX_PACKAGE_FILES = 4096
MAX_PACKAGE_EXPANDED_BYTES = 2 * 1024 * 1024 * 1024
MAX_READY_BYTES = 16 * 1024
READY_TIMEOUT_SECONDS = 20.0


def existing_cache(value: str) -> Path:
    path = Path(value)
    if (
        path.is_symlink()
        or path.resolve() != path.absolute()
        or not path.is_file()
    ):
        raise argparse.ArgumentTypeError(f"admission cache does not exist: {path}")
    return path


def existing_package_asset(value: str) -> Path:
    path = Path(value)
    if (
        path.is_symlink()
        or path.resolve() != path.absolute()
        or not path.is_file()
        or path.stat().st_size < 1
    ):
        raise argparse.ArgumentTypeError(
            f"target package asset must be a nonempty regular file: {path}"
        )
    return path


def package_id_value(value: str) -> str:
    if len(value) > 128 or PACKAGE_ID_RE.fullmatch(value) is None:
        raise argparse.ArgumentTypeError("package id must be a canonical lowercase slug")
    return value


def existing_private_key(value: str) -> Path:
    path = Path(value)
    if (
        path.is_symlink()
        or path.resolve() != path.absolute()
        or not path.is_file()
        or path.stat().st_size != 32
    ):
        raise argparse.ArgumentTypeError(
            "receipt attestation private key must be a regular 32-byte raw Ed25519 key"
        )
    return path


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Challenge the final packaged adapter and byte-match every retained "
            "wire grouping and sequence-bucket output"
        )
    )
    parser.add_argument(
        "--endpoint",
        help=(
            "already-running candidate loopback URL ending in /embeddings; "
            "final activation receipts instead launch --package-asset themselves"
        ),
    )
    parser.add_argument("--cache", required=True, type=existing_cache)
    parser.add_argument(
        "--cache-sha256",
        required=True,
        type=sha256_value,
        help="exact admitted cache digest",
    )
    parser.add_argument(
        "--compatibility-report-sha256",
        required=True,
        type=sha256_value,
        help="newest global cohort report digest required from the package",
    )
    parser.add_argument("--timeout-seconds", type=positive_float, default=120.0)
    parser.add_argument(
        "--bearer-token-env",
        metavar="NAME",
        help="read an optional loopback adapter bearer token from this environment variable",
    )
    parser.add_argument(
        "--stage-id",
        type=sha256_value,
        help="exact admission stage id for a content-addressed activation receipt",
    )
    parser.add_argument(
        "--stage-plan",
        type=Path,
        help="exact stage-plan.json whose assets and receipt key authorize activation",
    )
    parser.add_argument(
        "--package-id",
        type=package_id_value,
        help="target package id for the activation receipt",
    )
    parser.add_argument(
        "--package-asset",
        type=existing_package_asset,
        help="exact final target-native package bytes exercised by this endpoint",
    )
    parser.add_argument(
        "--package-sha256",
        type=sha256_value,
        help="expected SHA-256 of --package-asset",
    )
    parser.add_argument(
        "--receipt-directory",
        type=Path,
        help="write a digest-named activation receipt after full conformance",
    )
    parser.add_argument(
        "--dispatcher",
        help="plain root-level dispatcher basename inside the exact package ZIP",
    )
    parser.add_argument(
        "--receipt-attestation-private-key",
        type=existing_private_key,
        help="raw Ed25519 key matching the receipt public key pinned by --stage-plan",
    )
    return parser


def _receipt_arguments(args: argparse.Namespace) -> tuple[object, ...]:
    return (
        args.stage_id,
        args.stage_plan,
        args.package_id,
        args.package_asset,
        args.package_sha256,
        args.receipt_directory,
        args.dispatcher,
        args.receipt_attestation_private_key,
    )


def validate_receipt_arguments(args: argparse.Namespace) -> bool:
    """Require the package/stage receipt binding as one indivisible option set."""
    supplied = tuple(value is not None for value in _receipt_arguments(args))
    if any(supplied) and not all(supplied):
        raise ValueError(
            "--stage-id, --stage-plan, --package-id, --package-asset, "
            "--package-sha256, --receipt-directory, --dispatcher, and "
            "--receipt-attestation-private-key must be supplied together"
        )
    receipt_mode = all(supplied)
    if receipt_mode:
        if args.endpoint is not None or args.bearer_token_env is not None:
            raise ValueError(
                "receipt mode launches the exact package and forbids --endpoint and "
                "--bearer-token-env"
            )
    elif args.endpoint is None:
        raise ValueError("candidate replay requires --endpoint")
    return receipt_mode


def _dispatcher_basename(value: str) -> str:
    if (
        not value
        or len(value) > 255
        or value in {".", ".."}
        or "/" in value
        or "\\" in value
        or any(ord(character) < 32 or ord(character) == 127 for character in value)
        or Path(value).name != value
    ):
        raise ValueError("dispatcher must be a plain root-level basename")
    return value


def _zip_member_name(value: str) -> str:
    pure = PurePosixPath(value)
    if (
        not value
        or "\\" in value
        or pure.is_absolute()
        or not pure.parts
        or any(part in {"", ".", ".."} for part in pure.parts)
        or pure.as_posix() != value
    ):
        raise ValueError(f"package ZIP contains unsafe member name {value!r}")
    return value


def _extract_exact_package(package_asset: Path, destination: Path) -> set[str]:
    """Extract a bounded regular-file-only ZIP without trusting archive paths."""
    names: set[str] = set()
    expanded = 0
    try:
        archive = zipfile.ZipFile(package_asset, "r")
    except zipfile.BadZipFile as error:
        raise ValueError("target package asset must be a valid ZIP") from error
    with archive:
        infos = archive.infolist()
        if not infos or len(infos) > MAX_PACKAGE_FILES:
            raise ValueError(
                f"target package ZIP must contain 1..{MAX_PACKAGE_FILES} files"
            )
        for info in infos:
            name = _zip_member_name(info.filename)
            if name in names:
                raise ValueError(f"target package ZIP repeats member {name!r}")
            names.add(name)
            mode = info.external_attr >> 16
            if info.is_dir() or not stat.S_ISREG(mode):
                raise ValueError(
                    f"target package ZIP member {name!r} is not a regular file"
                )
            if info.file_size < 1:
                raise ValueError(f"target package ZIP member {name!r} is empty")
            expanded += info.file_size
            if expanded > MAX_PACKAGE_EXPANDED_BYTES:
                raise ValueError("target package ZIP exceeds its expanded byte bound")

            target = destination.joinpath(*PurePosixPath(name).parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            written = 0
            with archive.open(info, "r") as source, target.open("xb") as output:
                while chunk := source.read(1024 * 1024):
                    written += len(chunk)
                    if written > info.file_size:
                        raise ValueError(
                            f"target package ZIP member {name!r} exceeded its declared size"
                        )
                    output.write(chunk)
            if written != info.file_size:
                raise ValueError(
                    f"target package ZIP member {name!r} did not match its declared size"
                )
            os.chmod(target, stat.S_IMODE(mode))
    return names


def _read_ready_line(stream: object) -> bytes:
    result: queue.Queue[object] = queue.Queue(maxsize=1)

    def read() -> None:
        try:
            line = stream.readline(MAX_READY_BYTES + 1)  # type: ignore[attr-defined]
        except Exception as error:  # pragma: no cover - platform pipe failure
            result.put(error)
        else:
            result.put(line)

    reader = threading.Thread(target=read, name="cfetch-package-readiness", daemon=True)
    reader.start()
    try:
        value = result.get(timeout=READY_TIMEOUT_SECONDS)
    except queue.Empty as error:
        raise ValueError("timed out waiting for exact package readiness") from error
    if isinstance(value, Exception):
        raise ValueError(f"could not read exact package readiness: {value}") from value
    if (
        not isinstance(value, bytes)
        or not value.endswith(b"\n")
        or len(value) > MAX_READY_BYTES
    ):
        raise ValueError("exact package readiness line is missing or exceeds its bound")
    return value


def _package_scope_ids(
    root: Path,
    expected_manifest_sha256: str,
    requested_scope_id: str,
    expected_execution: Mapping[str, object],
    expected_attestation_public_key: str,
) -> list[str]:
    """Bind the requested scope to the cache and report in exact manifest bytes."""
    manifest_path = root / "package-manifest.json"
    if not manifest_path.is_file() or manifest_path.is_symlink():
        raise ValueError("exact package has no regular root package-manifest.json")
    raw = manifest_path.read_bytes()
    if not raw or len(raw) > 1024 * 1024:
        raise ValueError("exact package manifest exceeds its byte bound")
    if hashlib.sha256(raw).hexdigest() != expected_manifest_sha256:
        raise ValueError(
            "exact package manifest bytes do not match the externally pinned package plan"
        )
    manifest = parse_evidence_json(raw, "exact package manifest")
    frozen_identity = {
        "schema_version": 1,
        "profile_id": PROFILE_ID,
        "profile_manifest_sha256": PROFILE_MANIFEST_SHA256,
        "admission_policy_sha256": ADMISSION_POLICY_SHA256,
        "model": MODEL,
        "model_revision": MODEL_REVISION,
    }
    for field, expected in frozen_identity.items():
        if manifest.get(field) != expected:
            raise ValueError(
                f"exact package manifest {field}={manifest.get(field)!r}, "
                f"expected {expected!r}"
            )
    if manifest.get("package_state") != "release":
        raise ValueError("exact package manifest must be in release state")
    scopes = manifest.get("scopes") if isinstance(manifest, dict) else None
    if (
        not isinstance(scopes, list)
        or not scopes
        or any(not isinstance(scope, dict) for scope in scopes)
    ):
        raise ValueError("exact package manifest has no ordered scope array")
    scope_ids = [scope.get("scope_id") for scope in scopes]
    if (
        any(not isinstance(scope_id, str) for scope_id in scope_ids)
        or any(
            len(scope_id) > 128 or PACKAGE_ID_RE.fullmatch(scope_id) is None
            for scope_id in scope_ids
            if isinstance(scope_id, str)
        )
        or len(scope_ids) != len(set(scope_ids))
    ):
        raise ValueError("exact package manifest has invalid or duplicate scope ids")
    if requested_scope_id not in scope_ids:
        raise ValueError(
            f"exact package manifest does not contain requested scope {requested_scope_id!r}"
        )
    required_execution_fields = set(FINAL_EXECUTION_FIELDS) | {
        "compatibility_report_sha256"
    }
    if set(expected_execution) != required_execution_fields:
        raise ValueError("expected final execution binding has an invalid schema")
    requested_scope = scopes[scope_ids.index(requested_scope_id)]
    for field, expected in expected_execution.items():
        if requested_scope.get(field) != expected:
            raise ValueError(
                f"exact package manifest scope {requested_scope_id!r} {field}="
                f"{requested_scope.get(field)!r}, expected {expected!r}"
            )
    if (
        not isinstance(expected_attestation_public_key, str)
        or SHA256_RE.fullmatch(expected_attestation_public_key) is None
        or requested_scope.get("attestation_public_key")
        != expected_attestation_public_key
    ):
        raise ValueError(
            f"exact package manifest scope {requested_scope_id!r} does not bind "
            "the admission-cache attestation key"
        )
    return scope_ids


@contextmanager
def launch_exact_package(
    package_asset: Path,
    package_sha256: str,
    dispatcher_name: str,
    dispatcher_sha256: str,
    package_manifest_sha256: str,
    expected_scope_ids: Sequence[str],
    requested_scope_id: str,
    expected_execution: Mapping[str, object],
    expected_attestation_public_key: str,
):
    """Launch only the dispatcher extracted from the exact receipt-bound ZIP."""
    if file_sha256(package_asset) != package_sha256:
        raise ValueError("target package asset bytes do not match --package-sha256")
    dispatcher_name = _dispatcher_basename(dispatcher_name)
    temporary = Path(tempfile.mkdtemp(prefix="cfetch-final-package-"))
    process: subprocess.Popen[bytes] | None = None
    try:
        members = _extract_exact_package(package_asset, temporary)
        if dispatcher_name not in members:
            raise ValueError("exact package does not contain its declared dispatcher")
        packaged_scopes = _package_scope_ids(
            temporary,
            package_manifest_sha256,
            requested_scope_id,
            expected_execution,
            expected_attestation_public_key,
        )
        if packaged_scopes != list(expected_scope_ids):
            raise ValueError(
                "exact package manifest scope order does not match its staged package plan"
            )
        dispatcher = temporary / dispatcher_name
        if not os.access(dispatcher, os.X_OK):
            raise ValueError("exact package dispatcher is not executable")
        if file_sha256(dispatcher) != dispatcher_sha256:
            raise ValueError(
                "exact package dispatcher bytes do not match the staged package plan"
            )
        if file_sha256(package_asset) != package_sha256:
            raise ValueError("target package asset changed while it was extracted")
        bearer = secrets.token_hex(32)
        process = subprocess.Popen(
            [
                str(dispatcher),
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
                "--auth-stdin",
            ],
            cwd=temporary,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
        )
        if process.stdin is None or process.stdout is None:
            raise ValueError("exact package dispatcher pipes were not created")
        process.stdin.write(
            json.dumps({"bearer": bearer}, separators=(",", ":")).encode("utf-8")
            + b"\n"
        )
        process.stdin.flush()
        ready_raw = _read_ready_line(process.stdout)
        try:
            ready = json.loads(ready_raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ValueError("exact package emitted invalid readiness JSON") from error
        if (
            not isinstance(ready, dict)
            or set(ready) != {"schema_version", "url", "scope_ids"}
            or ready["schema_version"] != 1
            or ready["scope_ids"] != list(expected_scope_ids)
            or not isinstance(ready["url"], str)
        ):
            raise ValueError("exact package readiness does not match its manifest")
        match = re.fullmatch(
            r"http://127\.0\.0\.1:([1-9][0-9]{0,4})/v1", ready["url"]
        )
        if match is None or int(match.group(1)) > 65535:
            raise ValueError(
                "exact package readiness must advertise an ephemeral IPv4 loopback /v1 URL"
            )
        endpoint = validate_loopback_endpoint(
            ready["url"].rstrip("/") + "/embeddings"
        )
        yield endpoint, bearer
    finally:
        if process is not None:
            if process.stdin is not None:
                try:
                    process.stdin.close()
                except (BrokenPipeError, OSError):
                    pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            finally:
                if process.stdout is not None:
                    process.stdout.close()
        shutil.rmtree(temporary, ignore_errors=True)


def validate_stage_receipt_binding(
    args: argparse.Namespace, scope_id: str
) -> tuple[dict[str, object], str]:
    """Bind receipt mode to the exact assets and dispatcher declared by one stage."""
    from admission_transaction import (
        _load_stage_plan,
        _read_json,
        _resolve_input,
        _validate_stage_bytes,
    )

    verify_implementation_bundle()
    if args.stage_plan.is_symlink() or args.stage_plan.resolve() != args.stage_plan.absolute():
        raise ValueError("stage plan must be a regular non-symlink path")
    plan, stage_root = _load_stage_plan(args.stage_plan)
    _validate_stage_bytes(plan, stage_root)
    if plan["stage_id"] != args.stage_id:
        raise ValueError("--stage-id does not match --stage-plan")
    if (
        plan["admission_implementation_bundle_sha256"]
        != ADMISSION_IMPLEMENTATION_BUNDLE_SHA256
    ):
        raise ValueError("stage plan uses another admission implementation bundle")

    package_assets = [
        asset
        for asset in plan["assets"]
        if asset["kind"] == "target-package" and asset["owner_id"] == args.package_id
    ]
    if len(package_assets) != 1:
        raise ValueError("stage plan does not contain one exact target package asset")
    package_asset = package_assets[0]
    expected_package_path = _resolve_input(
        stage_root,
        f"assets/{package_asset['filename']}",
        "staged package asset",
        directory=False,
    )
    if (
        args.package_asset.resolve() != expected_package_path
        or args.package_sha256 != package_asset["sha256"]
        or args.package_asset.stat().st_size != package_asset["bytes"]
    ):
        raise ValueError(
            "receipt package must be the exact target package asset from --stage-plan"
        )

    expected_rows = [
        row
        for row in plan["expected_conformance_receipts"]
        if isinstance(row, dict)
        and row.get("package_id") == args.package_id
        and row.get("scope_id") == scope_id
    ]
    if len(expected_rows) != 1:
        raise ValueError("stage plan does not require this exact package/scope receipt")
    expected = expected_rows[0]
    if (
        expected.get("package_sha256") != args.package_sha256
        or expected.get("cache_sha256") != args.cache_sha256
        or expected.get("compatibility_report_sha256")
        != args.compatibility_report_sha256
    ):
        raise ValueError("receipt inputs do not match the expected stage binding")
    cache_assets = [
        asset
        for asset in plan["assets"]
        if asset["kind"] == "admission-cache"
        and asset["owner_id"] == scope_id
        and asset["sha256"] == args.cache_sha256
    ]
    if len(cache_assets) != 1:
        raise ValueError("stage plan has no exact cache asset for the receipt scope")
    expected_cache_path = _resolve_input(
        stage_root,
        f"assets/{cache_assets[0]['filename']}",
        "staged admission cache",
        directory=False,
    )
    if args.cache.resolve() != expected_cache_path:
        raise ValueError("receipt must replay the exact staged admission cache bytes")

    registry_path = _resolve_input(
        stage_root,
        plan["proposed_registry"],
        "proposed registry",
        directory=False,
    )
    registry, _ = _read_json(registry_path, 1024 * 1024, "proposed registry")
    packages = registry.get("local_packages")
    if not isinstance(packages, list):
        raise ValueError("proposed registry has no local package plan")
    matching = [
        package
        for package in packages
        if isinstance(package, dict) and package.get("package_id") == args.package_id
    ]
    if len(matching) != 1:
        raise ValueError("proposed registry has no unique matching package plan")
    package_plan = matching[0]
    dispatcher = package_plan.get("dispatcher")
    package_manifest_sha256 = package_plan.get("package_manifest_sha256")
    if (
        package_plan.get("package_sha256") != args.package_sha256
        or package_plan.get("ordered_scope_ids") is None
        or not isinstance(dispatcher, dict)
        or set(dispatcher) != {"binary", "sha256"}
        or dispatcher.get("binary") != args.dispatcher
        or SHA256_RE.fullmatch(str(dispatcher.get("sha256"))) is None
        or not isinstance(package_manifest_sha256, str)
        or SHA256_RE.fullmatch(package_manifest_sha256) is None
    ):
        raise ValueError("receipt dispatcher does not match the proposed package plan")
    public_key = plan["receipt_attestation_public_key"]
    if not isinstance(public_key, str) or SHA256_RE.fullmatch(public_key) is None:
        raise ValueError("stage plan has no valid receipt attestation public key")
    return package_plan, public_key


def canonical_receipt_bytes(receipt: dict[str, object]) -> bytes:
    data = (
        json.dumps(receipt, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n"
    ).encode("utf-8")
    if len(data) > MAX_RECEIPT_BYTES:
        raise ValueError("final package conformance receipt exceeds its byte bound")
    return data


def write_conformance_receipt(
    result: dict[str, object],
    args: argparse.Namespace,
    package_bytes: int,
) -> Path:
    """Create one signed receipt bound to exact stage, cache, report, and package."""
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    receipt = {
        "schema_version": 1,
        "passed": result["passed"],
        "stage_id": args.stage_id,
        "scope_id": result["scope_id"],
        "package_id": args.package_id,
        "package_sha256": args.package_sha256,
        "package_bytes": package_bytes,
        "cache_sha256": result["cache_sha256"],
        "compatibility_report_sha256": result["compatibility_report_sha256"],
        "admission_implementation_bundle_sha256": (
            ADMISSION_IMPLEMENTATION_BUNDLE_SHA256
        ),
        "wire_groupings": result["wire_groupings"],
        "sequence_buckets": result["sequence_buckets"],
        "signed_requests": result["signed_requests"],
    }
    claim_bytes = canonical_receipt_bytes(receipt)
    private_path = args.receipt_attestation_private_key
    if private_path.is_symlink() or private_path.resolve() != private_path.absolute():
        raise ValueError("receipt attestation key must be a regular non-symlink path")
    private_bytes = private_path.read_bytes()
    if len(private_bytes) != 32:
        raise ValueError("receipt attestation key must contain exactly 32 raw bytes")
    try:
        private_key = Ed25519PrivateKey.from_private_bytes(private_bytes)
    except ValueError as error:
        raise ValueError("receipt attestation private key is invalid") from error
    public_hex = private_key.public_key().public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    ).hex()
    if public_hex != args.receipt_attestation_public_key:
        raise ValueError("receipt attestation key does not match the stage plan")
    signature = private_key.sign(claim_bytes).hex()
    if private_path.read_bytes() != private_bytes:
        raise ValueError("receipt attestation key changed while the receipt was signed")
    envelope = {
        "schema_version": 1,
        "receipt": receipt,
        "signature": signature,
    }
    data = canonical_receipt_bytes(envelope)
    digest = hashlib.sha256(data).hexdigest()
    directory = args.receipt_directory
    if directory.exists() or directory.is_symlink():
        if directory.is_symlink() or not directory.is_dir():
            raise ValueError("receipt directory must be a real directory")
    else:
        if directory.parent.is_symlink() or not directory.parent.is_dir():
            raise ValueError("receipt directory parent must be a real directory")
        directory.mkdir()
    destination = directory / f"{digest}.json"
    try:
        with destination.open("xb") as output:
            output.write(data)
    except FileExistsError as error:
        raise ValueError(f"refusing to overwrite conformance receipt: {destination}") from error
    return destination


def load_retained_conformance(
    path: Path, expected_cache_sha256: str
) -> tuple[
    dict[str, object],
    dict[str, object],
    np.ndarray,
    tuple[np.ndarray, ...],
]:
    """Load the already bounded, schema-validated cache without trusting summaries."""
    import numpy as np

    validate_admission_cache_container(path)
    actual_before = file_sha256(path)
    if actual_before != expected_cache_sha256:
        raise ValueError(
            f"{path}: cache sha256 {actual_before}, expected {expected_cache_sha256}"
        )
    metadata, _, _ = load_cache(path)
    with np.load(path, allow_pickle=False) as cached:
        wire_outputs = np.array(cached["wire_batch_outputs"], copy=True)
        probe_outputs = tuple(
            np.array(cached[name], copy=True) for name in SEQUENCE_PROBE_ARRAY_NAMES
        )
        sequence_evidence = np.asarray(
            cached["sequence_capability_evidence_bytes"]
        ).tobytes()
    actual_after = file_sha256(path)
    if actual_after != actual_before:
        raise ValueError(f"{path}: admission cache changed during conformance loading")
    sequence_report = parse_evidence_json(
        sequence_evidence, f"{path}: sequence evidence"
    )
    return metadata, sequence_report, wire_outputs, probe_outputs


def expected_final_execution(
    metadata: dict[str, object], compatibility_report_sha256: str
) -> dict[str, object]:
    execution = {field: metadata[field] for field in FINAL_EXECUTION_FIELDS}
    execution["compatibility_report_sha256"] = compatibility_report_sha256
    return execution


def replay_retained_outputs(
    endpoint: str,
    requested_scope_id: str,
    canonical_wire_inputs: Sequence[str],
    sequence_report: dict[str, object],
    retained_wire_outputs: np.ndarray,
    retained_probe_outputs: tuple[np.ndarray, ...],
    timeout_seconds: float,
    bearer_token: str | None,
    wire_request: RequestFunction,
    probe_request: SequenceProbeRequestFunction,
) -> None:
    """Run all 64 groupings and seven two-run bucket probes against one package."""
    import numpy as np

    if sequence_report.get("supported_sequence_buckets") != SEQUENCE_BUCKETS:
        raise ValueError(
            "final package sequence evidence must cover every admitted sequence bucket"
        )
    live_wire_outputs = verify_wire_batch_contract(
        endpoint,
        requested_scope_id,
        canonical_wire_inputs,
        timeout_seconds,
        bearer_token,
        sequence_report,
        wire_request,
    )
    if not np.array_equal(live_wire_outputs, retained_wire_outputs):
        raise ValueError(
            "final package wire-grouping outputs do not byte-match the admitted cache"
        )

    live_probe_outputs = collect_sequence_probe_arrays(sequence_report, probe_request)
    if len(live_probe_outputs) != len(retained_probe_outputs):
        raise ValueError("final package returned an incomplete sequence-probe set")
    for name, live, retained in zip(
        SEQUENCE_PROBE_ARRAY_NAMES,
        live_probe_outputs,
        retained_probe_outputs,
        strict=True,
    ):
        if not np.array_equal(live, retained):
            raise ValueError(
                f"final package {name} does not byte-match the admitted cache"
            )


def run_final_package_conformance(
    args: argparse.Namespace,
    opener: urllib.request.OpenerDirector | None = None,
) -> dict[str, object]:
    endpoint = validate_loopback_endpoint(args.endpoint)
    metadata, sequence_report, retained_wire, retained_probes = (
        load_retained_conformance(args.cache, args.cache_sha256)
    )
    scope_id = metadata["scope_id"]
    public_key = metadata["attestation_public_key"]
    if not isinstance(scope_id, str) or not isinstance(public_key, str):
        raise ValueError("validated cache omitted its scope or package attestation key")

    bearer_token = None
    if args.bearer_token_env is not None:
        bearer_token = os.environ.get(args.bearer_token_env)
        if not bearer_token:
            raise ValueError(
                f"environment variable {args.bearer_token_env!r} is missing or empty"
            )

    query_inputs, document_inputs = load_scifact_inputs()
    canonical_wire_inputs = wire_batch_inputs(query_inputs, document_inputs)
    validate_wire_batch_output_cache(
        args.cache, ordered_input_json_sha256(canonical_wire_inputs)
    )
    execution = expected_final_execution(
        metadata, args.compatibility_report_sha256
    )
    used_nonces: set[bytes] = set()
    active_opener = opener if opener is not None else local_opener()

    def signed_wire_request(
        request_endpoint: str,
        request_scope_id: str,
        texts: Sequence[str],
        request_timeout: float,
        request_token: str | None,
    ) -> object:
        return request_embeddings(
            request_endpoint,
            request_scope_id,
            texts,
            request_timeout,
            request_token,
            opener=active_opener,
            expected_execution=execution,
            attestation_public_key=public_key,
            expected_compatibility_report_sha256=args.compatibility_report_sha256,
            used_attestation_nonces=used_nonces,
        )

    def signed_probe_request(
        texts: Sequence[str], expected_row_metadata: Sequence[dict[str, object]]
    ) -> object:
        return request_embeddings(
            endpoint,
            scope_id,
            texts,
            args.timeout_seconds,
            bearer_token,
            opener=active_opener,
            expected_execution=execution,
            attestation_public_key=public_key,
            expected_row_metadata=expected_row_metadata,
            expected_compatibility_report_sha256=args.compatibility_report_sha256,
            used_attestation_nonces=used_nonces,
        )

    replay_retained_outputs(
        endpoint,
        scope_id,
        canonical_wire_inputs,
        sequence_report,
        retained_wire,
        retained_probes,
        args.timeout_seconds,
        bearer_token,
        signed_wire_request,
        signed_probe_request,
    )
    expected_signed_requests = sum(
        row["request_count"] for row in sequence_report["wire_batch_results"]
    ) + 2 * len(SEQUENCE_BUCKETS)
    if len(used_nonces) != expected_signed_requests:
        raise ValueError(
            "final package conformance did not issue one fresh signed challenge "
            "for every adapter request"
        )
    return {
        "passed": True,
        "scope_id": scope_id,
        "cache_sha256": args.cache_sha256,
        "compatibility_report_sha256": args.compatibility_report_sha256,
        "wire_groupings": SUPPORTED_MAX_BATCH_SIZE,
        "sequence_buckets": len(SEQUENCE_BUCKETS),
        "signed_requests": len(used_nonces),
    }


def main() -> None:
    args = build_parser().parse_args()
    try:
        write_receipt = validate_receipt_arguments(args)
        package_size = None
        if write_receipt:
            package_size = args.package_asset.stat().st_size
            validate_admission_cache_container(args.cache)
            if file_sha256(args.cache) != args.cache_sha256:
                raise ValueError("receipt cache bytes do not match --cache-sha256")
            metadata, _, _ = load_cache(args.cache)
            scope_id = metadata.get("scope_id")
            if not isinstance(scope_id, str):
                raise ValueError("validated cache omitted its exact scope id")
            package_plan, receipt_public_key = validate_stage_receipt_binding(
                args, scope_id
            )
            args.receipt_attestation_public_key = receipt_public_key
            ordered_scope_ids = package_plan.get("ordered_scope_ids")
            dispatcher = package_plan.get("dispatcher")
            package_manifest_sha256 = package_plan.get("package_manifest_sha256")
            final_execution = expected_final_execution(
                metadata, args.compatibility_report_sha256
            )
            attestation_public_key = metadata.get("attestation_public_key")
            if (
                not isinstance(ordered_scope_ids, list)
                or any(not isinstance(item, str) for item in ordered_scope_ids)
                or not isinstance(dispatcher, dict)
                or not isinstance(dispatcher.get("sha256"), str)
                or not isinstance(package_manifest_sha256, str)
                or not isinstance(attestation_public_key, str)
            ):
                raise ValueError("staged package plan has invalid runtime bindings")
            with launch_exact_package(
                args.package_asset,
                args.package_sha256,
                args.dispatcher,
                dispatcher["sha256"],
                package_manifest_sha256,
                ordered_scope_ids,
                scope_id,
                final_execution,
                attestation_public_key,
            ) as (endpoint, bearer_token):
                args.endpoint = endpoint
                internal_bearer_env = "CFETCH_FINAL_PACKAGE_INTERNAL_BEARER"
                if internal_bearer_env in os.environ:
                    raise ValueError(
                        f"reserved environment variable {internal_bearer_env} is already set"
                    )
                os.environ[internal_bearer_env] = bearer_token
                args.bearer_token_env = internal_bearer_env
                try:
                    result = run_final_package_conformance(args)
                finally:
                    os.environ.pop(internal_bearer_env, None)
        else:
            result = run_final_package_conformance(args)
        receipt_path = None
        if write_receipt:
            if (
                args.package_asset.stat().st_size != package_size
                or file_sha256(args.package_asset) != args.package_sha256
            ):
                raise ValueError(
                    "target package asset changed during final conformance"
                )
            receipt_path = write_conformance_receipt(result, args, package_size)
    except (OSError, RuntimeError, ValueError) as error:
        raise SystemExit(f"final package conformance failed: {error}") from error
    output = dict(result)
    if receipt_path is not None:
        output["receipt"] = str(receipt_path)
    print(json.dumps(output, sort_keys=True))


if __name__ == "__main__":
    main()
