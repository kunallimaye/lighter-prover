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

## Revision 1 (2026-06-10): tag provenance fix

- **Tracking issue**: [#15](https://github.com/kunallimaye/lighter-prover/issues/15)

### What changed

The original Phase 1 build (PR #13) treated `LIGHTER_REF` as a **label
of intent**: the operator passed an upstream commit SHA via
`--build-arg LIGHTER_REF=<sha>` and the image was tagged
`:ref-<short-of-that-sha>`. The Containerfile then `COPY . .`'d the
build context — i.e. *this* repo's working tree — into the builder
stage. The label and the source disagreed: a build of this repo's
`main` could still produce an image tagged `:ref-5bbb307`, lying about
the bench code inside.

The fix shifts `LIGHTER_REF` from "label of intent" to **label of
provenance**:

- `cicd/cloudbuild.yaml` derives `LIGHTER_REF` from Cloud Build's
  built-in `$COMMIT_SHA` and tags the image `:ref-$SHORT_SHA`. The
  `_LIGHTER_REF` / `_LIGHTER_REF_SHORT` user substitutions are gone.
- `cicd/Containerfile` adds the OCI standard
  `org.opencontainers.image.revision` and `image.source` labels so
  any registry UI / `docker image inspect` consumer sees the SHA via
  the canonical keys.
- `scripts/container.sh::build` derives the same SHA from
  `git rev-parse HEAD` for the local podman path and tags the image
  both `:latest` and `:ref-<short>` so local and CI semantics match.
- `scripts/cloud.sh::cloud_bench_build` passes `COMMIT_SHA` /
  `SHORT_SHA` explicitly via `--substitutions` (manual
  `gcloud builds submit` does not auto-populate Cloud Build built-ins
  the way git triggers do).

### Why "derive from build context" over "git clone inside"

The alternative — adding `git clone <upstream>@<ref>` inside the
Containerfile so the label and the source agree because the source IS
fetched from `<ref>` — was considered and rejected:

- Heavier: needs `git` in the builder stage and a fresh fetch on every
  build (no Docker layer cache benefit because the SHA varies).
- Slower: a clone of `elliottech/lighter-prover` adds ~30–60 s to every
  build that would otherwise be a cached no-op.
- Breaks offline / air-gapped builds.
- Makes `COPY . .` actively confusing — why is there a build context
  at all if the source comes from git?

"This repo is the source of truth for our bench binary" is the
honest framing. Cross-repo pinning to a literal upstream SHA belongs
in a separate discussion if it's ever needed.

### Consequence: the old `:ref-5bbb307` tag is gone

Any operator who was pinning to `:ref-5bbb307` should now pin to a
specific `:sha-<short>` instead. The `:ref-*` tag still exists but now
equals `:sha-*` for any single build (we keep both for backward-compat
with anyone scripting against `:ref-*`).

## Revision 2 (2026-06-14): host-only orchestration + JSONL parser

- **Tracking issue**: [#21](https://github.com/kunallimaye/lighter-prover/issues/21)

### What changed

Two cleanups that reconcile the image with the three-role topology this
ADR established:

1. **The in-container `orchestrator` role was removed.** Sections above
   describe a two-role image (`worker` | `orchestrator`) with the
   orchestrator dispatched at container start. That `LIGHTER_ROLE=orchestrator`
   path was never wired up: fan-out has always been invoked from the
   host (`scripts/container.sh fanout` → `cicd/orchestrator.py`), never
   via `podman run -e LIGHTER_ROLE=orchestrator`. Keeping it in the
   runtime image contradicted the very decision recorded here — that
   orchestration is a host/build-role concern, not a runtime-role
   concern. The runtime image now ships **only the worker role**.
   - `cicd/entrypoint.sh`: dropped `run_orchestrator()` and the
     `orchestrator)` dispatch case.
   - `cicd/Containerfile`: dropped `COPY cicd/orchestrator.py` and the
     explicit `python3` / `python3-pip` apt installs (they existed
     solely for the in-container orchestrator). `cicd/orchestrator.py`
     itself stays in the repo — it is the host fan-out + aggregation
     tool, also imported by `scripts/local.sh`.

2. **The orchestrator parser is now a `BENCH_EVENT` JSONL consumer.**
   The host `orchestrator.py` previously regex-parsed the bench binary's
   `TOTAL`/`AVERAGE` INFO lines. Since #9 (structured instrumentation)
   and #18 landed, the bench emits structured `BENCH_EVENT ` JSON Lines
   (`bench/src/events.rs`). The parser was migrated to consume those
   directly — a hard cut, no regex fallback — yielding per-chunk × per-
   layer measurements, peak RSS, and multicore CPU efficiency instead of
   per-worker means-of-means. The legacy INFO lines are still emitted by
   the worker for human readers; they are simply no longer the
   aggregation contract.

### Why a hard cut (no regex fallback)

Per #15, the `:ref-<sha>` tag now truthfully encodes provenance, so no
operator legitimately needs to run a pre-#9 image *through the new
orchestrator*. Pre-#9 images can still be run directly via `podman run`;
they just aren't aggregable. Carrying two parser paths would bloat the
orchestrator and create a test matrix nobody benefits from.
