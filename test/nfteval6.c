// nftables の評価の IPv6 (kernel/src/nfhook.rs と nftables.rs の IPv6、inet.rs の ::1 の lo のパケット): 決まりを nft -f
// で入れて ::1 の TCP と UDP を出し入れし、答え、counter (nft list ruleset)、log の行 (dmesg) を出す。inet、ip6、ip の
// 表 (ip の決まりは IPv6 を見ない、ip6 の決まりは IPv4 を見ない) と、v4 の番地を写したもの (::ffff:127.0.0.1 は IPv4)。
// 出すだけのテスト: root で、手本の Linux (lo だけ) とくらべる:
//   LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root nfteval6 /nft      (aios の中は /opt/c/bin/nft)
// counter は、どちらでも同じ数になるもの (SYN だけの TCP と UDP) だけ (aios の中どうしの TCP は、つなぐときの SYN と
// SYN-ACK だけが決まりを通る: test/nfteval.c と同じ)
// net をつけると eth0 の IPv6 の case も (SLAAC を待ってから。QEMU のユーザーネットワークの fec0::2 と fe80::2 へ
// ping6 と TCP と UDP。ホストに IPv6 がなくても QEMU が echo に答え、TCP と UDP には ICMPv6 のエラーを返す):
//   LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root --net nfteval6 /nft net
// eth0 の TCP の SYN は smoltcp と Linux で長さがちがうので、数えるのは ICMPv6 の echo だけ。QEMU は TCP に fe80::2 から
// ICMPv6 の no route を返すが、Linux はそれが connect のあいだ (ソケットを持っているあいだ) に着くと sk_err_soft にしまう
// だけで、つながるのを待ちつづける (時間しだい) ので、TCP は出す側で落とす・RST で断る・受ける側で落とすものだけ
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/icmp6.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/klog.h>
#include <linux/sockios.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *nft = "/opt/c/bin/nft";

static const char *ename(int e) {
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case ECONNREFUSED: return "ECONNREFUSED";
    case EHOSTUNREACH: return "EHOSTUNREACH";
    case EACCES: return "EACCES";
    case ENETUNREACH: return "ENETUNREACH";
    case ETIMEDOUT: return "ETIMEDOUT";
    case EAGAIN: return "EAGAIN";
    case ENOPROTOOPT: return "ENOPROTOOPT";
    case ECONNRESET: return "ECONNRESET";
    case EINPROGRESS: return "EINPROGRESS";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "errno%d", e);
        return b;
    }
    }
}

static struct sockaddr_in6 sa6(const char *ip, int port) {
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
    inet_pton(AF_INET6, ip, &a.sin6_addr);
    return a;
}

static void ms_sleep(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

// nft ARGS (空白で分ける) を動かし、出力と終わりの番号を出す
static void nft_run(const char *line) {
    char buf[512], *argv[32];
    int n = 0;
    snprintf(buf, sizeof buf, "%s", line);
    argv[n++] = (char *)nft;
    for (char *t = strtok(buf, " "); t && n < 31; t = strtok(NULL, " "))
        argv[n++] = t;
    argv[n] = NULL;
    int p[2];
    if (pipe(p) < 0)
        return;
    pid_t c = fork();
    if (c == 0) {
        dup2(p[1], 1);
        dup2(p[1], 2);
        close(p[0]);
        execv(nft, argv);
        printf("exec %s: %s\n", nft, strerror(errno));
        _exit(127);
    }
    close(p[1]);
    printf("-- nft %s\n", line);
    char b[4096];
    long r;
    while ((r = read(p[0], b, sizeof b)) > 0)
        fwrite(b, 1, r, stdout);
    close(p[0]);
    int st = 0;
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st))
        printf("-- exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

// TCP: listen (fam のもの。AF_INET6 は :: で v6only、AF_INET は 0.0.0.0。lport が 0 ならしない) して、to:port へ
// 待たない connect (to に ':' があれば AF_INET6 のソケットから)。1.5 秒まで待って答え、listen したものに来たか
static void tcp_probe(const char *label, int lfam, const char *to, int port, int lport) {
    int l = -1;
    if (lport) {
        l = socket(lfam, SOCK_STREAM | SOCK_NONBLOCK, 0);
        int one = 1;
        setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
        int r;
        if (lfam == AF_INET6) {
            setsockopt(l, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
            struct sockaddr_in6 a = sa6("::", lport);
            r = bind(l, (void *)&a, sizeof a);
        } else {
            struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(lport)};
            r = bind(l, (void *)&a, sizeof a);
        }
        if (r < 0 || listen(l, 8) < 0)
            printf("tcp %s: listen %s\n", label, ename(errno));
    }
    int six = strchr(to, ':') != NULL;
    int c = socket(six ? AF_INET6 : AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    int r;
    if (six) {
        struct sockaddr_in6 a = sa6(to, port);
        r = connect(c, (void *)&a, sizeof a);
    } else {
        struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
        inet_pton(AF_INET, to, &a.sin_addr);
        r = connect(c, (void *)&a, sizeof a);
    }
    const char *res;
    if (r == 0) {
        res = "connected";
    } else if (errno != EINPROGRESS) {
        res = ename(errno);
    } else {
        struct pollfd pf = {c, POLLOUT, 0};
        if (poll(&pf, 1, 1500) == 0) {
            res = "timeout";
        } else {
            int e = 0;
            socklen_t el = sizeof e;
            getsockopt(c, SOL_SOCKET, SO_ERROR, &e, &el);
            res = e ? ename(e) : "connected";
        }
    }
    const char *acc = "";
    if (l >= 0) {
        struct pollfd pf = {l, POLLIN, 0};
        int got = poll(&pf, 1, strcmp(res, "timeout") ? 1500 : 0) > 0;
        int k = got ? accept(l, NULL, NULL) : -1;
        acc = k >= 0 ? ", accepted" : ", not accepted";
        if (k >= 0)
            close(k);
    }
    printf("tcp %s: %s%s\n", label, res, acc);
    close(c);
    if (l >= 0)
        close(l);
}

// UDP (AF_INET6): 受ける口 ([::]:lport の v6only、0 ならなし)、connect した送り手から to:port へ n 個 (len バイト)。
// 送りの答え、届いた数、送り手のエラー (ICMPv6)
static void udp_probe(const char *label, const char *to, int port, int lport, int n, int len) {
    int r = -1;
    if (lport) {
        r = socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        int one = 1;
        setsockopt(r, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
        struct sockaddr_in6 a = sa6("::", lport);
        if (bind(r, (void *)&a, sizeof a) < 0)
            printf("udp %s: bind %s\n", label, ename(errno));
    }
    int s = socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in6 a = sa6(to, port);
    connect(s, (void *)&a, sizeof a);
    char buf[1500];
    memset(buf, 'x', sizeof buf);
    char out[256] = "";
    for (int i = 0; i < n; i++) {
        int k = send(s, buf, len, 0);
        strcat(out, i ? " " : "");
        strcat(out, k == len ? "ok" : ename(errno));
        ms_sleep(50);
    }
    ms_sleep(300);
    int got = 0;
    if (r >= 0)
        while (recv(r, buf, sizeof buf, 0) >= 0)
            got++;
    int k = recv(s, buf, sizeof buf, MSG_DONTWAIT);
    printf("udp %s: send %s; got %d; then %s\n", label, out, got, k >= 0 ? "data" : ename(errno));
    close(s);
    if (r >= 0)
        close(r);
}

// eth0 に link-local でないアドレスができて tentative でなくなるのを待つ (30 秒まで)
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

// ping6 (raw の ICMPv6、echo reply と destination unreachable だけを受ける): 送りの答えと、2.5 秒のうちに来たもの
// (aios の smoltcp は近所さがしの NS を 1 つ出すと、どのあて先にも 1 秒は出さないので、近所の表が切れたあとは
// 2 つ目のあて先の答えが 1 秒おくれる)
static void ping_probe(const char *to, unsigned scope) {
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    struct icmp6_filter f;
    ICMP6_FILTER_SETBLOCKALL(&f);
    ICMP6_FILTER_SETPASS(ICMP6_ECHO_REPLY, &f);
    ICMP6_FILTER_SETPASS(ICMP6_DST_UNREACH, &f);
    setsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, sizeof f);
    struct sockaddr_in6 a = sa6(to, 0);
    a.sin6_scope_id = scope;
    unsigned char pkt[16] = {ICMP6_ECHO_REQUEST, 0, 0, 0, 0x12, 0x34, 0, 7, 'n', 'f', 't', 'e', 'v', 'a', 'l', '6'};
    long r = sendto(s, pkt, sizeof pkt, 0, (void *)&a, sizeof a);
    printf("ping %s: send %s", to, r == sizeof pkt ? "ok" : ename(errno));
    struct pollfd pf = {s, POLLIN, 0};
    if (poll(&pf, 1, 2500) > 0) {
        unsigned char b[256];
        struct sockaddr_in6 from;
        socklen_t fl = sizeof from;
        long got = recvfrom(s, b, sizeof b, 0, (void *)&from, &fl);
        char ip[INET6_ADDRSTRLEN];
        inet_ntop(AF_INET6, &from.sin6_addr, ip, sizeof ip);
        printf("; got type %u code %u from %s len %ld\n", b[0], b[1], ip, got);
    } else {
        printf("; nothing\n");
    }
    close(s);
}

// /proc/net/nf_conntrack の ipv6 の行のうち want をふくむものを、時間切れの秒と 32768 からのポートを * にして、ならべて
static int cmp_str(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }
static void conntrack_list(const char *want) {
    FILE *f = fopen("/proc/net/nf_conntrack", "r");
    if (!f) {
        printf("conntrack: %s\n", ename(errno));
        return;
    }
    char *lines[256];
    int n = 0;
    char l[512];
    while (n < 256 && fgets(l, sizeof l, f)) {
        if (strncmp(l, "ipv6", 4) || !strstr(l, want))
            continue;
        char o[512];
        int w = 0, field = 0;
        for (char *p = l; *p && w < (int)sizeof o - 4;) {
            if (*p == ' ') {
                field++;
                while (*p == ' ')
                    o[w++] = *p++;
                continue;
            }
            if (field == 4 && *p >= '0' && *p <= '9') {
                while (*p >= '0' && *p <= '9')
                    p++;
                o[w++] = '*';
                continue;
            }
            int key = !strncmp(p, "sport=", 6) || !strncmp(p, "dport=", 6) ? 6 : 0;
            if (key) {
                memcpy(o + w, p, key);
                w += key;
                p += key;
                long v = strtol(p, NULL, 10);
                char *e = p;
                while (*e >= '0' && *e <= '9')
                    e++;
                if (v >= 32768) {
                    o[w++] = '*';
                } else {
                    memcpy(o + w, p, e - p);
                    w += e - p;
                }
                p = e;
                continue;
            }
            o[w++] = *p++;
        }
        o[w] = 0;
        lines[n++] = strdup(o);
    }
    fclose(f);
    qsort(lines, n, sizeof *lines, cmp_str);
    for (int i = 0; i < n; i++)
        printf("ct %s", lines[i]);
    printf("ct (%d)\n", n);
}

// eth0 の向こうへの TCP (待たない connect、0.9 秒まで: Linux の SYN のやりなおしは 1 秒から)
static void tcp_out(const char *to, int port) {
    int c = socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in6 a = sa6(to, port);
    int r = connect(c, (void *)&a, sizeof a);
    const char *res = r == 0 ? "connected" : ename(errno);
    if (r < 0 && errno == EINPROGRESS) {
        struct pollfd pf = {c, POLLOUT, 0};
        if (poll(&pf, 1, 900) == 0) {
            res = "timeout";
        } else {
            int e = 0;
            socklen_t el = sizeof e;
            getsockopt(c, SOL_SOCKET, SO_ERROR, &e, &el);
            res = e ? ename(e) : "connected";
        }
    }
    printf("tcp %s %d: %s\n", to, port, res);
    close(c);
}

// dmesg の nfteval- の行 (log の prefix) (前に見せたものより後)。時刻と段を取り、変わるもの (IP の id、32768 からのポート) は *
static int seen;
static void klog_print(int print);
static void klog(void) {
    klog_print(1);
}

static void klog_print(int print) {
    static char b[1 << 20];
    int n = klogctl(3, b, sizeof b - 1);
    if (n < 0) {
        // aios: /proc/kmsg はためてあるものをそのまま読む
        int fd = open("/proc/kmsg", O_RDONLY);
        n = 0;
        if (fd >= 0) {
            long k;
            while ((k = read(fd, b + n, sizeof b - 1 - n)) > 0)
                n += k;
            close(fd);
        }
    }
    b[n] = 0;
    int idx = 0;
    for (char *line = strtok(b, "\n"); line; line = strtok(NULL, "\n")) {
        char *p = strstr(line, "nfteval6-");
        if (!p || idx++ < seen)
            continue;
        char o[1024];
        int w = 0;
        while (*p && w < (int)sizeof o - 16) {
            int key = !strncmp(p, "ID=", 3) ? 3 : !strncmp(p, "SPT=", 4) || !strncmp(p, "DPT=", 4) ? 4 : !strncmp(p, "FLOWLBL=", 8) ? 8 : 0;
            if (key && (p == line || p[-1] == ' ')) {
                memcpy(o + w, p, key);
                w += key;
                p += key;
                long v = strtol(p, NULL, 10);
                char *e = p;
                while (*e >= '0' && *e <= '9')
                    e++;
                if (key == 3 || key == 8 || v >= 32768) {
                    o[w++] = '*';
                } else {
                    memcpy(o + w, p, e - p);
                    w += e - p;
                }
                p = e;
                continue;
            }
            o[w++] = *p++;
        }
        o[w] = 0;
        if (print)
            printf("log %s\n", o);
    }
    seen = idx;
}

struct tc {
    const char *name;
    const char *rules;
    // 動かすもの: 6 (::1 の TCP)、n (::1 のだれもいない TCP)、u (::1 の UDP)、v (::1 のだれもいない UDP)、
    // 4 (127.0.0.1 の TCP)、m (AF_INET6 から ::ffff:127.0.0.1 の TCP: IPv4)、c (conntrack の ::1 の行)
    const char *probes;
};

static const struct tc cases[] = {
    {"accept",
     "table inet t {\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7101 tcp flags & (syn | ack) == syn counter\n\t\tudp dport 7102 counter\n"
     "\t\tmeta nfproto ipv6 udp dport 7102 counter\n\t\tmeta nfproto ipv4 udp dport 7102 counter\n"
     "\t\tip6 saddr ::1 udp dport 7102 counter\n\t\tip6 nexthdr udp udp dport 7102 counter\n\t\tmeta l4proto udp udp dport 7102 counter\n\t}\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\ttcp dport 7101 tcp flags & (syn | ack) == syn counter\n\t\tudp dport 7102 counter\n\t\toif lo ip6 daddr ::1 udp dport 7102 counter\n\t}\n"
     "\tchain pre {\n\t\ttype filter hook prerouting priority filter; policy accept;\n"
     "\t\tiif lo ip6 hoplimit 64 udp dport 7102 counter\n\t}\n"
     "}\n",
     "6nuv"},
    {"drop-in",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7101 counter drop\n\t\tudp dport 7102 counter drop\n\t}\n}\n",
     "6u"},
    {"reject-in",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7101 counter reject with tcp reset\n\t\tudp dport 7102 counter reject\n\t}\n}\n",
     "6u"},
    {"reject-icmpv6",
     "table ip6 t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7101 reject with icmpv6 admin-prohibited\n\t\tudp dport 7102 reject with icmpv6 addr-unreachable\n"
     "\t\tudp dport 7104 reject with icmpv6 no-route\n\t}\n}\n",
     "6uv"},
    {"reject-icmpx",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7101 reject with icmpx admin-prohibited\n\t\tudp dport 7102 reject with icmpx host-unreachable\n\t}\n}\n",
     "6u"},
    {"drop-out",
     "table inet t {\n\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\ttcp dport 7101 counter drop\n\t\tudp dport 7102 counter drop\n\t}\n}\n",
     "6u"},
    {"reject-out",
     "table inet t {\n\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\ttcp dport 7101 reject with tcp reset\n\t\tudp dport 7102 reject\n\t}\n}\n",
     "6u"},
    {"ip-only",
     "table ip t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7101 counter drop\n\t}\n}\n",
     "64m"},
    {"ip6-only",
     "table ip6 t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7101 counter drop\n\t}\n}\n",
     "64m"},
    {"policy-drop",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy drop;\n"
     "\t\tmeta nfproto ipv4 accept\n\t\tudp dport 7102 accept\n\t}\n}\n",
     "6u4"},
    {"log",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tudp dport 7102 log prefix \"nfteval6-udp \"\n"
     "\t\ttcp dport 7103 log prefix \"nfteval6-tcp \" level warn\n\t}\n}\n",
     "un"},
    {"ct-state6",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy drop;\n"
     "\t\tct state established,related counter accept\n\t\tct state new tcp dport 7101 counter accept\n"
     "\t\tct state new udp dport { 7102, 7104 } counter accept\n\t\tct state invalid counter drop\n\t\tct state untracked counter accept\n\t}\n}\n",
     "6uvnc"},
    {"ct-keys6",
     "table inet t {\n\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\tct l3proto ipv6 udp dport 7102 counter\n\t\tct original ip6 saddr ::1 udp dport 7102 counter\n"
     "\t\tct protocol udp ct original proto-dst 7102 counter\n\t\tct direction original udp dport 7102 counter\n"
     "\t\tct l3proto ipv4 udp dport 7102 counter\n\t\tmeta nfproto ipv6 ct state new udp dport 7102 counter\n\t}\n}\n",
     "u"},
};

// eth0 の case (net をつけたとき)。probes: p (fec0::2 へ ping6)、l (fe80::2%eth0 へ ping6)、t (fec0::2 の TCP 1)、
// w (fec0::2 の UDP 9 へ 2 つ)、x (2001:db8::1 へ ping6)、C (conntrack の fec0:: の行)
static const struct tc net_cases[] = {
    {"eth-accept",
     "table inet t {\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tiif eth0 ip6 saddr fec0::2 icmpv6 type echo-reply counter\n\t\tmeta nfproto ipv6 icmpv6 type echo-reply counter\n"
     "\t\tip6 saddr fe80::2 icmpv6 type echo-reply counter\n\t}\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\toif eth0 ip6 daddr fec0::2 icmpv6 type echo-request counter\n\t\tip6 daddr fe80::2 icmpv6 type echo-request counter\n\t}\n"
     "\tchain pre {\n\t\ttype filter hook prerouting priority filter; policy accept;\n"
     "\t\tiif eth0 icmpv6 type echo-reply counter\n\t}\n"
     "\tchain post {\n\t\ttype filter hook postrouting priority filter; policy accept;\n"
     "\t\toif eth0 icmpv6 type echo-request counter\n\t}\n"
     "}\n",
     "pl"},
    {"eth-drop-in",
     "table ip6 t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tip6 saddr fec0::2 icmpv6 type echo-reply counter drop\n\t}\n}\n",
     "p"},
    {"eth-drop-out",
     "table ip6 t {\n\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\tip6 daddr fec0::2 icmpv6 type echo-request counter drop\n\t\tip6 daddr fec0::2 tcp dport 1 drop\n\t\tip6 daddr fec0::2 udp dport 9 drop\n\t}\n}\n",
     "ptw"},
    {"eth-reject-out",
     "table inet t {\n\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\tip6 daddr fec0::2 icmpv6 type echo-request reject with icmpv6 admin-prohibited\n"
     "\t\tip6 daddr fec0::2 tcp dport 1 reject with tcp reset\n\t\tip6 daddr fec0::2 udp dport 9 reject\n\t}\n}\n",
     "ptw"},
    {"eth-reject-in",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tip6 saddr fec0::2 icmpv6 type echo-reply counter reject with icmpv6 admin-prohibited\n\t}\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\tip6 daddr fec0::2 icmpv6 type destination-unreachable counter\n\t}\n}\n",
     "p"},
    {"eth-policy-drop",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy drop;\n"
     "\t\ticmpv6 type { nd-neighbor-solicit, nd-neighbor-advert, nd-router-advert, echo-reply } accept\n\t}\n}\n",
     "pt"},
    {"eth-ct",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy drop;\n"
     "\t\ticmpv6 type { nd-neighbor-solicit, nd-neighbor-advert, nd-router-advert } accept\n"
     "\t\tct state established,related counter accept\n\t}\n}\n",
     "pxC"},
    {"eth-log",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tiif eth0 icmpv6 type echo-reply log prefix \"nfteval6-ein \"\n\t}\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\toif eth0 icmpv6 type echo-request log prefix \"nfteval6-eout \"\n\t}\n}\n",
     "p"},
};

static void probe(char c) {
    switch (c) {
    case '6': tcp_probe("::1 7101", AF_INET6, "::1", 7101, 7101); break;
    case 'n': tcp_probe("::1 7103 (nobody)", AF_INET6, "::1", 7103, 0); break;
    case 'u': udp_probe("::1 7102", "::1", 7102, 7102, 3, 5); break;
    case 'v': udp_probe("::1 7104 (nobody)", "::1", 7104, 0, 2, 5); break;
    case '4': tcp_probe("127.0.0.1 7101", AF_INET, "127.0.0.1", 7101, 7101); break;
    case 'm': tcp_probe("::ffff:127.0.0.1 7101", AF_INET, "::ffff:127.0.0.1", 7101, 7101); break;
    case 'p': ping_probe("fec0::2", 0); break;
    case 'l': ping_probe("fe80::2", 2); break;
    case 't': tcp_out("fec0::2", 1); break;
    case 'w': udp_probe("fec0::2 9", "fec0::2", 9, 0, 2, 5); break;
    case 'x': ping_probe("2001:db8::1", 0); break;
    case 'c': conntrack_list("0000:0000:0000:0000:0000:0000:0000:0001"); break;
    case 'C': conntrack_list("fec0:"); break;
    }
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1)
        nft = argv[1];
    // 手本の Linux は /nft (LINUX_FILES)、aios は /opt/c/bin/nft: あるほうを
    if (access(nft, X_OK) != 0)
        nft = access("/opt/c/bin/nft", X_OK) == 0 ? "/opt/c/bin/nft" : "/nft";
    if (access(nft, X_OK) != 0) {
        printf("nfteval6: no %s (skipped)\n", nft);
        return 0;
    }
    const char *only = argc > 2 && strcmp(argv[2], "net") ? argv[2] : NULL;
    int net = argc > 2 && !strcmp(argv[argc - 1], "net");
    if (net)
        printf("eth0 slaac: %s\n", wait_slaac() ? "yes" : "no");
    nft_run("flush ruleset");
    // 前に動かしたときの行は数えるだけ
    klog_print(0);
    size_t nc = sizeof cases / sizeof *cases, nn = net ? sizeof net_cases / sizeof *net_cases : 0;
    for (size_t i = 0; i < nc + nn; i++) {
        const struct tc *t = i < nc ? &cases[i] : &net_cases[i - nc];
        if (only && strcmp(only, t->name))
            continue;
        printf("== %s\n", t->name);
        nft_run("flush ruleset");
        int fd = open("/tmp/nfteval6.nft", O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (write(fd, t->rules, strlen(t->rules)) < 0)
            perror("write");
        close(fd);
        nft_run("-f /tmp/nfteval6.nft");
        for (const char *p = t->probes; *p; p++)
            probe(*p);
        nft_run("list ruleset");
        klog();
    }
    nft_run("flush ruleset");
    return 0;
}
