"""Offline unit tests for bench/feeder/size_distributions.py (issue #220).

Pure arithmetic + deterministic RNG; no network, no sleeps. Pins the
shared 7-band partition + #212 documented mainnet weights so any future
refactor or accidental edit surfaces immediately.
"""

import json
import random
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import size_distributions as sd  # noqa: E402


class TestBandsPartition(unittest.TestCase):
    """The 7 bands MUST partition 1..500 exactly (every tx_count belongs
    to exactly one band)."""

    def test_bands_partition_is_exhaustive_and_disjoint(self):
        for tx in range(1, sd.BLOCK_TX_CAP + 1):
            matches = [name for name, lo, hi, _r in sd.BANDS
                       if lo <= tx <= hi]
            self.assertEqual(
                len(matches), 1,
                f"tx={tx} matched bands {matches} (must match exactly one)")

    def test_band_names_in_canonical_order(self):
        # Order matches analyze.py:115-122 + gen_corpus.py:_BANDS so
        # provenance / histogram keys are stable across the codebase.
        self.assertEqual(
            sd.BAND_NAMES,
            ["eq_1", "2_49", "50_99", "100_249",
             "250_399", "400_499", "eq_500"])

    def test_representatives_within_chain_cap(self):
        for name, lo, hi, rep in sd.BANDS:
            self.assertTrue(
                1 <= rep <= sd.BLOCK_TX_CAP,
                f"band {name} representative {rep} outside 1..500")
            self.assertTrue(
                lo <= rep <= hi,
                f"band {name} representative {rep} outside its own range "
                f"[{lo}, {hi}]")


class TestMainnetCounts(unittest.TestCase):
    """The #212 documented weights MUST be reproduced verbatim."""

    def test_mainnet_counts_match_212_documented_weights(self):
        # Verbatim PR #163 body / trace-format.md §8.1 (issue #212).
        expected = {
            "eq_1": 660, "2_49": 122, "50_99": 124, "100_249": 301,
            "250_399": 219, "400_499": 136, "eq_500": 4348,
        }
        self.assertEqual(sd.MAINNET_BIMODAL_COUNTS, expected)

    def test_mainnet_total_and_cap_fraction(self):
        total = sum(sd.MAINNET_BIMODAL_COUNTS.values())
        self.assertEqual(total, 5910)
        # Bimodal headline: ~73.6% of blocks pinned to the cap.
        frac = sd.MAINNET_BIMODAL_COUNTS["eq_500"] / total
        self.assertAlmostEqual(frac, 0.7357, places=3)


class TestSizeSampler(unittest.TestCase):
    def test_sampler_is_deterministic_per_seed(self):
        # Two samplers with the same seed -> identical draws.
        s1 = sd.bimodal_mainnet_sampler(random.Random(220))
        s2 = sd.bimodal_mainnet_sampler(random.Random(220))
        seq1 = [s1.sample() for _ in range(1000)]
        seq2 = [s2.sample() for _ in range(1000)]
        self.assertEqual(seq1, seq2)

    def test_different_seeds_diverge(self):
        s1 = sd.bimodal_mainnet_sampler(random.Random(220))
        s2 = sd.bimodal_mainnet_sampler(random.Random(221))
        seq1 = [s1.sample() for _ in range(50)]
        seq2 = [s2.sample() for _ in range(50)]
        self.assertNotEqual(seq1, seq2)

    def test_sample_always_returns_a_representative(self):
        s = sd.bimodal_mainnet_sampler(random.Random(0))
        reps = {rep for _, _, _, rep in sd.BANDS}
        for _ in range(500):
            v = s.sample()
            self.assertIn(v, reps)
            self.assertIsInstance(v, int)

    def test_band_weights_records_canonical_keys(self):
        s = sd.bimodal_mainnet_sampler(random.Random(0))
        bw = s.band_weights()
        self.assertEqual(set(bw.keys()), set(sd.BAND_NAMES))
        # Should match the source table exactly.
        for name in sd.BAND_NAMES:
            self.assertEqual(bw[name], sd.MAINNET_BIMODAL_COUNTS[name])

    def test_rejects_bad_construction(self):
        with self.assertRaises(ValueError):
            sd.SizeSampler(name="x", weights=[1, 1, 1, 1, 1, 1],  # too short
                           representatives=[1, 25, 75, 175, 325, 450, 500],
                           rng=random.Random(0))
        with self.assertRaises(ValueError):
            sd.SizeSampler(name="x", weights=[0] * 7,           # zero total
                           representatives=[1, 25, 75, 175, 325, 450, 500],
                           rng=random.Random(0))
        with self.assertRaises(ValueError):
            sd.SizeSampler(name="x", weights=[1] * 7,
                           representatives=[1, 25, 75, 175, 325, 450, 501],  # >cap
                           rng=random.Random(0))
        with self.assertRaises(ValueError):
            sd.SizeSampler(name="x", weights=[1] * 7,
                           representatives=[1, 25, 75, 175, 325, 450, 500],
                           rng=None)


class TestSamplerFromFile(unittest.TestCase):
    def _write(self, doc):
        f = tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False)
        json.dump(doc, f)
        f.close()
        return f.name

    def test_sampler_from_file_roundtrip(self):
        doc = {
            "name": "test-mix",
            "bands": [
                {"lo": 1, "hi": 1, "weight": 1},
                {"lo": 2, "hi": 49, "weight": 0},
                {"lo": 50, "hi": 99, "weight": 0},
                {"lo": 100, "hi": 249, "weight": 0},
                {"lo": 250, "hi": 399, "weight": 0},
                {"lo": 400, "hi": 499, "weight": 0},
                {"lo": 500, "hi": 500, "weight": 1},
            ],
        }
        path = self._write(doc)
        s = sd.sampler_from_file(path, random.Random(0))
        # 50/50 between 1 and 500 (degenerate two-point dist).
        seen = {s.sample() for _ in range(200)}
        self.assertEqual(seen, {1, 500})
        self.assertEqual(s.name, "test-mix")

    def test_sampler_from_file_missing_band_rejected(self):
        doc = {
            "name": "missing",
            "bands": [
                {"lo": 1, "hi": 1, "weight": 1},
                # missing the other 6 bands
            ],
        }
        path = self._write(doc)
        with self.assertRaises(SystemExit) as cm:
            sd.sampler_from_file(path, random.Random(0))
        self.assertIn("missing bands", str(cm.exception))

    def test_sampler_from_file_unknown_band_rejected(self):
        # (lo=2, hi=100) does not match any canonical band.
        doc = {
            "name": "bad",
            "bands": [
                {"lo": 1, "hi": 1, "weight": 1},
                {"lo": 2, "hi": 100, "weight": 1},
                {"lo": 50, "hi": 99, "weight": 0},
                {"lo": 100, "hi": 249, "weight": 0},
                {"lo": 250, "hi": 399, "weight": 0},
                {"lo": 400, "hi": 499, "weight": 0},
                {"lo": 500, "hi": 500, "weight": 1},
            ],
        }
        path = self._write(doc)
        with self.assertRaises(SystemExit) as cm:
            sd.sampler_from_file(path, random.Random(0))
        self.assertIn("does not match any canonical band",
                      str(cm.exception))

    def test_sampler_from_file_missing_file_rejected(self):
        with self.assertRaises(SystemExit) as cm:
            sd.sampler_from_file(
                "/nonexistent-path-issue-220-XYZ", random.Random(0))
        self.assertIn("cannot read", str(cm.exception))

    def test_sampler_from_file_malformed_json_rejected(self):
        f = tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False)
        f.write("{not valid json")
        f.close()
        with self.assertRaises(SystemExit) as cm:
            sd.sampler_from_file(f.name, random.Random(0))
        self.assertIn("not valid JSON", str(cm.exception))


class TestRealizedHistogram(unittest.TestCase):
    def test_realized_histogram_band_assignment(self):
        # Known tx_counts -> assert per-band counts.
        h = sd.realized_histogram([1, 25, 75, 500, 500, 500])
        self.assertEqual(h["eq_1"], 1)
        self.assertEqual(h["2_49"], 1)
        self.assertEqual(h["50_99"], 1)
        self.assertEqual(h["100_249"], 0)
        self.assertEqual(h["250_399"], 0)
        self.assertEqual(h["400_499"], 0)
        self.assertEqual(h["eq_500"], 3)

    def test_realized_histogram_keys_always_present(self):
        # Empty input -> dict still has all 7 canonical keys at zero.
        h = sd.realized_histogram([])
        self.assertEqual(set(h.keys()), set(sd.BAND_NAMES))
        self.assertTrue(all(v == 0 for v in h.values()))

    def test_realized_histogram_rejects_out_of_range(self):
        with self.assertRaises(ValueError):
            sd.realized_histogram([0])
        with self.assertRaises(ValueError):
            sd.realized_histogram([501])


if __name__ == "__main__":
    unittest.main()
