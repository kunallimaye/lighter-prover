#!/usr/bin/env python3
"""feeder.py — trace producer for the streaming bench (issue #48).

Contract: bench/trace-format.md (pinned 2026-06-11). Every emitted stream
conforms to that spec: provenance header first line, JSONL block events
{ts_ms, height, tx_count}, gap markers {gap, ts_ms, reason}, ts_ms
non-decreasing, height strictly increasing, tx_count int|null (P4).

Subcommands:
  record     capture live chain cadence (WS height channel + explorer poll)
  replay     re-emit a recorded trace with scaled inter-arrival gaps
  synth-peak fabricate an idealized back-to-back-500-tx trace from a rate
  peak-hours report top-N hours by tx/s from the explorer stats API
  tx-mix     capture per-block tx_type counts -> the mainnet tx-mix (#128)

Only the network subcommands (record, peak-hours, tx-mix) need third-party
deps (websockets, requests — see requirements.txt); they are imported lazily
so replay/synth-peak/tests (and `tx-mix --sample-block`) run on a bare
Python 3 stdlib.

Policies (see spec §6) applied by replay, IN THIS ORDER:
  P2 expand height jumps -> P1 fill nulls with mean-of-non-null -> P4 round.
"""

import argparse
import asyncio
import datetime
import json
import math
import signal
import statistics
import sys
import time

WS_URL = "wss://mainnet.zklighter.elliot.ai/stream?readonly=true"
POLL_URL = "https://explorer.elliot.ai/api/blocks"
STATS_URL = "https://explorer.elliot.ai/api/stats/tx?aggregation_period=1h"
UA = "lighter-prover-feeder/48 (research; single conn; <=85 req/min)"

# tx-MIX capture (issue #128 tx-type gap). The explorer's block endpoints
# carry only block_size / total_transactions — NO per-tx tx_type field — so
# the only HTTP source that exposes tx_type per transaction is the zklighter
# mainnet API's blockTxs endpoint. (Confirmed by probing: explorer
# /api/blocks/{h} returns {total_transactions, markets, logs} only.)
# That API geo-blocks some IPs with HTTP 403 (same block the main REST API
# applies, see cmd_peak_hours), which `tx-mix` reports honestly rather than
# inventing numbers.
TXMIX_BLOCK_URL = "https://mainnet.zklighter.elliot.ai/api/v1/blockTxs"
TXMIX_POLL_PERIOD_S = 0.71  # ~84.5 req/min, under the 90/min per-IP limit

# Lighter tx_type enum -> human name. The four dominant trading types are
# {14,15,17,21}; others are carried through as "type_<n>" so an unexpected
# type is never silently dropped. Names follow circuit/src naming
# (tx_constraints.rs): L2_CREATE_ORDER=14, L2_CANCEL_ORDER=15,
# L2_MODIFY_ORDER=17, INTERNAL_CLAIM_ORDER=21.
TX_TYPE_NAMES = {
    14: "create",   # L2_CREATE_ORDER
    15: "cancel",   # L2_CANCEL_ORDER
    17: "modify",   # L2_MODIFY_ORDER
    21: "claim",    # INTERNAL_CLAIM_ORDER
}

BLOCK_TX_CAP = 500          # chain per-block tx cap (spec §6.2)
POLL_PERIOD_S = 0.71        # ~84.5 req/min, under the 90/min per-IP limit
WATERMARK_S = 4.0           # late-bind window for tx_count (3-5s per design)
WS_SILENCE_TIMEOUT_S = 10.0  # watchdog: reconnect after 10s of silence
BACKOFF_INITIAL_S = 1.0
BACKOFF_CAP_S = 15.0
SYNTH_HEIGHT_BASE = 1_000_000


# ──────────────────────────────────────────────────────────────────────
# Shared helpers (pure, stdlib-only — unit tested offline)
# ──────────────────────────────────────────────────────────────────────

def now_iso():
    return datetime.datetime.now(datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ")


def parse_duration(text):
    """'15m' / '900s' / '900' / '1h' -> seconds (float)."""
    text = str(text).strip().lower()
    if text.endswith("ms"):
        return float(text[:-2]) / 1000.0
    if text.endswith("m"):
        return float(text[:-1]) * 60.0
    if text.endswith("s"):
        return float(text[:-1])
    if text.endswith("h"):
        return float(text[:-1]) * 3600.0
    return float(text)


def provenance_line(generator, params, source_trace=None, generated_at=None):
    prov = {"generator": generator, "params": params}
    if source_trace is not None:
        prov["source_trace"] = source_trace
    prov["generated_at"] = generated_at or now_iso()
    return json.dumps({"provenance": prov})


def load_trace(lines):
    """Parse trace lines -> (header, events, gap_count, no_expand_heights).

    events: list of {"ts_ms","height","tx_count"} block events, in order.
    no_expand_heights: heights whose event immediately follows a gap
    marker — a discontinuity there is genuine feed loss, NOT a coalesced
    push, so P2 expansion is suppressed (spec §6.2 exception).
    Headerless input is accepted (pre-spec exemption, spec §4).
    """
    header = None
    events = []
    gap_count = 0
    no_expand = set()
    after_gap = False
    for i, raw in enumerate(lines):
        raw = raw.strip()
        if not raw:
            raise ValueError("blank line in trace (forbidden by spec §1)")
        obj = json.loads(raw)
        if "provenance" in obj:
            if i != 0:
                raise ValueError("provenance header must be the first line")
            header = obj["provenance"]
        elif obj.get("gap") is True:
            gap_count += 1
            after_gap = True
        elif "height" in obj:
            ev = {"ts_ms": obj["ts_ms"], "height": obj["height"],
                  "tx_count": obj.get("tx_count")}
            if after_gap:
                no_expand.add(ev["height"])
                after_gap = False
            events.append(ev)
        else:
            raise ValueError(f"unrecognized trace line: {raw[:80]}")
    return header, events, gap_count, no_expand


def validate_events(events):
    """Spec §5 monotonicity over block events. Raises ValueError."""
    prev_ts, prev_h = None, None
    for ev in events:
        if prev_ts is not None and ev["ts_ms"] < prev_ts:
            raise ValueError(f"ts_ms regression at height {ev['height']}")
        if prev_h is not None and ev["height"] <= prev_h:
            raise ValueError(f"height not strictly increasing at "
                             f"{ev['height']} (prev {prev_h})")
        prev_ts, prev_h = ev["ts_ms"], ev["height"]


def mean_fill_value(events):
    """P1: fill value = mean of the trace's non-null tx_counts (float)."""
    txs = [e["tx_count"] for e in events if e["tx_count"] is not None]
    if not txs:
        raise ValueError("trace has no non-null tx_count; cannot fill (P1)")
    return statistics.mean(txs)


def expand_and_fill(events, no_expand_heights=frozenset()):
    """Apply policies in order: P2 expand -> P1 fill -> P4 round.

    Returns list of {"ts_ms","height","tx_count"(int),"synthetic"(bool)}.
    A height jump of k expands into k events at the observed push's
    ts_ms; intermediates get the fill value, the final height keeps the
    observed tx_count (spec §6.2). Jumps landing on a height that
    directly follows a gap marker are NOT expanded (feed outage).
    """
    fill = mean_fill_value(events)
    out = []
    prev_h = None
    for ev in events:
        h, ts = ev["height"], ev["ts_ms"]
        if (prev_h is not None and h - prev_h > 1
                and h not in no_expand_heights):
            for hh in range(prev_h + 1, h):
                out.append({"ts_ms": ts, "height": hh,
                            "tx_count": None, "synthetic": True})
        out.append({"ts_ms": ts, "height": h,
                    "tx_count": ev["tx_count"], "synthetic": False})
        prev_h = h
    for ev in out:                       # P1 fill, then P4 round
        tx = ev["tx_count"] if ev["tx_count"] is not None else fill
        ev["tx_count"] = int(round(tx))
    return out


def aggregate_rate(expanded):
    """P1: Σ(post-fill, post-expansion tx) ÷ span seconds."""
    span_s = (expanded[-1]["ts_ms"] - expanded[0]["ts_ms"]) / 1000.0
    if span_s <= 0:
        raise ValueError("trace span is zero; cannot compute aggregate rate")
    return sum(e["tx_count"] for e in expanded) / span_s


def median_inter_block_gap_ms(expanded):
    gaps = [b["ts_ms"] - a["ts_ms"] for a, b in zip(expanded, expanded[1:])]
    return statistics.median(gaps) if gaps else 0.0


# ──────────────────────────────────────────────────────────────────────
# tx-MIX helpers (issue #128 tx-type gap) — pure, stdlib-only, unit tested.
# These aggregate per-tx tx_type values into counts; the network capture
# (cmd_tx_mix) feeds them rows from the blockTxs API, and the sample-block
# fallback feeds them the in-repo bench_test.json (labeled n=1 — a SAMPLE,
# never "the distribution").
# ──────────────────────────────────────────────────────────────────────

def tx_type_name(tx_type):
    """Human name for a Lighter tx_type, falling back to 'type_<n>'."""
    return TX_TYPE_NAMES.get(tx_type, f"type_{tx_type}")


def count_tx_types(txs):
    """Count tx_type occurrences over an iterable of tx dicts.

    Each item must carry a "tx_type" key (the field the blockTxs API and the
    in-repo sample block both use). Items lacking it are skipped and counted
    separately so a schema drift surfaces instead of silently shrinking the
    mix. Returns (counts: {int: int}, skipped: int).
    """
    counts = {}
    skipped = 0
    for t in txs:
        if isinstance(t, dict) and "tx_type" in t and t["tx_type"] is not None:
            tt = int(t["tx_type"])
            counts[tt] = counts.get(tt, 0) + 1
        else:
            skipped += 1
    return counts, skipped


def merge_tx_counts(into, more):
    """Accumulate one count dict into another (in place); returns `into`."""
    for k, v in more.items():
        into[k] = into.get(k, 0) + v
    return into


def tx_mix_proportions(counts):
    """{tx_type: count} -> sorted list of (tx_type, name, count, fraction).

    Sorted by descending count then ascending tx_type for a stable order.
    fraction is count/total (0.0 if total is 0).
    """
    total = sum(counts.values())
    rows = []
    for tt in sorted(counts, key=lambda k: (-counts[k], k)):
        frac = counts[tt] / total if total else 0.0
        rows.append((tt, tx_type_name(tt), counts[tt], frac))
    return rows


def render_tx_mix(counts, blocks, source, label):
    """Render a tx-type mix table. `label` MUST honestly state sample size,
    e.g. 'sample-size-1 (one block)' or 'window: heights A-B, N blocks'."""
    total = sum(counts.values())
    lines = []
    lines.append("=" * 60)
    lines.append("TX-TYPE MIX  (issue #128 — tx-mix gap)")
    lines.append("=" * 60)
    lines.append(f"source : {source}")
    lines.append(f"label  : {label}")
    lines.append(f"blocks : {blocks}   txs: {total:,}")
    lines.append("-" * 60)
    lines.append(f"{'tx_type':>7}  {'name':<10}  {'count':>10}  {'share':>8}")
    for tt, name, cnt, frac in tx_mix_proportions(counts):
        lines.append(f"{tt:>7}  {name:<10}  {cnt:>10,}  {frac * 100:>7.2f}%")
    lines.append("=" * 60)
    return "\n".join(lines)


def replay_schedule(expanded, speed, base_ts_ms, loop_seam_ms=None,
                    duration_s=None):
    """Yield scheduled events {"ts_ms","height","tx_count"}.

    speed > 1 compresses gaps (out_gap = src_gap / speed). When
    loop_seam_ms is set, iterations repeat forever (bounded by
    duration_s), separated by the seam gap (P3); heights are offset per
    iteration to preserve strict monotonicity (spec §5).
    """
    t0 = expanded[0]["ts_ms"]
    src_span = expanded[-1]["ts_ms"] - t0
    h0, h_last = expanded[0]["height"], expanded[-1]["height"]
    height_stride = h_last - h0 + 1
    limit_ms = None if duration_s is None else duration_s * 1000.0
    iteration = 0
    while True:
        iter_off_out = (iteration * (src_span / speed + loop_seam_ms)
                        if loop_seam_ms is not None else 0.0)
        for ev in expanded:
            rel_out = (ev["ts_ms"] - t0) / speed + iter_off_out
            if limit_ms is not None and rel_out > limit_ms:
                return
            yield {"ts_ms": base_ts_ms + int(round(rel_out)),
                   "height": ev["height"] + iteration * height_stride,
                   "tx_count": ev["tx_count"]}
        if loop_seam_ms is None:
            return
        iteration += 1


def synth_schedule(rate, duration_s, base_ts_ms):
    """Back-to-back BLOCK_TX_CAP-tx blocks at cadence = cap/rate seconds."""
    cadence_ms = BLOCK_TX_CAP / rate * 1000.0
    n = math.floor(duration_s * 1000.0 / cadence_ms) + 1
    for i in range(n):
        yield {"ts_ms": base_ts_ms + int(round(i * cadence_ms)),
               "height": SYNTH_HEIGHT_BASE + i,
               "tx_count": BLOCK_TX_CAP}


# ──────────────────────────────────────────────────────────────────────
# replay
# ──────────────────────────────────────────────────────────────────────

def cmd_replay(args, out=None):
    out = out or sys.stdout
    with open(args.input) as f:
        _header, events, _gaps, no_expand = load_trace(f)
    if not events:
        print("error: trace has no block events", file=sys.stderr)
        return 1
    validate_events(events)
    expanded = expand_and_fill(events, no_expand)
    fill = int(round(mean_fill_value(events)))
    agg = aggregate_rate(expanded)

    if args.target_rate is not None:
        speed = args.target_rate / agg          # P1 scale factor
        generator = "replay --target-rate"
    else:
        speed = args.speed
        generator = "replay"

    duration_s = parse_duration(args.duration) if args.duration else None
    if args.loop and duration_s is None and args.dry_run:
        print("error: --loop --dry-run requires --duration", file=sys.stderr)
        return 1
    seam_ms = (median_inter_block_gap_ms(expanded) / speed
               if args.loop else None)          # P3, in output time

    params = {"fill": "mean", "fill_value": fill,
              "speed_factor": round(speed, 6),
              "aggregate_rate": round(agg, 1)}
    if args.target_rate is not None:
        params["target_rate"] = args.target_rate
    else:
        params["speed"] = args.speed
    if args.loop:
        params["loop"] = True
        params["seam_ms"] = round(seam_ms, 3)
    if duration_s is not None:
        params["duration_s"] = duration_s
    if args.dry_run:
        params["dry_run"] = True

    print(provenance_line(generator, params, source_trace=args.input),
          file=out, flush=True)

    # Dry-run base = source t0 (deterministic); live base = wall clock.
    base_ts = events[0]["ts_ms"] if args.dry_run else int(time.time() * 1000)
    sched = replay_schedule(expanded, speed, base_ts,
                            loop_seam_ms=seam_ms, duration_s=duration_s)
    start_mono = time.monotonic()
    for ev in sched:
        if not args.dry_run:
            delay = (ev["ts_ms"] - base_ts) / 1000.0 - (
                time.monotonic() - start_mono)
            if delay > 0:
                time.sleep(delay)
        print(json.dumps(ev), file=out, flush=not args.dry_run)
    out.flush()
    return 0


# ──────────────────────────────────────────────────────────────────────
# synth-peak
# ──────────────────────────────────────────────────────────────────────

def cmd_synth_peak(args, out=None):
    out = out or sys.stdout
    duration_s = parse_duration(args.duration)
    params = {"peak_rate": args.rate, "duration_s": duration_s}
    if args.dry_run:
        params["dry_run"] = True
    print(provenance_line("synth-peak", params), file=out, flush=True)
    base_ts = 0 if args.dry_run else int(time.time() * 1000)
    start_mono = time.monotonic()
    for ev in synth_schedule(args.rate, duration_s, base_ts):
        if not args.dry_run:
            delay = (ev["ts_ms"] - base_ts) / 1000.0 - (
                time.monotonic() - start_mono)
            if delay > 0:
                time.sleep(delay)
        print(json.dumps(ev), file=out, flush=not args.dry_run)
    out.flush()
    return 0


# ──────────────────────────────────────────────────────────────────────
# peak-hours
# ──────────────────────────────────────────────────────────────────────

def _extract_buckets(payload):
    """Find [timestamp, tx_count] pairs in the stats response (shape-tolerant)."""
    if isinstance(payload, list):
        candidates = payload
    elif isinstance(payload, dict):
        candidates = None
        for key in ("data", "result", "results", "stats", "tx", "buckets",
                    "items"):
            if isinstance(payload.get(key), list):
                candidates = payload[key]
                break
        if candidates is None:
            for v in payload.values():
                if isinstance(v, list) and v:
                    candidates = v
                    break
        if candidates is None:
            raise ValueError(f"no bucket list in response keys "
                             f"{sorted(payload.keys())}")
    else:
        raise ValueError(f"unexpected response type {type(payload).__name__}")
    buckets = []
    for item in candidates:
        if isinstance(item, (list, tuple)) and len(item) >= 2:
            ts, count = item[0], item[1]
        elif isinstance(item, dict):
            ts = item.get("timestamp", item.get("time", item.get("ts")))
            count = item.get("tx_count", item.get("count",
                             item.get("value", item.get("txs"))))
        else:
            continue
        if ts is None or count is None:
            continue
        buckets.append((float(ts), float(count)))
    if not buckets:
        raise ValueError("stats response contained no parsable buckets")
    return buckets


def cmd_peak_hours(args, out=None):
    out = out or sys.stdout
    import requests  # lazy: keep offline subcommands dependency-free
    try:
        resp = requests.get(STATS_URL, headers={"User-Agent": UA}, timeout=30)
        resp.raise_for_status()
        payload = resp.json()
    except requests.exceptions.HTTPError as e:
        code = e.response.status_code if e.response is not None else "?"
        print(f"error: stats API returned HTTP {code}.", file=sys.stderr)
        if code == 403:
            print("note: the main REST API geo-blocks US IPs (403); the "
                  "explorer API normally does not — this may be a different "
                  "block or an upstream change.", file=sys.stderr)
        return 1
    except requests.exceptions.RequestException as e:
        print(f"error: stats API request failed: {e}", file=sys.stderr)
        return 1
    try:
        buckets = _extract_buckets(payload)
    except ValueError as e:
        print(f"error: could not parse stats response: {e}", file=sys.stderr)
        return 1

    rows = []
    for ts, count in buckets:
        ts_s = ts / 1000.0 if ts > 1e12 else ts   # ms vs s epoch
        rows.append((count / 3600.0, ts_s, count))
    rows.sort(key=lambda r: (-r[0], r[1]))
    top = rows[:args.top]
    print(f"# top {len(top)} hours by tx/s "
          f"({len(rows)} hourly buckets analyzed)", file=out)
    print(f"{'rank':>4}  {'hour_start_utc':<20}  {'tx/s':>10}  "
          f"{'tx_count':>12}", file=out)
    for i, (rate, ts_s, count) in enumerate(top, 1):
        when = datetime.datetime.fromtimestamp(
            ts_s, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%MZ")
        print(f"{i:>4}  {when:<20}  {rate:>10.1f}  {int(count):>12}",
              file=out)
    return 0


# ──────────────────────────────────────────────────────────────────────
# tx-mix  (issue #128 tx-type gap — the instrumentation fallback)
#
# The tx-type MIX is NOT in the trace format ({ts_ms,height,tx_count} only)
# and is NOT served by the explorer (its block endpoints carry block_size /
# total_transactions / markets, no per-tx tx_type). This subcommand captures
# per-block tx_type counts from the only HTTP source that exposes them — the
# zklighter mainnet blockTxs API — over a height window, and aggregates the
# real mix. If that API geo-blocks the caller (HTTP 403, as it does for the
# main REST API), the subcommand says so honestly and exits non-zero rather
# than fabricate a distribution. `--sample-block` reads the in-repo single
# sample block offline (labeled sample-size-1 — a SAMPLE, never the mix).
# ──────────────────────────────────────────────────────────────────────

def _extract_block_txs(payload):
    """Find the per-tx list inside a blockTxs API response (shape-tolerant).

    Returns a list of tx dicts (each expected to carry "tx_type"). Mirrors
    _extract_buckets's defensive style so a minor API shape change doesn't
    silently yield an empty mix.
    """
    if isinstance(payload, list):
        return [t for t in payload if isinstance(t, dict)]
    if isinstance(payload, dict):
        for key in ("txs", "transactions", "blockTxs", "data", "result",
                    "results", "items"):
            v = payload.get(key)
            if isinstance(v, list):
                return [t for t in v if isinstance(t, dict)]
        # Single nested block object?
        for key in ("block", "data", "result"):
            v = payload.get(key)
            if isinstance(v, dict):
                for k2 in ("txs", "transactions"):
                    if isinstance(v.get(k2), list):
                        return [t for t in v[k2] if isinstance(t, dict)]
    raise ValueError(
        f"no tx list in blockTxs response "
        f"(type {type(payload).__name__}"
        f"{', keys ' + str(sorted(payload.keys())) if isinstance(payload, dict) else ''})")


def _fetch_block_txs(requests, height, limit):
    """Fetch all txs for one block from the blockTxs API (paginated by index)."""
    txs = []
    index = 0
    while True:
        r = requests.get(
            TXMIX_BLOCK_URL,
            params={"block_height": height, "index": index, "limit": limit},
            headers={"User-Agent": UA}, timeout=15)
        r.raise_for_status()
        page = _extract_block_txs(r.json())
        if not page:
            break
        txs.extend(page)
        if len(page) < limit:
            break
        index += len(page)
    return txs


def _tx_mix_from_sample(args, out):
    """Offline fallback: the in-repo single sample block (sample-size-1)."""
    import os
    here = os.path.dirname(os.path.abspath(__file__))
    repo_root = os.path.abspath(os.path.join(here, "..", ".."))
    path = args.sample_block if isinstance(args.sample_block, str) \
        else os.path.join(repo_root, "bench", "bench_test.json")
    try:
        with open(path) as f:
            block = json.load(f)
    except OSError as e:
        print(f"error: cannot read sample block {path}: {e}", file=sys.stderr)
        return 1
    txs = block.get("txs") if isinstance(block, dict) else None
    if not isinstance(txs, list):
        print(f"error: {path} has no 'txs' array", file=sys.stderr)
        return 1
    counts, skipped = count_tx_types(txs)
    if skipped:
        print(f"note: {skipped} tx(s) lacked a tx_type field", file=sys.stderr)
    label = (f"sample-size-1 (ONE block, n={sum(counts.values())} txs) — "
             f"a SAMPLE, not the mainnet distribution")
    print(render_tx_mix(counts, blocks=1, source=path, label=label), file=out)
    return 0


def cmd_tx_mix(args, out=None):
    out = out or sys.stdout
    if args.sample_block is not None:
        return _tx_mix_from_sample(args, out)

    import requests  # lazy: keep offline subcommands dependency-free
    # Resolve the height window. With --heights A B we capture [A, B];
    # otherwise --blocks N most-recent blocks ending at the chain tip.
    if args.heights:
        lo, hi = args.heights
        if hi < lo:
            lo, hi = hi, lo
        heights = list(range(lo, hi + 1))
    else:
        try:
            r = requests.get(POLL_URL, headers={"User-Agent": UA}, timeout=15)
            r.raise_for_status()
            body = r.json()
            blocks = body if isinstance(body, list) else body.get("blocks", [])
            tip = max(b.get("block_height") for b in blocks
                      if b.get("block_height") is not None)
        except requests.exceptions.RequestException as e:
            print(f"error: could not resolve chain tip from {POLL_URL}: {e}",
                  file=sys.stderr)
            return 1
        heights = list(range(tip - args.blocks + 1, tip + 1))

    total = {}
    captured_blocks = 0
    captured_heights = []
    for h in heights:
        t0 = time.time()
        try:
            txs = _fetch_block_txs(requests, h, args.page_limit)
        except requests.exceptions.HTTPError as e:
            code = e.response.status_code if e.response is not None else "?"
            print(f"error: blockTxs API returned HTTP {code} for height {h}.",
                  file=sys.stderr)
            if code == 403:
                print(
                    "BLOCKER: the zklighter mainnet API (the only HTTP source "
                    "exposing per-tx tx_type) geo-blocks this IP with 403 — "
                    "the same block the main REST API applies (see peak-hours).\n"
                    "The explorer API does NOT carry tx_type (block endpoints "
                    "return block_size/total_transactions/markets only), so the "
                    "tx-mix cannot be measured from here. Run this command from "
                    "a non-geo-blocked network to capture the real mix, or use "
                    "--sample-block for the in-repo sample-size-1 block.",
                    file=sys.stderr)
            return 2
        except requests.exceptions.RequestException as e:
            print(f"error: blockTxs request failed for height {h}: {e}",
                  file=sys.stderr)
            return 1
        counts, skipped = count_tx_types(txs)
        if skipped:
            print(f"note: height {h}: {skipped} tx(s) lacked tx_type",
                  file=sys.stderr)
        merge_tx_counts(total, counts)
        if txs:
            captured_blocks += 1
            captured_heights.append(h)
        # polite pacing under the per-IP rate limit
        sleep_for = max(0.0, TXMIX_POLL_PERIOD_S - (time.time() - t0))
        if sleep_for and h != heights[-1]:
            time.sleep(sleep_for)

    if not total:
        print("error: no transactions captured in the requested window.",
              file=sys.stderr)
        return 1

    n = sum(total.values())
    win = (f"heights {captured_heights[0]}-{captured_heights[-1]}"
           if captured_heights else "none")
    if captured_blocks < 30:
        label = (f"sample-size-{captured_blocks} ({captured_blocks} blocks, "
                 f"n={n} txs; {win}) — small sample, NOT the full distribution")
    else:
        label = (f"window: {win}, {captured_blocks} blocks, n={n} txs")
    source = f"{TXMIX_BLOCK_URL} (block_height in {win})"
    print(render_tx_mix(total, captured_blocks, source, label), file=out)
    return 0


# ──────────────────────────────────────────────────────────────────────
# record
# ──────────────────────────────────────────────────────────────────────

class TraceWriter:
    """Tee schema-conformant lines to --out and stdout."""

    def __init__(self, path):
        self.f = open(path, "w")

    def emit(self, obj):
        line = json.dumps(obj)
        self.f.write(line + "\n")
        self.f.flush()
        print(line, flush=True)

    def close(self):
        self.f.close()


class RecordState:
    def __init__(self):
        self.buffer = {}        # height -> (server_ts_ms, recv_monotonic)
        self.polled = {}        # height -> block_size
        self.last_emitted_h = None
        self.last_emitted_ts = None
        self.ws_msgs = 0
        self.ws_disconnects = 0
        self.poll_reqs = 0
        self.stop = asyncio.Event()


def _flush_ready(state, writer, watermark_s):
    """Emit buffered WS events older than the watermark, height order.

    tx_count late-binds from the poll dict; unmatched after the
    watermark -> null (filling is replay-time only, P1). Duplicates and
    height regressions from the WS feed are dropped; ts_ms is clamped
    non-decreasing (spec §5).
    """
    now = time.monotonic()
    ready = sorted(h for h, (_ts, recv) in state.buffer.items()
                   if watermark_s == 0 or now - recv >= watermark_s)
    for h in ready:
        ts, _recv = state.buffer.pop(h)
        if state.last_emitted_h is not None and h <= state.last_emitted_h:
            continue
        if state.last_emitted_ts is not None:
            ts = max(ts, state.last_emitted_ts)
        writer.emit({"ts_ms": ts, "height": h,
                     "tx_count": state.polled.pop(h, None)})
        state.last_emitted_h = h
        state.last_emitted_ts = ts
    # Trim polled entries that can never bind anymore.
    if state.last_emitted_h is not None and len(state.polled) > 2048:
        floor = state.last_emitted_h
        state.polled = {h: v for h, v in state.polled.items() if h > floor}


async def _ws_collector(state, writer, stop_at):
    import websockets  # lazy: record is the only WS consumer
    backoff = BACKOFF_INITIAL_S
    while not state.stop.is_set() and time.time() < stop_at:
        try:
            async with websockets.connect(
                    WS_URL, ping_interval=20, ping_timeout=20) as ws:
                await ws.send(json.dumps(
                    {"type": "subscribe", "channel": "height"}))
                backoff = BACKOFF_INITIAL_S
                while not state.stop.is_set() and time.time() < stop_at:
                    remaining = stop_at - time.time()
                    msg = await asyncio.wait_for(
                        ws.recv(),
                        timeout=min(WS_SILENCE_TIMEOUT_S,
                                    max(0.1, remaining)))
                    m = json.loads(msg)
                    if m.get("type") != "update/height":
                        continue
                    state.ws_msgs += 1
                    h = m["height"]
                    # Server timestamp is authoritative for ts_ms.
                    if h not in state.buffer:
                        state.buffer[h] = (m["timestamp"], time.monotonic())
        except asyncio.TimeoutError:
            if state.stop.is_set() or time.time() >= stop_at:
                break
            state.ws_disconnects += 1
            writer.emit({"gap": True, "ts_ms": int(time.time() * 1000),
                         "reason": "ws_reconnect_timeout"})
            print(f"[ws] {WS_SILENCE_TIMEOUT_S:.0f}s silence -> reconnect",
                  file=sys.stderr, flush=True)
        except asyncio.CancelledError:
            raise
        except Exception as e:
            if state.stop.is_set() or time.time() >= stop_at:
                break
            state.ws_disconnects += 1
            writer.emit({"gap": True, "ts_ms": int(time.time() * 1000),
                         "reason": "ws_disconnect"})
            print(f"[ws] disconnect ({type(e).__name__}: {e}), "
                  f"backoff {backoff:.0f}s", file=sys.stderr, flush=True)
            try:
                await asyncio.wait_for(state.stop.wait(), timeout=backoff)
                break  # stop requested during backoff
            except asyncio.TimeoutError:
                pass
            backoff = min(backoff * 2, BACKOFF_CAP_S)


async def _poll_collector(state, stop_at):
    import requests  # lazy

    def fetch_once():
        r = requests.get(POLL_URL, headers={"User-Agent": UA}, timeout=10)
        r.raise_for_status()
        return r.json()

    while not state.stop.is_set() and time.time() < stop_at:
        t0 = time.time()
        try:
            body = await asyncio.to_thread(fetch_once)
            for b in body if isinstance(body, list) else body.get("blocks",
                                                                  []):
                h, sz = b.get("block_height"), b.get("block_size")
                if h is not None and sz is not None:
                    state.polled[h] = sz
        except asyncio.CancelledError:
            raise
        except Exception as e:
            print(f"[poll] {type(e).__name__}: {e}", file=sys.stderr,
                  flush=True)
        state.poll_reqs += 1
        sleep_for = max(0.0, POLL_PERIOD_S - (time.time() - t0))
        try:
            await asyncio.wait_for(state.stop.wait(), timeout=sleep_for)
        except asyncio.TimeoutError:
            pass


async def _flusher(state, writer, stop_at):
    while not state.stop.is_set() and time.time() < stop_at:
        _flush_ready(state, writer, WATERMARK_S)
        try:
            await asyncio.wait_for(state.stop.wait(), timeout=0.25)
        except asyncio.TimeoutError:
            pass


async def _record_async(args):
    duration_s = parse_duration(args.duration) if args.duration else None
    stop_at = time.time() + duration_s if duration_s else float("inf")
    state = RecordState()
    writer = TraceWriter(args.out)

    params = {"endpoint": WS_URL}
    if duration_s is not None:
        params["duration_s"] = duration_s
    params["poll_url"] = POLL_URL
    params["watermark_s"] = WATERMARK_S
    writer.emit({"provenance": {"generator": "record", "params": params,
                                "generated_at": now_iso()}})

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, state.stop.set)

    try:
        await asyncio.gather(
            _ws_collector(state, writer, stop_at),
            _poll_collector(state, stop_at),
            _flusher(state, writer, stop_at),
        )
    finally:
        # SIGINT-safe finalize: flush everything still buffered (watermark
        # waived — whatever tx_counts have arrived bind now, rest -> null).
        _flush_ready(state, writer, watermark_s=0)
        writer.close()
        print(f"[record] done: ws_msgs={state.ws_msgs} "
              f"disconnects={state.ws_disconnects} "
              f"poll_reqs={state.poll_reqs} -> {args.out}",
              file=sys.stderr, flush=True)
    return 0


def cmd_record(args):
    try:
        return asyncio.run(_record_async(args))
    except KeyboardInterrupt:
        return 0


# ──────────────────────────────────────────────────────────────────────
# CLI
# ──────────────────────────────────────────────────────────────────────

def positive_float(text):
    """argparse type: strictly positive float (rates, speeds)."""
    v = float(text)
    if v <= 0:
        raise argparse.ArgumentTypeError(f"must be > 0, got {text}")
    return v


def build_parser():
    p = argparse.ArgumentParser(
        prog="feeder.py",
        description="Trace producer for the streaming bench "
                    "(contract: bench/trace-format.md)")
    sub = p.add_subparsers(dest="command", required=True)

    pr = sub.add_parser("record", help="capture live chain cadence (network)")
    pr.add_argument("--out", required=True, help="output trace path (JSONL)")
    pr.add_argument("--duration", help="capture duration (e.g. 900s, 15m); "
                                       "default: until SIGINT")
    pr.set_defaults(func=cmd_record)

    pp = sub.add_parser("replay",
                        help="re-emit a recorded trace retimed (offline)")
    pp.add_argument("--in", dest="input", required=True,
                    help="input trace path (JSONL)")
    g = pp.add_mutually_exclusive_group()
    g.add_argument("--speed", type=positive_float, default=1.0,
                   help="speed multiplier (2 = twice as fast; default 1)")
    g.add_argument("--target-rate", type=positive_float, default=None,
                   help="scale so the aggregate rate (P1) hits this tx/s")
    pp.add_argument("--loop", action="store_true",
                    help="repeat the trace; seam per policy P3")
    pp.add_argument("--duration",
                    help="stop emitting after this much output time "
                         "(e.g. 15m, 900s)")
    pp.add_argument("--dry-run", action="store_true",
                    help="print the emission schedule without sleeping")
    pp.set_defaults(func=cmd_replay)

    ps = sub.add_parser("synth-peak",
                        help="fabricate idealized trace from a rate "
                             "(offline, no inputs)")
    ps.add_argument("--rate", type=positive_float, required=True,
                    help="target tx/s (cadence = 500/rate seconds)")
    ps.add_argument("--duration", required=True,
                    help="trace duration (e.g. 15m, 900s)")
    ps.add_argument("--dry-run", action="store_true",
                    help="print the emission schedule without sleeping")
    ps.set_defaults(func=cmd_synth_peak)

    ph = sub.add_parser("peak-hours",
                        help="locate peak windows (analysis helper)")
    ph.add_argument("--top", type=int, default=10,
                    help="number of top hours to report (default 10)")
    ph.set_defaults(func=cmd_peak_hours)

    pm = sub.add_parser(
        "tx-mix",
        help="capture per-block tx_type counts -> the mainnet tx-mix (#128)")
    pm.add_argument("--blocks", type=int, default=200,
                    help="capture the N most-recent blocks (default 200)")
    pm.add_argument("--heights", type=int, nargs=2, metavar=("LO", "HI"),
                    default=None,
                    help="capture an explicit inclusive height range LO HI")
    pm.add_argument("--page-limit", type=int, default=100,
                    help="blockTxs pagination page size (default 100)")
    pm.add_argument("--sample-block", nargs="?", const=True, default=None,
                    metavar="PATH",
                    help="offline: count tx_type in the in-repo sample block "
                         "(sample-size-1); optional PATH overrides the default")
    pm.set_defaults(func=cmd_tx_mix)
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except BrokenPipeError:
        # Downstream consumer (head, a closing bench, ...) went away:
        # normal termination for a stream producer.
        import os
        try:
            os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        except OSError:
            pass
        return 0


if __name__ == "__main__":
    sys.exit(main())
