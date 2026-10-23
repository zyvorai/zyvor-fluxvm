# FluxVM vs Parallels and VMware Fusion (Apple silicon)

Honest comparison for Mac desktop use. FluxVM is API-first (REST, MCP, fleet) with OCI sandboxes and an optional Docker
Engine API socket; Parallels and Fusion are consumer desktop hypervisors.

| Capability | Parallels | Fusion (Apple silicon) | FluxVM `vz` |
| --- | --- | --- | --- |
| Linux ARM64 VMs | yes | yes | **yes** |
| macOS guests | yes | yes | **yes** |
| Windows 11 ARM | yes (MS authorized) | yes (limited Tools) | **no** (licensing-gated) |
| NAT + port forwards | yes | yes | **yes** |
| Bridged networking | yes | yes | partial (entitlement) |
| Shared folders | yes | Linux yes / Win limited | **yes** (virtiofs) |
| Clipboard | yes | yes (limited on Win ARM) | partial (Linux) |
| Drag-drop files | yes | limited on Win ARM | roadmap |
| Coherence / Unity | Coherence | removed on AS | not planned in 0.4.x |
| Snapshots / clones | yes | yes | **yes** |
| Suspend / pause | yes | yes | **yes** |
| USB / camera / printers | yes | partial | partial / roadmap |
| Docker Engine API | via guest | via guest | **yes** (`docker.sock` → OCI sandboxes) |
| CLI / automation / fleet | limited | `vmrun` | **yes** (API, MCP, fleet) |

## Positioning

- **FluxVM** — API-first Mac hypervisor + governed container sandboxes + fleet. Use `DOCKER_HOST` for Docker CLI against
  OCI sandboxes ([oci-sandboxes.md](oci-sandboxes.md#docker-cli-docker_host)).
- **Parallels** — consumer desktop, especially Windows Coherence.
- **Fusion** — free desktop hypervisor on Apple silicon with Tools limits.

## Windows on Apple silicon

FluxVM does **not** claim Windows-on-Mac as a product feature until Microsoft authorization / OEM rights exist (Parallels model).
Research only until then.
