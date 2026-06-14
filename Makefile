# Lighter prover bench — Phase 1 Makefile (issue #2).
#
# All targets are thin wrappers around scripts/. Never invoke the scripts
# directly — go through `make <target>` so logging, trap handlers, and
# the heartbeat/checkpoint machinery in scripts/common.sh engage.
#
# Knobs (export at make-invoke time):
#   TX_PER_PROOF      Chunk size handed to bench (default: 4)
#   TX_LIMIT          Cap on txs consumed from bench_test.json (default: 480)
#   N                 Worker count for fan-out targets (default: 4)
#   TARGET_CPU_NATIVE Set to 1 for non-portable native-CPU build
#
# Note: there is no LIGHTER_REF knob — the container build derives it
# from `git rev-parse HEAD` (local) or $COMMIT_SHA (Cloud Build) so the
# :ref-<short> tag and GIT_SHA env truthfully name the source baked in
# (see issue #15 and ADR-0001 §Revision 1).
#
# Example:
#   make local-bench TX_PER_PROOF=2 TX_LIMIT=480
#   make local-fanout N=4
#   make cloud-bench-build

.PHONY: help \
  local-init local-clean local-build local-test local-bench local-fanout local-lint \
  container-init container-clean container-build container-run \
  container-test container-bench container-fanout \
  local-build-alias local-fanout-alias \
  cloud-help admin-cloud-init admin-cloud-destroy \
  cloud-preflight cloud-infra cloud-bench-build cloud-app-deploy \
  cloud-app-promote cloud-app-undeploy cloud-clean \
  cloud-txmix-build cloud-txmix-deploy cloud-txmix-smoke \
  cloud-txmix-capture cloud-txmix-results cloud-txmix-post \
  gke-smoke-up gke-smoke-validate gke-smoke-down \
  cloud-status cloud-recover \
  logs-list logs-last logs-clean \
  fleet-quota-check fleet-run fleet-run-dry fleet-status \
  fleet-collect fleet-publish fleet-teardown \
  stream-fetch-trace stream-record stream-replay stream-bench \
  stream-test stream-smoke stream-sweep \
  s-calibrate s-calibrate-fleet calibration-check \
  fleet-size fleet-size-test

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-24s\033[0m %s\n", $$1, $$2}'

# ─── Local (host cargo, no container) ────────────────────────────────

local-init: ## Verify Rust toolchain is installed (triggers rustup install of pinned nightly)
	@bash scripts/local.sh init

local-clean: ## cargo clean -p bench
	@bash scripts/local.sh clean

local-build: ## cargo build --release -p bench --bin bench
	@bash scripts/local.sh build

local-test: ## Smoke test: bench at tx_per_proof=1 tx_limit=4, assert TOTAL line
	@bash scripts/local.sh test
	@$(MAKE) -C bench feeder-test
	-@bash scripts/calibration-check.sh

local-bench: ## Full bench run (TX_PER_PROOF/TX_LIMIT overridable)
	@bash scripts/local.sh bench

local-fanout: ## N concurrent local bench processes; aggregate via orchestrator parser
	@bash scripts/local.sh fanout

local-lint: ## cargo fmt --check + cargo clippy on bench
	@bash scripts/local.sh lint

# ─── Container (Podman) ──────────────────────────────────────────────
# Issue #2 acceptance criteria use the names: local-build / local-test /
# local-bench / local-fanout. Per the makefile-ops scaffold convention,
# scripts/local.sh covers host-only flows, and scripts/container.sh
# covers podman flows. The acceptance-criteria names map to the
# container path (operator-visible image build, image-based bench,
# image-based fan-out). We provide both naming conventions.

container-init: ## Verify podman is installed
	@bash scripts/container.sh init

container-clean: ## Remove bench containers and the local image
	@bash scripts/container.sh clean

container-build: ## Build cicd/Containerfile -> localhost/lighter-bench:latest
	@bash scripts/container.sh build

container-run: ## Run one worker container (defaults)
	@bash scripts/container.sh run

container-test: ## Smoke test: container at tx_per_proof=1 tx_limit=4, assert TOTAL line
	@bash scripts/container.sh test

container-bench: ## Full bench in a single container
	@bash scripts/container.sh bench

container-fanout: ## N concurrent worker containers; aggregate via orchestrator
	@bash scripts/container.sh fanout

# Issue #2 acceptance-criteria target names. Aliased onto the container
# path because the criteria explicitly call for OCI image + N containers,
# not host processes. The host-only equivalents live behind
# `make local-build`, `make local-bench`, etc. above.
.PHONY: build test bench fanout
build: container-build ## Alias: build the OCI image (issue #2 acceptance)
test: container-test ## Alias: container smoke test (issue #2 acceptance)
bench: container-bench ## Alias: full container bench (issue #2 acceptance)
fanout: container-fanout ## Alias: N-container fan-out (issue #2 acceptance)

# ─── Cloud Runtime (three-role topology: orchestration / build / runtime) ──

cloud-help: ## Print the resolved three-role topology
	@bash scripts/cloud.sh help

admin-cloud-init: ## Owner-tier bootstrap: APIs, AR, TF state, builder SA, custom role.
	@bash scripts/cloud.sh admin-cloud-init

admin-cloud-destroy: ## Owner-tier teardown of bootstrap (preserves TF state + AR by default)
	@bash scripts/cloud.sh admin-cloud-destroy

cloud-preflight: ## Read-only audit of APIs, AR, builder SA roles
	@bash scripts/cloud.sh cloud-preflight

cloud-infra: ## TF apply via Cloud Build (provisions AR repo + IAM)
	@bash scripts/cloud.sh cloud-infra

cloud-bench-build: ## Build + push the bench container via Cloud Build (issue #2 acceptance)
	@bash scripts/cloud.sh cloud-bench-build

cloud-app-deploy: ## Image build + Cloud Run revision swap (deferred to Phase 2 runtime)
	@bash scripts/cloud.sh cloud-app-deploy

cloud-app-promote: ## Tag + deploy to non-staging runtime. Requires VERSION + IMAGE
	@bash scripts/cloud.sh cloud-app-promote

cloud-app-undeploy: ## Revert Cloud Run to placeholder image
	@bash scripts/cloud.sh cloud-app-undeploy

cloud-clean: ## TF destroy (runtime infrastructure)
	@bash scripts/cloud.sh cloud-clean

# ─── tx-mix Capture Job (Tokyo Cloud Run Job, issue #128) ────────────
# Reusable, parametrised capture of the mainnet tx-type mix. The SAME job
# runs a tiny SMOKE window and a big REPRESENTATIVE window by config alone.

cloud-txmix-build: ## Build + push the tx-mix capture image via Cloud Build (#128)
	@bash scripts/cloud-txmix.sh build

cloud-txmix-deploy: ## Create/update the Tokyo tx-mix Cloud Run JOB (#128)
	@bash scripts/cloud-txmix.sh deploy

cloud-txmix-smoke: ## SMOKE-run the job: tiny window (small-N validation, NOT the answer) (#128)
	@bash scripts/cloud-txmix.sh smoke

cloud-txmix-capture: ## OPERATOR representative capture: set TXMIX_HEIGHTS="LO HI" or TXMIX_BLOCKS=N (#128)
	@bash scripts/cloud-txmix.sh capture

cloud-txmix-results: ## Print the latest tx-mix GCS artifact (meta + mix + DONE) (#128)
	@bash scripts/cloud-txmix.sh results

cloud-txmix-post: ## Post the cited tx-mix summary (from GCS) to issue #128
	@bash scripts/cloud-txmix.sh post

# ─── GKE Autopilot deployment automation (issue #151, G4 enabler) ────
# Parametrised GKE-Autopilot deployment of the ADR-0006 two-machine-class
# topology (chunk-prover cells + coordinators) with the ADR-0003-amendment
# HARD DAY-1 eviction mitigation (coordinator safe-to-evict=false + PDB)
# and autoscaling on Pub/Sub backlog. Runs via Cloud Build as a GKE-capable
# service account (set GKE_BUILD_SA). The smoke config (smoke.tfvars) is a
# tiny, trivial-workload validation of the AUTOMATION — NOT real proving
# load (gated on G2) and NOT production sizes (gated on sizing #95). Feed
# production sizes via production.tfvars (same variable surface) later.
#
# Knobs: GKE_PROJECT=, GKE_REGION= (default us-central1, must support
# c4a/Axion), GKE_CLUSTER=, GKE_BUILD_SA= (GKE-capable SA), GKE_TF_BUCKET=,
# GKE_BACKLOG_MSGS= (HPA test backlog size).

gke-smoke-up: ## Stand up + validate the GKE Autopilot smoke topology (cluster, both classes, eviction mitigation, HPA-on-backlog)
	@bash scripts/gke-smoke.sh up

gke-smoke-validate: ## Alias of gke-smoke-up (the up pipeline includes the live validation)
	@bash scripts/gke-smoke.sh validate

gke-smoke-down: ## Tear down the GKE smoke topology + verify nothing remains (no cluster/nodes/LBs/disks)
	@bash scripts/gke-smoke.sh down

# ─── Detached Orchestration ──────────────────────────────────────────

cloud-status: ## Detached-orchestration status: RUNNING | STALLED | COMPLETE | NEVER_STARTED
	@bash scripts/cloud.sh cloud-status

cloud-recover: ## Read EXIT/HUP trap recovery files
	@bash scripts/cloud.sh cloud-recover

# ─── Logs ────────────────────────────────────────────────────────────

logs-list: ## List recent log files
	@ls -lt logs/*.log 2>/dev/null | head -20 || echo "No log files found"

logs-last: ## Show the most recent log file
	@ls -t logs/*.log 2>/dev/null | head -1 | xargs cat 2>/dev/null || echo "No log files found"

logs-clean: ## Remove all log files
	@rm -rf logs/*.log && echo "Cleaned log files" || true

# ─── GCP fleet benchmark (issue #11, container pivot #33) ────────────
# Wraps scripts/bench-fleet/run-fleet.sh for the 10-VM cross-architecture
# bench sweep. Fleet VMs run Container-Optimized OS and pull prebuilt
# per-microarch images (build them first: make cloud-bench-build).
# See scripts/bench-fleet/README.md for prerequisites (gcloud auth,
# kunal-scratch setup via make admin-cloud-init) and
# docs/decisions/ADR-0007-gcp-fleet-bench-architecture.md for the
# architecture rationale.
#
# Note: `fleet-run` passes --yes to skip the Make-level prompt; the
# underlying script's cost-estimate print remains the safety gate before
# any spend. Call the script directly without --yes for the interactive
# prompt.

FLEET := scripts/bench-fleet/run-fleet.sh

fleet-quota-check: ## Verify GCP quotas for all 10 machine types (read-only, no spend)
	@$(FLEET) quota-check

fleet-run-dry: ## Print the 10 gcloud create commands without executing (no spend)
	@$(FLEET) run --dry-run --yes

fleet-run: ## Provision 10 VMs in parallel, run S in {1,2,4,6} sweep, collect to GCS (~$$80-150, ~6h wall)
	@$(FLEET) run --yes

fleet-status: ## Show current fleet state from GCS (use RUN_ID=<id> for a specific run)
	@$(FLEET) status $(if $(RUN_ID),--run-id $(RUN_ID),)

fleet-collect: ## Pull logs from GCS and parse to TSV (requires RUN_ID=<id>)
	@test -n "$(RUN_ID)" || { echo "error: RUN_ID=<id> required"; exit 1; }
	@$(FLEET) collect --run-id $(RUN_ID)

fleet-publish: ## Render markdown, create Discussion, comment on #6 (requires RUN_ID=<id>)
	@test -n "$(RUN_ID)" || { echo "error: RUN_ID=<id> required"; exit 1; }
	@$(FLEET) publish --run-id $(RUN_ID)

fleet-teardown: ## Force-delete any leftover fleet VMs (optional RUN_ID=<id>, or all)
	@$(FLEET) teardown $(if $(RUN_ID),--run-id $(RUN_ID),--all)

# ─── Streaming bench (issues #47–#49) ────────────────────────────────
# Root operator surface for the streaming producer/consumer pipeline:
# feeder.py (trace producer, #48) piped into `bench --stream` (#49),
# per the trace contract in bench/trace-format.md (#47). All targets
# are thin wrappers around scripts/stream.sh (which cds into bench/ as
# needed); the bench/Makefile keeps its own crate-local targets
# (stream-record/stream-replay/stream-peak/feeder-test/stream-smoke/
# stream-sweep) — root names below do not collide with them.
#
# Knobs: TRACE=, RATE=|SPEED= (exactly one), SYNTH_RATE=, DURATION=,
# LOOP=1, OUT=, TX_PER_PROOF=, MAX_QUEUE=.

stream-fetch-trace: ## Download the banked 15-min mainnet trace to traces/ (gcloud auth required)
	@bash scripts/stream.sh fetch-trace

stream-record: ## Capture a live mainnet trace (network; OUT=, DURATION=)
	@bash scripts/stream.sh record

stream-replay: ## Replay a trace to stdout (TRACE=, RATE=|SPEED=, DURATION=, LOOP=1)
	@bash scripts/stream.sh replay

stream-bench: ## E2E: replay trace into bench --stream (TRACE=+RATE=|SPEED= or SYNTH_RATE=; TX_PER_PROOF=, MAX_QUEUE=, DURATION=)
	@bash scripts/stream.sh bench

stream-test: ## Offline streaming test suites (feeder + consumer; <1 min, no proving)
	@bash scripts/stream.sh test

stream-smoke: ## Manual real-proving smoke (~minutes; not part of any test target)
	@bash scripts/stream.sh smoke

stream-sweep: ## Rate-ladder sweep for max sustained tx/s (long-running; real proving)
	@bash scripts/stream.sh sweep

# ─── Chunk-size calibration (issues #85, #102) ───────────────────────
# Per-machine calibration suite: probes degree-bracket tops only (#60
# step-function finding), RAM-gates infeasible brackets, and reports the
# optimal S per objective (serial fold / tree fold / s-per-tx / SLO
# slack under the 20 s proof-lag budget) plus a BENCH-LEDGER entry for
# Discussion #77.
#
# Knobs (s-calibrate): CAL_SVALUES= (default auto: "8 9 10 11 20 21 32"
# + 40 when RAM clears the 2^20 gate), BLOCK_TX=500, MERGE_S=0.4764,
# L4_WALL=5.155, LAG_P50=20, LAG_P99=40, BLOCK_SIZES="500 4000 9000",
# CAL_L4=0|1 (opt-in measured MERGE_S/L4), OUT_REGISTRY=0|1 (emit
# calibration/<shape>.json + README.md), SHAPE_LABEL=, CHUNKS=4,
# OUT_DIR=, HEADROOM=1.5.
#
# calibration-check (issue #102): warn-only staleness guard comparing
# the circuit/src/** hash in calibration/*.json against the working
# tree. Never fails; also runs as a warning line inside local-test.
#
# s-calibrate-fleet reuses the bench-fleet provisioning path but with
# machines-calibrate.tsv and per-S tx_limit=4*S. It deliberately does
# NOT pass --yes: the script-level cost estimate + interactive prompt is
# the safety gate before any spend (~$10-25 for the 3 c4a shapes).
# The historical comparison fleet (S in {1,2,4,6}, ADR-0003 §D4) is
# untouched -- calibration is a separate, additive mode.

s-calibrate: ## Per-machine chunk-size calibration (CAL_SVALUES=, BLOCK_TX=, MERGE_S=, L4_WALL=, LAG_P50=, CAL_L4=1, OUT_REGISTRY=1, OUT_DIR=)
	@bash scripts/s-calibrate.sh

calibration-check: ## Staleness guard: WARN when calibration/ predates circuit/src changes (never fails)
	@bash scripts/calibration-check.sh

s-calibrate-fleet: ## Cloud calibration on machines-calibrate.tsv shapes (interactive cost gate; SHAPES=, REF=, CAL_L4=1)
	@$(FLEET) calibrate $(if $(SHAPES),--machines "$(SHAPES)",) $(if $(REF),--ref $(REF),) $(if $(filter 1,$(CAL_L4)),--cal-l4,)

# ─── Parametric fleet-sizing model (#95) ─────────────────────────────
# Consumes the MEASURED calibration/*.json constants and emits machines +
# topology (SIZE + SHAPE). Cost is a non-gating overlay only (Discussion
# #77). Pass-through args via ARGS=, e.g.:
#   make fleet-size ARGS="--shape c4a-highcpu-64 --s 9 --blocks-per-s 5 --tx-per-block 9000"
#   make fleet-size ARGS="--self-check"
fleet-size: ## Parametric fleet sizing from measured constants (ARGS="--shape ... --s ... --blocks-per-s ... --tx-per-block ..."; --json; --self-check; --cost-overlay PRICE)
	@python3 scripts/fleet-size.py $(ARGS)

fleet-size-test: ## Golden test for the fleet-sizing model (#95)
	@bash scripts/bench-fleet/tests/test-fleet-size.sh

# ─── Operator notes ──────────────────────────────────────────────────
# - ORCH_FORCE_RESTART=1 on any admin-cloud-* / cloud-* target invalidates
#   the stepwise checkpoint and restarts the run from step 1. Step
#   idempotency is a contract; restart is always safe.
# - Every Make target is a thin wrapper around scripts/. Never invoke
#   the scripts directly — go through `make <target>` so logging, trap
#   handlers, and the heartbeat/checkpoint machinery engage.
