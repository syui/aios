// 近所さがし (ARP と IPv6 の NS) があて先ごとか、と、しくじったときに Linux と同じになるか。近くにいない 3 つずつの
// あて先 (QEMU のユーザーネットワークが答えるのは自分の番地だけ: IPv4 は 10.0.2.200〜202、IPv6 は SLAAC の fec0::/64 の
// fec0::200〜202) へ、別々のソケットで続けて送る:
//   .200 はつないだ UDP、.201 は TCP の connect (待たない)、.202 は raw の ICMP の echo (ping)
// - 0.3 秒あと、近くの番地の表 (RTM_GETNEIGH) で .200 と .201 が両方 INCOMPLETE (Linux はあて先ごとに問い合わせる。
//   smoltcp 0.12 は 1 つ問い合わせると、どのあて先にも 1 秒は問い合わせず、1 つ目を問い合わせなおすたびにまただまるので、
//   ほかはいつまでも問い合わせてもらえなかった: todo の smoltcp-nd-silence)
// - 3 度 (1 秒ごと) 問い合わせて答えがなければ FAILED にして、待っていたパケットを捨て、自分から自分へ「たどりつけない」
//   (ICMP の host unreachable、ICMPv6 の address unreachable) を返す (neigh_invalidate → ipv4_link_failure /
//   ip6_link_failure)。TCP の connect は 3 秒ほどで EHOSTUNREACH、raw のソケット (ping) はその ICMP を受け、つないだ
//   UDP は EHOSTUNREACH、5 秒あとの表は FAILED (smoltcp は問い合わせつづけ、パケットはソケットの列の先にのこっていた:
//   todo の neigh-failed)
// 出すだけのテスト (root で: raw のソケット): 手本の Linux は ip=dhcp の virtio-net (test/ldiff.sh --root --net ndsilence)
//   aios の中で: sudo ./ndsilence      aarch64-linux-gnu-gcc -O2 -static -o ndsilence test/ndsilence.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/if_addr.h>
#include <linux/neighbour.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
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
static struct timespec t0;
// 思ったとおりでなかった数 (ctests の終わりの番号)
static int bad;

static const char *state_name(int s) {
    switch (s) {
    case NUD_INCOMPLETE: return "INCOMPLETE";
    case NUD_REACHABLE: return "REACHABLE";
    case NUD_STALE: return "STALE";
    case NUD_DELAY: return "DELAY";
    case NUD_PROBE: return "PROBE";
    case NUD_FAILED: return "FAILED";
    case NUD_NOARP: return "NOARP";
    case NUD_PERMANENT: return "PERMANENT";
    default: return "?";
    }
}

static long ms_since_t0(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0.tv_sec) * 1000 + (t.tv_nsec - t0.tv_nsec) / 1000000;
}

// t0 から at ミリ秒まで眠る
static void sleep_until(long at) {
    long left = at - ms_since_t0();
    if (left <= 0) return;
    struct timespec t = {left / 1000, (left % 1000) * 1000000};
    nanosleep(&t, NULL);
}

static const char *ename(int e) {
    switch (e) {
    case 0: return "ok";
    case EHOSTUNREACH: return "EHOSTUNREACH";
    case ENETUNREACH: return "ENETUNREACH";
    case ECONNREFUSED: return "ECONNREFUSED";
    case ETIMEDOUT: return "ETIMEDOUT";
    case EINPROGRESS: return "EINPROGRESS";
    case EAGAIN: return "EAGAIN";
    default: return strerror(e);
    }
}

// family の近くの番地の表から、want の 2 つ (len バイト) の状態を出す。want_state でないものは bad に
static void show(const char *what, int family, const unsigned char want[][16], int len, const char *want_state) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (s < 0) {
        printf("%s: socket: %s\n", what, strerror(errno));
        return;
    }
    struct {
        struct nlmsghdr h;
        struct ndmsg n;
    } req = {{sizeof req, RTM_GETNEIGH, NLM_F_REQUEST | NLM_F_DUMP, 1, 0}, {.ndm_family = family}};
    if (send(s, &req, sizeof req, 0) < 0) {
        printf("%s: send: %s\n", what, strerror(errno));
        close(s);
        return;
    }
    const char *st[2] = {"(none)", "(none)"};
    for (int done = 0; !done;) {
        int n = recv(s, buf, sizeof buf, 0);
        if (n <= 0) break;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, (unsigned)n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR) {
                done = 1;
                break;
            }
            if (h->nlmsg_type != RTM_NEWNEIGH) continue;
            struct ndmsg *nd = NLMSG_DATA(h);
            int alen = h->nlmsg_len - NLMSG_LENGTH(sizeof *nd);
            for (struct rtattr *a = (struct rtattr *)((char *)nd + NLMSG_ALIGN(sizeof *nd)); RTA_OK(a, alen); a = RTA_NEXT(a, alen)) {
                if (a->rta_type != NDA_DST || RTA_PAYLOAD(a) != (unsigned)len) continue;
                for (int i = 0; i < 2; i++)
                    if (!memcmp(RTA_DATA(a), want[i], len)) st[i] = state_name(nd->ndm_state);
            }
        }
    }
    close(s);
    char a0[64], a1[64];
    inet_ntop(family, want[0], a0, sizeof a0);
    inet_ntop(family, want[1], a1, sizeof a1);
    printf("%s: %s %s, %s %s\n", what, a0, st[0], a1, st[1]);
    bad += !!strcmp(st[0], want_state) + !!strcmp(st[1], want_state);
}

static socklen_t sa_of(int family, const unsigned char *a, int port, struct sockaddr_storage *ss) {
    memset(ss, 0, sizeof *ss);
    if (family == AF_INET) {
        struct sockaddr_in *s = (struct sockaddr_in *)ss;
        s->sin_family = AF_INET;
        s->sin_port = htons(port);
        memcpy(&s->sin_addr, a, 4);
        return sizeof *s;
    }
    struct sockaddr_in6 *s = (struct sockaddr_in6 *)ss;
    s->sin6_family = AF_INET6;
    s->sin6_port = htons(port);
    memcpy(&s->sin6_addr, a, 16);
    return sizeof *s;
}

// eth0 に fec0::/64 の番地ができて、DAD が終わる (IFA_F_TENTATIVE がとれる) まで (15 秒まで)。Linux は DAD のあいだの
// 番地を送り元にしないので、終わる前に送ると fe80:: から出る
static int slaac_ready(void) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (s < 0) return 0;
    struct {
        struct nlmsghdr h;
        struct ifaddrmsg a;
    } req = {{sizeof req, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 1, 0}, {.ifa_family = AF_INET6}};
    int ok = 0;
    if (send(s, &req, sizeof req, 0) == sizeof req) {
        for (int done = 0; !done;) {
            int n = recv(s, buf, sizeof buf, 0);
            if (n <= 0) break;
            for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, (unsigned)n); h = NLMSG_NEXT(h, n)) {
                if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR) {
                    done = 1;
                    break;
                }
                struct ifaddrmsg *a = NLMSG_DATA(h);
                unsigned flags = a->ifa_flags;
                const unsigned char *addr = NULL;
                int alen = IFA_PAYLOAD(h);
                for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, alen); r = RTA_NEXT(r, alen)) {
                    if (r->rta_type == IFA_ADDRESS) addr = RTA_DATA(r);
                    if (r->rta_type == IFA_FLAGS) flags = *(unsigned *)RTA_DATA(r);
                }
                if (addr && addr[0] == 0xfe && addr[1] == 0xc0 && !(flags & IFA_F_TENTATIVE)) ok = 1;
            }
        }
    }
    close(s);
    return ok;
}

static int wait_slaac(void) {
    for (int i = 0; i < 150; i++) {
        if (slaac_ready()) return 1;
        struct timespec t = {0, 100000000};
        nanosleep(&t, NULL);
    }
    return 0;
}

struct fam {
    const char *name;
    int family;
    unsigned char a[3][16];
    int udp, tcp, raw;
    long tcp_ms;
    int tcp_err;
};

static void start(struct fam *f) {
    struct sockaddr_storage ss;
    socklen_t sl;
    // .200: つないだ UDP
    f->udp = socket(f->family, SOCK_DGRAM, 0);
    sl = sa_of(f->family, f->a[0], 9, &ss);
    if (connect(f->udp, (struct sockaddr *)&ss, sl) < 0 || send(f->udp, "x", 1, 0) != 1) printf("%s udp: %s\n", f->name, ename(errno));
    // .201: TCP の connect (待たない)
    f->tcp = socket(f->family, SOCK_STREAM | SOCK_NONBLOCK, 0);
    sl = sa_of(f->family, f->a[1], 80, &ss);
    int r = connect(f->tcp, (struct sockaddr *)&ss, sl);
    if (r == 0 || errno != EINPROGRESS) printf("%s tcp connect: %s\n", f->name, r == 0 ? "ok" : ename(errno));
    f->tcp_ms = -1;
    // .202: raw の ICMP の echo
    if (f->family == AF_INET) {
        f->raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
        unsigned char e[16] = {8, 0, 0, 0, 0x12, 0x34, 0, 1, 'n', 'd', 's', 'i', 'l', 'e', 'n', 'c'};
        unsigned sum = 0;
        for (int i = 0; i < 16; i += 2) sum += e[i] << 8 | e[i + 1];
        while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
        e[2] = ~sum >> 8, e[3] = ~sum & 0xff;
        sl = sa_of(AF_INET, f->a[2], 0, &ss);
        if (sendto(f->raw, e, sizeof e, 0, (struct sockaddr *)&ss, sl) != sizeof e) printf("%s raw: %s\n", f->name, ename(errno));
    } else {
        f->raw = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
        struct icmp6_filter flt;
        ICMP6_FILTER_SETBLOCKALL(&flt);
        ICMP6_FILTER_SETPASS(ICMP6_ECHO_REPLY, &flt);
        ICMP6_FILTER_SETPASS(ICMP6_DST_UNREACH, &flt);
        setsockopt(f->raw, IPPROTO_ICMPV6, ICMP6_FILTER, &flt, sizeof flt);
        unsigned char e[16] = {ICMP6_ECHO_REQUEST, 0, 0, 0, 0x12, 0x34, 0, 1, 'n', 'd', 's', 'i', 'l', 'e', 'n', 'c'};
        sl = sa_of(AF_INET6, f->a[2], 0, &ss);
        if (sendto(f->raw, e, sizeof e, 0, (struct sockaddr *)&ss, sl) != sizeof e) printf("%s raw: %s\n", f->name, ename(errno));
    }
}

// raw のソケットに来たもの (6 秒まで、みな): type と code、送り元、中に写されたあて先をならべて (どれが先に来るかは時間しだい)
static int cmp_str(const void *a, const void *b) {
    return strcmp((const char *)a, (const char *)b);
}

static void raw_result(struct fam *f) {
    char got[8][160];
    int n = 0;
    for (;;) {
        struct pollfd p = {f->raw, POLLIN, 0};
        long left = 6000 - ms_since_t0();
        if (n == 8 || poll(&p, 1, left > 0 ? left : 0) <= 0) break;
        struct sockaddr_storage from;
        socklen_t fl = sizeof from;
        long len = recvfrom(f->raw, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl);
        char src[64] = "?", about[64] = "?";
        const unsigned char *icmp = (const unsigned char *)buf;
        if (f->family == AF_INET) {
            inet_ntop(AF_INET, &((struct sockaddr_in *)&from)->sin_addr, src, sizeof src);
            int ihl = (buf[0] & 15) * 4;
            icmp += ihl;
            if (len >= ihl + 8 + 20) inet_ntop(AF_INET, icmp + 8 + 16, about, sizeof about);
        } else {
            inet_ntop(AF_INET6, &((struct sockaddr_in6 *)&from)->sin6_addr, src, sizeof src);
            if (len >= 8 + 40) inet_ntop(AF_INET6, icmp + 8 + 24, about, sizeof about);
        }
        snprintf(got[n++], sizeof got[0], "type %u code %u from %s about %s", icmp[0], icmp[1], src, about);
    }
    qsort(got, n, sizeof got[0], cmp_str);
    printf("%s raw: %d\n", f->name, n);
    bad += n != 3;
    for (int i = 0; i < n; i++) printf("  %s\n", got[i]);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    struct fam fs[2] = {{"ipv4", AF_INET, {{10, 0, 2, 200}, {10, 0, 2, 201}, {10, 0, 2, 202}}}, {"ipv6", AF_INET6}};
    inet_pton(AF_INET6, "fec0::200", fs[1].a[0]);
    inet_pton(AF_INET6, "fec0::201", fs[1].a[1]);
    inet_pton(AF_INET6, "fec0::202", fs[1].a[2]);
    printf("slaac: %s\n", wait_slaac() ? "ok" : "no fec0:: address");
    clock_gettime(CLOCK_MONOTONIC, &t0);
    start(&fs[0]);
    start(&fs[1]);
    sleep_until(300);
    show("ipv4 after 0.3s", AF_INET, fs[0].a, 4, "INCOMPLETE");
    show("ipv6 after 0.3s", AF_INET6, fs[1].a, 16, "INCOMPLETE");
    // TCP の connect が終わるのを (6 秒まで) 待つ
    for (;;) {
        struct pollfd p[2] = {{fs[0].tcp_ms < 0 ? fs[0].tcp : -1, POLLOUT, 0}, {fs[1].tcp_ms < 0 ? fs[1].tcp : -1, POLLOUT, 0}};
        if (p[0].fd < 0 && p[1].fd < 0) break;
        long left = 6000 - ms_since_t0();
        if (left <= 0 || poll(p, 2, left) <= 0) break;
        for (int i = 0; i < 2; i++) {
            if (p[i].fd < 0 || !p[i].revents) continue;
            fs[i].tcp_ms = ms_since_t0();
            socklen_t l = sizeof fs[i].tcp_err;
            getsockopt(fs[i].tcp, SOL_SOCKET, SO_ERROR, &fs[i].tcp_err, &l);
        }
    }
    for (int i = 0; i < 2; i++) {
        if (fs[i].tcp_ms < 0) printf("%s tcp connect: still connecting after 6s\n", fs[i].name);
        else printf("%s tcp connect: %s after %s\n", fs[i].name, ename(fs[i].tcp_err), fs[i].tcp_ms < 2000 ? "less than 2s" : fs[i].tcp_ms <= 4500 ? "about 3s" : "more than 4.5s");
        bad += fs[i].tcp_err != EHOSTUNREACH;
    }
    raw_result(&fs[0]);
    raw_result(&fs[1]);
    sleep_until(6000);
    show("ipv4 after 6s", AF_INET, fs[0].a, 4, "FAILED");
    show("ipv6 after 6s", AF_INET6, fs[1].a, 16, "FAILED");
    for (int i = 0; i < 2; i++) {
        char c;
        long r = recv(fs[i].udp, &c, 1, MSG_DONTWAIT);
        printf("%s connected udp recv: %s\n", fs[i].name, r < 0 ? ename(errno) : "data");
    }
    return bad ? 1 : 0;
}
