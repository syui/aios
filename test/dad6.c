// IPv6 の DAD (重複アドレス検出: Linux の addrconf_dad_start / addrconf_dad_work / addrconf_dad_completed)。
// ip -6 addr add で足した番地は、まず tentative (IFA_F_TENTATIVE) で、近所さがしの NS を 1 つ (送り元 ::、あて先は
// solicited-node マルチキャスト) 送り、1 秒 (RetransTimer) 答えがなければ tentative をはずして RTM_NEWADDR で知らせる
// (はじめの NS の前に 0〜1 秒 (rtr_solicit_delay) 待つので、足してから 1〜2 秒)。tentative のあいだは:
//   - bind できない (EADDRNOTAVAIL)、送り元にえらばれない
// nodad (IFA_F_NODAD) はすぐ使え、知らせは 1 つ。tentative のまま消せば、その RTM_DELADDR だけ。
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net dad6)。知らせは fd00: の
// ものだけを出す (QEMU の RA はいつ来るかわからない)
//   aios の中で: sudo ./dad6      aarch64-linux-gnu-gcc -O2 -static -o dad6 test/dad6.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
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

// ms ミリ秒のあいだに来た知らせ (fd00: のもの) をみな
static void events_for(int ms) {
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    int any = 0;
    for (;;) {
        struct timespec t;
        clock_gettime(CLOCK_MONOTONIC, &t);
        long left = ms - ((t.tv_sec - t0.tv_sec) * 1000 + (t.tv_nsec - t0.tv_nsec) / 1000000);
        if (left <= 0)
            break;
        struct pollfd p = {ev, POLLIN, 0};
        if (poll(&p, 1, (int)left) <= 0)
            continue;
        long got = recv(ev, buf, sizeof buf, MSG_DONTWAIT);
        if (got <= 0)
            continue;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            char line[512];
            int show = 0;
            if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR)
                show = addr_line(h, line, "fd00:");
            else if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_DELROUTE)
                show = route_line(h, line, "fd00:");
            if (show) {
                printf("  event %s\n", line);
                any = 1;
            }
        }
    }
    if (!any)
        printf("  (no events)\n");
}

// その番地に bind できるか
static void try_bind(const char *addr) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 a = {.sin6_family = AF_INET6};
    inet_pton(AF_INET6, addr, &a.sin6_addr);
    int r = bind(s, (struct sockaddr *)&a, sizeof a);
    printf("bind %s: %s\n", addr, r ? en(errno) : "ok");
    close(s);
}

// UDP の connect でえらばれる送り元が want か
static void source_is(const char *dst, const char *want) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 d = {.sin6_family = AF_INET6, .sin6_port = htons(53)};
    inet_pton(AF_INET6, dst, &d.sin6_addr);
    int r = connect(s, (struct sockaddr *)&d, sizeof d);
    struct sockaddr_in6 me;
    socklen_t ml = sizeof me;
    getsockname(s, (struct sockaddr *)&me, &ml);
    char ip[INET6_ADDRSTRLEN];
    inet_ntop(AF_INET6, &me.sin6_addr, ip, sizeof ip);
    printf("source for %s is %s: %s\n", dst, want, r ? en(errno) : strcmp(ip, want) ? "no" : "yes");
    close(s);
}

// /proc/net/if_inet6 のその番地の flags (tentative 0x40、permanent 0x80、nodad 0x02、dadfailed 0x08)
static void proc_flags(const char *hex) {
    FILE *f = fopen("/proc/net/if_inet6", "r");
    char line[256];
    int found = 0;
    while (f && fgets(line, sizeof line, f)) {
        unsigned idx, plen, scope, flags;
        char addr[40], name[32];
        if (sscanf(line, "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) == 6 && !strcmp(addr, hex)) {
            printf("if_inet6 %s: plen %u scope %#x flags %#x\n", hex, plen, scope, flags);
            found = 1;
        }
    }
    if (f)
        fclose(f);
    if (!found)
        printf("if_inet6 %s: (none)\n", hex);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    unsigned eth0 = if_nametoindex("eth0");
    printf("eth0 slaac: %s\n", wait_slaac() ? "yes" : "no");
    ev = nlsock(RTMGRP_IPV6_IFADDR | RTMGRP_IPV6_ROUTE);

    printf("-- add fd00::10/64 (DAD)\n");
    addr_change("add fd00::10/64", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "fd00::10", 64, 0, 0, 0);
    dump(RTM_GETADDR, "fd00:");
    proc_flags("fd000000000000000000000000000010");
    try_bind("fd00::10");
    source_is("fd00::99", "fd00::10");
    printf("-- 3 seconds later\n");
    events_for(3000);
    dump(RTM_GETADDR, "fd00:");
    proc_flags("fd000000000000000000000000000010");
    try_bind("fd00::10");
    source_is("fd00::99", "fd00::10");

    printf("-- add fd00::11/64 nodad\n");
    addr_change("add fd00::11/64 nodad", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "fd00::11", 64, IFA_F_NODAD, 0, 0);
    proc_flags("fd000000000000000000000000000011");
    try_bind("fd00::11");
    events_for(2500);

    printf("-- add fd00::12/64 and delete it while tentative\n");
    addr_change("add fd00::12/64", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "fd00::12", 64, 0, 0, 0);
    addr_change("del fd00::12/64", RTM_DELADDR, 0, eth0, "fd00::12", 64, 0, 0, 0);
    events_for(2500);
    proc_flags("fd000000000000000000000000000012");

    printf("-- clean up\n");
    addr_change("del fd00::10/64", RTM_DELADDR, 0, eth0, "fd00::10", 64, 0, 0, 0);
    addr_change("del fd00::11/64", RTM_DELADDR, 0, eth0, "fd00::11", 64, 0, 0, 0);
    events_for(500);
    dump(RTM_GETADDR, "fd00:");
    return 0;
}
