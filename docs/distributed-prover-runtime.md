# Distributed prover runtime — coordinator + cell over Pub/Sub

**Issue:** #172 · **Refs:** #75 #61 #144 · **Builds on:** #164 (conductor lib),
ADR-0006 (conductor design), ADR-0008 (witness delivery plane), ADR-0004
(governing equation / `lag(c, l)`).

> **Status: honest-partial.** This delivers a GENUINE distributed entrypoint —
> `bench --mode coordinator` and `bench --mode cell` run as SEPARATE pods
> coordinating over REAL Pub/Sub, with the REAL L1+L2 prove on the cell. It is
> **not** a single-machine simulation. What is verified locally vs. only on
> GKE is stated explicitly at the bottom. **No proving output is ever
> fabricated and no crypto is stubbed.**

---

## 1. What this is

The container previously shipped only a `worker` role that ran the full
`./bench` pipeline against a fixture — embarrassingly-parallel, not
distributed. The conductor library (`bench/src/conductor/`, merged in #164)
modeled the two-tier dispatch IN-PROCESS (cells = threads, queue = in-memory
`LocalBlockQueue`). This change adds the real network transport and two new run
modes so the same binary runs as real GKE pods:

```
                    ┌────────────────────────────────────────────┐
   feeder publishes │            Pub/Sub: dispatch topic          │
   block events ───►│  {height, tx_count}                         │
                    └───────────────┬─────────────────────────────┘
                                    │ competing-pull (ADR-0006 §1.1)
                          ┌─────────▼──────────┐
                          │  COORDINATOR pod    │  bench --mode coordinator
                          │  SPLIT k=ceil(tx/S) │  (one per pod; vert.conc.=1)
                          └─────────┬──────────┘
            publish k chunk refs    │            ▲ pull results (GATHER/FOLD)
                                    ▼            │
                    ┌────────────────────────────────────────────┐
                    │       Pub/Sub: chunk topic / results topic   │
                    │  chunk:  {height, witness_index, tx_count}   │
                    │  result: {height, witness_index, prove_ms,   │
                    │           witness_fetch_ms, ok, cell}        │
                    └───────────────┬─────────────────────────────┘
                                    │ competing-pull
                          ┌─────────▼──────────┐
                          │   CELL pods (N)     │  bench --mode cell
                          │  resolve witness    │  REAL BlockTxCircuit (L1)
                          │  REAL L1 + L2 prove │  + BlockTxChainCircuit (L2)
                          └────────────────────┘
```

Cells receive work over the network and are SEPARATE PODS — not threads.

---

## 2. The exact commands the pods run

The bench binary is installed in the image under **two names** (binary-path
reconciliation, assumption #6 below):

- `/app/bench` — used by the `worker` entrypoint role (unchanged).
- `/usr/local/bin/prover` — symlink to `/app/bench`, so the GKE tfvars can
  call the forward-looking `["/usr/local/bin/prover","--mode","cell"]` form.

**Coordinator pod command** (this is what `scale-0p2pct.tfvars` sets in
`coordinator_command`; topic/sub names are tier-prefixed
`lighter-prover-scale-<tier>-*`):

```hcl
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--project", "<PROJECT_ID>",
  "--dispatch-subscription", "lighter-prover-scale-0p2pct-dispatch-sub",
  "--chunk-topic",           "lighter-prover-scale-0p2pct-chunk",
  "--results-subscription",  "lighter-prover-scale-0p2pct-results-sub",
  "--poll-interval-s", "2",
]
```

**Cell pod command** (this is what `scale-0p2pct.tfvars` sets in `cell_command`):

```hcl
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--project", "<PROJECT_ID>",
  "--chunk-subscription", "lighter-prover-scale-0p2pct-chunk-sub",
  "--results-topic",      "lighter-prover-scale-0p2pct-results",
  "--poll-interval-s", "2",
]
```

`--tx-per-proof` is read from the image default (`LIGHTER_TX_PER_PROOF=4`) when
omitted; the scale tfvars rely on the image default to keep `S` consistent with
the cell/coordinator pair. Pass it explicitly if overriding per tier.

Every flag also has an env-var equivalent (clap `env`), so the alternative is
`LIGHTER_ROLE=coordinator|cell` + the `LIGHTER_*` env block and let
`cicd/entrypoint.sh` invoke the right `--mode`. Both paths are supported.

### Env vars / Pub/Sub names the pods need

| Flag | Env var | Used by |
|---|---|---|
| `--mode` | `LIGHTER_MODE` | both |
| `--project` | `LIGHTER_PROJECT` | both |
| `--dispatch-subscription` | `LIGHTER_DISPATCH_SUBSCRIPTION` | coordinator |
| `--chunk-topic` | `LIGHTER_CHUNK_TOPIC` | coordinator |
| `--results-subscription` | `LIGHTER_RESULTS_SUBSCRIPTION` | coordinator |
| `--chunk-subscription` | `LIGHTER_CHUNK_SUBSCRIPTION` | cell |
| `--results-topic` | `LIGHTER_RESULTS_TOPIC` | cell |
| `--tx-per-proof` | `LIGHTER_TX_PER_PROOF` | both |
| `--max-units` | `LIGHTER_MAX_UNITS` | both (0 = run forever) |
| `--poll-interval-s` | `LIGHTER_POLL_INTERVAL_S` | both |
| `--gcloud-bin` | `LIGHTER_GCLOUD_BIN` | both (default `gcloud`) |

---

## 3. Terraform Pub/Sub resources added

`cicd/terraform/gke/main.tf` already provisioned the OUTER block-dispatch
topic+subscription (the backlog signal the HPA watches). This change adds the
inner planes, **gated behind `enable_chunk_plane`** (default `false`, so the
smoke automation is unchanged):

| Resource | Variable | Default |
|---|---|---|
| `google_pubsub_topic.chunk` | `chunk_topic` | `lighter-prover-chunk` |
| `google_pubsub_subscription.chunk` | `chunk_subscription` | `lighter-prover-chunk-sub` |
| `google_pubsub_topic.results` | `results_topic` | `lighter-prover-results` |
| `google_pubsub_subscription.results` | `results_subscription` | `lighter-prover-results-sub` |
| (ack deadline) | `chunk_ack_deadline_seconds` | `600` |

A new `chunk_plane` output reports the resolved names. The pre-existing,
sizing-model-grounded scale tfvars (`scale-0p2pct.tfvars`, `scale-0p3pct.tfvars`,
`scale-0p5pct.tfvars` — the 0.2/0.3/0.5% validation ladder, sized from the #95
model) already invoked `["/usr/local/bin/prover","--mode","cell"]` /
`--mode coordinator`. This change extends them **additively**: it turns on
`enable_chunk_plane`, adds the per-tier chunk/results topic+subscription names,
and extends the `cell_command` / `coordinator_command` with the required
Pub/Sub flags (so the pods pass the binary's config validation). Their
fleet-sizing rationale, replica counts, and the coordinator-floor design
decision are preserved untouched.

---

## 4. Every documented assumption

The user explicitly authorized engineering assumptions provided they are
documented. Here they are, in full:

### Assumption 1 — Message schemas (JSON, references not bytes)

- **Dispatch (block):** `{ "height": u64, "tx_count": u64 }`
- **Chunk:** `{ "height": u64, "witness_index": u64, "tx_count": u64 }`
- **Result:** `{ "height": u64, "witness_index": u64, "prove_ms": u64,
  "witness_fetch_ms": u64|null, "ok": bool, "cell": "<hostname>" }`

The witness bytes NEVER travel the bus — only the `{height, witness_index}`
reference (ADR-0008 §1.2). Schemas live in `bench/src/conductor/pubsub.rs`
(`BlockMessage`, `ChunkMessage`, `ChunkResultMessage`) and are
round-trip-unit-tested.

### Assumption 2 — Distribution mechanism (chunk fan-out)

Coordinator → cells over a **dedicated chunk-dispatch Pub/Sub topic**; cells
competing-pull its subscription. This is the defensible choice the brief
offered as the primary option, and it reuses the same competing-pull semantics
ADR-0006 §1.1 already specifies for the outer tier.

### Assumption 3 — Result-reporting mechanism

Cells → coordinator over a **results Pub/Sub topic/subscription**. The
coordinator pulls the results subscription to GATHER/FOLD per block. Chosen
over GCS-per-result (avoids object-write tax per chunk) and over a synchronous
RPC (keeps both roles decoupled and pull-based, matching the conductor model).

### Assumption 4 — Witness mount strategy for cells

The committed `bench/corpus/` `{height, witness_index}` index **and**
`bench_test.json` are **baked into the image** (`COPY ... /app/corpus`, and the
existing `bench_test.json` COPY). At startup the cell builds the in-memory
`MountedCorpus` from `bench_test.json` (the k=1 case, sliced into `S`-tx chunks
indexed `0..k-1`) exactly as the stream path does, and resolves each chunk's
slice locally — the real `witness_fetch_ms` **local-resolve floor** (ADR-0008
§2.1/§2.3, never `witness_move`). A chunk message's `witness_index` selects the
slice (mapped into the local pool by `% pool_total` so a wire index always
resolves to a real slice on the k=1 fixture).
**Future upgrade (documented, not built):** a GCS-backed read-only volume (CSI
FUSE) or a startup download, so cells mount the full multi-height corpus
(#165) instead of the bundled k=1 fixture. The resolver seam (`WitnessResolver`)
is unchanged by that swap.

### Assumption 5 — Pub/Sub client choice (THE big cross-compile call)

**Chosen: shell out to the `gcloud pubsub` CLI** already in the runtime image.

- The native `google-cloud-pubsub` crate pulls a heavy `tokio` + `tonic`
  (gRPC) + TLS dependency tree. There is **zero** async/HTTP/TLS dependency
  anywhere in this workspace today.
- The image is **cross-compiled for `aarch64` (neoverse-v2 / Axion)** on an x86
  Cloud Build worker (`cicd/cloudbuild.yaml`). A new async-TLS tree is a real
  cross-compile risk on that path.
- Shelling to `gcloud` adds **ZERO** Rust dependencies → ZERO cross-compile
  risk, and reuses the exact Application-Default-Credentials / Workload-Identity
  auth the image already supports (it already uses `gcloud storage`).
- Shell-exec latency (tens of ms) is irrelevant next to multi-second ZK proofs.

A native client is a clean future drop-in: it implements the same `BlockQueue`
trait + the same `publish_chunk` / `pull_chunks` / `publish_result` /
`pull_results` shapes. All `gcloud` invocation is isolated in
`bench/src/conductor/pubsub.rs`.

**Known relaxation:** the CLI path uses `--auto-ack` (ack at pull time).
ADR-0006 §1.1's stricter "ack after the block proof is emitted" contract (so a
dead coordinator/cell redelivers its in-flight unit) is what a native
manual-ack client should honor. With auto-ack, a pod that dies mid-prove does
NOT redeliver that unit. This is documented and bounded: the coordinator caps
its GATHER wait so a lost cell cannot hang it, and reports the block as
`block_partial`.

### Assumption 6 — Binary-path reconciliation

The bench binary is installed as **both** `/app/bench` (worker entrypoint) and
`/usr/local/bin/prover` (symlink), so the tfvars `--mode` form works AND the
worker role is untouched. One binary, two names.

### Assumption 7 — Coordinator L2 fold scope (honest gap)

The cell runs the REAL L1 (`BlockTxCircuit`) **and** a REAL single-chunk L2
LEAF chain proof (`BlockTxChainCircuit` folded onto the cyclic base). The
coordinator currently performs the **accounting** fold (collect results, sum
prove/fetch wall, emit per-block completion + lag) — it does NOT yet recursively
merge the cells' L2 leaf proofs into one block chain proof + L4 over the bus
(that needs the cells to ship the proof bytes back, a larger result payload, or
a shared proof store). The cell's per-chunk proof IS real and verified; the
coordinator's cross-cell L2→L4 merge is the named next slice of #75. This is
flagged so no one mistakes the accounting fold for a full block-proof merge.

---

## 5. What was verified locally vs. only on GKE

**Verified locally (host, amd64):**

- `cargo build -p bench` — green (the new modes compile).
- `cargo test -p bench --lib` — 66 tests green, incl. 9 new `pubsub` tests
  (message round-trips, argv construction, base64, pull-JSON parsing).
- `cargo test -p bench --test corpus_mount` — green (witness mount unaffected).
- `bench --mode coordinator|cell --help` parse; missing-config paths fail fast
  with precise errors (proves the wiring + validation, no GCP needed).
- aarch64 cross-compile: the new code adds **no** new dependencies, so the
  cross path is unaffected. A local `cargo check --target
  aarch64-unknown-linux-gnu` compiled all Rust crates (including the new
  `pubsub.rs`) and failed ONLY on the pre-existing `c-kzg`/`blst` C dependency
  needing `aarch64-linux-gnu-gcc`, which is NOT in this sandbox but IS installed
  by `cicd/Containerfile` (`gcc-aarch64-linux-gnu`). So this is a sandbox
  limitation, not a regression.

**Only runs on GKE / against the Pub/Sub emulator (NOT verified locally):**

- The full multi-pod flow: feeder → dispatch → coordinator SPLIT → chunk
  fan-out → cell REAL prove → results → coordinator GATHER. This needs real
  Pub/Sub (or the emulator) + the cross-compiled arm64 image + GKE Autopilot.
- The `gcloud pubsub` invocations against live topics (the argv is unit-tested;
  the live round-trip is not).
- Terraform `validate`/`plan` for the new resources (terraform is not installed
  in this sandbox; the HCL is hand-verified for reference/index/variable
  consistency).

**Local smoke to gain confidence WITHOUT a real fleet** (run against the
Pub/Sub emulator):

```sh
# 1. Start the emulator
gcloud beta emulators pubsub start --host-port=localhost:8085 &
$(gcloud beta emulators pubsub env-init)

# 2. Create the planes (one-time)
for t in dispatch chunk results; do gcloud pubsub topics create lighter-prover-$t --project test; done
gcloud pubsub subscriptions create lighter-prover-dispatch-sub --topic lighter-prover-dispatch --project test
gcloud pubsub subscriptions create lighter-prover-chunk-sub    --topic lighter-prover-chunk    --project test
gcloud pubsub subscriptions create lighter-prover-results-sub  --topic lighter-prover-results  --project test

# 3. Publish a block, run a coordinator (1 block) and a cell (a few chunks)
gcloud pubsub topics publish lighter-prover-dispatch --message='{"height":260138266,"tx_count":16}' --project test
prover --mode coordinator --project test \
  --dispatch-subscription lighter-prover-dispatch-sub \
  --chunk-topic lighter-prover-chunk \
  --results-subscription lighter-prover-results-sub --max-units 1 &
prover --mode cell --project test \
  --chunk-subscription lighter-prover-chunk-sub \
  --results-topic lighter-prover-results --max-units 4
```

(The emulator path requires `gcloud beta emulators pubsub`, which is not
available in this build sandbox; the commands are provided so an operator can
prove the wiring end-to-end before the GKE deploy.)

---

## 6. Risks to know before deploying to real Axion nodes

1. **Auto-ack relaxation (Assumption 5):** a cell that dies mid-prove does not
   redeliver its chunk under the CLI. For a first scaled-down run this is
   acceptable (the coordinator reports `block_partial`); a native manual-ack
   client closes it.
2. **Coordinator L2→L4 merge is accounting-only (Assumption 7):** the per-chunk
   proofs are real; the cross-cell block-proof merge is the next slice.
3. **`gcloud` auth in-pod:** cells/coordinators need a service account with
   `roles/pubsub.subscriber` + `roles/pubsub.publisher` via Workload Identity.
   Add that binding when applying the scale tfvars (the smoke config only
   granted the metrics adapter `monitoring.viewer`).
4. **`--auto-ack` + slow pull cadence:** at high backlog, a cell pulling
   `limit=1` per loop is conservative. Tune `--poll-interval-s` / pull limit
   once a real fleet's throughput is measured.
5. **Witness fixture is k=1 (Assumption 4):** every cell proves slices of the
   same bundled block. Real per-height witnesses arrive via the GCS-volume
   upgrade; until then the prove COST is real but the witness CONTENT repeats.
