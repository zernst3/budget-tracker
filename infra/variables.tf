###############################################################################
# Input variables. SECRETS HAVE NO DEFAULTS and NO VALUES committed here —
# they are supplied at apply time (TF_VAR_* env vars or an untracked
# *.tfvars file; .gitignore excludes *.tfvars). Non-secret naming/sizing
# knobs carry sensible defaults.
###############################################################################

# ---- Project / naming -------------------------------------------------------

variable "project" {
  description = "Project name prefix used in all resource names."
  type        = string
  default     = "budget-tracker"
}

variable "environment" {
  description = "Deployment environment (e.g. prod). Single-user app, one env."
  type        = string
  default     = "prod"
}

variable "location" {
  description = "Azure region. Pick one with a nearby Neon region (SPEC.md §8)."
  type        = string
  default     = "eastus"
}

# ---- Azure auth (secrets: no defaults) --------------------------------------

variable "azure_subscription_id" {
  description = "Azure subscription ID."
  type        = string
}

variable "azure_tenant_id" {
  description = "Azure AD tenant ID."
  type        = string
}

# ---- Neon (secrets: no defaults) --------------------------------------------

variable "neon_api_key" {
  description = "Neon API key for the kislerdm/neon provider."
  type        = string
  sensitive   = true
}

variable "neon_region_id" {
  description = "Neon region ID (e.g. azure-eastus2 to colocate with Azure, SPEC.md §8)."
  type        = string
  default     = "azure-eastus2"
}

variable "neon_role_password" {
  description = "Password for the Neon database role. If left empty Neon may auto-generate; supply explicitly for reproducible connection-string assembly."
  type        = string
  sensitive   = true
  default     = ""
}

# ---- GitHub (secrets: no defaults) ------------------------------------------

variable "github_token" {
  description = "GitHub PAT for the integrations/github provider (used for Actions secrets/variables wiring and GHCR)."
  type        = string
  sensitive   = true
}

variable "github_owner" {
  description = "GitHub owner/org that holds the repo and the GHCR image."
  type        = string
  default     = "zernst3"
}

variable "github_repository" {
  description = "GitHub repository name."
  type        = string
  default     = "budget-tracker"
}

# ---- Application image / container -------------------------------------------

variable "container_image" {
  description = "Fully-qualified GHCR image reference for the monolith (e.g. ghcr.io/zernst3/budget-tracker:latest). The CI workflow builds + pushes this."
  type        = string
  default     = "ghcr.io/zernst3/budget-tracker:latest"
}

variable "ghcr_username" {
  description = "GHCR username (usually the GitHub owner) for the Container App registry pull credential."
  type        = string
  default     = "zernst3"
}

variable "ghcr_pull_token" {
  description = "GHCR pull token (PAT with read:packages) for the Container App to pull the private image. Secret: no default."
  type        = string
  sensitive   = true
  default     = ""
}

variable "container_cpu" {
  description = "vCPU per replica. 0.25 is the smallest Consumption-plan size."
  type        = number
  default     = 0.25
}

variable "container_memory" {
  description = "Memory per replica. Must pair with container_cpu per Container Apps allowed combos."
  type        = string
  default     = "0.5Gi"
}

variable "container_target_port" {
  description = "Port the monolith listens on inside the container."
  type        = number
  default     = 8080
}

# ---- Application secrets (no defaults) ---------------------------------------
# These are written into Key Vault and referenced by the Container App. The
# Plaid access token itself is NOT here — it is exchanged at runtime and stored
# as a Key Vault secret reference by the app (BUDGET-PLAID-TOKEN-VAULT-1).

variable "plaid_client_id" {
  description = "Plaid client_id (Transactions product only, SPEC.md §6)."
  type        = string
  sensitive   = true
  default     = ""
}

variable "plaid_secret" {
  description = "Plaid secret for the configured environment."
  type        = string
  sensitive   = true
  default     = ""
}

variable "plaid_environment" {
  description = "Plaid environment (sandbox | development | production)."
  type        = string
  default     = "sandbox"
}

variable "app_auth_secret" {
  description = "Server-side session/JWT signing secret for single-user auth (SPEC.md §9)."
  type        = string
  sensitive   = true
  default     = ""
}

# ---- Log Analytics ----------------------------------------------------------

variable "log_retention_days" {
  description = "Log Analytics retention. Keep low; stay under the 5 GB/mo free tier (SPEC.md §8)."
  type        = number
  default     = 30
}
