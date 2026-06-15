"""Tests for scripts/lag-slo-verdict.py (issue #215).

Validates the per-block end-to-end lag parser against the committed
synthetic fixture (scripts/tests/fixtures/coordinator-lag-sample.jsonl)
whose ground truth is pinned in its header comment.

Dual-mode, mirroring scripts/trace-distribution/tests/test_analyze.py:
  - python3 -m unittest discover -s scripts/tests -p 'test_lag_slo_verdict.py'
  - pytest scripts/tests/test_lag_slo_verdict.py
  - python3 scripts/tests/test_lag_slo_verdict.py   (no pytest/unittest CLI)

The script filename has a hyphen, so it is imported via importlib from an
explicit path rather than `import lag_slo_verdict`.
"""

from __future__ import annotations

import importlib.util
import os
import sys
import unittest

_TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
_SCRIPTS_DIR = os.path.abspath(os.path.join(_TESTS_DIR, ".."))
_SCRIPT_PATH = os.path.join(_SCRIPTS_DIR, "lag-slo-verdict.py")
_FIXTURE = os.path.join(_TESTS_DIR, "fixtures", "coordinator-lag-sample.jsonl")


def _load_module():
    spec = importlib.util.spec_from_file_location("lag_slo_verdict", _SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    # Register before exec so @dataclass introspection (which looks the
    # module up in sys.modules by __module__) works for a hyphen-named file.
    sys.modules["lag_slo_verdict"] = mod
    spec.loader.exec_module(mod)
    return mod


lsv = _load_module()

_DEFAULT_THRESHOLDS = {"lag_p50_s": 20.0, "lag_p99_s": 40.0, "min_blocks_s": 5.0}


def _fixture_text() -> str:
    with open(_FIXTURE, encoding="utf-8") as fh:
        return fh.read()


def _report(text: str | None = None, **kwargs):
    return lsv.build_report(
        text if text is not None else _fixture_text(),
        dict(_DEFAULT_THRESHOLDS),
        **kwargs,
    )


class TestParsing(unittest.TestCase):
    def test_prefixed_parsing(self):
        text = 'BENCH_EVENT {"event":"chunk_proven","height":1,"lag_ms":5}\n'
        events = lsv._parse_events(text)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["height"], 1)

    def test_bare_json_parsing(self):
        # Prefix-stripped lines (entrypoint.sh / s-calibrate.sh strip it).
        text = '{"event":"chunk_proven","height":2,"lag_ms":7}\n'
        events = lsv._parse_events(text)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["height"], 2)

    def test_bare_json_without_event_key_ignored(self):
        # A bare JSON dict with no "event" key is NOT a bench event.
        text = '{"foo":1}\n'
        self.assertEqual(lsv._parse_events(text), [])

    def test_malformed_lines_skipped(self):
        text = (
            "banner line\n"
            "BENCH_EVENT not-json\n"
            'BENCH_EVENT {"event":"chunk_proven","height":3,"lag_ms":9}\n'
        )
        events = lsv._parse_events(text)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["height"], 3)

    def test_prefixed_and_bare_mixed(self):
        text = (
            'BENCH_EVENT {"event":"chunk_proven","height":1,"lag_ms":5}\n'
            '{"event":"coordinator_fold","height":1,"merge_source":"measured",'
            '"l4_source":"measured","merge_ms":1,"l4_ms":2}\n'
        )
        events = lsv._parse_events(text)
        self.assertEqual(len(events), 2)

    def test_bench_metric_parsing(self):
        text = (
            "BENCH_METRIC fold_storage height=100 level=1 mount=true "
            "storage_download_ms=120 payload_bytes=421888 (issue #206)\n"
        )
        metrics = lsv._parse_metrics(text)
        self.assertIn("fold_storage", metrics)
        rec = metrics["fold_storage"][0]
        self.assertEqual(rec["height"], 100)
        self.assertEqual(rec["storage_download_ms"], 120)
        self.assertEqual(rec["mount"], "true")


class TestLagComputation(unittest.TestCase):
    def test_per_block_lag_sum(self):
        rep = _report()
        by_h = {b["height"]: b for b in rep["blocks"]}
        # lag = gather + merge + l4, verified per block.
        self.assertEqual(by_h[100]["lag_ms"], 8000 + 3000 + 4000)
        self.assertEqual(by_h[105]["lag_ms"], 11000 + 3400 + 4600)
        for b in rep["blocks"]:
            self.assertEqual(
                b["lag_ms"],
                b["gather_wall_ms"] + b["merge_ms"] + b["l4_ms"],
            )

    def test_height_join_correctness(self):
        rep = _report()
        by_h = {b["height"]: b for b in rep["blocks"]}
        # h100's fold (merge 3000/l4 4000) must join h100's gather (8000),
        # not some other block's.
        self.assertEqual(by_h[100]["merge_ms"], 3000)
        self.assertEqual(by_h[100]["l4_ms"], 4000)
        self.assertEqual(by_h[101]["merge_ms"], 3500)
        self.assertEqual(by_h[101]["l4_ms"], 4500)

    def test_fold_scaling_fields_surfaced(self):
        rep = _report()
        b = rep["blocks"][0]
        self.assertEqual(b["depth"], 2)
        self.assertEqual(b["merges"], 3)
        self.assertEqual(b["leaves"], 4)


class TestGatherProvenance(unittest.TestCase):
    """Issue #222: GATHER must be the coordinator's REAL measured wall, not
    the slowest-chunk-lag proxy. The proxy is never the silent default."""

    def test_fixture_uses_measured_gather_not_proxy(self):
        # Every measured block in the fixture carries a block_complete summary
        # with block_wall_ms, so gather_source must be "measured" -- NOT the
        # chunk_proven.lag_ms proxy.
        rep = _report()
        for b in rep["blocks"]:
            self.assertEqual(
                b["gather_source"],
                "measured",
                f"block {b['height']} must use the measured wall, not the proxy",
            )
        self.assertEqual(rep["proxy_gather_count"], 0)
        self.assertEqual(rep["measured_gather_count"], 6)
        self.assertTrue(rep["gather_fully_measured"])
        self.assertEqual(rep["proxy_gather_heights"], [])

    def test_gather_value_is_measured_wall_not_chunk_lag(self):
        # The decisive assertion: the GATHER term equals the coordinator's
        # block_wall_ms (8000 for h100), NOT the slower-to-detect proxy value
        # max(chunk_proven.lag_ms) which is 7600 for h100. If the proxy path
        # were (wrongly) taken, gather_wall_ms would be 7600.
        rep = _report()
        by_h = {b["height"]: b for b in rep["blocks"]}
        self.assertEqual(by_h[100]["gather_wall_ms"], 8000)  # measured wall
        self.assertNotEqual(by_h[100]["gather_wall_ms"], 7600)  # NOT the proxy
        self.assertEqual(by_h[105]["gather_wall_ms"], 11000)  # measured wall
        # lag = measured gather + measured merge + measured l4.
        self.assertEqual(by_h[100]["lag_ms"], 8000 + 3000 + 4000)

    def test_proxy_fallback_when_no_measured_wall(self):
        # A legacy/partial stream WITHOUT block_complete summaries must still
        # be scoreable, but the block is tagged "proxy" and flagged loudly.
        text = (
            'BENCH_EVENT {"event":"chunk_proven","height":1,"lag_ms":5000,"queue_depth":1}\n'
            'BENCH_EVENT {"event":"coordinator_fold","height":1,"merge_source":"measured","l4_source":"measured","merge_ms":1000,"l4_ms":2000}\n'
        )
        rep = _report(text)
        self.assertEqual(rep["proxy_gather_count"], 1)
        self.assertIn(1, rep["proxy_gather_heights"])
        self.assertFalse(rep["gather_fully_measured"])
        b = rep["blocks"][0]
        self.assertEqual(b["gather_source"], "proxy")
        # Proxy gather (5000) + measured merge (1000) + measured l4 (2000).
        self.assertEqual(b["lag_ms"], 5000 + 1000 + 2000)

    def test_proxy_use_flagged_loudly_in_render(self):
        # When the proxy is used, the rendered report must say so LOUDLY --
        # never a silent default (issue #222 acceptance criterion).
        text = (
            'BENCH_EVENT {"event":"chunk_proven","height":1,"lag_ms":5000,"queue_depth":1}\n'
            'BENCH_EVENT {"event":"coordinator_fold","height":1,"merge_source":"measured","l4_source":"measured","merge_ms":1000,"l4_ms":2000}\n'
        )
        out = lsv._render(_report(text))
        self.assertIn("WARNING", out)
        self.assertIn("PROXY", out)
        self.assertIn("ESTIMATE", out)
        self.assertIn("issue #222", out)

    def test_fully_measured_render_states_no_proxy(self):
        # The all-measured fixture render must affirm no proxy/estimate.
        out = lsv._render(_report())
        self.assertIn("REAL", out)
        self.assertIn("Fully-measured end-to-end lag", out)
        self.assertNotIn("WARNING", out)

    def test_per_block_summary_join_keys_on_height(self):
        # The measured wall must join on the matching height: h102's summary
        # (block_wall_ms 9000) must attach to h102, not bleed into another.
        rep = _report()
        by_h = {b["height"]: b for b in rep["blocks"]}
        self.assertEqual(by_h[102]["gather_wall_ms"], 9000)
        self.assertEqual(by_h[104]["gather_wall_ms"], 10000)

    def test_explicit_chunk_proven_source_tags_proxy(self):
        # Forcing --gather-source chunk_proven must tag every block "proxy"
        # even when a measured wall exists (explicit opt-in to the estimate).
        rep = _report(gather_source="chunk_proven")
        self.assertEqual(rep["proxy_gather_count"], len(rep["blocks"]))
        for b in rep["blocks"]:
            self.assertEqual(b["gather_source"], "proxy")
        # h100's proxy gather is the chunk lag (7600), not the measured 8000.
        by_h = {b["height"]: b for b in rep["blocks"]}
        self.assertEqual(by_h[100]["gather_wall_ms"], 7600)


class TestModeledExclusion(unittest.TestCase):
    """The critical honesty test (issue #179/#215 acceptance rule)."""

    def test_modeled_block_excluded_from_measured(self):
        rep = _report()
        measured_heights = {b["height"] for b in rep["blocks"]}
        # height 106 is modeled -> must NOT appear in the measured set.
        self.assertNotIn(106, measured_heights)
        self.assertEqual(rep["measured_block_count"], 6)

    def test_modeled_block_flagged_and_counted(self):
        rep = _report()
        self.assertEqual(rep["modeled_excluded_count"], 1)
        self.assertIn(106, rep["modeled_excluded_heights"])

    def test_modeled_zeros_never_folded_in(self):
        # If the modeled zeros were (wrongly) counted, a 0 ms lag would
        # appear and drag p50 down. Assert no measured block has the
        # modeled block's zeroed walls.
        rep = _report()
        for b in rep["blocks"]:
            self.assertFalse(b["merge_ms"] == 0 and b["l4_ms"] == 0)


class TestPercentiles(unittest.TestCase):
    def test_nearest_rank_helper(self):
        srt = [15000, 15000, 16000, 16500, 17700, 19000]
        # idx round(0.50*6)-1 = 2 ; round(0.99*6)-1 = 5
        self.assertEqual(lsv._percentile(srt, 0.50), 16000)
        self.assertEqual(lsv._percentile(srt, 0.99), 19000)
        self.assertEqual(lsv._percentile([], 0.50), 0.0)

    def test_fixture_p50_p99(self):
        rep = _report()
        self.assertAlmostEqual(rep["lag_p50_s"], 16.0, places=3)
        self.assertAlmostEqual(rep["lag_p99_s"], 19.0, places=3)


class TestThroughputKeepPace(unittest.TestCase):
    def test_throughput_from_final_summary(self):
        rep = _report()
        self.assertAlmostEqual(rep["throughput_tx_s"], 56.0, places=3)

    def test_observed_block_rate(self):
        rep = _report()
        # 6 measured blocks / elapsed_s(final=1.2) = 5.0 blocks/s
        self.assertAlmostEqual(rep["observed_blocks_s"], 5.0, places=3)

    def test_keep_pace_true(self):
        rep = _report()
        self.assertTrue(rep["keep_pace"]["keep_pace"])
        self.assertTrue(rep["keep_pace"]["backlog_bounded"])

    def test_keep_pace_false_on_growing_backlog(self):
        text = (
            'BENCH_EVENT {"event":"chunk_proven","height":1,"lag_ms":5000,"queue_depth":1}\n'
            'BENCH_EVENT {"event":"coordinator_fold","height":1,"merge_source":"measured","l4_source":"measured","merge_ms":1000,"l4_ms":1000}\n'
            'BENCH_EVENT {"event":"chunk_proven","height":2,"lag_ms":5000,"queue_depth":5}\n'
            'BENCH_EVENT {"event":"coordinator_fold","height":2,"merge_source":"measured","l4_source":"measured","merge_ms":1000,"l4_ms":1000}\n'
            'BENCH_EVENT {"event":"chunk_proven","height":3,"lag_ms":5000,"queue_depth":10}\n'
            'BENCH_EVENT {"event":"coordinator_fold","height":3,"merge_source":"measured","l4_source":"measured","merge_ms":1000,"l4_ms":1000}\n'
            'BENCH_EVENT {"event":"chunk_proven","height":4,"lag_ms":5000,"queue_depth":18}\n'
            'BENCH_EVENT {"event":"coordinator_fold","height":4,"merge_source":"measured","l4_source":"measured","merge_ms":1000,"l4_ms":1000}\n'
            'BENCH_EVENT {"event":"stream_summary","phase":"final","throughput_tx_s":10.0,"lag_p50_ms":7000,"lag_p95_ms":7000,"dropped_chunks":0,"arrivals":1,"gaps_skipped":0,"chunks_proven":4,"elapsed_s":1.0,"ts":"x"}\n'
        )
        rep = _report(text)
        self.assertFalse(rep["keep_pace"]["backlog_bounded"])
        self.assertFalse(rep["keep_pace"]["keep_pace"])


class TestVerdict(unittest.TestCase):
    def test_fixture_pass(self):
        rep = _report()
        self.assertEqual(rep["verdict"], "PASS")

    def test_verdict_pass(self):
        v = lsv._verdict(16.0, 19.0, 5.0, _DEFAULT_THRESHOLDS)
        self.assertEqual(v, "PASS")

    def test_verdict_marginal_within_band(self):
        # p50 = 19.0 is within 2 s of the 20 s threshold -> MARGINAL.
        v = lsv._verdict(19.0, 19.0, 5.0, _DEFAULT_THRESHOLDS)
        self.assertEqual(v, "MARGINAL")

    def test_verdict_marginal_just_over(self):
        # p50 = 21.0 (over 20 but within the 2 s band) -> MARGINAL, not FAIL.
        v = lsv._verdict(21.0, 19.0, 5.0, _DEFAULT_THRESHOLDS)
        self.assertEqual(v, "MARGINAL")

    def test_verdict_fail_p50(self):
        # p50 = 23.0 (> 20 + 2) -> FAIL.
        v = lsv._verdict(23.0, 19.0, 5.0, _DEFAULT_THRESHOLDS)
        self.assertEqual(v, "FAIL")

    def test_verdict_fail_p99(self):
        v = lsv._verdict(16.0, 43.0, 5.0, _DEFAULT_THRESHOLDS)
        self.assertEqual(v, "FAIL")

    def test_verdict_fail_throughput(self):
        # Throughput well below the floor -> FAIL.
        v = lsv._verdict(16.0, 19.0, 4.0, _DEFAULT_THRESHOLDS)
        self.assertEqual(v, "FAIL")

    def test_verdict_fail_no_data(self):
        v = lsv._verdict(None, None, None, _DEFAULT_THRESHOLDS)
        self.assertEqual(v, "FAIL")


class TestRenderAndCaveats(unittest.TestCase):
    def test_all_caveats_present(self):
        rep = _report()
        out = lsv._render(rep)
        for caveat in lsv.CAVEATS:
            self.assertIn(caveat, out)
        # Exactly five mandatory caveats.
        self.assertEqual(len(lsv.CAVEATS), 5)

    def test_render_has_verdict_and_excluded_count(self):
        out = lsv._render(_report())
        self.assertIn("VERDICT: PASS", out)
        self.assertIn("EXCLUDED", out)
        self.assertIn("106", out)

    def test_render_surfaces_fold_decomposition(self):
        out = lsv._render(_report())
        self.assertIn("fold_barrier", out)
        self.assertIn("fold_transit", out)
        self.assertIn("fold_storage", out)


class TestJsonOutRoundTrip(unittest.TestCase):
    def test_json_out_mirrors_render(self):
        import json
        rep = _report()
        payload = json.loads(json.dumps(rep))
        self.assertEqual(payload["verdict"], "PASS")
        self.assertEqual(payload["modeled_excluded_count"], 1)
        self.assertAlmostEqual(payload["lag_p50_s"], 16.0, places=3)
        self.assertAlmostEqual(payload["lag_p99_s"], 19.0, places=3)
        self.assertEqual(payload["caveats"], lsv.CAVEATS)


class TestCli(unittest.TestCase):
    def test_main_exit_zero_on_pass(self):
        rc = lsv.main([_FIXTURE])
        self.assertEqual(rc, 0)

    def test_main_stdin_dash(self):
        import io
        import sys
        old = sys.stdin
        try:
            sys.stdin = io.StringIO(_fixture_text())
            rc = lsv.main(["-"])
        finally:
            sys.stdin = old
        self.assertEqual(rc, 0)

    def test_main_json_out_stdout(self):
        rc = lsv.main([_FIXTURE, "--json-out", "-"])
        self.assertEqual(rc, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
