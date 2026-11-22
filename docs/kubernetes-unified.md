# One k3s lifecycle for Mac and Linux

FluxVM had two k3s paths: `fluxctl kube up` (cloud-init provisioned, a dev front door) and `fluxctl kube apple` (journalled,
checksum-verified, HA, upgrade, backup, recovery), the second written for Apple Virtualization only. On Linux `kube up` and
`docker up` never finished: they waited forever for "the guest's address", because QEMU user-mode networking gives the guest no
address the host can reach (found on a Linux x86_64 KVM host).

## Decision

* One lifecycle engine: **`fluxctl kube cluster`** (the code in `crates/fluxctl/src/kube_apple/`). `fluxctl kube apple` stays as an
  alias and prints a one-line deprecation note on stderr.
* The VM backend is a runtime value, `vm_net::Backend` (`Vz` on macOS, `Qemu` on Linux). It decides the VM request's network, the
  guest architecture (arm64 under `Vz`, the host's under `Qemu`) and which features exist. The lifecycle sees it only through the
  `Host` trait (`backend()`, `ssh_forward_port()`), so the fake host in the tests still works.
* `kube up` and `docker up` remain the simple front doors. They share the backend layer, which is what fixes Linux. They are not yet
  re-implemented on top of the engine (see "Not done").

## How a Linux guest is reached

Under `Qemu` the VM request carries one forward, `127.0.0.1:<port> -> guest 22`, bound to the host's loopback only. The port starts
at a stable slot for the VM name (22000-22999, the slot `fluxctl stack` uses) and moves up to the first port nothing listens on, so a
second cluster or a busy port does not collide. The request is the VM record: the daemon stores it and `VmApi::guest_ip` reads the
port back as the endpoint `127.0.0.1:<port>`. `wait_ssh`, every ssh, `ssh-keyscan`, the API tunnel (`ssh -L`) and the Docker socket
forward use that endpoint, so there is no address to wait for. macOS keeps its request unchanged (no forward).

A tap network for a root daemon with a bridge is not built; `network_spec` is where it would go.

## Mac-only features, and what Linux says

| Feature | On Linux |
| --- | --- |
| `--servers N>1`, `join` of a local node, `kube up --workers` | refused: needs the private VM-to-VM switch (vz) |
| `--rosetta` | refused: Apple silicon only |
| `tunnel --install-agent` | refused: LaunchAgent; run `tunnel NAME --foreground` from a systemd unit |
| k3s binary and checksum file | `k3s` and `sha256sum-amd64.txt` on x86-64 (Mac: `k3s-arm64`, `sha256sum-arm64.txt`) |

Single-server create, stop/start, snapshots, backup/restore, upgrade, token rotation, rebuild, recover and doctor use only ssh and
the daemon.

## Migration

* Scripts using `fluxctl kube apple ...` keep working; switch to `fluxctl kube cluster ...` when convenient. State (journal
  directory, ownership labels, pinned host keys, kubeconfig `~/.kube/fluxvm-apple-<name>.yaml`) is unchanged.
* On Linux, `docker up` and `kube up` now create the VM with the loopback ssh forward. A VM an older build made on Linux has no
  forward and was never reachable: delete it and run `up` again.

## Not done

* `kube up` is not a thin wrapper over the engine yet (Cilium, service publishing, the supervisor and `--offline` live in it).
* Workers and HA on Linux need a QEMU private network in the daemon.
* The ssh port is not copied into the journal; the VM record is the source.
