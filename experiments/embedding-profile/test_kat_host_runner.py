"""Unit tests for the host KAT runner's dependency-light core."""

import unittest

import numpy as np

import kat_host_runner as r


class CanonicalInt8Tests(unittest.TestCase):
    def test_maxabs_element_lands_on_127(self):
        q = r.canonical_int8(np.array([0.6, 0.8]))
        self.assertEqual(int(q[1]), 127)
        self.assertIn(127, q.tolist())

    def test_l2_normalizes_before_scaling(self):
        # [3, 4] normalizes to [0.6, 0.8]; same result as the unit vector.
        a = r.canonical_int8(np.array([3.0, 4.0]))
        b = r.canonical_int8(np.array([0.6, 0.8]))
        self.assertTrue(np.array_equal(a, b))

    def test_never_emits_minus_128(self):
        q = r.canonical_int8(np.array([-1e9, 1.0, 2.0]))
        self.assertGreaterEqual(int(q.min()), -127)
        self.assertLessEqual(int(q.max()), 127)

    def test_rounds_to_nearest_even_on_ties(self):
        # Craft ties: after l2/maxabs scaling values hit exactly x.5.
        # rint(0.5)=0 and rint(1.5)=2 (banker's rounding / RNE).
        self.assertEqual(int(np.rint(0.5)), 0)
        self.assertEqual(int(np.rint(1.5)), 2)
        q = r.canonical_int8(np.array([1.0, 3.0]))
        # 1/3 * (127/(3/3)) -> 127/3 = 42.333 -> 42; 3 -> 127
        self.assertEqual(int(q[0]), 42)
        self.assertEqual(int(q[1]), 127)

    def test_zero_vector_raises(self):
        with self.assertRaises(ValueError):
            r.canonical_int8(np.zeros(4))

    def test_length_preserved(self):
        vec = np.arange(1, 769, dtype=np.float64)
        self.assertEqual(r.canonical_int8(vec).shape, (768,))


class BucketPaddingTests(unittest.TestCase):
    def test_pads_right_with_mask(self):
        ids, mask = r.pad_to_bucket([5, 6, 7], 8)
        self.assertEqual(ids.shape, (1, 8))
        self.assertEqual(mask.shape, (1, 8))
        self.assertEqual(ids.tolist(), [[5, 6, 7, 0, 0, 0, 0, 0]])
        self.assertEqual(mask.tolist(), [[1, 1, 1, 0, 0, 0, 0, 0]])
        self.assertEqual(ids.dtype, np.int64)
        self.assertEqual(mask.dtype, np.int64)

    def test_exact_fit(self):
        ids, mask = r.pad_to_bucket([1, 2], 2)
        self.assertEqual(ids.tolist(), [[1, 2]])
        self.assertEqual(mask.tolist(), [[1, 1]])

    def test_overflow_raises(self):
        with self.assertRaises(ValueError):
            r.pad_to_bucket([1, 2, 3], 2)


class CaseHelpersTests(unittest.TestCase):
    def test_prefixes(self):
        self.assertEqual(r.prefixed_text("query", "x"), "task: search result | query: x")
        self.assertEqual(r.prefixed_text("document", "y"), "title: none | text: y")
        with self.assertRaises(ValueError):
            r.prefixed_text("passage", "z")

    def test_compare_vector_counts_diffs(self):
        case = {
            "vector_sha256": "00" * 32,
            "vector_hex": "00" * 768,
        }
        q = np.zeros(768, dtype=np.int8)
        q[0] = 1
        ok, diff = r.compare_vector(q, case)
        self.assertFalse(ok)
        self.assertEqual(diff, 1)


if __name__ == "__main__":
    unittest.main()
