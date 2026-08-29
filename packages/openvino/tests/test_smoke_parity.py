from __future__ import annotations

from types import SimpleNamespace
import unittest

from packages.openvino import smoke_parity


class FrozenFixtureTokenizer:
    def __init__(self, offset: int = 0) -> None:
        self.offset = offset

    def encode(self, text: str, *, add_special_tokens: bool):
        if add_special_tokens:
            raise AssertionError("fixture tokenization must add BOS/EOS explicitly")
        if text.startswith("task: search result | query: "):
            prefix_tokens = 7
        elif text.startswith("title: none | text: "):
            prefix_tokens = 6
        else:
            raise AssertionError("unexpected semantic fixture prefix")
        topic_tokens = text.count("cat") + text.count("music")
        return SimpleNamespace(
            ids=[0] * (prefix_tokens + topic_tokens + self.offset)
        )


class SemanticFixtureSmokeTests(unittest.TestCase):
    def test_every_fixture_input_reaches_its_exact_bucket(self) -> None:
        results = smoke_parity.validate_sequence_semantic_fixture(
            FrozenFixtureTokenizer()
        )
        self.assertEqual(
            [row["bucket"] for row in results],
            [32, 64, 128, 256, 512, 1024, 2048],
        )
        self.assertTrue(
            all(row["token_counts"] == [row["bucket"]] * 3 for row in results)
        )

    def test_fixture_refuses_tokenizer_contract_drift(self) -> None:
        with self.assertRaisesRegex(
            smoke_parity.ParityError, "expected three exact 32-token inputs"
        ):
            smoke_parity.validate_sequence_semantic_fixture(
                FrozenFixtureTokenizer(offset=1)
            )


if __name__ == "__main__":
    unittest.main()
