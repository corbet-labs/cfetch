#!/usr/bin/env python3
"""Freeze the adapter and its exact Linux x86_64 runtime with PyInstaller."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from typing import Sequence

if __package__:
    from .package_inventory import (
        INVENTORY_NAME,
        InventoryError,
        create as create_inventory,
        patch_launcher,
        verify_bound as verify_inventory,
    )
    from .runtime_bundle import (
        DISPATCHER,
        LAUNCHER,
        RuntimeBundleError,
        create_manifest,
    )
else:
    from package_inventory import (  # type: ignore[no-redef]
        INVENTORY_NAME,
        InventoryError,
        create as create_inventory,
        patch_launcher,
        verify_bound as verify_inventory,
    )
    from runtime_bundle import (  # type: ignore[no-redef]
        DISPATCHER,
        LAUNCHER,
        RuntimeBundleError,
        create_manifest,
    )


class RuntimeBuildError(ValueError):
    """The frozen dispatcher could not be built as one validated payload."""


def _is_optional_requested_marker(relative: Path) -> bool:
    parts = relative.parts
    if len(parts) != 3 or parts[0] != "_internal" or parts[2] != "REQUESTED":
        return False
    directory = parts[1]
    suffix = ".dist-info"
    distribution = directory[: -len(suffix)] if directory.endswith(suffix) else ""
    return bool(distribution) and all(
        character.isascii() and (character.isalnum() or character in "._-")
        for character in distribution
    )


def _prune_optional_empty_metadata(root: Path) -> None:
    """Remove only pip's empty REQUESTED markers from the frozen payload."""

    root = root.resolve()
    requested_markers: list[Path] = []
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        current = Path(directory)
        directory_names.sort()
        file_names.sort()
        for name in directory_names:
            path = current / name
            if path.is_symlink():
                raise RuntimeBuildError(
                    f"PyInstaller emitted symlink after materialization: {path}"
                )
        for name in file_names:
            path = current / name
            relative = path.relative_to(root)
            if path.is_symlink() or not path.is_file():
                raise RuntimeBuildError(
                    f"PyInstaller emitted unsupported filesystem entry {path}"
                )
            if path.stat().st_size != 0:
                continue
            if not _is_optional_requested_marker(relative):
                raise RuntimeBuildError(
                    f"PyInstaller emitted unsupported empty file {relative.as_posix()}"
                )
            requested_markers.append(path)
    for path in requested_markers:
        path.unlink()


def _materialize_file_symlinks(root: Path) -> None:
    """Turn PyInstaller's relocatable in-tree library links into bound files."""

    root = root.resolve()
    stack = [root]
    links: list[Path] = []
    while stack:
        directory = stack.pop()
        with os.scandir(directory) as entries:
            for entry in entries:
                path = Path(entry.path)
                if entry.is_symlink():
                    links.append(path)
                elif entry.is_dir(follow_symlinks=False):
                    stack.append(path)
                elif not entry.is_file(follow_symlinks=False):
                    raise RuntimeBuildError(
                        f"PyInstaller emitted unsupported filesystem entry {path}"
                    )
    for link in sorted(links, key=lambda path: path.as_posix()):
        try:
            target = link.resolve(strict=True)
            target.relative_to(root)
        except (OSError, ValueError) as error:
            raise RuntimeBuildError(
                f"PyInstaller symlink escapes or is broken: {link}"
            ) from error
        if not target.is_file():
            raise RuntimeBuildError(
                f"PyInstaller emitted unsupported directory symlink {link}"
            )
        mode = 0o755 if target.stat().st_mode & 0o111 else 0o644
        link.unlink()
        shutil.copyfile(target, link)
        os.chmod(link, mode)


def _compile_launcher(source: Path, output: Path, compiler: str) -> None:
    command = [
        compiler,
        "-std=c17",
        "-O2",
        "-fPIE",
        "-pie",
        "-D_FORTIFY_SOURCE=2",
        "-fstack-protector-strong",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wl,-z,relro,-z,now",
        "-Wl,--build-id=sha1",
        str(source),
        "-o",
        str(output),
    ]
    try:
        completed = subprocess.run(
            command,
            check=False,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except OSError as error:
        raise RuntimeBuildError(f"could not invoke C compiler {compiler!r}") from error
    if completed.returncode != 0:
        raise RuntimeBuildError(
            "native integrity launcher compilation failed: "
            + completed.stderr.strip()[:4096]
        )
    os.chmod(output, 0o755)


def _temporary_runtime_self_check(root: Path) -> None:
    """Exercise the launcher without claiming this runtime is a final package."""

    launcher = root / LAUNCHER
    original = launcher.read_bytes()
    inventory: Path | None = None
    try:
        inventory, digest = create_inventory(root)
        patch_launcher(root, digest)
        verify_inventory(root, digest)
        completed = subprocess.run(
            [str(launcher), "runtime-check"],
            check=False,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=120,
        )
        if completed.returncode != 0:
            raise RuntimeBuildError(
                "frozen launcher/runtime self-check failed: "
                + completed.stderr.strip()[:4096]
            )
        try:
            report = json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeBuildError(
                "frozen launcher/runtime self-check did not emit one JSON result"
            ) from error
        if not isinstance(report, dict) or report.get("schema_version") != 1:
            raise RuntimeBuildError("frozen launcher/runtime self-check result is invalid")
    finally:
        launcher.write_bytes(original)
        os.chmod(launcher, 0o755)
        if inventory is not None:
            inventory.unlink(missing_ok=True)


def build(output_dir: Path, minimum_glibc: str, compiler: str) -> tuple[Path, str]:
    from PyInstaller.__main__ import run as pyinstaller_run

    output_dir = output_dir.resolve()
    if output_dir.exists():
        raise RuntimeBuildError(f"output directory must not exist: {output_dir}")
    output_dir.parent.mkdir(parents=True, exist_ok=True)
    recipe_dir = Path(__file__).resolve().parent
    adapter = recipe_dir / "adapter.py"
    os.environ.setdefault("SOURCE_DATE_EPOCH", "0")
    with tempfile.TemporaryDirectory(prefix="cfetch-openvino-pyinstaller-") as raw_work:
        work = Path(raw_work)
        pyinstaller_run(
            [
                str(adapter),
                f"--name={DISPATCHER}",
                "--onedir",
                "--noconfirm",
                "--clean",
                "--noupx",
                f"--distpath={work / 'dist'}",
                f"--workpath={work / 'work'}",
                f"--specpath={work / 'spec'}",
                "--contents-directory=_internal",
                "--collect-all=openvino",
                "--collect-all=numpy",
                "--collect-all=tokenizers",
                "--collect-all=cryptography",
                "--copy-metadata=openvino",
                "--copy-metadata=numpy",
                "--copy-metadata=tokenizers",
                "--copy-metadata=cryptography",
                "--hidden-import=openvino",
                "--hidden-import=numpy",
                "--hidden-import=tokenizers",
                "--hidden-import=cryptography",
                "--hidden-import=cryptography.hazmat.primitives.asymmetric.ed25519",
                "--exclude-module=torch",
                "--exclude-module=transformers",
                "--exclude-module=safetensors",
            ]
        )
        built = work / "dist" / DISPATCHER
        if not built.is_dir():
            raise RuntimeBuildError("PyInstaller did not create its onedir payload")
        shutil.move(str(built), output_dir)
    if not output_dir.is_dir():
        raise RuntimeBuildError("PyInstaller did not create the expected onedir payload")
    _materialize_file_symlinks(output_dir)
    _prune_optional_empty_metadata(output_dir)
    _compile_launcher(recipe_dir / "launcher.c", output_dir / LAUNCHER, compiler)
    manifest = create_manifest(output_dir, minimum_glibc)
    _temporary_runtime_self_check(output_dir)
    return manifest


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--output-dir", required=True, type=Path)
    result.add_argument("--minimum-glibc", required=True)
    result.add_argument("--cc", default="cc")
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        manifest, digest = build(args.output_dir, args.minimum_glibc, args.cc)
    except (
        InventoryError,
        OSError,
        RuntimeBuildError,
        RuntimeBundleError,
        subprocess.SubprocessError,
    ) as error:
        print(f"OpenVINO runtime build refused: {error}", file=sys.stderr)
        return 1
    print(
        json.dumps(
            {
                "schema_version": 1,
                "runtime_manifest": str(manifest),
                "runtime_manifest_sha256": digest,
            },
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
