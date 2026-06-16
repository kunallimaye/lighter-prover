.PHONY: help cloud-admin-init cloud-admin-undo cloud-deploy cloud-plan cloud-destroy zkp-image

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-24s\033[0m %s\n", $$1, $$2}'

zkp-image: ## Build isolated ZKP STARK generation container image (Dockerfile.zkp)
	docker build -f Dockerfile.zkp -t lighter-zkp-prover:latest .

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
