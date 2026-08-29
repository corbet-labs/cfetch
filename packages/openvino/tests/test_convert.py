from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from packages.openvino import convert


class ConversionContractTests(unittest.TestCase):
    def test_cli_keeps_failure_diagnostic_out_of_result_stdout(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with (
            mock.patch.object(
                convert,
                "convert",
                side_effect=convert.ConversionError("safe conversion failure"),
            ),
            redirect_stdout(stdout),
            redirect_stderr(stderr),
        ):
            status = convert.main(
                [
                    "--source-dir",
                    "unused-source",
                    "--legal-dir",
                    "unused-legal",
                    "--output-dir",
                    "unused-output",
                ]
            )
        self.assertEqual(status, 1)
        self.assertEqual(stdout.getvalue(), "")
        self.assertIn("safe conversion failure", stderr.getvalue())

    def test_source_file_verification_is_exact(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            (root / "one.bin").write_bytes(b"exact")
            expected = {"one.bin": hashlib.sha256(b"exact").hexdigest()}
            convert.verify_source_files(root, expected)
            (root / "one.bin").write_bytes(b"changed")
            with self.assertRaisesRegex(convert.ConversionError, "digest mismatch"):
                convert.verify_source_files(root, expected)

    def test_semantic_source_requires_mean_dense_dense_normalize_contract(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            documents = {
                "modules.json": convert.EXPECTED_MODULES,
                "1_Pooling/config.json": convert.EXPECTED_POOLING,
                "2_Dense/config.json": convert.EXPECTED_DENSE_2,
                "3_Dense/config.json": convert.EXPECTED_DENSE_3,
                "sentence_bert_config.json": convert.EXPECTED_SENTENCE_BERT,
                "config.json": {
                    "architectures": ["Gemma3TextModel"],
                    "dtype": "float32",
                    "hidden_size": 768,
                    "max_position_embeddings": 2048,
                    "model_type": "gemma3_text",
                    "num_hidden_layers": 24,
                    "pad_token_id": 0,
                    "use_bidirectional_attention": True,
                },
            }
            for relative, document in documents.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(document), encoding="utf-8")
            convert.validate_semantic_source(root)
            broken = dict(convert.EXPECTED_DENSE_3)
            broken["activation_function"] = "torch.nn.modules.activation.Tanh"
            (root / "3_Dense/config.json").write_text(
                json.dumps(broken), encoding="utf-8"
            )
            with self.assertRaisesRegex(convert.ConversionError, "frozen mean"):
                convert.validate_semantic_source(root)


if __name__ == "__main__":
    unittest.main()
