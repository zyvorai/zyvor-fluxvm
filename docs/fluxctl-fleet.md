# `fluxctl fleet`: command reference

Every `fluxctl fleet` command talks to a Mac cloud's central registry (`fluxvm-agent central`). This page is generated
from the commands' own `--help`, so it matches the binary; run `fluxctl fleet <command> --help` for the same text.
Concepts (tenants, leases, quotas, drain) are in [mac-cloud-tenancy.md](mac-cloud-tenancy.md); the REST routes behind
each command are in its [API summary](mac-cloud-tenancy.md#api-summary).

## Connecting

| Setting | Flag | Environment | Default |
|---|---|---|---|
| Central's URL | `--central URL` | `CENTRAL_URL` | `http://127.0.0.1:7799` |
| Bearer token: a tenant's primary token, one of its named keys, or the operator's (all start `fvt_`) | `--token TOKEN` | `FLUXVM_AGENT_TOKEN` | none |
| Output | `-o json\|table\|wide` | `FLUXCTL_OUTPUT` | human on a terminal, JSON when piped |

```bash
export CENTRAL_URL=https://cloud.example.com
export FLUXVM_AGENT_TOKEN=fvt_...
fluxctl fleet status
fluxctl fleet vms -o json | jq '.items[].name'     # scripts get JSON
```

- **Output.** On a terminal you get tables and plain sentences, with a suggested next command. Piped, or with
  `-o json`, you get central's JSON unchanged. `-o table` and `-o wide` force the human form (and wide adds columns).
  Colour is off with `NO_COLOR`.
- **Naming a VM.** By the name you gave it, by a unique ID prefix, or as `node/id`.
- **Confirmation.** `stop`, `restart` and `rm` ask `[y/N]` on a terminal. With input that is not a terminal (a script,
  CI) they proceed without asking.
- **What a token may do.** A tenant sees and changes only its own VMs, leases, volumes and pools. A read-only key can
  run the commands that only read and gets `403` on the rest. Members of a tenant who sign in with SSO use the console;
  the CLI always uses a token. Commands marked *(operator)* need the operator token.
- **Exit status.** `0` on success, non-zero on an error. In the human form `fleet run` prints the guest's output and
  exits with the guest command's own status; with `-o json` (or piped) it prints the JSON result and exits `0`.

## Start here

### `fluxctl fleet status`

What you have and what needs attention: a tenant's quota, Macs and next step, or the operator's
fleet health. Start here

```text
Usage: fluxctl fleet status [OPTIONS]

Options:
```

### `fluxctl fleet launch`

Launch a VM from the image catalog (or a disk path on the Mac). Picks a name, the image's default
size and a Mac with room; `--mac` pins one. Example: `fluxctl fleet launch --image macos-27-xcode
--ttl 8h`

```text
Usage: fluxctl fleet launch [OPTIONS] [NAME]

Arguments:
  [NAME]  VM name (default: the image name plus a number)

Options:
      --image <IMAGE>            Catalog image (`fluxctl fleet images`)
      --disk <DISK>              A disk path on the Mac instead of a catalog image
      --guest <GUEST>            Guest OS for `--disk` [possible values: macos, linux]
      --vcpus <VCPUS>            
      --memory-gib <MEMORY_GIB>  
      --mac <MAC>                The Mac to run on
      --network <NETWORK>        Join a private network across Macs (`fluxctl fleet networks`).
                                 Repeatable
      --ttl <TTL>                Delete the VM automatically after this long (`30m`, `8h`, `7d`)
      --volume <VOLUME>          Attach a persistent volume (`fluxctl fleet volumes`), by name or
                                 id. Repeatable. The VM runs on the volumes' Mac; Linux guests only
```

### `fluxctl fleet run`

Run a command in a VM, found by name or ID prefix. Output and exit code are the guest's. Example:
`fluxctl fleet run ios-build-1 -- xcodebuild -version`

```text
Usage: fluxctl fleet run [OPTIONS] <VM> <COMMAND>...

Arguments:
  <VM>          
  <COMMAND>...  

Options:
      --timeout <TIMEOUT>
```

## Power, screen and logs

### `fluxctl fleet start`

Start a stopped VM (by name or ID prefix)

```text
Usage: fluxctl fleet start [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
```

### `fluxctl fleet stop`

Shut a VM down; its disk is kept (asks first on a terminal)

```text
Usage: fluxctl fleet stop [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
  -y, --yes
```

### `fluxctl fleet restart`

Restart a VM (asks first on a terminal)

```text
Usage: fluxctl fleet restart [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
  -y, --yes
```

### `fluxctl fleet pause`

Pause a running VM (its memory is kept; it does not run)

```text
Usage: fluxctl fleet pause [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
```

### `fluxctl fleet resume`

Resume a paused VM

```text
Usage: fluxctl fleet resume [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
```

### `fluxctl fleet logs`

The end of a VM's console log

```text
Usage: fluxctl fleet logs [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
      --lines <LINES>  [default: 100]
```

### `fluxctl fleet screen`

Save a picture of a VM's display (found by name or ID prefix). Example: `fluxctl fleet screen
ios-build-1`

```text
Usage: fluxctl fleet screen [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
  -f, --file <FILE>            Where to write it (default: `<vm>.png`, or `.svg` from a simulated
                               Mac)
      --max-width <MAX_WIDTH>  Largest width in pixels [default: 1920]
```

### `fluxctl fleet exec`

Run a command in a VM through its node's guest agent. REST: `POST /fleet/vms/{node}/{id}/exec`

```text
Usage: fluxctl fleet exec [OPTIONS] <NODE> <ID> <COMMAND>...

Arguments:
  <NODE>        
  <ID>          
  <COMMAND>...  

Options:
      --timeout <TIMEOUT>
```

### `fluxctl fleet rm`

Delete a VM, found by name or ID prefix. Asks first on a terminal

```text
Usage: fluxctl fleet rm [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
  -y, --yes      Do not ask
```

## VMs and nodes (placement view)

### `fluxctl fleet create`

Create a VM through the fleet: residual-capacity placement picks a healthy, uncordoned node unless
`--node` names one explicitly (which bypasses placement entirely, same as `kubectl` with
`spec.nodeName` set — including landing on a cordoned node and ignoring any `--node-selector`). REST
equivalent: `POST /fleet/vms`

```text
Usage: fluxctl fleet create [OPTIONS] --spec <SPEC>

Options:
      --spec <SPEC>                    Path to a `CreateVmRequest` JSON spec, same shape `fluxvm
                                       create --spec` takes
      --node <NODE>                    Exact node name to create on, bypassing automatic placement
      --node-selector <NODE_SELECTOR>  Restrict automatic placement to a node whose labels (set via
                                       `fluxvm-agent node --label key=value`) match, as `key=value`.
                                       Repeatable. Ignored when `--node` is also given. Merged with
                                       (and overriding) any `"nodeSelector"` already in the spec
                                       file, the same override relationship `--node` has with the
                                       spec's own `"node"` field
```

### `fluxctl fleet vms`

The fleet-wide VM list, each entry tagged with which node it's on. Any node this couldn't account
for (unreachable, stale, or erroring) is named in the response's `unreachable_nodes` field rather
than silently missing from `items`. REST equivalent: `GET /fleet/vms`

```text
Usage: fluxctl fleet vms [OPTIONS]

Options:
```

### `fluxctl fleet node-vms`

The VMs on exactly one node, queried directly against that node rather than filtered out of the
fleet-wide list — works even when other nodes in the fleet are unreachable. REST equivalent: `GET
/fleet/nodes/{name}/vms`

```text
Usage: fluxctl fleet node-vms [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

### `fluxctl fleet delete`

Delete a VM on a specific node through the fleet proxy. REST equivalent: `DELETE
/fleet/vms/{node}/{id}`

```text
Usage: fluxctl fleet delete [OPTIONS] <NODE> <ID>

Arguments:
  <NODE>  
  <ID>    

Options:
```

### `fluxctl fleet nodes`

List every registered node, each with its live `healthy`/`cordoned` state and free capacity. REST
equivalent: `GET /fleet/nodes`

```text
Usage: fluxctl fleet nodes [OPTIONS]

Options:
```

### `fluxctl fleet node`

Exactly one node's own record -- the same shape a `Nodes` list entry has, without fetching and
filtering the whole fleet just to check one node's `healthy`/`cordoned`/free-capacity state. REST
equivalent: `GET /fleet/nodes/{name}`

```text
Usage: fluxctl fleet node [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

### `fluxctl fleet cordon`

Exclude a node from automatic (residual-capacity) placement without touching any VM already on it or
deregistering the node — same semantics as `kubectl cordon`. REST equivalent: `POST
/fleet/nodes/{name}/cordon`

```text
Usage: fluxctl fleet cordon [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

### `fluxctl fleet uncordon`

Clear a node's cordon, re-admitting it to automatic placement. REST equivalent: `POST
/fleet/nodes/{name}/uncordon`

```text
Usage: fluxctl fleet uncordon [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

### `fluxctl fleet deregister`

Permanently forget a decommissioned node's registry entry. Only works once the node's heartbeat has
already gone stale — the registry rejects deregistering a still-healthy node (409), since its very
next heartbeat would just re-add it. REST equivalent: `DELETE /fleet/nodes/{name}`

```text
Usage: fluxctl fleet deregister [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

### `fluxctl fleet capacity`

Fleet-wide capacity summary computed from each node's last heartbeat — no proxy calls to any node's
own `fluxctl serve`. REST equivalent: `GET /fleet/capacity`

```text
Usage: fluxctl fleet capacity [OPTIONS]

Options:
```

## Snapshots and clones

### `fluxctl fleet snapshot`

Snapshot a VM (by name or ID prefix): a copy-on-write clone of its disk, in any state. `--memory`
also saves what is running (the VM must be running or paused), so a restore resumes it there.
Example: `fluxctl fleet snapshot ios-build-1 --name before-upgrade`

```text
Usage: fluxctl fleet snapshot [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
      --name <NAME>  Snapshot name (default: `snap-<UTC time>`)
      --memory       Save the guest's memory and device state too
```

### `fluxctl fleet snapshots`

A VM's snapshots, newest first, with their size and whether they hold memory

```text
Usage: fluxctl fleet snapshots [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
```

### `fluxctl fleet restore`

Put a VM back to a snapshot: it is stopped, its disk swapped back from the snapshot, and started
again. Asks first on a terminal

```text
Usage: fluxctl fleet restore [OPTIONS] <VM> <SNAPSHOT>

Arguments:
  <VM>        
  <SNAPSHOT>  

Options:
  -y, --yes
```

### `fluxctl fleet snapshot-rm`

Delete one of a VM's snapshots. Asks first on a terminal

```text
Usage: fluxctl fleet snapshot-rm [OPTIONS] <VM> <SNAPSHOT>

Arguments:
  <VM>        
  <SNAPSHOT>  

Options:
  -y, --yes
```

### `fluxctl fleet clone`

A new VM on the same Mac, from one of a VM's snapshots or (without `--snapshot`) from the stopped VM
itself. It counts against your quota. Example: `fluxctl fleet clone ios-build-1 ios-build-2
--snapshot before-upgrade`

```text
Usage: fluxctl fleet clone [OPTIONS] <VM> <NEW_NAME>

Arguments:
  <VM>        
  <NEW_NAME>  

Options:
      --snapshot <SNAPSHOT>
```

## Volumes

### `fluxctl fleet volumes`

Persistent volumes: disks that keep their data when the VM using them is deleted. A tenant sees its
own

```text
Usage: fluxctl fleet volumes [OPTIONS]

Options:
      --tenant <TENANT>  Operator only: one tenant's volumes
```

### `fluxctl fleet volume-create`

Create a persistent volume on one Mac. Example: `fluxctl fleet volume-create pg-data --size 50`,
then `fluxctl fleet launch --image debian-13 --volume pg-data`

```text
Usage: fluxctl fleet volume-create [OPTIONS] --size <SIZE> <NAME>

Arguments:
  <NAME>  

Options:
      --size <SIZE>      Size in GiB (1-2048). The file is sparse: space is used on the Mac as the
                         guest writes
      --node <NODE>      The Mac to put it on (default: one you lease, else the one with the most
                         free memory)
      --tenant <TENANT>  Operator only: the tenant that owns it
```

### `fluxctl fleet volume-rm`

Delete a volume and its data (refused while a VM uses it). Asks first on a terminal

```text
Usage: fluxctl fleet volume-rm [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
  -y, --yes
```

### `fluxctl fleet volume-resize`

Grow a detached volume to `--size` GiB. Grow the file system in the guest afterwards

```text
Usage: fluxctl fleet volume-resize [OPTIONS] --size <SIZE> <NAME>

Arguments:
  <NAME>  

Options:
      --size <SIZE>
```

## Warm pools

### `fluxctl fleet pools`

Warm pools: VMs booted ahead of time, so a launch of their image starts in seconds. Shows ready and
booting VMs and the last day's claims. REST: `GET /fleet/pools`

```text
Usage: fluxctl fleet pools [OPTIONS]

Options:
```

### `fluxctl fleet pool-create`

Create a warm pool. Example: `fluxctl fleet pool-create ios-ci --image macos-27-xcode --min-ready
2`. A tenant's pool runs on the Macs it leases; the operator's is shared (Macs nobody leases) unless
`--tenant` is given

```text
Usage: fluxctl fleet pool-create [OPTIONS] --image <IMAGE> <NAME>

Arguments:
  <NAME>  

Options:
      --image <IMAGE>                  Catalog image (`fluxctl fleet images`)
      --min-ready <MIN_READY>          VMs kept booted and idle [default: 1]
      --max <MAX>                      Most VMs from the pool alive at once, warm plus claimed
                                       (default: 4 x min-ready, at least min-ready + 4)
      --vcpus <VCPUS>                  vCPUs per VM (default: the image's)
      --memory <MEMORY>                Memory per VM: `16` or `16G` (GiB), `4096M` (default: the
                                       image's)
      --tenant <TENANT>                Operator only: the tenant that owns the pool
      --node-selector <NODE_SELECTOR>  Only nodes with this label, as `key=value`. Repeatable
      --node <NODES>                   Only this node. Repeatable
```

### `fluxctl fleet pool-set`

Change how many VMs a pool keeps ready (`0` pauses it) or its maximum. REST: `PATCH
/fleet/pools/{name}`

```text
Usage: fluxctl fleet pool-set [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --min-ready <MIN_READY>  
      --max <MAX>
```

### `fluxctl fleet pool-rm`

Delete a pool and its warm VMs; VMs already claimed keep running. Asks first on a terminal

```text
Usage: fluxctl fleet pool-rm [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
  -y, --yes
```

## SSH and port tunnels

### `fluxctl fleet ssh`

SSH into a VM through central, without reaching its Mac: runs your `ssh` with a ProxyCommand tunnel,
so keys, agents and ssh_config work as usual. The user is `-l`, `user@vm`, the image's `ssh_user`,
or `admin`. Example: `fluxctl fleet ssh web-1 -- -L 5432:localhost:5432`

```text
Usage: fluxctl fleet ssh [OPTIONS] <VM> [-- <ARGS>...]

Arguments:
  <VM>       `[user@]vm`: a VM name, id or id prefix (or `node/id`)
  [ARGS]...  Passed to ssh after the host: options, or a command to run

Options:
  -l, --user <USER>  Login name on the VM
```

### `fluxctl fleet port-forward`

Forward a local port to a port of a VM through central; every connection gets its own tunnel.
Example: `fluxctl fleet port-forward web-1 8080:80`

```text
Usage: fluxctl fleet port-forward [OPTIONS] <VM> <PORTS>

Arguments:
  <VM>     
  <PORTS>  `local:remote`, or `remote` for the same port on both ends

Options:
      --address <ADDRESS>  Local address to listen on [default: 127.0.0.1]
```

### `fluxctl fleet tunnels`

Open tunnels (a tenant sees its own) and closing them. REST: `GET /fleet/tunnels`, `DELETE
/fleet/tunnels/{id}`

```text
Usage: fluxctl fleet tunnels [OPTIONS] [COMMAND]

Commands:
  list   The open tunnels (a tenant sees its own): VM, port, traffic and idle time
  close  Close a tunnel now. REST: `DELETE /fleet/tunnels/{id}`
  help   Print this message or the help of the given subcommand(s)

Options:
```

#### `fluxctl fleet tunnels list`

The open tunnels (a tenant sees its own): VM, port, traffic and idle time

```text
Usage: fluxctl fleet tunnels list [OPTIONS]

Options:
```

#### `fluxctl fleet tunnels close`

Close a tunnel now. REST: `DELETE /fleet/tunnels/{id}`

```text
Usage: fluxctl fleet tunnels close [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
```

### `fluxctl fleet tunnel-stdio`

Relay stdin/stdout through a tunnel to a port of a VM, for `ssh -o ProxyCommand="fluxctl fleet
tunnel-stdio <vm> %p"`. REST: `GET /fleet/vms/{node}/{id}/tunnel?port=N` (WebSocket)

```text
Usage: fluxctl fleet tunnel-stdio [OPTIONS] <VM> <PORT>

Arguments:
  <VM>    VM name, id or id prefix, or `node/id`
  <PORT>  

Options:
```

## Tenants, leases and drain (operator)

### `fluxctl fleet tenant`

Tenants (operator): create, list, quota, disable, rotate token. See docs/mac-cloud-tenancy.md

```text
Usage: fluxctl fleet tenant [OPTIONS] <COMMAND>

Commands:
  create        Create a tenant; prints its bearer token once
  list          
  get           
  me            The calling tenant's own record (with a tenant token)
  update        Change quota (only the flags given) and/or enable/disable
  delete        
  rotate-token  
  help          Print this message or the help of the given subcommand(s)

Options:
```

#### `fluxctl fleet tenant create`

Create a tenant; prints its bearer token once

```text
Usage: fluxctl fleet tenant create [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --display-name <DISPLAY_NAME>          
      --max-vms <MAX_VMS>                    
      --max-vcpus <MAX_VCPUS>                
      --max-memory-mib <MAX_MEMORY_MIB>      
      --max-leased-hosts <MAX_LEASED_HOSTS>  
      --max-volume-gib <MAX_VOLUME_GIB>      Total GiB of persistent volumes
```

#### `fluxctl fleet tenant list`

Usage: fluxctl fleet tenant list [OPTIONS]

Options:

```text

```

#### `fluxctl fleet tenant get`

Usage: fluxctl fleet tenant get [OPTIONS] <ID>

Arguments:
  <ID>  

Options:

```text

```

#### `fluxctl fleet tenant me`

The calling tenant's own record (with a tenant token)

```text
Usage: fluxctl fleet tenant me [OPTIONS]

Options:
```

#### `fluxctl fleet tenant update`

Change quota (only the flags given) and/or enable/disable

```text
Usage: fluxctl fleet tenant update [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --max-vms <MAX_VMS>                    
      --max-vcpus <MAX_VCPUS>                
      --max-memory-mib <MAX_MEMORY_MIB>      
      --max-leased-hosts <MAX_LEASED_HOSTS>  
      --max-volume-gib <MAX_VOLUME_GIB>      Total GiB of persistent volumes
      --disable                              
      --enable
```

#### `fluxctl fleet tenant delete`

Usage: fluxctl fleet tenant delete [OPTIONS] <ID>

Arguments:
  <ID>  

Options:

```text

```

#### `fluxctl fleet tenant rotate-token`

Usage: fluxctl fleet tenant rotate-token [OPTIONS] <ID>

Arguments:
  <ID>  

Options:

```text

```

### `fluxctl fleet lease`

Mac host leases: a tenant rents a whole, empty Mac for at least 24 hours

```text
Usage: fluxctl fleet lease [OPTIONS] <COMMAND>

Commands:
  create   Lease a Mac (the smallest free one that fits, or `--node`)
  list     
  get      
  release  End a lease (not before its 24-hour minimum, unless an operator passes `--force`)
  help     Print this message or the help of the given subcommand(s)

Options:
```

#### `fluxctl fleet lease create`

Lease a Mac (the smallest free one that fits, or `--node`)

```text
Usage: fluxctl fleet lease create [OPTIONS]

Options:
      --tenant <TENANT>
          Operator only: the tenant to lease for
      --node <NODE>
          
      --hours <HOURS>
          Lease length in hours (at least 24); omitted runs until released
      --min-vcpus <MIN_VCPUS>
          
      --min-memory-mib <MIN_MEMORY_MIB>
          
      --nested
          Only Macs with nested virtualization (M3 or later)
      --node-selector <NODE_SELECTOR>
          
          Print help
          Print version
```

#### `fluxctl fleet lease list`

Usage: fluxctl fleet lease list [OPTIONS]

Options:
      --tenant <TENANT>  
      --state <STATE>    `active` or `released`

```text

```

#### `fluxctl fleet lease get`

Usage: fluxctl fleet lease get [OPTIONS] <ID>

Arguments:
  <ID>  

Options:

```text

```

#### `fluxctl fleet lease release`

End a lease (not before its 24-hour minimum, unless an operator passes `--force`)

```text
Usage: fluxctl fleet lease release [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --force
```

### `fluxctl fleet drain`

Cordon a node and handle its VMs: `cordon` (default) only reports, `evict-ephemeral` deletes VMs
with a TTL or `fluxvm.dev/ephemeral=true`, `reschedule` also recreates `"reschedulable": true` VMs
on another node. REST: `POST /fleet/nodes/{name}/drain`

```text
Usage: fluxctl fleet drain [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --mode <MODE>  [default: cordon]
      --force        Drain a node that is leased to a tenant
```

### `fluxctl fleet drain-status`

Drain progress and lease/scrub state of a node. REST: `GET /fleet/nodes/{name}/drain`

```text
Usage: fluxctl fleet drain-status [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

### `fluxctl fleet scrub`

Clear a node released from a lease for reuse (refused while it runs VMs unless `--force` deletes
them). REST: `POST /fleet/nodes/{name}/scrub`

```text
Usage: fluxctl fleet scrub [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --force
```

### `fluxctl fleet usage`

Metering per tenant (VM, vCPU and memory hours; actual and billable lease hours). REST: `GET
/fleet/usage`

```text
Usage: fluxctl fleet usage [OPTIONS]

Options:
      --from <FROM>      RFC 3339 start (default: 30 days before `--to`)
      --to <TO>          RFC 3339 end (default: now)
      --tenant <TENANT>  
      --csv              CSV instead of JSON
```

### `fluxctl fleet reconcile`

Reconcile the VM ledger against every node and end expired leases now. REST: `POST /fleet/reconcile`

```text
Usage: fluxctl fleet reconcile [OPTIONS]

Options:
```

## Team members (SSO)

### `fluxctl fleet members`

The people who sign in to the console with SSO, and their roles. A tenant sees its own team; the
operator passes `--tenant`

```text
Usage: fluxctl fleet members [OPTIONS]

Options:
      --tenant <TENANT>  Operator only: whose members
```

### `fluxctl fleet member-add`

Invite a person by email: they can then sign in to the console with SSO. Roles: viewer (reads),
member (VMs and networks), admin (also leases, keys, members, budget, webhooks), owner (everything)

```text
Usage: fluxctl fleet member-add [OPTIONS] <EMAIL>

Arguments:
  <EMAIL>  

Options:
      --role <ROLE>      [default: member] [possible values: owner, admin, member, viewer]
      --tenant <TENANT>  Operator only: the tenant
```

### `fluxctl fleet member-rm`

Remove a member; their console sessions end at once

```text
Usage: fluxctl fleet member-rm [OPTIONS] <EMAIL>

Arguments:
  <EMAIL>  

Options:
      --tenant <TENANT>  Operator only: the tenant
      --yes              Do not ask for confirmation
```

### `fluxctl fleet member-role`

Change a member's role (owner, admin, member or viewer)

```text
Usage: fluxctl fleet member-role [OPTIONS] <EMAIL> <ROLE>

Arguments:
  <EMAIL>  
  <ROLE>   [possible values: owner, admin, member, viewer]

Options:
      --tenant <TENANT>  Operator only: the tenant
```

## Metrics

### `fluxctl fleet top`

The busiest VMs right now: CPU (share of each VM's vCPUs), memory, disk and network from central's
last sample. Example: `fluxctl fleet top --sort mem -n 10`

```text
Usage: fluxctl fleet top [OPTIONS]

Options:
      --sort <SORT>    Order by `cpu` (default) or `mem` [default: cpu] [possible values: cpu, mem]
  -n, --limit <LIMIT>  Show only the first N VMs
```

### `fluxctl fleet metrics`

A VM's CPU, memory, disk and network over the last hour (or `--range 6h|24h`), found by name or ID
prefix. Example: `fluxctl fleet metrics ios-build-1 --range 6h`

```text
Usage: fluxctl fleet metrics [OPTIONS] <VM>

Arguments:
  <VM>  

Options:
      --range <RANGE>  [default: 1h] [possible values: 1h, 6h, 24h]
```

## Prices, bills and budgets

### `fluxctl fleet prices`

The price list. `--set FILE` (operator) replaces it with a JSON file shaped like `fleet prices -o
json`

```text
Usage: fluxctl fleet prices [OPTIONS]

Options:
      --set <SET>
```

### `fluxctl fleet bill`

A month's statement: what leases and VMs cost. A tenant sees its own; the operator every tenant's

```text
Usage: fluxctl fleet bill [OPTIONS]

Options:
      --month <MONTH>    `2026-10` (default: this month)
      --tenant <TENANT>  Operator only: one tenant
      --csv
```

### `fluxctl fleet budget`

Show or set a tenant's monthly budget (a warning at 80% and when over; nothing is stopped)

```text
Usage: fluxctl fleet budget [OPTIONS] [AMOUNT]

Arguments:
  [AMOUNT]  The monthly amount; `0` removes it. Omit to show the current budget and spend

Options:
      --tenant <TENANT>  Operator only: whose budget
```

## Self-service sign-up, invoices and payment

### `fluxctl fleet signup`

Create an account on a central with self-service sign-up on; needs no token and prints the account's
token once. Example: `fluxctl fleet --central https://cloud.example.com signup acme --email
you@acme.io`

```text
Usage: fluxctl fleet signup [OPTIONS] --email <EMAIL> <TENANT_ID>

Arguments:
  <TENANT_ID>  Account ID: lowercase letters, digits and '-', used in URLs and network names

Options:
      --email <EMAIL>  
      --name <NAME>    Name shown in the console (default: the ID)
```

### `fluxctl fleet signup-verify`

Confirm a sign-up from the token in the mailed link (the link's page does the same) and print the account's token,
once. Example: `fluxctl fleet --central https://cloud.example.com signup-verify fvv_...`.

```text
Usage: fluxctl fleet signup-verify <TOKEN>

Arguments:
  <TOKEN>  The token from the confirmation link (or the whole link)
```

### `fluxctl fleet invoices`

Invoices with their status and a link to pay. A tenant sees its own; the operator everyone's

```text
Usage: fluxctl fleet invoices [OPTIONS]

Options:
      --month <MONTH>    `2026-09` (default: every month)
      --tenant <TENANT>  Operator only: one tenant
```

### `fluxctl fleet invoice-run`

Operator: turn a finished month's statements into Stripe invoices, one per tenant with billing on.
Safe to run again: a tenant already invoiced for the month is skipped

```text
Usage: fluxctl fleet invoice-run [OPTIONS] --month <MONTH>

Options:
      --month <MONTH>    `2026-09`
      --tenant <TENANT>  Only this tenant
  -y, --yes              Do not ask
```

### `fluxctl fleet payment-method`

A Stripe Checkout link to save a payment method for your invoices (tenant)

```text
Usage: fluxctl fleet payment-method [OPTIONS]

Options:
```

## Notifications, keys and the activity log

### `fluxctl fleet notifications`

Recent notifications: leases ending, budgets, VM changes (and, for the operator, Macs down, drains,
scrubs)

```text
Usage: fluxctl fleet notifications [OPTIONS]

Options:
      --limit <LIMIT>  [default: 20]
```

### `fluxctl fleet webhooks`

Webhooks that receive notifications as signed JSON: list (default), add, rm, test

```text
Usage: fluxctl fleet webhooks [OPTIONS] [COMMAND]

Commands:
  list  
  add   Add a webhook; prints its signing secret once
  rm    
  test  Send a test event and show what the endpoint answered
  help  Print this message or the help of the given subcommand(s)

Options:
```

#### `fluxctl fleet webhooks list`

Usage: fluxctl fleet webhooks list [OPTIONS]

Options:

```text

```

#### `fluxctl fleet webhooks add`

Add a webhook; prints its signing secret once

```text
Usage: fluxctl fleet webhooks add [OPTIONS] <URL>

Arguments:
  <URL>  

Options:
      --event <EVENTS>  Only these events (repeatable), e.g. `--event lease.ending --event
                        budget.over`. Default: all
```

#### `fluxctl fleet webhooks rm`

Usage: fluxctl fleet webhooks rm [OPTIONS] <ID>

Arguments:
  <ID>  

Options:

```text

```

#### `fluxctl fleet webhooks test`

Send a test event and show what the endpoint answered

```text
Usage: fluxctl fleet webhooks test [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
```

### `fluxctl fleet keys`

A tenant's named API keys: list (default), create, revoke. Keys are made and revoked with the
tenant's primary token (or by the operator with `--tenant`)

```text
Usage: fluxctl fleet keys [OPTIONS] [COMMAND]

Commands:
  list    
  create  Create a key; prints its token once
  revoke  Revoke a key by name or id; anything using it stops working at once
  help    Print this message or the help of the given subcommand(s)

Options:
      --tenant <TENANT>  Operator only: whose keys
```

#### `fluxctl fleet keys list`

Usage: fluxctl fleet keys list [OPTIONS]

Options:
      --tenant <TENANT>  Operator only: whose keys

```text

```

#### `fluxctl fleet keys create`

Create a key; prints its token once

```text
Usage: fluxctl fleet keys create [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --expires-days <EXPIRES_DAYS>  Days until it stops working (default 90; 0 never expires)
                                     [default: 90]
      --tenant <TENANT>              Operator only: whose keys
      --read-only                    It can only read
```

#### `fluxctl fleet keys revoke`

Revoke a key by name or id; anything using it stops working at once

```text
Usage: fluxctl fleet keys revoke [OPTIONS] <KEY>

Arguments:
  <KEY>  

Options:
      --tenant <TENANT>  Operator only: whose keys
```

### `fluxctl fleet events`

The activity log: every change made through central, newest first. A tenant sees its own

```text
Usage: fluxctl fleet events [OPTIONS]

Options:
      --limit <LIMIT>    [default: 50]
      --tenant <TENANT>  Operator only: one tenant's actions
```

## Images and private networks

### `fluxctl fleet images`

The image catalog: list (default), add or remove (operator)

```text
Usage: fluxctl fleet images [OPTIONS] [COMMAND]

Commands:
  list    
  add     Add or replace an image (operator). Put the disk on the Macs first (`fluxctl vm-image
          pull`)
  remove  Remove an image from the catalog (VMs made from it keep running)
  help    Print this message or the help of the given subcommand(s)

Options:
```

#### `fluxctl fleet images list`

Usage: fluxctl fleet images list [OPTIONS]

Options:

```text

```

#### `fluxctl fleet images add`

Add or replace an image (operator). Put the disk on the Macs first (`fluxctl vm-image pull`)

```text
Usage: fluxctl fleet images add [OPTIONS] --path <PATH> --guest <GUEST> <NAME>

Arguments:
  <NAME>  

Options:
      --path <PATH>                  The disk's path on every Mac that runs it
      --guest <GUEST>                [possible values: macos, linux]
      --vcpus <VCPUS>                Default vCPUs [default: 4]
      --memory-gib <MEMORY_GIB>      Default memory [default: 8]
      --display-name <DISPLAY_NAME>  
      --description <DESCRIPTION>    
      --source <SOURCE>              Where it came from, e.g. an OCI reference (shown to tenants)
```

#### `fluxctl fleet images remove`

Remove an image from the catalog (VMs made from it keep running)

```text
Usage: fluxctl fleet images remove [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

### `fluxctl fleet networks`

Private networks across Macs: list (default), show, create, delete, rotate, add-mac, remove-mac

```text
Usage: fluxctl fleet networks [OPTIONS] [COMMAND]

Commands:
  list        
  show        
  create      Create a network; members default to every Mac the tenant leases
  delete      
  rotate      Issue a new key; Macs switch over without dropping traffic
  add-mac     
  remove-mac  
  help        Print this message or the help of the given subcommand(s)

Options:
```

#### `fluxctl fleet networks list`

Usage: fluxctl fleet networks list [OPTIONS]

Options:

```text

```

#### `fluxctl fleet networks show`

Usage: fluxctl fleet networks show [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:

```text

```

#### `fluxctl fleet networks create`

Create a network; members default to every Mac the tenant leases

```text
Usage: fluxctl fleet networks create [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --mac <MAC>                  A member Mac. Repeatable
      --tenant <TENANT>            Operator only: the tenant that owns it
      --vms-per-mac <VMS_PER_MAC>  Guest addresses each Mac gets [default: 30]
      --mtu <MTU>                  [default: 1400]
```

#### `fluxctl fleet networks delete`

Usage: fluxctl fleet networks delete [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
  -y, --yes

```text

```

#### `fluxctl fleet networks rotate`

Issue a new key; Macs switch over without dropping traffic

```text
Usage: fluxctl fleet networks rotate [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
```

#### `fluxctl fleet networks add-mac`

Usage: fluxctl fleet networks add-mac [OPTIONS] <NAME> <MAC>

Arguments:
  <NAME>  
  <MAC>   

Options:

```text

```

#### `fluxctl fleet networks remove-mac`

Usage: fluxctl fleet networks remove-mac [OPTIONS] <NAME> <MAC>

Arguments:
  <NAME>  
  <MAC>   

Options:

```text

```
