// rtnetlink のリンクの答えの属性 (rtnl_fill_ifinfo の並びと値: ip -d link) と、マルチキャストの番地の一覧
// (RTM_GETMULTICAST: AF_INET は in_dev_dump_ifmcaddr、AF_INET6 は inet6_dump_ifmcaddr、7.4 の PF_PACKET は dev_mc_dump と
// IFA_F_GLOBAL: ip maddr)。lo と eth0 だけを見る (手本の Linux の sit0 などは見ない)。数が動くもの (統計、時刻) は長さだけ
// 出すテスト: 手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --net rtnllink)
//   aios の中で: ./rtnllink      aarch64-linux-gnu-gcc -O2 -static -o rtnllink test/rtnllink.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <time.h>
#include <linux/if_addr.h>
#include <linux/if_link.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef RTM_GETMULTICAST
#define RTM_GETMULTICAST 58
#endif
#ifndef IFA_MULTICAST
#define IFA_MULTICAST 7
#endif
#ifndef IFA_MC_USERS
#define IFA_MC_USERS 12
#endif

static char buf[1 << 16];
static unsigned seqno = 1;

// IFLA_* の名前 (7.4 の if_link.h の並び)
static const char *ifla[] = {
    "UNSPEC", "ADDRESS", "BROADCAST", "IFNAME", "MTU", "LINK", "QDISC", "STATS", "COST", "PRIORITY", "MASTER", "WIRELESS",
    "PROTINFO", "TXQLEN", "MAP", "WEIGHT", "OPERSTATE", "LINKMODE", "LINKINFO", "NET_NS_PID", "IFALIAS", "NUM_VF",
    "VFINFO_LIST", "STATS64", "VF_PORTS", "PORT_SELF", "AF_SPEC", "GROUP", "NET_NS_FD", "EXT_MASK", "PROMISCUITY",
    "NUM_TX_QUEUES", "NUM_RX_QUEUES", "CARRIER", "PHYS_PORT_ID", "CARRIER_CHANGES", "PHYS_SWITCH_ID", "LINK_NETNSID",
    "PHYS_PORT_NAME", "PROTO_DOWN", "GSO_MAX_SEGS", "GSO_MAX_SIZE", "PAD", "XDP", "EVENT", "NEW_NETNSID", "IF_NETNSID",
    "CARRIER_UP_COUNT", "CARRIER_DOWN_COUNT", "NEW_IFINDEX", "MIN_MTU", "MAX_MTU", "PROP_LIST", "ALT_IFNAME",
    "PERM_ADDRESS", "PROTO_DOWN_REASON", "PARENT_DEV_NAME", "PARENT_DEV_BUS_NAME", "GRO_MAX_SIZE", "TSO_MAX_SIZE",
    "TSO_MAX_SEGS", "ALLMULTI", "DEVLINK_PORT", "GSO_IPV4_MAX_SIZE", "GRO_IPV4_MAX_SIZE", "DPLL_PIN",
    "MAX_PACING_OFFLOAD_HORIZON", "NETNS_IMMUTABLE", "HEADROOM", "TAILROOM", "PACING_OFFLOAD",
};

static int nlsock(void) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (s < 0) {
        printf("socket: %s\n", strerror(errno));
        exit(1);
    }
    return s;
}

// dump を送り、答えの 1 つずつに f (DONE かエラーで終わり。エラーなら errno を出す)
static void dump(int s, int type, const void *hdr, int hlen, void (*f)(struct nlmsghdr *)) {
    struct {
        struct nlmsghdr h;
        char body[64];
    } req;
    memset(&req, 0, sizeof req);
    req.h.nlmsg_len = NLMSG_LENGTH(hlen);
    req.h.nlmsg_type = type;
    req.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    req.h.nlmsg_seq = seqno++;
    memcpy(req.body, hdr, hlen);
    if (send(s, &req, req.h.nlmsg_len, 0) < 0) {
        printf("  send: %s\n", strerror(errno));
        return;
    }
    for (;;) {
        int n = recv(s, buf, sizeof buf, 0);
        if (n <= 0) {
            printf("  recv: %s\n", n < 0 ? strerror(errno) : "eof");
            return;
        }
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, (unsigned)n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE) return;
            if (h->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *e = NLMSG_DATA(h);
                printf("  error: %s\n", e->error ? strerror(-e->error) : "ack");
                return;
            }
            f(h);
        }
    }
}

static void hex(const unsigned char *p, int n) {
    for (int i = 0; i < n; i++) printf("%s%02x", i ? ":" : "", p[i]);
}

// lo と eth0 のほかは見ない
static int wanted(int index) {
    char name[IF_NAMESIZE];
    if (!if_indextoname(index, name)) return 0;
    return !strcmp(name, "lo") || !strcmp(name, "eth0");
}

static void show_link(struct nlmsghdr *h) {
    if (h->nlmsg_type != RTM_NEWLINK) return;
    struct ifinfomsg *i = NLMSG_DATA(h);
    if (!wanted(i->ifi_index)) return;
    char name[IF_NAMESIZE];
    if_indextoname(i->ifi_index, name);
    printf("link %s: type %u flags 0x%x change 0x%x\n", name, i->ifi_type, i->ifi_flags, i->ifi_change);
    int len = IFLA_PAYLOAD(h);
    for (struct rtattr *a = IFLA_RTA(i); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        int t = a->rta_type & ~NLA_F_NESTED;
        int n = RTA_PAYLOAD(a);
        const unsigned char *d = RTA_DATA(a);
        printf("  %s (%d) len %d", t < (int)(sizeof ifla / sizeof *ifla) ? ifla[t] : "?", t, n);
        switch (t) {
        case IFLA_IFNAME:
        case IFLA_QDISC:
        case IFLA_PARENT_DEV_BUS_NAME:
            printf(": %.*s", n, d);
            break;
        case IFLA_PARENT_DEV_NAME:
            // virtioN の番号は VM の装置の数で変わる
            printf(": ");
            for (int j = 0; j < n && d[j]; j++) putchar(d[j] >= '0' && d[j] <= '9' ? 'N' : d[j]);
            break;
        case IFLA_ADDRESS:
        case IFLA_BROADCAST:
        case IFLA_PERM_ADDRESS:
            printf(": ");
            hex(d, n);
            break;
        case IFLA_STATS:
        case IFLA_STATS64:
            break;
        case IFLA_MAP:
            printf(": ");
            hex(d, n);
            break;
        case IFLA_AF_SPEC:
        case IFLA_XDP:
        case IFLA_PROP_LIST: {
            // 中の属性の型 (と、その中のもの) だけ
            printf(":");
            int m = n;
            for (struct rtattr *b = (struct rtattr *)d; RTA_OK(b, m); b = RTA_NEXT(b, m)) {
                printf(" %d", b->rta_type & ~NLA_F_NESTED);
                if (t == IFLA_AF_SPEC) {
                    // 家族ごとの属性の型と長さ。値は conf (u32 の並び)、IPv6 の flags、token、addr_gen_mode だけ
                    // (統計と cacheinfo の時刻・reachable_time は動く)
                    int fam = b->rta_type & ~NLA_F_NESTED;
                    printf("[");
                    int k = RTA_PAYLOAD(b);
                    for (struct rtattr *c = RTA_DATA(b); RTA_OK(c, k); c = RTA_NEXT(c, k)) {
                        int ct = c->rta_type & ~NLA_F_NESTED, cn = RTA_PAYLOAD(c);
                        const unsigned char *cd = RTA_DATA(c);
                        printf(" %d/%d", ct, cn);
                        int conf = (fam == AF_INET && ct == 1) || (fam == AF_INET6 && ct == 2);
                        if (conf) {
                            printf("{");
                            for (int j = 0; j + 4 <= cn; j += 4) printf("%s%d", j ? "," : "", *(int *)(cd + j));
                            printf("}");
                        } else if (fam == AF_INET6 && ct == 1) {
                            // IF_RA_RCVD (0x20) は RS のあとに RA が来たかで変わる (QEMU の RA が RS より先なら来ない)
                            printf("=0x%x", *(unsigned *)cd & ~0x20u);
                        } else if (fam == AF_INET6 && ct == 8) {
                            printf("=0x%x", cd[0]);
                        } else if (fam == AF_INET6 && ct == 7) {
                            printf("=");
                            hex(cd, cn);
                        }
                    }
                    printf(" ]");
                } else if (RTA_PAYLOAD(b) == 1) {
                    printf("=%u", *(unsigned char *)RTA_DATA(b));
                } else if (RTA_PAYLOAD(b) == 4) {
                    printf("=%u", *(unsigned *)RTA_DATA(b));
                }
            }
            break;
        }
        default:
            if (n == 1) printf(": %u", d[0]);
            else if (n == 2) printf(": %u", *(unsigned short *)d);
            else if (n == 4) printf(": %u", *(unsigned *)d);
            else if (n == 8) printf(": %llu", *(unsigned long long *)d);
        }
        printf("\n");
    }
}

static void show_mcast(struct nlmsghdr *h) {
    struct ifaddrmsg *a = NLMSG_DATA(h);
    if (!wanted(a->ifa_index)) return;
    char name[IF_NAMESIZE];
    if_indextoname(a->ifa_index, name);
    printf("  %s type %d family %d prefixlen %d flags 0x%x scope %d:", name, h->nlmsg_type, a->ifa_family, a->ifa_prefixlen, a->ifa_flags, a->ifa_scope);
    int len = IFA_PAYLOAD(h);
    for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
        int n = RTA_PAYLOAD(r);
        const unsigned char *d = RTA_DATA(r);
        switch (r->rta_type) {
        case IFA_MULTICAST: {
            char ip[64];
            if (a->ifa_family == AF_INET || a->ifa_family == AF_INET6) {
                inet_ntop(a->ifa_family, d, ip, sizeof ip);
                printf(" multicast %s", ip);
            } else {
                printf(" multicast ");
                hex(d, n);
            }
            break;
        }
        case IFA_MC_USERS:
            printf(" users %u", *(unsigned *)d);
            break;
        case IFA_FLAGS:
            printf(" flags 0x%x", *(unsigned *)d);
            break;
        case IFA_CACHEINFO: {
            const struct ifa_cacheinfo *c = (const void *)d;
            printf(" cacheinfo(%s %s)", c->ifa_prefered == 0xffffffff ? "forever" : "finite", c->ifa_valid == 0xffffffff ? "forever" : "finite");
            break;
        }
        default:
            printf(" attr%d(%d)", r->rta_type, n);
        }
    }
    printf("\n");
}

// eth0 に fec0::/64 の番地ができるまで (15 秒まで。solicited-node の組の users が 2 になる)
static int wait_slaac(void) {
    for (int i = 0; i < 150; i++) {
        struct ifaddrs *ifs;
        if (getifaddrs(&ifs) == 0) {
            int ok = 0;
            for (struct ifaddrs *p = ifs; p; p = p->ifa_next) {
                if (!p->ifa_addr || p->ifa_addr->sa_family != AF_INET6) continue;
                const unsigned char *b = ((struct sockaddr_in6 *)p->ifa_addr)->sin6_addr.s6_addr;
                if (b[0] == 0xfe && b[1] == 0xc0) ok = 1;
            }
            freeifaddrs(ifs);
            if (ok) return 1;
        }
        struct timespec ts = {0, 100000000};
        nanosleep(&ts, NULL);
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("slaac: %s\n", wait_slaac() ? "ok" : "no fec0:: address");
    int s = nlsock();
    struct ifinfomsg li = {.ifi_family = AF_UNSPEC};
    printf("-- RTM_GETLINK\n");
    dump(s, RTM_GETLINK, &li, sizeof li, show_link);
    int fams[] = {AF_INET, AF_INET6, AF_PACKET, AF_UNSPEC};
    for (int k = 0; k < 4; k++) {
        printf("-- RTM_GETMULTICAST family %d\n", fams[k]);
        struct ifaddrmsg am = {.ifa_family = fams[k]};
        dump(s, RTM_GETMULTICAST, &am, sizeof am, show_mcast);
    }
    close(s);
    return 0;
}
