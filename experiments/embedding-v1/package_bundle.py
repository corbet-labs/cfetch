#!/usr/bin/env python3
"""Create the deterministic, separately licensed cfetch embedding-v1 bundle."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import tarfile


ARTIFACT_ID = "cfetch-embeddinggemma-300m-a8w8-v1"
SOURCE_MODEL = "google/embeddinggemma-300m-qat-q8_0-unquantized"
SOURCE_REVISION = "7b5b24595322ab0ea4d08827066860a6df8cb0aa"
TOKENIZER_FILES = {
    "tokenizer.json": "6852f8d561078cc0cebe70ca03c5bfdd0d60a45f9d2e0e1e4cc05b68e9ec329e",
    "tokenizer_config.json": "9076840490613047bc9115963ee96b7702018b0d26ba644240bf856efda93118",
    "config.json": "8f863f76e2d9c710cc833dc92efa898c9adfd41031c786507cc6b0e49c2e3e68",
    "special_tokens_map.json": "2f7b0adf4fb469770bb1490e3e35df87b1dc578246c5e7e6fc76ecf33213a397",
}
NOTICE = "Gemma is provided under and subject to the Gemma Terms of Use found at ai.google.dev/gemma/terms\n"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def add_bytes(archive: tarfile.TarFile, name: str, data: bytes) -> None:
    info = tarfile.TarInfo(f"{ARTIFACT_ID}/{name}")
    info.size = len(data)
    info.mode = 0o644
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    import io

    archive.addfile(info, io.BytesIO(data))


def add_file(archive: tarfile.TarFile, name: str, path: Path) -> None:
    info = tarfile.TarInfo(f"{ARTIFACT_ID}/{name}")
    info.size = path.stat().st_size
    info.mode = 0o644
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    with path.open("rb") as source:
        archive.addfile(info, source)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--source-dir", required=True, type=Path)
    parser.add_argument("--build-report", required=True, type=Path)
    parser.add_argument("--retrieval-report", required=True, type=Path)
    parser.add_argument("--artifact-lock", required=True, type=Path)
    parser.add_argument("--gemma-terms", required=True, type=Path)
    parser.add_argument("--prohibited-use-policy", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    if args.output.exists():
        raise SystemExit(f"refusing to overwrite existing bundle: {args.output}")
    build = json.loads(args.build_report.read_text())
    retrieval = json.loads(args.retrieval_report.read_text())
    lock = json.loads(args.artifact_lock.read_text())
    model_hash = sha256_file(args.model)
    if lock.get("decision") != "accepted-as-network-major-1":
        raise SystemExit("artifact lock does not record the network-major-1 decision")
    if lock.get("artifact_id") != ARTIFACT_ID:
        raise SystemExit("artifact lock names a different artifact ID")
    if lock.get("artifact_sha256") != model_hash:
        raise SystemExit("artifact lock does not admit the supplied model digest")
    if lock.get("build_report_sha256") != sha256_file(args.build_report):
        raise SystemExit("artifact lock does not admit the supplied build report")
    if lock.get("retrieval_report_sha256") != sha256_file(args.retrieval_report):
        raise SystemExit("artifact lock does not admit the supplied retrieval report")
    if build.get("artifact_sha256") != model_hash:
        raise SystemExit("build report does not name the supplied model digest")
    candidate_hash = retrieval.get("results", {}).get("candidate", {}).get(
        "model_sha256"
    )
    if candidate_hash != model_hash:
        raise SystemExit("retrieval report does not evaluate the supplied model digest")

    payload: dict[str, Path] = {
        "model.onnx": args.model,
        "model.onnx.build.json": args.build_report,
        "scifact.json": args.retrieval_report,
        "v1-artifact-lock.json": args.artifact_lock,
        "GEMMA_TERMS.html": args.gemma_terms,
        "GEMMA_PROHIBITED_USE_POLICY.html": args.prohibited_use_policy,
    }
    model_card = args.source_dir / "README.md"
    if model_card.is_file():
        payload["SOURCE_MODEL_CARD.md"] = model_card
    for name, expected in TOKENIZER_FILES.items():
        path = args.source_dir / name
        actual = sha256_file(path)
        if actual != expected:
            raise SystemExit(
                f"{name} has SHA-256 {actual}, profile requires {expected}"
            )
        payload[name] = path

    terms_text = args.gemma_terms.read_text(errors="replace")
    policy_text = args.prohibited_use_policy.read_text(errors="replace")
    if "Gemma Terms of Use" not in terms_text or "Prohibited Use" not in policy_text:
        raise SystemExit("supplied Gemma agreement/policy copies do not identify themselves")

    modification = (
        f"# Modification notice\n\n"
        f"`model.onnx` is a modified Gemma model derivative produced by cfetch "
        f"contributors from `{SOURCE_MODEL}` at revision `{SOURCE_REVISION}`. "
        "The source checkpoint was exported to ONNX opset 18, statically "
        "quantized to the cfetch v1 signed W8A8 Q/DQ profile, and packaged "
        "with its masked-mean pooling, projection, and L2-normalization "
        f"pipeline. Its SHA-256 is `{model_hash}`.\n"
    ).encode()
    model_license = (
        "CFETCH EMBEDDING MODEL BUNDLE TERMS\n\n"
        "The model and tokenizer files in this archive are not licensed under "
        "the cfetch software license. They are Gemma or Gemma Model "
        "Derivatives distributed solely under and subject to the included "
        "GEMMA_TERMS.html. The use restrictions in section 3.2 of those terms, "
        "including the included Gemma Prohibited Use Policy, are enforceable "
        "conditions of using, reproducing, modifying, or redistributing this "
        "bundle. By doing any of those things, you agree to those terms and "
        "restrictions.\n"
    ).encode()
    file_hashes = {
        name: sha256_file(path) for name, path in sorted(payload.items())
    }
    file_hashes["MODEL_LICENSE.txt"] = hashlib.sha256(model_license).hexdigest()
    file_hashes["MODIFICATIONS.md"] = hashlib.sha256(modification).hexdigest()
    file_hashes["Notice"] = hashlib.sha256(NOTICE.encode()).hexdigest()
    manifest = {
        "schema": 1,
        "artifact_id": ARTIFACT_ID,
        "artifact_sha256": model_hash,
        "network_major": 1,
        "profile_id": "cfetch-embedding-v1",
        "source_model": SOURCE_MODEL,
        "source_revision": SOURCE_REVISION,
        "model_license": "Gemma Terms of Use (included; separate from cfetch)",
        "files": file_hashes,
    }
    manifest_bytes = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    sums = "".join(
        f"{digest}  {name}\n" for name, digest in sorted(file_hashes.items())
    ).encode()

    temporary = args.output.with_suffix(args.output.suffix + ".tmp")
    if temporary.exists():
        raise SystemExit(f"refusing existing temporary output: {temporary}")
    try:
        with temporary.open("xb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w|") as archive:
                    for name, path in sorted(payload.items()):
                        add_file(archive, name, path)
                    add_bytes(archive, "BUNDLE.json", manifest_bytes)
                    add_bytes(archive, "MODEL_LICENSE.txt", model_license)
                    add_bytes(archive, "MODIFICATIONS.md", modification)
                    add_bytes(archive, "Notice", NOTICE.encode())
                    add_bytes(archive, "SHA256SUMS", sums)
        temporary.replace(args.output)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    print(
        json.dumps(
            {
                "bundle": str(args.output),
                "bundle_sha256": sha256_file(args.output),
                "artifact_sha256": model_hash,
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
