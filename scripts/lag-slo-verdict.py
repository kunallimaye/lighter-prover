#!/usr/bin/env python3
"""lag-slo-verdict.py -- per-block end-to-end lag + keep-pace SLO verdict
from a distributed coordinator's BENCH_EVENT JSONL stream (issue #215).

WHAT THIS COMPUTES
==================
A live distributed run emits the COMPONENTS of per-block lag but never the
SUM, and no committed tool computes the SLO verdict. This parser joins the
two coordinator event streams on `height` and computes the TRUE per-block
lag:

    lag_block = gather_wall_ms        (REAL measured coordinator gather wall:
                                       the per-block stream_summary
                                       block-complete record's block_wall_ms,
                                       keyed by height -- the L1->L2 GATHER
                                       wall, anchored at coordinator-DEQUEUE.
                                       issue #222)
              + coordinator_fold.merge_ms   (MEASURED distributed merge wall)
              + coordinator_fold.l4_ms      (MEASURED L4 prove+verify wall)

    GATHER PROVENANCE (the issue #222 honesty rule)
    ===============================================
    The GATHER term must be the coordinator's REAL recorded gather wall, not
    an estimate. A real coordinator's per-block `stream_summary` (phase
    `block_complete`) now carries `height` + `block_wall_ms` (its own measured
    block wall), so the join keys the TRUE measured gather wall by height.

    A legacy/partial stream that lacks the measured wall can still be scored
    via the slowest-chunk-lag PROXY (`max(chunk_proven.lag_ms)` per height),
    but that proxy APPROXIMATES the gather wall -- it omits the coordination
    time after the last cell finishes. The proxy is therefore NEVER the silent
    default: any block scored on the proxy is tagged, COUNTED, and flagged
    LOUDLY in the rendered report and the machine-readable mirror. A run that
    leans on the proxy for any block is explicitly NOT a fully-measured
    end-to-end lag.

then derives run-level lag p50 AND p99 (nearest-rank, matching
cicd/orchestrator.py `_aggregate`), throughput, a backlog/keep-pace
result, and a PASS / MARGINAL / FAIL verdict against the ADR-0004 §0 SLO:

    lag_p50 <= 20 s  AND  lag_p99 <= 40 s,  sustained >= 5 blocks/s

MEASURED-vs-MODELED GATE (the honesty rule -- issue #179 / #215)
================================================================
A `coordinator_fold` carries `merge_source`/`l4_source` each "measured" or
"modeled". On the modeled path (`--proof-bucket` unset) `merge_ms`/`l4_ms`
are ZERO -- model constants applied DOWNSTREAM, NOT real walls. This tool
NEVER folds a modeled block into the measured percentiles: any block whose
fold is "modeled" is EXCLUDED from the lag distribution and explicitly
flagged + counted. Treating modeled zeros as a real 0 ms merge/L4 would
fabricate a passing verdict -- forbidden.

MANDATORY CAVEATS (always emitted, verbatim -- issue #215 honesty reqs)
=======================================================================
The verdict is scoped and the scope is stated in every report:
  - lag anchor is coordinator-DEQUEUE, NOT mainnet tip;
  - pre-state DELIVERY cost (#177 / #178 / #119) is EXCLUDED;
  - witness_move is UNMODELED (#61 / ADR-0008 §2.3);
  - scope is L1->L4 only (L5 segment scheduling / L6 finalization excluded);
  - a "modeled" merge/L4 is NEVER counted as a measured lag.

EXIT-CODE CONTRACT
==================
  0  -- PASS or MARGINAL (the verdict string is in the output either way;
        MARGINAL is a within-2s-of-threshold band, not a failure)
  1  -- FAIL (any SLO threshold breached beyond the MARGINAL band, or no
        measured blocks at all)
  2  -- usage / IO error (bad path, unreadable stream)

USAGE
=====
    python3 scripts/lag-slo-verdict.py <stream.jsonl>
    grep '^BENCH_EVENT ' run.log | python3 scripts/lag-slo-verdict.py -

Refs: ADR-0004 §0 (SLO); #179 (CoordinatorFold event); #208 (fold_storage
metric); #198 (fold_barrier / fold_transit); #21 / #150 (orchestrator
BENCH_EVENT parser); #144 (north star). The BENCH_EVENT parse contract is
ported from cicd/orchestrator.py:98-117 (NOT imported -- that file is a
live podman fan-out orchestrator with a different concern).
"""

from __future__ import annotations  # issue #215

import argparse
import json
import math
import sys
from dataclasses import dataclass
from typing import Any

# The bench binary emits one structured event per line, prefixed with this
# exact string (note the trailing space). Ported from cicd/orchestrator.py
# (constant `_BENCH_EVENT_PREFIX` at line 60). issue #215
_BENCH_EVENT_PREFIX = "BENCH_EVENT "

# A BENCH_METRIC line is key=value text emitted via info!() (NOT JSON):
# `BENCH_METRIC <name> height=.. key=val ...`. See bench/src/bin/bench.rs
# (fold_barrier/fold_transit #198, fold_storage #206/#208). issue #215
_BENCH_METRIC_PREFIX = "BENCH_METRIC "

# MARGINAL band: a result within this many seconds of a threshold is
# MARGINAL, not a clean PASS/FAIL. Convention borrowed from
# scripts/s-calibrate-report.py (MARGINAL_SLACK_S = 2.0). issue #215
MARGINAL_SLACK_S = 2.0

# The five mandatory caveats, emitted VERBATIM in every report (issue #215
# honesty requirements; acceptance criterion 3). Kept as a module constant
# so the unit test can assert each string is present in rendered output.
CAVEATS = [
    "lag anchor is coordinator-DEQUEUE, not mainnet tip",
    "pre-state DELIVERY cost (#177/#178/#119) is EXCLUDED",
    "witness_move is UNMODELED (#61)",
    "scope is L1->L4 only (L5/L6 excluded)",
    'a "modeled" merge/L4 is NEVER counted as a measured lag',
]


def _parse_events(text: str) -> list[dict[str, Any]]:
    """Extract and decode every BENCH_EVENT JSONL object from `text`.

    Ported from cicd/orchestrator.py:98-117. Lines starting with the
    ``BENCH_EVENT `` prefix have the prefix stripped and the remainder
    json.loads()'d; malformed JSON is skipped defensively (a truncated
    line must not abort the whole parse).

    ADDITIONALLY (issue #215): prefix-stripped streams are accepted --
    entrypoint.sh:120 and s-calibrate.sh strip the prefix when writing
    JSONL, so a bare line that parses to a dict carrying an "event" key is
    also taken. Lines that are neither are ignored (banners, info!() text).
    """
    events: list[dict[str, Any]] = []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        payload: str | None = None
        if line.startswith(_BENCH_EVENT_PREFIX):
            payload = line[len(_BENCH_EVENT_PREFIX):]
        elif stripped.startswith("{"):
            # Possible prefix-stripped JSON line. Only accept it if it
            # decodes to a dict carrying an "event" key (issue #215).
            payload = stripped
        if payload is None:
            continue
        try:
            obj = json.loads(payload)
        except (json.JSONDecodeError, ValueError):
            continue
        if not isinstance(obj, dict):
            continue
        # Prefixed lines are trusted; bare lines must self-identify.
        if line.startswith(_BENCH_EVENT_PREFIX) or "event" in obj:
            events.append(obj)
    return events


def _parse_metrics(text: str) -> dict[str, list[dict[str, Any]]]:
    """Extract BENCH_METRIC key=value lines, grouped by metric name.

    Returns ``{name: [ {key: value, ...}, ... ]}``. Values are coerced to
    int/float when they look numeric, else kept as strings. Used to surface
    the fold_barrier / fold_transit / fold_storage decomposition (issue
    #198 / #206 / #208) when present. issue #215
    """
    out: dict[str, list[dict[str, Any]]] = {}
    for line in text.splitlines():
        if not line.lstrip().startswith(_BENCH_METRIC_PREFIX):
            continue
        body = line.lstrip()[len(_BENCH_METRIC_PREFIX):]
        toks = body.split()
        if not toks:
            continue
        name = toks[0]
        fields: dict[str, Any] = {}
        for tok in toks[1:]:
            if "=" not in tok:
                continue
            key, _, val = tok.partition("=")
            fields[key] = _coerce(val)
        out.setdefault(name, []).append(fields)
    return out


def _coerce(val: str) -> Any:
    """int -> float -> str coercion for a BENCH_METRIC value token."""
    try:
        return int(val)
    except ValueError:
        pass
    try:
        return float(val)
    except ValueError:
        return val


@dataclass
class BlockLag:
    """One block's joined per-block lag accounting. issue #215"""

    height: int
    gather_wall_ms: int
    merge_ms: int
    l4_ms: int
    lag_ms: int
    fold_measured: bool
    # Issue #222: provenance of the GATHER term. "measured" when it is the
    # coordinator's REAL recorded gather wall (the per-block stream_summary
    # block_wall_ms keyed by height); "proxy" when it falls back to the
    # slowest-chunk lag_ms (max chunk_proven.lag_ms), which APPROXIMATES the
    # gather wall but omits coordination time after the last cell finishes.
    # merge_ms/l4_ms remain genuine measured walls regardless.
    gather_source: str = "measured"
    depth: int = 0
    merges: int = 0
    leaves: int = 0


def _extract_blocks(
    events: list[dict[str, Any]],
    block_phase: str = "block_complete",
    gather_source: str = "auto",
) -> tuple[list[BlockLag], list[int]]:
    """Join GATHER walls with coordinator_fold walls on `height`.

    Returns ``(measured_blocks, modeled_heights)`` where ``measured_blocks``
    are the blocks whose fold is MEASURED (merge_source == l4_source ==
    "measured") and ``modeled_heights`` are the heights EXCLUDED because
    their fold was modeled (zeroed walls -- never counted; issue #179/#215).

    GATHER source selection (issue #215, provenance-tracked since #222):
      - "summary":      a stream_summary whose phase == `block_phase`,
                        using block_wall_ms (the coordinator's REAL measured
                        gather wall) -- the truthful default for a real run.
      - "chunk_proven": chunk_proven.lag_ms keyed by height. This is the
                        slowest-chunk-lag PROXY: it APPROXIMATES the gather
                        wall but omits coordination time after the last cell
                        finishes, so it is NOT a fully-measured end-to-end
                        lag (issue #222). Selecting it is an explicit opt-in.
      - "auto":         PER BLOCK, prefer the real measured summary wall when
                        the coordinator emitted one for that height; fall back
                        to the chunk_proven proxy ONLY for blocks with no
                        measured wall. Each block records which source it used
                        in `gather_source` so the report can flag any proxy
                        use LOUDLY rather than silently (issue #222).

    Each returned BlockLag carries `gather_source` = "measured" | "proxy"
    so the caller can refuse to silently ship a proxy lag on a real run.
    """
    # Index coordinator_fold by height (last write wins -- one per block).
    folds: dict[int, dict[str, Any]] = {}
    for ev in events:
        if ev.get("event") == "coordinator_fold":
            h = ev.get("height")
            if h is not None:
                folds[int(h)] = ev

    # Collect candidate GATHER walls from each source, keyed by height.
    # summary_walls is the REAL measured gather wall the coordinator records on
    # its per-block completion record (issue #222); chunk_walls is the proxy.
    summary_walls: dict[int, int] = {}
    for ev in events:
        if ev.get("event") != "stream_summary":
            continue
        if ev.get("phase") != block_phase:
            continue
        h = ev.get("height")
        wall = ev.get("block_wall_ms")
        if h is not None and wall is not None:
            summary_walls[int(h)] = int(wall)

    chunk_walls: dict[int, int] = {}
    for ev in events:
        if ev.get("event") != "chunk_proven":
            continue
        h = ev.get("height")
        lag = ev.get("lag_ms")
        if h is None or lag is None:
            continue
        # A block may emit several chunk_proven lines (one per chunk); the
        # PROXY gather wall is the slowest chunk's lag (block is complete when
        # the last chunk lands). issue #215
        h = int(h)
        chunk_walls[h] = max(chunk_walls.get(h, 0), int(lag))

    # Build the per-block (wall, source) map according to the selection mode.
    # On "auto" we resolve PER HEIGHT so a partly-instrumented stream uses the
    # real measured wall wherever it exists and only proxies the gap (#222).
    if gather_source == "summary":
        gather: dict[int, tuple[int, str]] = {
            h: (w, "measured") for h, w in summary_walls.items()
        }
    elif gather_source == "chunk_proven":
        # Explicit opt-in to the proxy: every block is tagged "proxy".
        gather = {h: (w, "proxy") for h, w in chunk_walls.items()}
    else:  # auto -- per-block: real measured wall wins, proxy fills the gap.
        heights = set(summary_walls) | set(chunk_walls)
        gather = {}
        for h in heights:
            if h in summary_walls:
                gather[h] = (summary_walls[h], "measured")
            else:
                gather[h] = (chunk_walls[h], "proxy")

    measured: list[BlockLag] = []
    modeled: list[int] = []
    for height in sorted(gather):
        fold = folds.get(height)
        gather_wall, gsrc = gather[height]
        if fold is None:
            # No fold event for this block -- cannot compute a real lag.
            # Treat as modeled/excluded so it is flagged, never silently
            # counted with a zeroed merge/L4. issue #215
            modeled.append(height)
            continue
        merge_src = fold.get("merge_source")
        l4_src = fold.get("l4_source")
        fold_measured = (merge_src == "measured" and l4_src == "measured")
        if not fold_measured:
            modeled.append(height)
            continue
        merge_ms = int(fold.get("merge_ms", 0))
        l4_ms = int(fold.get("l4_ms", 0))
        lag_ms = gather_wall + merge_ms + l4_ms
        measured.append(
            BlockLag(
                height=height,
                gather_wall_ms=gather_wall,
                merge_ms=merge_ms,
                l4_ms=l4_ms,
                lag_ms=lag_ms,
                fold_measured=True,
                gather_source=gsrc,
                depth=int(fold.get("depth", 0)),
                merges=int(fold.get("merges", 0)),
                leaves=int(fold.get("leaves", 0)),
            )
        )
    return measured, modeled


def _percentile(sorted_vals: list[float], q: float) -> float:
    """Nearest-rank percentile, matching cicd/orchestrator.py `_aggregate`.

    `q` is a fraction in [0, 1]. Returns 0.0 for an empty input. issue #215
    """
    n = len(sorted_vals)
    if n == 0:
        return 0.0
    idx = max(0, min(n - 1, int(round(q * n)) - 1))
    return sorted_vals[idx]


def _keep_pace(
    events: list[dict[str, Any]],
    drive_rate_blocks_s: float | None,
    blocks: list[BlockLag],
) -> dict[str, Any]:
    """Derive throughput + backlog trend + the boolean keep-pace result.

    "Keep pace" means blocks are FINISHED at least as fast as they ARRIVE.
    This function reports the result on one of two bases, and labels which
    one was used so a verdict can never be over-read (issue #223):

      - "finish-vs-arrival" (STRONG): when ``drive_rate_blocks_s`` is supplied
        and an observed block rate is computable, keep-pace rests on the REAL
        arrival-vs-finish comparison ``observed_blocks_s >= drive_rate_blocks_s``,
        with backlog-bounded kept as a corroborating guard. This is the
        primary signal.

      - "queue-proxy" (WEAK): when no drive rate is supplied, keep-pace can
        only rest on the non-growing ``queue_depth`` series. That is a HINT
        ("the local queue isn't growing"), NOT a proof of finishing as fast
        as arriving. This path is loudly labelled as a proxy and carries a
        warning instructing the operator to pass ``--drive-rate-blocks-s``
        for the strong check.

    Backlog is read from queue_depth (chunk_proven), with
    num_undelivered_messages / dropped_chunks as additional signals when
    present (issue #215).
    """
    # Observed throughput: last stream_summary's throughput_tx_s.
    throughput_tx_s = None
    for ev in events:
        if ev.get("event") == "stream_summary":
            tps = ev.get("throughput_tx_s")
            if tps is not None:
                throughput_tx_s = float(tps)

    # Backlog trend: monotonic-growth check over the queue_depth series.
    backlog_series: list[int] = []
    for ev in events:
        if ev.get("event") == "chunk_proven":
            qd = ev.get("queue_depth")
            if qd is not None:
                backlog_series.append(int(qd))
        # num_undelivered_messages may ride on stream_summary in a future
        # coordinator; read it defensively wherever it appears.
        num_undeliv = ev.get("num_undelivered_messages")
        if num_undeliv is not None:
            backlog_series.append(int(num_undeliv))

    backlog_growing = _is_monotonic_growth(backlog_series)

    dropped = 0
    for ev in events:
        if ev.get("event") == "stream_summary":
            d = ev.get("dropped_chunks")
            if d is not None:
                dropped = max(dropped, int(d))

    # Observed block rate over the measured run, from stream_summary
    # elapsed_s when available (final phase carries the run span).
    elapsed_s = None
    for ev in events:
        if ev.get("event") == "stream_summary":
            es = ev.get("elapsed_s")
            if es is not None:
                elapsed_s = float(es)
    observed_blocks_s = None
    if elapsed_s and elapsed_s > 0 and blocks:
        # Observed block rate over the measured set: measured-block count
        # divided by the run span. We deliberately use the MEASURED blocks
        # (modeled blocks are excluded by the lag-honesty gate); on a
        # measured `--proof-bucket` run every block has a measured fold, so
        # this is the full block count. issue #215
        observed_blocks_s = len(blocks) / elapsed_s

    backlog_bounded = not backlog_growing

    # Strong basis requires BOTH a supplied drive rate AND an observable
    # block rate to compare against it. Otherwise we can only fall back to
    # the weak queue-only proxy. issue #223
    have_real_comparison = (
        drive_rate_blocks_s is not None and observed_blocks_s is not None
    )

    rate_ok = True
    if have_real_comparison:
        rate_ok = observed_blocks_s >= drive_rate_blocks_s

    if have_real_comparison:
        # STRONG: keep-pace IS the finish-rate-vs-arrival-rate comparison,
        # with backlog-bounded kept as a corroborating guard.
        keep_pace_basis = "finish-vs-arrival"
        keep_pace_strong = True
        keep_pace = rate_ok and backlog_bounded
        keep_pace_warning = ""
    else:
        # WEAK: no drive rate (or no observable block rate) -> we can only
        # observe whether the local queue is growing. Label it loudly so the
        # verdict is not mistaken for a true keep-pace measurement. issue #223
        keep_pace_basis = "queue-proxy"
        keep_pace_strong = False
        keep_pace = backlog_bounded
        keep_pace_warning = (
            "keep-pace is the WEAK queue-only proxy (queue_depth not growing); "
            "it does NOT prove blocks finish as fast as they arrive. Pass "
            "--drive-rate-blocks-s for the real finish-rate-vs-arrival-rate check."
        )

    return {
        "throughput_tx_s": throughput_tx_s,
        "observed_blocks_s": observed_blocks_s,
        "drive_rate_blocks_s": drive_rate_blocks_s,
        "backlog_series": backlog_series,
        "backlog_bounded": backlog_bounded,
        "dropped_chunks": dropped,
        "rate_ok": rate_ok,
        "keep_pace": keep_pace,
        "keep_pace_basis": keep_pace_basis,
        "keep_pace_strong": keep_pace_strong,
        "keep_pace_warning": keep_pace_warning,
    }


def _is_monotonic_growth(series: list[int]) -> bool:
    """True when the backlog series shows sustained, unbounded growth.

    A strictly non-decreasing series that ends materially higher than it
    started is treated as growing (the dispatch backlog is not draining).
    A short or flat series is NOT growing. issue #215
    """
    if len(series) < 3:
        return False
    # Sustained growth = the last value exceeds the first AND the series is
    # (weakly) non-decreasing across most steps.
    non_decreasing_steps = sum(
        1 for a, b in zip(series, series[1:]) if b >= a
    )
    fraction_up = non_decreasing_steps / (len(series) - 1)
    return series[-1] > series[0] and fraction_up >= 0.8


def _verdict(
    lag_p50_s: float | None,
    lag_p99_s: float | None,
    throughput_blocks_s: float | None,
    thresholds: dict[str, float],
) -> str:
    """Return PASS / MARGINAL / FAIL with the 2 s MARGINAL band. issue #215

    FAIL  : any threshold breached by more than MARGINAL_SLACK_S, or no
            measured data (lag is None).
    MARGINAL: within MARGINAL_SLACK_S of a lag threshold, or the throughput
            is within a proportional band of the min rate.
    PASS  : comfortably within every threshold.
    """
    if lag_p50_s is None or lag_p99_s is None:
        return "FAIL"

    p50_max = thresholds["lag_p50_s"]
    p99_max = thresholds["lag_p99_s"]
    min_blocks_s = thresholds["min_blocks_s"]

    # Hard failures: beyond the MARGINAL band on either lag threshold.
    if lag_p50_s > p50_max + MARGINAL_SLACK_S:
        return "FAIL"
    if lag_p99_s > p99_max + MARGINAL_SLACK_S:
        return "FAIL"

    marginal = False
    # Within the band (above OR just below) on either lag threshold.
    if lag_p50_s > p50_max - MARGINAL_SLACK_S:
        marginal = True
    if lag_p99_s > p99_max - MARGINAL_SLACK_S:
        marginal = True

    # Throughput: a measured rate below the floor fails; within ~10% is
    # MARGINAL. Unknown throughput cannot upgrade a verdict to PASS.
    if throughput_blocks_s is not None:
        if throughput_blocks_s < min_blocks_s * 0.9:
            return "FAIL"
        if throughput_blocks_s < min_blocks_s:
            marginal = True

    return "MARGINAL" if marginal else "PASS"


def build_report(
    text: str,
    thresholds: dict[str, float],
    block_phase: str = "block_complete",
    gather_source: str = "auto",
    drive_rate_blocks_s: float | None = None,
) -> dict[str, Any]:
    """Parse a stream and assemble the full machine-readable report dict."""
    events = _parse_events(text)
    metrics = _parse_metrics(text)
    blocks, modeled_heights = _extract_blocks(
        events, block_phase=block_phase, gather_source=gather_source
    )

    lags_ms_sorted = sorted(b.lag_ms for b in blocks)
    lag_p50_s = (
        _percentile(lags_ms_sorted, 0.50) / 1000.0 if lags_ms_sorted else None
    )
    lag_p99_s = (
        _percentile(lags_ms_sorted, 0.99) / 1000.0 if lags_ms_sorted else None
    )

    keep = _keep_pace(events, drive_rate_blocks_s, blocks)
    verdict = _verdict(
        lag_p50_s, lag_p99_s, keep.get("observed_blocks_s"), thresholds
    )

    fold_decomp = {
        name: metrics[name]
        for name in ("fold_barrier", "fold_transit", "fold_storage")
        if name in metrics
    }

    # Issue #222: GATHER provenance accounting. Any block whose GATHER term is
    # the slowest-chunk-lag PROXY (not the coordinator's real measured wall) is
    # counted and listed so the report can flag it LOUDLY. A fully-measured run
    # has proxy_gather_count == 0.
    proxy_heights = [b.height for b in blocks if b.gather_source == "proxy"]
    measured_gather_count = sum(
        1 for b in blocks if b.gather_source == "measured"
    )

    return {
        "blocks": [b.__dict__ for b in blocks],
        "modeled_excluded_heights": modeled_heights,
        "modeled_excluded_count": len(modeled_heights),
        "measured_block_count": len(blocks),
        "measured_gather_count": measured_gather_count,
        "proxy_gather_heights": proxy_heights,
        "proxy_gather_count": len(proxy_heights),
        "gather_fully_measured": len(proxy_heights) == 0,
        "lag_p50_s": lag_p50_s,
        "lag_p99_s": lag_p99_s,
        "throughput_tx_s": keep.get("throughput_tx_s"),
        "observed_blocks_s": keep.get("observed_blocks_s"),
        "keep_pace": keep,
        "thresholds": thresholds,
        "verdict": verdict,
        "fold_decomposition": fold_decomp,
        "caveats": CAVEATS,
    }


def _render(report: dict[str, Any]) -> str:
    """Human-readable verdict block. issue #215"""
    lines: list[str] = []
    lines.append("=" * 64)
    lines.append("per-block end-to-end lag + keep-pace SLO verdict (issue #215)")
    lines.append("=" * 64)
    lines.append("")

    # Per-block lag table. The `gsrc` column (issue #222) marks each block's
    # GATHER provenance: "meas" = real measured coordinator wall; "PROXY" =
    # slowest-chunk-lag estimate (flagged loudly below).
    lines.append("Per-block lag (measured merge+L4, ms):")
    header = (
        f"  {'height':>10} {'gather':>9} {'gsrc':>6} {'merge':>9} {'l4':>9} "
        f"{'lag_ms':>9} {'depth':>6} {'merges':>7} {'leaves':>7}"
    )
    lines.append(header)
    for b in report["blocks"]:
        gsrc = "PROXY" if b.get("gather_source") == "proxy" else "meas"
        lines.append(
            f"  {b['height']:>10} {b['gather_wall_ms']:>9} {gsrc:>6} "
            f"{b['merge_ms']:>9} {b['l4_ms']:>9} {b['lag_ms']:>9} "
            f"{b['depth']:>6} {b['merges']:>7} {b['leaves']:>7}"
        )
    if not report["blocks"]:
        lines.append("  (no MEASURED blocks -- nothing to score)")
    lines.append("")

    # Issue #222: GATHER provenance banner. The proxy is NEVER silent -- if any
    # block was scored on the slowest-chunk-lag estimate, say so LOUDLY so the
    # reported lag is not mistaken for a fully-measured end-to-end number.
    proxy_n = report.get("proxy_gather_count", 0)
    if proxy_n:
        ph = ", ".join(str(h) for h in report["proxy_gather_heights"])
        lines.append("!" * 64)
        lines.append(
            f"WARNING: GATHER is an ESTIMATE (PROXY) for {proxy_n} block(s) at "
            f"height(s) {ph}."
        )
        lines.append(
            "  These blocks use max(chunk_proven.lag_ms) -- the slowest single "
            "cell's lag --"
        )
        lines.append(
            "  NOT the coordinator's real measured gather wall. The reported "
            "lag for these"
        )
        lines.append(
            "  blocks is therefore PARTLY ESTIMATED (it omits coordination "
            "time after the"
        )
        lines.append(
            "  last cell finishes), NOT a fully-measured end-to-end lag. "
            "(issue #222)"
        )
        lines.append("!" * 64)
    else:
        lines.append(
            "GATHER provenance: all measured blocks use the coordinator's REAL "
            "measured"
        )
        lines.append(
            "  gather wall (no proxy/estimate). Fully-measured end-to-end lag. "
            "(issue #222)"
        )
    lines.append("")

    # Excluded modeled blocks (the honesty count).
    excl = report["modeled_excluded_count"]
    if excl:
        heights = ", ".join(str(h) for h in report["modeled_excluded_heights"])
        lines.append(
            f"EXCLUDED (modeled/zeroed fold -- NOT counted): {excl} "
            f"block(s) at height(s) {heights}"
        )
    else:
        lines.append("EXCLUDED (modeled/zeroed fold): 0 blocks")
    lines.append("")

    # Run-level figures.
    p50 = report["lag_p50_s"]
    p99 = report["lag_p99_s"]
    th = report["thresholds"]
    lines.append("Run-level:")
    lines.append(
        f"  lag p50 = {_fmt_s(p50)} s   (SLO <= {th['lag_p50_s']:g} s)"
    )
    lines.append(
        f"  lag p99 = {_fmt_s(p99)} s   (SLO <= {th['lag_p99_s']:g} s)"
    )
    keep = report["keep_pace"]
    lines.append(
        f"  throughput = {_fmt_s(report['throughput_tx_s'])} tx/s"
    )
    obs = keep.get("observed_blocks_s")
    lines.append(
        f"  observed = {_fmt_s(obs)} blocks/s   "
        f"(SLO >= {th['min_blocks_s']:g} blocks/s)"
    )
    drive = keep.get("drive_rate_blocks_s")
    if drive is not None:
        lines.append(f"  drive rate = {drive:g} blocks/s")
    lines.append(
        f"  backlog bounded = {keep['backlog_bounded']}; "
        f"dropped_chunks = {keep['dropped_chunks']}; "
        f"keep-pace = {keep['keep_pace']}"
    )
    # Keep-pace basis: unmistakably label STRONG vs WEAK so a verdict can
    # never be over-read as a true keep-pace measurement. issue #223
    if keep.get("keep_pace_strong"):
        lines.append(
            "  keep-pace basis = finish-vs-arrival (STRONG: observed block "
            "rate vs supplied drive rate)"
        )
    else:
        lines.append(
            "  keep-pace basis = queue-proxy (WEAK: queue_depth not growing "
            "-- pass --drive-rate-blocks-s for the real finish-rate-vs-"
            "arrival-rate check)"
        )
    warning = keep.get("keep_pace_warning")
    if warning:
        lines.append(f"  WARNING: {warning}")
    lines.append("")

    # Fold decomposition, when present.
    decomp = report["fold_decomposition"]
    if decomp:
        lines.append("Fold decomposition (BENCH_METRIC):")
        for name in ("fold_barrier", "fold_transit", "fold_storage"):
            if name in decomp:
                lines.append(f"  {name}: {len(decomp[name])} record(s)")
        lines.append("")

    # Verdict.
    lines.append(f"VERDICT: {report['verdict']}  "
                 f"(vs {th['lag_p50_s']:g}/{th['lag_p99_s']:g} s "
                 f"@ >= {th['min_blocks_s']:g} blk/s)")
    lines.append("")

    # Mandatory caveats (verbatim).
    lines.append("CAVEATS (scope -- read before trusting this verdict):")
    for c in report["caveats"]:
        lines.append(f"  - {c}")
    lines.append("=" * 64)
    return "\n".join(lines)


def _fmt_s(v: float | None) -> str:
    return "NA" if v is None else f"{v:.3f}"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument(
        "path",
        help="coordinator BENCH_EVENT JSONL path, or '-' for stdin",
    )
    ap.add_argument("--lag-p50-s", type=float, default=20.0,
                    help="SLO p50 lag budget in s (ADR-0004 §0; default 20)")
    ap.add_argument("--lag-p99-s", type=float, default=40.0,
                    help="SLO p99 lag budget in s (ADR-0004 §0; default 40)")
    ap.add_argument("--min-blocks-s", type=float, default=5.0,
                    help="sustained block rate floor (default 5 blocks/s)")
    ap.add_argument("--block-phase", default="block_complete",
                    help="stream_summary phase carrying the per-block gather "
                         "wall (default block_complete)")
    ap.add_argument("--gather-source", choices=["summary", "chunk_proven", "auto"],
                    default="auto",
                    help="per-block GATHER wall source (default auto: prefer "
                         "block-complete summaries, else chunk_proven.lag_ms)")
    ap.add_argument("--drive-rate-blocks-s", type=float, default=None,
                    help="offered block rate for the keep-pace check "
                         "(observed throughput must meet or exceed it)")
    ap.add_argument("--json-out", default=None, metavar="PATH",
                    help="write a machine-readable mirror of the verdict to "
                         "PATH (use '-' for stdout)")
    args = ap.parse_args(argv)

    try:
        if args.path == "-":
            text = sys.stdin.read()
        else:
            with open(args.path, encoding="utf-8") as fh:
                text = fh.read()
    except OSError as exc:
        print(f"error: cannot read {args.path}: {exc}", file=sys.stderr)
        return 2

    thresholds = {
        "lag_p50_s": args.lag_p50_s,
        "lag_p99_s": args.lag_p99_s,
        "min_blocks_s": args.min_blocks_s,
    }
    report = build_report(
        text,
        thresholds,
        block_phase=args.block_phase,
        gather_source=args.gather_source,
        drive_rate_blocks_s=args.drive_rate_blocks_s,
    )

    print(_render(report))

    if args.json_out:
        payload = json.dumps(report, indent=2, sort_keys=False) + "\n"
        if args.json_out == "-":
            sys.stdout.write(payload)
        else:
            try:
                with open(args.json_out, "w", encoding="utf-8") as fh:
                    fh.write(payload)
            except OSError as exc:
                print(f"error: cannot write {args.json_out}: {exc}",
                      file=sys.stderr)
                return 2

    # Exit-code contract: 0 for PASS/MARGINAL, 1 for FAIL.
    return 0 if report["verdict"] in ("PASS", "MARGINAL") else 1


if __name__ == "__main__":
    sys.exit(main())
