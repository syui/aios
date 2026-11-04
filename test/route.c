// 道の表 (NETLINK_ROUTE の RTM_GETROUTE): dump (fib_dump_info: main と local の表、strict の表えらび) と 1 つの道
// (inet_rtm_getroute と rt_fill_info: ip route get。fibmatch、RTA_SRC、RTA_OIF、strict のたしかめ)、そして dump の
// 家族の fallback (AF_PACKET などは rtnl_dump_all。Firefox の NetlinkService が送るもの) と、Firefox の「つながって
// いるか」(ROUTE_CHECK への道が unicast で scope universe で RTA_OIF がある)。
// 出すだけのテスト: 手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --net route)。eth0 が 10.0.2.15/24 で
// ゲートウェイ 10.0.2.2 (QEMU のユーザーネットワーク) のときの答え。IPv6 のもの (手本の Linux にはある) は出さない。
// uid はじかに出さず、自分の uid か。Firefox の見かたで「つながっていない」なら終わりの番号 1 (test/vm-ctests.sh)
//   aios の中で: ./route      aarch64-linux-gnu-gcc -O2 -static -o route test/route.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef NETLINK_GET_STRICT_CHK
#define NETLINK_GET_STRICT_CHK 12
#endif
#ifndef RTM_F_FIB_MATCH
#define RTM_F_FIB_MATCH 0x2000
#endif

static char buf[65536];
static unsigned seqno = 100;

static int nlsock(int strict) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (s < 0) {
        printf("socket: %s\n", strerror(errno));
        exit(1);
    }
    if (strict) {
        int one = 1;
        if (setsockopt(s, SOL_NETLINK, NETLINK_GET_STRICT_CHK, &one, sizeof one) != 0)
            printf("strict: %s\n", strerror(errno));
    }
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

static void send_msg(int s, char *msg) {
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    if (sendto(s, msg, ((struct nlmsghdr *)msg)->nlmsg_len, 0, (struct sockaddr *)&k, sizeof k) < 0)
        printf("  send: %s\n", strerror(errno));
}

static const char *ip(const void *p) {
    static char b[4][32];
    static int i;
    i = (i + 1) % 4;
    inet_ntop(AF_INET, p, b[i], sizeof b[i]);
    return b[i];
}

static const char *ifname(unsigned i) {
    static char b[IF_NAMESIZE + 8];
    if (i == 1)
        return "lo";
    if (!if_indextoname(i, b))
        snprintf(b, sizeof b, "if%u", i);
    return b;
}

// RTM_NEWROUTE (AF_INET): 頭と属性 (並びの順も)
static void show_route(struct nlmsghdr *h) {
    struct rtmsg *r = NLMSG_DATA(h);
    char line[1024];
    int n = snprintf(line, sizeof line, "  route flags=%#x table=%u type=%u scope=%u proto=%u dst_len=%u src_len=%u tos=%u rtm_flags=%#x:",
                     h->nlmsg_flags, r->rtm_table, r->rtm_type, r->rtm_scope, r->rtm_protocol, r->rtm_dst_len, r->rtm_src_len,
                     r->rtm_tos, r->rtm_flags);
    int len = RTM_PAYLOAD(h);
    for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        void *d = RTA_DATA(a);
        unsigned v = RTA_PAYLOAD(a) >= 4 ? *(unsigned *)d : 0;
        switch (a->rta_type) {
        case RTA_TABLE: n += snprintf(line + n, sizeof line - n, " table %u", v); break;
        case RTA_DST: n += snprintf(line + n, sizeof line - n, " dst %s", ip(d)); break;
        case RTA_SRC: n += snprintf(line + n, sizeof line - n, " src %s", ip(d)); break;
        case RTA_GATEWAY: n += snprintf(line + n, sizeof line - n, " via %s", ip(d)); break;
        case RTA_PREFSRC: n += snprintf(line + n, sizeof line - n, " prefsrc %s", ip(d)); break;
        case RTA_OIF: n += snprintf(line + n, sizeof line - n, " oif %s", ifname(v)); break;
        case RTA_PRIORITY: n += snprintf(line + n, sizeof line - n, " metric %u", v); break;
        case RTA_UID: n += snprintf(line + n, sizeof line - n, " uid %s", v == getuid() ? "self" : "other"); break;
        case RTA_CACHEINFO: {
            struct rta_cacheinfo *c = d;
            n += snprintf(line + n, sizeof line - n, " cache(len %u error %u expires %d id %u)", RTA_PAYLOAD(a), c->rta_error,
                          c->rta_expires, c->rta_id);
            break;
        }
        default: n += snprintf(line + n, sizeof line - n, " attr%u(%u)", a->rta_type, RTA_PAYLOAD(a)); break;
        }
    }
    printf("%s\n", line);
}

// 受けた答えを、データグラムごとに出す。IPv6 のものはかぞえずにとばす。DONE か ERROR までか、もうないまで
static void replies(int s, unsigned seq) {
    for (int dg = 0; dg < 64; dg++) {
        long got = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (got < 0) {
            printf("  (no more: %s)\n", errno == EAGAIN ? "EAGAIN" : strerror(errno));
            return;
        }
        int end = 0, shown = 0;
        printf(" datagram:\n");
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            if (h->nlmsg_seq != seq)
                printf("  seq %u (want %u)\n", h->nlmsg_seq, seq);
            if (h->nlmsg_type == NLMSG_DONE) {
                printf("  DONE flags=%#x error=%d\n", h->nlmsg_flags, *(int *)NLMSG_DATA(h));
                end = 1;
            } else if (h->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *e = NLMSG_DATA(h);
                printf("  ERROR %s flags=%#x\n", e->error ? strerrorname_np(-e->error) : "0", h->nlmsg_flags);
                end = 1;
            } else if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_NEWLINK) {
                unsigned char fam = *(unsigned char *)NLMSG_DATA(h);
                if (h->nlmsg_type == RTM_NEWROUTE && fam == AF_INET) {
                    show_route(h);
                    shown++;
                } else if (h->nlmsg_type == RTM_NEWADDR && fam == AF_INET) {
                    struct ifaddrmsg *a = NLMSG_DATA(h);
                    printf("  addr flags=%#x %s/%u\n", h->nlmsg_flags, ifname(a->ifa_index), a->ifa_prefixlen);
                    shown++;
                } else if (h->nlmsg_type == RTM_NEWLINK) {
                    struct ifinfomsg *l = NLMSG_DATA(h);
                    const char *nm = ifname(l->ifi_index);
                    if (!strcmp(nm, "lo") || !strcmp(nm, "eth0")) {
                        printf("  link flags=%#x %s\n", h->nlmsg_flags, nm);
                        shown++;
                    }
                }
            } else {
                printf("  type %u flags=%#x\n", h->nlmsg_type, h->nlmsg_flags);
            }
        }
        if (end)
            return;
        if (!shown)
            printf("  (only IPv6 or other links)\n");
    }
}

// rtgenmsg (1 バイト) の dump
static void gen_dump(const char *what, int s, int type, int family) {
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtgenmsg));
    h->nlmsg_type = type;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    h->nlmsg_seq = ++seqno;
    ((struct rtgenmsg *)NLMSG_DATA(h))->rtgen_family = family;
    printf("%s\n", what);
    send_msg(s, msg);
    replies(s, seqno);
}

// rtmsg の dump (table は頭に、attr_table は RTA_TABLE で)
static void route_dump(const char *what, int strict, int family, int table, int attr_table, int rtm_flags, int scope) {
    int s = nlsock(strict);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = family;
    r->rtm_table = table;
    r->rtm_flags = rtm_flags;
    r->rtm_scope = scope;
    if (attr_table)
        put_attr(msg, RTA_TABLE, &attr_table, 4);
    printf("%s\n", what);
    send_msg(s, msg);
    replies(s, seqno);
    close(s);
}

// 1 つの道 (ip route get)。dst が NULL なら RTA_DST なし
static void route_get(const char *what, int strict, const char *dst, const char *src, int oif, int rtm_flags, int dst_len, int ack) {
    int s = nlsock(strict);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST | (ack ? NLM_F_ACK : 0);
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET;
    r->rtm_flags = rtm_flags;
    r->rtm_dst_len = dst_len;
    struct in_addr a;
    if (dst) {
        inet_pton(AF_INET, dst, &a);
        put_attr(msg, RTA_DST, &a, 4);
    }
    if (src) {
        r->rtm_src_len = 32;
        inet_pton(AF_INET, src, &a);
        put_attr(msg, RTA_SRC, &a, 4);
    }
    if (oif)
        put_attr(msg, RTA_OIF, &oif, 4);
    printf("%s\n", what);
    send_msg(s, msg);
    replies(s, seqno);
    close(s);
}

// 頭が短い (rtmsg より短い) 1 つの道と、属性のちがうもの
static void odd_gets(void) {
    int s = nlsock(0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(4);
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST;
    h->nlmsg_seq = ++seqno;
    *(unsigned char *)NLMSG_DATA(h) = AF_INET;
    printf("get with a 4-byte body\n");
    send_msg(s, msg);
    replies(s, seqno);
    h->nlmsg_len = NLMSG_LENGTH(0);
    h->nlmsg_seq = ++seqno;
    printf("get with an empty body\n");
    send_msg(s, msg);
    replies(s, seqno);
    close(s);

    // strict: 知らない属性 (RTA_PRIORITY)、頭の table、RTA_DST に dst_len 0
    s = nlsock(1);
    memset(msg, 0, sizeof msg);
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET;
    r->rtm_dst_len = 32;
    struct in_addr a;
    inet_pton(AF_INET, "1.1.1.1", &a);
    put_attr(msg, RTA_DST, &a, 4);
    int prio = 5;
    put_attr(msg, RTA_PRIORITY, &prio, 4);
    printf("strict get with RTA_PRIORITY\n");
    send_msg(s, msg);
    replies(s, seqno);
    close(s);
}

// Firefox の NetlinkService のように: AF_PACKET の dump をいくつかと、ROUTE_CHECK への道
static int firefox(void) {
    int s = nlsock(0);
    struct sockaddr_nl me = {.nl_family = AF_NETLINK,
                             .nl_groups = RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR | RTMGRP_LINK | RTMGRP_NEIGH | RTMGRP_IPV4_ROUTE |
                                          RTMGRP_IPV6_ROUTE};
    if (bind(s, (struct sockaddr *)&me, sizeof me) != 0)
        printf("firefox bind: %s\n", strerror(errno));
    int addrs = 0, routes = 0, linkup = 0;
    int types[] = {RTM_GETLINK, RTM_GETADDR, RTM_GETROUTE};
    for (int i = 0; i < 3; i++) {
        char msg[64] = {0};
        struct nlmsghdr *h = (struct nlmsghdr *)msg;
        h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtgenmsg));
        h->nlmsg_type = types[i];
        h->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
        h->nlmsg_seq = ++seqno;
        ((struct rtgenmsg *)NLMSG_DATA(h))->rtgen_family = AF_PACKET;
        send_msg(s, msg);
        for (int done = 0; !done;) {
            long got = recv(s, buf, sizeof buf, MSG_DONTWAIT);
            if (got < 0)
                break;
            for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
                if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR)
                    done = 1;
                else if (h->nlmsg_type == RTM_NEWADDR && ((struct ifaddrmsg *)NLMSG_DATA(h))->ifa_family == AF_INET)
                    addrs++;
                else if (h->nlmsg_type == RTM_NEWROUTE && ((struct rtmsg *)NLMSG_DATA(h))->rtm_family == AF_INET)
                    routes++;
            }
        }
    }
    printf("firefox: %d IPv4 addresses, %d IPv4 routes\n", addrs, routes);
    // NetlinkRtMsg: rtm_dst_len 32 と RTA_DST、NLM_F_REQUEST だけ
    char msg[64] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET;
    r->rtm_dst_len = 32;
    struct in_addr a;
    inet_pton(AF_INET, "8.8.8.8", &a);
    put_attr(msg, RTA_DST, &a, 4);
    send_msg(s, msg);
    long got = recv(s, buf, sizeof buf, MSG_DONTWAIT);
    for (struct nlmsghdr *h = (struct nlmsghdr *)buf; got > 0 && NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
        if (h->nlmsg_type != RTM_NEWROUTE || (h->nlmsg_flags & NLM_F_MULTI))
            continue;
        struct rtmsg *r = NLMSG_DATA(h);
        int len = RTM_PAYLOAD(h), oif = 0;
        for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len))
            if (a->rta_type == RTA_OIF)
                oif = 1;
        linkup = r->rtm_type == RTN_UNICAST && r->rtm_scope == RT_SCOPE_UNIVERSE && oif;
    }
    printf("firefox: link %s\n", linkup ? "up" : "down");
    close(s);
    return linkup;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    // dump: 家族ごと
    route_dump("dump AF_INET", 0, AF_INET, 0, 0, 0, 0);
    route_dump("dump AF_UNSPEC", 0, AF_UNSPEC, 0, 0, 0, 0);
    route_dump("dump AF_INET RTM_F_CLONED", 0, AF_INET, 0, 0, RTM_F_CLONED, 0);
    route_dump("strict dump table main", 1, AF_INET, RT_TABLE_MAIN, 0, 0, 0);
    route_dump("strict dump table local", 1, AF_INET, RT_TABLE_LOCAL, 0, 0, 0);
    route_dump("strict dump RTA_TABLE 255", 1, AF_INET, 0, 255, 0, 0);
    route_dump("strict dump table 100", 1, AF_INET, 100, 0, 0, 0);
    route_dump("strict dump all", 1, AF_INET, 0, 0, 0, 0);
    route_dump("strict dump with scope", 1, AF_INET, 0, 0, 0, RT_SCOPE_LINK);
    route_dump("strict dump RTM_F_CLONED", 1, AF_INET, 0, 0, RTM_F_CLONED, 0);
    // rtgenmsg の dump と家族の fallback
    int s = nlsock(0);
    gen_dump("gen dump route AF_PACKET", s, RTM_GETROUTE, AF_PACKET);
    gen_dump("gen dump route AF_INET", s, RTM_GETROUTE, AF_INET);
    gen_dump("gen dump addr AF_PACKET", s, RTM_GETADDR, AF_PACKET);
    gen_dump("gen dump addr AF_INET", s, RTM_GETADDR, AF_INET);
    gen_dump("gen dump link AF_INET", s, RTM_GETLINK, AF_INET);
    gen_dump("gen dump route AF_UNIX", s, RTM_GETROUTE, AF_UNIX);
    close(s);
    // 1 つの道
    route_get("get 1.1.1.1", 0, "1.1.1.1", NULL, 0, 0, 32, 0);
    route_get("get 10.0.2.2", 0, "10.0.2.2", NULL, 0, 0, 32, 0);
    route_get("get 10.0.2.15", 0, "10.0.2.15", NULL, 0, 0, 32, 0);
    route_get("get 10.0.2.255", 0, "10.0.2.255", NULL, 0, 0, 32, 0);
    route_get("get 10.0.2.0", 0, "10.0.2.0", NULL, 0, 0, 32, 0);
    route_get("get 127.0.0.1", 0, "127.0.0.1", NULL, 0, 0, 32, 0);
    route_get("get 127.1.2.3", 0, "127.1.2.3", NULL, 0, 0, 32, 0);
    route_get("get 127.255.255.255", 0, "127.255.255.255", NULL, 0, 0, 32, 0);
    route_get("get without RTA_DST", 0, NULL, NULL, 0, 0, 0, 0);
    route_get("get 1.1.1.1 dst_len 0", 0, "1.1.1.1", NULL, 0, 0, 0, 0);
    route_get("get 1.1.1.1 with ACK", 0, "1.1.1.1", NULL, 0, 0, 32, 1);
    route_get("get 1.1.1.1 from 10.0.2.15", 0, "1.1.1.1", "10.0.2.15", 0, 0, 32, 0);
    route_get("get 1.1.1.1 from 127.0.0.1", 0, "1.1.1.1", "127.0.0.1", 0, 0, 32, 0);
    route_get("get 1.1.1.1 from 1.2.3.4", 0, "1.1.1.1", "1.2.3.4", 0, 0, 32, 0);
    route_get("get 1.1.1.1 oif 99", 0, "1.1.1.1", NULL, 99, 0, 32, 0);
    route_get("get 10.0.2.2 oif eth0", 0, "10.0.2.2", NULL, 2, 0, 32, 0);
    route_get("fibmatch 1.1.1.1", 0, "1.1.1.1", NULL, 0, RTM_F_FIB_MATCH, 32, 0);
    route_get("fibmatch 10.0.2.2", 0, "10.0.2.2", NULL, 0, RTM_F_FIB_MATCH, 32, 0);
    route_get("fibmatch 10.0.2.15", 0, "10.0.2.15", NULL, 0, RTM_F_FIB_MATCH, 32, 0);
    route_get("fibmatch 127.1.2.3", 0, "127.1.2.3", NULL, 0, RTM_F_FIB_MATCH, 32, 0);
    route_get("strict get 1.1.1.1", 1, "1.1.1.1", NULL, 0, 0, 32, 0);
    route_get("strict get 1.1.1.1 dst_len 0", 1, "1.1.1.1", NULL, 0, 0, 0, 0);
    route_get("strict get 1.1.1.1 dst_len 24", 1, "1.1.1.1", NULL, 0, 0, 24, 0);
    route_get("strict get RTM_F_CLONED", 1, "1.1.1.1", NULL, 0, RTM_F_CLONED, 32, 0);
    odd_gets();
    return firefox() ? 0 : 1;
}
