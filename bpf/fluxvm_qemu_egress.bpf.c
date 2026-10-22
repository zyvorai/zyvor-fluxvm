// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: GPL-2.0-only
//
// Sentinel Set 7S: per-VM outbound-IP policy for the QEMU/VMM process.
//
// Attached (BPF_CGROUP_INET_EGRESS) to the same fluxvm.slice/{id}.scope
// cgroup as fluxvm_qemu_device.bpf.c. cgroup_skb only ever sees IP-family
// socket traffic -- QEMU's virtiofsd/vhost-user/QMP control channels are
// AF_UNIX and its guest VSOCK channel is AF_VSOCK, neither of which is
// network-reachable in the first place and neither of which this hook type
// can see or needs to restrict.
//
// What it restricts: IP sockets the QEMU process itself opens. For a guest on
// QEMU user-mode (slirp) networking that is *all* guest traffic, because slirp
// re-originates every guest connection from the QEMU process. For a tap/bridged
// guest the traffic crosses the TAP device instead (fluxvm_tc.bpf.c governs
// that path) and this hook only sees QEMU's own sockets.
//
// Modes (fluxvm_qemu_cfg[0].mode), set by the daemon per VM:
//   0 LOOPBACK  only 127.0.0.0/8 / ::1 leaves the cgroup. This is the default
//               and the pre-policy behaviour: a compromised QEMU cannot dial out.
//   1 ALLOWLIST loopback plus the (cidr, proto, port) rules in fluxvm_qemu_allow4.
//   2 AUDIT     everything is allowed, but traffic an ALLOWLIST would have
//               dropped is recorded in fluxvm_qemu_seen. Used to learn what a
//               guest needs before enforcing.
//   3 OPEN      everything is allowed.
// Dropped (ALLOWLIST/LOOPBACK) and would-be-dropped (AUDIT) IPv4 flows are
// counted in fluxvm_qemu_seen so the control plane can explain a denial and
// suggest the rule that would fix it.
//
// IPv6: loopback is always allowed; in ALLOWLIST/LOOPBACK mode other IPv6 is
// dropped (no v6 rules yet), in AUDIT/OPEN mode it is allowed.

#include <linux/bpf.h>
#include <bpf/bpf_endian.h>
#include <bpf/bpf_helpers.h>

#define LINUX_AF_INET  2
#define LINUX_AF_INET6 10
// Fixed byte offsets within a bare (no Ethernet framing -- cgroup_skb starts
// at L3) IPv4/IPv6 header. Not pulled from <linux/ip.h>/<linux/ipv6.h> to avoid
// a struct-layout dependency for numbers fixed by the RFCs themselves.
// IPv4: 1(ver/ihl)+1(tos)+2(len)+2(id)+2(frag)+1(ttl)+1(proto)+2(csum)+4(saddr) = 16.
// IPv6: 4(ver/class/flow)+2(len)+1(nexthdr)+1(hoplimit)+16(saddr) = 24.
#define IPV4_PROTO_OFFSET 9
#define IPV4_FRAG_OFFSET  6
#define IPV4_DADDR_OFFSET 16
#define IPV6_DADDR_OFFSET 24

#define PROTO_TCP 6
#define PROTO_UDP 17

#define MODE_LOOPBACK  0
#define MODE_ALLOWLIST 1
#define MODE_AUDIT     2
#define MODE_OPEN      3

// Must match the QEMU_EGRESS_* constants in crates/fluxvm-network/src/qemu_egress.rs.
#define MAX_RULES       256
#define MAX_RULE_PORTS  4
#define MAX_SEEN        512

struct qemu_cfg {
    __u32 mode;
    __u32 reserved;
};

// LPM key: prefixlen in bits over the IPv4 destination (network byte order).
struct lpm_key4 {
    __u32 prefixlen;
    __u32 addr;
};

// A rule's port list. nports == 0 means any port and any protocol. Otherwise
// the packet must match one (proto, port) pair; proto 0 matches tcp and udp.
// Userspace folds the ports of covering prefixes into each entry so that
// longest-prefix match alone gives the right answer.
struct rule_val {
    __u8 nports;
    __u8 proto[MAX_RULE_PORTS];
    __u8 pad[3];
    __u16 port[MAX_RULE_PORTS]; // host byte order
};

struct seen_key {
    __u32 daddr;  // network byte order
    __u16 port;   // host byte order, 0 for non-tcp/udp or fragments
    __u8 proto;
    __u8 audit;   // 1 = would be dropped, traffic was allowed (AUDIT mode)
};

struct seen_val {
    __u64 packets;
    __u64 bytes;
    __u64 last_ns;
};

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct qemu_cfg);
} fluxvm_qemu_cfg SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LPM_TRIE);
    __uint(max_entries, MAX_RULES);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, struct lpm_key4);
    __type(value, struct rule_val);
} fluxvm_qemu_allow4 SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, MAX_SEEN);
    __type(key, struct seen_key);
    __type(value, struct seen_val);
} fluxvm_qemu_seen SEC(".maps");

static __always_inline void note(__u32 daddr, __u16 port, __u8 proto, __u8 audit,
                                 __u32 len)
{
    struct seen_key k = {.daddr = daddr, .port = port, .proto = proto, .audit = audit};
    struct seen_val *v = bpf_map_lookup_elem(&fluxvm_qemu_seen, &k);
    if (v) {
        __sync_fetch_and_add(&v->packets, 1);
        __sync_fetch_and_add(&v->bytes, len);
        v->last_ns = bpf_ktime_get_ns();
        return;
    }
    struct seen_val nv = {.packets = 1, .bytes = len, .last_ns = bpf_ktime_get_ns()};
    bpf_map_update_elem(&fluxvm_qemu_seen, &k, &nv, BPF_ANY);
}

static __always_inline int rule_allows(const struct rule_val *r, __u8 proto, __u16 port)
{
    if (r->nports == 0)
        return 1;
    #pragma unroll
    for (int i = 0; i < MAX_RULE_PORTS; i++) {
        if (i >= r->nports)
            break;
        if (r->port[i] == port && (r->proto[i] == 0 || r->proto[i] == proto))
            return 1;
    }
    return 0;
}

SEC("cgroup_skb/egress")
int fluxvm_qemu_egress(struct __sk_buff *skb)
{
    __u32 zero = 0;
    struct qemu_cfg *cfg = bpf_map_lookup_elem(&fluxvm_qemu_cfg, &zero);
    __u32 mode = cfg ? cfg->mode : MODE_LOOPBACK;

    if (skb->family == LINUX_AF_INET) {
        __u32 daddr;
        if (bpf_skb_load_bytes(skb, IPV4_DADDR_OFFSET, &daddr, sizeof(daddr)) < 0)
            return 1; // truncated/malformed read: fail open rather than
                      // wedge every IPv4 packet on a parse edge case.
        if ((daddr & bpf_htonl(0xff000000)) == bpf_htonl(0x7f000000))
            return 1; // loopback is always allowed
        if (mode == MODE_OPEN)
            return 1;

        __u8 proto = 0;
        __u16 port = 0;
        __u8 ver_ihl = 0;
        if (bpf_skb_load_bytes(skb, 0, &ver_ihl, 1) == 0 &&
            bpf_skb_load_bytes(skb, IPV4_PROTO_OFFSET, &proto, 1) == 0) {
            __u16 frag = 0;
            bpf_skb_load_bytes(skb, IPV4_FRAG_OFFSET, &frag, 2);
            __u32 ihl = (ver_ihl & 0x0f) * 4;
            // Ports exist only in the first fragment of tcp/udp.
            if ((proto == PROTO_TCP || proto == PROTO_UDP) &&
                (frag & bpf_htons(0x1fff)) == 0 && ihl >= 20 && ihl <= 60) {
                __u16 dport = 0;
                if (bpf_skb_load_bytes(skb, ihl + 2, &dport, 2) == 0)
                    port = bpf_ntohs(dport);
            }
        }

        int allow = 0;
        if (mode == MODE_ALLOWLIST || mode == MODE_AUDIT) {
            struct lpm_key4 key = {.prefixlen = 32, .addr = daddr};
            struct rule_val *r = bpf_map_lookup_elem(&fluxvm_qemu_allow4, &key);
            if (r && rule_allows(r, proto, port))
                allow = 1;
        }
        if (allow)
            return 1;
        if (mode == MODE_AUDIT) {
            note(daddr, port, proto, 1, skb->len);
            return 1;
        }
        note(daddr, port, proto, 0, skb->len);
        return 0;
    }
    if (skb->family == LINUX_AF_INET6) {
        __u8 daddr[16];
        if (bpf_skb_load_bytes(skb, IPV6_DADDR_OFFSET, &daddr, sizeof(daddr)) < 0)
            return 1;
        __u8 loopback[16] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1};
        int is_loopback = 1;
        #pragma unroll
        for (int i = 0; i < 16; i++) {
            if (daddr[i] != loopback[i])
                is_loopback = 0;
        }
        if (is_loopback || mode == MODE_OPEN || mode == MODE_AUDIT)
            return 1;
        return 0;
    }
    // Non-IP families (AF_UNIX, AF_VSOCK, AF_NETLINK, ...) never reach this
    // hook in the first place; this branch exists only for forward
    // compatibility with an address family this program does not know
    // about yet, and stays fail-open rather than breaking unrelated traffic.
    return 1;
}

char LICENSE[] SEC("license") = "GPL";
