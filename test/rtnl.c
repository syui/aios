// NETLINK_ROUTE の近くの番地の表と、変わったときの知らせ (kernel/src/netlink.rs の「近くの番地の表」と「道を変える」):
// RTM_GETNEIGH (dump と ip neigh get、strict のたしかめ)、ARP で番地をさがしたときの RTM_NEWNEIGH の知らせ (RTMGRP_NEIGH)、
// ioctl (SIOCADDRT / SIOCDELRT) と netlink (RTM_NEWROUTE / RTM_DELROUTE: ip route add / replace / del) で道を変えたときの
// RTM_NEWROUTE / RTM_DELROUTE の知らせ (RTMGRP_IPV4_ROUTE) と、しくじりの errno。
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net rtnl)。QEMU のユーザー
// ネットワークでは 10.0.2.3 (DNS) が ARP に答え、10.0.2.2 がゲートウェイ。見るのは 10.0.2.3 の項と道の知らせだけ (ほかの
// 近くの番地はいつ問い合わせたかで変わる)。おわりに既定の道をもとにもどす
//   aios の中で: sudo ./rtnl      aarch64-linux-gnu-gcc -O2 -static -o rtnl test/rtnl.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/neighbour.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <net/route.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef NETLINK_GET_STRICT_CHK
#define NETLINK_GET_STRICT_CHK 12
#endif

static char buf[65536];
static unsigned seqno = 500;
static unsigned evport;

static int nlsock(int strict, unsigned groups) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (s < 0) {
        printf("socket: %s\n", strerror(errno));
        exit(1);
    }
    if (strict) {
        int one = 1;
        setsockopt(s, SOL_NETLINK, NETLINK_GET_STRICT_CHK, &one, sizeof one);
    }
    struct sockaddr_nl me = {.nl_family = AF_NETLINK, .nl_groups = groups};
    if (bind(s, (struct sockaddr *)&me, sizeof me) != 0)
        printf("bind: %s\n", strerror(errno));
    return s;
}

static unsigned portid(int s) {
    struct sockaddr_nl a;
    socklen_t l = sizeof a;
    getsockname(s, (struct sockaddr *)&a, &l);
    return a.nl_pid;
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

static in_addr_t A(const char *s) {
    return inet_addr(s);
}

// 1 つのメッセージを 1 行に (print が 0 なら出さずに、出すものかだけ)。neigh は 10.0.2.3 のものだけ (ほかは 0 を返して
// とばす)。pid はじかに出さず、0 か、送ったソケットか、知らせを受けるソケットか、ほかか
static int show(struct nlmsghdr *h, unsigned me, int print) {
    char line[1024];
    int n = 0;
    const char *who = h->nlmsg_pid == 0 ? "0" : h->nlmsg_pid == me ? "me" : h->nlmsg_pid == evport ? "ev" : "other";
    if (h->nlmsg_type == RTM_NEWNEIGH || h->nlmsg_type == RTM_DELNEIGH) {
        struct ndmsg *nd = NLMSG_DATA(h);
        if (nd->ndm_family != AF_INET)
            return 0;
        int len = h->nlmsg_len - NLMSG_LENGTH(sizeof *nd), want = 0;
        for (struct rtattr *a = (struct rtattr *)((char *)nd + NLMSG_ALIGN(sizeof *nd)); RTA_OK(a, len); a = RTA_NEXT(a, len))
            if (a->rta_type == NDA_DST && *(in_addr_t *)RTA_DATA(a) == A("10.0.2.3"))
                want = 1;
        if (!want)
            return 0;
        n += snprintf(line + n, sizeof line - n, "  %s flags=%#x pid=%s ifindex=%d state=%#x ndm_flags=%#x type=%u:",
                      h->nlmsg_type == RTM_NEWNEIGH ? "NEWNEIGH" : "DELNEIGH", h->nlmsg_flags, who, nd->ndm_ifindex, nd->ndm_state,
                      nd->ndm_flags, nd->ndm_type);
        len = h->nlmsg_len - NLMSG_LENGTH(sizeof *nd);
        for (struct rtattr *a = (struct rtattr *)((char *)nd + NLMSG_ALIGN(sizeof *nd)); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
            unsigned char *d = RTA_DATA(a);
            switch (a->rta_type) {
            case NDA_DST: n += snprintf(line + n, sizeof line - n, " dst %s", ip(d)); break;
            case NDA_LLADDR:
                n += snprintf(line + n, sizeof line - n, " lladdr %02x:%02x:%02x:%02x:%02x:%02x", d[0], d[1], d[2], d[3], d[4], d[5]);
                break;
            case NDA_PROBES: n += snprintf(line + n, sizeof line - n, " probes %u", *(unsigned *)d); break;
            case NDA_CACHEINFO: n += snprintf(line + n, sizeof line - n, " cacheinfo(%u)", (unsigned)RTA_PAYLOAD(a)); break;
            default: n += snprintf(line + n, sizeof line - n, " attr%u(%u)", a->rta_type, (unsigned)RTA_PAYLOAD(a)); break;
            }
        }
    } else if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_DELROUTE) {
        struct rtmsg *r = NLMSG_DATA(h);
        if (r->rtm_family != AF_INET)
            return 0;
        n += snprintf(line + n, sizeof line - n, "  %s flags=%#x pid=%s seq=%s table=%u type=%u scope=%u proto=%u dst_len=%u:",
                      h->nlmsg_type == RTM_NEWROUTE ? "NEWROUTE" : "DELROUTE", h->nlmsg_flags, who,
                      h->nlmsg_seq == 0 ? "0" : h->nlmsg_seq == seqno ? "req" : "other", r->rtm_table, r->rtm_type, r->rtm_scope,
                      r->rtm_protocol, r->rtm_dst_len);
        int len = RTM_PAYLOAD(h);
        for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
            void *d = RTA_DATA(a);
            unsigned v = *(unsigned *)d;
            switch (a->rta_type) {
            case RTA_TABLE: n += snprintf(line + n, sizeof line - n, " table %u", v); break;
            case RTA_DST: n += snprintf(line + n, sizeof line - n, " dst %s", ip(d)); break;
            case RTA_GATEWAY: n += snprintf(line + n, sizeof line - n, " via %s", ip(d)); break;
            case RTA_PREFSRC: n += snprintf(line + n, sizeof line - n, " prefsrc %s", ip(d)); break;
            case RTA_OIF: n += snprintf(line + n, sizeof line - n, " oif %u", v); break;
            case RTA_UID: n += snprintf(line + n, sizeof line - n, " uid"); break;
            case RTA_CACHEINFO: n += snprintf(line + n, sizeof line - n, " cache"); break;
            default: n += snprintf(line + n, sizeof line - n, " attr%u(%u)", a->rta_type, (unsigned)RTA_PAYLOAD(a)); break;
            }
        }
    } else if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR) {
        struct ifaddrmsg *a = NLMSG_DATA(h);
        if (a->ifa_family != AF_INET)
            return 0;
        n += snprintf(line + n, sizeof line - n, "  %s flags=%#x pid=%s ifindex=%u prefixlen=%u", h->nlmsg_type == RTM_NEWADDR ? "NEWADDR" : "DELADDR",
                      h->nlmsg_flags, who, a->ifa_index, a->ifa_prefixlen);
    } else if (h->nlmsg_type == NLMSG_DONE) {
        n += snprintf(line + n, sizeof line - n, "  DONE flags=%#x error=%d", h->nlmsg_flags, *(int *)NLMSG_DATA(h));
    } else if (h->nlmsg_type == NLMSG_ERROR) {
        struct nlmsgerr *e = NLMSG_DATA(h);
        n += snprintf(line + n, sizeof line - n, "  ERROR %s flags=%#x", e->error ? strerrorname_np(-e->error) : "0", h->nlmsg_flags);
    } else {
        return 0;
    }
    if (print)
        printf("%s\n", line);
    return 1;
}

// 答えをデータグラムごとに (出すものがあるデータグラムだけ "datagram:")。DONE か ERROR までか、もうないまで
static void answers(int s) {
    unsigned me = portid(s);
    for (int dg = 0; dg < 64; dg++) {
        long got = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (got < 0) {
            printf("  (no more)\n");
            return;
        }
        int end = 0, any = 0;
        long left = got;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, left); h = NLMSG_NEXT(h, left)) {
            any |= show(h, me, 0);
            end |= h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR;
        }
        if (any)
            printf(" datagram:\n");
        left = got;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, left); h = NLMSG_NEXT(h, left))
            show(h, me, 1);
        if (end)
            return;
    }
}

// 知らせのソケットに来たもの (10.0.2.3 の近くの番地と、道とアドレス)
static void events(int ev) {
    long got;
    int any = 0;
    while ((got = recv(ev, buf, sizeof buf, MSG_DONTWAIT)) > 0)
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got))
            any |= show(h, evport, 1);
    if (!any)
        printf("  (no events)\n");
}

static void neigh_dump(const char *what, int strict, int family, int ifindex_hdr, int nda_ifindex, int bad_attr) {
    int s = nlsock(strict, 0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct ndmsg));
    h->nlmsg_type = RTM_GETNEIGH;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    h->nlmsg_seq = ++seqno;
    struct ndmsg *nd = NLMSG_DATA(h);
    nd->ndm_family = family;
    nd->ndm_ifindex = ifindex_hdr;
    if (nda_ifindex)
        put_attr(msg, NDA_IFINDEX, &nda_ifindex, 4);
    if (bad_attr) {
        in_addr_t a = A("10.0.2.3");
        put_attr(msg, NDA_DST, &a, 4);
    }
    printf("%s\n", what);
    send_msg(s, msg);
    answers(s);
    close(s);
}

static void neigh_get(const char *what, int family, int ifindex, const char *dst, int dstlen, int state, int flags, int extra) {
    int s = nlsock(0, 0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct ndmsg));
    h->nlmsg_type = RTM_GETNEIGH;
    h->nlmsg_flags = NLM_F_REQUEST;
    h->nlmsg_seq = ++seqno;
    struct ndmsg *nd = NLMSG_DATA(h);
    nd->ndm_family = family;
    nd->ndm_ifindex = ifindex;
    nd->ndm_state = state;
    nd->ndm_flags = flags;
    if (dst) {
        unsigned char a[16] = {0};
        *(in_addr_t *)a = A(dst);
        put_attr(msg, NDA_DST, a, dstlen);
    }
    if (extra) {
        int v = 1;
        put_attr(msg, NDA_PROBES, &v, 4);
    }
    printf("%s\n", what);
    send_msg(s, msg);
    answers(s);
    close(s);
}

// RTM_NEWROUTE / RTM_DELROUTE (ip route add / replace / del)。gw が NULL ならゲートウェイなし
static void route_change(const char *what, int ev, int type, int flags, const char *dst, int len, const char *gw, int rtype, int scope,
                         int proto, int tos) {
    int s = nlsock(0, 0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = type;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET;
    r->rtm_dst_len = len;
    r->rtm_table = RT_TABLE_MAIN;
    r->rtm_type = rtype;
    r->rtm_scope = scope;
    r->rtm_protocol = proto;
    r->rtm_tos = tos;
    if (dst && len) {
        in_addr_t a = A(dst);
        put_attr(msg, RTA_DST, &a, 4);
    }
    if (gw) {
        in_addr_t a = A(gw);
        put_attr(msg, RTA_GATEWAY, &a, 4);
    }
    printf("%s\n", what);
    send_msg(s, msg);
    answers(s);
    printf(" events:\n");
    events(ev);
    close(s);
}

static void route_get(const char *what, const char *dst) {
    int s = nlsock(0, 0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET;
    r->rtm_dst_len = 32;
    in_addr_t a = A(dst);
    put_attr(msg, RTA_DST, &a, 4);
    printf("%s\n", what);
    send_msg(s, msg);
    answers(s);
    close(s);
}

// strict の dump で main の表
static void main_table(void) {
    int s = nlsock(1, 0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET;
    r->rtm_table = RT_TABLE_MAIN;
    printf("dump table main\n");
    send_msg(s, msg);
    answers(s);
    close(s);
}

// SIOCADDRT / SIOCDELRT で既定の道
static void ioctl_default(const char *what, int ev, unsigned long req) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct rtentry rt;
    memset(&rt, 0, sizeof rt);
    struct sockaddr_in *d = (struct sockaddr_in *)&rt.rt_dst, *g = (struct sockaddr_in *)&rt.rt_gateway,
                       *m = (struct sockaddr_in *)&rt.rt_genmask;
    d->sin_family = g->sin_family = m->sin_family = AF_INET;
    g->sin_addr.s_addr = A("10.0.2.2");
    rt.rt_flags = RTF_UP | RTF_GATEWAY;
    int rc = ioctl(s, req, &rt);
    printf("%s: %s\n", what, rc == 0 ? "0" : strerrorname_np(errno));
    printf(" events:\n");
    events(ev);
    close(s);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    int ev = nlsock(0, RTMGRP_NEIGH | RTMGRP_IPV4_ROUTE | RTMGRP_IPV4_IFADDR);
    evport = portid(ev);

    // 近くの番地: 10.0.2.3 にはじめて送ると、ARP で問い合わせて REACHABLE
    neigh_dump("neigh dump before", 0, AF_INET, 0, 0, 0);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in to = {.sin_family = AF_INET, .sin_port = htons(9), .sin_addr.s_addr = A("10.0.2.3")};
    if (sendto(u, "x", 1, 0, (struct sockaddr *)&to, sizeof to) != 1)
        printf("sendto: %s\n", strerror(errno));
    usleep(500000);
    close(u);
    printf("after sending to 10.0.2.3\n events:\n");
    events(ev);
    neigh_dump("neigh dump AF_INET", 0, AF_INET, 0, 0, 0);
    neigh_dump("neigh dump AF_UNSPEC", 0, AF_UNSPEC, 0, 0, 0);
    neigh_dump("neigh dump AF_PACKET", 0, AF_PACKET, 0, 0, 0);
    neigh_dump("neigh dump AF_INET6", 0, AF_INET6, 0, 0, 0);
    neigh_dump("strict neigh dump NDA_IFINDEX 2", 1, AF_INET, 0, 2, 0);
    neigh_dump("strict neigh dump NDA_IFINDEX 1", 1, AF_INET, 0, 1, 0);
    neigh_dump("strict neigh dump ndm_ifindex 2", 1, AF_INET, 2, 0, 0);
    neigh_dump("strict neigh dump NDA_DST", 1, AF_INET, 0, 0, 1);
    neigh_dump("neigh dump NDA_IFINDEX 1", 0, AF_INET, 0, 1, 0);
    neigh_get("get 10.0.2.3 eth0", AF_INET, 2, "10.0.2.3", 4, 0, 0, 0);
    neigh_get("get 10.0.2.200 eth0", AF_INET, 2, "10.0.2.200", 4, 0, 0, 0);
    neigh_get("get 10.0.2.3 no ifindex", AF_INET, 0, "10.0.2.3", 4, 0, 0, 0);
    neigh_get("get 10.0.2.3 ifindex 99", AF_INET, 99, "10.0.2.3", 4, 0, 0, 0);
    neigh_get("get 10.0.2.3 lo", AF_INET, 1, "10.0.2.3", 4, 0, 0, 0);
    neigh_get("get AF_PACKET", AF_PACKET, 2, "10.0.2.3", 4, 0, 0, 0);
    neigh_get("get without NDA_DST", AF_INET, 2, NULL, 0, 0, 0, 0);
    neigh_get("get 6-byte NDA_DST", AF_INET, 2, "10.0.2.3", 6, 0, 0, 0);
    neigh_get("get with ndm_state", AF_INET, 2, "10.0.2.3", 4, NUD_REACHABLE, 0, 0);
    neigh_get("get NTF_PROXY", AF_INET, 0, "10.0.2.3", 4, 0, NTF_PROXY, 0);
    neigh_get("get NTF_ROUTER", AF_INET, 2, "10.0.2.3", 4, 0, NTF_ROUTER, 0);
    neigh_get("get with NDA_PROBES", AF_INET, 2, "10.0.2.3", 4, 0, 0, 1);

    // 道: ioctl と netlink で既定の道を消して足す
    ioctl_default("SIOCDELRT default", ev, SIOCDELRT);
    route_get("get 1.1.1.1 without a default route", "1.1.1.1");
    ioctl_default("SIOCADDRT default via 10.0.2.2", ev, SIOCADDRT);
    route_change("add default (exists)", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, NULL, 0, "10.0.2.2", RTN_UNICAST, RT_SCOPE_UNIVERSE,
                 RTPROT_BOOT, 0);
    route_change("del default", ev, RTM_DELROUTE, 0, NULL, 0, NULL, 0, RT_SCOPE_NOWHERE, 0, 0);
    route_change("del default again", ev, RTM_DELROUTE, 0, NULL, 0, NULL, 0, RT_SCOPE_NOWHERE, 0, 0);
    route_change("add default via 10.0.2.2 proto static", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, NULL, 0, "10.0.2.2", RTN_UNICAST,
                 RT_SCOPE_UNIVERSE, RTPROT_STATIC, 0);
    route_change("add 192.168.7.0/24 via 10.0.2.2", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.7.0", 24, "10.0.2.2",
                 RTN_UNICAST, RT_SCOPE_UNIVERSE, RTPROT_BOOT, 0);
    main_table();
    route_get("get 192.168.7.9", "192.168.7.9");
    route_change("replace 192.168.7.0/24 proto static", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_REPLACE, "192.168.7.0", 24, "10.0.2.2",
                 RTN_UNICAST, RT_SCOPE_UNIVERSE, RTPROT_STATIC, 0);
    route_change("del 192.168.7.0/24 proto boot", ev, RTM_DELROUTE, 0, "192.168.7.0", 24, NULL, 0, RT_SCOPE_NOWHERE, RTPROT_BOOT, 0);
    route_change("del 192.168.7.0/24 via 10.0.2.9", ev, RTM_DELROUTE, 0, "192.168.7.0", 24, "10.0.2.9", 0, RT_SCOPE_NOWHERE, 0, 0);
    route_change("del 192.168.7.0/24", ev, RTM_DELROUTE, 0, "192.168.7.0", 24, NULL, 0, RT_SCOPE_NOWHERE, 0, 0);
    route_change("del 192.168.7.0/24 again", ev, RTM_DELROUTE, 0, "192.168.7.0", 24, NULL, 0, RT_SCOPE_NOWHERE, 0, 0);
    route_change("add via 10.9.9.9", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.8.0", 24, "10.9.9.9", RTN_UNICAST,
                 RT_SCOPE_UNIVERSE, RTPROT_BOOT, 0);
    route_change("add via 10.0.2.255", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.8.0", 24, "10.0.2.255", RTN_UNICAST,
                 RT_SCOPE_UNIVERSE, RTPROT_BOOT, 0);
    route_change("add 192.168.8.1/24", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.8.1", 24, "10.0.2.2", RTN_UNICAST,
                 RT_SCOPE_UNIVERSE, RTPROT_BOOT, 0);
    route_change("add /33", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.8.0", 33, "10.0.2.2", RTN_UNICAST, RT_SCOPE_UNIVERSE,
                 RTPROT_BOOT, 0);
    route_change("add without NLM_F_CREATE", ev, RTM_NEWROUTE, 0, "192.168.8.0", 24, "10.0.2.2", RTN_UNICAST, RT_SCOPE_UNIVERSE,
                 RTPROT_BOOT, 0);
    route_change("add type 0", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.8.0", 24, "10.0.2.2", 0, RT_SCOPE_UNIVERSE,
                 RTPROT_BOOT, 0);
    route_change("add tos 1", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.8.0", 24, "10.0.2.2", RTN_UNICAST, RT_SCOPE_UNIVERSE,
                 RTPROT_BOOT, 1);
    route_change("add without a gateway or a device", ev, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.8.0", 24, NULL, RTN_UNICAST,
                 RT_SCOPE_UNIVERSE, RTPROT_BOOT, 0);
    // もとにもどす (ipconfig と同じ proto boot)
    route_change("del default", ev, RTM_DELROUTE, 0, NULL, 0, NULL, 0, RT_SCOPE_NOWHERE, 0, 0);
    ioctl_default("SIOCADDRT default via 10.0.2.2 again", ev, SIOCADDRT);
    route_get("get 1.1.1.1", "1.1.1.1");
    return 0;
}
