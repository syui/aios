// IPv6 の path MTU (packet too big: Linux の icmpv6_rcv → icmpv6_notify → __udp6_lib_err と ip6_sk_update_pmtu、
// rt6_update_pmtu の例外の道)。QEMU のユーザーネットワークは packet too big を送らないので、raw の ICMPv6 で自分の
// 番地あてに作って流す (ループバックで受ける)。つないだ UDP (あて先 2001:db8:5::1、ルーター経由) について:
//   - その前と後の IPV6_MTU、ip -6 route get (例外の道の mtu と expires)
//   - IPV6_RECVERR の行列 (EMSGSIZE、SO_EE_ORIGIN_ICMP6、type 2、info は MTU)、SO_ERROR
//   - 1280 より小さい MTU は 1280 に、大きくする packet too big は見ない
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net pmtu6)
//   aios の中で: sudo ./pmtu6      aarch64-linux-gnu-gcc -O2 -static -o pmtu6 test/pmtu6.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/errqueue.h>
#include <netinet/ip6.h>
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
    case EMSGSIZE: return "EMSGSIZE";
    case EAGAIN: return "EAGAIN";
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

static void route_get(const char *dst) {
    int s = nlsock(0);
    char msg[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)msg;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    h->nlmsg_type = RTM_GETROUTE;
    h->nlmsg_flags = NLM_F_REQUEST;
    h->nlmsg_seq = ++seqno;
    struct rtmsg *r = NLMSG_DATA(h);
    r->rtm_family = AF_INET6;
    r->rtm_dst_len = 128;
    unsigned char ip[16];
    inet_pton(AF_INET6, dst, ip);
    put_attr(msg, RTA_DST, ip, 16);
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
        route_line(a, line, NULL);
        printf("get %s: %s\n", dst, line);
    }
    close(s);
}

// eth0 の site-local / global の番地 (SLAAC)
static int my_addr(struct in6_addr *out) {
    FILE *f = fopen("/proc/net/if_inet6", "r");
    char line[256];
    int ok = 0;
    while (f && fgets(line, sizeof line, f)) {
        unsigned idx, plen, scope, flags;
        char hex[40], name[32];
        if (sscanf(line, "%39s %x %x %x %x %31s", hex, &idx, &plen, &scope, &flags, name) == 6 && !strcmp(name, "eth0") && scope != 0x20 && !(flags & 0x40)) {
            for (int i = 0; i < 16; i++) {
                unsigned b;
                sscanf(hex + 2 * i, "%2x", &b);
                out->s6_addr[i] = b;
            }
            ok = 1;
        }
    }
    if (f)
        fclose(f);
    return ok;
}

static int mtu_of(int s) {
    int v = -1;
    socklen_t l = sizeof v;
    if (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &v, &l))
        return -errno;
    return v;
}

static void errq(int s) {
    char data[256], name[64], ctl[256];
    struct iovec iov = {data, sizeof data};
    struct msghdr m = {.msg_name = name, .msg_namelen = sizeof name, .msg_iov = &iov, .msg_iovlen = 1, .msg_control = ctl, .msg_controllen = sizeof ctl};
    long r = recvmsg(s, &m, MSG_ERRQUEUE | MSG_DONTWAIT);
    if (r < 0) {
        printf("  errqueue: %s\n", en(errno));
        return;
    }
    char ip[INET6_ADDRSTRLEN];
    struct sockaddr_in6 *n = (void *)name;
    printf("  errqueue %ld: name [%s]:%u data '%.*s'\n", r, inet_ntop(AF_INET6, &n->sin6_addr, ip, sizeof ip), ntohs(n->sin6_port), (int)r, data);
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c))
        if (c->cmsg_level == SOL_IPV6 && c->cmsg_type == IPV6_RECVERR) {
            struct sock_extended_err *e = (void *)CMSG_DATA(c);
            struct sockaddr_in6 *o = (void *)SO_EE_OFFENDER(e);
            printf("    errno %s origin %u type %u code %u info %u offender is me: %s\n", en(e->ee_errno), e->ee_origin, e->ee_type, e->ee_code, e->ee_info,
                   o->sin6_family == AF_INET6 ? "yes" : "no");
        }
}

// packet too big (mtu) を自分あてに: 中は me:sport から dst:9 への UDP
static void ptb(const struct in6_addr *me, const struct in6_addr *dst, unsigned short sport, unsigned mtu) {
    int r = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    unsigned char p[8 + 40 + 8 + 4] = {0};
    p[0] = 2; // ICMP6_PACKET_TOO_BIG
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
    printf("eth0 slaac: %s\n", wait_slaac() ? "yes" : "no");
    struct in6_addr me, dst;
    if (!my_addr(&me)) {
        printf("no address\n");
        return 1;
    }
    inet_pton(AF_INET6, "2001:db8:5::1", &dst);
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    int one = 1;
    setsockopt(s, IPPROTO_IPV6, IPV6_RECVERR, &one, sizeof one);
    struct sockaddr_in6 d = {.sin6_family = AF_INET6, .sin6_addr = dst, .sin6_port = htons(9)};
    printf("connect: %s\n", connect(s, (struct sockaddr *)&d, sizeof d) ? en(errno) : "ok");
    struct sockaddr_in6 sn;
    socklen_t sl = sizeof sn;
    getsockname(s, (struct sockaddr *)&sn, &sl);
    printf("source is mine: %s\n", memcmp(&sn.sin6_addr, &me, 16) ? "no" : "yes");
    printf("IPV6_MTU before: %d\n", mtu_of(s));
    route_get("2001:db8:5::1");

    ptb(&me, &dst, ntohs(sn.sin6_port), 1300);
    errq(s);
    errq(s);
    int e = 0;
    socklen_t el = sizeof e;
    getsockopt(s, SOL_SOCKET, SO_ERROR, &e, &el);
    printf("SO_ERROR: %s\n", en(e));
    printf("IPV6_MTU after 1300: %d\n", mtu_of(s));
    route_get("2001:db8:5::1");
    route_get("2001:db8:5::2");

    ptb(&me, &dst, ntohs(sn.sin6_port), 1000);
    errq(s);
    printf("IPV6_MTU after 1000: %d\n", mtu_of(s));
    route_get("2001:db8:5::1");

    ptb(&me, &dst, ntohs(sn.sin6_port), 1400);
    errq(s);
    printf("IPV6_MTU after 1400: %d\n", mtu_of(s));
    return 0;
}
