// RA の中の、ユーザーのプログラムに渡す ND の項目 (RDNSS: DNS サーバー、DNSSL など: Linux の ndisc_is_useropt) を
// RTM_NEWNDUSEROPT で知らせる (ndisc_ra_useropt: 組は RTNLGRP_ND_USEROPT (20)、項目 1 つに 1 つ。struct nduseroptmsg、
// その項目のバイト、NDUSEROPT_SRCADDR (RA を送ったルーター))。raw の ICMPv6 で RS を ff02::2 へ送ると、QEMU の
// ユーザーネットワークが RA (SLLA、prefix、RDNSS fec0::3 など) を返す。
// その RA で、もうある SLAAC の番地の lifetime を新しくすると RTM_NEWADDR (addrconf_prefix_rcv_add_addr の update_lft:
// tentative でなければ ipv6_ifa_notify) も。番地と道の知らせ (RTMGRP_IPV6_IFADDR、RTMGRP_IPV6_ROUTE) も出す。
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net rdnss6)
//   aios の中で: sudo ./rdnss6      aarch64-linux-gnu-gcc -O2 -static -o rdnss6 test/rdnss6.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/icmp6.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static char buf[65536];

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

// ND の項目を 1 行に
static void opt_line(const unsigned char *o, int len) {
    char ip[INET6_ADDRSTRLEN];
    printf("    option type %u len %u", o[0], o[1]);
    if (o[0] == 25 && len >= 24) {
        unsigned life = ntohl(*(unsigned *)(o + 4));
        printf(" rdnss lifetime %u:", life);
        for (int i = 8; i + 16 <= len; i += 16)
            printf(" %s", inet_ntop(AF_INET6, o + i, ip, sizeof ip));
    } else if (o[0] == 31 && len >= 8) {
        printf(" dnssl lifetime %u:", ntohl(*(unsigned *)(o + 4)));
        for (int i = 8; i < len && o[i];) {
            printf(" ");
            while (i < len && o[i]) {
                printf("%.*s.", o[i], o + i + 1);
                i += o[i] + 1;
            }
            i++;
        }
    } else if (o[0] == 3 && len >= 32) {
        printf(" prefix %s/%u flags %#x", inet_ntop(AF_INET6, o + 16, ip, sizeof ip), o[2], o[3]);
    }
    printf("\n");
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    unsigned eth0 = if_nametoindex("eth0");
    printf("eth0 slaac: %s\n", wait_slaac() ? "yes" : "no");
    int nl = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = 1u << (RTNLGRP_ND_USEROPT - 1) | RTMGRP_IPV6_IFADDR | RTMGRP_IPV6_ROUTE};
    printf("bind nd_useropt: %s\n", bind(nl, (struct sockaddr *)&a, sizeof a) ? strerror(errno) : "ok");
    int g = RTNLGRP_ND_USEROPT;
    printf("add membership: %s\n", setsockopt(nl, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &g, sizeof g) ? strerror(errno) : "ok");

    // RS を送る (ndisc_send_rs と同じ: ff02::2、hop limit 255)
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    int hops = 255;
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &hops, sizeof hops);
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &eth0, sizeof eth0);
    unsigned char rs[8] = {ND_ROUTER_SOLICIT, 0};
    struct sockaddr_in6 d = {.sin6_family = AF_INET6, .sin6_scope_id = eth0};
    inet_pton(AF_INET6, "ff02::2", &d.sin6_addr);
    long r = sendto(s, rs, sizeof rs, 0, (struct sockaddr *)&d, sizeof d);
    printf("send rs: %s\n", r < 0 ? strerror(errno) : "ok");

    // 知らせ (はじめのものから 500 ミリ秒まで)
    int got_any = 0, total = 3000;
    while (total > 0) {
        struct pollfd p = {nl, POLLIN, 0};
        if (poll(&p, 1, total) <= 0)
            break;
        long n = recv(nl, buf, sizeof buf, 0);
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR) {
                struct ifaddrmsg *ia = NLMSG_DATA(h);
                char ip[INET6_ADDRSTRLEN] = "";
                unsigned fl = ia->ifa_flags;
                const char *pref = "?", *valid = "?";
                int alen = IFA_PAYLOAD(h);
                for (struct rtattr *at = IFA_RTA(ia); RTA_OK(at, alen); at = RTA_NEXT(at, alen)) {
                    if (at->rta_type == IFA_ADDRESS)
                        inet_ntop(AF_INET6, RTA_DATA(at), ip, sizeof ip);
                    if (at->rta_type == IFA_FLAGS)
                        fl = *(unsigned *)RTA_DATA(at);
                    if (at->rta_type == IFA_CACHEINFO) {
                        struct ifa_cacheinfo *c = RTA_DATA(at);
                        pref = c->ifa_prefered == 0xffffffff ? "forever" : "finite";
                        valid = c->ifa_valid == 0xffffffff ? "forever" : "finite";
                    }
                }
                printf("%s flags=%#x if%s %s/%u flags %#x prefered %s valid %s\n", h->nlmsg_type == RTM_NEWADDR ? "NEWADDR" : "DELADDR", h->nlmsg_flags,
                       ia->ifa_index == eth0 ? "eth0" : "other", ip, ia->ifa_prefixlen, fl, pref, valid);
                continue;
            }
            if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_DELROUTE) {
                struct rtmsg *rt = NLMSG_DATA(h);
                char dst[INET6_ADDRSTRLEN] = "::", gw[INET6_ADDRSTRLEN] = "";
                int alen = RTM_PAYLOAD(h);
                for (struct rtattr *at = RTM_RTA(rt); RTA_OK(at, alen); at = RTA_NEXT(at, alen)) {
                    if (at->rta_type == RTA_DST)
                        inet_ntop(AF_INET6, RTA_DATA(at), dst, sizeof dst);
                    if (at->rta_type == RTA_GATEWAY)
                        inet_ntop(AF_INET6, RTA_DATA(at), gw, sizeof gw);
                }
                printf("%s flags=%#x %s/%u via %s proto %u type %u\n", h->nlmsg_type == RTM_NEWROUTE ? "NEWROUTE" : "DELROUTE", h->nlmsg_flags, dst, rt->rtm_dst_len,
                       gw[0] ? gw : "-", rt->rtm_protocol, rt->rtm_type);
                continue;
            }
            if (h->nlmsg_type != RTM_NEWNDUSEROPT) {
                printf("msg type %u\n", h->nlmsg_type);
                continue;
            }
            struct nduseroptmsg *m = NLMSG_DATA(h);
            printf("NEWNDUSEROPT flags=%#x family %u opts_len %u ifindex %s icmp %u/%u pad %u/%u/%u len %u\n", h->nlmsg_flags, m->nduseropt_family,
                   m->nduseropt_opts_len, m->nduseropt_ifindex == (int)eth0 ? "eth0" : "other", m->nduseropt_icmp_type, m->nduseropt_icmp_code,
                   m->nduseropt_pad1, m->nduseropt_pad2, m->nduseropt_pad3, h->nlmsg_len);
            unsigned char *o = (unsigned char *)(m + 1);
            opt_line(o, m->nduseropt_opts_len);
            int off = NLMSG_ALIGN(sizeof *m + m->nduseropt_opts_len);
            int alen = h->nlmsg_len - NLMSG_HDRLEN - off;
            for (struct rtattr *at = (struct rtattr *)((char *)m + off); RTA_OK(at, alen); at = RTA_NEXT(at, alen)) {
                char ip[INET6_ADDRSTRLEN] = "?";
                if (at->rta_type == NDUSEROPT_SRCADDR && RTA_PAYLOAD(at) == 16)
                    inet_ntop(AF_INET6, RTA_DATA(at), ip, sizeof ip);
                printf("    attr %u len %u %s\n", at->rta_type, (unsigned)RTA_PAYLOAD(at), ip);
            }
            if (!got_any)
                total = 500;
            got_any = 1;
        }
    }
    if (!got_any)
        printf("(no NEWNDUSEROPT)\n");
    return 0;
}
