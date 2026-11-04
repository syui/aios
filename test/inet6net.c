// eth0 の IPv6 (IPv6 の段階 2c): QEMU のユーザーネットワーク (fec0::/64 を RA で配る。ルーターは fe80::2、ホストは
// fec0::2) での SLAAC のアドレス (link-local と global)、/proc/net/if_inet6、netlink の番地と道と近所、送り元の
// えらびかた (UDP の connect と getsockname)、外への TCP と UDP と ICMPv6 (ping)。
// 出すだけのテスト: 手本の Linux は ip=dhcp の virtio-net で、MAC は aios と同じ 52:54:00:12:34:56 (QEMU の既定) なので
// アドレスも同じになる。のこり時間 (lifetime、expires) は変わるので「限りあり」とだけ出す。root で。eth0 に IPv6 の
// アドレス (SLAAC) ができなければ終わりの番号 1 (test/vm-ctests.sh)
//   test/ldiff.sh --root --net inet6net
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <linux/neighbour.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/icmp6.h>
#include <netinet/in.h>
#include <netinet/ip6.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case ENETUNREACH: return "ENETUNREACH";
    case EHOSTUNREACH: return "EHOSTUNREACH";
    case ECONNREFUSED: return "ECONNREFUSED";
    case ETIMEDOUT: return "ETIMEDOUT";
    case EADDRNOTAVAIL: return "EADDRNOTAVAIL";
    case EACCES: return "EACCES";
    case EPERM: return "EPERM";
    case EAGAIN: return "EAGAIN";
    case EINPROGRESS: return "EINPROGRESS";
    case ENODEV: return "ENODEV";
    case ENOENT: return "ENOENT";
    case EAFNOSUPPORT: return "EAFNOSUPPORT";
    case EPROTONOSUPPORT: return "EPROTONOSUPPORT";
    case ESOCKTNOSUPPORT: return "ESOCKTNOSUPPORT";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "errno%d", e);
        return b;
    }
    }
}

static struct sockaddr_in6 a6(const char *s, int port, unsigned scope) {
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port), .sin6_scope_id = scope};
    inet_pton(AF_INET6, s, &a.sin6_addr);
    return a;
}

static const char *show6(const struct sockaddr_in6 *a) {
    static char b[128];
    char ip[INET6_ADDRSTRLEN];
    inet_ntop(AF_INET6, &a->sin6_addr, ip, sizeof ip);
    snprintf(b, sizeof b, "[%s]:%s scope %u", ip, ntohs(a->sin6_port) ? "port" : "0", a->sin6_scope_id);
    return b;
}

static void ms_sleep(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

static int sorted_lines(const char *path, char lines[][256], int max) {
    FILE *f = fopen(path, "r");
    int n = 0;
    while (f && n < max && fgets(lines[n], 256, f))
        n++;
    if (f)
        fclose(f);
    for (int i = 0; i < n; i++)
        for (int j = i + 1; j < n; j++)
            if (strcmp(lines[i], lines[j]) > 0) {
                char t[256];
                strcpy(t, lines[i]);
                strcpy(lines[i], lines[j]);
                strcpy(lines[j], t);
            }
    return n;
}

// eth0 に link-local でないアドレス (fec0::/64 は site-local: if_inet6 の scope 40) ができて、tentative (DAD の
// 途中) でなくなるのを待つ (30 秒まで)
static int wait_global(void) {
    for (int i = 0; i < 300; i++) {
        char lines[16][256];
        int n = sorted_lines("/proc/net/if_inet6", lines, 16);
        for (int k = 0; k < n; k++) {
            unsigned idx, plen, scope, flags;
            char addr[40], name[32];
            if (sscanf(lines[k], "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) == 6 && strcmp(name, "eth0") == 0 &&
                scope != 0x20 && !(flags & 0x40))
                return 1;
        }
        ms_sleep(100);
    }
    return 0;
}

static void if_inet6(void) {
    char lines[16][256];
    int n = sorted_lines("/proc/net/if_inet6", lines, 16);
    for (int k = 0; k < n; k++)
        printf("if_inet6: %s", lines[k]);
}

static int nl(void) {
    return socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
}

static const char *life(unsigned v) {
    return v == 0xffffffff ? "forever" : v ? "finite" : "0";
}

// netlink の dump: 中身を 1 つずつ cb に (データグラムの数は出さない: IPv4 のものもまじる)
static void dump(int type, int family, void (*cb)(struct nlmsghdr *)) {
    int s = nl();
    struct {
        struct nlmsghdr h;
        struct rtmsg r;
    } req = {{NLMSG_LENGTH(sizeof(struct rtmsg)), type, NLM_F_REQUEST | NLM_F_DUMP, 9, 0}, {.rtm_family = family}};
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &req, req.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
    static char buf[65536];
    for (int dg = 0; dg < 32; dg++) {
        struct pollfd p = {s, POLLIN, 0};
        if (poll(&p, 1, 1000) <= 0)
            break;
        long got = recv(s, buf, sizeof buf, 0);
        if (got <= 0)
            break;
        int end = 0;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR) {
                end = 1;
                continue;
            }
            cb(h);
        }
        if (end)
            break;
    }
    close(s);
}

static char out[64][512];
static int nout;

static void flush_sorted(void) {
    for (int i = 0; i < nout; i++)
        for (int j = i + 1; j < nout; j++)
            if (strcmp(out[i], out[j]) > 0) {
                char t[512];
                strcpy(t, out[i]);
                strcpy(out[i], out[j]);
                strcpy(out[j], t);
            }
    for (int i = 0; i < nout; i++)
        printf("%s\n", out[i]);
    nout = 0;
}

static void addr_cb(struct nlmsghdr *h) {
    struct ifaddrmsg *a = NLMSG_DATA(h);
    if (h->nlmsg_type != RTM_NEWADDR || a->ifa_family != AF_INET6 || nout >= 64)
        return;
    char *line = out[nout++];
    int n = snprintf(line, 512, "addr if%u prefixlen=%u ifa_flags=%#x scope=%u:", a->ifa_index, a->ifa_prefixlen, a->ifa_flags, a->ifa_scope);
    int len = IFA_PAYLOAD(h);
    for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
        unsigned char *d = RTA_DATA(r);
        char ip[INET6_ADDRSTRLEN];
        switch (r->rta_type) {
        case IFA_ADDRESS: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " address %s", ip); break;
        case IFA_LOCAL: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " local %s", ip); break;
        case IFA_CACHEINFO: {
            struct ifa_cacheinfo *c = (void *)d;
            n += snprintf(line + n, 512 - n, " cacheinfo(prefered %s valid %s)", life(c->ifa_prefered), life(c->ifa_valid));
            break;
        }
        case IFA_FLAGS: n += snprintf(line + n, 512 - n, " flags %#x", *(unsigned *)d); break;
        case 11: n += snprintf(line + n, 512 - n, " proto %u", d[0]); break;
        default: n += snprintf(line + n, 512 - n, " attr%u(%u)", r->rta_type, (unsigned)RTA_PAYLOAD(r)); break;
        }
    }
}

static void route_line(struct nlmsghdr *h, char *line) {
    struct rtmsg *r = NLMSG_DATA(h);
    int n = snprintf(line, 512, "route6 table=%u type=%u scope=%u proto=%u dst_len=%u flags=%#x:", r->rtm_table, r->rtm_type, r->rtm_scope,
                     r->rtm_protocol, r->rtm_dst_len, r->rtm_flags);
    int len = RTM_PAYLOAD(h);
    for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        unsigned char *d = RTA_DATA(a);
        char ip[INET6_ADDRSTRLEN];
        unsigned v = RTA_PAYLOAD(a) >= 4 ? *(unsigned *)d : 0;
        switch (a->rta_type) {
        case RTA_TABLE: n += snprintf(line + n, 512 - n, " table %u", v); break;
        case RTA_DST: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " dst %s", ip); break;
        case RTA_SRC: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " src %s", ip); break;
        case RTA_PREFSRC: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " prefsrc %s", ip); break;
        case RTA_GATEWAY: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " via %s", ip); break;
        case RTA_OIF: n += snprintf(line + n, 512 - n, " oif %u", v); break;
        case RTA_IIF: n += snprintf(line + n, 512 - n, " iif %u", v); break;
        case RTA_PRIORITY: n += snprintf(line + n, 512 - n, " metric %u", v); break;
        case RTA_PREF: n += snprintf(line + n, 512 - n, " pref %u", d[0]); break;
        case RTA_UID: n += snprintf(line + n, 512 - n, " uid %s", v == getuid() ? "self" : "other"); break;
        case RTA_METRICS: n += snprintf(line + n, 512 - n, " metrics(%u)", (unsigned)RTA_PAYLOAD(a)); break;
        case RTA_EXPIRES: n += snprintf(line + n, 512 - n, " expires(%u)", (unsigned)RTA_PAYLOAD(a)); break;
        case RTA_CACHEINFO: {
            struct rta_cacheinfo *c = (void *)d;
            n += snprintf(line + n, 512 - n, " cache(error %u expires %s)", c->rta_error, c->rta_expires ? "finite" : "0");
            break;
        }
        default: n += snprintf(line + n, 512 - n, " attr%u(%u)", a->rta_type, (unsigned)RTA_PAYLOAD(a)); break;
        }
    }
}

static void route_cb(struct nlmsghdr *h) {
    if (h->nlmsg_type != RTM_NEWROUTE || ((struct rtmsg *)NLMSG_DATA(h))->rtm_family != AF_INET6 || nout >= 64)
        return;
    route_line(h, out[nout++]);
}

static const char *nud(unsigned s) {
    switch (s) {
    case NUD_INCOMPLETE: return "INCOMPLETE";
    case NUD_REACHABLE: return "REACHABLE";
    case NUD_STALE: return "STALE";
    case NUD_DELAY: return "DELAY";
    case NUD_PROBE: return "PROBE";
    case NUD_FAILED: return "FAILED";
    case NUD_NOARP: return "NOARP";
    case NUD_PERMANENT: return "PERMANENT";
    default: return "?";
    }
}

static void neigh_cb(struct nlmsghdr *h) {
    struct ndmsg *m = NLMSG_DATA(h);
    // 本物の近所 (unicast) だけ。マルチキャストの NOARP のもの (MLD や DAD で送ったあて先) と自分の番地 (local) は出さない
    if (h->nlmsg_type != RTM_NEWNEIGH || m->ndm_family != AF_INET6 || m->ndm_type != RTN_UNICAST || nout >= 64)
        return;
    char *line = out[nout++];
    int n = snprintf(line, 512, "neigh6 if%u flags=%#x type=%u:", m->ndm_ifindex, m->ndm_flags, m->ndm_type);
    // 状態は時間で変わる (REACHABLE → STALE …): 「答えを見た」ものかだけ
    n += snprintf(line + n, 512 - n, " %s", m->ndm_state & (NUD_REACHABLE | NUD_STALE | NUD_DELAY | NUD_PROBE) ? "valid" : nud(m->ndm_state));
    int len = RTM_PAYLOAD(h);
    for (struct rtattr *a = (struct rtattr *)((char *)m + NLMSG_ALIGN(sizeof *m)); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        unsigned char *d = RTA_DATA(a);
        char ip[INET6_ADDRSTRLEN];
        switch (a->rta_type) {
        case NDA_DST: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " dst %s", ip); break;
        case NDA_LLADDR: n += snprintf(line + n, 512 - n, " lladdr %02x:%02x:%02x:%02x:%02x:%02x", d[0], d[1], d[2], d[3], d[4], d[5]); break;
        case NDA_CACHEINFO: n += snprintf(line + n, 512 - n, " cacheinfo"); break;
        case NDA_PROBES: n += snprintf(line + n, 512 - n, " probes"); break;
        default: n += snprintf(line + n, 512 - n, " attr%u(%u)", a->rta_type, (unsigned)RTA_PAYLOAD(a)); break;
        }
    }
}

// RTM_GETROUTE の 1 つ (ip -6 route get DST [oif])
static void route_get(const char *dst, unsigned oif) {
    int s = nl();
    struct {
        struct nlmsghdr h;
        struct rtmsg r;
        char attrs[64];
    } req;
    memset(&req, 0, sizeof req);
    req.h.nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    req.h.nlmsg_type = RTM_GETROUTE;
    req.h.nlmsg_flags = NLM_F_REQUEST;
    req.h.nlmsg_seq = 5;
    req.r.rtm_family = AF_INET6;
    req.r.rtm_dst_len = 128;
    struct rtattr *a = (struct rtattr *)((char *)&req + NLMSG_ALIGN(req.h.nlmsg_len));
    a->rta_type = RTA_DST;
    a->rta_len = RTA_LENGTH(16);
    inet_pton(AF_INET6, dst, RTA_DATA(a));
    req.h.nlmsg_len = NLMSG_ALIGN(req.h.nlmsg_len) + RTA_ALIGN(a->rta_len);
    if (oif) {
        a = (struct rtattr *)((char *)&req + NLMSG_ALIGN(req.h.nlmsg_len));
        a->rta_type = RTA_OIF;
        a->rta_len = RTA_LENGTH(4);
        memcpy(RTA_DATA(a), &oif, 4);
        req.h.nlmsg_len = NLMSG_ALIGN(req.h.nlmsg_len) + RTA_ALIGN(a->rta_len);
    }
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &req, req.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
    static char buf[8192];
    struct pollfd p = {s, POLLIN, 0};
    long got = poll(&p, 1, 1000) > 0 ? recv(s, buf, sizeof buf, 0) : -1;
    struct nlmsghdr *h = (struct nlmsghdr *)buf;
    char line[512];
    if (got <= 0)
        printf("get %s oif %u: no answer\n", dst, oif);
    else if (h->nlmsg_type == NLMSG_ERROR)
        printf("get %s oif %u: %s\n", dst, oif, en(-((struct nlmsgerr *)NLMSG_DATA(h))->error));
    else {
        route_line(h, line);
        printf("get %s oif %u: %s\n", dst, oif, line);
    }
    close(s);
}

// UDP の connect でえらばれる送り元
static void source(const char *dst, unsigned scope) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 d = a6(dst, 53, scope);
    int r = connect(s, (struct sockaddr *)&d, sizeof d);
    int e = r ? errno : 0;
    struct sockaddr_in6 me;
    socklen_t ml = sizeof me;
    getsockname(s, (struct sockaddr *)&me, &ml);
    printf("udp connect %s scope %u: %s, source %s\n", dst, scope, en(e), r ? "-" : show6(&me));
    close(s);
}

// 外への TCP の connect (fec0::2 はホスト: だれも待っていないポートなら QEMU が RST を返す)
static void tcp_out(const char *dst, int port, unsigned scope) {
    int s = socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in6 d = a6(dst, port, scope);
    int r = connect(s, (struct sockaddr *)&d, sizeof d);
    int e = r ? errno : 0;
    const char *res = en(e);
    if (e == EINPROGRESS) {
        struct pollfd p = {s, POLLOUT, 0};
        if (poll(&p, 1, 5000) <= 0)
            res = "timeout";
        else {
            int err = 0;
            socklen_t l = sizeof err;
            getsockopt(s, SOL_SOCKET, SO_ERROR, &err, &l);
            res = en(err);
        }
    }
    struct sockaddr_in6 me;
    socklen_t ml = sizeof me;
    getsockname(s, (struct sockaddr *)&me, &ml);
    printf("tcp %s port %d: %s, source %s\n", dst, port, res, show6(&me));
    close(s);
}

// 自分の eth0 のアドレスへの TCP と UDP (Linux は lo を通す。link-local は scope がいる)
static void local(unsigned eth0) {
    int l = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 any = a6("::", 0, 0);
    bind(l, (struct sockaddr *)&any, sizeof any);
    listen(l, 8);
    socklen_t al = sizeof any;
    getsockname(l, (struct sockaddr *)&any, &al);
    int port = ntohs(any.sin6_port);
    struct {
        const char *ip;
        unsigned scope;
    } to[] = {{"fec0::5054:ff:fe12:3456", 0}, {"fe80::5054:ff:fe12:3456", eth0}, {"fe80::5054:ff:fe12:3456", 0}, {"fec0::5054:ff:fe12:3456", eth0}};
    for (int i = 0; i < 4; i++) {
        int c = socket(AF_INET6, SOCK_STREAM, 0);
        struct sockaddr_in6 d = a6(to[i].ip, port, to[i].scope);
        int r = connect(c, (struct sockaddr *)&d, sizeof d);
        int e = r ? errno : 0;
        printf("local tcp %s scope %u: %s", to[i].ip, to[i].scope, en(e));
        if (r == 0) {
            struct sockaddr_in6 me, peer;
            socklen_t ml = sizeof me, pl = sizeof peer;
            int a = accept(l, (struct sockaddr *)&peer, &pl);
            getsockname(c, (struct sockaddr *)&me, &ml);
            printf(", me %s", show6(&me));
            printf(", accepted peer %s", show6(&peer));
            ml = sizeof me;
            getsockname(a, (struct sockaddr *)&me, &ml);
            printf(", its name %s", show6(&me));
            write(c, "hi", 2);
            char b[4] = "";
            read(a, b, 2);
            printf(", data %s", b);
            close(a);
        }
        printf("\n");
        close(c);
    }
    close(l);
    int u = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 ua = a6("::", 0, 0);
    bind(u, (struct sockaddr *)&ua, sizeof ua);
    al = sizeof ua;
    getsockname(u, (struct sockaddr *)&ua, &al);
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    for (int i = 0; i < 2; i++) {
        struct sockaddr_in6 d = a6(to[i].ip, ntohs(ua.sin6_port), to[i].scope);
        long r = sendto(s, "udp", 3, 0, (struct sockaddr *)&d, sizeof d);
        int e = r < 0 ? errno : 0;
        char b[8] = "";
        struct sockaddr_in6 from;
        socklen_t fl = sizeof from;
        struct pollfd p = {u, POLLIN, 0};
        long got = poll(&p, 1, 1000) > 0 ? recvfrom(u, b, sizeof b - 1, 0, (struct sockaddr *)&from, &fl) : -1;
        printf("local udp %s scope %u: send %s, got %ld from %s\n", to[i].ip, to[i].scope, en(e), got, got > 0 ? show6(&from) : "-");
    }
    close(s);
    close(u);
}

// ICMPv6 の echo (raw ソケット、root)。答えの type と、id と seq が合うか
static void ping6(const char *dst, unsigned scope) {
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if (s < 0) {
        printf("ping %s: socket %s\n", dst, en(errno));
        return;
    }
    struct icmp6_filter f;
    ICMP6_FILTER_SETBLOCKALL(&f);
    ICMP6_FILTER_SETPASS(ICMP6_ECHO_REPLY, &f);
    ICMP6_FILTER_SETPASS(ICMP6_DST_UNREACH, &f);
    setsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, sizeof f);
    struct sockaddr_in6 d = a6(dst, 0, scope);
    unsigned char pkt[16] = {ICMP6_ECHO_REQUEST, 0, 0, 0, 0x12, 0x34, 0, 7, 'a', 'i', 'o', 's', 'p', 'i', 'n', 'g'};
    long r = sendto(s, pkt, sizeof pkt, 0, (struct sockaddr *)&d, sizeof d);
    if (r < 0) {
        printf("ping %s: send %s\n", dst, en(errno));
        close(s);
        return;
    }
    for (;;) {
        struct pollfd p = {s, POLLIN, 0};
        if (poll(&p, 1, 3000) <= 0) {
            printf("ping %s: no reply\n", dst);
            break;
        }
        unsigned char b[256];
        struct sockaddr_in6 from;
        socklen_t fl = sizeof from;
        long got = recvfrom(s, b, sizeof b, 0, (struct sockaddr *)&from, &fl);
        if (got < 8)
            continue;
        char ip[INET6_ADDRSTRLEN];
        inet_ntop(AF_INET6, &from.sin6_addr, ip, sizeof ip);
        printf("ping %s: type %u code %u from %s scope %u len %ld id %s seq %s data %s\n", dst, b[0], b[1], ip, from.sin6_scope_id, got,
               b[4] == 0x12 && b[5] == 0x34 ? "same" : "other", b[7] == 7 ? "same" : "other", got == 16 && memcmp(b + 8, pkt + 8, 8) == 0 ? "same" : "other");
        break;
    }
    close(s);
}

// ping ソケット (SOCK_DGRAM の ICMPv6): ping_group_range の既定 (1 0) ではだれも作れない
static void ping_socket(void) {
    int s = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    printf("ping socket: %s\n", s < 0 ? en(errno) : "ok");
    if (s >= 0)
        close(s);
    char line[64] = "";
    FILE *f = fopen("/proc/sys/net/ipv4/ping_group_range", "r");
    if (f && fgets(line, sizeof line, f))
        printf("ping_group_range: %s", line);
    if (f)
        fclose(f);
}

static void sysctl(const char *p) {
    char path[256], line[128] = "";
    snprintf(path, sizeof path, "/proc/sys/net/ipv6/conf/%s", p);
    FILE *f = fopen(path, "r");
    if (f && fgets(line, sizeof line, f))
        printf("%s: %s", p, line);
    else
        printf("%s: %s\n", p, en(errno));
    if (f)
        fclose(f);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    unsigned eth0 = if_nametoindex("eth0");
    printf("eth0 index %u\n", eth0);
    int global = wait_global();
    printf("global address: %s\n", global ? "yes" : "no");
    printf("-- /proc/net/if_inet6\n");
    if_inet6();
    printf("-- RTM_GETADDR AF_INET6\n");
    dump(RTM_GETADDR, AF_INET6, addr_cb);
    flush_sorted();
    printf("-- getifaddrs\n");
    struct ifaddrs *ia;
    if (getifaddrs(&ia) == 0) {
        for (struct ifaddrs *p = ia; p; p = p->ifa_next)
            if (p->ifa_addr && p->ifa_addr->sa_family == AF_INET6 && nout < 64) {
                char ip[INET6_ADDRSTRLEN], m[INET6_ADDRSTRLEN];
                inet_ntop(AF_INET6, &((struct sockaddr_in6 *)p->ifa_addr)->sin6_addr, ip, sizeof ip);
                inet_ntop(AF_INET6, &((struct sockaddr_in6 *)p->ifa_netmask)->sin6_addr, m, sizeof m);
                snprintf(out[nout++], 512, "getifaddrs: %s inet6 %s mask %s scope_id %u flags %#x", p->ifa_name, ip, m,
                         ((struct sockaddr_in6 *)p->ifa_addr)->sin6_scope_id, p->ifa_flags);
            }
        freeifaddrs(ia);
    }
    flush_sorted();
    printf("-- RTM_GETROUTE AF_INET6\n");
    dump(RTM_GETROUTE, AF_INET6, route_cb);
    flush_sorted();
    printf("-- route get\n");
    route_get("fec0::2", 0);
    route_get("2001:db8::1", 0);
    route_get("fe80::2", 0);
    route_get("fe80::2", eth0);
    route_get("fec0::5054:ff:fe12:3456", 0);
    route_get("fe80::5054:ff:fe12:3456", 0);
    route_get("ff02::1", 0);
    route_get("ff02::1", eth0);
    printf("-- source addresses\n");
    source("fec0::2", 0);
    source("2001:db8::1", 0);
    source("fe80::2", 0);
    source("fe80::2", eth0);
    source("fe80::2", 99);
    source("ff02::1", 0);
    source("ff02::1", eth0);
    printf("-- local\n");
    local(eth0);
    printf("-- tcp\n");
    tcp_out("fec0::2", 1, 0);
    tcp_out("fe80::2", 1, eth0);
    tcp_out("2001:db8::1", 80, 0);
    printf("-- ping\n");
    ping6("fec0::2", 0);
    ping6("fe80::2", eth0);
    ping6("fec0::5054:ff:fe12:3456", 0);
    ping_socket();
    printf("-- neighbours\n");
    dump(RTM_GETNEIGH, AF_INET6, neigh_cb);
    flush_sorted();
    printf("-- sysctl\n");
    sysctl("eth0/disable_ipv6");
    sysctl("eth0/autoconf");
    sysctl("eth0/accept_ra");
    sysctl("eth0/forwarding");
    sysctl("eth0/hop_limit");
    sysctl("eth0/mtu");
    sysctl("eth0/addr_gen_mode");
    sysctl("eth0/use_tempaddr");
    sysctl("all/forwarding");
    sysctl("lo/mtu");
    // 参照の数と使った数 (refcnt、use) は時間で変わるので出さない
    printf("-- /proc/net/ipv6_route\n");
    char lines[32][256];
    int n = sorted_lines("/proc/net/ipv6_route", lines, 32);
    for (int k = 0; k < n; k++) {
        char dst[40], dl[4], src[40], sl[4], nh[40], metric[12], ref[12], use[12], fl[12], dev[32];
        if (sscanf(lines[k], "%39s %3s %39s %3s %39s %11s %11s %11s %11s %31s", dst, dl, src, sl, nh, metric, ref, use, fl, dev) == 10)
            printf("ipv6_route: %s %s %s %s %s %s %s %s (width %zu)\n", dst, dl, src, sl, nh, metric, fl, dev, strlen(lines[k]));
        else
            printf("ipv6_route: ? %s", lines[k]);
    }
    return global ? 0 : 1;
}
