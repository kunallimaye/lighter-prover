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
