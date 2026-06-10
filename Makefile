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
#   LIGHTER_REF       Upstream commit the container is built against
#                     (default: 5bbb307, last commit before upstream #9)
#   TARGET_CPU_NATIVE Set to 1 for non-portable native-CPU build
#
# Example:
#   make local-bench TX_PER_PROOF=2 TX_LIMIT=480
#   make local-fanout N=4
#   make cloud-bench-build LIGHTER_REF=<new-sha>

.PHONY: help \
  local-init local-clean local-build local-test local-bench local-fanout local-lint \
  container-init container-clean container-build container-run \
  container-test container-bench container-fanout \
  local-build-alias local-fanout-alias \
  cloud-help admin-cloud-init admin-cloud-destroy \
  cloud-preflight cloud-infra cloud-bench-build cloud-app-deploy \
  cloud-app-promote cloud-app-undeploy cloud-clean \
  cloud-status cloud-recover \
  logs-list logs-last logs-clean

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

# ─── Operator notes ──────────────────────────────────────────────────
# - ORCH_FORCE_RESTART=1 on any admin-cloud-* / cloud-* target invalidates
#   the stepwise checkpoint and restarts the run from step 1. Step
#   idempotency is a contract; restart is always safe.
# - Every Make target is a thin wrapper around scripts/. Never invoke
#   the scripts directly — go through `make <target>` so logging, trap
#   handlers, and the heartbeat/checkpoint machinery engage.
