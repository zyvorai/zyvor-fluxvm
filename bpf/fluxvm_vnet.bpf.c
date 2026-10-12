// Copyright 2026 Zyvor
// SPDX-License-Identifier: GPL-2.0-only
//
// FluxVM virtual network datapath (docs/virtual-network.md).
//
// Each virtual network (VNI) is an isolated IPv4 network. Guests attach with a
// bridge-less tap; the host routes for them at L3 with a distributed gateway:
// every host answers ARP for every address on the network with the same
// gateway MAC, so guests never see each other's MACs on the wire and a guest
// keeps its gateway wherever it runs.
//
//   vnet_guest   TC ingress of a guest tap (frames from the guest)
//                ARP -> answered here; IPv4 -> source checked, then
//                  to the gateway: ICMP echo answered here, anything else dropped
//                  on the network: local tap, or Geneve to the owning host
//                  off the network: floating IP (1:1) or SNAT, then the uplink
//   vnet_tunnel  TC ingress of the host's collect_md Geneve device
//                delivers to a local guest of the packet's VNI
//   vnet_uplink  TC ingress of an uplink
//                floating IP and SNAT replies back to the guest; anything
//                else continues to the host stack (TC_ACT_UNSPEC)
//
// Maps are pinned once per host and written by fluxvm-network's vnet module;
// keep the structs in lockstep with crates/fluxvm-network/src/vnet.rs.

#include <stddef.h>
#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/in.h>
#include <linux/ip.h>
#include <linux/tcp.h>
#include <linux/udp.h>
#include <linux/pkt_cls.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>

#define VNET_FIB_LOCAL  1
#define VNET_FIB_REMOTE 2

#define VNET_NAT_PORT_MIN   61000
#define VNET_NAT_PORT_SPAN  4535 /* 61000..65534, outside Linux's default ephemeral range */
#define VNET_NAT_PROBES     16

/* key: guest tap ifindex */
struct vnet_port {
    __u32 vni;
    __be32 ip;
    __u8 mac[6];
    __u8 pad[2];
};
_Static_assert(sizeof(struct vnet_port) == 16, "vnet_port ABI");

/* key: vni */
struct vnet_net {
    __be32 net;
    __be32 mask;
    __be32 gateway;
    __be32 snat_ip;       /* 0: no SNAT */
    __u32 uplink_ifindex; /* 0: nothing leaves the network */
    __u32 pad;
};
_Static_assert(sizeof(struct vnet_net) == 24, "vnet_net ABI");

struct vnet_fib_key {
    __u32 vni;
    __be32 ip;
};
_Static_assert(sizeof(struct vnet_fib_key) == 8, "vnet_fib_key ABI");

struct vnet_fib {
    __u32 kind;
    __u32 ifindex;  /* VNET_FIB_LOCAL: the guest's tap */
    __be32 remote;  /* VNET_FIB_REMOTE: underlay IPv4 of the owning host */
    __u8 mac[6];
    __u8 pad[2];
};
_Static_assert(sizeof(struct vnet_fib) == 20, "vnet_fib ABI");

/* key: 0 */
struct vnet_host {
    __u32 geneve_ifindex;
    __u32 pad;
};
_Static_assert(sizeof(struct vnet_host) == 8, "vnet_host ABI");

/* key: public address */
struct vnet_fip {
    __u32 vni;
    __be32 ip;
};
_Static_assert(sizeof(struct vnet_fip) == 8, "vnet_fip ABI");

/* SNAT, reply direction: what arrives on the uplink. */
struct vnet_nat_key {
    __be32 remote_ip;
    __be32 snat_ip;
    __be16 remote_port;
    __be16 snat_port;
    __u8 proto;
    __u8 pad[3];
};
_Static_assert(sizeof(struct vnet_nat_key) == 16, "vnet_nat_key ABI");

struct vnet_nat {
    __u32 vni;
    __be32 ip;
    __be16 port;
    __u8 pad[2];
};
_Static_assert(sizeof(struct vnet_nat) == 12, "vnet_nat ABI");

/* SNAT, original direction: what the guest sends. */
struct vnet_natf_key {
    __u32 vni;
    __be32 ip;
    __be32 remote_ip;
    __be16 port;
    __be16 remote_port;
    __u8 proto;
    __u8 pad[3];
};
_Static_assert(sizeof(struct vnet_natf_key) == 20, "vnet_natf_key ABI");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, __u32);
    __type(value, struct vnet_port);
} fluxvm_vnet_port SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, __u32);
    __type(value, struct vnet_net);
} fluxvm_vnet_net SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, struct vnet_fib_key);
    __type(value, struct vnet_fib);
} fluxvm_vnet_fib SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct vnet_host);
} fluxvm_vnet_host SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, __be32);
    __type(value, struct vnet_fip);
} fluxvm_vnet_fip SEC(".maps");

/* (vni, guest ip) -> public address */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, struct vnet_fib_key);
    __type(value, __be32);
} fluxvm_vnet_fipr SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 65536);
    __type(key, struct vnet_nat_key);
    __type(value, struct vnet_nat);
} fluxvm_vnet_nat SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 65536);
    __type(key, struct vnet_natf_key);
    __type(value, __u32);
} fluxvm_vnet_natf SEC(".maps");

#define ETH_OFF   0
#define IP_OFF    ETH_HLEN
#define L4_OFF    (ETH_HLEN + sizeof(struct iphdr))
#define IP_CSUM   (IP_OFF + offsetof(struct iphdr, check))
#define IP_SADDR  (IP_OFF + offsetof(struct iphdr, saddr))
#define IP_DADDR  (IP_OFF + offsetof(struct iphdr, daddr))
#define TCP_CSUM  (L4_OFF + offsetof(struct tcphdr, check))
#define UDP_CSUM  (L4_OFF + offsetof(struct udphdr, check))
#define ICMP_CSUM (L4_OFF + offsetof(struct icmphdr, checksum))
#define ICMP_ID   (L4_OFF + offsetof(struct icmphdr, un.echo.id))
#define L4_SPORT  (L4_OFF + 0)
#define L4_DPORT  (L4_OFF + 2)

/* ICMP echo header (linux/icmp.h drags in libc headers). */
struct icmphdr {
    __u8 type;
    __u8 code;
    __sum16 checksum;
    union {
        struct {
            __be16 id;
            __be16 sequence;
        } echo;
    } un;
};

struct arp_ipv4 {
    __be16 hrd;
    __be16 pro;
    __u8 hln;
    __u8 pln;
    __be16 op;
    __u8 sha[6];
    __be32 sip;
    __u8 tha[6];
    __be32 tip;
} __attribute__((packed));

static __always_inline void gateway_mac(__u32 vni, __u8 mac[6])
{
    mac[0] = 0x02;
    mac[1] = 0x66;
    mac[2] = 0x76;
    mac[3] = (vni >> 16) & 0xff;
    mac[4] = (vni >> 8) & 0xff;
    mac[5] = vni & 0xff;
}

/* Rewrites both MACs: the frame now comes from the gateway. */
static __always_inline int set_macs(struct __sk_buff *skb, __u32 vni, const __u8 dst[6])
{
    __u8 macs[12];
    __builtin_memcpy(macs, dst, 6);
    gateway_mac(vni, macs + 6);
    return bpf_skb_store_bytes(skb, ETH_OFF, macs, sizeof(macs), 0);
}

/* The headers this program reads, or NULL when the frame is too short or
 * carries IP options (never produced by the guests this serves). */
static __always_inline struct iphdr *ipv4(struct __sk_buff *skb)
{
    void *data = (void *)(long)skb->data;
    void *end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > end || eth->h_proto != bpf_htons(ETH_P_IP))
        return NULL;
    struct iphdr *ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > end || ip->ihl != 5 || ip->version != 4)
        return NULL;
    return ip;
}

static __always_inline int deliver(struct __sk_buff *skb, __u32 vni, struct vnet_fib *f)
{
    __u8 mac[6];
    __builtin_memcpy(mac, f->mac, 6);
    __u32 kind = f->kind, ifindex = f->ifindex;
    __be32 remote = f->remote;
    if (set_macs(skb, vni, mac) < 0)
        return TC_ACT_SHOT;
    if (kind == VNET_FIB_LOCAL)
        return bpf_redirect(ifindex, 0);
    if (kind != VNET_FIB_REMOTE)
        return TC_ACT_SHOT;
    __u32 zero = 0;
    struct vnet_host *h = bpf_map_lookup_elem(&fluxvm_vnet_host, &zero);
    if (!h || !h->geneve_ifindex)
        return TC_ACT_SHOT;
    struct bpf_tunnel_key key = {};
    key.tunnel_id = vni;
    key.remote_ipv4 = bpf_ntohl(remote);
    key.tunnel_ttl = 64;
    if (bpf_skb_set_tunnel_key(skb, &key, sizeof(key), 0) < 0)
        return TC_ACT_SHOT;
    return bpf_redirect(h->geneve_ifindex, 0);
}

/* Delivers a packet addressed to (vni, ip) on this host or through Geneve. */
static __always_inline int deliver_to(struct __sk_buff *skb, __u32 vni, __be32 ip)
{
    struct vnet_fib_key k = { .vni = vni, .ip = ip };
    struct vnet_fib *f = bpf_map_lookup_elem(&fluxvm_vnet_fib, &k);
    if (!f)
        return TC_ACT_SHOT;
    return deliver(skb, vni, f);
}

/* Replaces an IPv4 address and fixes the IP and L4 checksums. */
static __always_inline int rewrite_addr(struct __sk_buff *skb, __u8 proto, int off, __be32 old, __be32 new)
{
    if (bpf_l3_csum_replace(skb, IP_CSUM, old, new, sizeof(new)) < 0)
        return -1;
    if (proto == IPPROTO_TCP) {
        if (bpf_l4_csum_replace(skb, TCP_CSUM, old, new, BPF_F_PSEUDO_HDR | sizeof(new)) < 0)
            return -1;
    } else if (proto == IPPROTO_UDP) {
        if (bpf_l4_csum_replace(skb, UDP_CSUM, old, new,
                                BPF_F_PSEUDO_HDR | BPF_F_MARK_MANGLED_0 | sizeof(new)) < 0)
            return -1;
    }
    return bpf_skb_store_bytes(skb, off, &new, sizeof(new), 0);
}

/* Replaces a TCP/UDP port or an ICMP echo id and fixes the L4 checksum. */
static __always_inline int rewrite_port(struct __sk_buff *skb, __u8 proto, int off, __be16 old, __be16 new)
{
    int csum;
    __u64 flags = sizeof(new);
    if (proto == IPPROTO_TCP)
        csum = TCP_CSUM;
    else if (proto == IPPROTO_UDP) {
        csum = UDP_CSUM;
        flags |= BPF_F_MARK_MANGLED_0;
    } else
        csum = ICMP_CSUM;
    if (bpf_l4_csum_replace(skb, csum, old, new, flags) < 0)
        return -1;
    return bpf_skb_store_bytes(skb, off, &new, sizeof(new), 0);
}

/* Ports of a flow: TCP/UDP ports, or the ICMP echo id as the local "port". */
static __always_inline int flow_ports(struct __sk_buff *skb, __u8 proto, int echo_type,
                                      __be16 *local, __be16 *remote, int *local_off)
{
    void *data = (void *)(long)skb->data;
    void *end = (void *)(long)skb->data_end;
    if (proto == IPPROTO_TCP || proto == IPPROTO_UDP) {
        __be16 *ports = data + L4_OFF;
        if ((void *)(ports + 2) > end)
            return -1;
        *local = ports[0];
        *remote = ports[1];
        *local_off = L4_SPORT;
        return 0;
    }
    if (proto == IPPROTO_ICMP) {
        struct icmphdr *ic = data + L4_OFF;
        if ((void *)(ic + 1) > end || ic->type != echo_type)
            return -1;
        *local = ic->un.echo.id;
        *remote = 0;
        *local_off = ICMP_ID;
        return 0;
    }
    return -1;
}

static __always_inline int arp_reply(struct __sk_buff *skb, struct vnet_port *p)
{
    void *data = (void *)(long)skb->data;
    void *end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    struct arp_ipv4 *a = (void *)(eth + 1);
    if ((void *)(a + 1) > end)
        return TC_ACT_SHOT;
    if (a->hrd != bpf_htons(1) || a->pro != bpf_htons(ETH_P_IP) || a->hln != 6 || a->pln != 4 ||
        a->op != bpf_htons(1))
        return TC_ACT_SHOT;
    /* Address conflict probes for the guest's own address get no answer. */
    if (a->tip == p->ip || a->tip == 0)
        return TC_ACT_SHOT;
    struct {
        struct ethhdr eth;
        struct arp_ipv4 arp;
    } __attribute__((packed)) r;
    __builtin_memcpy(r.eth.h_dest, a->sha, 6);
    gateway_mac(p->vni, r.eth.h_source);
    r.eth.h_proto = bpf_htons(ETH_P_ARP);
    r.arp.hrd = bpf_htons(1);
    r.arp.pro = bpf_htons(ETH_P_IP);
    r.arp.hln = 6;
    r.arp.pln = 4;
    r.arp.op = bpf_htons(2);
    gateway_mac(p->vni, r.arp.sha);
    r.arp.sip = a->tip;
    __builtin_memcpy(r.arp.tha, a->sha, 6);
    r.arp.tip = a->sip;
    if (bpf_skb_store_bytes(skb, 0, &r, sizeof(r), 0) < 0)
        return TC_ACT_SHOT;
    return bpf_redirect(skb->ifindex, 0);
}

/* Answers an ICMP echo request to the gateway address in place. */
static __always_inline int gateway_echo(struct __sk_buff *skb, __u32 vni)
{
    void *data = (void *)(long)skb->data;
    void *end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    struct iphdr *ip = data + IP_OFF;
    struct icmphdr *ic = data + L4_OFF;
    if ((void *)(ic + 1) > end || ip->protocol != IPPROTO_ICMP || ic->type != 8 || ic->code != 0)
        return TC_ACT_SHOT;
    __u8 guest[6];
    __builtin_memcpy(guest, eth->h_source, 6);
    __be32 src = ip->saddr, dst = ip->daddr;
    /* Swapping the addresses leaves the IP checksum unchanged. */
    if (bpf_skb_store_bytes(skb, IP_SADDR, &dst, sizeof(dst), 0) < 0 ||
        bpf_skb_store_bytes(skb, IP_DADDR, &src, sizeof(src), 0) < 0)
        return TC_ACT_SHOT;
    __be16 old_tc = bpf_htons(0x0800), new_tc = 0;
    if (bpf_l4_csum_replace(skb, ICMP_CSUM, old_tc, new_tc, sizeof(new_tc)) < 0 ||
        bpf_skb_store_bytes(skb, L4_OFF, &new_tc, sizeof(new_tc), 0) < 0)
        return TC_ACT_SHOT;
    if (set_macs(skb, vni, guest) < 0)
        return TC_ACT_SHOT;
    return bpf_redirect(skb->ifindex, 0);
}

/* Picks a free SNAT port for this flow (or its existing one). */
static __always_inline int snat_port(struct vnet_natf_key *fk, __be32 snat_ip, __be16 *out)
{
    __u32 *have = bpf_map_lookup_elem(&fluxvm_vnet_natf, fk);
    if (have) {
        *out = (__be16)*have;
        return 0;
    }
    __u32 h = fk->ip * 2654435761u ^ fk->remote_ip ^ ((__u32)fk->port << 16 | fk->remote_port) ^ fk->vni;
    h ^= h >> 15;
    struct vnet_nat v = { .vni = fk->vni, .ip = fk->ip, .port = fk->port };
    for (int i = 0; i < VNET_NAT_PROBES; i++) {
        __be16 port = bpf_htons(VNET_NAT_PORT_MIN + (h + i * 7919) % VNET_NAT_PORT_SPAN);
        struct vnet_nat_key rk = {
            .remote_ip = fk->remote_ip,
            .snat_ip = snat_ip,
            .remote_port = fk->remote_port,
            .snat_port = port,
            .proto = fk->proto,
        };
        if (bpf_map_update_elem(&fluxvm_vnet_nat, &rk, &v, BPF_NOEXIST) == 0) {
            __u32 val = port;
            bpf_map_update_elem(&fluxvm_vnet_natf, fk, &val, BPF_ANY);
            *out = port;
            return 0;
        }
    }
    return -1;
}

static __always_inline int leave_network(struct __sk_buff *skb, struct vnet_port *p, struct vnet_net *n,
                                         struct iphdr *ip)
{
    __u32 vni = p->vni, uplink = n->uplink_ifindex;
    __be32 src = ip->saddr, dst = ip->daddr, snat_ip = n->snat_ip;
    __u8 proto = ip->protocol;
    if (!uplink)
        return TC_ACT_SHOT;

    struct vnet_fib_key gk = { .vni = vni, .ip = src };
    __be32 *public = bpf_map_lookup_elem(&fluxvm_vnet_fipr, &gk);
    if (public) {
        __be32 pub = *public;
        if (rewrite_addr(skb, proto, IP_SADDR, src, pub) < 0)
            return TC_ACT_SHOT;
        return bpf_redirect_neigh(uplink, NULL, 0, 0);
    }
    if (!snat_ip)
        return TC_ACT_SHOT;

    __be16 local, remote;
    int local_off;
    if (flow_ports(skb, proto, 8 /* echo request */, &local, &remote, &local_off) < 0)
        return TC_ACT_SHOT;
    struct vnet_natf_key fk = {
        .vni = vni,
        .ip = src,
        .remote_ip = dst,
        .port = local,
        .remote_port = remote,
        .proto = proto,
    };
    __be16 port;
    if (snat_port(&fk, snat_ip, &port) < 0)
        return TC_ACT_SHOT;
    if (rewrite_port(skb, proto, local_off, local, port) < 0)
        return TC_ACT_SHOT;
    if (rewrite_addr(skb, proto, IP_SADDR, src, snat_ip) < 0)
        return TC_ACT_SHOT;
    return bpf_redirect_neigh(uplink, NULL, 0, 0);
}

SEC("tc")
int vnet_guest(struct __sk_buff *skb)
{
    __u32 ifindex = skb->ifindex;
    struct vnet_port *p = bpf_map_lookup_elem(&fluxvm_vnet_port, &ifindex);
    if (!p)
        return TC_ACT_UNSPEC;

    void *data = (void *)(long)skb->data;
    void *end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > end)
        return TC_ACT_SHOT;
    if (eth->h_proto == bpf_htons(ETH_P_ARP))
        return arp_reply(skb, p);

    struct iphdr *ip = ipv4(skb);
    if (!ip || ip->saddr != p->ip)
        return TC_ACT_SHOT;
    __u32 vni = p->vni;
    struct vnet_net *n = bpf_map_lookup_elem(&fluxvm_vnet_net, &vni);
    if (!n)
        return TC_ACT_SHOT;
    __be32 dst = ip->daddr;
    if (dst == n->gateway)
        return gateway_echo(skb, vni);
    if ((dst & n->mask) == n->net)
        return deliver_to(skb, vni, dst);
    return leave_network(skb, p, n, ip);
}

SEC("tc")
int vnet_tunnel(struct __sk_buff *skb)
{
    struct bpf_tunnel_key key = {};
    if (bpf_skb_get_tunnel_key(skb, &key, sizeof(key), 0) < 0)
        return TC_ACT_SHOT;
    struct iphdr *ip = ipv4(skb);
    if (!ip)
        return TC_ACT_SHOT;
    struct vnet_fib_key k = { .vni = key.tunnel_id, .ip = ip->daddr };
    struct vnet_fib *f = bpf_map_lookup_elem(&fluxvm_vnet_fib, &k);
    /* Only local guests: never bounce a packet back into the overlay. */
    if (!f || f->kind != VNET_FIB_LOCAL)
        return TC_ACT_SHOT;
    return deliver(skb, key.tunnel_id, f);
}

SEC("tc")
int vnet_uplink(struct __sk_buff *skb)
{
    struct iphdr *ip = ipv4(skb);
    if (!ip)
        return TC_ACT_UNSPEC;
    __be32 src = ip->saddr, dst = ip->daddr;
    __u8 proto = ip->protocol;

    struct vnet_fip *fip = bpf_map_lookup_elem(&fluxvm_vnet_fip, &dst);
    if (fip) {
        __u32 vni = fip->vni;
        __be32 guest = fip->ip;
        if (rewrite_addr(skb, proto, IP_DADDR, dst, guest) < 0)
            return TC_ACT_SHOT;
        return deliver_to(skb, vni, guest);
    }

    __be16 local, remote;
    int local_off;
    /* Reply direction: the remote's port is the source, ours the destination. */
    if (proto == IPPROTO_TCP || proto == IPPROTO_UDP) {
        void *data = (void *)(long)skb->data;
        void *end = (void *)(long)skb->data_end;
        __be16 *ports = data + L4_OFF;
        if ((void *)(ports + 2) > end)
            return TC_ACT_UNSPEC;
        remote = ports[0];
        local = ports[1];
        local_off = L4_DPORT;
    } else if (flow_ports(skb, proto, 0 /* echo reply */, &local, &remote, &local_off) < 0) {
        return TC_ACT_UNSPEC;
    }
    struct vnet_nat_key rk = {
        .remote_ip = src,
        .snat_ip = dst,
        .remote_port = remote,
        .snat_port = local,
        .proto = proto,
    };
    struct vnet_nat *v = bpf_map_lookup_elem(&fluxvm_vnet_nat, &rk);
    if (!v)
        return TC_ACT_UNSPEC;
    __u32 vni = v->vni;
    __be32 guest = v->ip;
    __be16 port = v->port;
    if (rewrite_port(skb, proto, local_off, local, port) < 0)
        return TC_ACT_SHOT;
    if (rewrite_addr(skb, proto, IP_DADDR, dst, guest) < 0)
        return TC_ACT_SHOT;
    return deliver_to(skb, vni, guest);
}

char LICENSE[] SEC("license") = "GPL";
