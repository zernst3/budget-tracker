###############################################################################
# Core infrastructure for the budget tracker (SPEC.md §8), Agora infra pattern
# shrunk to a single-user app:
#
#   Neon (serverless Postgres, scale-to-zero, $0 single-user)
#     project -> branch -> database -> role
#       |
#       v  connection string assembled from Neon outputs
#   Azure Key Vault secret  (neon-database-url)
#       |
#       v  secret reference (Key Vault -> Container App secret)
#   Azure Container App  (scale-to-zero monolith, image from GHCR)
#
#   + Log Analytics (free tier) -> Container Apps environment
#   + User-assigned managed identity with Key Vault access (Get/List secrets)
#
# No secret values are committed; everything secret comes from variables.tf.
###############################################################################

locals {
  name_prefix = "${var.project}-${var.environment}"

  # Tags applied to every Azure resource for cost attribution / cleanup.
  tags = {
    project     = var.project
    environment = var.environment
    managed_by  = "terraform"
  }
}

# ---- Resource group ---------------------------------------------------------

resource "azurerm_resource_group" "main" {
  name     = "${local.name_prefix}-rg"
  location = var.location
  tags     = local.tags
}

# ---- Log Analytics (free-tier-friendly) -------------------------------------
# PerGB2018 SKU; keep ingestion non-verbose to stay under 5 GB/mo (SPEC.md §8).

resource "azurerm_log_analytics_workspace" "main" {
  name                = "${local.name_prefix}-law"
  resource_group_name = azurerm_resource_group.main.name
  location            = azurerm_resource_group.main.location
  sku                 = "PerGB2018"
  retention_in_days   = var.log_retention_days
  tags                = local.tags
}

# ---- User-assigned managed identity -----------------------------------------
# The Container App authenticates to Key Vault with this identity to resolve
# the secret references (no secrets in app config / image).

resource "azurerm_user_assigned_identity" "app" {
  name                = "${local.name_prefix}-id"
  resource_group_name = azurerm_resource_group.main.name
  location            = azurerm_resource_group.main.location
  tags                = local.tags
}

# ---- Key Vault --------------------------------------------------------------
# Holds the Neon connection string + Plaid + auth secrets. The Plaid ACCESS
# TOKEN is written at runtime by the app, never by Terraform
# (BUDGET-PLAID-TOKEN-VAULT-1). RBAC-authorization model.

data "azurerm_client_config" "current" {}

resource "azurerm_key_vault" "main" {
  name                       = substr(replace("${local.name_prefix}-kv", "-", ""), 0, 24)
  resource_group_name        = azurerm_resource_group.main.name
  location                   = azurerm_resource_group.main.location
  tenant_id                  = data.azurerm_client_config.current.tenant_id
  sku_name                   = "standard"
  rbac_authorization_enabled = true
  purge_protection_enabled   = false
  soft_delete_retention_days = 7
  tags                       = local.tags
}

# The deploying principal needs to write secrets during apply.
resource "azurerm_role_assignment" "deployer_kv_admin" {
  scope                = azurerm_key_vault.main.id
  role_definition_name = "Key Vault Secrets Officer"
  principal_id         = data.azurerm_client_config.current.object_id
}

# The Container App's managed identity needs to read secrets at runtime.
resource "azurerm_role_assignment" "app_kv_reader" {
  scope                = azurerm_key_vault.main.id
  role_definition_name = "Key Vault Secrets User"
  principal_id         = azurerm_user_assigned_identity.app.principal_id
}

# ---- Neon: project -> branch -> database -> role ----------------------------

resource "neon_project" "main" {
  name      = local.name_prefix
  region_id = var.neon_region_id
}

resource "neon_branch" "main" {
  project_id = neon_project.main.id
  name       = "main"
}

resource "neon_role" "app" {
  project_id = neon_project.main.id
  branch_id  = neon_branch.main.id
  name       = "budget_app"
}

resource "neon_database" "main" {
  project_id = neon_project.main.id
  branch_id  = neon_branch.main.id
  name       = "budget"
  owner_name = neon_role.app.name
}

# Compute endpoint for the branch — exposes the connection `host` used to
# assemble the connection string. Scale-to-zero is Neon's default; the
# autoscaling floor of 0.25 CU keeps the single-user cost at ~$0 (SPEC.md §8).
resource "neon_endpoint" "main" {
  project_id               = neon_project.main.id
  branch_id                = neon_branch.main.id
  type                     = "read_write"
  autoscaling_limit_min_cu = 0.25
  autoscaling_limit_max_cu = 0.25
  region_id                = var.neon_region_id
}

# ---- Neon connection string -> Key Vault secret -----------------------------
# Assemble the libpq/sqlx URL from Neon outputs and the role password, then
# store it ONLY in Key Vault. The Container App references it (below); the URL
# never lives in app config, the image, or git.

locals {
  # neon_role exposes the connection password via the provider; fall back to
  # the supplied var when a password was provided explicitly for reproducibility.
  neon_password = var.neon_role_password != "" ? var.neon_role_password : neon_role.app.password

  neon_database_url = format(
    "postgresql://%s:%s@%s/%s?sslmode=require",
    neon_role.app.name,
    local.neon_password,
    neon_endpoint.main.host,
    neon_database.main.name,
  )
}

resource "azurerm_key_vault_secret" "neon_database_url" {
  name         = "neon-database-url"
  value        = local.neon_database_url
  key_vault_id = azurerm_key_vault.main.id

  depends_on = [azurerm_role_assignment.deployer_kv_admin]
}

resource "azurerm_key_vault_secret" "plaid_client_id" {
  count        = var.plaid_client_id != "" ? 1 : 0
  name         = "plaid-client-id"
  value        = var.plaid_client_id
  key_vault_id = azurerm_key_vault.main.id

  depends_on = [azurerm_role_assignment.deployer_kv_admin]
}

resource "azurerm_key_vault_secret" "plaid_secret" {
  count        = var.plaid_secret != "" ? 1 : 0
  name         = "plaid-secret"
  value        = var.plaid_secret
  key_vault_id = azurerm_key_vault.main.id

  depends_on = [azurerm_role_assignment.deployer_kv_admin]
}

resource "azurerm_key_vault_secret" "app_auth_secret" {
  count        = var.app_auth_secret != "" ? 1 : 0
  name         = "app-auth-secret"
  value        = var.app_auth_secret
  key_vault_id = azurerm_key_vault.main.id

  depends_on = [azurerm_role_assignment.deployer_kv_admin]
}

# ---- Container Apps environment ---------------------------------------------

resource "azurerm_container_app_environment" "main" {
  name                       = "${local.name_prefix}-cae"
  resource_group_name        = azurerm_resource_group.main.name
  location                   = azurerm_resource_group.main.location
  log_analytics_workspace_id = azurerm_log_analytics_workspace.main.id
  tags                       = local.tags
}

# ---- Container App (scale-to-zero monolith) ---------------------------------
# min_replicas = 0 is the scale-to-zero behavior the lazy-init design relies on
# (SPEC.md §4.6 / BUDGET-IDEMPOTENT-MONTH-INIT-1). The image is pulled from
# GHCR; the Neon URL is injected as a Key Vault secret reference, resolved by
# the managed identity.

resource "azurerm_container_app" "main" {
  name                         = "${local.name_prefix}-app"
  resource_group_name          = azurerm_resource_group.main.name
  container_app_environment_id = azurerm_container_app_environment.main.id
  revision_mode                = "Single"
  tags                         = local.tags

  identity {
    type         = "UserAssigned"
    identity_ids = [azurerm_user_assigned_identity.app.id]
  }

  # GHCR pull credential. The token is a secret value; only the reference name
  # is used in the registry block.
  dynamic "registry" {
    for_each = var.ghcr_pull_token != "" ? [1] : []
    content {
      server               = "ghcr.io"
      username             = var.ghcr_username
      password_secret_name = "ghcr-pull-token"
    }
  }

  dynamic "secret" {
    for_each = var.ghcr_pull_token != "" ? [1] : []
    content {
      name  = "ghcr-pull-token"
      value = var.ghcr_pull_token
    }
  }

  # Key Vault secret reference: Key Vault -> Container App secret -> env var.
  secret {
    name                = "database-url"
    key_vault_secret_id = azurerm_key_vault_secret.neon_database_url.versionless_id
    identity            = azurerm_user_assigned_identity.app.id
  }

  ingress {
    external_enabled = true
    target_port      = var.container_target_port
    transport        = "auto"

    traffic_weight {
      latest_revision = true
      percentage      = 100
    }
  }

  template {
    min_replicas = 0 # scale-to-zero (SPEC.md §8)
    max_replicas = 1 # single-user app

    container {
      name   = var.project
      image  = var.container_image
      cpu    = var.container_cpu
      memory = var.container_memory

      # DATABASE_URL comes from the Key Vault secret reference above.
      env {
        name        = "DATABASE_URL"
        secret_name = "database-url"
      }

      env {
        name  = "TZ"
        value = "America/New_York" # D2 home TZ for month membership
      }

      env {
        name  = "KEY_VAULT_URI"
        value = azurerm_key_vault.main.vault_uri
      }
    }
  }

  depends_on = [
    azurerm_role_assignment.app_kv_reader,
  ]
}
