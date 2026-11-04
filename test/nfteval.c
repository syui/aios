// nftables の評価 (doc/aios.md の「ファイアウォール (nftables)」の段階 2): 決まりを nft -f で入れて、パケットを出し入れ
// し、答え (つながる・断られる・時間切れ・届く・EPERM)、counter の数 (nft list ruleset)、log の行 (dmesg) を出す。
// 出すだけのテスト: 手本の Linux 7.3.1 (virtio-net、ip=dhcp) の出力とくらべる。root で
//   aios の中で: sudo ./nfteval NFT P1 P2 P3
//   Linux で: LINUX_NET=P1,P2 LINUX_FILES="nft=NFT" test/linux-vm.sh rootexec latest test/bin/nfteval /nft P1 P2 P3
//   aios の VM: AIOS_FWD=P1,P2 test/vm.py start (bin/run.sh が hostfwd を足す)
//   中どうしだけなら: LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root --net nfteval /nft 0 0 0
//   P1 か P3 を 0 にすると、それを使うもの (外を一回りするもの、外のだれもいないポート) はしない (vm-ctests)
// 道:
//   - 中どうし (lo): 127.0.0.1 と eth0 のアドレス (10.0.2.15) への TCP の connect と UDP
//   - 外 (eth0): QEMU のユーザーネットワークの 10.0.2.2。P3 はホストでだれも待っていないポート (断られる)。QEMU の
//     hostfwd でホストの 127.0.0.1:P1 → 中の 7001 (TCP)、P2 → 7002 (UDP) にしてあるので、中から 10.0.2.2:P1 に
//     つなぐと QEMU を一回りして、外 (10.0.2.2) から 7001 に来る (hairpin)
// 時間切れは 1.5 秒 (SYN は 1 秒でもう一度送られる: 落とす決まりの counter は 2)
// counter は、どちらでも同じ数になるものだけ (SYN だけの TCP と、UDP)。中どうしの TCP は、aios ではつなぐときの
// SYN と SYN-ACK だけが決まりを通る。外へつなぐ TCP は aios では smoltcp が作るので、SYN のやりなおしの間が Linux と
// ちがう (数えない)
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
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
static int P1, P2, P3;
static const char *ETH = "10.0.2.15";
static const char *GW = "10.0.2.2";

static const char *ename(int e) {
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case ECONNREFUSED: return "ECONNREFUSED";
    case EHOSTUNREACH: return "EHOSTUNREACH";
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

static struct sockaddr_in sa(const char *ip, int port) {
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
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

// TCP: listen (0.0.0.0:lport、lport が 0 ならしない) して、to:port へ待たない connect。1.5 秒まで待って答え、
// listen したものに来たか (accept)
static void tcp_probe(const char *label, const char *to, int port, int lport) {
    int l = -1;
    if (lport) {
        l = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
        int one = 1;
        setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
        struct sockaddr_in a = sa("0.0.0.0", lport);
        if (bind(l, (void *)&a, sizeof a) < 0 || listen(l, 8) < 0)
            printf("tcp %s: listen %s\n", label, ename(errno));
    }
    int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in a = sa(to, port);
    const char *res;
    if (connect(c, (void *)&a, sizeof a) == 0) {
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

// UDP: 受ける口 (0.0.0.0:lport、0 ならなし)、connect した送り手から to:port へ n 個 (len バイト)。送りの答え、届いた数、
// 送り手のエラー (ICMP)
static void udp_probe(const char *label, const char *to, int port, int lport, int n, int len) {
    int r = -1;
    if (lport) {
        r = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        struct sockaddr_in a = sa("0.0.0.0", lport);
        if (bind(r, (void *)&a, sizeof a) < 0)
            printf("udp %s: bind %s\n", label, ename(errno));
    }
    int s = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in a = sa(to, port);
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

// dmesg の nfteval- の行 (log の prefix) (前に見せたものより後)。時刻と段を取り、変わるもの (IP の id、32768 からのポート) は *
static int seen;
static void klog(void) {
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
        char *p = strstr(line, "nfteval-");
        if (!p || idx++ < seen)
            continue;
        char o[1024];
        int w = 0;
        while (*p && w < (int)sizeof o - 16) {
            int key = !strncmp(p, "ID=", 3) ? 3 : !strncmp(p, "SPT=", 4) || !strncmp(p, "DPT=", 4) ? 4 : 0;
            if (key && (p == line || p[-1] == ' ')) {
                memcpy(o + w, p, key);
                w += key;
                p += key;
                long v = strtol(p, NULL, 10);
                char *e = p;
                while (*e >= '0' && *e <= '9')
                    e++;
                if (key == 3 || v >= 32768) {
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
        printf("log %s\n", o);
    }
    seen = idx;
}

struct tc {
    const char *name;
    const char *rules;
    // 動かすもの: 文字ごとに t (lo の TCP)、n (lo のだれもいないポート)、e (eth0 のアドレスへ TCP)、u (lo の UDP)、
    // v (lo のだれもいない UDP)、x (外のだれもいないポート)、h (外を一回りする TCP)、k (外を一回りする UDP)、
    // L (limit: UDP を 5 つ)、Q (quota: 7003 へ UDP を 3 つ)
    const char *probes;
};

static const struct tc cases[] = {
    {"accept",
     "table inet t {\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7001 tcp flags & (syn | ack) == syn counter\n\t\tudp dport 7002 counter\n\t\tudp dport 7004 counter\n\t}\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\ttcp dport 7001 tcp flags & (syn | ack) == syn counter\n\t\tudp dport 7002 counter\n\t}\n"
     "}\n",
     "tneuvxhk"},
    {"drop-in",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7001 counter drop\n\t\tudp dport 7002 counter drop\n\t}\n}\n",
     "teuhk"},
    {"reject-in",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7001 counter reject with tcp reset\n\t\tudp dport 7002 counter reject\n\t}\n}\n",
     "teuhk"},
    {"reject-icmp",
     "table ip t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7001 reject with icmp host-unreachable\n\t\tudp dport 7002 reject with icmp admin-prohibited\n"
     "\t\tudp dport 7004 reject with icmp host-unreachable\n\t}\n}\n",
     "tuv"},
    {"drop-out",
     "table inet t {\n\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\ttcp dport 7001 counter drop\n\t\tudp dport 7002 counter drop\n\t\tip daddr 10.0.2.2 meta l4proto udp counter drop\n"
     "\t\tip daddr 10.0.2.2 drop\n\t}\n}\n",
     "tuxhk"},
    {"reject-out",
     "table inet t {\n\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\ttcp dport 7001 reject with tcp reset\n\t\tudp dport 7002 reject\n"
     "\t\tip daddr 10.0.2.2 meta l4proto tcp reject with tcp reset\n\t\tip daddr 10.0.2.2 reject with icmpx host-unreachable\n\t}\n}\n",
     "tuxhk"},
    {"policy-drop",
     "table inet filter {\n\tchain input {\n\t\ttype filter hook input priority filter; policy drop;\n"
     "\t\tiif \"lo\" accept\n\t\ttcp dport 7001 accept\n\t}\n}\n",
     "tneuxhk"},
    {"jumps",
     "table ip t {\n\tchain in {\n\t\ttype filter hook input priority 0; policy accept;\n"
     "\t\tjump c1\n\t\ttcp dport 7001 tcp flags & (syn | ack) == syn counter drop\n\t}\n"
     "\tchain c1 {\n\t\ttcp dport 7001 return\n\t\tudp dport 7002 goto c2\n\t\tudp dport 7002 counter accept\n\t}\n"
     "\tchain c2 {\n\t\tudp dport 7002 counter drop\n\t}\n}\n",
     "tu"},
    {"sets",
     "table inet t {\n"
     "\tset ports {\n\t\ttype inet_service\n\t\telements = { 7001, 7002 }\n\t}\n"
     "\tset nets {\n\t\ttype ipv4_addr\n\t\tflags interval\n\t\telements = { 127.0.0.0/8 }\n\t}\n"
     "\tmap act {\n\t\ttype inet_service : verdict\n\t\telements = { 7001 : drop, 7002 : accept }\n\t}\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tip saddr @nets tcp dport @ports tcp flags & (syn | ack) == syn counter\n"
     "\t\tip saddr != @nets udp dport @ports counter\n"
     "\t\tip saddr . udp dport { 127.0.0.1 . 7002, 10.0.2.2 . 7002 } counter\n"
     "\t\ttcp dport vmap @act\n\t\tudp dport vmap @act\n\t}\n}\n",
     "tek"},
    {"limit-quota",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tudp dport 7002 limit rate 1/minute burst 2 packets counter accept\n\t\tudp dport 7002 counter drop\n"
     "\t\tudp dport 7003 quota over 100 bytes counter drop\n\t}\n}\n",
     "LQ"},
    {"meta-log",
     "table inet t {\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\tmeta skuid 0 udp dport 7002 counter\n\t\tmeta skuid 1000 udp dport 7002 counter\n"
     "\t\toifname \"lo\" udp dport 7002 counter\n\t\tmeta l4proto udp udp dport 7002 meta pkttype host counter\n"
     "\t\tudp dport 7002 meta mark set 42\n"
     "\t\tudp dport 7002 log prefix \"nfteval-out: \" counter\n\t}\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tiifname \"lo\" udp dport 7002 log prefix \"nfteval-in: \" flags all counter\n"
     "\t\tiifname \"eth0\" udp dport 7002 log prefix \"nfteval-eth: \" flags all counter\n"
     "\t\tmeta iiftype loopback udp dport 7002 meta mark 42 counter\n"
     "\t\tudp dport 7004 log prefix \"nfteval-in4: \" counter\n\t}\n"
     "\tchain post {\n\t\ttype filter hook postrouting priority filter; policy accept;\n"
     "\t\tmeta mark 42 counter\n\t\tip protocol icmp log prefix \"nfteval-icmp: \" counter\n\t}\n"
     "}\n",
     "ukv"},
    // conntrack (段階 3)
    {"ct-state",
     "table inet filter {\n\tchain input {\n\t\ttype filter hook input priority filter; policy drop;\n"
     "\t\tct state established,related accept\n\t\tct state invalid drop\n\t\tiif \"lo\" accept\n"
     "\t\ttcp dport 7001 ct state new counter accept\n\t}\n}\n",
     "tnuxhk"},
    {"ct-keys",
     "table inet t {\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n"
     "\t\tudp dport 7002 ct mark set 42\n\t\tct direction original udp dport 7002 counter\n"
     "\t\tct original proto-dst 7002 ct original ip saddr 127.0.0.1 counter\n\t}\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tct mark 42 counter\n\t\tct direction reply tcp flags & (syn | ack) == syn | ack counter\n"
     "\t\tct state new tcp dport 7001 counter\n\t\tct status seen-reply tcp sport 7001 counter\n"
     "\t\tct l3proto ipv4 ct protocol udp udp dport 7002 counter\n\t}\n}\n",
     "tu"},
    {"ct-notrack",
     "table inet t {\n"
     "\tchain raw {\n\t\ttype filter hook output priority raw; policy accept;\n\t\tudp dport 7002 notrack\n\t}\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tct state untracked udp dport 7002 counter\n\t\tct state new udp dport 7004 counter\n\t}\n}\n",
     "uv"},
    {"ct-related",
     "table inet t {\n"
     "\tchain out {\n\t\ttype filter hook output priority filter; policy accept;\n\t\tudp dport 7003 reject\n\t}\n"
     "\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\tudp dport 7002 reject\n\t\ttcp dport 7001 reject with tcp reset\n"
     "\t\tip protocol icmp ct state related counter\n\t\tip protocol icmp ct state invalid counter\n"
     "\t\ttcp flags rst ct state related counter\n\t\ttcp flags rst ct state established counter\n\t}\n}\n",
     "tuvQ"},
    // 中どうしの TCP のデータ (データの segment も決まりを通る: PSH ACK)
    {"lo-data",
     "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n"
     "\t\ttcp dport 7001 tcp flags & (psh | ack) == psh | ack counter\n"
     "\t\ttcp sport 7001 tcp flags & (psh | ack) == psh | ack counter\n"
     "\t\ttcp dport 7001 meta length > 52 counter\n\t}\n}\n",
     "D"},
    // つながったあとで落とすと届かず (送りなおし)、決まりを消すと届く
    {"lo-retransmit", "table inet t {\n\tchain in {\n\t\ttype filter hook input priority filter; policy accept;\n\t}\n}\n", "R"},
    {"ct-list", "table inet t {\n\tchain c {\n\t\tct state new\n\t}\n}\n", "C"},
    {"order",
     "table inet a {\n\tchain in {\n\t\ttype filter hook input priority 10; policy accept;\n\t\tudp dport 7002 counter\n\t}\n}\n"
     "table ip b {\n\tchain in {\n\t\ttype filter hook input priority -10; policy accept;\n\t\tudp dport 7002 counter drop\n\t}\n}\n"
     "table inet c {\n\tflags dormant\n\tchain in {\n\t\ttype filter hook input priority -20; policy drop;\n\t}\n}\n",
     "uk"},
};

// /proc/net/nf_conntrack を、時間切れの秒と 32768 からのポートを * にして、ならべて出す。外 (10.0.2.2) のものは
// TCP の見張りの道 (smoltcp と Linux) がちがうので、数だけ
static int cmp_str(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }
static void conntrack_list(void) {
    FILE *f = fopen("/proc/net/nf_conntrack", "r");
    if (!f) {
        printf("conntrack: %s\n", strerror(errno));
        return;
    }
    char *lines[512];
    int n = 0, ext = 0;
    char l[512];
    while (n < 512 && fgets(l, sizeof l, f)) {
        if (strstr(l, "10.0.2.2")) {
            ext++;
            continue;
        }
        char o[512];
        int w = 0, field = 0;
        for (char *p = l; *p && w < (int)sizeof o - 4;) {
            if (*p == ' ') {
                field++;
                while (*p == ' ')
                    o[w++] = *p++;
                continue;
            }
            // 5 つめ (ipv4 2 tcp 6 の次) は残りの秒
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
    printf("ct (%d with 10.0.2.2)\n", ext);
}

// TCP でデータを送り受け (中どうし): 3 回送って、1 回返す
static int tcp_pair(int *l, int *c, int *k) {
    *l = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(*l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = sa("0.0.0.0", 7001);
    if (bind(*l, (void *)&a, sizeof a) < 0 || listen(*l, 8) < 0)
        return -1;
    *c = socket(AF_INET, SOCK_STREAM, 0);
    a = sa("127.0.0.1", 7001);
    if (connect(*c, (void *)&a, sizeof a) < 0)
        return -1;
    *k = accept(*l, NULL, NULL);
    return *k < 0 ? -1 : 0;
}

static int drain(int fd) {
    char b[4096];
    int n = 0;
    long r;
    while ((r = recv(fd, b, sizeof b, MSG_DONTWAIT)) > 0)
        n += r;
    return n;
}

static void data_probe(void) {
    int l, c, k;
    if (tcp_pair(&l, &c, &k) < 0) {
        printf("tcp data: %s\n", ename(errno));
        return;
    }
    for (int i = 0; i < 3; i++) {
        send(c, "hello", 5, 0);
        ms_sleep(50);
    }
    ms_sleep(100);
    int got = drain(k);
    send(k, "bye", 3, 0);
    ms_sleep(100);
    int back = drain(c);
    printf("tcp data: server got %d, client got %d\n", got, back);
    close(k);
    close(c);
    close(l);
    ms_sleep(100);
}

static void retransmit_probe(void) {
    int l, c, k;
    if (tcp_pair(&l, &c, &k) < 0) {
        printf("tcp retransmit: %s\n", ename(errno));
        return;
    }
    send(c, "one", 3, 0);
    ms_sleep(100);
    int before = drain(k);
    nft_run("add rule inet t in tcp dport 7001 drop");
    int sent = send(c, "two", 3, 0);
    ms_sleep(500);
    int during = drain(k);
    int outq = -1, outq2 = -1;
    ioctl(c, SIOCOUTQ, &outq);
    nft_run("flush chain inet t in");
    ms_sleep(1500);
    int after = drain(k);
    ioctl(c, SIOCOUTQ, &outq2);
    printf("tcp retransmit: before %d, sent %d, while dropped %d (outq %d), after %d (outq %d)\n", before, sent, during, outq, after, outq2);
    close(k);
    close(c);
    close(l);
    ms_sleep(100);
}

static void probe(char c) {
    char lbl[64];
    // ポート 0: hostfwd がない (vm-ctests) ので外を一回りするもの、ホストのポートがないので外のだれもいないもの、はしない
    if ((!P1 && (c == 'h' || c == 'k')) || (!P3 && c == 'x'))
        return;
    switch (c) {
    case 't': tcp_probe("lo 7001", "127.0.0.1", 7001, 7001); break;
    case 'n': tcp_probe("lo 7003 (nobody)", "127.0.0.1", 7003, 0); break;
    case 'e': tcp_probe("eth-addr 7001", ETH, 7001, 7001); break;
    case 'u': udp_probe("lo 7002", "127.0.0.1", 7002, 7002, 3, 5); break;
    case 'v': udp_probe("lo 7004 (nobody)", "127.0.0.1", 7004, 0, 2, 5); break;
    case 'x': tcp_probe("gw (nobody)", GW, P3, 0); break;
    case 'h': tcp_probe("hairpin", GW, P1, 7001); break;
    case 'k': udp_probe("hairpin", GW, P2, 7002, 2, 5); break;
    case 'L': udp_probe("limit 7002", "127.0.0.1", 7002, 7002, 5, 5); break;
    case 'Q':
        snprintf(lbl, sizeof lbl, "quota 7003");
        udp_probe(lbl, "127.0.0.1", 7003, 7003, 3, 50);
        break;
    case 'C':
        conntrack_list();
        break;
    case 'D':
        data_probe();
        break;
    case 'R':
        retransmit_probe();
        break;
    }
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1)
        nft = argv[1];
    // 手本の Linux は /nft (LINUX_FILES)、aios は /opt/c/bin/nft: あるほうを (test/ldiff.sh で同じ引数で動かせる)
    if (access(nft, X_OK) != 0)
        nft = access("/opt/c/bin/nft", X_OK) == 0 ? "/opt/c/bin/nft" : "/nft";
    if (access(nft, X_OK) != 0) {
        printf("nfteval: no %s (skipped)\n", nft);
        return 0;
    }
    if (argc < 5) {
        printf("usage: nfteval NFT P1 P2 P3\n");
        return 2;
    }
    P1 = atoi(argv[2]);
    P2 = atoi(argv[3]);
    P3 = atoi(argv[4]);
    const char *only = argc > 5 ? argv[5] : NULL;
    nft_run("flush ruleset");
    klog();
    // DHCP でアドレスがつくまで (Linux の ip=dhcp は起こすときに待つ。aios もふつうはもうついている)
    for (int i = 0; i < 50; i++) {
        int s = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in a = sa(GW, 9), me;
        socklen_t ml = sizeof me;
        connect(s, (void *)&a, sizeof a);
        getsockname(s, (void *)&me, &ml);
        close(s);
        if (me.sin_addr.s_addr == sa(ETH, 0).sin_addr.s_addr)
            break;
        ms_sleep(200);
    }
    for (size_t i = 0; i < sizeof cases / sizeof *cases; i++) {
        const struct tc *t = &cases[i];
        if (only && strcmp(only, t->name))
            continue;
        printf("== %s\n", t->name);
        nft_run("flush ruleset");
        int fd = open("/tmp/nfteval.nft", O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (write(fd, t->rules, strlen(t->rules)) < 0)
            perror("write");
        close(fd);
        nft_run("-f /tmp/nfteval.nft");
        for (const char *p = t->probes; *p; p++)
            probe(*p);
        nft_run("list ruleset");
        klog();
    }
    nft_run("flush ruleset");
    return 0;
}
