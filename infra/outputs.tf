###############################################################################
# Outputs. The connection string is intentionally NOT output (it lives only in
# Key Vault). Non-secret references for the interactive first deploy.
###############################################################################

output "resource_group_name" {
  description = "Azure resource group holding all resources."
  value       = azurerm_resource_group.main.name
}

output "container_app_fqdn" {
  description = "Public FQDN of the Container App."
  value       = azurerm_container_app.main.ingress[0].fqdn
}

output "key_vault_uri" {
  description = "Key Vault URI the app reads secret references from."
  value       = azurerm_key_vault.main.vault_uri
}

output "managed_identity_client_id" {
  description = "Client ID of the user-assigned identity the app authenticates with."
  value       = azurerm_user_assigned_identity.app.client_id
}

output "neon_project_id" {
  description = "Neon project ID."
  value       = neon_project.main.id
}

output "neon_endpoint_host" {
  description = "Neon compute endpoint host (the connection endpoint)."
  value       = neon_endpoint.main.host
}
