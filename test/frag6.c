// MTU より大きい UDP (Linux の __ip6_append_data / ip6_fragment と __ip_append_data / ip_fragment): 断片にして送る
// か、EMSGSIZE か。IPV6_DONTFRAG (と付帯データ)、IPV6_RECVPATHMTU / IPV6_PATHMTU (ipv6_local_rxpmtu)、IPV6_MTU
// (frag_size)、IPV6_MTU_DISCOVER (DO・PROBE・INTERFACE は断片にしない、OMIT と DONT はする)、IPV6_RECVERR の
// SO_EE_ORIGIN_LOCAL (ipv6_local_error)、いちばん大きいもの。IPv4 も (IP_MTU_DISCOVER、IP_RECVERR、65492 まで)。
// packet too big で path MTU を 1280 にしたあて先にも。断片は外に出るだけなので、eth0 の送ったフレームの数とバイト数
// (/sys/class/net/eth0/statistics の tx_packets と tx_bytes) で見る。近所さがしのフレームが入らないように、IPv6 は
// マルチキャスト (ff02::1234)、IPv4 はブロードキャスト (10.0.2.255) に送り、ルーターを通るユニキャストは最後に
// (Linux の近所さがしの NS が出ても、もうかぞえない)
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net frag6)
//   aios の中で: sudo ./frag6      aarch64-linux-gnu-gcc -O2 -static -o frag6 test/frag6.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/errqueue.h>
#include <net/if.h>
#include <netinet/ip6.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#ifndef IPV6_RECVPATHMTU
#define IPV6_RECVPATHMTU 60
#define IPV6_PATHMTU 61
#define IPV6_DONTFRAG 62
#endif
#ifndef IPV6_PMTUDISC_INTERFACE
#define IPV6_PMTUDISC_INTERFACE 4
#define IPV6_PMTUDISC_OMIT 5
#endif

static char big[70000];
static unsigned eth0;

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case EMSGSIZE: return "EMSGSIZE";
    case EAGAIN: return "EAGAIN";
    case ENOTCONN: return "ENOTCONN";
    case ENOPROTOOPT: return "ENOPROTOOPT";
    case EACCES: return "EACCES";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "errno%d", e);
        return b;
    }
    }
}

static void ms_sleep(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

// eth0 の SLAAC の番地 (DAD がおわったもの)
static int my_addr(struct in6_addr *me) {
    for (int i = 0; i < 300; i++) {
        FILE *f = fopen("/proc/net/if_inet6", "r");
        char line[256];
        int ok = 0;
        while (f && fgets(line, sizeof line, f)) {
            unsigned idx, plen, scope, flags;
            char addr[40], name[32];
            if (sscanf(line, "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) == 6 && !strcmp(name, "eth0") && scope != 0x20 &&
                !(flags & 0x41)) {
                for (int k = 0; k < 16; k++)
                    sscanf(addr + 2 * k, "%2hhx", &me->s6_addr[k]);
                ok = 1;
            }
        }
        if (f)
            fclose(f);
        if (ok)
            return 1;
        ms_sleep(100);
    }
    return 0;
}

static unsigned long long stat_of(const char *n) {
    char p[128];
    snprintf(p, sizeof p, "/sys/class/net/eth0/statistics/%s", n);
    FILE *f = fopen(p, "r");
    unsigned long long v = 0;
    if (f) {
        if (fscanf(f, "%llu", &v) != 1)
            v = 0;
        fclose(f);
    }
    return v;
}

// 送って、eth0 から出たフレームの数とバイト数
static void tx(int s, const struct sockaddr *to, socklen_t tl, size_t len, const char *what) {
    unsigned long long p0 = stat_of("tx_packets"), b0 = stat_of("tx_bytes");
    long r = sendto(s, big, len, 0, to, tl);
    int e = r < 0 ? errno : 0;
    ms_sleep(150);
    printf("  %s %zu: %s, tx +%llu frames +%llu bytes\n", what, len, r < 0 ? en(e) : r == (long)len ? "ok" : "short", stat_of("tx_packets") - p0,
           stat_of("tx_bytes") - b0);
}

// IPV6_PATHMTU の付帯データ (ipv6_recv_rxpmtu) と、エラーの行列 (MSG_ERRQUEUE)。どちらも読めるだけ
static void pending(int s, const char *what) {
    for (int q = 0; q < 2; q++) {
        for (int i = 0; i < 4; i++) {
            char data[64], cbuf[256];
            struct sockaddr_in6 from;
            memset(&from, 0, sizeof from);
            struct iovec iov = {data, sizeof data};
            struct msghdr m = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
            long r = recvmsg(s, &m, MSG_DONTWAIT | (q ? MSG_ERRQUEUE : 0));
            if (r < 0) {
                printf("  %s %s: %s\n", what, q ? "errqueue" : "recv", en(errno));
                break;
            }
            char a[INET6_ADDRSTRLEN] = "";
            if (from.sin6_family == AF_INET6)
                inet_ntop(AF_INET6, &from.sin6_addr, a, sizeof a);
            else if (from.sin6_family == AF_INET)
                inet_ntop(AF_INET, &((struct sockaddr_in *)&from)->sin_addr, a, sizeof a);
            printf("  %s %s: %ld bytes from fam %u %s port %u scope %u flags %#x\n", what, q ? "errqueue" : "recv", r, from.sin6_family, a,
                   ntohs(from.sin6_port), from.sin6_family == AF_INET6 ? from.sin6_scope_id : 0, m.msg_flags);
            for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
                if (c->cmsg_level == IPPROTO_IPV6 && c->cmsg_type == IPV6_PATHMTU) {
                    struct ip6_mtuinfo *mi = (void *)CMSG_DATA(c);
                    char b[INET6_ADDRSTRLEN];
                    inet_ntop(AF_INET6, &mi->ip6m_addr.sin6_addr, b, sizeof b);
                    printf("    IPV6_PATHMTU len %zu: fam %u %s port %u scope %u mtu %u\n", (size_t)(c->cmsg_len - CMSG_LEN(0)), mi->ip6m_addr.sin6_family, b,
                           ntohs(mi->ip6m_addr.sin6_port), mi->ip6m_addr.sin6_scope_id, mi->ip6m_mtu);
                } else if ((c->cmsg_level == IPPROTO_IPV6 && c->cmsg_type == IPV6_RECVERR) || (c->cmsg_level == IPPROTO_IP && c->cmsg_type == IP_RECVERR)) {
                    struct sock_extended_err *ee = (void *)CMSG_DATA(c);
                    struct sockaddr *off = SO_EE_OFFENDER(ee);
                    printf("    %s errno %s origin %u type %u code %u info %u data %u offender fam %u\n", c->cmsg_level == IPPROTO_IP ? "IP_RECVERR" : "IPV6_RECVERR",
                           en(ee->ee_errno), ee->ee_origin, ee->ee_type, ee->ee_code, ee->ee_info, ee->ee_data, off->sa_family);
                } else {
                    printf("    cmsg level %d type %d len %zu\n", c->cmsg_level, c->cmsg_type, (size_t)(c->cmsg_len - CMSG_LEN(0)));
                }
            }
        }
    }
}

static void setint(int s, int level, int opt, int v, const char *name) {
    int r = setsockopt(s, level, opt, &v, sizeof v);
    printf("set %s %d: %s\n", name, v, r ? en(errno) : "ok");
}

static void getint(int s, int level, int opt, const char *name) {
    int v = -1;
    socklen_t l = sizeof v;
    int r = getsockopt(s, level, opt, &v, &l);
    printf("get %s: %s %d\n", name, r ? en(errno) : "ok", v);
}

static void pathmtu(int s, const char *what) {
    struct ip6_mtuinfo mi;
    memset(&mi, 0xee, sizeof mi);
    socklen_t l = sizeof mi;
    int r = getsockopt(s, IPPROTO_IPV6, IPV6_PATHMTU, &mi, &l);
    if (r) {
        printf("get IPV6_PATHMTU %s: %s\n", what, en(errno));
        return;
    }
    char b[INET6_ADDRSTRLEN];
    inet_ntop(AF_INET6, &mi.ip6m_addr.sin6_addr, b, sizeof b);
    printf("get IPV6_PATHMTU %s: ok len %u fam %u %s mtu %u\n", what, (unsigned)l, mi.ip6m_addr.sin6_family, b, mi.ip6m_mtu);
}

// sendmsg で付帯データの IPV6_DONTFRAG (clen はその長さ)
static void send_dontfrag(int s, const struct sockaddr_in6 *to, size_t len, int v, int clen) {
    char cbuf[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec iov = {big, len};
    struct msghdr m = {.msg_name = (void *)to, .msg_namelen = sizeof *to, .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = IPPROTO_IPV6;
    c->cmsg_type = IPV6_DONTFRAG;
    c->cmsg_len = CMSG_LEN(clen);
    memcpy(CMSG_DATA(c), &v, sizeof v);
    long r = sendmsg(s, &m, 0);
    printf("  sendmsg IPV6_DONTFRAG %d (cmsg len %d) %zu: %s\n", v, clen, len, r < 0 ? en(errno) : "ok");
}

// packet too big (mtu) を自分あてに: 中は me:sport から dst:9 への UDP
static void ptb(const struct in6_addr *me, const struct in6_addr *dst, unsigned short sport, unsigned mtu) {
    int r = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    unsigned char p[8 + 40 + 8 + 4] = {0};
    p[0] = 2;
    *(unsigned *)(p + 4) = htonl(mtu);
    struct ip6_hdr *ip = (void *)(p + 8);
    ip->ip6_flow = htonl(6u << 28);
    ip->ip6_plen = htons(8 + 1400);
    ip->ip6_nxt = IPPROTO_UDP;
    ip->ip6_hlim = 64;
    ip->ip6_src = *me;
    ip->ip6_dst = *dst;
    unsigned char *u = p + 48;
    *(unsigned short *)u = htons(sport);
    *(unsigned short *)(u + 2) = htons(9);
    *(unsigned short *)(u + 4) = htons(8 + 1400);
    memcpy(p + 56, "data", 4);
    struct sockaddr_in6 to = {.sin6_family = AF_INET6, .sin6_addr = *me};
    long n = sendto(r, p, sizeof p, 0, (struct sockaddr *)&to, sizeof to);
    printf("ptb mtu %u: %s\n", mtu, n < 0 ? en(errno) : "sent");
    close(r);
    ms_sleep(200);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    memset(big, 'x', sizeof big);
    eth0 = if_nametoindex("eth0");
    struct in6_addr me;
    if (!my_addr(&me)) {
        printf("no address\n");
        return 1;
    }
    // Linux は RA が来るまで RS をくりかえす (4 秒から倍に): かぞえるフレームに入らないように止める (aios にはない)
    FILE *rs = fopen("/proc/sys/net/ipv6/conf/eth0/router_solicitations", "w");
    if (rs) {
        fputs("0", rs);
        fclose(rs);
    }
    struct sockaddr_in6 mc = {.sin6_family = AF_INET6, .sin6_port = htons(9), .sin6_scope_id = eth0};
    inet_pton(AF_INET6, "ff02::1234", &mc.sin6_addr);
    const struct sockaddr *to6 = (struct sockaddr *)&mc;

    printf("== IPv6 (ff02::1234 on eth0, mtu 1500)\n");
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    getint(s, IPPROTO_IPV6, IPV6_DONTFRAG, "IPV6_DONTFRAG");
    getint(s, IPPROTO_IPV6, IPV6_RECVPATHMTU, "IPV6_RECVPATHMTU");
    getint(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER, "IPV6_MTU_DISCOVER");
    pathmtu(s, "unconnected");
    setint(s, IPPROTO_IPV6, IPV6_PATHMTU, 1, "IPV6_PATHMTU");
    tx(s, to6, sizeof mc, 100, "warm");
    tx(s, to6, sizeof mc, 1452, "send");
    tx(s, to6, sizeof mc, 1453, "send");
    tx(s, to6, sizeof mc, 4000, "send");
    tx(s, to6, sizeof mc, 65527, "send");
    tx(s, to6, sizeof mc, 65528, "send");
    pending(s, "plain");

    printf("-- IPV6_DONTFRAG\n");
    setint(s, IPPROTO_IPV6, IPV6_DONTFRAG, 1, "IPV6_DONTFRAG");
    getint(s, IPPROTO_IPV6, IPV6_DONTFRAG, "IPV6_DONTFRAG");
    tx(s, to6, sizeof mc, 1452, "send");
    tx(s, to6, sizeof mc, 1453, "send");
    pending(s, "no RECVPATHMTU");
    setint(s, IPPROTO_IPV6, IPV6_RECVPATHMTU, 1, "IPV6_RECVPATHMTU");
    tx(s, to6, sizeof mc, 1453, "send");
    tx(s, to6, sizeof mc, 3000, "send");
    pending(s, "RECVPATHMTU");
    setint(s, IPPROTO_IPV6, IPV6_RECVERR, 1, "IPV6_RECVERR");
    tx(s, to6, sizeof mc, 1453, "send");
    pending(s, "RECVPATHMTU+RECVERR");
    setint(s, IPPROTO_IPV6, IPV6_DONTFRAG, 0, "IPV6_DONTFRAG");
    setint(s, IPPROTO_IPV6, IPV6_RECVPATHMTU, 0, "IPV6_RECVPATHMTU");

    printf("-- IPV6_MTU_DISCOVER\n");
    for (int v = 0; v <= 5; v++) {
        setint(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER, v, "IPV6_MTU_DISCOVER");
        tx(s, to6, sizeof mc, 1453, "send");
        pending(s, "pmtudisc");
    }
    setint(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER, 1, "IPV6_MTU_DISCOVER");

    printf("-- IPV6_MTU (frag_size)\n");
    setint(s, IPPROTO_IPV6, IPV6_MTU, 1279, "IPV6_MTU");
    setint(s, IPPROTO_IPV6, IPV6_MTU, 1280, "IPV6_MTU");
    getint(s, IPPROTO_IPV6, IPV6_MTU, "IPV6_MTU");
    tx(s, to6, sizeof mc, 1232, "send");
    tx(s, to6, sizeof mc, 1233, "send");
    tx(s, to6, sizeof mc, 4000, "send");
    setint(s, IPPROTO_IPV6, IPV6_DONTFRAG, 1, "IPV6_DONTFRAG");
    tx(s, to6, sizeof mc, 1233, "send");
    pending(s, "frag_size");
    setint(s, IPPROTO_IPV6, IPV6_MTU, 9000, "IPV6_MTU");
    tx(s, to6, sizeof mc, 1453, "send");
    pending(s, "frag_size 9000");
    setint(s, IPPROTO_IPV6, IPV6_MTU, 0, "IPV6_MTU");
    close(s);

    printf("-- IPV6_DONTFRAG in sendmsg\n");
    s = socket(AF_INET6, SOCK_DGRAM, 0);
    send_dontfrag(s, &mc, 1453, 1, sizeof(int));
    send_dontfrag(s, &mc, 1453, 0, sizeof(int));
    send_dontfrag(s, &mc, 1453, 1, 2);
    send_dontfrag(s, &mc, 1452, 1, sizeof(int));
    close(s);

    printf("-- ::1 (lo, mtu 65536)\n");
    s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 lo = {.sin6_family = AF_INET6, .sin6_port = htons(9), .sin6_addr = IN6ADDR_LOOPBACK_INIT};
    setint(s, IPPROTO_IPV6, IPV6_DONTFRAG, 1, "IPV6_DONTFRAG");
    tx(s, (struct sockaddr *)&lo, sizeof lo, 3000, "send");
    tx(s, (struct sockaddr *)&lo, sizeof lo, 65527, "send");
    close(s);

    printf("== IPv4 (10.0.2.255 broadcast, mtu 1500)\n");
    int s4 = socket(AF_INET, SOCK_DGRAM, 0);
    int one = 1;
    setsockopt(s4, SOL_SOCKET, SO_BROADCAST, &one, sizeof one);
    struct sockaddr_in bc = {.sin_family = AF_INET, .sin_port = htons(9)};
    inet_pton(AF_INET, "10.0.2.255", &bc.sin_addr);
    const struct sockaddr *to4 = (struct sockaddr *)&bc;
    getint(s4, IPPROTO_IP, IP_MTU_DISCOVER, "IP_MTU_DISCOVER");
    tx(s4, to4, sizeof bc, 100, "warm");
    tx(s4, to4, sizeof bc, 1472, "send");
    tx(s4, to4, sizeof bc, 1473, "send");
    tx(s4, to4, sizeof bc, 4000, "send");
    tx(s4, to4, sizeof bc, 65492, "send");
    tx(s4, to4, sizeof bc, 65493, "send");
    tx(s4, to4, sizeof bc, 65508, "send");
    setint(s4, IPPROTO_IP, IP_RECVERR, 1, "IP_RECVERR");
    for (int v = 0; v <= 5; v++) {
        setint(s4, IPPROTO_IP, IP_MTU_DISCOVER, v, "IP_MTU_DISCOVER");
        tx(s4, to4, sizeof bc, 1473, "send");
        pending(s4, "ip pmtudisc");
    }
    close(s4);

    printf("== IPv6 path MTU 1280 (2001:db8:6::1 via the router)\n");
    struct sockaddr_in6 far = {.sin6_family = AF_INET6, .sin6_port = htons(9)};
    inet_pton(AF_INET6, "2001:db8:6::1", &far.sin6_addr);
    s = socket(AF_INET6, SOCK_DGRAM, 0);
    printf("connect: %s\n", connect(s, (struct sockaddr *)&far, sizeof far) ? en(errno) : "ok");
    pathmtu(s, "connected");
    struct sockaddr_in6 sn;
    socklen_t sl = sizeof sn;
    getsockname(s, (struct sockaddr *)&sn, &sl);
    ptb(&me, &far.sin6_addr, ntohs(sn.sin6_port), 1280);
    close(s);
    s = socket(AF_INET6, SOCK_DGRAM, 0);
    connect(s, (struct sockaddr *)&far, sizeof far);
    pathmtu(s, "after ptb");
    getint(s, IPPROTO_IPV6, IPV6_MTU, "IPV6_MTU");
    setint(s, IPPROTO_IPV6, IPV6_DONTFRAG, 1, "IPV6_DONTFRAG");
    setint(s, IPPROTO_IPV6, IPV6_RECVPATHMTU, 1, "IPV6_RECVPATHMTU");
    long r = send(s, big, 1232, 0);
    printf("  send 1232: %s\n", r < 0 ? en(errno) : "ok");
    r = send(s, big, 1233, 0);
    printf("  send 1233: %s\n", r < 0 ? en(errno) : "ok");
    pending(s, "pmtu 1280");
    setint(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER, 3, "IPV6_MTU_DISCOVER");
    r = send(s, big, 1452, 0);
    printf("  send 1452 (probe: device mtu): %s\n", r < 0 ? en(errno) : "ok");
    r = send(s, big, 1453, 0);
    printf("  send 1453 (probe: device mtu): %s\n", r < 0 ? en(errno) : "ok");
    pending(s, "probe");
    close(s);

    return 0;
}
