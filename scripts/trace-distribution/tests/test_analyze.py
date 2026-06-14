"""Pytest wrapper for the trace-distribution analyzer (issue #128).

Runs analyze.py against the committed 201-line fixture and asserts the
documented ground-truth properties from bench/trace-format.md §8.2. This both
tests the script and validates it against ground truth.

Runnable two ways:
  - pytest scripts/trace-distribution/tests/
  - python3 scripts/trace-distribution/tests/test_analyze.py   (no pytest dep)
"""

import os
import sys

_TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
_PKG_DIR = os.path.abspath(os.path.join(_TESTS_DIR, ".."))
if _PKG_DIR not in sys.path:
    sys.path.insert(0, _PKG_DIR)

import analyze  # noqa: E402


def _report():
    return analyze.analyze_trace(analyze.DEFAULT_FIXTURE)


def test_block_count_matches_8_2():
    assert _report()["block_size"]["blocks"] == 201


def test_height_range_matches_8_2():
    m = _report()["meta"]
    assert m["height_first"] == 260138266
    assert m["height_last"] == 260138493
    assert m["height_span"] == 228


def test_null_blocks_matches_8_2():
    bs = _report()["block_size"]
    assert bs["null_blocks"] == 40
    assert abs(bs["null_fraction"] * 100 - 19.9) <= 0.1


def test_block_size_central_tendency_matches_8_2():
    bs = _report()["block_size"]
    assert bs["min"] == 1
    assert bs["max"] == 500
    assert abs(bs["mean_non_null"] - 367.55) <= 0.01
    assert bs["median_non_null"] == 500


def test_jumps_match_8_2():
    ar = _report()["arrival_rate"]
    assert ar["jumps"] == 9
    assert ar["jump_deltas"] == [9, 4, 4, 4, 4, 4, 3, 2, 2]
    assert ar["max_jump_delta"] == 9
    assert ar["skipped_heights"] == 27


def test_span_matches_8_2():
    assert abs(_report()["meta"]["span_s"] - 19.64) <= 0.01


def test_no_over_cap_blocks():
    assert _report()["outliers"]["over_cap_blocks"] == 0


def test_self_check_passes():
    # The script's own --self-check must exit 0.
    assert analyze.self_check() == 0


def test_size_bands_sum_to_non_null():
    # The size-band counts must partition the non-null block set exactly —
    # no double-counting, no gaps. (Bands extension, issue #128.)
    bs = _report()["block_size"]
    bands = bs["bands"]
    assert sum(bands.values()) == bs["non_null_blocks"]
    # Cap band == outliers.at_cap_blocks (cross-check).
    assert bands["eq_500"] == _report()["outliers"]["at_cap_blocks"]


def test_p99_9_and_p75_present():
    # New tail percentiles emitted alongside the existing p50/p90/p95/p99.
    bs = _report()["block_size"]
    assert bs["p75"] is not None
    assert bs["p99_9"] is not None
    # On the bimodal fixture the cap dominates the upper half; p99.9 == 500.
    assert bs["p99_9"] == 500


def test_arrival_burst_fields_present():
    # Burst characterization is the conductor-sizing signal (ADR-0004).
    ar = _report()["arrival_rate"]
    assert "bursts" in ar and ar["bursts"]
    for w in (1, 3, 5, 10):
        assert f"peak_blocks_in_{w}s" in ar["bursts"]
    assert ar["blocks_per_s_max"] >= ar["blocks_per_s_p50"]
    assert ar["gap_p99_9_ms"] >= ar["gap_p99_ms"] >= ar["gap_p95_ms"]


def test_percentile_supports_fractional_p():
    # The percentile helper must accept fractional p (99.9 etc.) — the
    # original integer ceil division silently rounded these down to 99.
    # Use N=10000 to dodge float-rounding edge cases on round-decimal p.
    srt = list(range(1, 10001))  # 1..10000
    assert analyze.percentile(srt, 50) == 5000
    assert analyze.percentile(srt, 99) == 9900
    # nearest-rank on N=10000 at p=99.9: ceil(0.999*10000)=9990
    assert analyze.percentile(srt, 99.9) == 9990
    # Edge bounds.
    assert analyze.percentile(srt, 0) == 1
    assert analyze.percentile(srt, 100) == 10000


if __name__ == "__main__":
    # Allow running without pytest installed (e.g. bare make local-test envs).
    funcs = [v for k, v in sorted(globals().items())
             if k.startswith("test_") and callable(v)]
    failures = 0
    for fn in funcs:
        try:
            fn()
            print(f"  ok   {fn.__name__}")
        except AssertionError as e:
            failures += 1
            print(f"  FAIL {fn.__name__}: {e}")
    if failures:
        print(f"\n{failures} test(s) failed")
        sys.exit(1)
    print(f"\nall {len(funcs)} tests passed")
