# Lease a Mac, launch a macOS VM from the image catalog with a cache volume on a private network, and get told
# before the lease ends.
#
#   export FLUXVM_ENDPOINT=https://fleet.example.com
#   export FLUXVM_TOKEN=fvt_...            # a tenant's primary token (or a full-access API key)
#   terraform init && terraform apply

terraform {
  required_providers {
    fluxvm = {
      source  = "zyvorai/fluxvm"
      version = "~> 0.1"
    }
  }
}

provider "fluxvm" {
  # endpoint and token default to FLUXVM_ENDPOINT and FLUXVM_TOKEN.
}

variable "webhook_url" {
  type    = string
  default = "https://hooks.example.com/fluxvm"
}

data "fluxvm_images" "macos" {
  guest_os = "macos"
}

# A whole Mac for 72 hours (the macOS licence asks for at least 24 hours of sole use).
resource "fluxvm_lease" "ci" {
  hours                 = 72
  min_memory_mib        = 32768
  nested_virtualization = true
}

# A private network across the Macs the tenant leases.
resource "fluxvm_network" "build" {
  name       = "build"
  nodes      = [fluxvm_lease.ci.node]
  depends_on = [fluxvm_lease.ci]
}

# A cache volume on the leased Mac (requires central with volumes).
resource "fluxvm_volume" "cache" {
  name     = "xcode-cache"
  size_gib = 100
  node     = fluxvm_lease.ci.node
}

resource "fluxvm_vm" "runner" {
  name          = "ci-runner-1"
  catalog_image = contains(data.fluxvm_images.macos.names, "macos-27-xcode") ? "macos-27-xcode" : data.fluxvm_images.macos.names[0]
  node          = fluxvm_lease.ci.node
  networks      = [fluxvm_network.build.name]
  volumes       = [fluxvm_volume.cache.name]

  # Anything else the create request takes, as JSON.
  extra_json = jsonencode({
    labels = { "team" = "ios" }
  })
}

# Signed notifications for the events worth acting on.
resource "fluxvm_webhook" "alerts" {
  url    = var.webhook_url
  events = ["lease.ending", "lease.ended", "budget.near", "budget.over"]
}

# A named key for CI (made with the tenant's primary token; never with another key).
resource "fluxvm_api_key" "github" {
  name            = "github-actions"
  expires_in_days = 90
}

output "vm" {
  value = "${fluxvm_vm.runner.name} on ${fluxvm_vm.runner.node} (${fluxvm_vm.runner.status})"
}

output "network_subnet" {
  value = fluxvm_network.build.subnet
}

output "webhook_secret" {
  value     = fluxvm_webhook.alerts.secret
  sensitive = true
}

output "ci_key" {
  value     = fluxvm_api_key.github.token
  sensitive = true
}
