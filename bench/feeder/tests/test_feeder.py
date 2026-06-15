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


class TestTxMixHelpers(unittest.TestCase):
    """Pure tx-type-mix helpers (issue #128 tx-type gap). Offline only."""

    def test_tx_type_name_known_and_unknown(self):
        self.assertEqual(feeder.tx_type_name(15), "cancel")
        self.assertEqual(feeder.tx_type_name(14), "create")
        self.assertEqual(feeder.tx_type_name(17), "modify")
        self.assertEqual(feeder.tx_type_name(21), "claim")
        # unknown types are carried through, never dropped/renamed silently
        self.assertEqual(feeder.tx_type_name(99), "type_99")

    def test_count_tx_types_and_skips_malformed(self):
        txs = [{"tx_type": 15}, {"tx_type": 15}, {"tx_type": 17},
               {"no_type": 1}, {"tx_type": None}]
        counts, skipped = feeder.count_tx_types(txs)
        self.assertEqual(counts, {15: 2, 17: 1})
        self.assertEqual(skipped, 2)

    def test_merge_tx_counts_accumulates(self):
        a = {15: 2, 17: 1}
        feeder.merge_tx_counts(a, {15: 3, 21: 4})
        self.assertEqual(a, {15: 5, 17: 1, 21: 4})

    def test_proportions_sorted_and_normalized(self):
        rows = feeder.tx_mix_proportions({15: 1, 17: 2, 21: 2})
        # descending count, then ascending tx_type for the 17/21 tie
        self.assertEqual([(tt, name, c) for tt, name, c, _ in rows],
                         [(17, "modify", 2), (21, "claim", 2),
                          (15, "cancel", 1)])
        self.assertAlmostEqual(sum(f for *_, f in rows), 1.0)

    def test_extract_block_txs_shape_tolerant(self):
        # bare list
        self.assertEqual(
            feeder._extract_block_txs([{"tx_type": 15}]), [{"tx_type": 15}])
        # common wrapper keys
        for key in ("txs", "transactions", "data"):
            self.assertEqual(
                feeder._extract_block_txs({key: [{"tx_type": 17}]}),
                [{"tx_type": 17}])
        # nested block object
        self.assertEqual(
            feeder._extract_block_txs({"block": {"txs": [{"tx_type": 21}]}}),
            [{"tx_type": 21}])
        with self.assertRaises(ValueError):
            feeder._extract_block_txs({"unexpected": 1})

    def test_render_tx_mix_includes_label_and_counts(self):
        text = feeder.render_tx_mix(
            {15: 3, 17: 1}, blocks=1, source="x", label="sample-size-1")
        self.assertIn("sample-size-1", text)
        self.assertIn("cancel", text)
        self.assertIn("75.00%", text)  # 3/4

    def test_sample_block_cli_offline(self):
        # The in-repo sample block is the only real tx_type data available
        # offline. It is honestly labeled sample-size-1, never "the mix".
        sample = Path(__file__).resolve().parents[3] / "bench" / \
            "bench_test.json"
        if not sample.exists():
            self.skipTest("sample block not present")
        rc, lines = run_cli(["tx-mix", "--sample-block", str(sample)])
        self.assertEqual(rc, 0)
        text = "\n".join(lines)
        self.assertIn("sample-size-1", text)
        self.assertIn("TX-TYPE MIX", text)


# ──────────────────────────────────────────────────────────────────────
# tx-mix region/egress config + rate-limit hardening (issue #128 follow-up).
# All offline: a fake `requests` module drives the 403 / 429 / Retry-After /
# transient-backoff paths with NO real network and NO real sleeps.
# ──────────────────────────────────────────────────────────────────────


class _FakeRequestException(Exception):
    pass


class _FakeConnectionError(_FakeRequestException):
    pass


class _FakeHTTPError(_FakeRequestException):
    def __init__(self, msg, response=None):
        super().__init__(msg)
        self.response = response


class _FakeExceptions:
    RequestException = _FakeRequestException
    ConnectionError = _FakeConnectionError
    HTTPError = _FakeHTTPError


class _FakeResponse:
    """Minimal stand-in for a requests.Response."""

    def __init__(self, status_code=200, json_data=None, headers=None):
        self.status_code = status_code
        self._json = json_data if json_data is not None else []
        self.headers = headers or {}

    def json(self):
        return self._json

    def raise_for_status(self):
        if self.status_code >= 400:
            raise _FakeHTTPError(f"HTTP {self.status_code}", response=self)


class _FakeRequests:
    """Scriptable fake `requests` module. `responses` is a list of
    _FakeResponse or Exception instances returned/raised in order; the last
    entry repeats if exhausted. Records every GET call for assertions."""

    exceptions = _FakeExceptions

    def __init__(self, responses):
        self._responses = list(responses)
        self.calls = []

    def get(self, url, params=None, headers=None, proxies=None, timeout=None):
        self.calls.append({"url": url, "params": params, "headers": headers,
                           "proxies": proxies, "timeout": timeout})
        item = (self._responses.pop(0) if len(self._responses) > 1
                else self._responses[0])
        if isinstance(item, Exception):
            raise item
        return item


class _RecordingSleep:
    """Captures sleep durations instead of waiting."""

    def __init__(self):
        self.calls = []

    def __call__(self, secs):
        self.calls.append(secs)


class TestTxMixConfigResolution(unittest.TestCase):
    """Region / egress / rate config resolution (pure, precedence rules)."""

    def test_base_url_precedence_flag_env_default(self):
        self.assertEqual(feeder.resolve_base_url("http://flag", {}),
                         "http://flag")
        self.assertEqual(
            feeder.resolve_base_url(
                None, {feeder.ENV_TXMIX_BASE_URL: "http://env"}),
            "http://env")
        self.assertEqual(feeder.resolve_base_url(None, {}),
                         feeder.TXMIX_BLOCK_URL)

    def test_proxy_precedence_and_none(self):
        self.assertEqual(feeder.resolve_proxies("http://p", {}),
                         {"http": "http://p", "https": "http://p"})
        self.assertEqual(
            feeder.resolve_proxies(
                None, {feeder.ENV_EGRESS_PROXY: "http://envp"}),
            {"http": "http://envp", "https": "http://envp"})
        # HTTPS_PROXY fallback
        self.assertEqual(
            feeder.resolve_proxies(None, {"HTTPS_PROXY": "http://hp"}),
            {"http": "http://hp", "https": "http://hp"})
        # nothing configured -> None (so requests uses its own defaults)
        self.assertIsNone(feeder.resolve_proxies(None, {}))

    def test_region_label_precedence(self):
        self.assertEqual(feeder.resolve_region("tokyo", {}), "tokyo")
        self.assertEqual(
            feeder.resolve_region(None, {feeder.ENV_REGION: "asia-ne1"}),
            "asia-ne1")
        self.assertIsNone(feeder.resolve_region(None, {}))

    def test_min_interval_from_rpm(self):
        self.assertAlmostEqual(feeder.min_interval_from_rpm(60), 1.0)
        self.assertAlmostEqual(feeder.min_interval_from_rpm(120), 0.5)
        # conservative default stays under the 90/min per-IP limit
        self.assertGreater(feeder.min_interval_from_rpm(feeder.TXMIX_MAX_RPM),
                           60.0 / 90.0)
        self.assertEqual(feeder.min_interval_from_rpm(0), 0.0)

    def test_backoff_is_exponential_and_capped(self):
        delays = [feeder._backoff_delay(i, initial=1.0, cap=60.0)
                  for i in range(8)]
        self.assertEqual(delays[:4], [1.0, 2.0, 4.0, 8.0])
        self.assertTrue(all(d <= 60.0 for d in delays))
        self.assertEqual(delays[-1], 60.0)  # saturates at the cap

    def test_parse_retry_after_capped_and_tolerant(self):
        self.assertEqual(feeder._parse_retry_after("5"), 5.0)
        self.assertEqual(feeder._parse_retry_after("2.5"), 2.5)
        self.assertIsNone(feeder._parse_retry_after(None))
        self.assertIsNone(feeder._parse_retry_after("Mon, 01 Jan 2030"))
        self.assertIsNone(feeder._parse_retry_after("-3"))
        # hostile/huge value is capped, never honored unboundedly
        self.assertEqual(
            feeder._parse_retry_after("99999", cap_s=120.0), 120.0)


class TestRateLimiter(unittest.TestCase):
    """Min-interval pacing — the client cannot accidentally hammer."""

    def test_first_call_no_wait_then_paces(self):
        clock = {"t": 0.0}
        sleeps = _RecordingSleep()

        def fake_clock():
            return clock["t"]

        rl = feeder.RateLimiter(1.0, sleep=sleeps, clock=fake_clock)
        rl.wait()                       # first call: no sleep
        self.assertEqual(sleeps.calls, [])
        clock["t"] = 0.25               # only 0.25s elapsed
        rl.wait()                       # must sleep the remaining 0.75s
        self.assertEqual(len(sleeps.calls), 1)
        self.assertAlmostEqual(sleeps.calls[0], 0.75)

    def test_zero_interval_never_sleeps(self):
        sleeps = _RecordingSleep()
        rl = feeder.RateLimiter(0.0, sleep=sleeps)
        rl.wait()
        rl.wait()
        self.assertEqual(sleeps.calls, [])


class TestTxMixHTTPHardening(unittest.TestCase):
    """403 / 429 / Retry-After / transient backoff — offline, no real waits."""

    def _http(self, responses, sleep=None, **kw):
        fake = _FakeRequests(responses)
        sleep = sleep or _RecordingSleep()
        limiter = feeder.RateLimiter(0.0, sleep=sleep)
        http = feeder.TxMixHTTP(fake, "http://test/blockTxs", limiter=limiter,
                                sleep=sleep, **kw)
        return http, fake, sleep

    def test_403_raises_immediately_not_retried(self):
        resp = _FakeResponse(status_code=403)
        http, fake, sleep = self._http([resp])
        with self.assertRaises(_FakeHTTPError) as cm:
            feeder._http_get_with_retry(http, http.base_url, {})
        self.assertEqual(cm.exception.response.status_code, 403)
        # geo-block is NOT transient: exactly one attempt, no backoff sleeps
        self.assertEqual(len(fake.calls), 1)
        self.assertEqual(sleep.calls, [])

    def test_429_honors_retry_after_then_succeeds(self):
        responses = [
            _FakeResponse(status_code=429, headers={"Retry-After": "7"}),
            _FakeResponse(status_code=200, json_data=[{"tx_type": 15}]),
        ]
        http, fake, sleep = self._http(responses)
        r = feeder._http_get_with_retry(http, http.base_url, {})
        self.assertEqual(r.status_code, 200)
        self.assertEqual(len(fake.calls), 2)
        # slept exactly the Retry-After value, not the backoff schedule
        self.assertIn(7.0, sleep.calls)

    def test_429_without_retry_after_uses_backoff(self):
        responses = [
            _FakeResponse(status_code=429),   # no Retry-After header
            _FakeResponse(status_code=200, json_data=[]),
        ]
        http, fake, sleep = self._http(responses)
        feeder._http_get_with_retry(http, http.base_url, {})
        # first retry backoff = initial (1.0s)
        self.assertIn(feeder.TXMIX_BACKOFF_INITIAL_S, sleep.calls)

    def test_transient_5xx_backs_off_then_succeeds(self):
        responses = [
            _FakeResponse(status_code=503),
            _FakeResponse(status_code=503),
            _FakeResponse(status_code=200, json_data=[{"tx_type": 17}]),
        ]
        http, fake, sleep = self._http(responses)
        r = feeder._http_get_with_retry(http, http.base_url, {})
        self.assertEqual(r.status_code, 200)
        self.assertEqual(len(fake.calls), 3)
        # exponential: 1.0 then 2.0
        self.assertEqual(sleep.calls[:2], [1.0, 2.0])

    def test_connection_error_retried_then_reraised_after_max(self):
        err = _FakeConnectionError("boom")
        http, fake, sleep = self._http([err], max_retries=2)
        with self.assertRaises(_FakeConnectionError):
            feeder._http_get_with_retry(http, http.base_url, {})
        # initial attempt + 2 retries = 3 calls; 2 backoff sleeps
        self.assertEqual(len(fake.calls), 3)
        self.assertEqual(len(sleep.calls), 2)

    def test_429_gives_up_after_max_retries(self):
        resp = _FakeResponse(status_code=429, headers={"Retry-After": "1"})
        http, fake, sleep = self._http([resp], max_retries=3)
        with self.assertRaises(_FakeHTTPError) as cm:
            feeder._http_get_with_retry(http, http.base_url, {})
        self.assertEqual(cm.exception.response.status_code, 429)
        self.assertEqual(len(fake.calls), 4)   # 1 + 3 retries

    def test_proxy_threaded_through_to_requests(self):
        resp = _FakeResponse(status_code=200, json_data=[])
        fake = _FakeRequests([resp])
        limiter = feeder.RateLimiter(0.0, sleep=_RecordingSleep())
        http = feeder.TxMixHTTP(fake, "http://test/blockTxs",
                                proxies={"https": "http://tokyo"},
                                limiter=limiter)
        feeder._http_get_with_retry(http, http.base_url, {})
        self.assertEqual(fake.calls[0]["proxies"], {"https": "http://tokyo"})


class TestTxMixGeoBlockGuidance(unittest.TestCase):
    """The 403 path must fail clearly with actionable Tokyo guidance and
    NOT produce a mix (measurement-citation norm)."""

    def test_guidance_text_is_actionable(self):
        msg = feeder.geo_block_guidance("http://x/blockTxs")
        for needle in ("403", "US", "Tokyo", "ap-northeast", "asia-northeast1",
                       "--base-url", "--proxy", "sample-block"):
            self.assertIn(needle, msg)
        self.assertIn("http://x/blockTxs", msg)

    def test_cmd_tx_mix_403_exits_2_with_guidance(self):
        resp = _FakeResponse(status_code=403)
        fake = _FakeRequests([resp])

        class Args:
            sample_block = None
            heights = [100, 100]      # avoid the tip-resolution call
            blocks = 1
            page_limit = 100
            base_url = "http://blocked/blockTxs"
            proxy = None
            region = "us-test"
            max_rpm = feeder.TXMIX_MAX_RPM
            max_retries = 2

        out = io.StringIO()
        err = io.StringIO()

        # Inject the fake `requests` for the lazy `import requests`.
        import sys as _sys
        from contextlib import redirect_stderr
        had_requests = "requests" in _sys.modules
        saved = _sys.modules.get("requests")
        _sys.modules["requests"] = fake
        try:
            with redirect_stdout(out), redirect_stderr(err):
                rc = feeder.cmd_tx_mix(Args(), out=out)
        finally:
            if had_requests:
                _sys.modules["requests"] = saved
            else:
                _sys.modules.pop("requests", None)

        self.assertEqual(rc, 2)                          # honest geo-block exit
        self.assertIn("Tokyo", err.getvalue())           # actionable guidance
        # measurement-citation norm: NO mix table emitted on a geo-block
        self.assertNotIn("TX-TYPE MIX", out.getvalue())


# ──────────────────────────────────────────────────────────────────────
# Native Pub/Sub publisher bridge (#211).
# Offline only: a fake publisher captures every publish; a deterministic
# clock drives pacing arithmetic with NO real sleeps and NO real network.
# These tests pin the bridge's three contracts from the issue:
#   - pacing fidelity (the publish loop honors ts_ms-derived schedule);
#   - honest backpressure (a publish failure RAISES, does not drop);
#   - wire payload byte-equivalence (the on-the-wire JSON matches what
#     `gcloud pubsub publish` of {height, tx_count} would have sent).
# ──────────────────────────────────────────────────────────────────────


class _FakeFuture:
    """Stand-in for the google-cloud-pubsub publish future.

    `outcome` is either None (success, sentinel-id returned) or an Exception
    (raised from `.result()` to drive the failure path). A `timeout`
    argument is accepted and recorded so tests assert backpressure honors
    the per-publish deadline.
    """

    def __init__(self, outcome=None):
        self.outcome = outcome
        self.timeouts_seen = []

    def result(self, timeout=None):
        self.timeouts_seen.append(timeout)
        if isinstance(self.outcome, BaseException):
            raise self.outcome
        return "message-id-stub"


class _FakePublisher:
    """Captures every publish call for assertions. `responses` is a list of
    _FakeFuture instances handed out in order; the last entry repeats when
    exhausted, matching the _FakeRequests style elsewhere in this file."""

    def __init__(self, responses=None):
        self._responses = list(responses) if responses else [_FakeFuture()]
        self.calls = []   # [{"topic": ..., "data": ...}]

    @staticmethod
    def topic_path(project, topic):
        return f"projects/{project}/topics/{topic}"

    def publish(self, topic, data, **attrs):
        self.calls.append({"topic": topic, "data": data, "attrs": attrs})
        return (self._responses.pop(0) if len(self._responses) > 1
                else self._responses[0])


class _Clock:
    """Deterministic clock + sleep that advances `t` by the slept amount."""

    def __init__(self, start=0.0):
        self.t = start
        self.sleeps = []

    def clock(self):
        return self.t

    def sleep(self, secs):
        self.sleeps.append(secs)
        self.t += secs


class TestBlockMessagePayload(unittest.TestCase):
    """Wire-payload byte-equivalence with `gcloud pubsub publish` (#211)."""

    def test_projects_to_height_and_tx_count_only(self):
        ev = {"ts_ms": 12345, "height": 1001, "tx_count": 500,
              "synthetic": False}
        self.assertEqual(feeder.block_message_payload(ev),
                         {"height": 1001, "tx_count": 500})

    def test_encode_is_compact_utf8_json(self):
        ev = {"ts_ms": 0, "height": 7, "tx_count": 1}
        # The Rust coordinator (`bench --mode coordinator`) parses
        # BlockMessage as JSON. The bridge MUST emit the same bytes that
        # `gcloud pubsub publish --message '{"height":7,"tx_count":1}'`
        # would have sent — compact, no whitespace, UTF-8.
        self.assertEqual(feeder.encode_block_message(ev),
                         b'{"height":7,"tx_count":1}')

    def test_coerces_to_int(self):
        # Defensive: replay's expanded events are already ints (P4), but
        # synthetic/null upstream can present floats. Pin int coercion.
        ev = {"ts_ms": 0, "height": 7.0, "tx_count": 12.0}
        p = feeder.block_message_payload(ev)
        self.assertEqual(p, {"height": 7, "tx_count": 12})
        self.assertIsInstance(p["height"], int)
        self.assertIsInstance(p["tx_count"], int)


class TestPacingReport(unittest.TestCase):
    """Drift bookkeeping (#211 requirement 2: tail pacing drift reported)."""

    def test_empty_report(self):
        r = feeder.PacingReport()
        self.assertEqual(r.summary(), {"published": 0})
        self.assertIn("no events", r.render())

    def test_records_signed_and_absolute_drift(self):
        r = feeder.PacingReport()
        r.record(scheduled_ms=100.0, actual_ms=110.0)   # +10  late
        r.record(scheduled_ms=200.0, actual_ms=195.0)   # -5   early
        r.record(scheduled_ms=300.0, actual_ms=320.0)   # +20  late
        s = r.summary()
        self.assertEqual(s["published"], 3)
        self.assertEqual(s["late_count"], 2)
        self.assertAlmostEqual(s["late_fraction"], 2 / 3)
        # abs drifts sorted: [5, 10, 20] -> max = 20
        self.assertEqual(s["abs_drift_ms_max"], 20.0)
        # mean signed = (10 - 5 + 20)/3
        self.assertAlmostEqual(s["signed_drift_ms_mean"], 25.0 / 3)

    def test_render_mentions_late_and_tail(self):
        r = feeder.PacingReport()
        for d in (0, 5, 10, 50, 100):
            r.record(0.0, float(d))
        text = r.render()
        self.assertIn("published 5", text)
        self.assertIn("p50", text)
        self.assertIn("p95", text)
        self.assertIn("p99", text)
        self.assertIn("max", text)


class TestPublisherBridgePublishOne(unittest.TestCase):
    """publish_one: sleeps to the scheduled instant, blocks on server
    accept, records drift, raises on failure (no silent drop). All with
    deterministic clock + sleep — NO real time elapses."""

    def _bridge(self, futures=None):
        pub = _FakePublisher(futures)
        c = _Clock()
        bridge = feeder.PublisherBridge(
            pub, "projects/p/topics/t", sleep=c.sleep, clock=c.clock)
        return bridge, pub, c

    def test_sleeps_to_scheduled_offset_then_publishes(self):
        bridge, pub, c = self._bridge()
        # Schedule says: publish at +250ms from base; current clock is base.
        bridge.publish_one(b'{"height":1,"tx_count":1}', 250.0, base_clock=0.0)
        # Slept ~0.25s, then published exactly once.
        self.assertEqual(len(c.sleeps), 1)
        self.assertAlmostEqual(c.sleeps[0], 0.25)
        self.assertEqual(len(pub.calls), 1)
        self.assertEqual(pub.calls[0]["topic"], "projects/p/topics/t")

    def test_does_not_sleep_when_behind_schedule(self):
        bridge, pub, c = self._bridge()
        c.t = 1.0  # already 1s past base
        bridge.publish_one(b'x', 250.0, base_clock=0.0)
        # No sleep — we're already late; just publish immediately.
        self.assertEqual(c.sleeps, [])
        self.assertEqual(len(pub.calls), 1)
        # Drift recorded as positive (late by 750ms).
        s = bridge.report.summary()
        self.assertEqual(s["late_count"], 1)
        self.assertAlmostEqual(s["abs_drift_ms_max"], 750.0)

    def test_drift_is_measured_after_server_accept(self):
        # Make the server's accept take wall time (publish-side latency).
        # The bridge measures actual_ms AFTER future.result() returns, so a
        # slow accept must show up as positive drift in the report.
        future = _FakeFuture()
        bridge, pub, c = self._bridge([future])
        # Patch the clock to advance during result() to simulate accept time.
        original_result = future.result

        def slow_result(timeout=None):
            c.t += 0.020   # 20 ms server accept
            return original_result(timeout=timeout)

        future.result = slow_result
        bridge.publish_one(b'x', 0.0, base_clock=0.0)
        s = bridge.report.summary()
        self.assertEqual(s["published"], 1)
        # Drift ~20ms positive (late by accept time, not by scheduling).
        self.assertGreaterEqual(s["abs_drift_ms_max"], 19.0)

    def test_publish_failure_raises_loudly_no_silent_drop(self):
        # Honest backpressure (#211 req 5): a server-side failure MUST
        # propagate; we never swallow + continue (that would corrupt the
        # benchmark's throughput claim by silently dropping a block).
        boom = RuntimeError("pubsub publish: server unavailable")
        bridge, pub, _ = self._bridge([_FakeFuture(outcome=boom)])
        with self.assertRaises(RuntimeError) as cm:
            bridge.publish_one(b'x', 0.0, base_clock=0.0)
        self.assertIn("unavailable", str(cm.exception))
        # And nothing was recorded as "published" — the report is honest.
        self.assertEqual(bridge.report.summary(), {"published": 0})

    def test_timeout_is_passed_to_future_result(self):
        # The per-publish deadline MUST reach future.result(timeout=...);
        # otherwise a hung publish would hang the benchmark indefinitely.
        future = _FakeFuture()
        bridge, pub, _ = self._bridge([future])
        bridge.timeout_s = 7.5
        bridge.publish_one(b'x', 0.0, base_clock=0.0)
        self.assertEqual(future.timeouts_seen, [7.5])


class TestPublishScheduledEvents(unittest.TestCase):
    """The bridge over a multi-event schedule (the realistic path)."""

    def _bridge(self, futures=None):
        pub = _FakePublisher(futures)
        c = _Clock()
        bridge = feeder.PublisherBridge(
            pub, "projects/p/topics/t", sleep=c.sleep, clock=c.clock)
        return bridge, pub, c

    def test_paces_to_each_events_ts_ms_offset(self):
        # Schedule mirrors what replay_schedule / synth_schedule produce:
        # ts_ms is the wall-clock target, sorted non-decreasing.
        events = [
            {"ts_ms": 1000, "height": 1, "tx_count": 100},
            {"ts_ms": 1250, "height": 2, "tx_count": 200},
            {"ts_ms": 1500, "height": 3, "tx_count": 300},
        ]
        bridge, pub, c = self._bridge()
        feeder.publish_scheduled_events(bridge, events, base_clock=0.0)
        # First event: offset 0 (its own ts_ms is the schedule zero) -> no sleep.
        # Second event: +250ms after first. Third: +250ms after second.
        # Cumulative slept ~= [0, 0.25, 0.25] (no sleep before the first).
        # The 3 publish calls hit the topic with the right payloads:
        wires = [c["data"] for c in pub.calls]
        self.assertEqual(wires, [
            b'{"height":1,"tx_count":100}',
            b'{"height":2,"tx_count":200}',
            b'{"height":3,"tx_count":300}',
        ])
        # Two sleeps of 0.25s each (not three; the first event runs immediately).
        self.assertEqual(len(c.sleeps), 2)
        for s in c.sleeps:
            self.assertAlmostEqual(s, 0.25)

    def test_failure_midstream_raises_with_partial_report(self):
        # Two ok publishes, then a server error on the third. The error
        # must propagate, and the bridge's report must reflect exactly the
        # two successes (no fabricated third).
        events = [
            {"ts_ms": 0, "height": 1, "tx_count": 1},
            {"ts_ms": 100, "height": 2, "tx_count": 1},
            {"ts_ms": 200, "height": 3, "tx_count": 1},
        ]
        bridge, pub, c = self._bridge([
            _FakeFuture(),
            _FakeFuture(),
            _FakeFuture(outcome=RuntimeError("publish failed")),
        ])
        with self.assertRaises(RuntimeError):
            feeder.publish_scheduled_events(bridge, events, base_clock=0.0)
        self.assertEqual(bridge.report.summary()["published"], 2)
        self.assertEqual(len(pub.calls), 3)   # the third call was attempted

    def test_progress_callback_called_per_success(self):
        events = [
            {"ts_ms": 0, "height": 1, "tx_count": 1},
            {"ts_ms": 0, "height": 2, "tx_count": 1},
        ]
        bridge, _, _ = self._bridge()
        seen = []
        feeder.publish_scheduled_events(
            bridge, events, base_clock=0.0, progress=seen.append)
        self.assertEqual([e["height"] for e in seen], [1, 2])


class TestPublishCLIWiring(unittest.TestCase):
    """The CLI surface: --publish-to validation + dry-run mutex."""

    def test_publish_to_requires_project(self):
        rc, _ = run_cli(["synth-peak", "--rate", "11", "--duration", "1s",
                         "--publish-to", "dispatch-topic"])
        self.assertNotEqual(rc, 0)   # missing --project -> non-zero exit

    def test_publish_to_rejects_dry_run(self):
        rc, _ = run_cli(["synth-peak", "--rate", "11", "--duration", "1s",
                         "--publish-to", "dispatch", "--project", "p",
                         "--dry-run"])
        self.assertNotEqual(rc, 0)

    def test_replay_publish_to_requires_project(self):
        rc, _ = run_cli(["replay", "--in", str(FIXTURE), "--target-rate",
                         "11", "--publish-to", "dispatch-topic"])
        self.assertNotEqual(rc, 0)

    def test_replay_publish_to_rejects_dry_run(self):
        rc, _ = run_cli(["replay", "--in", str(FIXTURE), "--target-rate",
                         "11", "--publish-to", "d", "--project", "p",
                         "--dry-run"])
        self.assertNotEqual(rc, 0)


class TestBuildPublisherBridgeWithStub(unittest.TestCase):
    """build_publisher_bridge wiring with an injected fake pubsub_v1 module
    — exercises the real glue path without google-cloud-pubsub installed."""

    def test_constructs_with_injected_pubsub_v1(self):
        class _StubPubSubV1:
            class PublisherClient(_FakePublisher):
                def __init__(self):
                    super().__init__()
        bridge = feeder.build_publisher_bridge(
            "my-proj", "dispatch", pubsub_v1=_StubPubSubV1)
        self.assertEqual(bridge.topic_path,
                         "projects/my-proj/topics/dispatch")
        self.assertIsInstance(bridge, feeder.PublisherBridge)


if __name__ == "__main__":
    unittest.main()
