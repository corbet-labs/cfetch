#!/usr/bin/env python3
"""Assemble a fail-closed OpenVINO adapter directory from verified inputs.

The command does not generate keys, download artifacts, or invent evidence.
Each candidate scope configuration supplies its already-created unique
Ed25519 key and physical evidence digests.  The final admission transaction
reruns this command with the global compatibility-report digest filled in.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
from typing import Any, Sequence

if __package__:
    from .adapter import Ed25519Signer
    from .manifest import (
        ADMISSION_POLICY_SHA256,
        MODEL,
        MODEL_REVISION,
        PROFILE_ID,
        PROFILE_MANIFEST_SHA256,
        ManifestError,
        load_package_manifest,
        read_bounded_file,
    )
    from .runtime_bundle import (
        DISPATCHER,
        LAUNCHER,
        MANIFEST_NAME as RUNTIME_MANIFEST_NAME,
        RuntimeBundleError,
        load_and_verify as load_runtime_bundle,
    )
    from .package_inventory import (
        INVENTORY_NAME,
        InventoryError,
        create as create_package_inventory,
        patch_launcher,
        verify_bound as verify_package_inventory,
    )
else:
    from adapter import Ed25519Signer  # type: ignore[no-redef]
    from manifest import (  # type: ignore[no-redef]
        ADMISSION_POLICY_SHA256,
        MODEL,
        MODEL_REVISION,
        PROFILE_ID,
        PROFILE_MANIFEST_SHA256,
        ManifestError,
        load_package_manifest,
        read_bounded_file,
    )
    from runtime_bundle import (  # type: ignore[no-redef]
        DISPATCHER,
        LAUNCHER,
        MANIFEST_NAME as RUNTIME_MANIFEST_NAME,
        RuntimeBundleError,
        load_and_verify as load_runtime_bundle,
    )
    from package_inventory import (  # type: ignore[no-redef]
        INVENTORY_NAME,
        InventoryError,
        create as create_package_inventory,
        patch_launcher,
        verify_bound as verify_package_inventory,
    )


class AssemblyError(ValueError):
    """The requested package cannot be assembled without weakening identity."""


def _read_config(path: Path) -> dict[str, Any]:
    try:
        raw = read_bounded_file(path, 1024 * 1024, "scope configuration")
    except ManifestError as error:
        raise AssemblyError(str(error)) from error
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AssemblyError(f"scope configuration is not valid UTF-8 JSON: {error}") from error
    if not isinstance(value, dict) or set(value) != {
        "schema_version",
        "package_state",
        "dependency_versions",
        "scopes",
    }:
        raise AssemblyError(
            "scope configuration must contain exactly schema_version, package_state, "
            "dependency_versions, and scopes"
        )
    if value["schema_version"] != 1:
        raise AssemblyError("scope configuration schema_version must be 1")
    if value["package_state"] not in ("physical-probe", "candidate", "release"):
        raise AssemblyError(
            "scope configuration package_state must be physical-probe, candidate, or release"
        )
    if not isinstance(value["scopes"], list) or not value["scopes"]:
        raise AssemblyError("scope configuration scopes must be a nonempty array")
    return value


def _copy_artifact(artifact_dir: Path, destination: Path) -> str:
    source_manifest = artifact_dir / "artifact-manifest.json"
    if not source_manifest.is_file():
        raise AssemblyError("artifact directory has no artifact-manifest.json")
    try:
        raw = read_bounded_file(
            source_manifest, 1024 * 1024, "artifact manifest"
        )
    except ManifestError as error:
        raise AssemblyError(str(error)) from error
    try:
        manifest = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AssemblyError(f"artifact manifest is invalid: {error}") from error
    if not isinstance(manifest, dict) or not isinstance(manifest.get("files"), list):
        raise AssemblyError("artifact manifest has no files array")
    destination.mkdir()
    shutil.copyfile(source_manifest, destination / "artifact-manifest.json")
    for entry in manifest["files"]:
        if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
            raise AssemblyError("artifact manifest files entries must name paths")
        relative = Path(entry["path"])
        if relative.is_absolute() or ".." in relative.parts or len(relative.parts) != 1:
            raise AssemblyError("artifact files must be flat normalized relative paths")
        source = artifact_dir / relative
        if not source.is_file():
            raise AssemblyError(f"artifact file is missing: {relative}")
        shutil.copyfile(source, destination / relative)
    return hashlib.sha256(raw).hexdigest()


def _copy_runtime(
    runtime_dir: Path, destination: Path, expected_manifest_sha256: str
) -> dict[str, Any]:
    document = load_runtime_bundle(runtime_dir, expected_manifest_sha256)
    reserved = {
        "artifact",
        "keys",
        "package-manifest.json",
        INVENTORY_NAME,
    }
    for entry in document["files"]:
        relative = Path(*entry["path"].split("/"))
        if relative.parts[0] in reserved:
            raise AssemblyError(
                f"runtime bundle collides with package path {relative.parts[0]!r}"
            )
        source = runtime_dir / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        os.chmod(target, 0o755 if entry["executable"] else 0o644)
    shutil.copyfile(
        runtime_dir / RUNTIME_MANIFEST_NAME,
        destination / RUNTIME_MANIFEST_NAME,
    )
    os.chmod(destination / RUNTIME_MANIFEST_NAME, 0o644)
    shutil.copyfile(runtime_dir / LAUNCHER, destination / LAUNCHER)
    os.chmod(destination / LAUNCHER, 0o755)
    return document


def assemble(
    artifact_dir: Path,
    runtime_dir: Path,
    runtime_manifest_sha256: str,
    scope_config_path: Path,
    output_dir: Path,
) -> tuple[Path, dict[str, Any]]:
    artifact_dir = artifact_dir.resolve()
    scope_config_path = scope_config_path.resolve()
    output_dir = output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise AssemblyError(f"output directory must be absent or empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    config = _read_config(scope_config_path)
    runtime = _copy_runtime(
        runtime_dir.resolve(), output_dir, runtime_manifest_sha256
    )
    if config["dependency_versions"] != runtime["dependency_versions"]:
        raise AssemblyError(
            "scope configuration dependency_versions do not exactly match the frozen runtime"
        )
    artifact_digest = _copy_artifact(artifact_dir, output_dir / "artifact")
    keys_dir = output_dir / "keys"
    keys_dir.mkdir()
    scopes: list[dict[str, Any]] = []
    for index, original in enumerate(config["scopes"]):
        if not isinstance(original, dict):
            raise AssemblyError(f"scopes[{index}] must be an object")
        scope = dict(original)
        scope_id = scope.get("scope_id")
        key_value = scope.get("attestation_private_key_file")
        if not isinstance(scope_id, str) or not isinstance(key_value, str):
            raise AssemblyError(
                f"scopes[{index}] must name scope_id and attestation_private_key_file"
            )
        source_key = (scope_config_path.parent / key_value).resolve()
        if not source_key.is_file():
            raise AssemblyError(f"scopes[{index}] attestation key is missing")
        target_key = keys_dir / f"{scope_id}.key"
        if target_key.exists():
            raise AssemblyError(f"duplicate or unsafe scope key target for {scope_id!r}")
        shutil.copyfile(source_key, target_key)
        os.chmod(target_key, 0o600)
        scope["attestation_private_key_file"] = f"keys/{scope_id}.key"
        scope["artifact_sha256"] = artifact_digest
        scopes.append(scope)

    package_document = {
        "schema_version": 1,
        "package_state": config["package_state"],
        "profile_id": PROFILE_ID,
        "profile_manifest_sha256": PROFILE_MANIFEST_SHA256,
        "admission_policy_sha256": ADMISSION_POLICY_SHA256,
        "model": MODEL,
        "model_revision": MODEL_REVISION,
        "artifact_manifest": "artifact/artifact-manifest.json",
        "artifact_manifest_sha256": artifact_digest,
        "runtime_manifest_sha256": runtime["runtime_manifest_sha256"],
        "dependency_versions": config["dependency_versions"],
        "scopes": scopes,
    }
    raw = (
        json.dumps(
            package_document,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n"
    ).encode("utf-8")
    manifest_path = output_dir / "package-manifest.json"
    manifest_path.write_bytes(raw)
    package = load_package_manifest(manifest_path)
    for scope in package.scopes.values():
        Ed25519Signer(scope.attestation_private_key_file, scope.attestation_public_key)
    inventory_path, inventory_sha256 = create_package_inventory(output_dir)
    launcher_sha256 = patch_launcher(output_dir, inventory_sha256)
    verify_package_inventory(output_dir, inventory_sha256)
    completed = subprocess.run(
        [str(output_dir / LAUNCHER), "runtime-check"],
        check=False,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=120,
    )
    if completed.returncode != 0:
        raise AssemblyError(
            "assembled frozen runtime self-check failed: "
            + completed.stderr.strip()[:4096]
        )
    runtime["package_inventory"] = inventory_path.name
    runtime["package_inventory_sha256"] = inventory_sha256
    runtime["launcher_sha256"] = launcher_sha256
    return manifest_path, runtime


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--artifact-dir", required=True, type=Path)
    result.add_argument("--runtime-dir", required=True, type=Path)
    result.add_argument("--runtime-manifest-sha256", required=True)
    result.add_argument("--scope-config", required=True, type=Path)
    result.add_argument("--output-dir", required=True, type=Path)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        manifest_path, runtime = assemble(
            args.artifact_dir,
            args.runtime_dir,
            args.runtime_manifest_sha256,
            args.scope_config,
            args.output_dir,
        )
    except (
        AssemblyError,
        InventoryError,
        ManifestError,
        RuntimeBundleError,
        RuntimeError,
        OSError,
        subprocess.SubprocessError,
    ) as error:
        print(f"OpenVINO package assembly refused: {error}")
        return 1
    raw = manifest_path.read_bytes()
    print(
        json.dumps(
            {
                "schema_version": 1,
                "package_manifest": str(manifest_path),
                "package_manifest_sha256": hashlib.sha256(raw).hexdigest(),
                "dispatcher": LAUNCHER,
                "dispatcher_sha256": runtime["launcher_sha256"],
                "runtime_dispatcher": DISPATCHER,
                "package_inventory": runtime["package_inventory"],
                "package_inventory_sha256": runtime[
                    "package_inventory_sha256"
                ],
                "runtime_manifest_sha256": runtime[
                    "runtime_manifest_sha256"
                ],
            },
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
