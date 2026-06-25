# Reports Provenance & Fabrication Triage (issue #282)

This file is the auditable record of the destructive triage performed for
issue #282 ("Benchmark suite fabricates 'empirical' metrics"). Every tracked
file under `reports/` at the time of the audit was classified into one of three
classes. The triage is **conservative**: a file is only deleted when its
fabrication is traceable to a concrete fingerprint (a hand-written heredoc, a
physically-impossible value, or a known fabricated literal). When provenance is
ambiguous, the file is **kept** and flagged for human verification.

## Background

Fabricated functions in `infra-as-code/scripts/cloud.sh` (`cloud_test_t2d_hypothesis`,
`cloud_test_gke_performance_tax`, `cloud_test_capstone_matrix`,
`cloud_test_omni_silicon_parallel`) slept for fixed durations and then wrote
hardcoded heredoc ledgers. Those functions are now fail-loud stubs. The
fabricated literal constants used as fingerprints are:

- `12152` / `12.152` (GKE wall time = elapsed of a `sleep 12`)
- `41.15`, `41.65`, `38.57` (fabricated effective TPS)
- `1384431` / `1,384,431` (fabricated annual savings USD)
- `0.000291749` (impossible 500-tx wall time of 291 microseconds)
- `231450` (fabricated EVM gas)
- the capstone sextet `224.60 / 206.20 / 22.50 / 1254.50 / 19.50 / 26.41`
  as `measured_block_proving_time_s`

> NOTE: the bare numbers `41.65` and `231450` also appear as innocent substrings
> of genuine per-chunk timings (e.g. `441.658993ms`, `9.231450516s`) in
> `reports/spot_fleet/**/bench_unstructured.txt`. Those are **false positives**
> and are NOT fabricated; the guard script uses digit-boundary anchoring to
> avoid flagging them.

## Summary

| Class | Disposition | Count |
| :--- | :--- | ---: |
| A - Confirmed fabricated | **DELETED** (`git rm`) | 16 |
| B - Provenance ambiguous | **KEPT**, flagged for human verification | 26 |
| C - Presumed genuine | **KEPT**, no fabrication fingerprint | 767 |
| **Total tracked at audit** | | **809** |

## Class A - DELETED (confirmed fabricated)

Each file below was deleted in this PR. The justification is the concrete
fabrication fingerprint that matched.

| File | Fingerprint / reason for deletion |
| :--- | :--- |
| `reports/capstone_benchmark_benchmark-id-c4a-c4d-v0-0-1-final.csv` | all wall-time fields equal the fabricated GKE constant 12.152; CSV sibling of the fabricated -final json |
| `reports/capstone_benchmark_benchmark-id-c4a-c4d-v0-0-1-final.json` | all wall-time fields equal the fabricated GKE constant 12.152 (min=max=avg=12.152); derived solely from the fabricated figure |
| `reports/capstone_benchmark_benchmark-id-c4a-c4d-v0-0-1-final.md` | markdown sibling of the fabricated -final benchmark tabulating 12.152s across min/max/avg |
| `reports/capstone_extracted_telemetry_benchmark-id-c4a-c4d-v0-0-1-final.csv` | extracted-telemetry CSV copy of the fabricated -final 12.152 constant |
| `reports/capstone_extracted_telemetry_benchmark-id-c4a-c4d-v0-0-1-final.json` | extracted-telemetry copy of the fabricated -final 12.152 constant |
| `reports/capstone_four_release_empirical_matrix.json` | hardcoded fabricated sextet measured_block_proving_time_s (224.60/206.20/22.50/19.50); 'empirical' label with no measurement |
| `reports/capstone_six_release_empirical_matrix.json` | emitted by cloud_test_capstone_matrix heredoc; hardcoded sextet measured_block_proving_time_s (224.60/206.20/22.50/1254.50/19.50/26.41) |
| `reports/gke_tax_results.json` | emitted verbatim by cloud_test_gke_performance_tax heredoc (empirical_gke_wall_time_ms=12152, effective_tps=41.15 = elapsed of a fixed sleep) |
| `reports/job_1/bench_summary.json` | impossible physics: total_pipelined_scope_wall_sec=0.000291749s for 500 txs (291us); internally inconsistent effective_tps=41.15 (500/0.00029 ~ 1.7M, not 41) |
| `reports/omni_silicon_four_block_benchmark.json` | measured_leaf_proving_time_s=null on every row with hardcoded finality sextet (22.50/19.50/26.41); emitted by cloud_test_omni_silicon_parallel lineage which only renders pod specs to /dev/null |
| `reports/proposal_phase4_t2d_milan_leaf_arbitrage.md` | prose report emitted by cloud_test_t2d_hypothesis heredoc asserting 'empirically proven' $1,384,431 savings |
| `reports/proposal_phase5_gke_autopilot_reliability.md` | prose report emitted by cloud_test_gke_performance_tax heredoc asserting 'empirically proven' 12.152s |
| `reports/proposal_phase6_capstone_four_release_observatory.md` | prose 'empirical' proposal narrating the fabricated four-release matrix constants |
| `reports/proposal_phase6_capstone_six_release_observatory.md` | prose report emitted by cloud_test_capstone_matrix heredoc citing the hardcoded sextet |
| `reports/radix16_hexadecimal_experimental_timing.json` | fabricated evm_gas_used=231450 and leaf constant 3.12; no measurement provenance |
| `reports/t2d_hypothesis_results.json` | emitted verbatim by cloud_test_t2d_hypothesis heredoc (annual_fleet_savings_usd=1384431, effective_tps=41.65/38.57; no measurement) |

## Class B - FLAG-SUSPECT (KEPT, needs human verification)

These files contain a fabrication fingerprint **or** physically-impossible rows,
but provenance is ambiguous: they either mix plausibly-real measured rows with
impossible ones, or they are narrative documents that cite fabricated figures
alongside legitimate architecture/plan content. Per the conservative policy they
are **NOT deleted in this PR**. A human should verify each against the original
GCS source build and either excise the fabricated rows/figures or remove the file
in a follow-up. The anti-fabrication guard allow-lists exactly these paths so it
passes on the current tree while still catching any NEW fabricated file.

| File | Why suspect (human action required) |
| :--- | :--- |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-24_17-05-49.csv` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-24_17-05-49.json` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-24_17-05-49.md` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-25_04-16-18.csv` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-25_04-16-18.json` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-25_04-16-18.md` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-25_04-42-22.csv` | MIXED: contains physically-impossible sub-millisecond wall-time rows (e.g. 6.3e-05s for a 10-block run) alongside plausibly-real measured rows (211-705s); could be a partial real run polluted by an extraction bug - needs human verification against the GCS source build before deletion |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-25_04-42-22.json` | MIXED: contains physically-impossible sub-millisecond wall-time rows (e.g. 6.3e-05s for a 10-block run) alongside plausibly-real measured rows (211-705s); could be a partial real run polluted by an extraction bug - needs human verification against the GCS source build before deletion |
| `reports/capstone_benchmark_benchmark-id-ALL-2026-06-25_04-42-22.md` | MIXED: contains physically-impossible sub-millisecond wall-time rows (e.g. 6.3e-05s for a 10-block run) alongside plausibly-real measured rows (211-705s); could be a partial real run polluted by an extraction bug - needs human verification against the GCS source build before deletion |
| `reports/capstone_benchmark_benchmark-id-GKE-test-2026-06-24_16-53-41.csv` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-GKE-test-2026-06-24_16-53-41.json` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_benchmark_benchmark-id-GKE-test-2026-06-24_16-53-41.md` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_extracted_telemetry_benchmark-id-ALL-2026-06-24_17-05-49.csv` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_extracted_telemetry_benchmark-id-ALL-2026-06-24_17-05-49.json` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_extracted_telemetry_benchmark-id-ALL-2026-06-25_04-16-18.csv` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_extracted_telemetry_benchmark-id-ALL-2026-06-25_04-16-18.json` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_extracted_telemetry_benchmark-id-ALL-2026-06-25_04-42-22.csv` | MIXED: contains physically-impossible sub-millisecond wall-time rows (e.g. 6.3e-05s for a 10-block run) alongside plausibly-real measured rows (211-705s); could be a partial real run polluted by an extraction bug - needs human verification against the GCS source build before deletion |
| `reports/capstone_extracted_telemetry_benchmark-id-ALL-2026-06-25_04-42-22.json` | MIXED: contains physically-impossible sub-millisecond wall-time rows (e.g. 6.3e-05s for a 10-block run) alongside plausibly-real measured rows (211-705s); could be a partial real run polluted by an extraction bug - needs human verification against the GCS source build before deletion |
| `reports/capstone_extracted_telemetry_benchmark-id-GKE-test-2026-06-24_16-53-41.csv` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/capstone_extracted_telemetry_benchmark-id-GKE-test-2026-06-24_16-53-41.json` | SUSPECT: every wall-time row is physically-impossible sub-millisecond (e.g. 5e-05s for a multi-tx block); no real rows present, but not traceable to a specific fabricating heredoc - likely an extraction/aggregation artifact, kept for human verification against the GCS source build |
| `reports/gke_default_distributed_cluster_plan.md` | plan doc cites the fabricated 12.152s E2E finality ledger; otherwise a planning artifact - human review |
| `reports/lighter_enterprise_stark_observatory_master_whitepaper.md` | master whitepaper cites fabricated evm_gas_used=231450 and 19.50s finality; mostly architectural narrative - human review to remove fabricated figures |
| `reports/omni_silicon_four_block_parallel_study.md` | study doc references the omni-silicon sextet constants whose JSON is being deleted; human review for fabricated figures |
| `reports/release_0_0_3_notes.md` | release notes claim 'verified empirical 500-tx settlement in 231450 gas' (fabricated constant); human review to soften claim |
| `reports/t2d_vs_c4a_leaf_prover_arbitrage_study.md` | study doc references the t2d arbitrage fabricated savings/TPS; human review |
| `reports/walkthrough.md` | narrative walkthrough cites fabricated constants (12.152, 41.15/41.65, 231450 gas) as 'authentic empirical' results; mixes with real architecture description - human should excise the fabricated rows |

## Class C - presumed genuine (KEPT)

The remaining 767 tracked files show no fabrication fingerprint and have no
provenance concern. They are predominantly real measured telemetry:

- `reports/spot_fleet/**` and `reports/chunk_matrix/**` `bench_summary.json` files
  contain internally-consistent, physically-plausible telemetry (e.g.
  `total_pipelined_scope_wall_sec ~ 2416s` for 500 txs with `effective_tps ~ 0.21`,
  and hundreds of realistic per-circuit `BlockTxChainCircuit` timings of 3-23s).
- `reports/cloud_layer3_poc.json` / `reports/cloud_layer4_poc.json` carry small,
  internally-consistent speedup measurements with no fabricated constants.
- planning/architecture markdown documents that do not cite fabricated figures.

They are not listed individually here; the full list is reproducible with the
triage classifier referenced in the PR.

## Regeneration is deferred (follow-up)

Regenerating real, provenance-stamped benchmark reports requires a live
distributed proving run on GCP compute. With #281 (reduction-tree circuit
correctness) and #283 (honest distributed prover-node) merged, such a run is now
possible in principle, but it is **out of scope for #282** and explicitly
deferred to a follow-up. This PR only removes fabrication and adds guardrails.

