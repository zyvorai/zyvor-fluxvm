// Copyright 2026 Zyvor
// SPDX-License-Identifier: GPL-2.0-only
//
// Optional standalone-node XDP guard. Do not enable this on a Cilium-owned
// interface: Cilium may itself own an XDP program for acceleration.

#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/ipv6.h>
#include <bpf/bpf_endian.h>
#include <bpf/bpf_helpers.h>

struct ipv4_lpm_key {
    __u32 prefixlen;
    __u32 addr;
};

struct ipv6_lpm_key {
    __u32 prefixlen;
    __u8 addr[16];
};

struct {
    __uint(type, BPF_MAP_TYPE_LPM_TRIE);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __uint(max_entries, 4096);
    __type(key, struct ipv4_lpm_key);
    __type(value, __u32);
} fvm_xdp_block4 SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LPM_TRIE);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __uint(max_entries, 4096);
    __type(key, struct ipv6_lpm_key);
    __type(value, __u32);
} fvm_xdp_block6 SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 2);
    __type(key, __u32);
    __type(value, __u64);
} fvm_xdp_stats SEC(".maps");

static __always_inline void count(__u32 verdict)
{
    __u64 *v = bpf_map_lookup_elem(&fvm_xdp_stats, &verdict);
    if (v)
        *v += 1;
}

SEC("xdp")
int fluxvm_xdp_guard(struct xdp_md *ctx)
{
    void *data = (void *)(long)ctx->data;
    void *data_end = (void *)(long)ctx->data_end;
    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > data_end)
        return XDP_DROP;

    __u16 proto = bpf_ntohs(eth->h_proto);
    if (proto == ETH_P_IP) {
        struct iphdr *iph = (void *)(eth + 1);
        if ((void *)(iph + 1) > data_end) {
            count(0);
            return XDP_DROP;
        }
        struct ipv4_lpm_key key = {
            .prefixlen = 32,
            .addr = iph->saddr,
        };
        __u32 *blocked = bpf_map_lookup_elem(&fvm_xdp_block4, &key);
        if (blocked && *blocked) {
            count(0);
            return XDP_DROP;
        }
    } else if (proto == ETH_P_IPV6) {
        struct ipv6hdr *ip6 = (void *)(eth + 1);
        if ((void *)(ip6 + 1) > data_end) {
            count(0);
            return XDP_DROP;
        }
        struct ipv6_lpm_key key = {
            .prefixlen = 128,
        };
        __builtin_memcpy(key.addr, ip6->saddr.in6_u.u6_addr8, 16);
        __u32 *blocked = bpf_map_lookup_elem(&fvm_xdp_block6, &key);
        if (blocked && *blocked) {
            count(0);
            return XDP_DROP;
        }
    }

    count(1);
    return XDP_PASS;
}

char LICENSE[] SEC("license") = "GPL";
