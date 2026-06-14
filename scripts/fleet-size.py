#!/usr/bin/env python3
"""fleet-size.py -- the parametric fleet-sizing model (issue #95).

Given a demand level `l = (blocks/s, tx/block)`, a config
`(S, M, segments, fold mode)`, and the MEASURED single-machine constants
loaded directly from `calibration/<shape>.json`, this model emits the
infrastructure SIZE and SHAPE:

  - N chunk-prover worker cells   (Little's law over the cell pool)
  - M coordinators                (a SEPARATE machine class -- never summed)
  - RAM per class                 (from measured peak_rss_mb)
  - segment-folder topology       (block-grain fold-tree depth)
  - headroom vs the lag SLO       (p50 <= LAG_P50; default 20 s)

This is the SOLVER for the governing equation decided in ADR-0004 §0:

    lag_p50(c, l) <= 20 s  AND  lag_p99(c, l) <= 40 s,  at l >= 5 blocks/s

where c is capacity COUNTED BY CLASS (a worker cell is not a coordinator;
they are NEVER summed into one pool -- ADR-0004 §6.2). The model reads the
equation the three ways ADR-0004 §4 names:

  (a) HOLD lag at the bound, solve for c given l   -> the sizing answer
  (b) HOLD c, read out lag                          -> the lag readout
  (c) PUSH l to peak, verify the bound still holds  -> the headroom check

================================================================
COST DISCIPLINE (Discussion #77 standing norm, hard requirement)
================================================================
The model's PRIMARY OUTPUT is MACHINES + TOPOLOGY -- size and shape, NO
dollar sign. Cost is "final-validation-only, never a design constraint or
gate" (Discussion #77). It appears ONLY as an OPTIONAL `--cost-overlay`
that multiplies the already-decided machine counts by a price. The overlay
is labeled REPORTING-ONLY / NON-GATING everywhere it appears. Cost does
NOT enter any sizing decision, any feasibility verdict, or any branch in
this file. (Search this file: every sizing path runs to completion BEFORE
any price is read, and no price ever changes N, M, RAM, or the verdict.)

================================================================
HONESTY / CITATION (Discussion #58/#77 norms, hard requirement)
================================================================
Every constant this model consumes is loaded from a real, versioned
artifact (`calibration/<shape>.json`) and reported with its path + key +
value. The model does NOT invent numbers.

This is the IDEALIZED projection from SINGLE-MACHINE constants. It is the
k=1 lower bound of the true cross-cell lag (ADR-0004 §3.3). The following
are explicitly UNMODELED -- named here, never fabricated:

  - witness_move            (#61; no witness_fetch_ms in code yet)
  - contention / scaling losses on a running distributed system (G4; only
    measurable once the conductor #75 exists)
  - realistic-data effects  (G2; synthetic blocks not generated yet)
  - coordinator-failure recovery latency (#75; no coordinator exists yet)
  - L6 batch-finalization wrapper gate (#83; block-proof lag excludes it)
  - the straggler/recovery p99 tail coefficient (#101; needs wider than
    the n=3 per-chunk variance sample)

Folding G4 contention + G2 realism into this model is the refinement that
turns "idealized" into "confident" (north-star #144, goal G5). Until then
this is an idealized projection, stated plainly in every report it emits.

Self-consistency check (the worked example, also a golden test):
  c4a-highcpu-64 @ S=9, 9000-tx central path reproduces the committed
  `single_machine_wall_9000 = 8.730` in calibration/c4a-highcpu-64.json
  (S=9 row) to the millisecond:
    3.051 (L1) + ceil(log2(1000))*0.2751 (merge) + 2.928 (L4) = 8.730

Math reused VERBATIM from scripts/s-calibrate-report.py (the single code
path for merge_depth / single_machine_wall): see merge_depth() below.
"""

import argparse
import json
import math
import os
import sys

# ── repo geometry ──────────────────────────────────────────────────────
THIS_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(THIS_DIR)
CALIB_DIR = os.path.join(REPO_ROOT, "calibration")

# The governing equation's block-proof lag bound (ADR-0004 §0, decided in
# Discussion #77). Overridable so the model is parametric, but these are
# the committed defaults baked into calibration/*.json (lag_p50_s/lag_p99_s).
DEFAULT_LAG_P50_S = 20.0
DEFAULT_LAG_P99_S = 40.0

# Default demand floor the governing equation must hold at (ADR-0004 §0):
# "sustained at l >= 5 blocks/s". Used as the default peak in reading (c).
DEFAULT_PEAK_BLOCKS_PER_S = 5.0


def merge_depth(block_tx: int, s: int) -> int:
    """Number of pairwise merge levels to fold ceil(B/S) leaf chain proofs
    into one root proof. Reused VERBATIM from
    scripts/s-calibrate-report.py:merge_depth (the single site encoding the
    always-split policy's pairwise/2-ary merge arity). If merges ever go
    >2-ary, change the log base there and here together.
    """
    chunks = math.ceil(block_tx / s)
    return math.ceil(math.log2(chunks)) if chunks > 1 else 0


def chunks_per_block(block_tx: int, s: int) -> int:
    """k = ceil(B / S): the always-split chunk count for one block."""
    return math.ceil(block_tx / s)


# ── constant loading (CITATION discipline: path + key + value) ─────────
class Constants:
    """The measured single-machine constants for one shape, loaded from
    calibration/<shape>.json. Every field carries the JSON path + key it
    came from so the model can self-document its provenance.
    """

    def __init__(self, shape, path, l1_wall_s, merge_s, l4_wall_s,
                 peak_rss_mb, s, label, citations):
        self.shape = shape
        self.path = path
        self.l1_wall_s = l1_wall_s        # L1 chunk prove wall (per chunk)
        self.merge_s = merge_s            # L2 merge-tree step
        self.l4_wall_s = l4_wall_s        # block prove (serial, coordinator)
        self.peak_rss_mb = peak_rss_mb    # measured per-cell RAM envelope
        self.s = s                        # the S row these came from
        self.label = label                # "measured" / "extrapolated"
        self.citations = citations        # list of (key, value, source) tuples

    @property
    def rel_path(self):
        return os.path.relpath(self.path, REPO_ROOT)


def load_constants(shape: str, s: int) -> Constants:
    """Load measured constants for `shape` at chunk size `s` from
    calibration/<shape>.json. Raises with an honest message if the shape
    or the S row is absent -- we do NOT extrapolate or invent here.
    """
    path = os.path.join(CALIB_DIR, f"{shape}.json")
    if not os.path.isfile(path):
        available = sorted(
            os.path.splitext(f)[0]
            for f in os.listdir(CALIB_DIR)
            if f.endswith(".json")
        )
        raise SystemExit(
            f"fleet-size: no calibration artifact for shape '{shape}' "
            f"(looked for {os.path.relpath(path, REPO_ROOT)}).\n"
            f"  Available shapes: {', '.join(available)}\n"
            f"  This model consumes ONLY measured constants -- it will not "
            f"invent a row for an unmeasured shape."
        )
    with open(path) as fh:
        entry = json.load(fh)

    merge_s = entry["constants"]["merge_s"]["value"]
    merge_label = entry["constants"]["merge_s"]["label"]
    l4_wall_s = entry["constants"]["l4_wall_s"]["value"]
    l4_label = entry["constants"]["l4_wall_s"]["label"]

    row = next((r for r in entry["per_s_table"] if int(r["S"]) == s), None)
    if row is None:
        measured_s = [r["S"] for r in entry["per_s_table"]]
        raise SystemExit(
            f"fleet-size: shape '{shape}' has no measured row for S={s} "
            f"(measured S values: {measured_s}).\n"
            f"  No extrapolation -- pick an S that this shape actually "
            f"measured, or calibrate S={s} first (make s-calibrate)."
        )

    l1_wall_s = float(row["l1_wall_ms"]) / 1000.0
    peak_rss_mb = float(row["peak_rss_mb"])
    row_label = row.get("label", "measured")

    # Citation ledger: every consumed value, with its exact JSON location.
    rel = os.path.relpath(path, REPO_ROOT)
    citations = [
        (f"per_s_table[S={s}].l1_wall_ms", row["l1_wall_ms"],
         f"{rel} (-> l1_wall_s={l1_wall_s:.3f}s, {row_label})"),
        ("constants.merge_s.value", merge_s,
         f"{rel} (merge_s={merge_s}s, {merge_label})"),
        ("constants.l4_wall_s.value", l4_wall_s,
         f"{rel} (l4_wall_s={l4_wall_s}s, {l4_label})"),
        (f"per_s_table[S={s}].peak_rss_mb", row["peak_rss_mb"],
         f"{rel} (peak_rss_mb={peak_rss_mb:.0f}MB, {row_label})"),
    ]
    # Overall label is "measured" only if every consumed term is measured.
    label = "measured" if (merge_label == "measured"
                           and l4_label == "measured"
                           and row_label == "measured") else "extrapolated"
    return Constants(shape, path, l1_wall_s, merge_s, l4_wall_s,
                     peak_rss_mb, s, label, citations)


# ── the lag function (ADR-0004 §3.1 central path; k=1 lower bound) ──────
def central_path_lag_s(c: Constants, block_tx: int) -> dict:
    """Per-block central-path lag (L1->L4), ADR-0004 §3.1/§3.3.

        per_block_lag ~= max_over_chunks(L1)          (parallel across cells)
                       + ceil(log2(k)) * merge_step    (L2 tree fold)
                       + L4                            (serial, on coordinator)
                       [+ witness_move = UNMODELED]

    This is the SINGLE-MACHINE lower bound (the perfect-parallel,
    zero-transport, zero-straggler floor; ADR-0004 §3.3). The true
    cross-cell lag is this PLUS UNMODELED witness_move, PLUS real transport
    (~0.02% of prove, #97 -- negligible but real), PLUS the §3.4 tail. We
    return the floor and name what is omitted.
    """
    k = chunks_per_block(block_tx, c.s)
    depth = merge_depth(block_tx, c.s)
    l1 = c.l1_wall_s
    merge = depth * c.merge_s
    l4 = c.l4_wall_s
    total = l1 + merge + l4
    return {
        "k": k,
        "merge_depth": depth,
        "l1_s": l1,
        "merge_s_total": merge,
        "l4_s": l4,
        "central_path_s": total,
        # UNMODELED additive terms (named, not invented):
        "witness_move_s": None,     # #61
        "straggler_tail_s": None,   # #101
        "transport_s": None,        # ~0.02% of prove, #97 -- negligible
    }


# ── reading (a): HOLD lag -> solve for c (the sizing answer) ───────────
def size_fleet(c: Constants, blocks_per_s: float, block_tx: int,
               coordinators_per_block_concurrency: float,
               lag_p50_s: float) -> dict:
    """Solve lag(c, l) = bound for c, by class. Two SEPARATE pools.

    WORKER CELLS (chunk grain): throughput is Little's law over the cell
    pool, independent of per-block lag (ADR-0004 §4.1):

        cells ~= arrival_rate_of_chunks * chunk_service_time / 1
               = (blocks/s * k) * L1_wall

    i.e. the number of chunk-proves in flight at steady state (each cell
    proves one chunk at a time; one chunk in flight per busy cell).

    COORDINATORS (block grain): a coordinator folds the L2 merge tree +
    L4 -- ~one block's worth of serial proving per block (ADR-0004 §6.2).
    Its service time is (merge_tree + L4); arrival is blocks/s:

        coordinators ~= blocks/s * (merge_tree_s + L4_s)

    These two pools are sized SEPARATELY and returned SEPARATELY. They are
    NEVER summed -- a worker cell is not a coordinator (ADR-0004 §0/§6.2).
    """
    lag = central_path_lag_s(c, block_tx)
    k = lag["k"]

    # --- WORKER CELL pool (chunk grain) ---
    chunk_arrival_rate = blocks_per_s * k          # chunks/s offered
    chunk_service_time = c.l1_wall_s               # s per chunk per cell
    cells_raw = chunk_arrival_rate * chunk_service_time
    cells = math.ceil(cells_raw)

    # --- COORDINATOR pool (block grain) -- SEPARATE class ---
    coord_service_time = lag["merge_s_total"] + lag["l4_s"]   # s per block
    coords_raw = (blocks_per_s * coord_service_time
                  / max(coordinators_per_block_concurrency, 1.0))
    coordinators = max(1, math.ceil(coords_raw))

    # --- RAM (per class, from measured envelope) ---
    # Worker cell RAM = measured peak_rss_mb at this S (proving-key resident).
    cell_rss_mb = c.peak_rss_mb
    # Coordinator holds merge + L4 proving keys resident. We do NOT have a
    # separately-measured coordinator RSS envelope in the registry, so we
    # report the worker-cell envelope as a documented PROXY and name the
    # coordinator-specific RSS as UNMODELED (do not invent a number).
    coord_rss_mb = c.peak_rss_mb  # PROXY: see coord_rss_unmodeled below

    # --- headroom vs the SLO ---
    slack_s = lag_p50_s - lag["central_path_s"]
    verdict = ("FEASIBLE" if slack_s >= 2.0
               else "MARGINAL" if slack_s >= 0.0
               else "INFEASIBLE")

    return {
        "lag": lag,
        # WORKER CELL class
        "worker_cells": cells,
        "worker_cells_raw": cells_raw,
        "chunk_arrival_rate": chunk_arrival_rate,
        "chunk_service_time_s": chunk_service_time,
        "cell_rss_mb": cell_rss_mb,
        "cell_rss_gb": cell_rss_mb / 1024.0,
        "fleet_cell_rss_gb": cells * cell_rss_mb / 1024.0,
        # COORDINATOR class (SEPARATE -- never summed with worker_cells)
        "coordinators": coordinators,
        "coordinators_raw": coords_raw,
        "coord_service_time_s": coord_service_time,
        "coord_rss_mb_proxy": coord_rss_mb,
        "coord_rss_unmodeled": True,   # coordinator-specific RSS not measured
        # SLO headroom
        "slack_s": slack_s,
        "verdict": verdict,
    }


# ── reading (c): segment-folder topology (block grain, ADR-0004 §5) ────
def segment_folder_topology(blocks_per_batch: int) -> dict:
    """Block-grain fold tree shape (ADR-0004 §2/§5): folding N block proofs
    into one batch proof is the SAME recursive primitive at the block grain.
    The segment-fold tree depth is ceil(log2(blocks_per_batch)); the number
    of pairwise merge nodes is (blocks_per_batch - 1).

    NOTE: this is TOPOLOGY ONLY (shape, not timing). The L5/L6 batch-finalize
    WALL is a SEPARATE cadence on EPYC-only constants and terminates in the
    UNMODELED L6 gate (#83) -- ADR-0004 §5. We deliberately do NOT fold any
    batch-finalize time into the block-proof lag.
    """
    n = max(1, blocks_per_batch)
    depth = math.ceil(math.log2(n)) if n > 1 else 0
    return {
        "blocks_per_batch": n,
        "segment_fold_depth": depth,
        "segment_fold_nodes": max(0, n - 1),
        "l5_l6_wall": None,  # UNMODELED here -- separate cadence, #83 L6 gate
    }


# ── rendering ──────────────────────────────────────────────────────────
def fmt_s(x):
    return f"{x:.3f} s" if x is not None else "UNMODELED"


def render_report(c, blocks_per_s, block_tx, segments, fold_mode,
                  coord_concurrency, lag_p50_s, lag_p99_s,
                  sizing, topo, cost_overlay):
    """Human-readable size + shape report. Cost (if any) is appended LAST,
    fenced and labeled NON-GATING, after every sizing decision is final.
    """
    lag = sizing["lag"]
    out = []
    A = out.append
    A("=" * 70)
    A("FLEET-SIZING MODEL -- output is SIZE + SHAPE (machines + topology)")
    A("=" * 70)
    A("")
    A(f"  shape (machine class basis) : {c.shape}")
    A(f"  constants label             : {c.label}")
    A(f"  S (chunk size)              : {c.s}")
    A(f"  fold mode                   : {fold_mode}")
    A("")
    A("  DEMAND (l):")
    A(f"    blocks/s   : {blocks_per_s}")
    A(f"    tx/block   : {block_tx}")
    A(f"    -> k chunks/block = ceil({block_tx}/{c.s}) = {lag['k']}")
    A("")
    A("  GOVERNING EQUATION (ADR-0004 §0): lag_p50 <= "
      f"{lag_p50_s} s, lag_p99 <= {lag_p99_s} s")
    A("")
    A("  ---- (b) HOLD c, READ lag : per-block central path (L1->L4) ----")
    A(f"    L1 max-over-chunks  : {fmt_s(lag['l1_s'])}")
    A(f"    L2 merge tree       : {fmt_s(lag['merge_s_total'])}  "
      f"(depth ceil(log2({lag['k']})) = {lag['merge_depth']})")
    A(f"    L4 block prove      : {fmt_s(lag['l4_s'])}  (serial, coordinator)")
    A(f"    witness_move        : {fmt_s(lag['witness_move_s'])}  "
      f"(#61 -- omitted from the sum)")
    A(f"    => central path     : {fmt_s(lag['central_path_s'])}  "
      f"(k=1 single-machine LOWER BOUND; ADR-0004 §3.3)")
    A(f"    SLO slack vs p50    : {sizing['slack_s']:+.3f} s   "
      f"verdict: {sizing['verdict']}")
    A("")
    A("  ---- (a) HOLD lag, SOLVE for c : the fleet size, BY CLASS ----")
    A("  Two SEPARATE machine classes (ADR-0004 §6.2) -- NEVER summed.")
    A("")
    A("  [class 1] CHUNK-PROVER WORKER CELLS (Little's law, chunk grain):")
    A(f"    chunk arrival rate  : {blocks_per_s} blk/s * {lag['k']} chunks "
      f"= {sizing['chunk_arrival_rate']:.1f} chunks/s")
    A(f"    chunk service time  : {sizing['chunk_service_time_s']:.3f} s/chunk")
    A(f"    cells (raw)         : {sizing['worker_cells_raw']:.1f}")
    A(f"    => N worker cells   : {sizing['worker_cells']}")
    A(f"    RAM / cell          : {sizing['cell_rss_mb']:.0f} MB "
      f"({sizing['cell_rss_gb']:.1f} GiB, measured peak_rss_mb)")
    A(f"    fleet RAM (cells)   : {sizing['fleet_cell_rss_gb']:.0f} GiB")
    A("")
    A("  [class 2] COORDINATORS (block grain) -- SEPARATE class:")
    A(f"    coord service time  : {sizing['coord_service_time_s']:.3f} s/block "
      f"(merge tree + L4)")
    A(f"    coord concurrency   : {coord_concurrency} block(s)/coordinator")
    A(f"    coordinators (raw)  : {sizing['coordinators_raw']:.2f}")
    A(f"    => M coordinators   : {sizing['coordinators']}")
    A(f"    RAM / coordinator   : {sizing['coord_rss_mb_proxy']:.0f} MB "
      f"(PROXY = worker envelope; coordinator-specific RSS UNMODELED)")
    A("")
    A("  !! N worker cells and M coordinators are DISTINCT pools. The model")
    A("     NEVER reports a single summed machine count (ADR-0004 §0/§6.2).")
    A("")
    A("  ---- SEGMENT-FOLDER TOPOLOGY (block grain, ADR-0004 §5) ----")
    A(f"    blocks/batch        : {topo['blocks_per_batch']}")
    A(f"    segment-fold depth  : {topo['segment_fold_depth']} "
      f"(ceil(log2(blocks/batch)))")
    A(f"    segment-fold nodes  : {topo['segment_fold_nodes']} (pairwise merges)")
    A(f"    L5/L6 batch wall    : UNMODELED here (separate cadence; L6 gate "
      f"#83 -- ADR-0004 §5)")
    A("")
    A("  ---- (c) PUSH l to peak, VERIFY the bound ----")
    A(f"    at l = {blocks_per_s} blocks/s, {block_tx} tx/block: central path "
      f"{lag['central_path_s']:.3f} s vs p50 {lag_p50_s} s")
    A(f"    => {sizing['slack_s']:+.3f} s of p50 slack "
      f"({'bound HOLDS' if sizing['slack_s'] >= 0 else 'bound VIOLATED'} "
      f"on the central path)")
    A("    The slack is the ENTIRE budget for the UNMODELED tail (straggler")
    A("    max-of-k #101 + coordinator recovery #75 + witness_move #61).")
    A("")
    A("  ---- CONSTANT CITATIONS (every value -> real artifact) ----")
    for key, val, src in c.citations:
        A(f"    {key} = {val}  <-  {src}")
    A("")
    A("  ---- UNMODELED (named, NOT invented) ----")
    A("    - witness_move ................. #61 (no witness_fetch_ms in code)")
    A("    - contention / scaling losses .. G4 (needs running system #75)")
    A("    - realistic-data effects ....... G2 (synthetic blocks unbuilt)")
    A("    - coordinator recovery latency . #75 (no coordinator exists yet)")
    A("    - coordinator-specific RSS ..... unmeasured (worker envelope = PROXY)")
    A("    - p99 straggler tail coeff ..... #101 (n=3 variance too thin)")
    A("    - L6 batch-finalize wrapper .... #83 (excluded from block-proof lag)")
    A("")
    A("  This is the IDEALIZED single-machine projection (the k=1 lower")
    A("  bound). Folding in G4 contention + G2 realism is what turns")
    A("  'idealized' into 'confident' (north-star #144, goal G5).")
    A("")
    if cost_overlay is not None:
        A("  " + "-" * 64)
        A("  COST OVERLAY -- REPORTING ONLY, NON-GATING (Discussion #77).")
        A("  Cost did NOT shape any number above. It is a post-hoc")
        A("  multiply, shown for final-validation reference only.")
        A("  " + "-" * 64)
        price = cost_overlay
        cell_cost = sizing["worker_cells"] * price
        coord_cost = sizing["coordinators"] * price
        A(f"    price (per machine-hour, you supplied) : {price}")
        A(f"    worker-cell pool : {sizing['worker_cells']} x {price} "
          f"= {cell_cost:.2f} /hr  [reporting-only]")
        A(f"    coordinator pool : {sizing['coordinators']} x {price} "
          f"= {coord_cost:.2f} /hr  [reporting-only]")
        A("    (pools priced separately; NOT a combined gate.)")
        A("")
    A("=" * 70)
    return "\n".join(out)


def render_json(c, blocks_per_s, block_tx, segments, fold_mode,
                lag_p50_s, lag_p99_s, sizing, topo, cost_overlay):
    payload = {
        "model": "fleet-size (#95)",
        "framing": "OUTPUT IS SIZE + SHAPE; cost is non-gating overlay only",
        "shape": c.shape,
        "constants_label": c.label,
        "config": {"S": c.s, "segments": segments, "fold_mode": fold_mode},
        "demand": {"blocks_per_s": blocks_per_s, "tx_per_block": block_tx,
                   "k_chunks_per_block": sizing["lag"]["k"]},
        "governing_equation": {"lag_p50_s": lag_p50_s, "lag_p99_s": lag_p99_s},
        "lag_readout": {
            "central_path_s": round(sizing["lag"]["central_path_s"], 3),
            "l1_s": round(sizing["lag"]["l1_s"], 3),
            "merge_tree_s": round(sizing["lag"]["merge_s_total"], 3),
            "merge_depth": sizing["lag"]["merge_depth"],
            "l4_s": round(sizing["lag"]["l4_s"], 3),
            "slo_slack_s": round(sizing["slack_s"], 3),
            "verdict": sizing["verdict"],
            "is_k1_lower_bound": True,
        },
        # TWO SEPARATE classes -- intentionally NO combined total field.
        "size_by_class": {
            "worker_cells": {
                "count": sizing["worker_cells"],
                "raw": round(sizing["worker_cells_raw"], 3),
                "rss_mb_each": sizing["cell_rss_mb"],
                "basis": "Little's law over chunk pool (ADR-0004 §4.1)",
            },
            "coordinators": {
                "count": sizing["coordinators"],
                "raw": round(sizing["coordinators_raw"], 3),
                "rss_mb_each_proxy": sizing["coord_rss_mb_proxy"],
                "rss_unmodeled": sizing["coord_rss_unmodeled"],
                "basis": "block-grain fold service time (ADR-0004 §6.2)",
            },
            "_never_summed": ("worker_cells and coordinators are distinct "
                              "machine classes; do not add them -- ADR-0004 §6.2"),
        },
        "segment_folder_topology": topo,
        "citations": [
            {"key": k, "value": v, "source": s} for (k, v, s) in c.citations
        ],
        "unmodeled": [
            "witness_move (#61)", "contention/scaling losses (G4)",
            "realistic-data effects (G2)", "coordinator recovery (#75)",
            "coordinator-specific RSS (unmeasured)",
            "p99 straggler tail coeff (#101)", "L6 batch gate (#83)",
        ],
        "idealized": True,
    }
    if cost_overlay is not None:
        payload["cost_overlay_reporting_only_non_gating"] = {
            "price_per_machine": cost_overlay,
            "worker_cell_pool": sizing["worker_cells"] * cost_overlay,
            "coordinator_pool": sizing["coordinators"] * cost_overlay,
            "note": ("NON-GATING; did not influence any sizing above "
                     "(Discussion #77 norm)"),
        }
    return json.dumps(payload, indent=2)


def main(argv=None):
    p = argparse.ArgumentParser(
        description=("Parametric fleet-sizing model (#95): demand + config + "
                     "measured constants -> machines + topology. "
                     "OUTPUT IS SIZE + SHAPE; cost is a non-gating overlay."))
    p.add_argument("--shape", default="c4a-highcpu-64",
                   help="machine class basis; a calibration/<shape>.json "
                        "(default: c4a-highcpu-64, the deployment candidate)")
    p.add_argument("--s", type=int, default=9,
                   help="chunk size S (default: 9, the SLO-slack winner)")
    p.add_argument("--blocks-per-s", type=float, default=DEFAULT_PEAK_BLOCKS_PER_S,
                   help="demand: blocks/s (default: 5, the §0 floor)")
    p.add_argument("--tx-per-block", type=int, default=9000,
                   help="demand: tx/block (default: 9000, the worst case)")
    p.add_argument("--segments", type=int, default=8,
                   help="L5 segments per block (block-grain fold; default 8)")
    p.add_argument("--blocks-per-batch", type=int, default=None,
                   help="blocks per batch for segment-folder topology "
                        "(default: equals --segments)")
    p.add_argument("--fold-mode", choices=["tree", "serial"], default="tree",
                   help="fold mode (default: tree)")
    p.add_argument("--coord-concurrency", type=float, default=1.0,
                   help="blocks a single coordinator folds concurrently "
                        "(default 1)")
    p.add_argument("--lag-p50", type=float, default=None,
                   help="override the p50 lag bound (default: from the "
                        "shape's calibration JSON, else 20 s)")
    p.add_argument("--lag-p99", type=float, default=None,
                   help="override the p99 lag bound (default: from JSON, "
                        "else 40 s)")
    p.add_argument("--cost-overlay", type=float, default=None,
                   metavar="PRICE_PER_MACHINE",
                   help="OPTIONAL, REPORTING-ONLY, NON-GATING: multiply each "
                        "pool's machine count by this price. Does NOT affect "
                        "sizing (Discussion #77: cost is final-validation "
                        "only, never a gate).")
    p.add_argument("--json", action="store_true",
                   help="emit machine-readable JSON instead of the report")
    p.add_argument("--self-check", action="store_true",
                   help="run the c4a-highcpu-64 S=9 9000-tx self-consistency "
                        "check against single_machine_wall_9000 and exit")
    args = p.parse_args(argv)

    if args.self_check:
        return run_self_check()

    c = load_constants(args.shape, args.s)

    # Lag bound: prefer the shape's committed JSON value, then CLI, then default.
    with open(c.path) as fh:
        entry = json.load(fh)
    lag_p50_s = (args.lag_p50 if args.lag_p50 is not None
                 else float(entry.get("lag_p50_s", DEFAULT_LAG_P50_S)))
    lag_p99_s = (args.lag_p99 if args.lag_p99 is not None
                 else float(entry.get("lag_p99_s", DEFAULT_LAG_P99_S)))

    sizing = size_fleet(c, args.blocks_per_s, args.tx_per_block,
                        args.coord_concurrency, lag_p50_s)
    bpb = (args.blocks_per_batch if args.blocks_per_batch is not None
           else args.segments)
    topo = segment_folder_topology(bpb)

    if args.json:
        print(render_json(c, args.blocks_per_s, args.tx_per_block,
                          args.segments, args.fold_mode, lag_p50_s, lag_p99_s,
                          sizing, topo, args.cost_overlay))
    else:
        print(render_report(c, args.blocks_per_s, args.tx_per_block,
                            args.segments, args.fold_mode,
                            args.coord_concurrency, lag_p50_s, lag_p99_s,
                            sizing, topo, args.cost_overlay))
    return 0


def run_self_check():
    """Self-consistency: the c4a-highcpu-64 @ S=9 9000-tx central path must
    reproduce the committed single_machine_wall_9000 to the millisecond.
    This is the worked example AND the golden assertion (ADR-0004 §3.3).
    """
    shape, s, block_tx = "c4a-highcpu-64", 9, 9000
    c = load_constants(shape, s)
    lag = central_path_lag_s(c, block_tx)
    got = round(lag["central_path_s"], 3)

    # Read the committed reference directly from the artifact (no hardcoding).
    with open(c.path) as fh:
        entry = json.load(fh)
    row = next(r for r in entry["per_s_table"] if int(r["S"]) == s)
    expected = float(row["single_machine_wall_9000"])

    ok = abs(got - expected) <= 0.0005  # to the millisecond
    print(f"self-check: {shape} S={s} {block_tx}-tx central path")
    print(f"  computed central path        = {got:.3f} s")
    print(f"  committed single_machine_wall_9000 = {expected:.3f} s  "
          f"({c.rel_path})")
    print(f"  components: L1 {lag['l1_s']:.3f} + merge {lag['merge_s_total']:.3f}"
          f" (depth {lag['merge_depth']}) + L4 {lag['l4_s']:.3f}")
    print(f"  match (to the ms): {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
