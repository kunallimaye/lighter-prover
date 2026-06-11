Once `make build` is run and the `bench` executable is generated, that and `bench_test.json` can be copied across machines being placed at the same level, and run independently from the repository.

## Supported `--tx-per-proof` range

This bench is validated for `--tx-per-proof ∈ {1, 2, 3, 4, 5, 6}` on
upstream commit `5bbb307`. Larger values trigger an unrelated bug in
the chain-recursion circuit sizing (`log_gates = 14` is insufficient
for the resulting verifier). See
[issue #8](https://github.com/kunallimaye/lighter-prover/issues/8)
for the analysis and proposed fixes.

The default `--tx-per-proof 4` matches upstream's production setting
(`bench/src/bin/bench.rs:33`, `build_circuits.sh:21`).

## Structured output

Alongside the existing `info!()` log lines (`TOTAL ...`, `AVERAGE ...`,
`BENCH_META ...`), the bench emits structured per-layer measurements on
stdout as JSON Lines. Each event is a single line prefixed with
`BENCH_EVENT ` (so it is easy to `grep` out of the normal log stream)
followed by a JSON object. stdout is flushed after every event, so
partial output survives a later crash (e.g. an OOM during proving).

### Event types

| `event`           | When                                              | Notable fields                                                                                                                                  |
|-------------------|---------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------|
| `circuit_define`  | After each circuit's `define + build` step        | `layer`, `name`, `wall_ms`, `rss_mb_after`, `ts`                                                                                                |
| `layer_prove`     | After each `*::prove(...)` call (L1, L2, L3)      | `layer`, `name`, `chunk_idx`, `chunk_total`, `tx_per_proof`, `wall_ms`, `cpu_ms`, `rss_mb_peak`, `rss_mb_after`, `ts`                            |
| `summary`         | Once at end of `main`                             | `tx_per_proof`, `tx_limit`, `chunks`, `total_wall_ms`, `total_cpu_ms`, `peak_rss_mb`, `ts`                                                      |

For L3 (`BlockPreExecutionCircuit`, one-shot) the `chunk_idx` and
`chunk_total` fields are explicitly `null`. For L1/L2 (per-chunk) they
are non-null `0..chunk_total`.

`cpu_ms`, `rss_mb_peak`, `rss_mb_after`, `peak_rss_mb`, and
`total_cpu_ms` are Linux-only (parsed from `/proc/self/status` /
`getrusage`). On non-Linux platforms they serialize as `null`. `ts`
is UTC in `YYYY-MM-DDTHH:MM:SSZ` form.

### Example events

```jsonl
BENCH_EVENT {"event":"circuit_define","layer":1,"name":"BlockTxCircuit","wall_ms":92340,"rss_mb_after":2400,"ts":"2026-06-10T03:08:22Z"}
BENCH_EVENT {"event":"layer_prove","layer":3,"name":"BlockPreExecutionCircuit","chunk_idx":null,"chunk_total":null,"tx_per_proof":4,"wall_ms":542,"cpu_ms":4120,"rss_mb_peak":1280,"rss_mb_after":1240,"ts":"2026-06-10T03:09:11Z"}
BENCH_EVENT {"event":"layer_prove","layer":1,"name":"BlockTxCircuit","chunk_idx":5,"chunk_total":120,"tx_per_proof":4,"wall_ms":2310,"cpu_ms":18432,"rss_mb_peak":2920,"rss_mb_after":2900,"ts":"2026-06-10T03:14:22Z"}
BENCH_EVENT {"event":"layer_prove","layer":2,"name":"BlockTxChainCircuit","chunk_idx":5,"chunk_total":120,"tx_per_proof":4,"wall_ms":498,"cpu_ms":3960,"rss_mb_peak":2925,"rss_mb_after":2910,"ts":"2026-06-10T03:14:23Z"}
BENCH_EVENT {"event":"summary","tx_per_proof":4,"tx_limit":480,"chunks":120,"total_wall_ms":345200,"total_cpu_ms":10984000,"peak_rss_mb":2925,"ts":"2026-06-10T03:15:00Z"}
```

### Aggregating with `jq`

Strip the prefix, parse as JSON, then aggregate. For example, mean
wall-clock per layer across all chunks:

```bash
grep '^BENCH_EVENT ' bench.log \
  | sed 's/^BENCH_EVENT //' \
  | jq -s '[.[] | select(.event=="layer_prove")]
           | group_by(.layer)
           | map({layer: .[0].layer,
                  n: length,
                  avg_wall_ms: (map(.wall_ms) | add/length)})'
```

Or just the summary line:

```bash
grep '^BENCH_EVENT ' bench.log | sed 's/^BENCH_EVENT //' \
  | jq 'select(.event=="summary")'
```

## Feeder (trace producer)

`bench/feeder/feeder.py` produces JSONL block-event streams for the
streaming bench. The producer/consumer contract — schema, gap markers,
provenance header, monotonicity rules, and policies P1–P4 — is pinned in
[`trace-format.md`](trace-format.md); every emitted stream conforms to it.
Witnesses never flow through the feeder: it carries only block cadence
and tx counts; the consumer (`bench --stream`, issue #49) sources witness
data itself.

| Subcommand | Mode | Summary |
|---|---|---|
| `record` | network | Capture live chain cadence: WS height channel (`wss://mainnet.zklighter.elliot.ai/stream?readonly=true`) merged with explorer tx counts via a ~4 s late-bind watermark; unmatched heights emit `tx_count: null` |
| `replay` | offline | Re-emit a recorded trace retimed (`--speed N` or `--target-rate TXS`; `--loop`, `--duration`, `--dry-run`) |
| `synth-peak` | offline, no inputs | Fabricate an idealized trace from a rate: back-to-back 500-tx blocks at cadence `500/rate` s |
| `peak-hours` | analysis helper | Locate peak windows from the explorer hourly tx stats (top-N hours by tx/s) |

Make targets (from `bench/`): `stream-record OUT=... [DURATION=...]`,
`stream-replay TRACE=... [SPEED=N | RATE=TXS] [DURATION=...]`,
`stream-peak RATE=... DURATION=...`, and `feeder-test` (offline suite,
no network, <1 min; also wired into the repo-root `make local-test`).

Dependencies: `record` and `peak-hours` need `bench/feeder/requirements.txt`
(`websockets`, `requests`); `replay`, `synth-peak`, and the tests are pure
Python 3 stdlib.

Geo-block note: the chain's main REST API returns 403 to US IPs. The
feeder avoids it entirely — the WS stream with `?readonly=true` and the
explorer API (`explorer.elliot.ai`) are not geo-blocked. The explorer
blocks endpoint is rate-limited to 90 req/min per IP; `record` polls at
~85 req/min with an identifying User-Agent.
