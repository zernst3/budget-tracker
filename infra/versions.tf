###############################################################################
# Terraform + provider version constraints, and the remote-state backend stub.
#
# This is the single source of truth for BOTH Azure and Neon (SPEC.md §8): the
# Container Apps environment + app, Key Vault, Log Analytics, the managed
# identity, the Neon project/branch/database/role, and the wiring of the Neon
# connection string -> Key Vault secret -> Container App secret reference.
#
# No secret VALUES live in this config. Every secret is a variable
# (see variables.tf) sourced at apply time from a .tfvars file or TF_VAR_*
# environment variables. Zach provisions the accounts and supplies the values;
# the routine writes only the code (SPEC.md §12, Phase-2 build setup).
###############################################################################

terraform {
  required_version = ">= 1.9.0"

  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 4.0"
    }
    # Community provider for Neon serverless Postgres.
    # https://registry.terraform.io/providers/kislerdm/neon/latest
    neon = {
      source  = "kislerdm/neon"
      version = "~> 0.13"
    }
    github = {
      source  = "integrations/github"
      version = "~> 6.0"
    }
  }

  # Remote-state backend stub. Uncomment and fill in once the state storage
  # account exists (Zach provisions it during the first interactive deploy).
  # Until then, Terraform uses local state. State is NOT committed (.gitignore
  # excludes *.tfstate). The backend block cannot reference variables, so these
  # values are supplied via `terraform init -backend-config=...` or filled in
  # directly when the storage account is created.
  #
  # backend "azurerm" {
  #   resource_group_name  = "budget-tracker-tfstate-rg"
  #   storage_account_name = "budgettrackertfstate"
  #   container_name       = "tfstate"
  #   key                  = "budget-tracker.tfstate"
  # }
}

provider "azurerm" {
  features {}

  subscription_id = var.azure_subscription_id
  tenant_id       = var.azure_tenant_id
}

provider "neon" {
  # NEON_API_KEY env var or api_key = var.neon_api_key.
  api_key = var.neon_api_key
}

provider "github" {
  # GITHUB_TOKEN env var or token = var.github_token.
  token = var.github_token
  owner = var.github_owner
}
