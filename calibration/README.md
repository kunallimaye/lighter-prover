# Calibration registry

Machine-readable chunk-size calibration results, one JSON per
shape, emitted by the s-calibrate suite (issues #85 / #102).
Calibration validity is tied to the circuit code it measured, so
results live in-repo, versioned with that code: **recalibration =
a PR that diffs these files**, making "did this circuit change
move the optimum?" a code-review diff.

## Purpose

The five questions this suite answers: (1) optimal S on an unmeasured
shape; (2) did a circuit change move the optimum; (3) can we trust this
row (n/stdev/load-quality); (4) what S should this worker run
(machine-consumable artifact, future boot-time self-config); (5) is S=9
still the winner with measured per-shape merge/L4 (Phase C, via this
suite).

## Current recommendations

| shape | date | sha | circuit hash | load | MERGE_S (s) | L4_WALL (s) | S* (SLO slack) | min slack (s) | S* serial | S* tree | S* s/tx |
|---|---|---|---|---|---|---|---|---|---|---|---|
| c4a-highcpu-64 | 2026-06-11 | 5be70d9 | `f634a649afd2` | clean | 0.238 (extrapolated) | 2.579 (extrapolated) | S=9 | 12.054 | S=20 | S=8 | S=10 |
| c4a-highmem-64 | 2026-06-11 | 5be70d9 | `f634a649afd2` | clean | 0.240 (extrapolated) | 2.598 (extrapolated) | S=9 | 11.998 | S=20 | S=8 | S=10 |
| c4a-highmem-96-metal | 2026-06-11 | 5be70d9 | `f634a649afd2` | clean | 0.225 (extrapolated) | 2.433 (extrapolated) | S=9 | 12.553 | S=20 | S=8 | S=10 |
| epyc-7b13-ref | 2026-06-11 | 5be70d9 | `f634a649afd2` | loaded | 0.476 (measured) | 5.155 (measured) | S=9 | 4.630 | S=20 | S=9 | S=9 |

Constants labeled `extrapolated` come from the Phase A reference
machine scaled by the shape's S=20 L1-wall ratio; `measured` means
this shape ran the opt-in `CAL_L4=1` merge/L4 measurement (or the
Phase A reference measurement itself). Rows from `loaded` runs
carry ~10-20% inflated walls -- treat near-zero-slack verdicts as
unreliable there.

## Regenerating

```
make s-calibrate OUT_REGISTRY=1                  # this machine
make s-calibrate OUT_REGISTRY=1 CAL_L4=1         # + measured MERGE_S/L4_WALL
make s-calibrate-fleet                           # collect the c4a cloud probes
make calibration-check                           # staleness guard (warn-only)
```

Fleet runs collect probes + reports per shape; emit their registry
entries afterwards by re-running scripts/s-calibrate-report.py on
each collected directory with `--out-registry calibration
--shape-label <shape>`.

Commit the resulting `calibration/*.json` + this README in a PR --
the diff IS the recalibration review.

## Ledger link policy

Discussion #77's BENCH-LEDGER remains the append-only history;
every new ledger entry should link the commit that updated this
registry. The registry holds only the CURRENT recommendation per
shape; history lives in git + the ledger.
