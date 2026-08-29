#!/usr/bin/env python3
"""Fetch and validate the exact legal payload required for Gemma derivatives."""

from __future__ import annotations

import argparse
import hashlib
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import shutil
from typing import Any, Sequence
from urllib.request import Request, urlopen


TERMS_URL = "https://ai.google.dev/gemma/terms"
PROHIBITED_USE_URL = "https://ai.google.dev/gemma/prohibited_use_policy"
MAX_SOURCE_BYTES = 2 * 1024 * 1024
MAX_LEGAL_FILE_BYTES = 256 * 1024
LEGAL_FILES = (
    "GEMMA_TERMS.txt",
    "GEMMA_PROHIBITED_USE_POLICY.txt",
    "MODEL_USE_RESTRICTIONS.txt",
    "MODEL_MODIFICATIONS.txt",
    "NOTICE",
)
NOTICE_BYTES = (
    b"Gemma is provided under and subject to the Gemma Terms of Use found at "
    b"ai.google.dev/gemma/terms\n"
)
USE_RESTRICTIONS_BYTES = b"""MODEL DERIVATIVE USE RESTRICTIONS

Use, reproduction, modification, distribution, performance, or display of this
EmbeddingGemma Model Derivative is permitted only in compliance with Section
3.2 of the Gemma Terms of Use included in GEMMA_TERMS.txt. The Gemma Prohibited
Use Policy included in GEMMA_PROHIBITED_USE_POLICY.txt is incorporated into
those restrictions. By using or distributing this Model Derivative, you agree
to those restrictions and must pass them on as an enforceable provision to any
recipient.
"""
MODIFICATIONS_BYTES = b"""PROMINENT MODEL DERIVATIVE MODIFICATION NOTICE

The EmbeddingGemma files in this package were modified by cfetch. The original
Google checkpoint was converted into an OpenVINO IR and composed with the
checkpoint's attention-mask-weighted mean pooling, two bias-free Dense modules,
and L2 normalization. IR constant storage may use FP16 compression. The source
checkpoint identity and every converted artifact digest are recorded in
artifact-manifest.json.
"""

# Filled from the canonical text extracted from the official pages. A page
# change fails closed until the legal payload and its requirements are reviewed.
PINNED_LEGAL_SHA256 = {
    "GEMMA_TERMS.txt": "b3609ee6ac1616e087bb5fe53356202eff274aecde124ab1e065fac5e0eb1f2e",
    "GEMMA_PROHIBITED_USE_POLICY.txt": (
        "14a821208c6a174b08c942112c132a4a802af4efd100ba5ec4086dd9822bc698"
    ),
    "MODEL_USE_RESTRICTIONS.txt": hashlib.sha256(USE_RESTRICTIONS_BYTES).hexdigest(),
    "MODEL_MODIFICATIONS.txt": hashlib.sha256(MODIFICATIONS_BYTES).hexdigest(),
    "NOTICE": hashlib.sha256(NOTICE_BYTES).hexdigest(),
}


class LegalError(ValueError):
    """The legal payload is absent, changed, or incomplete."""


class _ArticleExtractor(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self._capturing = False
        self._div_depth = 0
        self._lists: list[list[Any]] = []
        self._pieces: list[str] = []

    def _break(self, prefix: str = "") -> None:
        self._pieces.append("\n" + prefix)

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attributes = dict(attrs)
        if not self._capturing:
            classes = (attributes.get("class") or "").split()
            if tag == "div" and "devsite-article-body" in classes:
                self._capturing = True
                self._div_depth = 1
            return
        if tag == "div":
            self._div_depth += 1
        if tag == "h2":
            self._break("## ")
        elif tag == "h3":
            self._break("### ")
        elif tag in ("p", "br"):
            self._break()
        elif tag in ("ol", "ul"):
            self._lists.append([tag, 0])
            self._break()
        elif tag == "li":
            if not self._lists:
                raise LegalError("official legal page contains a list item without a list")
            kind, count = self._lists[-1]
            indent = "  " * (len(self._lists) - 1)
            if kind == "ol":
                count += 1
                self._lists[-1][1] = count
                self._break(f"{indent}{count}. ")
            else:
                self._break(f"{indent}- ")

    def handle_endtag(self, tag: str) -> None:
        if not self._capturing:
            return
        if tag in ("p", "li", "h2", "h3"):
            self._break()
        if tag in ("ol", "ul"):
            if not self._lists or self._lists[-1][0] != tag:
                raise LegalError("official legal page has malformed list nesting")
            self._lists.pop()
            self._break()
        if tag == "div":
            self._div_depth -= 1
            if self._div_depth == 0:
                self._capturing = False

    def handle_data(self, data: str) -> None:
        if self._capturing:
            self._pieces.append(data)

    def text(self, title: str, source_url: str) -> bytes:
        if self._div_depth != 0 or not self._pieces:
            raise LegalError("official legal page has no complete article body")
        joined = "".join(self._pieces)
        lines = [re.sub(r"\s+", " ", line).strip() for line in joined.splitlines()]
        output: list[str] = []
        for line in lines:
            if line:
                output.append(line)
            elif output and output[-1] != "":
                output.append("")
        body = "\n".join(output).strip()
        return f"{title}\nSource: {source_url}\n\n{body}\n".encode("utf-8")


def extract_official_article(raw: bytes, title: str, source_url: str) -> bytes:
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise LegalError("official legal page is not UTF-8") from error
    parser = _ArticleExtractor()
    try:
        parser.feed(text)
        parser.close()
    except Exception as error:
        if isinstance(error, LegalError):
            raise
        raise LegalError(f"cannot parse official legal page: {error}") from error
    return parser.text(title, source_url)


def _fetch(url: str) -> bytes:
    request = Request(
        url,
        headers={
            "Accept": "text/html",
            "User-Agent": "cfetch-openvino-legal-fetch/1",
        },
    )
    with urlopen(request, timeout=30) as response:
        if response.geturl() != url:
            raise LegalError(f"official legal URL redirected unexpectedly: {response.geturl()}")
        raw = response.read(MAX_SOURCE_BYTES + 1)
    if not raw or len(raw) > MAX_SOURCE_BYTES:
        raise LegalError("official legal page is empty or exceeds its byte limit")
    return raw


def canonical_legal_files() -> dict[str, bytes]:
    terms = extract_official_article(
        _fetch(TERMS_URL), "Gemma Terms of Use", TERMS_URL
    )
    prohibited = extract_official_article(
        _fetch(PROHIBITED_USE_URL),
        "Gemma Prohibited Use Policy",
        PROHIBITED_USE_URL,
    )
    if not all(
        marker in terms
        for marker in (
            b"Last modified: April 1, 2026",
            b"Section 3: DISTRIBUTION AND RESTRICTIONS",
            b"EmbeddingGemma",
        )
    ):
        raise LegalError("official Gemma Terms extraction is incomplete")
    if not all(
        marker in prohibited
        for marker in (
            b"Last modified: February 21, 2024",
            b"You may not use nor allow others to use Gemma or Model Derivatives",
        )
    ):
        raise LegalError("official Prohibited Use Policy extraction is incomplete")
    return {
        "GEMMA_TERMS.txt": terms,
        "GEMMA_PROHIBITED_USE_POLICY.txt": prohibited,
        "MODEL_USE_RESTRICTIONS.txt": USE_RESTRICTIONS_BYTES,
        "MODEL_MODIFICATIONS.txt": MODIFICATIONS_BYTES,
        "NOTICE": NOTICE_BYTES,
    }


def _bounded_read(path: Path) -> bytes:
    try:
        metadata = path.stat()
    except OSError as error:
        raise LegalError(f"cannot inspect legal file {path.name}: {error}") from error
    if path.is_symlink() or not path.is_file():
        raise LegalError(f"legal file must be regular and non-symlink: {path.name}")
    if metadata.st_size < 1 or metadata.st_size > MAX_LEGAL_FILE_BYTES:
        raise LegalError(f"legal file has an invalid size: {path.name}")
    with path.open("rb") as source:
        raw = source.read(MAX_LEGAL_FILE_BYTES + 1)
    if not raw or len(raw) > MAX_LEGAL_FILE_BYTES:
        raise LegalError(f"legal file changed size while read: {path.name}")
    return raw


def validate_legal_dir(directory: Path) -> dict[str, str]:
    directory = directory.resolve()
    if not directory.is_dir() or directory.is_symlink():
        raise LegalError("legal payload must be a regular directory")
    names: set[str] = set()
    for index, path in enumerate(directory.iterdir()):
        if index >= len(LEGAL_FILES):
            raise LegalError("legal payload contains unexpected files")
        names.add(path.name)
    if names != set(LEGAL_FILES):
        raise LegalError("legal payload files do not exactly match the required set")
    return validate_embedded_legal(directory)


def validate_embedded_legal(directory: Path) -> dict[str, str]:
    directory = directory.resolve()
    if not directory.is_dir() or directory.is_symlink():
        raise LegalError("embedded legal payload directory is invalid")
    digests: dict[str, str] = {}
    for name in LEGAL_FILES:
        digest = hashlib.sha256(_bounded_read(directory / name)).hexdigest()
        expected = PINNED_LEGAL_SHA256[name]
        if digest != expected:
            raise LegalError(
                f"legal file digest mismatch for {name}: expected {expected}, found {digest}"
            )
        digests[name] = digest
    return digests


def fetch(directory: Path) -> dict[str, str]:
    directory = directory.resolve()
    if directory.exists() and any(directory.iterdir()):
        raise LegalError(f"legal output directory must be absent or empty: {directory}")
    directory.mkdir(parents=True, exist_ok=True)
    for name, raw in canonical_legal_files().items():
        (directory / name).write_bytes(raw)
    return validate_legal_dir(directory)


def copy_legal_payload(source: Path, destination: Path) -> dict[str, str]:
    digests = validate_legal_dir(source)
    for name in LEGAL_FILES:
        shutil.copyfile(source / name, destination / name)
    return digests


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    subcommands = result.add_subparsers(dest="command", required=True)
    fetch_parser = subcommands.add_parser("fetch")
    fetch_parser.add_argument("--output-dir", required=True, type=Path)
    validate_parser = subcommands.add_parser("validate")
    validate_parser.add_argument("--directory", required=True, type=Path)
    subcommands.add_parser("show-current-digests")
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "fetch":
            digests = fetch(args.output_dir)
        elif args.command == "validate":
            digests = validate_legal_dir(args.directory)
        else:
            digests = {
                name: hashlib.sha256(raw).hexdigest()
                for name, raw in canonical_legal_files().items()
            }
    except (LegalError, OSError) as error:
        print(f"Gemma legal payload refused: {error}")
        return 1
    print(json.dumps({"schema_version": 1, "files": digests}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
