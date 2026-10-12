---
page_title: "fluxvm_images Data Source - FluxVM"
subcategory: ""
description: |-
  The image catalog.
---

# fluxvm_images (Data Source)

The images the operator has put on the Macs (`GET /fleet/images`).

## Example Usage

```terraform
data "fluxvm_images" "macos" {
  guest_os = "macos"
}

output "macos_images" {
  value = data.fluxvm_images.macos.names
}
```

## Argument Reference

- `guest_os` (String) Only `macos` or only `linux` images.

## Attribute Reference

- `names` (List of String) Sorted.
- `images` (List of Object) `name`, `display_name`, `description`, `guest_os`, `image` (the disk path on each Mac),
  `vcpus`, `memory_mib`, `disk_size_gib`, `source`.
