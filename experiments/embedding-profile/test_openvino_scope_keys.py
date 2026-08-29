#!/usr/bin/env python3
"""Tests for package-scope key generation and file format."""

from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import unittest

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from openvino_scope_keys import generate_scope_keys


class OpenVinoScopeKeyTests(unittest.TestCase):
    def test_generates_three_distinct_matching_hex_keys_without_overwrite(self) -> None:
        scope_ids = ("intel-lnl-npu", "intel-lnl-gpu", "intel-lnl-cpu")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "keys"
            manifest_path = generate_scope_keys(scope_ids, output)
            manifest = json.loads(manifest_path.read_bytes())
            self.assertEqual(manifest["schema_version"], 1)
            self.assertEqual(
                [row["scope_id"] for row in manifest["keys"]], list(scope_ids)
            )
            public_keys: set[str] = set()
            for row in manifest["keys"]:
                key_path = output / row["attestation_private_key_file"]
                private_hex = key_path.read_text().removesuffix("\n")
                self.assertRegex(private_hex, r"^[0-9a-f]{64}$")
                self.assertEqual(os.stat(key_path).st_mode & 0o777, 0o600)
                private_key = Ed25519PrivateKey.from_private_bytes(
                    bytes.fromhex(private_hex)
                )
                actual_public = private_key.public_key().public_bytes(
                    encoding=serialization.Encoding.Raw,
                    format=serialization.PublicFormat.Raw,
                )
                self.assertEqual(actual_public.hex(), row["attestation_public_key"])
                public_keys.add(actual_public.hex())
            self.assertEqual(len(public_keys), 3)
            with self.assertRaisesRegex(ValueError, "must not already exist"):
                generate_scope_keys(scope_ids, output)

    def test_rejects_wrong_count_duplicates_and_noncanonical_scope_ids(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases = (
                ["npu", "gpu"],
                ["same", "same", "cpu"],
                ["NPU", "gpu", "cpu"],
            )
            for index, scope_ids in enumerate(cases):
                with self.subTest(scope_ids=scope_ids), self.assertRaises(ValueError):
                    generate_scope_keys(scope_ids, root / f"keys-{index}")


if __name__ == "__main__":
    unittest.main()
