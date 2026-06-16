#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
"""
Synthetic block-corpus generator (issue #165; Refs #128 #121 #144).

Lays out a `{height, witness_index}` MountedCorpus structure + a provenance
manifest whose block-SIZE distribution matches the REAL mainnet load
distribution, derived by running the real analyzer
(`scripts/trace-distribution/analyze.py`) on the real banked trace.

================================ HONEST SCOPE =================================
GENERATION ONLY. No proving fleet, no cloud spend, no matching engine.

What this DOES synthesize:
  - The `{height, witness_index}` ADDRESSING LAYOUT (ADR-0008 §1.1) — one
    `height` per corpus block, pre-sliced into `k = ceil(tx_count / S)` chunk
    slices indexed `0..k-1`, exactly mirroring how `bench/src/bin/bench.rs`
    slices `bench_test.json` into the k=1 MountedCorpus.
  - The corpus block-SIZE distribution: the per-band proportions from the real
    trace, scaled down to a small committable corpus (largest-remainder
    rounding so the ~73.6%-at-cap shape is preserved).
  - A provenance manifest (trace source, analyzer command, per-band counts,
    scale factor, seed sha256) + a flat `{height, witness_index}` index.

What this DOES NOT synthesize (and says so, per band):
  - It does NOT fabricate witness BYTES / Merkle roots for the lower bands.
    The repo ships exactly ONE real chain-VALID, fully prover-serializable
    500-tx (cap) block: `bench/bench_test.json` (tx dist
    {15:118, 17:168, 21:169, 14:45}, sample-size-1). Every cap-band corpus
    block REFERENCES that real validated seed (`seed_ref`, `synthesized:false`).
    Lower-band blocks are LAYOUT/MANIFEST placeholders that record their
    `{height, witness_index, tx_count, band}` and reference the seed; their
    witness bytes are NOT materialized (synthesizing arbitrary fully-
    serializable sub-cap blocks needs signatures + public-data + the full
    account-tree leaf, gated on #120/#126/#125 — see
    tools/witness-reconstructor/largerblock.go HONEST SCOPE).

  Honest-partial > fake-complete: the corpus reproduces the real block-SIZE
  shape and the real load contract layout; it does not invent witness values.
==============================================================================
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
from datetime import datetime, timezone

_THIS = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.abspath(os.path.join(_THIS, "..", ".."))
_ANALYZER = os.path.join(_REPO, "scripts", "trace-distribution", "analyze.py")
_SEED = os.path.join(_REPO, "bench", "bench_test.json")

# Real seed block height (bench_test.json "bn"), the k=1 precedent height.
_SEED_HEIGHT = 186974592

# Shared 7-band partition + documented mainnet weights (issue #220):
# single source of truth in `bench/feeder/size_distributions.py` so the
# feeder's --size-distribution bimodal sampler and this corpus generator
# stay in lockstep. Previously had local _BANDS / _DOC_BANDS copies here.
sys.path.insert(
    0, os.path.join(_REPO, "bench", "feeder"))
import size_distributions as _sd  # noqa: E402

# The analyzer's band order (`bands_spec` in analyze.py) with a representative
# tx_count per band for sizing the slices. The cap band is exact (=500); the
# range bands use the band MIDPOINT as the representative size (documented).
# Shape preserved verbatim ((name, representative) tuples) so call sites
# below are unchanged.
_BANDS = [(name, rep) for name, _lo, _hi, rep in _sd.BANDS]

# Documented fallback band counts (PR #163 body / trace-format.md §8.1), used
# ONLY with --from-doc and clearly labeled. Recomputed-from-real-trace is the
# default and strongly preferred.
_DOC_BANDS = dict(_sd.MAINNET_BIMODAL_COUNTS)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def real_bands(trace_path):
    """Run the REAL analyzer on the REAL trace; return (bands, command)."""
    cmd = [sys.executable, _ANALYZER, trace_path, "--json"]
    out = subprocess.check_output(cmd, text=True)
    data = json.loads(out)
    bands = data["block_size"]["bands"]
    nn = data["block_size"]["non_null_blocks"]
    total = sum(bands.values())
    if total != nn:
        raise SystemExit(
            f"band partition check failed: sum={total} != non_null={nn}")
    return bands, " ".join(cmd), nn


def scale_distribution(bands, n_blocks):
    """Scale the per-band counts to n_blocks, preserving proportions via
    largest-remainder rounding (so the cap band stays ~73.6%)."""
    total = sum(bands.values())
    raw = {k: bands[k] / total * n_blocks for k in bands}
    floored = {k: int(v) for k, v in raw.items()}
    remainder = n_blocks - sum(floored.values())
    # distribute the remainder to the largest fractional parts
    fracs = sorted(bands, key=lambda k: raw[k] - floored[k], reverse=True)
    for k in fracs[:remainder]:
        floored[k] += 1
    return floored


def build_corpus(scaled, chunk_size, base_height):
    """Build the {height, witness_index} layout. The single cap-band
    representative at `base_height` is the real validated seed; all other
    blocks are layout placeholders referencing the seed."""
    band_size = dict(_BANDS)
    index = []          # flat {height, witness_index} entries
    blocks = []         # per-block summary
    height = base_height
    seed_assigned = False

    # Emit in band order; within the cap band, the FIRST block is the real
    # seed (synthesized:false, the validated bench_test.json), the rest are
    # cap-sized placeholders referencing it.
    for band, _rep in _BANDS:
        count = scaled.get(band, 0)
        tx_count = band_size[band]
        for _ in range(count):
            is_seed = (band == "eq_500" and not seed_assigned)
            if is_seed:
                seed_assigned = True
            k = max(1, -(-tx_count // chunk_size))  # ceil division
            slices = []
            for wi in range(k):
                # last slice may be short; record real per-slice tx count
                lo = wi * chunk_size
                hi = min(lo + chunk_size, tx_count)
                s_tx = hi - lo
                slices.append(wi)
                index.append({
                    "height": height,
                    "witness_index": wi,
                    "tx_count": s_tx,
                })
            blocks.append({
                "height": height,
                "band": band,
                "tx_count": tx_count,
                "k": k,
                "chunk_size": chunk_size,
                "witness_indices": slices,
                "synthesized": False,
                "is_real_seed": is_seed,
                "seed_ref": "bench/bench_test.json",
                "note": (
                    "real chain-VALID 500-tx seed block (bench_test.json), "
                    "validated bit-for-bit by tools/witness-reconstructor"
                    if is_seed else
                    "layout/manifest placeholder: {height, witness_index} "
                    "addressing only; witness bytes NOT materialized — "
                    "references the real seed (no fabricated roots)"
                ),
            })
            height += 1
    return blocks, index


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--trace", help="path to the real banked trace (jsonl); "
                    "bands are recomputed from it via the real analyzer")
    ap.add_argument("--from-doc", action="store_true",
                    help="use the DOCUMENTED #163 band counts instead of "
                         "recomputing (clearly labeled; NOT preferred)")
    ap.add_argument("--n-blocks", type=int, default=100,
                    help="corpus size in blocks (default 100; small + "
                         "committable, NOT the full ~5910)")
    ap.add_argument("--chunk-size", type=int, default=100,
                    help="S, tx-per-chunk slice (default 100; matches "
                         "bench.rs --tx-per-proof slicing)")
    ap.add_argument("--base-height", type=int, default=_SEED_HEIGHT,
                    help="first corpus height (default = seed bench_test.json bn)")
    ap.add_argument("--out", default=os.path.join(_REPO, "bench", "corpus"),
                    help="output directory (default bench/corpus)")
    args = ap.parse_args()

    if args.from_doc:
        bands = dict(_DOC_BANDS)
        source = "DOCUMENTED #163 PR body / trace-format.md §8.1 (NOT freshly recomputed)"
        analyzer_cmd = "(--from-doc: no analyzer run)"
        nn = sum(bands.values())
    else:
        if not args.trace:
            raise SystemExit(
                "error: --trace <path> is required (or use --from-doc).\n"
                "Fetch the real trace first, e.g.:\n"
                "  gcloud storage cat gs://kunal-scratch-bench-fleet-runs/"
                "traces/2026-06-11T0204Z-15m-offpeak/trace_15m.jsonl "
                "> /tmp/trace_15m.jsonl")
        bands, analyzer_cmd, nn = real_bands(args.trace)
        source = f"REAL analyzer on REAL trace ({args.trace}), non_null={nn}"

    scaled = scale_distribution(bands, args.n_blocks)
    blocks, index = build_corpus(scaled, args.chunk_size, args.base_height)

    seed_sha = sha256_file(_SEED)
    manifest = {
        "schema": "lighter-prover/corpus-manifest/v1",
        "issue": 165,
        "refs": [128, 121, 144],
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "contract": {
            "spec": "ADR-0008 §1.1 {height, witness_index}; §1.4 k=1 "
                    "bench_test.json precedent",
            "resolver": "bench::conductor::MountedCorpus (in-memory, "
                        "mount_block(height, slices))",
            "loader_precedent": "bench/src/bin/bench.rs slices bench_test.json "
                                "into S-tx chunks indexed 0..k-1",
        },
        "distribution": {
            "source": source,
            "analyzer_command": analyzer_cmd,
            "real_bands_non_null": bands,
            "scaled_bands": scaled,
            "n_blocks": args.n_blocks,
            "chunk_size_S": args.chunk_size,
            "cap_fraction_scaled": round(
                scaled.get("eq_500", 0) / max(1, args.n_blocks), 4),
        },
        "seed": {
            "path": "bench/bench_test.json",
            "sha256": seed_sha,
            "height": _SEED_HEIGHT,
            "tx_count": 500,
            "tx_type_dist": {"15": 118, "17": 168, "21": 169, "14": 45},
            "note": "sample-size-1 real chain-VALID cap block; the ONLY "
                    "fully prover-serializable block in-repo; validated "
                    "bit-for-bit by tools/witness-reconstructor",
        },
        "honest_scope": {
            "synthesized_witness_bytes": False,
            "explanation": "Cap-band blocks reference the real validated seed; "
                           "lower-band blocks are {height, witness_index} "
                           "layout placeholders. No witness bytes / Merkle "
                           "roots are fabricated. Fully-serializable sub-cap "
                           "synthesis is gated on #120/#126/#125.",
        },
        "totals": {
            "blocks": len(blocks),
            "index_entries": len(index),
        },
        "blocks": blocks,
    }

    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")
    with open(os.path.join(args.out, "index.json"), "w") as f:
        json.dump({"schema": "lighter-prover/corpus-index/v1",
                   "entries": index}, f, indent=2)
        f.write("\n")

    print(f"corpus written to {args.out}")
    print(f"  blocks         : {len(blocks)}")
    print(f"  index entries  : {len(index)} {{height, witness_index}}")
    print(f"  cap fraction   : {manifest['distribution']['cap_fraction_scaled']*100:.1f}% "
          f"(real: {bands['eq_500']*100/nn:.1f}%)")
    print(f"  distribution   : {source}")
    print(f"  seed sha256    : {seed_sha[:16]}...")


if __name__ == "__main__":
    main()
