# Lighter Prover — bench-fleet operator interface
#
# Wraps scripts/bench-fleet/run-fleet.sh for GCP fleet benchmark orchestration.
# See scripts/bench-fleet/README.md for prerequisites (gcloud auth, bench-sweep
# service account, project setup) and docs/decisions/ADR-0001-* for the
# architecture rationale.
#
# Note: `fleet-run` passes --yes to skip the Make-level prompt; the underlying
# script's cost-estimate print remains the safety gate before any spend.
# Call the script directly without --yes if you want the interactive prompt.

.DEFAULT_GOAL := help
.PHONY: help fleet-quota-check fleet-run fleet-run-dry fleet-status fleet-collect fleet-publish fleet-teardown

FLEET := scripts/bench-fleet/run-fleet.sh

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make <target>\n\nTargets:\n"} \
	      /^[a-zA-Z_-]+:.*## / {printf "  %-22s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

fleet-quota-check: ## Verify GCP quotas for all 10 machine types (read-only, no spend)
	@$(FLEET) quota-check

fleet-run-dry: ## Print the 10 gcloud create commands without executing (no spend)
	@$(FLEET) run --dry-run --yes

fleet-run: ## Provision 10 VMs in parallel, run S in {1,2,4,6} sweep, collect to GCS (~$18, ~1h wall)
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
