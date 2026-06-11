"""Offline test suite for feeder.py (issue #48).

Pure arithmetic on --dry-run schedules against the committed fixture
bench/feeder/fixtures/trace_excerpt.jsonl (201 lines, heights
260,138,266-260,138,493, 9 jumps incl. one Delta=9, 40 nulls — measured
properties pinned in bench/trace-format.md §8.2).

CRITICAL invariants: no network, no sleeps, suite runs well under 1 min.
"""

import io
import json
import math
import statistics
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import feeder  # noqa: E402

FIXTURE = Path(__file__).resolve().parents[1] / "fixtures" / \
    "trace_excerpt.jsonl"

# Fixture ground truth from bench/trace-format.md §8.2.
FIX_LINES = 201
FIX_SKIPPED = 27          # heights skipped across 9 jumps (deltas 2,9,4x5,3,2)
FIX_NULLS = 40
FIX_MEAN_NONNULL = 367.55
FIX_JUMP_FROM, FIX_JUMP_TO = 260_138_395, 260_138_404   # the Delta=9 jump


def spearman(x, y):
    """Spearman rank correlation with average ranks for ties (no scipy)."""
    def ranks(v):
        order = sorted(range(len(v)), key=lambda i: v[i])
        r = [0.0] * len(v)
        i = 0
        while i < len(order):
            j = i
            while j + 1 < len(order) and v[order[j + 1]] == v[order[i]]:
                j += 1
            avg = (i + j) / 2 + 1
            for k in range(i, j + 1):
                r[order[k]] = avg
            i = j + 1
        return r
    rx, ry = ranks(x), ranks(y)
    mx, my = sum(rx) / len(rx), sum(ry) / len(ry)
    num = sum((a - mx) * (b - my) for a, b in zip(rx, ry))
    den = math.sqrt(sum((a - mx) ** 2 for a in rx)
                    * sum((b - my) ** 2 for b in ry))
    return num / den


def load_fixture():
    with open(FIXTURE) as f:
        header, events, gaps, no_expand = feeder.load_trace(f)
    return header, events, gaps, no_expand


def run_cli(argv):
    """Run feeder.main() in-process, return (exit_code, stdout_lines)."""
    buf = io.StringIO()
    with redirect_stdout(buf):
        rc = feeder.main(argv)
    return rc, buf.getvalue().splitlines()


def parse_stream(lines):
    """Split an emitted stream into (header_obj, block_events). Validates
    every line parses and is one of the three spec line types."""
    assert lines, "empty stream"
    header = json.loads(lines[0])
    assert "provenance" in header, "first line must be a provenance header"
    events, gap_markers = [], []
    for raw in lines[1:]:
        obj = json.loads(raw)
        if obj.get("gap") is True:
            gap_markers.append(obj)
        else:
            assert "height" in obj, f"unrecognized line: {raw[:80]}"
            events.append(obj)
    return header, events, gap_markers


class TestFixtureSanity(unittest.TestCase):
    def test_fixture_matches_spec_pinned_properties(self):
        header, events, gaps, _ = load_fixture()
        self.assertIsNone(header)               # pre-spec exemption path
        self.assertEqual(gaps, 0)
        self.assertEqual(len(events), FIX_LINES)
        self.assertEqual(events[0]["height"], 260_138_266)
        self.assertEqual(events[-1]["height"], 260_138_493)
        self.assertEqual(
            sum(1 for e in events if e["tx_count"] is None), FIX_NULLS)
        feeder.validate_events(events)          # must not raise


class TestExpansion(unittest.TestCase):
    """Test 1 — P2 height-jump expansion on the fixture's 9 jumps."""

    def test_jump_count_and_total_expansion(self):
        _, events, _, no_expand = load_fixture()
        deltas = [b["height"] - a["height"]
                  for a, b in zip(events, events[1:])]
        jumps = [d for d in deltas if d > 1]
        self.assertEqual(len(jumps), 9)
        self.assertEqual(sorted(jumps), [2, 2, 3, 4, 4, 4, 4, 4, 9])
        expanded = feeder.expand_and_fill(events, no_expand)
        self.assertEqual(len(expanded), FIX_LINES + FIX_SKIPPED)  # 228
        # Heights are now gapless and strictly increasing.
        hs = [e["height"] for e in expanded]
        self.assertEqual(hs, list(range(hs[0], hs[0] + len(hs))))

    def test_delta9_jump_yields_9_events_at_one_ts(self):
        _, events, _, no_expand = load_fixture()
        observed_ts = next(e["ts_ms"] for e in events
                           if e["height"] == FIX_JUMP_TO)
        expanded = feeder.expand_and_fill(events, no_expand)
        jump_events = [e for e in expanded
                       if FIX_JUMP_FROM < e["height"] <= FIX_JUMP_TO]
        self.assertEqual(len(jump_events), 9)
        self.assertEqual({e["ts_ms"] for e in jump_events}, {observed_ts})
        # Intermediates are synthetic; the observed final height is not.
        self.assertTrue(all(e["synthetic"] for e in jump_events[:-1]))
        self.assertFalse(jump_events[-1]["synthetic"])

    def test_gap_marker_suppresses_expansion(self):
        """Spec §6.2 exception: jump right after a gap marker not expanded."""
        lines = [
            '{"ts_ms": 1000, "height": 10, "tx_count": 100}',
            '{"gap": true, "ts_ms": 1500, "reason": "ws_disconnect"}',
            '{"ts_ms": 2000, "height": 15, "tx_count": 200}',
            '{"ts_ms": 2100, "height": 18, "tx_count": null}',
        ]
        _, events, gaps, no_expand = feeder.load_trace(lines)
        self.assertEqual(gaps, 1)
        self.assertEqual(no_expand, {15})
        expanded = feeder.expand_and_fill(events, no_expand)
        # 10->15 NOT expanded (post-gap); 15->18 expanded (+2 synthetics).
        self.assertEqual([e["height"] for e in expanded], [10, 15, 16, 17, 18])


class TestNullFill(unittest.TestCase):
    """Test 2 — P1 mean-of-non-null fill, P4 ints."""

    def test_fill_value_is_mean_of_nonnull(self):
        _, events, _, _ = load_fixture()
        fill = feeder.mean_fill_value(events)
        manual = statistics.mean(
            e["tx_count"] for e in events if e["tx_count"] is not None)
        self.assertAlmostEqual(fill, manual, places=9)
        self.assertAlmostEqual(fill, FIX_MEAN_NONNULL, places=2)  # spec §8.2

    def test_filled_events_are_ints(self):
        _, events, _, no_expand = load_fixture()
        fill = feeder.mean_fill_value(events)
        expanded = feeder.expand_and_fill(events, no_expand)
        for e in expanded:
            self.assertIsInstance(e["tx_count"], int)
            self.assertNotIsInstance(e["tx_count"], bool)
        filled = [e for e in expanded if e["synthetic"]]
        self.assertTrue(all(e["tx_count"] == int(round(fill))
                            for e in filled))
        # Non-null observed counts pass through unchanged.
        observed = {e["height"]: e["tx_count"] for e in events
                    if e["tx_count"] is not None}
        for e in expanded:
            if e["height"] in observed:
                self.assertEqual(e["tx_count"], observed[e["height"]])


class TestTargetRate(unittest.TestCase):
    """Test 3 — replay --target-rate 2213 --dry-run schedule arithmetic."""

    @classmethod
    def setUpClass(cls):
        rc, lines = run_cli(["replay", "--in", str(FIXTURE),
                             "--target-rate", "2213", "--dry-run"])
        assert rc == 0
        cls.header, cls.events, _ = parse_stream(lines)

    def test_aggregate_rate_within_2pct(self):
        total_tx = sum(e["tx_count"] for e in self.events)
        span_s = (self.events[-1]["ts_ms"] - self.events[0]["ts_ms"]) / 1000.0
        agg = total_tx / span_s
        err_pct = abs(agg - 2213.0) / 2213.0 * 100.0
        self.assertLessEqual(
            err_pct, 2.0,
            f"aggregate {agg:.1f} tx/s deviates {err_pct:.3f}% from 2213")
        # Stash for human-readable reporting.
        print(f"\n[measured] target-rate aggregate={agg:.1f} tx/s "
              f"err={err_pct:.4f}%", file=sys.stderr)

    def test_rank_correlation_of_gaps(self):
        _, src_events, _, no_expand = load_fixture()
        expanded = feeder.expand_and_fill(src_events, no_expand)
        src_gaps = [b["ts_ms"] - a["ts_ms"]
                    for a, b in zip(expanded, expanded[1:])]
        out_gaps = [b["ts_ms"] - a["ts_ms"]
                    for a, b in zip(self.events, self.events[1:])]
        self.assertEqual(len(src_gaps), len(out_gaps))
        rho = spearman(src_gaps, out_gaps)
        self.assertGreaterEqual(rho, 0.99,
                                f"Spearman rho {rho:.6f} < 0.99")
        print(f"\n[measured] gap rank correlation rho={rho:.6f}",
              file=sys.stderr)

    def test_provenance_header(self):
        prov = self.header["provenance"]
        self.assertEqual(prov["generator"], "replay --target-rate")
        self.assertEqual(prov["params"]["target_rate"], 2213.0)
        self.assertEqual(prov["params"]["fill"], "mean")
        self.assertEqual(prov["params"]["fill_value"],
                         int(round(FIX_MEAN_NONNULL)))
        self.assertEqual(prov["source_trace"], str(FIXTURE))
        self.assertIn("generated_at", prov)


class TestSpeedReplay(unittest.TestCase):
    """Test 4 — replay --speed 10 --dry-run: deterministic, duration/10."""

    def test_deterministic_byte_identical(self):
        argv = ["replay", "--in", str(FIXTURE), "--speed", "10", "--dry-run"]
        rc1, lines1 = run_cli(argv)
        rc2, lines2 = run_cli(argv)
        self.assertEqual((rc1, rc2), (0, 0))
        # Schedule lines (everything after the provenance header, whose
        # generated_at is a wall-clock timestamp) must be byte-identical.
        self.assertEqual(lines1[1:], lines2[1:])

    def test_duration_scales_by_inverse_speed(self):
        _, src_events, _, _ = load_fixture()
        src_span_ms = src_events[-1]["ts_ms"] - src_events[0]["ts_ms"]
        rc, lines = run_cli(["replay", "--in", str(FIXTURE),
                             "--speed", "10", "--dry-run"])
        self.assertEqual(rc, 0)
        _, events, _ = parse_stream(lines)
        out_span_ms = events[-1]["ts_ms"] - events[0]["ts_ms"]
        self.assertAlmostEqual(out_span_ms, src_span_ms / 10.0,
                               delta=2)  # int-ms rounding at each end


class TestSynthPeak(unittest.TestCase):
    """Test 5 — synth-peak --rate 2213 --duration 60s --dry-run."""

    @classmethod
    def setUpClass(cls):
        rc, lines = run_cli(["synth-peak", "--rate", "2213",
                             "--duration", "60s", "--dry-run"])
        assert rc == 0
        cls.header, cls.events, _ = parse_stream(lines)

    def test_uniform_cadence(self):
        expected_ms = 500.0 / 2213.0 * 1000.0    # 225.9376...
        gaps = [b["ts_ms"] - a["ts_ms"]
                for a, b in zip(self.events, self.events[1:])]
        for g in gaps:
            self.assertLessEqual(abs(g - expected_ms), 1.0,
                                 f"gap {g} ms vs cadence {expected_ms:.2f}")

    def test_aggregate_within_2pct(self):
        total_tx = sum(e["tx_count"] for e in self.events)
        agg = total_tx / 60.0                    # rate over the window
        err_pct = abs(agg - 2213.0) / 2213.0 * 100.0
        self.assertLessEqual(err_pct, 2.0)
        self.assertTrue(all(e["tx_count"] == 500 for e in self.events))

    def test_provenance_header(self):
        prov = self.header["provenance"]
        self.assertEqual(prov["generator"], "synth-peak")
        self.assertEqual(prov["params"]["peak_rate"], 2213.0)
        self.assertEqual(prov["params"]["duration_s"], 60.0)


class TestSchemaAndMonotonicity(unittest.TestCase):
    """Test 6 — every emitted stream parses + spec §5 monotonicity."""

    STREAMS = [
        ["replay", "--in", str(FIXTURE), "--target-rate", "2213",
         "--dry-run"],
        ["replay", "--in", str(FIXTURE), "--speed", "10", "--dry-run"],
        ["replay", "--in", str(FIXTURE), "--speed", "1", "--loop",
         "--duration", "40s", "--dry-run"],
        ["replay", "--in", str(FIXTURE), "--speed", "2",
         "--duration", "5s", "--dry-run"],
        ["synth-peak", "--rate", "2213", "--duration", "60s", "--dry-run"],
        ["synth-peak", "--rate", "9000", "--duration", "15m", "--dry-run"],
    ]

    def test_all_streams_valid(self):
        for argv in self.STREAMS:
            with self.subTest(argv=" ".join(argv)):
                rc, lines = run_cli(argv)
                self.assertEqual(rc, 0)
                header, events, gap_markers = parse_stream(lines)
                self.assertEqual(gap_markers, [])
                self.assertIn("generator", header["provenance"])
                self.assertIn("params", header["provenance"])
                self.assertIn("generated_at", header["provenance"])
                prev_ts, prev_h = None, None
                for e in events:
                    self.assertIsInstance(e["ts_ms"], int)
                    self.assertIsInstance(e["height"], int)
                    self.assertTrue(e["tx_count"] is None
                                    or isinstance(e["tx_count"], int),
                                    "tx_count must be int|null (P4)")
                    if prev_ts is not None:
                        self.assertGreaterEqual(e["ts_ms"], prev_ts,
                                                "ts_ms must be non-decreasing")
                        self.assertGreater(e["height"], prev_h,
                                           "height must strictly increase")
                    prev_ts, prev_h = e["ts_ms"], e["height"]

    def test_loop_duration_bounds_and_seam(self):
        rc, lines = run_cli(["replay", "--in", str(FIXTURE), "--speed", "1",
                             "--loop", "--duration", "40s", "--dry-run"])
        self.assertEqual(rc, 0)
        header, events, _ = parse_stream(lines)
        span_ms = events[-1]["ts_ms"] - events[0]["ts_ms"]
        self.assertLessEqual(span_ms, 40_000)
        # Fixture spans 19.64s -> 40s at speed 1 must cross the seam at
        # least once (more events than one expanded pass).
        self.assertGreater(len(events), FIX_LINES + FIX_SKIPPED)
        self.assertIn("seam_ms", header["provenance"]["params"])


class TestArgValidation(unittest.TestCase):
    def test_nonpositive_rates_rejected(self):
        for argv in (["replay", "--in", str(FIXTURE), "--speed", "0",
                      "--dry-run"],
                     ["replay", "--in", str(FIXTURE), "--target-rate", "-5",
                      "--dry-run"],
                     ["synth-peak", "--rate", "0", "--duration", "60s",
                      "--dry-run"]):
            with self.subTest(argv=" ".join(argv)):
                with self.assertRaises(SystemExit) as cm:
                    feeder.main(argv)
                self.assertEqual(cm.exception.code, 2)  # argparse usage error


class TestDurationParsing(unittest.TestCase):
    def test_forms(self):
        self.assertEqual(feeder.parse_duration("15m"), 900.0)
        self.assertEqual(feeder.parse_duration("900s"), 900.0)
        self.assertEqual(feeder.parse_duration("900"), 900.0)
        self.assertEqual(feeder.parse_duration("1h"), 3600.0)


if __name__ == "__main__":
    unittest.main()
