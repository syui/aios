// netlink (kernel/src/netlink.rs): AF_NETLINK のソケット (型と protocol、ポート、bind、getsockname、データグラムの受け方と
// MSG_TRUNC) と NETLINK_ROUTE の口とアドレスの一覧 (RTM_GETLINK と RTM_GETADDR の dump、1 つの RTM_GETLINK、NLMSG_DONE、
// NLMSG_ERROR と ACK、NETLINK_CAP_ACK)、それを使う getifaddrs と if_nameindex。lo のことだけを見る (aios の eth0 と、
// 手本の Linux の sit0 は見ない)。root でない人で (Linux は test/linux-vm.sh exec の nobody)。
// aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o netlink test/netlink.c; ./netlink。Linux でも同じ答え
#define _GNU_SOURCE
#include <errno.h>
#include <ifaddrs.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef IFF_LOWER_UP
#define IFF_LOWER_UP 0x10000
#endif

static int fails, checks;

static void check(const char *what, long got, long want) {
    checks++;
    if (got != want) {
        fails++;
        printf("FAIL %s: got %ld, want %ld\n", what, got, want);
    }
}

static int nlsock(void) {
    return socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
}

static unsigned portid(int s) {
    struct sockaddr_nl a;
    socklen_t l = sizeof a;
    memset(&a, 0xff, sizeof a);
    if (getsockname(s, (struct sockaddr *)&a, &l) != 0)
        return 0xdead;
    return a.nl_pid;
}

// 頭と中身 (len バイト) を送る
static long req(int s, int type, int flags, unsigned seq, const void *body, size_t len) {
    char b[256];
    struct nlmsghdr *h = (struct nlmsghdr *)b;
    memset(b, 0, sizeof b);
    h->nlmsg_len = NLMSG_LENGTH(len);
    h->nlmsg_type = type;
    h->nlmsg_flags = flags;
    h->nlmsg_seq = seq;
    memcpy(NLMSG_DATA(h), body, len);
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    return sendto(s, b, h->nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
}

static char buf[65536];

// 待たずに受ける (答えは送ったときにもうある)
static long take(int s) {
    return recv(s, buf, sizeof buf, MSG_DONTWAIT);
}

static struct rtattr *rta(void *start, int len, int type) {
    for (struct rtattr *a = start; RTA_OK(a, len); a = RTA_NEXT(a, len))
        if (a->rta_type == type)
            return a;
    return NULL;
}

static void sock_types(void) {
    int s = socket(AF_NETLINK, SOCK_STREAM, NETLINK_ROUTE);
    check("SOCK_STREAM", s < 0 ? errno : 0, ESOCKTNOSUPPORT);
    s = socket(AF_NETLINK, SOCK_RAW, 40);
    check("protocol 40", s < 0 ? errno : 0, EPROTONOSUPPORT);
    s = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_ROUTE);
    check("SOCK_DGRAM", s >= 0, 1);
    int v = -1;
    socklen_t l = sizeof v;
    getsockopt(s, SOL_SOCKET, SO_TYPE, &v, &l);
    check("SO_TYPE", v, SOCK_DGRAM);
    getsockopt(s, SOL_SOCKET, SO_DOMAIN, &v, &l);
    check("SO_DOMAIN", v, AF_NETLINK);
    getsockopt(s, SOL_SOCKET, SO_PROTOCOL, &v, &l);
    check("SO_PROTOCOL", v, NETLINK_ROUTE);
    close(s);
}

static void ports(void) {
    int a = nlsock(), b = nlsock(), c = nlsock();
    struct sockaddr_nl n = {.nl_family = AF_NETLINK};
    check("portid before bind", portid(a), 0);
    check("bind 0", bind(a, (struct sockaddr *)&n, sizeof n), 0);
    check("bind 0 takes the pid", portid(a), getpid());
    check("second bind 0", bind(b, (struct sockaddr *)&n, sizeof n), 0);
    check("second portid is negative", (int)portid(b) < 0, 1);
    n.nl_pid = getpid();
    check("bind a used portid", bind(c, (struct sockaddr *)&n, sizeof n) < 0 ? errno : 0, EADDRINUSE);
    n.nl_pid = getpid() + 7;
    check("rebind to another portid", bind(a, (struct sockaddr *)&n, sizeof n) < 0 ? errno : 0, EINVAL);
    check("short name", bind(c, (struct sockaddr *)&n, 8) < 0 ? errno : 0, EINVAL);
    // 送ると、まだならポートが決まる
    struct rtgenmsg g = {AF_PACKET};
    check("send autobinds", req(c, RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, &g, sizeof g) > 0, 1);
    check("autobound", portid(c) != 0, 1);
    close(a);
    close(b);
    close(c);
}

// RTM_GETLINK の dump: lo (1、ARPHRD_LOOPBACK、UP と LOOPBACK と RUNNING と LOWER_UP、名前、MTU、アドレス)
static void dump_links(void) {
    int s = nlsock();
    struct rtgenmsg g = {AF_PACKET};
    check("GETLINK dump sent", req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 77, &g, sizeof g) > 0, 1);
    unsigned me = portid(s);
    int lo = 0, done = 0, bad = 0;
    while (!done) {
        long n = take(s);
        if (n <= 0)
            break;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_seq != 77 || h->nlmsg_pid != me || !(h->nlmsg_flags & NLM_F_MULTI))
                bad++;
            if (h->nlmsg_type == NLMSG_DONE) {
                check("DONE error", *(int *)NLMSG_DATA(h), 0);
                done = 1;
                break;
            }
            if (h->nlmsg_type != RTM_NEWLINK) {
                bad++;
                continue;
            }
            struct ifinfomsg *i = NLMSG_DATA(h);
            if (i->ifi_index != 1)
                continue;
            lo = 1;
            int len = IFLA_PAYLOAD(h);
            check("lo type", i->ifi_type, 772);
            check("lo flags", i->ifi_flags & (IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP), IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP);
            struct rtattr *a = rta(IFLA_RTA(i), len, IFLA_IFNAME);
            check("lo IFLA_IFNAME", a && !strcmp(RTA_DATA(a), "lo"), 1);
            a = rta(IFLA_RTA(i), len, IFLA_MTU);
            check("lo IFLA_MTU", a ? *(unsigned *)RTA_DATA(a) : 0, 65536);
            a = rta(IFLA_RTA(i), len, IFLA_ADDRESS);
            static const char zero[6];
            check("lo IFLA_ADDRESS", a && RTA_PAYLOAD(a) == 6 && !memcmp(RTA_DATA(a), zero, 6), 1);
            check("lo IFLA_STATS64", rta(IFLA_RTA(i), len, IFLA_STATS64) != NULL, 1);
            check("lo IFLA_STATS", rta(IFLA_RTA(i), len, IFLA_STATS) != NULL, 1);
            a = rta(IFLA_RTA(i), len, IFLA_TXQLEN);
            check("lo IFLA_TXQLEN", a ? *(unsigned *)RTA_DATA(a) : 0, 1000);
        }
    }
    check("GETLINK dump ends", done, 1);
    check("GETLINK dump has lo", lo, 1);
    check("GETLINK dump headers", bad, 0);
    check("nothing after DONE", take(s) < 0 ? errno : 0, EAGAIN);
    close(s);
}

// RTM_GETADDR の dump (AF_INET): lo の 127.0.0.1/8、scope host、ブロードキャストなし
static void dump_addrs(void) {
    int s = nlsock();
    struct rtgenmsg g = {AF_INET};
    req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 5, &g, sizeof g);
    int lo = 0, done = 0;
    while (!done) {
        long n = take(s);
        if (n <= 0)
            break;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE) {
                done = 1;
                break;
            }
            struct ifaddrmsg *a = NLMSG_DATA(h);
            if (h->nlmsg_type != RTM_NEWADDR || a->ifa_index != 1)
                continue;
            lo = 1;
            int len = IFA_PAYLOAD(h);
            check("lo family", a->ifa_family, AF_INET);
            check("lo prefixlen", a->ifa_prefixlen, 8);
            check("lo scope", a->ifa_scope, RT_SCOPE_HOST);
            check("lo IFA_F_PERMANENT", a->ifa_flags & IFA_F_PERMANENT, IFA_F_PERMANENT);
            struct rtattr *x = rta(IFA_RTA(a), len, IFA_LOCAL);
            check("lo IFA_LOCAL", x ? *(unsigned *)RTA_DATA(x) : 0, htonl(INADDR_LOOPBACK));
            x = rta(IFA_RTA(a), len, IFA_ADDRESS);
            check("lo IFA_ADDRESS", x ? *(unsigned *)RTA_DATA(x) : 0, htonl(INADDR_LOOPBACK));
            x = rta(IFA_RTA(a), len, IFA_LABEL);
            check("lo IFA_LABEL", x && !strcmp(RTA_DATA(x), "lo"), 1);
            check("lo no IFA_BROADCAST", rta(IFA_RTA(a), len, IFA_BROADCAST) == NULL, 1);
            x = rta(IFA_RTA(a), len, IFA_CACHEINFO);
            check("lo IFA_CACHEINFO valid", x ? ((struct ifa_cacheinfo *)RTA_DATA(x))->ifa_valid : 0, 0xffffffffu);
        }
    }
    check("GETADDR dump ends", done, 1);
    check("GETADDR dump has lo", lo, 1);
    close(s);
}

// 1 つの RTM_GETLINK (番号で、名前で、ないもの)、知らない type、NOOP の ACK、REQUEST のないもの、root でない人の NEWADDR
static void single(void) {
    int s = nlsock();
    struct ifinfomsg i = {.ifi_family = AF_UNSPEC, .ifi_index = 1};
    req(s, RTM_GETLINK, NLM_F_REQUEST, 9, &i, sizeof i);
    long n = take(s);
    struct nlmsghdr *h = (struct nlmsghdr *)buf;
    check("GETLINK 1 type", n > 0 ? h->nlmsg_type : -1, RTM_NEWLINK);
    check("GETLINK 1 flags", n > 0 ? h->nlmsg_flags : -1, 0);
    check("GETLINK 1 index", n > 0 ? ((struct ifinfomsg *)NLMSG_DATA(h))->ifi_index : -1, 1);
    check("GETLINK 1 is one message", n > 0 ? (long)NLMSG_ALIGN(h->nlmsg_len) : -1, n);
    // 名前で
    char b[64];
    memset(b, 0, sizeof b);
    struct ifinfomsg *j = (struct ifinfomsg *)b;
    j->ifi_family = AF_UNSPEC;
    struct rtattr *a = (struct rtattr *)(b + NLMSG_ALIGN(sizeof *j));
    a->rta_type = IFLA_IFNAME;
    a->rta_len = RTA_LENGTH(3);
    memcpy(RTA_DATA(a), "lo", 3);
    req(s, RTM_GETLINK, NLM_F_REQUEST, 10, b, NLMSG_ALIGN(sizeof *j) + RTA_ALIGN(a->rta_len));
    n = take(s);
    check("GETLINK by name", n > 0 ? ((struct ifinfomsg *)NLMSG_DATA(h))->ifi_index : -1, 1);
    // ないもの: NLMSG_ERROR で ENODEV と、もとのメッセージまるごと
    i.ifi_index = 999;
    req(s, RTM_GETLINK, NLM_F_REQUEST, 11, &i, sizeof i);
    n = take(s);
    struct nlmsgerr *e = NLMSG_DATA(h);
    check("GETLINK 999 is an error", n > 0 ? h->nlmsg_type : -1, NLMSG_ERROR);
    check("GETLINK 999 errno", n > 0 ? e->error : 1, -ENODEV);
    check("GETLINK 999 seq", n > 0 ? h->nlmsg_seq : 0, 11);
    check("error carries the request", n > 0 ? (long)h->nlmsg_len : -1, NLMSG_LENGTH(sizeof *e - sizeof e->msg) + NLMSG_LENGTH(sizeof i));
    check("error request type", n > 0 ? e->msg.nlmsg_type : -1, RTM_GETLINK);
    // NETLINK_CAP_ACK: しくじっても頭だけ (NLM_F_CAPPED)
    int one = 1;
    check("NETLINK_CAP_ACK", setsockopt(s, SOL_NETLINK, NETLINK_CAP_ACK, &one, sizeof one), 0);
    req(s, RTM_GETLINK, NLM_F_REQUEST, 12, &i, sizeof i);
    n = take(s);
    check("capped error length", n > 0 ? (long)h->nlmsg_len : -1, NLMSG_LENGTH(sizeof *e));
    check("capped flag", n > 0 ? h->nlmsg_flags & NLM_F_CAPPED : -1, NLM_F_CAPPED);
    one = 0;
    setsockopt(s, SOL_NETLINK, NETLINK_CAP_ACK, &one, sizeof one);
    // 知らない type
    req(s, 200, NLM_F_REQUEST | NLM_F_ACK, 13, &i, sizeof i);
    n = take(s);
    check("type 200", n > 0 ? e->error : 1, -EOPNOTSUPP);
    // NOOP (制御) に ACK: エラー 0 で頭だけ
    req(s, NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 14, &i, sizeof i);
    n = take(s);
    check("NOOP ack type", n > 0 ? h->nlmsg_type : -1, NLMSG_ERROR);
    check("NOOP ack errno", n > 0 ? e->error : 1, 0);
    check("ack length", n > 0 ? (long)h->nlmsg_len : -1, NLMSG_LENGTH(sizeof *e));
    // REQUEST のないものには答えない
    req(s, RTM_GETLINK, 0, 15, &i, sizeof i);
    check("no REQUEST, no answer", take(s) < 0 ? errno : 0, EAGAIN);
    // 短いもの (rtgenmsg もない) はだまって捨てる
    req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 16, &i, 0);
    check("empty body, no answer", take(s) < 0 ? errno : 0, EAGAIN);
    // root でない人は変えられない
    struct ifaddrmsg ad = {.ifa_family = AF_INET, .ifa_prefixlen = 8, .ifa_index = 1};
    req(s, RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, 17, &ad, sizeof ad);
    n = take(s);
    check("NEWADDR as a user", n > 0 ? e->error : 1, -EPERM);
    // ほかのポートへは送れない (root だけ)
    struct sockaddr_nl to = {.nl_family = AF_NETLINK, .nl_pid = 12345};
    check("send to a port", sendto(s, b, 16, 0, (struct sockaddr *)&to, sizeof to) < 0 ? errno : 0, EPERM);
    close(s);
}

// 受ける: データグラムごと。短い入れ物なら切って MSG_TRUNC (残りは捨てる)、MSG_TRUNC なら本当の長さ。dump は中身と
// NLMSG_DONE の 2 つのデータグラム
static void truncated(void) {
    int s = nlsock();
    struct rtgenmsg g = {AF_INET};
    req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 3, &g, sizeof g);
    long full = recv(s, buf, sizeof buf, MSG_DONTWAIT | MSG_PEEK | MSG_TRUNC);
    check("peek full length", full > 20, 1);
    long t = recv(s, buf, 20, MSG_DONTWAIT | MSG_PEEK | MSG_TRUNC);
    check("MSG_TRUNC gives the real length", t, full);
    struct iovec iov = {buf, 20};
    struct sockaddr_nl from;
    struct msghdr m = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &iov, .msg_iovlen = 1};
    long k = recvmsg(s, &m, MSG_DONTWAIT);
    check("short read", k, 20);
    check("msg_flags MSG_TRUNC", m.msg_flags & MSG_TRUNC, MSG_TRUNC);
    check("from the kernel", from.nl_pid, 0);
    check("name length", m.msg_namelen, sizeof(struct sockaddr_nl));
    // そのデータグラムの残りは捨てられ、次は NLMSG_DONE のデータグラム (Linux の dump は、中身のあとに DONE を別に)
    long d = take(s);
    check("next is DONE", d > 0 ? ((struct nlmsghdr *)buf)->nlmsg_type : -1, NLMSG_DONE);
    check("DONE alone", d, NLMSG_LENGTH(sizeof(int)));
    check("then nothing", take(s) < 0 ? errno : 0, EAGAIN);
    close(s);
}

// getifaddrs と if_nameindex (libc が netlink で聞く)
static void libc_side(void) {
    struct ifaddrs *a;
    check("getifaddrs", getifaddrs(&a), 0);
    int lo4 = 0;
    for (struct ifaddrs *p = a; p; p = p->ifa_next)
        if (!strcmp(p->ifa_name, "lo") && p->ifa_addr && p->ifa_addr->sa_family == AF_INET) {
            lo4 = 1;
            check("lo addr", ((struct sockaddr_in *)p->ifa_addr)->sin_addr.s_addr, htonl(INADDR_LOOPBACK));
            check("lo netmask", ((struct sockaddr_in *)p->ifa_netmask)->sin_addr.s_addr, htonl(0xff000000));
            check("lo ifa_flags", p->ifa_flags & (IFF_UP | IFF_LOOPBACK | IFF_RUNNING), IFF_UP | IFF_LOOPBACK | IFF_RUNNING);
        }
    check("getifaddrs has lo", lo4, 1);
    freeifaddrs(a);
    struct if_nameindex *ni = if_nameindex();
    check("if_nameindex", ni != NULL, 1);
    check("if_nameindex lo", ni && ni[0].if_index == 1 && !strcmp(ni[0].if_name, "lo"), 1);
    if (ni)
        if_freenameindex(ni);
    check("if_nametoindex lo", if_nametoindex("lo"), 1);
}

int main(void) {
    sock_types();
    ports();
    dump_links();
    dump_addrs();
    single();
    truncated();
    libc_side();
    printf("netlink: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
