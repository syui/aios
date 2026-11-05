// IPv4 の番地を netlink で変える (ip addr add / replace / del: inet_rtm_newaddr、inet_rtm_deladdr) と、変わったときの
// 知らせ (RTMGRP_IPV4_IFADDR と RTMGRP_IPV4_ROUTE)、primary と secondary (同じ網の 2 つ目は IFA_F_SECONDARY で、
// prefix の道を作らない)、local の道の prefsrc、送り元、しくじりの errno。
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net rtnladdr)。DHCP の
// 10.0.2.15/24 はそのまま (aios の VM は 10.0.2.2 からテストを取ってくる)、10.0.2.50/24 (secondary) と
// 192.168.50.1/24 brd 192.168.50.255 label eth0:1 (2 つ目の primary) を足して消す。知らせと dump はその 2 つと
// 192.168.50.0/24 のものだけ
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static char buf[65536];
static unsigned seqno = 900;
static int ev;

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case EEXIST: return "EEXIST";
    case ENODEV: return "ENODEV";
    case ESRCH: return "ESRCH";
    case EADDRNOTAVAIL: return "EADDRNOTAVAIL";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ENETUNREACH: return "ENETUNREACH";
    case EPERM: return "EPERM";
    case ECONNREFUSED: return "ECONNREFUSED";
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

static const char *ip4(const void *p) {
    static char b[4][INET_ADDRSTRLEN];
    static int k;
    k = (k + 1) % 4;
    inet_ntop(AF_INET, p, b[k], sizeof b[k]);
    return b[k];
}

// 見るもの (10.0.2.50 と 192.168.50.x)
static int wanted(const char *a) {
    return !strcmp(a, "10.0.2.50") || !strncmp(a, "192.168.50.", 11);
}

// RTM_NEWADDR / RTM_DELADDR (AF_INET) を 1 行に。見るものでなければ 0 (all なら eth0 のものはみな)
static int addr_line(struct nlmsghdr *h, char *line, int all) {
    struct ifaddrmsg *a = NLMSG_DATA(h);
    if (a->ifa_family != AF_INET)
        return 0;
    char local[INET_ADDRSTRLEN] = "";
    int n = snprintf(line, 512, "%s flags=%#x if%u prefixlen=%u ifa_flags=%#x scope=%u:", h->nlmsg_type == RTM_NEWADDR ? "NEWADDR" : "DELADDR",
                     h->nlmsg_flags, a->ifa_index, a->ifa_prefixlen, a->ifa_flags, a->ifa_scope);
    int len = IFA_PAYLOAD(h);
    for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
        unsigned char *d = RTA_DATA(r);
        switch (r->rta_type) {
        case IFA_ADDRESS: n += snprintf(line + n, 512 - n, " address %s", ip4(d)); break;
        case IFA_LOCAL:
            strcpy(local, ip4(d));
            n += snprintf(line + n, 512 - n, " local %s", local);
            break;
        case IFA_BROADCAST: n += snprintf(line + n, 512 - n, " brd %s", ip4(d)); break;
        case IFA_LABEL: n += snprintf(line + n, 512 - n, " label %s", (char *)d); break;
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
    return (all && a->ifa_index != 1) || wanted(local);
}

// RTM_NEWROUTE / RTM_DELROUTE (AF_INET) を 1 行に。あて先か prefsrc が見るものでなければ 0
static int route_line(struct nlmsghdr *h, char *line) {
    struct rtmsg *r = NLMSG_DATA(h);
    if (r->rtm_family != AF_INET)
        return 0;
    char dst[INET_ADDRSTRLEN] = "0.0.0.0", src[INET_ADDRSTRLEN] = "";
    int n = snprintf(line, 512, "%s flags=%#x table=%u type=%u scope=%u proto=%u dst_len=%u:", h->nlmsg_type == RTM_NEWROUTE ? "NEWROUTE" : "DELROUTE",
                     h->nlmsg_flags, r->rtm_table, r->rtm_type, r->rtm_scope, r->rtm_protocol, r->rtm_dst_len);
    int len = RTM_PAYLOAD(h);
    for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        unsigned char *d = RTA_DATA(a);
        unsigned v = RTA_PAYLOAD(a) >= 4 ? *(unsigned *)d : 0;
        switch (a->rta_type) {
        case RTA_TABLE: n += snprintf(line + n, 512 - n, " table %u", v); break;
        case RTA_DST:
            strcpy(dst, ip4(d));
            n += snprintf(line + n, 512 - n, " dst %s", dst);
            break;
        case RTA_PREFSRC:
            strcpy(src, ip4(d));
            n += snprintf(line + n, 512 - n, " prefsrc %s", src);
            break;
        case RTA_GATEWAY: n += snprintf(line + n, 512 - n, " via %s", ip4(d)); break;
        case RTA_OIF: n += snprintf(line + n, 512 - n, " oif %u", v); break;
        case RTA_PRIORITY: n += snprintf(line + n, 512 - n, " metric %u", v); break;
        case RTA_UID: n += snprintf(line + n, 512 - n, " uid"); break;
        default: n += snprintf(line + n, 512 - n, " attr%u(%u)", a->rta_type, (unsigned)RTA_PAYLOAD(a)); break;
        }
    }
    return wanted(dst) || wanted(src);
}

static void events(void) {
    long got;
    int any = 0;
    ms_sleep(100);
    while ((got = recv(ev, buf, sizeof buf, MSG_DONTWAIT)) > 0)
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            char line[512];
            int show = 0;
            if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR)
                show = addr_line(h, line, 0);
            else if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_DELROUTE)
                show = route_line(h, line);
            if (show) {
                printf("  event %s\n", line);
                any = 1;
            }
        }
    if (!any)
        printf("  (no events)\n");
}

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

// ip addr add / replace / del (local が NULL なら IFA_LOCAL なし)
static void addr_change(const char *what, int type, int flags, unsigned ifindex, const char *local, int plen, const char *brd, const char *label, unsigned pref,
                        unsigned valid) {
    int s = nlsock(0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct ifaddrmsg));
    h->nlmsg_type = type;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    h->nlmsg_seq = ++seqno;
    struct ifaddrmsg *a = NLMSG_DATA(h);
    a->ifa_family = AF_INET;
    a->ifa_prefixlen = plen;
    a->ifa_index = ifindex;
    if (local) {
        struct in_addr x;
        inet_pton(AF_INET, local, &x);
        put_attr(msg, IFA_LOCAL, &x, 4);
        put_attr(msg, IFA_ADDRESS, &x, 4);
    }
    if (brd) {
        struct in_addr x;
        inet_pton(AF_INET, brd, &x);
        put_attr(msg, IFA_BROADCAST, &x, 4);
    }
    if (label)
        put_attr(msg, IFA_LABEL, label, strlen(label) + 1);
    if (pref || valid) {
        struct ifa_cacheinfo c = {.ifa_prefered = pref, .ifa_valid = valid};
        put_attr(msg, IFA_CACHEINFO, &c, sizeof c);
    }
    send_msg(s, msg);
    ack(s, what);
    close(s);
    events();
}

// dump して見るものだけ (順のまま: addr は eth0 のものみな)
static void dump(int type) {
    int s = nlsock(0);
    struct {
        struct nlmsghdr h;
        struct rtmsg r;
    } req = {{NLMSG_LENGTH(sizeof(struct rtmsg)), type, NLM_F_REQUEST | NLM_F_DUMP, ++seqno, 0}, {.rtm_family = AF_INET}};
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &req, req.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
    int any = 0;
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
            char line[512];
            if ((type == RTM_GETADDR && addr_line(h, line, 1)) || (type == RTM_GETROUTE && route_line(h, line))) {
                printf("  %s\n", line);
                any = 1;
            }
        }
        if (end)
            break;
    }
    if (!any)
        printf("  (none)\n");
    close(s);
}

static void route_get(const char *dst) {
    int s = nlsock(0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET;
    r->rtm_dst_len = 32;
    struct in_addr x;
    inet_pton(AF_INET, dst, &x);
    put_attr(msg, RTA_DST, &x, 4);
    send_msg(s, msg);
    struct pollfd p = {s, POLLIN, 0};
    long got = poll(&p, 1, 1000) > 0 ? recv(s, buf, sizeof buf, 0) : -1;
    struct nlmsghdr *a = (struct nlmsghdr *)buf;
    char line[512];
    if (got <= 0)
        printf("get %s: no answer\n", dst);
    else if (a->nlmsg_type == NLMSG_ERROR)
        printf("get %s: %s\n", dst, en(-((struct nlmsgerr *)NLMSG_DATA(a))->error));
    else {
        route_line(a, line);
        printf("get %s: %s\n", dst, line);
    }
    close(s);
}

// UDP の connect でえらばれる送り元
static void source(const char *dst) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in d = {.sin_family = AF_INET, .sin_port = htons(53)};
    inet_pton(AF_INET, dst, &d.sin_addr);
    int r = connect(s, (struct sockaddr *)&d, sizeof d);
    int e = r ? errno : 0;
    struct sockaddr_in me;
    socklen_t ml = sizeof me;
    getsockname(s, (struct sockaddr *)&me, &ml);
    printf("source for %s: %s\n", dst, r ? en(e) : ip4(&me.sin_addr));
    close(s);
}

// 自分の番地への TCP (受ける側の名前と、相手の名前)
static void local_tcp(const char *to) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET};
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 4);
    socklen_t al = sizeof a;
    getsockname(l, (struct sockaddr *)&a, &al);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    inet_pton(AF_INET, to, &a.sin_addr);
    int r = connect(c, (struct sockaddr *)&a, sizeof a);
    printf("local tcp %s: %s", to, r ? en(errno) : "ok");
    if (r == 0) {
        struct sockaddr_in me, peer;
        socklen_t ml = sizeof me, pl = sizeof peer;
        int k = accept(l, (struct sockaddr *)&peer, &pl);
        getsockname(c, (struct sockaddr *)&me, &ml);
        printf(", me %s, accepted peer %s", ip4(&me.sin_addr), ip4(&peer.sin_addr));
        close(k);
    }
    printf("\n");
    close(c);
    close(l);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    unsigned eth0 = if_nametoindex("eth0");
    ev = nlsock(RTMGRP_IPV4_IFADDR | RTMGRP_IPV4_ROUTE);
    printf("-- add\n");
    addr_change("add 10.0.2.50/24", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "10.0.2.50", 24, NULL, NULL, 0, 0);
    addr_change("add 192.168.50.1/24 brd label eth0:1", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "192.168.50.1", 24, "192.168.50.255", "eth0:1", 0, 0);
    printf("-- dump\n");
    dump(RTM_GETADDR);
    dump(RTM_GETROUTE);
    route_get("192.168.50.7");
    route_get("10.0.2.50");
    route_get("192.168.50.1");
    source("192.168.50.7");
    source("10.0.2.9");
    local_tcp("10.0.2.50");
    local_tcp("192.168.50.1");
    printf("-- again and replace\n");
    addr_change("add 10.0.2.50/24 again (excl)", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "10.0.2.50", 24, NULL, NULL, 0, 0);
    addr_change("replace 10.0.2.50/24 lifetimes 300/600", RTM_NEWADDR, NLM_F_CREATE | NLM_F_REPLACE, eth0, "10.0.2.50", 24, NULL, NULL, 300, 600);
    printf("-- errors\n");
    addr_change("add 10.0.3.1/33", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "10.0.3.1", 33, NULL, NULL, 0, 0);
    addr_change("add without IFA_LOCAL", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, NULL, 24, NULL, NULL, 0, 0);
    addr_change("add 10.0.3.1/24 dev 99", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, 99, "10.0.3.1", 24, NULL, NULL, 0, 0);
    addr_change("add 10.0.3.1/24 valid 0", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, eth0, "10.0.3.1", 24, NULL, NULL, 5, 0);
    addr_change("del 10.0.3.1/24 (none)", RTM_DELADDR, 0, eth0, "10.0.3.1", 24, NULL, NULL, 0, 0);
    addr_change("del dev 99", RTM_DELADDR, 0, 99, "10.0.3.1", 24, NULL, NULL, 0, 0);
    printf("-- delete\n");
    addr_change("del 192.168.50.1/24", RTM_DELADDR, 0, eth0, "192.168.50.1", 24, NULL, NULL, 0, 0);
    addr_change("del 10.0.2.50/24", RTM_DELADDR, 0, eth0, "10.0.2.50", 24, NULL, NULL, 0, 0);
    dump(RTM_GETADDR);
    dump(RTM_GETROUTE);
    route_get("192.168.50.7");
    return 0;
}
