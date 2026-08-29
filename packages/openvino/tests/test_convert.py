from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from packages.openvino import convert


class ConversionContractTests(unittest.TestCase):
    def test_masked_mean_selects_real_tokens_before_reduction(self) -> None:
        calls = []

        class FakeMask:
            def unsqueeze(self, axis):
                calls.append(("unsqueeze", axis))
                return self

            def bool(self):
                calls.append(("bool",))
                return "real-token-mask"

            def sum(self, **kwargs):
                calls.append(("mask-sum", kwargs))
                return FakeDenominator()

        class FakeDenominator:
            def to(self, dtype):
                calls.append(("denominator-to", dtype))
                return self

            def clamp_min(self, value):
                calls.append(("clamp-min", value))
                return "safe-denominator"

        class FakeEmbeddings:
            dtype = "embedding-dtype"

        class FakeSelected:
            def sum(self, **kwargs):
                calls.append(("selected-sum", kwargs))
                return FakeNumerator()

        class FakeNumerator:
            def __truediv__(self, denominator):
                calls.append(("divide", denominator))
                return "pooled"

        embeddings = FakeEmbeddings()
        fake_torch = SimpleNamespace(
            zeros_like=lambda value: calls.append(("zeros-like", value)) or "zeros",
            where=lambda condition, value, zero: calls.append(
                ("where", condition, value, zero)
            )
            or FakeSelected(),
        )
        with mock.patch.dict("sys.modules", {"torch": fake_torch}):
            result = convert.masked_mean(embeddings, FakeMask())

        self.assertEqual(result, "pooled")
        where_index = next(
            index for index, call in enumerate(calls) if call[0] == "where"
        )
        reduction_index = next(
            index for index, call in enumerate(calls) if call[0] == "selected-sum"
        )
        self.assertLess(where_index, reduction_index)
        self.assertIn(("where", "real-token-mask", embeddings, "zeros"), calls)
        self.assertIn(("selected-sum", {"dim": 1}), calls)
        self.assertIn(("mask-sum", {"dim": 1, "keepdim": True}), calls)

    def test_torch_export_binds_one_bounded_sequence_symbol_to_both_inputs(
        self,
    ) -> None:
        dimensions = []
        calls = []

        class FakeDim:
            def __init__(self, name, **bounds):
                self.name = name
                self.bounds = bounds
                dimensions.append(self)

        def fake_export(*args, **kwargs):
            calls.append((args, kwargs))
            return "exported-program"

        fake_torch = SimpleNamespace(
            export=SimpleNamespace(Dim=FakeDim, export=fake_export)
        )
        with mock.patch.dict("sys.modules", {"torch": fake_torch}):
            result = convert.export_torch_pipeline(
                "pipeline", "example-ids", "example-mask"
            )

        self.assertEqual(result, "exported-program")
        self.assertEqual(len(dimensions), 1)
        self.assertEqual(dimensions[0].name, "sequence")
        self.assertEqual(dimensions[0].bounds, {"min": 1, "max": 2048})
        args, kwargs = calls[0]
        self.assertEqual(args, ("pipeline", ("example-ids", "example-mask")))
        id_shapes, mask_shapes = kwargs["dynamic_shapes"]
        self.assertIs(id_shapes[1], dimensions[0])
        self.assertIs(mask_shapes[1], dimensions[0])
        self.assertIs(kwargs["strict"], False)

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
