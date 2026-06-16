.PHONY: help cloud-deploy

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-24s\033[0m %s\n", $$1, $$2}'

cloud-deploy: ## Run Cloud Build to deploy infrastructure using Terraform (infra-as-code/cloudbuild.yaml)
	@bash scripts/cloud.sh cloud-deploy
