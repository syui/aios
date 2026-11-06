// IPv6 のマルチキャストの組 (Linux の net/ipv6/mcast.c): ソケットが組に入る・出る (IPV6_JOIN_GROUP / LEAVE_GROUP、
// MCAST_JOIN_GROUP / LEAVE_GROUP: ipv6_sock_mc_join / drop のエラー)、口の組の一覧 (RTM_GETMULTICAST の users と、
// RTNLGRP_IPV6_MCADDR への RTM_NEWMULTICAST / DELMULTICAST の知らせ)、組あてに送ったものを自分で受ける
// (ip6_finish_output2 の IPV6_MULTICAST_LOOP と、__udp6_lib_mcast_deliver: 同じポートのソケットのみなに。
// IPV6_MULTICAST_ALL)、ソケットを閉じると組を出る。MLD の報告そのものは外に出るだけなので見ない
// 出すだけのテスト: 手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --net mld6)
//   aios の中で: ./mld6      aarch64-linux-gnu-gcc -O2 -static -o mld6 test/mld6.c
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

#ifndef RTM_NEWMULTICAST
#define RTM_NEWMULTICAST 56
#define RTM_DELMULTICAST 57
#define RTM_GETMULTICAST 58
#endif
#ifndef IFA_MULTICAST
#define IFA_MULTICAST 7
#endif
#ifndef IFA_MC_USERS
#define IFA_MC_USERS 12
#endif
#ifndef IPV6_MULTICAST_ALL
#define IPV6_MULTICAST_ALL 29
#endif
// RTNLGRP_IPV6_MCADDR (6.14 から)
#define GRP_IPV6_MCADDR 38

static char buf[65536];
static unsigned seqno = 900;

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case ENODEV: return "ENODEV";
    case EADDRINUSE: return "EADDRINUSE";
    case EADDRNOTAVAIL: return "EADDRNOTAVAIL";
    case ENOPROTOOPT: return "ENOPROTOOPT";
    case EPROTO: return "EPROTO";
    case EAGAIN: return "EAGAIN";
    case ENETUNREACH: return "ENETUNREACH";
    case EOPNOTSUPP: return "EOPNOTSUPP";
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

// eth0 の SLAAC の番地ができて DAD が終わるまで
static int wait_slaac(void) {
    for (int i = 0; i < 300; i++) {
        FILE *f = fopen("/proc/net/if_inet6", "r");
        char line[256];
        int ok = 0, tent = 0;
        while (f && fgets(line, sizeof line, f)) {
            unsigned idx, plen, scope, flags;
            char addr[40], name[32];
            if (sscanf(line, "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) == 6 && !strcmp(name, "eth0")) {
                if (scope != 0x20)
                    ok = 1;
                if (flags & 0x40)
                    tent = 1;
            }
        }
        if (f)
            fclose(f);
        if (ok && !tent)
            return 1;
        ms_sleep(100);
    }
    return 0;
}

static struct in6_addr a6(const char *s) {
    struct in6_addr a;
    inet_pton(AF_INET6, s, &a);
    return a;
}

static int join(int s, int opt, const char *grp, int ifindex) {
    struct ipv6_mreq m = {.ipv6mr_multiaddr = a6(grp), .ipv6mr_interface = ifindex};
    return setsockopt(s, IPPROTO_IPV6, opt, &m, sizeof m) ? errno : 0;
}

static int gjoin(int s, int opt, const char *grp, int ifindex, int family) {
    struct group_req g;
    memset(&g, 0, sizeof g);
    g.gr_interface = ifindex;
    struct sockaddr_in6 *p = (struct sockaddr_in6 *)&g.gr_group;
    p->sin6_family = family;
    p->sin6_addr = a6(grp);
    return setsockopt(s, IPPROTO_IPV6, opt, &g, sizeof g) ? errno : 0;
}

// 知らせ (RTM_NEWMULTICAST / DELMULTICAST) と dump の 1 行
static void mc_line(struct nlmsghdr *h) {
    struct ifaddrmsg *a = NLMSG_DATA(h);
    if (a->ifa_index > 2)
        return;
    const char *t = h->nlmsg_type == RTM_NEWMULTICAST ? "NEWMULTICAST" : h->nlmsg_type == RTM_DELMULTICAST ? "DELMULTICAST" : "GETMULTICAST";
    printf("  %s flags=%#x seq=%u if%u fam=%u plen=%u ifa_flags=%#x scope=%u:", t, h->nlmsg_flags, h->nlmsg_seq ? 1 : 0, a->ifa_index, a->ifa_family,
           a->ifa_prefixlen, a->ifa_flags, a->ifa_scope);
    int len = IFA_PAYLOAD(h);
    for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
        unsigned char *d = RTA_DATA(r);
        switch (r->rta_type) {
        case IFA_MULTICAST:
            if (RTA_PAYLOAD(r) == 16) {
                char ip[INET6_ADDRSTRLEN];
                inet_ntop(AF_INET6, d, ip, sizeof ip);
                printf(" %s", ip);
            } else if (RTA_PAYLOAD(r) == 6) {
                printf(" %02x:%02x:%02x:%02x:%02x:%02x", d[0], d[1], d[2], d[3], d[4], d[5]);
            } else {
                printf(" mc(%d)", (int)RTA_PAYLOAD(r));
            }
            break;
        case IFA_MC_USERS: printf(" users=%u", *(unsigned *)d); break;
        case IFA_CACHEINFO: {
            struct ifa_cacheinfo *c = (void *)d;
            printf(" cacheinfo(%#x %#x)", c->ifa_prefered, c->ifa_valid);
            break;
        }
        default: printf(" attr%d(%d)", r->rta_type, (int)RTA_PAYLOAD(r));
        }
    }
    printf("\n");
}

// とどいている知らせを読んで出す
static void drain(int n, const char *what) {
    printf("%s:\n", what);
    for (;;) {
        struct pollfd p = {n, POLLIN, 0};
        if (poll(&p, 1, 150) <= 0)
            break;
        int k = recv(n, buf, sizeof buf, MSG_DONTWAIT);
        if (k <= 0)
            break;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, (unsigned)k); h = NLMSG_NEXT(h, k))
            mc_line(h);
    }
}

static void dump(int family, int index) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct {
        struct nlmsghdr h;
        struct ifaddrmsg a;
    } req = {{sizeof req, RTM_GETMULTICAST, NLM_F_REQUEST | NLM_F_DUMP, ++seqno, 0}, {.ifa_family = family, .ifa_index = index}};
    send(s, &req, sizeof req, 0);
    printf("-- dump family %d if%d\n", family, index);
    for (;;) {
        int k = recv(s, buf, sizeof buf, 0);
        if (k <= 0)
            break;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, (unsigned)k); h = NLMSG_NEXT(h, k)) {
            if (h->nlmsg_type == NLMSG_DONE) {
                close(s);
                return;
            }
            if (h->nlmsg_type == NLMSG_ERROR) {
                printf("  error %s\n", en(-((struct nlmsgerr *)NLMSG_DATA(h))->error));
                close(s);
                return;
            }
            mc_line(h);
        }
    }
    close(s);
}

static int udp6(int port, int reuse) {
    int s = socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    int one = 1;
    if (reuse)
        setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    setsockopt(s, IPPROTO_IPV6, IPV6_RECVPKTINFO, &one, sizeof one);
    setsockopt(s, IPPROTO_IPV6, IPV6_RECVHOPLIMIT, &one, sizeof one);
    if (port) {
        struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
        if (bind(s, (struct sockaddr *)&a, sizeof a))
            printf("bind %d: %s\n", port, en(errno));
    }
    return s;
}

static void send_to(int s, const char *dst, int port, int scope, const char *msg) {
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port), .sin6_addr = a6(dst), .sin6_scope_id = scope};
    int r = sendto(s, msg, strlen(msg), 0, (struct sockaddr *)&a, sizeof a);
    printf("send %s%%%d %s: %s\n", dst, scope, msg, r < 0 ? en(errno) : "ok");
}

// 受けたものを 1 つずつ (送り元、あて先 (IPV6_PKTINFO)、hop limit)。なければ EAGAIN
static void recv_all(int s, const char *name) {
    ms_sleep(100);
    int got = 0;
    for (;;) {
        char data[256], cbuf[256];
        struct sockaddr_in6 from;
        struct iovec iov = {data, sizeof data - 1};
        struct msghdr m = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
        int k = recvmsg(s, &m, MSG_DONTWAIT);
        if (k < 0) {
            if (!got)
                printf("  %s: %s\n", name, en(errno));
            return;
        }
        got = 1;
        data[k] = 0;
        char f[INET6_ADDRSTRLEN], to[INET6_ADDRSTRLEN] = "?";
        inet_ntop(AF_INET6, &from.sin6_addr, f, sizeof f);
        int hops = -1, ifi = -1;
        for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
            if (c->cmsg_level == IPPROTO_IPV6 && c->cmsg_type == IPV6_PKTINFO) {
                struct in6_pktinfo *pi = (void *)CMSG_DATA(c);
                inet_ntop(AF_INET6, &pi->ipi6_addr, to, sizeof to);
                ifi = pi->ipi6_ifindex;
            } else if (c->cmsg_level == IPPROTO_IPV6 && c->cmsg_type == IPV6_HOPLIMIT) {
                hops = *(int *)CMSG_DATA(c);
            }
        }
        printf("  %s: \"%s\" from %s scope=%u to %s if%d hops=%d\n", name, data, f, from.sin6_scope_id, to, ifi, hops);
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (!wait_slaac()) {
        printf("no SLAAC address on eth0\n");
        return 1;
    }
    int eth = if_nametoindex("eth0");
    printf("eth0 index %d\n", eth);

    // 知らせ: RTNLGRP_IPV6_MCADDR に入る
    int n = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct sockaddr_nl na = {.nl_family = AF_NETLINK};
    bind(n, (struct sockaddr *)&na, sizeof na);
    int g = GRP_IPV6_MCADDR;
    printf("add membership %d: %s\n", g, setsockopt(n, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &g, sizeof g) ? en(errno) : "ok");
    // RTNLGRP_MAX (7.4 は RTNLGRP_IPV6_ACADDR)
    for (g = GRP_IPV6_MCADDR + 1; g <= GRP_IPV6_MCADDR + 2; g++) {
        int t = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
        printf("add membership %d: %s\n", g, setsockopt(t, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &g, sizeof g) ? en(errno) : "ok");
        close(t);
    }

    // エラー
    int r1 = udp6(5555, 1);
    struct ipv6_mreq m = {.ipv6mr_multiaddr = a6("ff02::1234"), .ipv6mr_interface = eth};
    printf("short optlen: %s\n", setsockopt(r1, IPPROTO_IPV6, IPV6_JOIN_GROUP, &m, sizeof m - 1) ? en(errno) : "ok");
    printf("unicast group: %s\n", en(join(r1, IPV6_JOIN_GROUP, "2001:db8::1", eth)));
    printf("no such if: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff02::1234", 99)));
    printf("leave unicast: %s\n", en(join(r1, IPV6_LEAVE_GROUP, "2001:db8::1", 0)));
    printf("leave not joined: %s\n", en(join(r1, IPV6_LEAVE_GROUP, "ff02::1234", 0)));
    socklen_t ol = sizeof m;
    printf("getsockopt join: %s\n", getsockopt(r1, IPPROTO_IPV6, IPV6_JOIN_GROUP, &m, &ol) ? en(errno) : "ok");
    int t = socket(AF_INET6, SOCK_STREAM, 0);
    printf("tcp join: %s\n", en(join(t, IPV6_JOIN_GROUP, "ff02::1234", eth)));
    close(t);
    int v4 = socket(AF_INET, SOCK_DGRAM, 0);
    printf("AF_INET join: %s\n", en(join(v4, IPV6_JOIN_GROUP, "ff02::1234", eth)));
    close(v4);
    int raw = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if (raw >= 0) {
        printf("raw join: %s\n", en(join(raw, IPV6_JOIN_GROUP, "ff02::4321", eth)));
        close(raw);
    }
    drain(n, "after errors");

    // 入る: ifindex 0 は道 (ff00::/8 は eth0) から。同じ組に 2 度は EADDRINUSE、口がちがえばよい
    printf("join ff02::1234 if0: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff02::1234", 0)));
    drain(n, "notify");
    printf("again if0: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff02::1234", 0)));
    printf("again eth0: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff02::1234", eth)));
    printf("join lo: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff02::1234", 1)));
    printf("join ff02::1 eth0: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff02::1", eth)));
    printf("join ff05::1:3 eth0: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff05::1:3", eth)));
    printf("join ff0e::abcd if0: %s\n", en(join(r1, IPV6_JOIN_GROUP, "ff0e::abcd", 0)));
    int r2 = udp6(5555, 1);
    printf("r2 join ff02::1234 eth0: %s\n", en(join(r2, IPV6_JOIN_GROUP, "ff02::1234", eth)));
    printf("r2 MCAST_JOIN_GROUP ff02::77 eth0: %s\n", en(gjoin(r2, MCAST_JOIN_GROUP, "ff02::77", eth, AF_INET6)));
    printf("r2 MCAST_JOIN_GROUP AF_INET: %s\n", en(gjoin(r2, MCAST_JOIN_GROUP, "ff02::78", eth, AF_INET)));
    printf("r2 MCAST_LEAVE_GROUP ff02::77: %s\n", en(gjoin(r2, MCAST_LEAVE_GROUP, "ff02::77", 0, AF_INET6)));
    drain(n, "notify");
    dump(AF_INET6, 0);
    dump(AF_PACKET, eth);

    // 送って受ける: r1 (ff02::1234 に入った)、r2 (入った)、r3 (入っていない)。IPV6_MULTICAST_LOOP は 1 がはじめ
    int r3 = udp6(5555, 1);
    int s = udp6(0, 0);
    send_to(s, "ff02::1234", 5555, eth, "group");
    recv_all(r1, "r1");
    recv_all(r2, "r2");
    recv_all(r3, "r3");
    send_to(s, "ff02::1", 5555, eth, "allnodes");
    recv_all(r1, "r1");
    recv_all(r3, "r3");
    recv_all(r2, "r2");
    send_to(s, "ff02::5678", 5555, eth, "notjoined");
    recv_all(r1, "r1");
    send_to(s, "ff01::1", 5555, eth, "nodelocal");
    recv_all(r1, "r1");
    recv_all(r2, "r2");
    recv_all(r3, "r3");
    send_to(s, "ff05::1:3", 5555, 0, "site");
    recv_all(r1, "r1");
    recv_all(r3, "r3");
    recv_all(r2, "r2");
    // IPV6_MULTICAST_ALL を 0 に: 自分が入った組だけ
    int zero = 0;
    printf("r3 MULTICAST_ALL 0: %s\n", setsockopt(r3, IPPROTO_IPV6, IPV6_MULTICAST_ALL, &zero, sizeof zero) ? en(errno) : "ok");
    send_to(s, "ff02::1234", 5555, eth, "all0");
    recv_all(r1, "r1");
    recv_all(r3, "r3");
    recv_all(r2, "r2");
    // hop limit
    int h = 7;
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &h, sizeof h);
    send_to(s, "ff02::1234", 5555, eth, "hops7");
    recv_all(r2, "r2");
    recv_all(r1, "r1");
    // IPV6_MULTICAST_LOOP 0
    printf("loop 0: %s\n", setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &zero, sizeof zero) ? en(errno) : "ok");
    send_to(s, "ff02::1234", 5555, eth, "noloop");
    recv_all(r1, "r1");
    recv_all(r2, "r2");
    int one = 1;
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &one, sizeof one);

    // 出る: r1 の eth0 の ff02::1234 (r2 がまだいる)、r2 も出ると組がなくなる
    printf("r1 leave ff02::1234 eth0: %s\n", en(join(r1, IPV6_LEAVE_GROUP, "ff02::1234", eth)));
    drain(n, "notify");
    printf("r1 leave ff02::1234 if0 (lo): %s\n", en(join(r1, IPV6_LEAVE_GROUP, "ff02::1234", 0)));
    drain(n, "notify");
    printf("r2 leave ff02::1234 if0: %s\n", en(join(r2, IPV6_LEAVE_GROUP, "ff02::1234", 0)));
    drain(n, "notify");
    send_to(s, "ff02::1234", 5555, eth, "left");
    recv_all(r1, "r1");
    recv_all(r2, "r2");
    // 閉じると入っていた組を出る
    close(r1);
    drain(n, "close r1");
    close(r2);
    close(r3);
    close(s);
    drain(n, "close rest");
    dump(AF_INET6, eth);
    dump(AF_INET6, 1);
    dump(AF_PACKET, eth);
    return 0;
}
