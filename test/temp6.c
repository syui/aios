// IPv6 の一時アドレス (RFC 8981。Linux の addrconf.c: manage_tempaddrs、ipv6_create_tempaddr、ipv6_gen_rnd_iid と、
// 送り元のえらびかたの rule 7 (IPV6_SADDR_RULE_PRIVACY)、IPV6_ADDR_PREFERENCES): eth0 の use_tempaddr を 2 にして
// raw の ICMPv6 で RS を送ると、QEMU のユーザーネットワークの RA で SLAAC の番地が新しくなり、一時アドレス (IFA_F_TEMPORARY、
// でたらめな IID、lifetime は SLAAC のものと temp_valid_lft / temp_prefered_lft の小さいほう) ができる (DAD つき)。
// その知らせ、番地の一覧、/proc/net/if_inet6、送り元 (use_tempaddr と IPV6_ADDR_PREFERENCES ごと)、消す。一時アドレスの
// IID は毎回ちがうので "<prefix>::<temp>" と出す。lifetime は SLAAC の番地のものとくらべる
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net temp6)
//   aios の中で: sudo ./temp6      aarch64-linux-gnu-gcc -O2 -static -o temp6 test/temp6.c
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

#ifndef IPV6_ADDR_PREFERENCES
#define IPV6_ADDR_PREFERENCES 72
#endif
#ifndef IFA_PROTO
#define IFA_PROTO 11
#endif

static char buf[65536];
static unsigned eth0;
static unsigned char ll[16], pub[16];
static unsigned pub_pref, pub_valid;
static unsigned char tmp_addr[16];
static int have_tmp, silent;

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case ENOENT: return "ENOENT";
    case EADDRNOTAVAIL: return "EADDRNOTAVAIL";
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

static void hex16(const char *s, unsigned char *a) {
    for (int i = 0; i < 16; i++)
        sscanf(s + 2 * i, "%2hhx", &a[i]);
}

// eth0 の SLAAC の番地 (DAD がおわったもの) と link-local
static int wait_slaac(void) {
    for (int i = 0; i < 300; i++) {
        FILE *f = fopen("/proc/net/if_inet6", "r");
        char line[256];
        int ok = 0;
        while (f && fgets(line, sizeof line, f)) {
            unsigned idx, plen, scope, flags;
            char addr[40], name[32];
            if (sscanf(line, "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) != 6 || strcmp(name, "eth0"))
                continue;
            if (scope == 0x20)
                hex16(addr, ll);
            else if (!(flags & 0x41)) {
                hex16(addr, pub);
                ok = 1;
            }
        }
        if (f)
            fclose(f);
        if (ok && ll[0] == 0xfe)
            return 1;
        ms_sleep(100);
    }
    return 0;
}

// 番地を出す: 一時アドレス (SLAAC の番地と同じ /64 で IID がちがう) は <prefix>::<temp>
static const char *show(const unsigned char *a) {
    static char b[4][64];
    static int k;
    char *o = b[k++ & 3];
    if (!memcmp(a, pub, 8) && memcmp(a + 8, ll + 8, 8)) {
        unsigned char p[16] = {0};
        memcpy(p, a, 8);
        char s[INET6_ADDRSTRLEN];
        inet_ntop(AF_INET6, p, s, sizeof s);
        s[strlen(s) - 1] = 0;
        snprintf(o, 64, "%s:<temp>", s);
    } else {
        inet_ntop(AF_INET6, a, o, 64);
    }
    return o;
}

static void sysctl(const char *dev, const char *name) {
    char p[128], v[64] = "";
    snprintf(p, sizeof p, "/proc/sys/net/ipv6/conf/%s/%s", dev, name);
    FILE *f = fopen(p, "r");
    if (!f) {
        printf("%s/%s: %s\n", dev, name, en(errno));
        return;
    }
    if (fgets(v, sizeof v, f))
        v[strcspn(v, "\n")] = 0;
    fclose(f);
    printf("%s/%s: %s\n", dev, name, v);
}

static void set_sysctl(const char *dev, const char *name, const char *val) {
    char p[128];
    snprintf(p, sizeof p, "/proc/sys/net/ipv6/conf/%s/%s", dev, name);
    FILE *f = fopen(p, "w");
    int e = 0;
    if (!f)
        e = errno;
    else if (fputs(val, f) < 0 || fclose(f))
        e = errno;
    printf("write %s/%s %s: %s\n", dev, name, val, en(e));
}

// lifetime を SLAAC の番地のものとくらべる (3 秒まではおなじ)
static const char *cmp(unsigned v, unsigned ref) {
    if (v == 0xffffffff)
        return "forever";
    if (v + 3 >= ref && v <= ref + 3)
        return "=slaac";
    static char b[4][32];
    static int k;
    char *o = b[k++ & 3];
    snprintf(o, 32, "%s", v < ref ? "<slaac" : ">slaac");
    return o;
}

// RTM_NEWADDR / DELADDR / GETADDR の 1 行
static void addr_line(struct nlmsghdr *h, const char *what) {
    struct ifaddrmsg *ia = NLMSG_DATA(h);
    if (ia->ifa_family != AF_INET6 || ia->ifa_index != eth0)
        return;
    unsigned char a[16] = {0};
    unsigned fl = ia->ifa_flags, proto = 0, pref = 0, valid = 0;
    int alen = IFA_PAYLOAD(h);
    for (struct rtattr *at = IFA_RTA(ia); RTA_OK(at, alen); at = RTA_NEXT(at, alen)) {
        if (at->rta_type == IFA_ADDRESS)
            memcpy(a, RTA_DATA(at), 16);
        if (at->rta_type == IFA_FLAGS)
            fl = *(unsigned *)RTA_DATA(at);
        if (at->rta_type == IFA_PROTO)
            proto = *(unsigned char *)RTA_DATA(at);
        if (at->rta_type == IFA_CACHEINFO) {
            struct ifa_cacheinfo *c = RTA_DATA(at);
            pref = c->ifa_prefered;
            valid = c->ifa_valid;
        }
    }
    if (!memcmp(a, pub, 16) && h->nlmsg_type != RTM_DELADDR) {
        pub_pref = pref;
        pub_valid = valid;
    }
    if ((fl & 1) && !have_tmp) {
        memcpy(tmp_addr, a, 16);
        have_tmp = 1;
    }
    if (silent)
        return;
    int temp = !memcmp(a, pub, 8) && memcmp(a + 8, ll + 8, 8) && h->nlmsg_type != RTM_DELADDR;
    printf("  %s flags=%#x %s/%u scope %u ifa_flags %#x proto %u prefered %s valid %s", what, h->nlmsg_flags, show(a), ia->ifa_prefixlen, ia->ifa_scope, fl,
           proto, temp ? cmp(pref, pub_pref) : pref == 0xffffffff ? "forever" : "finite",
           temp ? cmp(valid, pub_valid) : valid == 0xffffffff ? "forever" : "finite");
    if (temp) {
        unsigned long long iid;
        memcpy(&iid, a + 8, 8);
        printf(" iid %s", iid ? "random" : "zero");
    }
    printf("\n");
}

static void route_line(struct nlmsghdr *h) {
    struct rtmsg *rt = NLMSG_DATA(h);
    unsigned char dst[16] = {0};
    unsigned table = rt->rtm_table;
    int alen = RTM_PAYLOAD(h);
    for (struct rtattr *at = RTM_RTA(rt); RTA_OK(at, alen); at = RTA_NEXT(at, alen)) {
        if (at->rta_type == RTA_DST)
            memcpy(dst, RTA_DATA(at), 16);
        if (at->rta_type == RTA_TABLE)
            table = *(unsigned *)RTA_DATA(at);
    }
    printf("  %s flags=%#x %s/%u table %u proto %u type %u\n", h->nlmsg_type == RTM_NEWROUTE ? "NEWROUTE" : "DELROUTE", h->nlmsg_flags, show(dst),
           rt->rtm_dst_len, table, rt->rtm_protocol, rt->rtm_type);
}

// 知らせを ms ミリ秒まで (来るたびにのばさない)
static void drain(int nl, int ms, const char *what) {
    printf("%s:\n", what);
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (;;) {
        struct timespec t;
        clock_gettime(CLOCK_MONOTONIC, &t);
        int left = ms - (int)((t.tv_sec - t0.tv_sec) * 1000 + (t.tv_nsec - t0.tv_nsec) / 1000000);
        if (left <= 0)
            break;
        struct pollfd p = {nl, POLLIN, 0};
        if (poll(&p, 1, left) <= 0)
            break;
        long n = recv(nl, buf, sizeof buf, 0);
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR)
                addr_line(h, h->nlmsg_type == RTM_NEWADDR ? "NEWADDR" : "DELADDR");
            else if (h->nlmsg_type == RTM_NEWROUTE || h->nlmsg_type == RTM_DELROUTE)
                route_line(h);
            else if (h->nlmsg_type == NLMSG_ERROR)
                printf("  ack %s\n", en(-((struct nlmsgerr *)NLMSG_DATA(h))->error));
            else
                printf("  msg type %u\n", h->nlmsg_type);
        }
    }
}

// 番地の一覧 (1 度めは SLAAC の番地の lifetime を見るだけ) と /proc/net/if_inet6 (Linux はハッシュの順なので並べる)
static void dump_once(void) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct {
        struct nlmsghdr h;
        struct ifaddrmsg a;
    } req = {{sizeof req, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 1, 0}, {.ifa_family = AF_INET6}};
    send(s, &req, sizeof req, 0);
    for (int done = 0; !done;) {
        long n = recv(s, buf, sizeof buf, 0);
        if (n <= 0)
            break;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR) {
                done = 1;
                break;
            }
            addr_line(h, "addr");
        }
    }
    close(s);
}

static int cmpstr(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

static void dump_addrs(void) {
    silent = 1;
    dump_once();
    silent = 0;
    printf("-- addresses\n");
    dump_once();
    printf("-- /proc/net/if_inet6 (eth0, sorted)\n");
    FILE *f = fopen("/proc/net/if_inet6", "r");
    char line[256], *lines[16];
    int n = 0;
    while (f && fgets(line, sizeof line, f) && n < 16) {
        unsigned idx, plen, scope, flags;
        char addr[40], name[32];
        if (sscanf(line, "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) != 6 || strcmp(name, "eth0"))
            continue;
        unsigned char a[16];
        hex16(addr, a);
        lines[n] = malloc(128);
        snprintf(lines[n++], 128, "  %s plen %u scope %#x flags %#x", show(a), plen, scope, flags);
    }
    if (f)
        fclose(f);
    qsort(lines, n, sizeof lines[0], cmpstr);
    for (int i = 0; i < n; i++)
        printf("%s\n", lines[i]);
}

// 送り元: UDP を dst へ connect して getsockname (prefs が 0 でなければ IPV6_ADDR_PREFERENCES も)
static void source(const char *dst, unsigned scope, int prefs) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    int e = 0;
    if (prefs && setsockopt(s, IPPROTO_IPV6, IPV6_ADDR_PREFERENCES, &prefs, sizeof prefs))
        e = errno;
    struct sockaddr_in6 d = {.sin6_family = AF_INET6, .sin6_port = htons(9), .sin6_scope_id = scope};
    inet_pton(AF_INET6, dst, &d.sin6_addr);
    if (!e && connect(s, (struct sockaddr *)&d, sizeof d))
        e = errno;
    struct sockaddr_in6 me = {0};
    socklen_t l = sizeof me;
    getsockname(s, (struct sockaddr *)&me, &l);
    int v = -1;
    socklen_t vl = sizeof v;
    int ge = getsockopt(s, IPPROTO_IPV6, IPV6_ADDR_PREFERENCES, &v, &vl) ? errno : 0;
    printf("  source for %s prefs %#x: %s %s (getsockopt %s %#x)\n", dst, prefs, en(e), e ? "-" : show(me.sin6_addr.s6_addr), en(ge), v);
    close(s);
}

static void sources(void) {
    source("2001:db8::1", 0, 0);
    source("2001:db8::1", 0, 0x1);
    source("2001:db8::1", 0, 0x2);
    source("2001:db8::1", 0, 0x3);
    source("2001:db8::1", 0, 0x102);
    source("fe80::1", eth0, 0x1);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    eth0 = if_nametoindex("eth0");
    printf("eth0 slaac: %s\n", wait_slaac() ? "yes" : "no");
    const char *names[] = {"use_tempaddr", "temp_valid_lft", "temp_prefered_lft", "regen_max_retry", "max_desync_factor", "max_addresses", "regen_min_advance"};
    for (int i = 0; i < 7; i++) {
        sysctl("default", names[i]);
        sysctl("eth0", names[i]);
    }
    sysctl("lo", "use_tempaddr");
    sysctl("all", "use_tempaddr");

    int nl = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = RTMGRP_IPV6_IFADDR | RTMGRP_IPV6_ROUTE};
    bind(nl, (struct sockaddr *)&a, sizeof a);
    dump_addrs();
    printf("-- use_tempaddr 0\n");
    sources();

    // use_tempaddr を 2 に (書いただけでは何もできない)、RS を送り、RA で一時アドレスを
    set_sysctl("eth0", "use_tempaddr", "2");
    drain(nl, 500, "after write");
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    int hops = 255;
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &hops, sizeof hops);
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &eth0, sizeof eth0);
    unsigned char rs[8] = {ND_ROUTER_SOLICIT, 0};
    struct sockaddr_in6 d = {.sin6_family = AF_INET6, .sin6_scope_id = eth0};
    inet_pton(AF_INET6, "ff02::2", &d.sin6_addr);
    long r = sendto(s, rs, sizeof rs, 0, (struct sockaddr *)&d, sizeof d);
    printf("send rs: %s\n", r < 0 ? en(errno) : "ok");
    drain(nl, 3500, "after RA");
    dump_addrs();
    printf("-- use_tempaddr 2\n");
    sources();
    set_sysctl("eth0", "use_tempaddr", "1");
    printf("-- use_tempaddr 1\n");
    sources();

    // 2 度めの RA: 一時アドレスがあれば新しく作らない (lifetime をのばすだけ)
    set_sysctl("eth0", "use_tempaddr", "2");
    r = sendto(s, rs, sizeof rs, 0, (struct sockaddr *)&d, sizeof d);
    printf("send rs: %s\n", r < 0 ? en(errno) : "ok");
    drain(nl, 1500, "after second RA");

    // 消す (もとにもどす)
    set_sysctl("eth0", "use_tempaddr", "0");
    if (have_tmp) {
        struct {
            struct nlmsghdr h;
            struct ifaddrmsg a;
            struct rtattr ra;
            unsigned char addr[16];
        } req = {{sizeof req, RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, 2, 0}, {.ifa_family = AF_INET6, .ifa_prefixlen = 64, .ifa_index = eth0}, {RTA_LENGTH(16), IFA_LOCAL}};
        memcpy(req.addr, tmp_addr, 16);
        send(nl, &req, sizeof req, 0);
        drain(nl, 1000, "delete temp");
    } else {
        printf("no temporary address\n");
    }
    dump_addrs();
    printf("-- use_tempaddr 0\n");
    sources();
    return 0;
}
