.PHONY: help container-build container-run cloud-admin-init cloud-admin-undo cloud-bench-run cloud-run-distributed-cluster cloud-gke-provision cloud-gke-bench cloud-gke-bench-csweep csweep-report cloud-gke-destroy test-t2d-hypothesis test-gke-tax test-capstone verify-enhanced-proof-validity cloud-deploy cloud-plan cloud-destroy cloud-vm-start cloud-vm-stop cloud-zkp-build zkp-image local-build local-run local-build-and-run test-distributed-fast bench-reduction-local lint-reports

# Dynamic GKE architecture default (defaults to 'all' unless ARCH is explicitly overridden on command line)
GKE_ARCH = $(if $(filter command line,$(origin ARCH)),$(ARCH),all)

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-24s\033[0m %s\n", $$1, $$2}'

container-build: ## Build local ZKP STARK container image using podman/docker (infra-as-code/scripts/container.sh)
	@bash infra-as-code/scripts/container.sh container-build $(ARCH)

container-run: ## Run local STARK performance benchmark container against test fixture (infra-as-code/scripts/container.sh)
	@bash infra-as-code/scripts/container.sh container-run $(BLOCK)

zkp-image: container-build ## Alias for container-build

cloud-zkp-build: ## Build and push isolated ZKP STARK container image on GCP via Cloud Build (infra-as-code/cloudbuild-zkp.yaml)
	@TAG="$(TAG)" bash infra-as-code/scripts/cloud.sh cloud-zkp-build $(ARCH)

JOBS ?= 1
CHUNK ?= 1
# (#321 Phase 9) DEFAULT chunk size for the GKE bench path specifically. C=4
# (125 leaves for the 500-tx block) massively outperforms C=1 (500 leaves): the
# attempt-45/46 GKE runs used C=1 and suffered a leaf-count explosion (the
# Phase-1 sizing analysis showed C=4 is far better). Scoped to `cloud-gke-bench`
# only — the other cloud paths (cloud-bench-run / cloud-run-distributed-cluster)
# keep the C=1 default, and the local `test-distributed-fast` harness sets its
# own C independently in container.sh. Override with `GKE_CHUNK=<n>`.
GKE_CHUNK ?= 4
BLOCKS ?= 2
ENGINE ?= gke
ARCH ?= c3d
RADIX ?= 16
# #321 Phase 8: reduction is the DEFAULT fold strategy on GKE; opt out with
# `FOLD_STRATEGY=hex make cloud-gke-bench ...`. Empty passes through to the
# cloud.sh/CLI reduction default, so an unset var keeps the reduction default.
FOLD_STRATEGY ?= reduction
IMAGE := $(if $(IMAGE),$(IMAGE),default)
cloud-bench-run: ## Run remote ZKP benchmark container across GCE VMs (defaults to ALL VMs in config.toml)
	@bash infra-as-code/scripts/cloud.sh cloud-bench-run "$(VM)" "$(JOBS)" "$(CHUNK)" "$(IMAGE)" "$(BENCHMARK_ID)"

cloud-run-distributed-cluster: ## Run collaborative cloud distributed proving experiment (accepts ENGINE=gke/mig ARCH=c4a/c3d/t2d BLOCKS=2 CHUNK=1 RADIX=2)
	@bash infra-as-code/scripts/cloud.sh cloud-run-distributed-cluster --engine=$(ENGINE) --arch=$(ARCH) --blocks=$(BLOCKS) --chunk=$(CHUNK) --radix=$(RADIX)

cloud-gke-provision: ## Provision GKE cluster(s) (accepts ARCH=c4a/c3d/t2d/c4d/all, defaults to all)
	@bash infra-as-code/scripts/cloud.sh cloud-gke-provision --arch=$(GKE_ARCH)

cloud-gke-bench: ## Run benchmark on existing GKE cluster(s) (accepts ARCH=c4a/c3d/t2d/c4d/all, defaults to all; BLOCKS=10 IMAGE=amd64/arm64 RADIX=16 FOLD_STRATEGY=reduction|hex)
	@bash infra-as-code/scripts/cloud.sh cloud-gke-bench --arch=$(GKE_ARCH) --blocks=$(BLOCKS) --chunk=$(GKE_CHUNK) --image=$(IMAGE) --radix=$(RADIX) --fold-strategy=$(FOLD_STRATEGY) --benchmark-id=$(BENCHMARK_ID)

# --- C-sweep (#321): sweep chunk size C to find the CPU-optimal (throughput)
# operating point. The lever is TOTAL CPU per block (core-sec/block): fewer
# core-sec/block => smaller fleet at a given blocks/sec. This is a THIN LOOP over
# `cloud-gke-bench` — one submit per C, each with a DISTINCT benchmark-id
# `${BENCHMARK_ID}-c${C}` (so #337 applies C for real and artifacts don't clash).
#
# C must EVENLY DIVIDE txs_per_block=500 (#337). Valid: 1 2 4 5 10 20 25 50 100
# 125 250 500. The default sweep uses 1 2 4 5.
#
# Dry-run the loop + arg construction (no GCP submit):
#     make -n cloud-gke-bench-csweep C_VALUES="1 4"
C_VALUES ?= 1 2 4 5
BENCHMARK_ID ?= csweep
cloud-gke-bench-csweep: ## Sweep chunk size C over C_VALUES="1 2 4 5"; one cloud-gke-bench per C with benchmark-id ${BENCHMARK_ID}-c${C} (C must divide txs_per_block=500)
	@bash infra-as-code/scripts/csweep_validate.sh $(C_VALUES)
	$(foreach C,$(C_VALUES),$(MAKE) cloud-gke-bench GKE_CHUNK=$(C) BENCHMARK_ID=$(BENCHMARK_ID)-c$(C) ARCH=$(ARCH) BLOCKS=$(BLOCKS) IMAGE=$(IMAGE) RADIX=$(RADIX) FOLD_STRATEGY=$(FOLD_STRATEGY);)

csweep-report: ## Compare THROUGHPUT across a C-sweep: make csweep-report SUMMARIES="a/bench_summary.json b/bench_summary.json" (local paths or gs:// URIs)
	@python3 infra-as-code/scripts/csweep_report.py $(SUMMARIES)

cloud-gke-destroy: ## Tear down GKE cluster(s) (accepts ARCH=c4a/c3d/t2d/c4d/all, defaults to all)
	@bash infra-as-code/scripts/cloud.sh cloud-gke-destroy --arch=$(GKE_ARCH)

test-t2d-hypothesis: ## (DISABLED #282) t2d vs c4a AB race - fails loudly; use cloud-run-distributed-cluster for a real run
	@bash infra-as-code/scripts/cloud.sh cloud-test-t2d-hypothesis

test-gke-tax: ## (DISABLED #282) GKE overlay-tax benchmark - fails loudly; use cloud-run-distributed-cluster for a real run
	@bash infra-as-code/scripts/cloud.sh cloud-test-gke-performance-tax

test-capstone: ## (DISABLED #282) six-release capstone matrix - fails loudly; use cloud-run-distributed-cluster for a real run
	@bash infra-as-code/scripts/cloud.sh cloud-test-capstone-matrix

verify-enhanced-proof-validity: ## Verify authentic production cloud STARK proof calldata against EVM via containerized podman Foundry runner
	@bash infra-as-code/scripts/container.sh verify-enhanced-proof-validity




cloud-vm-start: ## Start GCE VM instances (defaults to ALL VMs in config.toml unless VM=<id> is specified)
	@bash infra-as-code/scripts/cloud.sh cloud-vm-start $(VM)

cloud-vm-stop: ## Stop GCE VM instances (defaults to ALL VMs in config.toml unless VM=<id> is specified)
	@bash infra-as-code/scripts/cloud.sh cloud-vm-stop $(VM)

cloud-admin-init: ## Bootstrap target GCP Service Accounts & IAM roles (Owner-tier)
	@bash infra-as-code/scripts/cloud.sh cloud-admin-init

cloud-admin-undo: ## Tear down target GCP Service Accounts & IAM roles (Owner-tier)
	@bash infra-as-code/scripts/cloud.sh cloud-admin-undo

cloud-deploy: ## Run Cloud Build to deploy infrastructure using Terraform (infra-as-code/cloudbuild.yaml)
	@bash infra-as-code/scripts/cloud.sh cloud-deploy

cloud-plan: ## Run Cloud Build to preview infrastructure changes (Terraform plan)
	@bash infra-as-code/scripts/cloud.sh cloud-plan

cloud-destroy: ## Run Cloud Build to tear down infrastructure (Terraform destroy)
	@bash infra-as-code/scripts/cloud.sh cloud-destroy

local-build: ## Build local ZKP benchmark binary using native host CPU instructions (bench/Makefile)
	@rm -f bench/bench
	@$(MAKE) -C bench build

local-run: ## Run local ZKP benchmark binary against test block (bench/Makefile)
	@$(MAKE) -C bench run

local-build-and-run: ## Build and run local ZKP benchmark binary (bench/Makefile)
	@rm -f bench/bench
	@$(MAKE) -C bench build-and-run

test-distributed-fast: ## Execute 2-minute scaled developer distributed proving simulation (C=4 chunks over local Pub/Sub)
	@bash infra-as-code/scripts/container.sh test-distributed-fast

bench-reduction-local: ## Run local reduction pipeline, capture logs, and extract a provenance-stamped sizing report (#321 Phase 7 / #328)
	@bash infra-as-code/scripts/container.sh bench-reduction-local

lint-reports: ## Anti-fabrication guard: fail if fabricated benchmark metrics reappear in scripts or reports (#282)
	@bash infra-as-code/scripts/check_no_fabricated_reports.sh
