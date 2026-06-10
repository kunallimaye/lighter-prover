# ADR-0001: Container topology for the Lighter bench (Phase 1)

- **Status**: Accepted
- **Date**: 2026-06-10
- **Tracking issue**: [#2](https://github.com/kunallimaye/lighter-prover/issues/2)
- **Related issues**: [#1](https://github.com/kunallimaye/lighter-prover/issues/1) (topology reference), [#3](https://github.com/kunallimaye/lighter-prover/issues/3) (Phase 2 work-sharding), [#4](https://github.com/kunallimaye/lighter-prover/issues/4) (CLI patch)

## Context

We need to measure Lighter prover throughput on local hardware (Podman) and
on Google Cloud (Cloud Build → Artifact Registry → Cloud Run Jobs) **without
modifying any Rust code beyond the CLI-flags patch landed in #4**.

Two parallelism axes were on the table:

1. **True work-sharding** — split layer-1 chunks across N workers so that
   N workers produce a single block proof in ~1/N the wall time. This
   requires Rust changes to `bench/src/bin/bench.rs` (chunk-assignment
   logic, inter-worker witness shuffling) and recursion-circuit changes
   to merge the partial proofs. Deferred to Phase 2 (#3).
2. **Embarrassingly-parallel fan-out** — every worker runs the **same**
   fixture; the orchestrator collects N independent timings and reports
   aggregates (mean, p50, p95, stdev). Gives per-worker throughput
   numbers and stress-tests the runtime.

Phase 1 ships option 2. It is the maximum useful work we can do today
under the "no Rust changes" constraint.

A second decision was where the prover binary's git ref is pinned. Upstream
`main` panics on the committed `bench_test.json` (see
[`elliottech/lighter-prover#9`](https://github.com/elliottech/lighter-prover/issues/9)
— `missing field 'amab'`, `index out of bounds`, double-assignment). The
last commit where `bench_test.json` is schema-compatible end-to-end is
[`5bbb307`](https://github.com/elliottech/lighter-prover/commit/5bbb307dfb26276c48054f2c3ea9dcfe80d3678a)
("Add bench binary"). Pinning to that commit lets us produce real numbers
today.

## Decision

### 1. One image, two roles

A single container image ships the bench binary plus a tiny
`entrypoint.sh` that dispatches on the `LIGHTER_ROLE` env var:

- `LIGHTER_ROLE=worker` (default) — runs `./bench` once (or
  `LIGHTER_BENCH_REPEAT` times) against `LIGHTER_BENCH_INPUT`, streams
  timings to stdout in the bench binary's native `TOTAL ... time: ...` /
  `AVERAGE ... time: ...` format.
- `LIGHTER_ROLE=orchestrator` — runs `orchestrator.py` which spawns
  `LIGHTER_WORKERS` sibling worker containers via `podman run`,
  captures their stdout, parses the timing lines, and prints aggregates.

Two roles, one image keeps the matrix simple: build once, push once,
deploy once. The role dispatch happens at container start.

The orchestrator role is only invoked from the operator's workstation
(or from a Cloud Build step). The container itself does **not** know how
to spawn its sibling workers on GCP — that's the build/orchestration
role's job per the three-role topology (see [issue #141](https://github.com/kunal-labs/lib-agents/issues/141)
and `AGENTS.local.md`). The GCP fan-out path uses Cloud Run Jobs and
lives in `scripts/cloud.sh`.

### 2. Pinned upstream ref via build arg

The Containerfile's `LIGHTER_REF` build argument defaults to
`5bbb307dfb26276c48054f2c3ea9dcfe80d3678a` and is captured into both
the image label and the `GIT_SHA` env var that the bench binary reads
into its machine-metadata header line (per #4).

Re-building against newer upstream is a one-line override:

```bash
make container-build LIGHTER_REF=<new-sha>
make cloud-bench-build LIGHTER_REF=<new-sha>
```

We deliberately did **not** vendor the source tree at `5bbb307` into
this repo; the bench source already lives at `5bbb307` in the workspace
fork that this CI/CD bundle targets.

### 3. Fan-out, not work-sharding

Phase 1's `make local-fanout N=4` and the orchestrator role produce N
independent proofs of the same block. The aggregate output looks like:

```
label                                                    n   mean    p50    p95   stdev
TOTAL BlockPreExecutionCircuit::prove time               4  12.3s  12.2s  12.6s   0.2s
TOTAL BlockTxCircuit::prove time                         4 245.1s 244.9s 248.3s   1.4s
AVERAGE BlockTxCircuit::prove time                       4   2.04s  2.04s  2.07s  0.01s
TOTAL BlockTxChainCircuit::prove time                    4  88.2s  88.1s  89.0s   0.4s
```

This is **per-worker throughput under load** (the meaningful metric for
sizing GCP machine pools); it is not "how fast can we prove one block
with N CPUs". Phase 2 (#3) will answer the latter.

### 4. Runtime base = debian:stable-slim + tini

The runtime stage is `debian:stable-slim` plus `tini` (PID 1, signal
forwarding), `python3` (orchestrator role), and `ca-certificates`.

Alternatives considered:

- `distroless/cc` — rejected. The bench binary needs `gethostname(3)`
  + `/proc/cpuinfo` + `/proc/meminfo` parsing (per #4's machine-metadata
  header), plus the orchestrator role needs `python3`. Distroless makes
  both painful.
- `alpine` — rejected. musl-vs-glibc symbol differences with the Rust
  nightly toolchain have bitten us before; debian-slim is the safer
  default for a one-shot benchmark image.

Image-size budget: the issue's acceptance criterion is < 1 GB. The
runtime stage is ~200 MB (debian-slim ~80 MB + tini/python ~30 MB +
stripped bench binary ~80 MB + 1.5 MB fixture).

### 5. Build cache portability over native-CPU performance

Default `RUSTFLAGS` is empty (no `-C target-cpu=native`). The build arg
`TARGET_CPU_NATIVE=1` opts in to native-CPU codegen. Rationale: Cloud
Build runs on machines the consumer (Cloud Run) never sees; baking native
codegen into a registry image risks `SIGILL` on cousin CPUs. Operators
who control the runtime fleet can rebuild with `TARGET_CPU_NATIVE=1` for
~10-15% throughput.

### 6. Terraform scope = AR repo + IAM only

Phase 1's Terraform module manages **only** the Artifact Registry
repository and the IAM grants Cloud Build needs to push to it. We
deliberately defer:

- The Cloud Run service. Bench is one-shot; Cloud Run Jobs handles
  on-demand invocation without a long-lived service.
- LB + DNS. Bench produces stdout/stderr, not a network endpoint.

When Phase 2 introduces a long-lived dashboard / API, the runtime
resources go back into Terraform via a new ADR.

## Consequences

### Positive

- One image, one CI pipeline, one Terraform module.
- The pin to `5bbb307` is explicit and visible (label + env var + image
  tag `:ref-5bbb307`).
- Switching to newer upstream is a one-line override.
- Local and GCP paths use the same image and the same orchestrator
  parsing logic — fewer surprises when moving from `make local-fanout`
  to Cloud Run Jobs.

### Negative

- Fan-out doesn't measure single-block latency reduction. That's a Phase
  2 concern; we explicitly flag the limitation in this ADR and in the
  README's "Phase 1 vs Phase 2" section.
- The runtime image is glibc-based and ~200 MB, larger than a distroless
  equivalent would be. The diagnostic + orchestrator-script ergonomics
  paid for this.
- The container is pinned to upstream behaviour at `5bbb307`. When
  upstream lands schema-compatibility fixes for `bench_test.json`
  (upstream #9), we have to rebuild with a new `LIGHTER_REF`. Documented
  in the README.

### Neutral

- Three-role topology is overkill for a hobby project that runs
  everything in one GCP project. We keep the topology scaffolded but
  collapse all three roles to one project by default (see
  `config.toml.example`).
