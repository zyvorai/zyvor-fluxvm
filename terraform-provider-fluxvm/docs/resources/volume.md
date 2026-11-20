---
page_title: "fluxvm_volume Resource - FluxVM"
subcategory: ""
description: |-
  A block volume on one Mac. Requires central with volumes.
---

# fluxvm_volume (Resource)

A block volume that lives on one Mac and is attached to a VM at launch (`fluxvm_vm.volumes`). Growing `size_gib`
resizes it in place; shrinking it plans a replacement (and loses the data). Central refuses to delete a volume that is
attached, so destroy the VM first (Terraform does when the VM references the volume).

**Requires central with volumes** (`POST/GET /fleet/volumes`, `DELETE /fleet/volumes/{name}`,
`POST /fleet/volumes/{name}/resize`).

## Example Usage

```terraform
resource "fluxvm_volume" "cache" {
  name     = "xcode-cache"
  size_gib = 100
  node     = fluxvm_lease.ci.node
}
```

## Argument Reference

- `name` (String, Required) Unique within the tenant: 1-40 of `a-z`, `0-9` and `-`, starting and ending with a letter
  or digit, not starting with `vol-`. Replaces.
- `size_gib` (Number, Required) 1-2048. Grows in place (only while detached); a smaller value replaces.
- `node` (String) The Mac; otherwise central picks one (a Mac the tenant leases first). Replaces.

## Attribute Reference

- `id`, `tenant`, `attached_to` (VM id), `created_at`.

## Import

```shell
terraform import fluxvm_volume.cache xcode-cache
```
