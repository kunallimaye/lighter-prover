"""Per-block size distributions for the feeder (issue #220).

Single source of truth for the 7-band partition (`scripts/trace-distribution/
analyze.py` lines 115-122) and the documented mainnet bimodal weights
(`trace-format.md` §8.1, sourced from #212 / PR #163 body). Importable
from both `bench/feeder/feeder.py` (the `synth-peak` sampled-size path)
and `tools/corpus-gen/gen_corpus.py` (which previously had local copies).

Pure, stdlib-only, offline. All sampling MUST go through an explicitly-
threaded `random.Random(seed)` — never the module-level `random` — so
runs are byte-deterministic given the same seed.

The chain's per-block tx cap (`BLOCK_TX_CAP = 500`) is the upper bound
on every representative; band predicates and the `eq_500` band reflect
that hard cap (spec §6.2).
"""

import json
import random  # noqa: F401  (kept for docstring / future use; sampling uses injected RNG)

# Chain per-block tx cap (spec §6.2 / feeder.BLOCK_TX_CAP). Inlined here so
# this module is stdlib-only and importable without dragging feeder.py in.
BLOCK_TX_CAP = 500

# Canonical 7-band partition (mirrors analyze.py:115-122 predicates).
#   (name, lo, hi, representative_tx_count)
# `eq_1` and `eq_500` are degenerate singletons (lo == hi). Range bands
# use the midpoint as the representative (the convention already used by
# tools/corpus-gen/gen_corpus.py:_BANDS for sizing slices).
BANDS = [
    ("eq_1", 1, 1, 1),
    ("2_49", 2, 49, 25),
    ("50_99", 50, 99, 75),
    ("100_249", 100, 249, 175),
    ("250_399", 250, 399, 325),
    ("400_499", 400, 499, 450),
    ("eq_500", 500, 500, 500),
]

BAND_NAMES = [b[0] for b in BANDS]

# Documented mainnet bimodal band counts — verbatim copy of the
# `_DOC_BANDS` table previously local to tools/corpus-gen/gen_corpus.py
# (PR #163 body / trace-format.md §8.1, traced to issue #212). The mass
# is bimodal: ~11% at tx==1, ~74% pinned to the 500-tx cap; the middle
# bands carry the long tail. Totals 5910 blocks.
MAINNET_BIMODAL_COUNTS = {
    "eq_1": 660,
    "2_49": 122,
    "50_99": 124,
    "100_249": 301,
    "250_399": 219,
    "400_499": 136,
    "eq_500": 4348,
}


def _band_of(tx):
    """Return the band name a given tx_count falls in, or None.

    Mirrors the analyze.py:115-122 predicates exactly so realized
    histograms can be compared to source-of-truth distributions.
    """
    for name, lo, hi, _rep in BANDS:
        if lo <= tx <= hi:
            return name
    return None


def realized_histogram(tx_counts):
    """Count an iterable of emitted tx_count values into the 7 bands.

    Returns a dict keyed by every band name (zero counts included so the
    output schema is stable across runs).
    Raises ValueError if any tx_count falls outside 1..BLOCK_TX_CAP.
    """
    hist = {name: 0 for name in BAND_NAMES}
    for tx in tx_counts:
        band = _band_of(tx)
        if band is None:
            raise ValueError(
                f"tx_count {tx} outside 1..{BLOCK_TX_CAP} (no band match)")
        hist[band] += 1
    return hist


class SizeSampler:
    """Weighted sampler over the 7-band partition.

    Draws a band with probability proportional to `weights[i]`, returns
    that band's `representatives[i]` (an integer tx_count).

    The RNG is injected (`rng: random.Random`) so callers control seeding;
    no module-level `random` is used anywhere in this class.
    """

    def __init__(self, *, name, weights, representatives, rng):
        if len(weights) != len(BAND_NAMES):
            raise ValueError(
                f"weights length {len(weights)} != {len(BAND_NAMES)} bands")
        if len(representatives) != len(BAND_NAMES):
            raise ValueError(
                f"representatives length {len(representatives)} != "
                f"{len(BAND_NAMES)} bands")
        total = sum(weights)
        if total <= 0:
            raise ValueError(
                f"weights must sum to a positive total, got {total}")
        for rep in representatives:
            if not isinstance(rep, int):
                raise ValueError(
                    f"representative {rep!r} must be int, got {type(rep).__name__}")
            if rep < 1 or rep > BLOCK_TX_CAP:
                raise ValueError(
                    f"representative {rep} outside 1..{BLOCK_TX_CAP}")
        if rng is None:
            raise ValueError("rng (random.Random) is required for determinism")
        self.name = name
        self.weights = list(weights)
        self.representatives = list(representatives)
        self._rng = rng

    def sample(self):
        """Draw one band per `weights` and return its representative tx_count."""
        # random.choices is stdlib, deterministic given the RNG state.
        idx = self._rng.choices(
            range(len(BAND_NAMES)), weights=self.weights, k=1)[0]
        return self.representatives[idx]

    def band_weights(self):
        """Return the per-band weights as a {name: weight} dict.

        Recorded into provenance `params.sampler_bands` so the seeded
        fixture is reviewable in-PR.
        """
        return {name: w for name, w in zip(BAND_NAMES, self.weights)}


def bimodal_mainnet_sampler(rng):
    """Factory wired to MAINNET_BIMODAL_COUNTS (the #212 mix).

    `name = "bimodal"` — recorded into provenance so the operator
    invocation can be reproduced from the trace header alone.
    """
    weights = [MAINNET_BIMODAL_COUNTS[name] for name in BAND_NAMES]
    representatives = [rep for _, _, _, rep in BANDS]
    return SizeSampler(
        name="bimodal", weights=weights,
        representatives=representatives, rng=rng)


def sampler_from_file(path, rng):
    """Load a sampler config from a JSON file.

    Schema:
      {
        "name": "<label>",
        "bands": [
          {"lo": 1, "hi": 1, "weight": 660},
          ...
        ]
      }

    Bands MUST cover ALL 7 partition bands by (lo, hi) match against
    `BANDS`; representative defaults to band midpoint (or `lo` if
    `lo == hi`). Raises SystemExit on malformed input so the CLI fails
    with a clear message rather than a cryptic traceback.
    """
    try:
        with open(path) as f:
            doc = json.load(f)
    except (OSError, FileNotFoundError) as e:
        raise SystemExit(f"error: --size-dist-file: cannot read {path}: {e}")
    except json.JSONDecodeError as e:
        raise SystemExit(
            f"error: --size-dist-file: {path} is not valid JSON: {e}")
    if not isinstance(doc, dict) or "bands" not in doc:
        raise SystemExit(
            f"error: --size-dist-file: {path}: missing required 'bands' key")
    raw_bands = doc.get("bands")
    if not isinstance(raw_bands, list):
        raise SystemExit(
            f"error: --size-dist-file: {path}: 'bands' must be a list")
    by_range = {(lo, hi): rep for _name, lo, hi, rep in BANDS}
    range_to_name = {(lo, hi): name for name, lo, hi, _rep in BANDS}
    weights_by_name = {}
    reps_by_name = {}
    for i, band in enumerate(raw_bands):
        if not isinstance(band, dict):
            raise SystemExit(
                f"error: --size-dist-file: {path}: bands[{i}] must be an object")
        for required in ("lo", "hi", "weight"):
            if required not in band:
                raise SystemExit(
                    f"error: --size-dist-file: {path}: "
                    f"bands[{i}] missing required key '{required}'")
        lo, hi, w = band["lo"], band["hi"], band["weight"]
        key = (lo, hi)
        if key not in by_range:
            raise SystemExit(
                f"error: --size-dist-file: {path}: bands[{i}] (lo={lo}, hi={hi}) "
                f"does not match any canonical band (must be one of: "
                f"{sorted(by_range)})")
        name = range_to_name[key]
        if name in weights_by_name:
            raise SystemExit(
                f"error: --size-dist-file: {path}: band {name} declared twice")
        weights_by_name[name] = w
        # representative override (optional) or default to file's
        # canonical-band representative.
        reps_by_name[name] = int(band.get("representative", by_range[key]))
    missing = [n for n in BAND_NAMES if n not in weights_by_name]
    if missing:
        raise SystemExit(
            f"error: --size-dist-file: {path}: missing bands {missing}; "
            f"all 7 canonical bands MUST be present "
            f"(zero-weight allowed but the band entry is required)")
    weights = [weights_by_name[n] for n in BAND_NAMES]
    representatives = [reps_by_name[n] for n in BAND_NAMES]
    return SizeSampler(
        name=str(doc.get("name", "custom")),
        weights=weights, representatives=representatives, rng=rng)
