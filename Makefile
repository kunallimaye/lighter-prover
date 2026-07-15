.PHONY: help container-build container-run cloud-admin-init cloud-admin-undo cloud-bench-run cloud-run-distributed-cluster cloud-gke-provision cloud-gke-bench cloud-gke-destroy test-t2d-hypothesis test-gke-tax test-capstone verify-enhanced-proof-validity cloud-deploy cloud-plan cloud-destroy cloud-vm-start cloud-vm-stop cloud-zkp-build zkp-image local-build local-run local-build-and-run test-distributed-fast bench-reduction-local lint-reports

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
BLOCKS ?= 2
ENGINE ?= gke
ARCH ?= c3d
RADIX ?= 16
IMAGE := $(if $(IMAGE),$(IMAGE),default)
cloud-bench-run: ## Run remote ZKP benchmark container across GCE VMs (defaults to ALL VMs in config.toml)
	@bash infra-as-code/scripts/cloud.sh cloud-bench-run "$(VM)" "$(JOBS)" "$(CHUNK)" "$(IMAGE)" "$(BENCHMARK_ID)"

cloud-run-distributed-cluster: ## Run collaborative cloud distributed proving experiment (accepts ENGINE=gke/mig ARCH=c4a/c3d/t2d BLOCKS=2 CHUNK=1 RADIX=2)
	@bash infra-as-code/scripts/cloud.sh cloud-run-distributed-cluster --engine=$(ENGINE) --arch=$(ARCH) --blocks=$(BLOCKS) --chunk=$(CHUNK) --radix=$(RADIX)

cloud-gke-provision: ## Provision GKE cluster(s) (accepts ARCH=c4a/c3d/t2d/c4d/all, defaults to all)
	@bash infra-as-code/scripts/cloud.sh cloud-gke-provision --arch=$(GKE_ARCH)

cloud-gke-bench: ## Run benchmark on existing GKE cluster(s) (accepts ARCH=c4a/c3d/t2d/c4d/all, defaults to all; BLOCKS=10 IMAGE=amd64/arm64 RADIX=16)
	@bash infra-as-code/scripts/cloud.sh cloud-gke-bench --arch=$(GKE_ARCH) --blocks=$(BLOCKS) --chunk=$(CHUNK) --image=$(IMAGE) --radix=$(RADIX) --benchmark-id=$(BENCHMARK_ID)

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
