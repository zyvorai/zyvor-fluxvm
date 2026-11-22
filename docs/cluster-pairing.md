# Pairing: add a machine with a short code

> **Status.** The protocol, the server's rules and the machine's flow are unit-tested with fakes, and the pairing HTTP routes are tested end to end
> in-process against the real enrollment code. The mDNS announcer and browser were exercised **only over loopback**. **Not verified:** discovery
> on a real LAN (a router or switch that filters multicast, a Wi-Fi network with client isolation, another responder holding port 5353, IPv6-only
> networks), a Mac or Linux machine pairing across two real machines, and the Studio page in a browser. This is authentication code and has had no
> external security review. The `--server` path does not depend on mDNS.
>
> **Run for real (Linux, loopback only, `--pairing-no-announce`):** a real `fluxvm-agent central --pairing` and a real `fluxctl cluster pair --server`, approved with
> `fluxctl cluster pending` and `approve CODE`; the node credential was stored 0600, registered as the node with HTTP 200, and `cluster report` listed it with no secret in the output.

## What a person does

On the server (the machine that runs `fluxvm-agent central`), once:

```bash
fluxvm-agent central --pairing          # also announces itself on the LAN; add --pairing-name "Home Lab" if you like
```

On the machine to add (Mac mini or Linux box):

```bash
fluxctl cluster pair
#   Looking for a FluxVM fleet on this network...
#   Found the fleet "Home Lab" at http://192.168.1.20:7799. Ask to join it? [Y/n]
#
#     This machine is asking to join as "mini-2".
#
#         Your code:  7KQ4M-T9XH2
#
#     On the server, run `fluxctl cluster pending` (or open Studio) and approve the request that shows this exact code.
```

On the server:

```bash
fluxctl cluster pending                  # CODE  NAME  FROM  EXPIRES IN
fluxctl cluster approve 7KQ4M-T9XH2      # the same code the machine shows
```

or open Studio, **Home, Machines asking to join**, compare the code and press **Approve** (or **Deny**). The machine receives its credential
within a few seconds and stores it. Nobody typed a URL or a token. Then start the node agent with the line `cluster pair` prints (on a Mac,
`fluxctl cluster pair --install` does it).

`--pairing-no-announce` accepts requests without announcing on the LAN (machines then need `--server`).

If no fleet is found, the command says why and what to do: `fluxctl cluster pair --server ADDRESS` (a host, host:port or URL) skips discovery.
`--fleet NAME` picks one when several answer. `--yes` skips the question before the fleet is contacted.

## How it works

```
machine                                       central (--pairing)
  | POST /fleet/pair/request  name, ed25519 pub, x25519 pub ->|   nothing is granted
  |<- id, central's one-time x25519 pub --------------------- |
  |   code = H(machine keys, id, central key)                  |   the same code is computed for the operator's list
  |                                     operator: approve CODE |   -> EnrollState::issue_paired (the existing enrollment)
  | POST /fleet/pair/claim  id, ed25519 signature ----------->|   pending ... pending ... approved
  |<- credential, sealed to the machine (X25519 + HKDF + AES-256-GCM), delivered once
```

- **The credential is the existing one.** Approval mints a single-use join token limited to that one name and spends it on `enroll` at once, then
  revokes it, so the result is the ordinary `fvnc_` node credential of [fleet-enroll.md](fleet-enroll.md), the same `--node-auth` rules, the same
  renewal and revocation. Pairing adds no credential type and leaves no token behind. The node id is derived from the machine's public key.
- **Files.** The machine keeps `pair-key` (its signing seed), `node-id` and `node-credential` in its state directory, all mode 0600 in a 0700
  directory, where `fluxvm-agent node` already looks. No key or credential is ever on a command line.
- **The code** is ten characters of a 32-symbol alphabet without look-alikes (50 bits), shown as `ABCDE-FGHJK`. It is a fingerprint of the request,
  not a password: both screens derive it from the machine's keys, the request id and central's one-time key, so they only agree if nobody swapped a key.
  Typing it (any case, dashes optional) is also how the operator names which request to approve.
- **Limits.** A request lives 10 minutes and is memory-only (a restart of central drops it). At most 16 are open, 3 per address. A request is
  collected once. Claims are limited to one per second per request, and 20 failed claims per minute per address are blocked for the rest of the minute.
  Enrollment's own admission control still applies on top.
- **Off by default.** Without `--pairing` the two public routes answer 404 and nothing is announced. With it, they are the only unauthenticated
  pairing routes; listing, approving and denying need the operator token like every other `/fleet/*` call, and a node credential or tenant token cannot use them.

## Threat model

| Threat | What happens |
|---|---|
| **Someone on the LAN sees the code** (shoulder surfing, a photo) | The code unlocks nothing. Approval needs the operator; collecting the credential needs the machine's private key (every claim is signed), and the credential is sealed to the machine's one-time key. A watcher can at most ask the operator to approve a request, which is why the operator compares the code with the machine's own screen. |
| **Guessing codes or request ids** | Ids are 128 random bits. The code is not a credential. Failed claims are throttled per address, claims per request are throttled, and open requests are capped, so spamming requests cannot fill the operator's screen indefinitely. |
| **A request that is not mine** (an attacker on the LAN asks to join) | It appears in the list with its name and address and a code the real machine will never show. Deny it, or ignore it: it expires. An operator who approves a code no machine is displaying has approved the attacker. Compare every time. |
| **Man in the middle** (relays a real machine's request with its own keys, or the reverse) | The code covers both public keys, the id and central's key, so a substitution on either side makes the machine's screen and the operator's list disagree. 50 bits make grinding a collision within the ten minutes impractical; an attacker able to do 2^50 hashes in that time is outside this model. |
| **Rogue announcer** (another host announces `_fluxvm-pair._tcp`) | A machine never contacts a fleet without asking first (`Found the fleet "X" at ADDRESS. Ask to join it?`; `--fleet NAME` or `--server` pin it). A rogue fleet receives the machine's name and public keys and can show a code, but cannot give the machine anything useful: a credential from a fleet is worth only what that fleet is, and the person never approves it on their own server. Check the address printed before saying yes. mDNS is not authenticated; that is why the question exists. |
| **Replay** | A claim after delivery gets 409 and nothing. A request that was approved but never collected is delivered once, to the signature holder. Approving twice is refused. Expired requests are gone. |
| **Sniffing the credential** | It is sealed to the machine's one-time X25519 key (AES-256-GCM, bound to the request id), so a passive sniffer sees ciphertext. The *request* travels in the clear unless central has TLS (`--tls-cert`); use TLS beyond a trusted LAN. The enrolled node's later heartbeats carry the credential as a bearer header exactly as before ([fleet-enroll.md](fleet-enroll.md)). |
| **Active attacker who can also modify the request** | Cannot obtain the credential without the key, but can make the machine pair with the attacker's own central if it is the one answering. Same as the rogue announcer: the address is shown first. |
| **Nothing on the network until approved** | Before the person says yes, the machine only listens for announcements (a multicast query). After yes, it sends its name and public keys; it gets no credential until approval. Nothing is installed, no service started, no port opened on the machine by pairing. |
| **A shared or cloned machine key** | The signing seed is per machine (`pair-key`, 0600). Cloning an image with it clones the identity: delete `pair-key`, `node-id` and `node-credential` before imaging, the same rule as the node id in [fleet-install.md](fleet-install.md). |

What pairing does **not** protect against: an operator who approves without comparing, a compromised server, a compromised machine, an attacker who
controls the whole LAN and the server's address at the same time as the operator (use TLS and `--server` with a pinned address).

## What is on the wire for discovery

Central answers multicast DNS queries for `_fluxvm-pair._tcp.local` with a PTR, an SRV (host and port), a TXT (`v=1`, `scheme=http|https`) and an
A record. The fleet name is the instance name. Nothing else is announced, and no token, key or machine list is in it. The machine sends a
"legacy unicast" query (from an ephemeral port), so it binds no well-known port. Central binds UDP 5353 with address reuse (so it can coexist with
the system's responder where the OS allows it); if that fails, central logs a warning and machines use `--server`.

## Names

`fluxctl cluster pair | pending | approve | deny` follow the `cluster` verb the installer (`install.sh --server/--join`, `fluxctl cluster init|join|nodes|token|remove`)
and the health work (`cluster status`) use; they share one command group, and `cluster report` is the passport ([cluster-passport.md](cluster-passport.md)).
When those land, `cluster pair` is the no-URL, no-token way of what `cluster join` does with a token.

## Leftovers

- The Studio page needs the UI bundle rebuilt (`cd ui && npm ci && npm run build`); see the status note in [ui.md](ui.md).
- Requests are not persisted and not shared between HA centrals: approve on the leader.
- The credential is delivered once; if the response is lost the machine must pair again (the old request stays consumed).
- No IPv6 announcement, and no per-interface selection of the address announced.
