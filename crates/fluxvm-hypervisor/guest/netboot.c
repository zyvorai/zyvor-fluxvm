// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

/* Freestanding 64-bit guest: serial + virtio-mmio-net + ARP + ICMP ping. */
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;

#define MMIO 0xFEB00000ULL
#define COM1 0x3F8
#define QNUM 8
#define RX_DESC 0x400000ULL
#define RX_AVAIL 0x401000ULL
#define RX_USED 0x402000ULL
#define TX_DESC 0x403000ULL
#define TX_AVAIL 0x404000ULL
#define TX_USED 0x405000ULL
#define RX_BUFS 0x410000ULL
#define TX_BUFS 0x450000ULL
#define BUF_SZ 2048

#define VRING_DESC_F_NEXT 1
#define VRING_DESC_F_WRITE 2

#define VIRTIO_CONFIG_S_ACKNOWLEDGE 1
#define VIRTIO_CONFIG_S_DRIVER 2
#define VIRTIO_CONFIG_S_DRIVER_OK 4
#define VIRTIO_CONFIG_S_FEATURES_OK 8

static inline void outb(u16 port, u8 v) {
    __asm__ volatile("outb %0, %1" ::"a"(v), "Nd"(port));
}

static void sput(const char *s) {
    while (*s)
        outb(COM1, (u8)*s++);
}

static void sputhex(u32 v) {
    const char *h = "0123456789abcdef";
    for (int i = 7; i >= 0; i--)
        outb(COM1, (u8)h[(v >> (i * 4)) & 0xf]);
}

static inline u32 mmr(u64 off) { return *(volatile u32 *)(MMIO + off); }
static inline void mmw(u64 off, u32 v) { *(volatile u32 *)(MMIO + off) = v; }
static inline u8 mmb(u64 off) { return *(volatile u8 *)(MMIO + off); }

struct desc {
    u64 addr;
    u32 len;
    u16 flags;
    u16 next;
};

struct avail {
    u16 flags;
    u16 idx;
    u16 ring[QNUM];
};

struct used_elem {
    u32 id;
    u32 len;
};

struct used {
    u16 flags;
    u16 idx;
    struct used_elem ring[QNUM];
};

static void *zmem(u64 a, u32 n) {
    u8 *p = (u8 *)a;
    for (u32 i = 0; i < n; i++)
        p[i] = 0;
    return p;
}

static void setup_queue(u32 sel, u64 desc, u64 avail, u64 used) {
    mmw(0x030, sel);
    u32 max = mmr(0x034);
    if (max < QNUM) {
        sput("queue too small\n");
    }
    mmw(0x038, QNUM);
    mmw(0x080, (u32)desc);
    mmw(0x084, (u32)(desc >> 32));
    mmw(0x090, (u32)avail);
    mmw(0x094, (u32)(avail >> 32));
    mmw(0x0a0, (u32)used);
    mmw(0x0a4, (u32)(used >> 32));
    mmw(0x044, 1);
}

static u16 htons_local(u16 x) { return (u16)((x << 8) | (x >> 8)); }

static u16 cksum(const u8 *p, int n) {
    u32 s = 0;
    for (int i = 0; i + 1 < n; i += 2)
        s += ((u32)p[i] << 8) | p[i + 1];
    if (n & 1)
        s += (u32)p[n - 1] << 8;
    while (s >> 16)
        s = (s & 0xffff) + (s >> 16);
    return (u16)~s;
}

static volatile u16 tx_avail_idx;
static volatile u16 rx_avail_idx;
static u8 gmac[6];

static void rx_replenish(void) {
    struct desc *d = (struct desc *)RX_DESC;
    struct avail *a = (struct avail *)RX_AVAIL;
    for (u32 i = 0; i < QNUM; i++) {
        d[i].addr = RX_BUFS + (u64)i * BUF_SZ;
        d[i].len = BUF_SZ;
        d[i].flags = VRING_DESC_F_WRITE;
        d[i].next = 0;
        a->ring[i] = (u16)i;
    }
    a->flags = 0;
    rx_avail_idx = QNUM;
    a->idx = rx_avail_idx;
    mmw(0x050, 0);
}

static void tx_frame(const u8 *eth, u32 elen) {
    sput("tx_frame\n");
    struct desc *d = (struct desc *)TX_DESC;
    struct avail *a = (struct avail *)TX_AVAIL;
    u32 slot = tx_avail_idx % QNUM;
    u8 *buf = (u8 *)(TX_BUFS + (u64)slot * BUF_SZ);
    for (int i = 0; i < 12; i++)
        buf[i] = 0; /* virtio_net_hdr */
    for (u32 i = 0; i < elen; i++)
        buf[12 + i] = eth[i];
    d[slot].addr = TX_BUFS + (u64)slot * BUF_SZ;
    d[slot].len = 12 + elen;
    d[slot].flags = 0;
    d[slot].next = 0;
    a->ring[slot] = (u16)slot;
    tx_avail_idx++;
    a->idx = tx_avail_idx;
    mmw(0x050, 1);
    sput("tx_kicked\n");
}

static int parse_arp_reply(const u8 *eth, u32 n) {
    if (n < 42)
        return 0;
    u16 et = ((u16)eth[12] << 8) | eth[13];
    if (et != 0x0806)
        return 0;
    u16 op = ((u16)eth[20] << 8) | eth[21];
    return op == 2;
}

static int parse_icmp_reply(const u8 *eth, u32 n) {
    if (n < 34)
        return 0;
    u16 et = ((u16)eth[12] << 8) | eth[13];
    if (et != 0x0800)
        return 0;
    if (eth[23] != 1)
        return 0;
    return eth[34] == 0;
}

__attribute__((section(".text.start")))
void _start(void) {
    sput("FluxVM guest boot\n");

    u32 magic = mmr(0x000);
    u32 ver = mmr(0x004);
    u32 id = mmr(0x008);
    sput("virtio magic=");
    sputhex(magic);
    sput(" ver=");
    sputhex(ver);
    sput(" id=");
    sputhex(id);
    sput("\n");
    if (magic != 0x74726976 || id != 1) {
        sput("no virtio-net\n");
        goto halt;
    }

    mmw(0x070, VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER);

    mmw(0x014, 0);
    u32 f0 = mmr(0x010);
    mmw(0x014, 1);
    u32 f1 = mmr(0x010);
    /* MAC + STATUS + VERSION_1 */
    mmw(0x024, 0);
    mmw(0x020, f0);
    mmw(0x024, 1);
    mmw(0x020, f1 | (1u)); /* bit 32 in high word = VERSION_1 */

    mmw(0x070, VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER |
                   VIRTIO_CONFIG_S_FEATURES_OK);
    u32 st = mmr(0x070);
    if (!(st & VIRTIO_CONFIG_S_FEATURES_OK)) {
        sput("features rejected\n");
        goto halt;
    }

    for (int i = 0; i < 6; i++)
        gmac[i] = mmb(0x100 + (u64)i);
    sput("mac=");
    for (int i = 0; i < 6; i++) {
        sputhex(gmac[i]);
        if (i < 5)
            outb(COM1, ':');
    }
    sput("\n");

    zmem(RX_DESC, 0x3000);
    zmem(TX_DESC, 0x3000);
    setup_queue(0, RX_DESC, RX_AVAIL, RX_USED);
    setup_queue(1, TX_DESC, TX_AVAIL, TX_USED);
    mmw(0x070, VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER |
                   VIRTIO_CONFIG_S_FEATURES_OK | VIRTIO_CONFIG_S_DRIVER_OK);

    rx_replenish();
    tx_avail_idx = 0;
    ((struct avail *)TX_AVAIL)->flags = 0;
    ((struct avail *)TX_AVAIL)->idx = 0;
    sput("virtio-net ready\n");

    /* ARP request: who-has 192.168.100.1 tell 192.168.100.2 */
    u8 pkt[64];
    for (int i = 0; i < 64; i++)
        pkt[i] = 0;
    for (int i = 0; i < 6; i++)
        pkt[i] = 0xff;
    for (int i = 0; i < 6; i++)
        pkt[6 + i] = gmac[i];
    pkt[12] = 0x08;
    pkt[13] = 0x06;
    pkt[14] = 0x00;
    pkt[15] = 0x01;
    pkt[16] = 0x08;
    pkt[17] = 0x00;
    pkt[18] = 0x06;
    pkt[19] = 0x04;
    pkt[20] = 0x00;
    pkt[21] = 0x01;
    for (int i = 0; i < 6; i++)
        pkt[22 + i] = gmac[i];
    pkt[28] = 192;
    pkt[29] = 168;
    pkt[30] = 100;
    pkt[31] = 2;
    pkt[38] = 192;
    pkt[39] = 168;
    pkt[40] = 100;
    pkt[41] = 1;
    tx_frame(pkt, 42);
    sput("arp who-has 192.168.100.1\n");

    int got_arp = 0, got_ping = 0;
    u16 last_used = 0;
    struct used *ru = (struct used *)RX_USED;
    for (u32 spin = 0; spin < 20000000u; spin++) {
        u16 idx = ru->idx;
        while (last_used != idx) {
            struct used_elem e = ru->ring[last_used % QNUM];
            u8 *buf = (u8 *)(RX_BUFS + (u64)e.id * BUF_SZ);
            u32 n = e.len;
            const u8 *eth = buf + 12;
            u32 elen = n > 12 ? n - 12 : 0;
            if (!got_arp && parse_arp_reply(eth, elen)) {
                sput("arp reply — link up\n");
                got_arp = 1;
                /* ICMP echo to 192.168.100.1 */
                u8 icmp[42 + 8 + 8];
                for (int i = 0; i < (int)sizeof(icmp); i++)
                    icmp[i] = 0;
                /* dst mac = sender mac of ARP (offset 22 in ARP reply eth) */
                for (int i = 0; i < 6; i++)
                    icmp[i] = eth[6 + i];
                for (int i = 0; i < 6; i++)
                    icmp[6 + i] = gmac[i];
                icmp[12] = 0x08;
                icmp[13] = 0x00;
                icmp[14] = 0x45;
                icmp[15] = 0x00;
                u16 iplen = 20 + 8 + 8;
                icmp[16] = (u8)(iplen >> 8);
                icmp[17] = (u8)iplen;
                icmp[18] = 0;
                icmp[19] = 1;
                icmp[22] = 64;
                icmp[23] = 1;
                icmp[26] = 192;
                icmp[27] = 168;
                icmp[28] = 100;
                icmp[29] = 2;
                icmp[30] = 192;
                icmp[31] = 168;
                icmp[32] = 100;
                icmp[33] = 1;
                u16 ipc = cksum(icmp + 14, 20);
                icmp[24] = (u8)(ipc >> 8);
                icmp[25] = (u8)ipc;
                icmp[34] = 8;
                icmp[35] = 0;
                icmp[38] = 0x13;
                icmp[39] = 0x37;
                u16 ic = cksum(icmp + 34, 16);
                icmp[36] = (u8)(ic >> 8);
                icmp[37] = (u8)ic;
                tx_frame(icmp, 42 + 8);
                sput("icmp echo -> 192.168.100.1\n");
            } else if (got_arp && parse_icmp_reply(eth, elen)) {
                sput("PING OK\n");
                got_ping = 1;
            }
            /* recycle RX desc */
            struct desc *d = (struct desc *)RX_DESC;
            d[e.id].addr = RX_BUFS + (u64)e.id * BUF_SZ;
            d[e.id].len = BUF_SZ;
            d[e.id].flags = VRING_DESC_F_WRITE;
            struct avail *av = (struct avail *)RX_AVAIL;
            av->ring[rx_avail_idx % QNUM] = (u16)e.id;
            rx_avail_idx++;
            av->idx = rx_avail_idx;
            last_used++;
        }
        if (got_ping)
            break;
        if ((spin & 0xfffff) == 0)
            mmw(0x050, 0);
    }

    if (got_ping)
        sput("NETWORK IS UP\n");
    else if (got_arp)
        sput("LINK UP but ping timeout\n");
    else
        sput("NET TIMEOUT\n");

halt:
    for (;;)
        __asm__ volatile("hlt");
}
