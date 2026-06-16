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
import os
import random
import signal
import statistics
import sys
import time

# Sized-block sampler for synth-peak (issue #220). Imported eagerly: the
# module is stdlib-only and adds no runtime cost on the constant-tx_count
# back-compat path; co-locating with feeder.py keeps the producer pipeline
# in one place.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import size_distributions  # noqa: E402

WS_URL = "wss://mainnet.zklighter.elliot.ai/stream?readonly=true"
POLL_URL = "https://explorer.elliot.ai/api/blocks"
STATS_URL = "https://explorer.elliot.ai/api/stats/tx?aggregation_period=1h"
UA = "lighter-prover-feeder/48 (research; single conn; <=85 req/min)"

# tx-MIX capture (issue #128 tx-type gap). The explorer's block endpoints
# carry only block_size / total_transactions — NO per-tx tx_type field — so
# the only HTTP source that exposes tx_type per transaction is the zklighter
# mainnet API's blockTxs endpoint. (Confirmed by probing: explorer
# /api/blocks/{h} returns {total_transactions, markets, logs} only.)
# That API geo-blocks some IPs with HTTP 403 (observed: US regions); Tokyo /
# ap-northeast is normally NOT geo-blocked, so the operable answer is to run
# this capture from there. The tool cannot change its own egress IP, so it
# (a) reports a 403 honestly with crisp Tokyo guidance rather than inventing
# numbers, and (b) exposes config knobs (--base-url / --proxy / env) so an
# operator can point it at a Tokyo egress without code edits.
TXMIX_BLOCK_URL = "https://mainnet.zklighter.elliot.ai/api/v1/blockTxs"

# Rate-limit hardening (the explicit maintainer requirement: the tool must
# be a well-behaved client that cannot accidentally hammer the endpoint).
# Conservative defaults keep us well under the documented 90 req/min per-IP
# limit; all are CLI/env-overridable. Backoff honors Retry-After / 429.
TXMIX_MAX_RPM = 80          # cap requests/min (-> min-interval below)
TXMIX_BACKOFF_INITIAL_S = 1.0
TXMIX_BACKOFF_CAP_S = 60.0  # ceiling for a single backoff sleep
TXMIX_MAX_RETRIES = 5       # per-request retries on 429/transient before fail
TXMIX_RETRY_AFTER_CAP_S = 120.0  # honor Retry-After but never sleep forever

# Region/egress config knobs (issue #128 follow-up). The tool runs from
# wherever it is invoked and cannot change its IP from inside the process;
# these let an operator point it at a Tokyo egress without code edits.
#   LIGHTER_TXMIX_BASE_URL  alternate blockTxs base URL (e.g. a Tokyo egress)
#   LIGHTER_EGRESS_PROXY    egress proxy (falls back to HTTPS_PROXY)
#   LIGHTER_REGION          a label recorded in output for citation hygiene
ENV_TXMIX_BASE_URL = "LIGHTER_TXMIX_BASE_URL"
ENV_EGRESS_PROXY = "LIGHTER_EGRESS_PROXY"
ENV_REGION = "LIGHTER_REGION"

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

# ── Native Pub/Sub publisher bridge (issue #211) ──────────────────────
# Per-publish timeout for the native google-cloud-pubsub publisher. The bridge
# fails LOUDLY (not silently) on a missed deadline so a publisher bottleneck
# corrupts the pacing report instead of the benchmark's throughput number
# (issue #211 requirement 5: honest backpressure).
PUBLISH_TIMEOUT_S = 30.0


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


def synth_schedule(duration_s, base_ts_ms, *, tx_count=BLOCK_TX_CAP,
                   block_rate):
    """Fabricate a sustained synthetic block stream on TWO independent axes.

    Load is configured by two orthogonal knobs (issue #217):
      * block_rate -- blocks/sec; sets the cadence between blocks
        (cadence_ms = 1000 / block_rate), INDEPENDENT of tx_count.
      * tx_count   -- txns per block; every block carries exactly this many
        transactions (constant -> no height jumps, no nulls).

    Decoupling these axes is what lets synth-peak drive a fixed-k stream:
    the coordinator splits k = ceil(tx_count / S), so a constant tx_count
    pins k regardless of how fast blocks arrive. (The legacy single-axis
    form coupled cadence to tx/s with tx_count fixed at BLOCK_TX_CAP, so
    every block was 500 tx -> k = 56 only.)

    tx_count is capped at BLOCK_TX_CAP (the chain per-block tx cap, spec
    §6.2); callers validate the 1..BLOCK_TX_CAP range before calling.
    """
    cadence_ms = 1000.0 / block_rate
    n = math.floor(duration_s * 1000.0 / cadence_ms) + 1
    for i in range(n):
        yield {"ts_ms": base_ts_ms + int(round(i * cadence_ms)),
               "height": SYNTH_HEIGHT_BASE + i,
               "tx_count": int(tx_count)}


def synth_schedule_sampled(duration_s, base_ts_ms, *, block_rate, sampler):
    """Same shape as synth_schedule, but tx_count per block is drawn from
    `sampler.sample()` (issue #220 — third axis: per-block size).

    Cadence math identical to synth_schedule (so --block-rate semantics
    are preserved exactly); heights are SYNTH_HEIGHT_BASE + i strictly
    increasing (no P2 mean-fill expansion). The yielded shape
    {ts_ms, height, tx_count} is identical so the publisher bridge
    (publish_scheduled_events) consumes it unchanged.

    Determinism: `sampler` carries its own injected random.Random; the
    same seed + invocation produces a byte-identical stream every run.
    """
    cadence_ms = 1000.0 / block_rate
    n = math.floor(duration_s * 1000.0 / cadence_ms) + 1
    for i in range(n):
        yield {"ts_ms": base_ts_ms + int(round(i * cadence_ms)),
               "height": SYNTH_HEIGHT_BASE + i,
               "tx_count": int(sampler.sample())}


# ──────────────────────────────────────────────────────────────────────
# Native Pub/Sub publisher bridge (issue #211)
#
# The feeder produces a paced {ts_ms, height, tx_count} stream; the
# distributed coordinator consumes BlockMessage {height, tx_count} from a
# Pub/Sub dispatch topic. The seam between them used to be a shell pipe
# (xargs gcloud pubsub publish), which spawns a ~1-2 s gcloud process per
# message — the publisher becomes the bottleneck before the prover does, and
# at the measured mainnet rates (mean 11.08 blk/s, p99 25 blk/s, 1 s rolling
# peak 41 blk/s; see #128) pacing fidelity collapses. The bridge below uses
# the NATIVE google-cloud-pubsub Python client: one persistent gRPC
# connection, in-process auth, no per-publish process spawn.
#
# Mirrors the discipline of #205 / pubsub_native.rs (the merge-task plane's
# native streaming-pull client) on the publish side. Publisher-side is much
# simpler than consumer-side: no manual ack, no streaming-pull state.
#
# Honest-failure contract (issue #211 requirement 5): a publish that fails
# or times out RAISES; we never silently drop a block message and corrupt
# the stream's pacing. The benchmark would rather fail visibly than report
# an inflated throughput number.
#
# Honest-pacing contract (issue #211 requirement 2): every publish records
# the wall-clock drift between its scheduled time and its actual publish
# time. At end of run we report median/p95/p99/max drift on stderr so any
# future pacing degradation is loud, not silent.
# ──────────────────────────────────────────────────────────────────────


def block_message_payload(ev):
    """Project a scheduled feeder event to the wire BlockMessage payload.

    The dispatch plane carries only `{height, tx_count}` (the coordinator's
    `BlockMessage` shape; ts_ms is wall-clock and is reconstructed from
    publish/receive time at the consumer). Synthetic-flag and other
    feeder-internal fields are dropped here so the wire payload is
    byte-for-byte equivalent to what a manual `gcloud pubsub publish` of
    `{height, tx_count}` would have sent. Pure, offline-testable.
    """
    return {"height": int(ev["height"]), "tx_count": int(ev["tx_count"])}


def encode_block_message(ev):
    """Serialize a scheduled feeder event to the wire bytes (UTF-8 JSON).

    Pub/Sub messages are arbitrary bytes; the coordinator parses UTF-8 JSON
    with the BlockMessage fields. Kept tiny and pure so tests can assert the
    on-the-wire payload without a real client.
    """
    return json.dumps(block_message_payload(ev),
                      separators=(",", ":")).encode("utf-8")


class PacingReport:
    """Accumulate per-event scheduled-vs-actual drift; report at end.

    Drift = actual_publish_time - scheduled_time (ms). Positive = late
    (publisher fell behind the schedule). We track ABSOLUTE drift for the
    tail summary (the SLO question is "how far off the schedule did we
    get", in either direction) and a separate count of late events.

    Pure (sleep/clock injectable from the caller's loop) — no hidden global
    state; tests can drive it with a deterministic clock.
    """

    def __init__(self):
        self.drifts_ms = []   # signed: actual - scheduled, in ms
        self.published = 0
        self.late_count = 0   # events where actual > scheduled

    def record(self, scheduled_ms, actual_ms):
        d = actual_ms - scheduled_ms
        self.drifts_ms.append(d)
        self.published += 1
        if d > 0:
            self.late_count += 1

    def summary(self):
        """Return a dict of the pacing statistics (mediums, tails, late%)."""
        n = len(self.drifts_ms)
        if n == 0:
            return {"published": 0}
        absd = [abs(d) for d in self.drifts_ms]
        absd_sorted = sorted(absd)

        def pct(p):
            # nearest-rank percentile; small-sample correct for our use
            if n == 1:
                return absd_sorted[0]
            idx = max(0, min(n - 1, int(round(p / 100.0 * (n - 1)))))
            return absd_sorted[idx]

        return {
            "published": n,
            "late_count": self.late_count,
            "late_fraction": self.late_count / n,
            "abs_drift_ms_p50": pct(50),
            "abs_drift_ms_p95": pct(95),
            "abs_drift_ms_p99": pct(99),
            "abs_drift_ms_max": absd_sorted[-1],
            "signed_drift_ms_mean": sum(self.drifts_ms) / n,
        }

    def render(self):
        """Human + machine readable pacing summary (single line + table)."""
        s = self.summary()
        if s.get("published", 0) == 0:
            return "pacing: no events published."
        lines = []
        lines.append(f"pacing: published {s['published']} events; "
                     f"late={s['late_count']} "
                     f"({s['late_fraction'] * 100:.2f}%)")
        lines.append(
            f"  abs drift (ms): p50={s['abs_drift_ms_p50']:.1f} "
            f"p95={s['abs_drift_ms_p95']:.1f} "
            f"p99={s['abs_drift_ms_p99']:.1f} "
            f"max={s['abs_drift_ms_max']:.1f}")
        lines.append(
            f"  signed mean drift (ms): "
            f"{s['signed_drift_ms_mean']:+.1f} "
            "(positive = behind schedule)")
        return "\n".join(lines)


class PublisherBridge:
    """Native Pub/Sub publisher: persistent client, paced real-time
    publish, drift tracking, honest backpressure.

    `publisher` and `topic_path` are injected so the unit tests use a fake
    publisher (no real GCP); `sleep` and `clock` are injected so pacing
    arithmetic is testable with a deterministic clock.

    Backpressure contract: every publish blocks until the message is
    accepted by the server (`future.result(timeout=...)`); a server error
    or timeout RAISES, the loop above does NOT swallow it, the run fails
    loudly with the pacing report so far on stderr.
    """

    def __init__(self, publisher, topic_path, *,
                 timeout_s=PUBLISH_TIMEOUT_S,
                 sleep=time.sleep, clock=time.monotonic):
        self.publisher = publisher
        self.topic_path = topic_path
        self.timeout_s = timeout_s
        self._sleep = sleep
        self._clock = clock
        self.report = PacingReport()

    def publish_one(self, payload_bytes, scheduled_offset_ms, base_clock):
        """Publish `payload_bytes` aiming at `scheduled_offset_ms` from
        `base_clock`. Blocks until the server accepts. Records pacing
        drift. Raises (loudly) on publish failure or timeout.

        `base_clock` is the monotonic-clock value at run start; the
        scheduled wall-clock instant is `base_clock + scheduled_offset/1000`.
        Sleeping to that instant uses the injected sleep so tests can run
        instantly. Drift is measured AFTER the server accepts the publish
        — the SLO-relevant quantity (when did the message actually arrive
        on the wire, not just when did we hand it off to a library buffer).
        """
        now = self._clock()
        elapsed_ms = (now - base_clock) * 1000.0
        delay_ms = scheduled_offset_ms - elapsed_ms
        if delay_ms > 0:
            self._sleep(delay_ms / 1000.0)
        future = self.publisher.publish(self.topic_path, payload_bytes)
        # Block on server accept; raises on transport/server error or timeout.
        future.result(timeout=self.timeout_s)
        actual_ms = (self._clock() - base_clock) * 1000.0
        self.report.record(scheduled_offset_ms, actual_ms)


def _import_pubsub():
    """Lazy import google.cloud.pubsub_v1 with a crisp error on missing dep.

    Mirrors the lazy-import pattern other network subcommands use; keeps
    replay/synth-peak/the offline test suite stdlib-only when --publish-to
    is not used."""
    try:
        from google.cloud import pubsub_v1  # noqa: PLC0415
    except ImportError as e:
        raise SystemExit(
            "error: --publish-to requires the 'google-cloud-pubsub' Python "
            "package.\n"
            "  Install: pip install -r bench/feeder/requirements.txt\n"
            f"  (import error: {e})")
    return pubsub_v1


def build_publisher_bridge(project, topic, *, pubsub_v1=None,
                           sleep=time.sleep, clock=time.monotonic):
    """Construct a PublisherBridge wired to a real Pub/Sub publisher.

    Resolved as a single function so tests can stub `pubsub_v1` and the
    real path stays a thin wiring layer. Honors no extra batch knobs —
    publisher-side defaults are fine for the rates we target (mean ~11
    blk/s, peak ~41); we keep the seam tight + predictable.
    """
    if pubsub_v1 is None:
        pubsub_v1 = _import_pubsub()
    publisher = pubsub_v1.PublisherClient()
    topic_path = publisher.topic_path(project, topic)
    return PublisherBridge(publisher, topic_path, sleep=sleep, clock=clock)


def publish_scheduled_events(bridge, events, *, base_clock=None,
                             progress=None):
    """Drive a bridge over an iterable of scheduled events.

    Each event must carry `ts_ms`, `height`, `tx_count`. The first event's
    `ts_ms` defines the schedule's zero — every later event's
    `ts_ms - first.ts_ms` is its scheduled offset from `base_clock` (so
    real-time pacing is preserved exactly as the schedule said). The
    ts_ms-based timeline is the feeder's own (`replay_schedule` /
    `synth_schedule`), so this honors issue #211 requirement 2 byte-for-byte.

    `progress` is an optional callable invoked after each successful
    publish (for stderr heartbeat); kept injectable so tests stay silent.

    Raises (does NOT swallow) on any publish failure or timeout. Returns
    the bridge so the caller can render the pacing report.
    """
    first_ts_ms = None
    if base_clock is None:
        base_clock = bridge._clock()
    for ev in events:
        if first_ts_ms is None:
            first_ts_ms = ev["ts_ms"]
        scheduled_offset_ms = ev["ts_ms"] - first_ts_ms
        payload = encode_block_message(ev)
        bridge.publish_one(payload, scheduled_offset_ms, base_clock)
        if progress is not None:
            progress(ev)
    return bridge


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

    # --publish-to wires the schedule into a native Pub/Sub publisher
    # instead of stdout JSONL. The provenance header (which carries the
    # measured rate/speed/fill) is still written to `out` so the run is
    # cited the same way; the per-event JSONL is replaced by Pub/Sub
    # messages whose payload is `{height, tx_count}` (see issue #211).
    publish_to = getattr(args, "publish_to", None)
    if publish_to:
        if args.dry_run:
            print("error: --dry-run and --publish-to are mutually exclusive",
                  file=sys.stderr)
            return 1
        project = getattr(args, "project", None)
        if not project:
            print("error: --publish-to requires --project <id>",
                  file=sys.stderr)
            return 1
        params["publish_to"] = {"project": project, "topic": publish_to}

    print(provenance_line(generator, params, source_trace=args.input),
          file=out, flush=True)

    # Dry-run base = source t0 (deterministic); live base = wall clock.
    base_ts = events[0]["ts_ms"] if args.dry_run else int(time.time() * 1000)
    sched = replay_schedule(expanded, speed, base_ts,
                            loop_seam_ms=seam_ms, duration_s=duration_s)
    if publish_to:
        bridge = build_publisher_bridge(project, publish_to)
        try:
            publish_scheduled_events(bridge, sched)
        finally:
            print(bridge.report.render(), file=sys.stderr, flush=True)
        return 0
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

def _render_realized_histogram(hist, total):
    """Single-line stderr summary of the realized per-band counts.

    Mirrors the PacingReport end-of-run pattern: machine-greppable token
    (REALIZED_HISTOGRAM) + key=value pairs in canonical band order.
    """
    parts = " ".join(f"{name}={hist[name]}"
                     for name in size_distributions.BAND_NAMES)
    return f"REALIZED_HISTOGRAM {parts} total={total}"


def cmd_synth_peak(args, out=None):
    out = out or sys.stdout
    duration_s = parse_duration(args.duration)

    # ── Sampled-size axis (issue #220) ─────────────────────────────────
    # Resolve the optional size-distribution sampler. When set, --tx-count is
    # ignored (mutex enforced below) and tx_count per block is drawn from
    # `sampler.sample()`. The sampler carries its own random.Random(seed) so
    # the stream is byte-deterministic given --seed.
    size_distribution = getattr(args, "size_distribution", None)
    size_dist_file = getattr(args, "size_dist_file", None)
    seed = getattr(args, "seed", None)
    histogram_out = getattr(args, "histogram_out", None)
    def _usage_error(msg):
        # Match argparse's usage-error exit code (2) and stderr-channel
        # message so the caller cannot distinguish runtime vs syntax
        # validation failures. Raises SystemExit(2).
        print(f"error: {msg}", file=sys.stderr)
        raise SystemExit(2)

    sampler = None
    if size_distribution is not None or size_dist_file is not None:
        # Mutex validation already done at parse time for size_distribution
        # vs size_dist_file; here we enforce the cross-axis rules.
        if args.tx_count is not None:
            _usage_error(
                "--tx-count is mutually exclusive with --size-distribution "
                "/ --size-dist-file (a sampler sets tx_count per block)")
        if args.rate is not None:
            _usage_error(
                "--rate is undefined with a sampled size distribution "
                "(the aggregate-tx/s axis assumes a constant tx_count); "
                "use --block-rate B with --size-distribution")
        if args.block_rate is None:
            _usage_error(
                "--block-rate is required with --size-distribution "
                "/ --size-dist-file")
        if seed is None:
            _usage_error(
                "--seed is required with --size-distribution "
                "/ --size-dist-file (determinism contract)")
        rng = random.Random(seed)
        if size_distribution == "bimodal":
            sampler = size_distributions.bimodal_mainnet_sampler(rng)
        elif size_dist_file is not None:
            # SystemExit from the loader is caught by main()'s argparse layer;
            # propagate so the CLI exits 1 with the loader's clear message.
            sampler = size_distributions.sampler_from_file(size_dist_file, rng)
    elif seed is not None:
        # Seed without a sampler: warn (matches the plan's "ignored otherwise
        # with a stderr warning" contract) — the stream stays back-compat.
        print("warning: --seed is ignored without --size-distribution "
              "/ --size-dist-file", file=sys.stderr)

    # Two independent load axes (issue #217). Cadence comes from --block-rate
    # directly; if only --rate (aggregate tx/s) is given, derive the block
    # rate as rate / tx_count so the default tx_count=500 reproduces the
    # legacy cadence = 500/rate exactly (back-compat).
    # tx_count defaults to BLOCK_TX_CAP when --tx-count is omitted; the
    # explicit-None sentinel is what the sampler-vs-constant mutex check
    # above relies on (so it MUST be filled in below the mutex, not at
    # argparse-default time).
    tx_count = args.tx_count if args.tx_count is not None else BLOCK_TX_CAP
    if args.block_rate is not None:
        block_rate = args.block_rate
    else:
        block_rate = args.rate / tx_count

    params = {"block_rate": block_rate, "tx_count": tx_count,
              "duration_s": duration_s}
    if sampler is not None:
        # When sampling, tx_count varies per block — recording the constant
        # default would be misleading. Drop it; the sampler config is the
        # truthful provenance.
        params.pop("tx_count", None)
        params["size_distribution"] = sampler.name
        params["seed"] = int(seed)
        params["sampler_bands"] = sampler.band_weights()
    if args.rate is not None:
        params["peak_rate"] = args.rate  # legacy axis, when --rate was used
    if args.dry_run:
        params["dry_run"] = True

    publish_to = getattr(args, "publish_to", None)
    if publish_to:
        if args.dry_run:
            print("error: --dry-run and --publish-to are mutually exclusive",
                  file=sys.stderr)
            return 1
        project = getattr(args, "project", None)
        if not project:
            print("error: --publish-to requires --project <id>",
                  file=sys.stderr)
            return 1
        params["publish_to"] = {"project": project, "topic": publish_to}

    print(provenance_line("synth-peak", params), file=out, flush=True)
    base_ts = 0 if args.dry_run else int(time.time() * 1000)
    if sampler is not None:
        sched = synth_schedule_sampled(
            duration_s, base_ts, block_rate=block_rate, sampler=sampler)
    else:
        sched = synth_schedule(duration_s, base_ts,
                               tx_count=tx_count, block_rate=block_rate)

    # Wrap the iterator in an accumulator that records every emitted tx_count
    # so the realized histogram can be emitted at end of run (success OR
    # exception). The list lives in the enclosing scope so `finally` blocks
    # can render even when the publisher raises.
    realized_tx_counts = []

    def _track(events):
        for ev in events:
            realized_tx_counts.append(int(ev["tx_count"]))
            yield ev
    sched = _track(sched)

    def _emit_histogram():
        """End-of-run histogram emission (stderr + optional sidecar JSON)."""
        if sampler is None:
            return
        hist = size_distributions.realized_histogram(realized_tx_counts)
        total = len(realized_tx_counts)
        print(_render_realized_histogram(hist, total),
              file=sys.stderr, flush=True)
        if histogram_out is not None:
            sidecar = {
                "sampler": {
                    "name": sampler.name,
                    "bands": sampler.band_weights(),
                },
                "seed": int(seed),
                "realized": hist,
                "n_blocks": total,
            }
            with open(histogram_out, "w") as hf:
                json.dump(sidecar, hf, indent=2, sort_keys=True)
                hf.write("\n")

    if publish_to:
        bridge = build_publisher_bridge(project, publish_to)
        try:
            publish_scheduled_events(bridge, sched)
        finally:
            print(bridge.report.render(), file=sys.stderr, flush=True)
            _emit_histogram()
        return 0
    try:
        start_mono = time.monotonic()
        for ev in sched:
            if not args.dry_run:
                delay = (ev["ts_ms"] - base_ts) / 1000.0 - (
                    time.monotonic() - start_mono)
                if delay > 0:
                    time.sleep(delay)
            print(json.dumps(ev), file=out, flush=not args.dry_run)
        out.flush()
    finally:
        _emit_histogram()
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

# ── region / egress config resolution (no hardcoded US assumptions) ──────
# Precedence everywhere: explicit CLI value > environment variable > default.
# None of these change the process's egress IP — they let an operator point
# the tool at a Tokyo egress (alternate base URL and/or proxy) without code
# edits. Pure + offline-testable (env passed in so tests need no monkeypatch
# of os.environ globally).

def resolve_base_url(cli_value=None, env=None):
    """Resolve the blockTxs base URL: --base-url > env > TXMIX_BLOCK_URL."""
    if cli_value:
        return cli_value
    env = os.environ if env is None else env
    return env.get(ENV_TXMIX_BASE_URL) or TXMIX_BLOCK_URL


def resolve_proxies(cli_value=None, env=None):
    """Resolve an egress proxy -> a requests `proxies` dict, or None.

    Precedence: --proxy > LIGHTER_EGRESS_PROXY > HTTPS_PROXY/https_proxy.
    A single URL is applied to both http and https (requests honors the
    scheme of the target). Returns None when no proxy is configured.
    """
    env = os.environ if env is None else env
    proxy = (cli_value or env.get(ENV_EGRESS_PROXY)
             or env.get("HTTPS_PROXY") or env.get("https_proxy"))
    if not proxy:
        return None
    return {"http": proxy, "https": proxy}


def resolve_region(cli_value=None, env=None):
    """Resolve a human region label for citation hygiene (or None).

    This is purely a label recorded in the output's source line so a
    captured mix cites WHERE it was captured from; it does not influence
    routing. Precedence: --region > LIGHTER_REGION.
    """
    if cli_value:
        return cli_value
    env = os.environ if env is None else env
    return env.get(ENV_REGION) or None


def min_interval_from_rpm(max_rpm):
    """Requests/min cap -> minimum seconds between requests (>= 0)."""
    if max_rpm is None or max_rpm <= 0:
        return 0.0
    return 60.0 / float(max_rpm)


def geo_block_guidance(base_url):
    """Crisp, actionable 403 guidance (centralized so tests stay in sync).

    The endpoint geo-blocks some regions (observed: US). The operable
    answer is to run the capture from Tokyo / ap-northeast (not geo-blocked)
    — or point the tool at a Tokyo egress via --base-url / --proxy.
    """
    return (
        "BLOCKER: this endpoint geo-blocks some regions (observed: US) with "
        "HTTP 403.\n"
        f"  endpoint: {base_url}\n"
        "The zklighter mainnet blockTxs API is the ONLY HTTP source exposing "
        "per-tx tx_type; the explorer API does NOT carry tx_type (its block "
        "endpoints return block_size/total_transactions/markets only), so the "
        "tx-mix cannot be measured from a geo-blocked IP.\n"
        "FIX — run from Tokyo / ap-northeast (normally NOT geo-blocked):\n"
        "  * provision a small GCP VM in asia-northeast1 (or an equivalent "
        "non-US egress) and run this `tx-mix` command there; or\n"
        "  * point this tool at a Tokyo egress without moving it: set "
        "--base-url <tokyo-egress-url> and/or --proxy <tokyo-proxy> "
        "(env: LIGHTER_TXMIX_BASE_URL / LIGHTER_EGRESS_PROXY).\n"
        "Or use --sample-block for the in-repo sample-size-1 block (a SAMPLE, "
        "never presented as the mainnet mix)."
    )


class RateLimiter:
    """A minimal, well-behaved client pacer: enforce a minimum interval
    between requests so the tool cannot accidentally hammer the endpoint.

    `sleep` is injectable so tests assert pacing without real waits. Uses a
    monotonic clock; `clock` is injectable for the same reason.
    """

    def __init__(self, min_interval_s, sleep=time.sleep, clock=time.monotonic):
        self.min_interval_s = max(0.0, float(min_interval_s))
        self._sleep = sleep
        self._clock = clock
        self._last = None

    def wait(self):
        """Block (via the injected sleep) until min_interval has elapsed
        since the previous call. First call returns immediately."""
        if self.min_interval_s <= 0:
            self._last = self._clock()
            return 0.0
        now = self._clock()
        if self._last is not None:
            elapsed = now - self._last
            remaining = self.min_interval_s - elapsed
            if remaining > 0:
                self._sleep(remaining)
                now = self._clock()
        self._last = now
        return 0.0


def _parse_retry_after(value, cap_s=TXMIX_RETRY_AFTER_CAP_S):
    """Parse a Retry-After header value (delta-seconds form) -> capped float.

    Only the integer/float delta-seconds form is honored (the HTTP-date form
    is uncommon for rate limiting here); unparseable -> None so the caller
    falls back to exponential backoff. Always capped so we never sleep
    unboundedly on a hostile/misconfigured header.
    """
    if value is None:
        return None
    try:
        secs = float(str(value).strip())
    except (TypeError, ValueError):
        return None
    if secs < 0:
        return None
    return min(secs, cap_s)


def _backoff_delay(attempt, initial=TXMIX_BACKOFF_INITIAL_S,
                   cap=TXMIX_BACKOFF_CAP_S):
    """Exponential backoff for retry `attempt` (0-based), capped. Pure."""
    return min(cap, initial * (2 ** attempt))


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


class TxMixHTTP:
    """Bundles the resolved network config for a tx-mix capture run:
    base URL, optional egress proxy, a RateLimiter (min-interval pacing),
    and the backoff/retry parameters. Injectable `requests`/`sleep` keep
    the retry+backoff paths fully unit-testable offline (no real network,
    no real waits)."""

    def __init__(self, requests, base_url, proxies=None, limiter=None,
                 sleep=time.sleep, max_retries=TXMIX_MAX_RETRIES,
                 backoff_initial=TXMIX_BACKOFF_INITIAL_S,
                 backoff_cap=TXMIX_BACKOFF_CAP_S,
                 retry_after_cap=TXMIX_RETRY_AFTER_CAP_S):
        self.requests = requests
        self.base_url = base_url
        self.proxies = proxies
        self.limiter = limiter or RateLimiter(0.0, sleep=sleep)
        self.sleep = sleep
        self.max_retries = max_retries
        self.backoff_initial = backoff_initial
        self.backoff_cap = backoff_cap
        self.retry_after_cap = retry_after_cap


def _http_get_with_retry(http, url, params):
    """A well-behaved GET: rate-limited, honors Retry-After / 429, and
    backs off exponentially on transient errors (429 + 5xx + connection).

    - Paces every attempt through the RateLimiter (cannot hammer).
    - On HTTP 429: sleeps Retry-After (capped) if present, else exponential
      backoff, then retries — never tight-loops.
    - On HTTP 5xx / connection errors: exponential backoff then retry.
    - On HTTP 403 (geo-block) or other 4xx: raises immediately — these are
      NOT transient, retrying would just hammer a hard rejection.
    - Gives up after max_retries, re-raising the last error.
    """
    requests = http.requests
    last_exc = None
    for attempt in range(http.max_retries + 1):
        http.limiter.wait()
        try:
            r = requests.get(
                url, params=params,
                headers={"User-Agent": UA},
                proxies=http.proxies, timeout=15)
        except requests.exceptions.RequestException as e:
            # Connection-level transient: back off and retry.
            last_exc = e
            if attempt >= http.max_retries:
                raise
            http.sleep(_backoff_delay(attempt, http.backoff_initial,
                                      http.backoff_cap))
            continue

        status = getattr(r, "status_code", None)
        if status == 429:
            # Rate limited: respect Retry-After, else exponential backoff.
            retry_after = _parse_retry_after(
                r.headers.get("Retry-After") if getattr(r, "headers", None)
                else None, http.retry_after_cap)
            last_exc = requests.exceptions.HTTPError(
                "429 Too Many Requests", response=r)
            if attempt >= http.max_retries:
                r.raise_for_status()
            delay = (retry_after if retry_after is not None
                     else _backoff_delay(attempt, http.backoff_initial,
                                         http.backoff_cap))
            http.sleep(delay)
            continue
        if status is not None and 500 <= status < 600:
            # Server-side transient: back off and retry.
            last_exc = requests.exceptions.HTTPError(
                f"{status} server error", response=r)
            if attempt >= http.max_retries:
                r.raise_for_status()
            http.sleep(_backoff_delay(attempt, http.backoff_initial,
                                      http.backoff_cap))
            continue

        # 403 (geo-block) and any other 4xx fall through to raise_for_status,
        # which raises HTTPError immediately — NOT retried (not transient).
        r.raise_for_status()
        return r
    # Exhausted retries without returning (defensive; loop normally raises).
    if last_exc is not None:
        raise last_exc
    raise RuntimeError("unreachable: retry loop exited without result")


def _fetch_block_txs(http, height, limit):
    """Fetch all txs for one block from the blockTxs API (paginated by index).

    `http` is a TxMixHTTP carrying the resolved base URL, proxy, rate
    limiter, and retry policy — so every page fetch is paced + backoff-aware.
    """
    txs = []
    index = 0
    while True:
        r = _http_get_with_retry(
            http, http.base_url,
            {"block_height": height, "index": index, "limit": limit})
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


def _build_tx_mix_http(args, requests):
    """Resolve region/egress config + rate policy into a TxMixHTTP.

    Honors --base-url/--proxy/--region (and their env fallbacks) and the
    rate knobs (--max-rpm/--max-retries). Conservative defaults keep us a
    well-behaved client well under the per-IP limit.
    """
    base_url = resolve_base_url(getattr(args, "base_url", None))
    proxies = resolve_proxies(getattr(args, "proxy", None))
    max_rpm = getattr(args, "max_rpm", None)
    max_rpm = TXMIX_MAX_RPM if max_rpm is None else max_rpm
    limiter = RateLimiter(min_interval_from_rpm(max_rpm))
    max_retries = getattr(args, "max_retries", None)
    max_retries = TXMIX_MAX_RETRIES if max_retries is None else max_retries
    return TxMixHTTP(requests, base_url, proxies=proxies, limiter=limiter,
                     max_retries=max_retries)


def cmd_tx_mix(args, out=None):
    out = out or sys.stdout
    if args.sample_block is not None:
        return _tx_mix_from_sample(args, out)

    import requests  # lazy: keep offline subcommands dependency-free
    http = _build_tx_mix_http(args, requests)
    region = resolve_region(getattr(args, "region", None))

    # Resolve the height window. With --heights A B we capture [A, B];
    # otherwise --blocks N most-recent blocks ending at the chain tip.
    if args.heights:
        lo, hi = args.heights
        if hi < lo:
            lo, hi = hi, lo
        heights = list(range(lo, hi + 1))
    else:
        try:
            r = requests.get(POLL_URL, headers={"User-Agent": UA},
                             proxies=http.proxies, timeout=15)
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
        try:
            txs = _fetch_block_txs(http, h, args.page_limit)
        except requests.exceptions.HTTPError as e:
            code = e.response.status_code if e.response is not None else "?"
            print(f"error: blockTxs API returned HTTP {code} for height {h}.",
                  file=sys.stderr)
            if code == 403:
                print(geo_block_guidance(http.base_url), file=sys.stderr)
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
    # Citation hygiene: record WHERE the capture egressed from (region label
    # and the actual base URL hit) so a captured mix cites its provenance.
    region_tag = f"; region={region}" if region else ""
    source = f"{http.base_url} (block_height in {win}{region_tag})"
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


def block_tx_count(text):
    """argparse type: txns/block in 1..BLOCK_TX_CAP (the chain per-block cap)."""
    v = int(text)
    if v < 1 or v > BLOCK_TX_CAP:
        raise argparse.ArgumentTypeError(
            f"must be in 1..{BLOCK_TX_CAP} (chain per-block tx cap), got {text}")
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
    # Native Pub/Sub publisher bridge (#211): when both flags are set the
    # scheduled stream is published to the dispatch topic instead of stdout,
    # using the in-process google-cloud-pubsub client (NOT a per-message
    # `gcloud pubsub publish` shell-out — the bottleneck this replaces).
    pp.add_argument("--publish-to", default=None, metavar="TOPIC",
                    help="dispatch topic name to publish to via the native "
                         "Pub/Sub client (replaces stdout JSONL); requires "
                         "--project")
    pp.add_argument("--project", default=None, metavar="PROJECT_ID",
                    help="GCP project for --publish-to")
    pp.set_defaults(func=cmd_replay)

    ps = sub.add_parser("synth-peak",
                        help="fabricate idealized trace on two load axes "
                             "(blocks/sec + txns/block; offline, no inputs)")
    # Two independent load axes (issue #217):
    #   --block-rate (blocks/sec) sets cadence; --tx-count sets txns/block.
    # The rate axis is mutually exclusive: pick the block cadence directly
    # (--block-rate) OR specify an aggregate tx/s budget (--rate) that is
    # split across blocks of size --tx-count. At least one is required.
    rate_axis = ps.add_mutually_exclusive_group(required=True)
    rate_axis.add_argument("--block-rate", type=positive_float, default=None,
                           metavar="B",
                           help="blocks/sec; cadence = 1000/B ms, independent "
                                "of --tx-count")
    rate_axis.add_argument("--rate", type=positive_float, default=None,
                           help="aggregate target tx/s; cadence derived as "
                                "tx_count/rate seconds (back-compat: with the "
                                "default tx_count=500 this is 500/rate)")
    ps.add_argument("--tx-count", "--block-size", dest="tx_count",
                    type=block_tx_count, default=None, metavar="N",
                    help=f"txns per block, 1..{BLOCK_TX_CAP} "
                         f"(default {BLOCK_TX_CAP}); constant -> pins "
                         "k = ceil(N/S) for a fixed-k stream. Mutually "
                         "exclusive with --size-distribution / "
                         "--size-dist-file (a sampler sets tx_count per block)")
    ps.add_argument("--duration", required=True,
                    help="trace duration (e.g. 15m, 900s)")
    ps.add_argument("--dry-run", action="store_true",
                    help="print the emission schedule without sleeping")
    # ── Sampled per-block size distribution (issue #220) ────────────────
    # Third axis (block size) sampled from a seeded distribution; defaults
    # to the documented #212 mainnet bimodal mix when --size-distribution
    # bimodal is set. Mutually exclusive with --tx-count. Requires --seed
    # (determinism contract: same seed -> byte-identical stream).
    size_axis = ps.add_mutually_exclusive_group()
    size_axis.add_argument(
        "--size-distribution", choices=["bimodal"], default=None,
        metavar="NAME",
        help="sample tx_count per block from a named distribution "
             "(currently: 'bimodal' = the #212 mainnet 7-band mix). "
             "Requires --seed; mutually exclusive with --tx-count and "
             "--size-dist-file")
    size_axis.add_argument(
        "--size-dist-file", default=None, metavar="PATH",
        help="sample tx_count per block from a JSON sampler config file "
             "(schema: {name, bands:[{lo,hi,weight,representative?}]}). "
             "Requires --seed; mutually exclusive with --tx-count and "
             "--size-distribution")
    ps.add_argument("--seed", type=int, default=None, metavar="N",
                    help="RNG seed for sampled distributions (required with "
                         "--size-distribution / --size-dist-file; ignored "
                         "otherwise with a warning)")
    ps.add_argument("--histogram-out", default=None, metavar="PATH",
                    help="write the realized per-band histogram + sampler "
                         "config to PATH as JSON (audit sidecar). Stderr "
                         "summary line is always emitted when a sampler is in "
                         "use, regardless of this flag")
    # Native Pub/Sub publisher bridge (#211); see replay subcommand for shape.
    ps.add_argument("--publish-to", default=None, metavar="TOPIC",
                    help="dispatch topic name to publish to via the native "
                         "Pub/Sub client (replaces stdout JSONL); requires "
                         "--project")
    ps.add_argument("--project", default=None, metavar="PROJECT_ID",
                    help="GCP project for --publish-to")
    ps.set_defaults(func=cmd_synth_peak)

    ph = sub.add_parser("peak-hours",
                        help="locate peak windows (analysis helper)")
    ph.add_argument("--top", type=int, default=10,
                    help="number of top hours to report (default 10)")
    ph.set_defaults(func=cmd_peak_hours)

    pm = sub.add_parser(
        "tx-mix",
        help="capture per-block tx_type counts -> the mainnet tx-mix (#128)",
        description=(
            "Capture the mainnet tx-type mix from the zklighter blockTxs API "
            "(the only HTTP source exposing per-tx tx_type). That endpoint "
            "geo-blocks some regions (observed: US) with HTTP 403; run this "
            "from Tokyo / ap-northeast (normally NOT geo-blocked) — e.g. a GCP "
            "VM in asia-northeast1 — or point it at a Tokyo egress with "
            "--base-url / --proxy. On 403 it fails honestly with this guidance "
            "rather than fabricating a mix. See bench/README.md for the full "
            "Tokyo run recipe."))
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
    # Region / egress config (no hardcoded US assumptions; cannot change the
    # process IP — these point the tool at a Tokyo egress without code edits).
    pm.add_argument("--base-url", default=None, metavar="URL",
                    help="alternate blockTxs base URL, e.g. a Tokyo egress "
                         f"(env {ENV_TXMIX_BASE_URL}; "
                         f"default {TXMIX_BLOCK_URL})")
    pm.add_argument("--proxy", default=None, metavar="URL",
                    help="egress proxy URL to route requests through, e.g. a "
                         f"Tokyo proxy (env {ENV_EGRESS_PROXY}, else "
                         "HTTPS_PROXY)")
    pm.add_argument("--region", default=None, metavar="LABEL",
                    help="region label recorded in the output for citation "
                         f"hygiene (env {ENV_REGION}); does not affect routing")
    # Rate-limit hardening (well-behaved client; conservative defaults).
    pm.add_argument("--max-rpm", type=positive_float, default=TXMIX_MAX_RPM,
                    metavar="N",
                    help="max requests/min — paces every call so the endpoint "
                         f"is never hammered (default {TXMIX_MAX_RPM}, under "
                         "the 90/min per-IP limit)")
    pm.add_argument("--max-retries", type=int, default=TXMIX_MAX_RETRIES,
                    metavar="N",
                    help="retries on 429/transient errors before failing; "
                         "honors Retry-After + exponential backoff "
                         f"(default {TXMIX_MAX_RETRIES})")
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
