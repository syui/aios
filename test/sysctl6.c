// /proc/sys/net/ipv6/conf/{all,default,lo,eth0}/ の値 (Linux の addrconf_sysctl): 読む値と、書けるものの範囲
// (hop_limit は 1〜255、mtu は 1280〜口の MTU、dad_transmits は 0 以上) と、書いた値が効くか: eth0 の dad_transmits か
// accept_dad (all と eth0 のどちらかが 1 以上なら DAD をする) を 0 にすると、足した番地はすぐ使える (tentative に
// ならない)。
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net sysctl6)
//   aios の中で: sudo ./sysctl6      aarch64-linux-gnu-gcc -O2 -static -o sysctl6 test/sysctl6.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static char buf[65536];
static unsigned seqno = 700;

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case EEXIST: return "EEXIST";
    case ENODEV: return "ENODEV";
    case ESRCH: return "ESRCH";
    case ENOENT: return "ENOENT";
    case EADDRNOTAVAIL: return "EADDRNOTAVAIL";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ENETUNREACH: return "ENETUNREACH";
    case EHOSTUNREACH: return "EHOSTUNREACH";
    case EPERM: return "EPERM";
    case EAFNOSUPPORT: return "EAFNOSUPPORT";
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

static int wait_slaac(void) {
    for (int i = 0; i < 300; i++) {
        FILE *f = fopen("/proc/net/if_inet6", "r");
        char line[256];
        int ok = 0;
        while (f && fgets(line, sizeof line, f)) {
            unsigned idx, plen, scope, flags;
            char addr[40], name[32];
            if (sscanf(line, "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) == 6 && !strcmp(name, "eth0") && scope != 0x20 &&
                !(flags & 0x40))
                ok = 1;
        }
        if (f)
            fclose(f);
        if (ok)
            return 1;
        ms_sleep(100);
    }
    return 0;
}

static int nlsock(unsigned groups) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = groups};
    bind(s, (struct sockaddr *)&a, sizeof a);
    return s;
}

static void put_attr(char *msg, int type, const void *data, int len) {
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    struct rtattr *a = (struct rtattr *)(msg + NLMSG_ALIGN(h->nlmsg_len));
    a->rta_type = type;
    a->rta_len = RTA_LENGTH(len);
    memcpy(RTA_DATA(a), data, len);
    h->nlmsg_len = NLMSG_ALIGN(h->nlmsg_len) + RTA_ALIGN(a->rta_len);
}

static const char *life(unsigned v) {
    return v == 0xffffffff ? "forever" : v ? "finite" : "0";
}

// RTM_NEWADDR / RTM_DELADDR (AF_INET6) を 1 行に。want の prefix のものでなければ 0
static int addr_line(struct nlmsghdr *h, char *line, const char *want) {
    struct ifaddrmsg *a = NLMSG_DATA(h);
    if (a->ifa_family != AF_INET6)
        return 0;
    char ip[INET6_ADDRSTRLEN] = "";
    int n = snprintf(line, 512, "%s flags=%#x if%u prefixlen=%u ifa_flags=%#x scope=%u:", h->nlmsg_type == RTM_NEWADDR ? "NEWADDR" : "DELADDR",
                     h->nlmsg_flags, a->ifa_index, a->ifa_prefixlen, a->ifa_flags, a->ifa_scope);
    int len = IFA_PAYLOAD(h);
    for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
        unsigned char *d = RTA_DATA(r);
        switch (r->rta_type) {
        case IFA_ADDRESS: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " address %s", ip); break;
        case IFA_LOCAL: {
            char l[INET6_ADDRSTRLEN];
            inet_ntop(AF_INET6, d, l, sizeof l);
            n += snprintf(line + n, 512 - n, " local %s", l);
            break;
        }
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
    return want == NULL || strncmp(ip, want, strlen(want)) == 0;
}

// RTM_NEWROUTE / RTM_DELROUTE (AF_INET6) を 1 行に。dst が want で始まらなければ 0
static int route_line(struct nlmsghdr *h, char *line, const char *want) {
    struct rtmsg *r = NLMSG_DATA(h);
    if (r->rtm_family != AF_INET6)
        return 0;
    char dst[INET6_ADDRSTRLEN] = "::";
    int n = snprintf(line, 512, "%s flags=%#x table=%u type=%u scope=%u proto=%u dst_len=%u rtm_flags=%#x:",
                     h->nlmsg_type == RTM_NEWROUTE ? "NEWROUTE" : "DELROUTE", h->nlmsg_flags, r->rtm_table, r->rtm_type, r->rtm_scope,
                     r->rtm_protocol, r->rtm_dst_len, r->rtm_flags);
    int len = RTM_PAYLOAD(h);
    for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        unsigned char *d = RTA_DATA(a);
        char ip[INET6_ADDRSTRLEN];
        unsigned v = RTA_PAYLOAD(a) >= 4 ? *(unsigned *)d : 0;
        switch (a->rta_type) {
        case RTA_TABLE: n += snprintf(line + n, 512 - n, " table %u", v); break;
        case RTA_DST: inet_ntop(AF_INET6, d, dst, sizeof dst); n += snprintf(line + n, 512 - n, " dst %s", dst); break;
        case RTA_PREFSRC: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " prefsrc %s", ip); break;
        case RTA_GATEWAY: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, 512 - n, " via %s", ip); break;
        case RTA_OIF: n += snprintf(line + n, 512 - n, " oif %u", v); break;
        case RTA_PRIORITY: n += snprintf(line + n, 512 - n, " metric %u", v); break;
        case RTA_PREF: n += snprintf(line + n, 512 - n, " pref %u", d[0]); break;
        case RTA_METRICS: n += snprintf(line + n, 512 - n, " metrics(%u)", (unsigned)RTA_PAYLOAD(a)); break;
        case RTA_CACHEINFO: {
            struct rta_cacheinfo *c = (void *)d;
            n += snprintf(line + n, 512 - n, " cache(error %u expires %s)", c->rta_error, c->rta_expires ? "finite" : "0");
            break;
        }
        default: n += snprintf(line + n, 512 - n, " attr%u(%u)", a->rta_type, (unsigned)RTA_PAYLOAD(a)); break;
        }
    }
    return want == NULL || strncmp(dst, want, strlen(want)) == 0;
}

// 知らせのうち、fd00: か 2001:db8: のもの
static int ev;
static void events(void) {
    return;
    long got;
    int any = 0;
    ms_sleep(100);
    while ((got = recv(ev, buf, sizeof buf, MSG_DONTWAIT)) > 0)
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            char line[512];
            int show = 0;
            if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR)
                show = addr_line(h, line, "fd00:") || addr_line(h, line, "2001:db8:");
            else if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_DELROUTE)
                show = route_line(h, line, "fd00:") || route_line(h, line, "2001:db8:");
            if (show) {
                printf("  event %s\n", line);
                any = 1;
            }
        }
    if (!any)
        printf("  (no events)\n");
}

// 答え (ACK か NLMSG_ERROR) を 1 つ
static void ack(int s, const char *what) {
    struct pollfd p = {s, POLLIN, 0};
    long got = poll(&p, 1, 1000) > 0 ? recv(s, buf, sizeof buf, 0) : -1;
    struct nlmsghdr *h = (struct nlmsghdr *)buf;
    if (got <= 0)
        printf("%s: no answer\n", what);
    else if (h->nlmsg_type == NLMSG_ERROR)
        printf("%s: %s\n", what, en(-((struct nlmsgerr *)NLMSG_DATA(h))->error));
    else
        printf("%s: type %u\n", what, h->nlmsg_type);
}

static void send_msg(int s, char *msg) {
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, msg, ((struct nlmsghdr *)msg)->nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
}

// ip -6 addr add / replace / del
static void addr_change(const char *what, int type, int flags, unsigned ifindex, const char *addr, int plen, unsigned ifa_flags, unsigned pref, unsigned valid) {
    int s = nlsock(0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct ifaddrmsg));
    h->nlmsg_type = type;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    h->nlmsg_seq = ++seqno;
    struct ifaddrmsg *a = NLMSG_DATA(h);
    a->ifa_family = AF_INET6;
    a->ifa_prefixlen = plen;
    a->ifa_index = ifindex;
    unsigned char ip[16];
    inet_pton(AF_INET6, addr, ip);
    put_attr(msg, IFA_LOCAL, ip, 16);
    if (ifa_flags)
        put_attr(msg, IFA_FLAGS, &ifa_flags, 4);
    if (pref || valid) {
        struct ifa_cacheinfo c = {.ifa_prefered = pref, .ifa_valid = valid};
        put_attr(msg, IFA_CACHEINFO, &c, sizeof c);
    }
    send_msg(s, msg);
    ack(s, what);
    close(s);
    events();
}

// dump して want で始まるものだけ (並べて)
static void dump(int type, const char *want) {
    int s = nlsock(0);
    struct {
        struct nlmsghdr h;
        struct rtmsg r;
    } req = {{NLMSG_LENGTH(sizeof(struct rtmsg)), type, NLM_F_REQUEST | NLM_F_DUMP, ++seqno, 0}, {.rtm_family = AF_INET6}};
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &req, req.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
    char lines[32][512];
    int nl = 0;
    for (int dg = 0; dg < 32; dg++) {
        struct pollfd p = {s, POLLIN, 0};
        if (poll(&p, 1, 1000) <= 0)
            break;
        long got = recv(s, buf, sizeof buf, 0);
        int end = 0;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR) {
                end = 1;
                continue;
            }
            if (nl < 32 && ((type == RTM_GETADDR && addr_line(h, lines[nl], want)) || (type == RTM_GETROUTE && route_line(h, lines[nl], want))))
                nl++;
        }
        if (end)
            break;
    }
    close(s);
    for (int i = 0; i < nl; i++)
        for (int j = i + 1; j < nl; j++)
            if (strcmp(lines[i], lines[j]) > 0) {
                char t[512];
                strcpy(t, lines[i]);
                strcpy(lines[i], lines[j]);
                strcpy(lines[j], t);
            }
    for (int i = 0; i < nl; i++)
        printf("  %s\n", lines[i]);
    if (!nl)
        printf("  (none)\n");
}

static const char *const devs[] = {"all", "default", "lo", "eth0"};
static const char *const keys[] = {"accept_dad", "accept_ra", "addr_gen_mode", "autoconf", "dad_transmits", "disable_ipv6", "forwarding", "hop_limit", "mtu", "use_tempaddr"};

static void show(const char *dev, const char *key) {
    char p[128], v[64] = "";
    snprintf(p, sizeof p, "/proc/sys/net/ipv6/conf/%s/%s", dev, key);
    FILE *f = fopen(p, "r");
    if (!f) {
        printf("%s/%s: %s\n", dev, key, en(errno));
        return;
    }
    if (!fgets(v, sizeof v, f))
        v[0] = 0;
    fclose(f);
    v[strcspn(v, "\n")] = 0;
    printf("%s/%s = %s\n", dev, key, v);
}

static void set(const char *dev, const char *key, const char *val) {
    char p[128];
    snprintf(p, sizeof p, "/proc/sys/net/ipv6/conf/%s/%s", dev, key);
    int fd = open(p, O_WRONLY);
    if (fd < 0) {
        printf("write %s/%s %s: open %s\n", dev, key, val, en(errno));
        return;
    }
    long r = write(fd, val, strlen(val));
    printf("write %s/%s %s: %s\n", dev, key, val, r < 0 ? en(errno) : "ok");
    close(fd);
}

static void try_bind(const char *addr) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 a = {.sin6_family = AF_INET6};
    inet_pton(AF_INET6, addr, &a.sin6_addr);
    int r = bind(s, (struct sockaddr *)&a, sizeof a);
    printf("bind %s: %s\n", addr, r ? en(errno) : "ok");
    close(s);
}

// 足してすぐの flags (tentative 0x40) と bind、消す
static void add_now(const char *addr) {
    addr_change("add", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, if_nametoindex("eth0"), addr, 64, 0, 0, 0);
    dump(RTM_GETADDR, addr);
    try_bind(addr);
    addr_change("del", RTM_DELADDR, 0, if_nametoindex("eth0"), addr, 64, 0, 0, 0);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("eth0 slaac: %s\n", wait_slaac() ? "yes" : "no");
    ev = nlsock(0);
    printf("-- values\n");
    for (unsigned d = 0; d < sizeof devs / sizeof *devs; d++)
        for (unsigned k = 0; k < sizeof keys / sizeof *keys; k++)
            show(devs[d], keys[k]);
    printf("-- ranges\n");
    set("eth0", "hop_limit", "0");
    set("eth0", "hop_limit", "256");
    set("eth0", "hop_limit", "33");
    show("eth0", "hop_limit");
    set("eth0", "hop_limit", "64");
    set("eth0", "mtu", "1279");
    set("eth0", "mtu", "1501");
    set("eth0", "mtu", "1400");
    show("eth0", "mtu");
    set("eth0", "mtu", "1500");
    set("lo", "mtu", "70000");
    set("default", "mtu", "9000");
    show("default", "mtu");
    set("default", "mtu", "1280");
    set("eth0", "dad_transmits", "-1");
    set("eth0", "dad_transmits", "abc");
    set("eth0", "use_tempaddr", "2");
    show("eth0", "use_tempaddr");
    set("eth0", "use_tempaddr", "0");
    printf("-- DAD on\n");
    add_now("fd00::40");
    printf("-- eth0 dad_transmits 0\n");
    set("eth0", "dad_transmits", "0");
    add_now("fd00::41");
    set("eth0", "dad_transmits", "1");
    printf("-- eth0 accept_dad 0\n");
    set("eth0", "accept_dad", "0");
    add_now("fd00::42");
    printf("-- eth0 accept_dad 0, all accept_dad 1\n");
    set("all", "accept_dad", "1");
    add_now("fd00::43");
    set("all", "accept_dad", "0");
    set("eth0", "accept_dad", "1");
    return 0;
}
