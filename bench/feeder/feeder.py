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

Only the network subcommands (record, peak-hours) need third-party deps
(websockets, requests — see requirements.txt); they are imported lazily so
replay/synth-peak/tests run on a bare Python 3 stdlib.

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
    g.add_argument("--speed", type=float, default=1.0,
                   help="speed multiplier (2 = twice as fast; default 1)")
    g.add_argument("--target-rate", type=float, default=None,
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
    ps.add_argument("--rate", type=float, required=True,
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
