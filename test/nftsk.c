// 受けるパケットの meta skuid / skgid (Linux の nft_meta_get_eval_skugid と、ip_rcv_finish_core / ip6_rcv_finish_core の
// early demux: tcp_v4_early_demux・tcp_v6_early_demux はつながり (ehash: SYN_SENT も) のソケット、udp は connect した
// もの)。PRE_ROUTING はまだソケットがなく、lo から来たものは dst があるので early demux しない (どちらも当たらない)。
// QEMU のユーザーネットワークのホスト (10.0.2.2 と fec0::2) のだれも待っていないポート 1 へ connect すると RST が来る
// ので、それを input と prerouting の鎖の counter でかぞえる。持ち主は root と、uid 4242 / gid 4243 にした子
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net と静的な nft
//   LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root --net nftsk
//   aios の中で: sudo ./nftsk (nftables のパッケージがいる)
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *nft = "/opt/c/bin/nft";

static void ms_sleep(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

// nft ARGS (空白で分ける) を動かし、出力 (空白の行はのぞく) と終わりの番号を出す
static void nft_run(const char *line) {
    char buf[1024], *argv[64];
    int n = 0;
    snprintf(buf, sizeof buf, "%s", line);
    argv[n++] = (char *)nft;
    for (char *t = strtok(buf, " "); t && n < 63; t = strtok(NULL, " "))
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
    FILE *f = fdopen(p[0], "r");
    char l[1024];
    while (fgets(l, sizeof l, f))
        if (strspn(l, " \t\n") != strlen(l))
            fputs(l, stdout);
    fclose(f);
    int st = 0;
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st))
        printf("-- exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case ECONNREFUSED: return "ECONNREFUSED";
    case ETIMEDOUT: return "ETIMEDOUT";
    case ENETUNREACH: return "ENETUNREACH";
    case EHOSTUNREACH: return "EHOSTUNREACH";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "errno%d", e);
        return b;
    }
    }
}

// fam の addr:1 へ connect (5 秒まで)。uid が 0 でなければ、その uid と gid+1 の子で
static void try_connect(int fam, const char *addr, unsigned uid) {
    pid_t c = fork();
    if (c == 0) {
        if (uid && (setresgid(uid + 1, uid + 1, uid + 1) || setresuid(uid, uid, uid))) {
            printf("setresuid: %s\n", strerror(errno));
            _exit(1);
        }
        int s = socket(fam, SOCK_STREAM, 0);
        struct timeval tv = {5, 0};
        setsockopt(s, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
        int r;
        if (fam == AF_INET) {
            struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(1)};
            inet_pton(AF_INET, addr, &a.sin_addr);
            r = connect(s, (struct sockaddr *)&a, sizeof a);
        } else {
            struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(1)};
            inet_pton(AF_INET6, addr, &a.sin6_addr);
            r = connect(s, (struct sockaddr *)&a, sizeof a);
        }
        printf("connect %s:1 as uid %u: %s\n", addr, uid, r ? en(errno) : "ok");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    ms_sleep(200);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1)
        nft = argv[1];
    // 手本の Linux は /nft (LINUX_FILES)、aios は /opt/c/bin/nft: あるほうを
    if (access(nft, X_OK) != 0)
        nft = access("/opt/c/bin/nft", X_OK) == 0 ? "/opt/c/bin/nft" : "/nft";
    if (access(nft, X_OK) != 0) {
        printf("nftsk: no %s (skipped)\n", nft);
        return 0;
    }
    // eth0 の SLAAC の番地 (fec0::2 へつなぐのに) を待つ
    for (int i = 0; i < 100; i++) {
        FILE *f = fopen("/proc/net/if_inet6", "r");
        char line[256];
        int ok = 0;
        while (f && fgets(line, sizeof line, f))
            if (strstr(line, "eth0") && strncmp(line, "fe80", 4) && !strstr(line, " 40 "))
                ok = 1;
        if (f)
            fclose(f);
        if (ok)
            break;
        ms_sleep(100);
    }
    nft_run("add table inet sk");
    nft_run("add chain inet sk pre { type filter hook prerouting priority 0 ; }");
    nft_run("add chain inet sk in { type filter hook input priority 0 ; }");
    nft_run("add rule inet sk pre tcp sport 1 meta skuid 0 counter");
    nft_run("add rule inet sk pre tcp sport 1 counter");
    const char *in[] = {
        "iifname eth0 meta nfproto ipv4 tcp sport 1 meta skuid 0 counter",
        "iifname eth0 meta nfproto ipv4 tcp sport 1 meta skuid 4242 counter",
        "iifname eth0 meta nfproto ipv4 tcp sport 1 meta skgid 4243 counter",
        "iifname eth0 meta nfproto ipv4 tcp sport 1 meta skuid != 0 counter",
        "iifname eth0 meta nfproto ipv4 tcp sport 1 counter",
        "iifname eth0 meta nfproto ipv6 tcp sport 1 meta skuid 0 counter",
        "iifname eth0 meta nfproto ipv6 tcp sport 1 meta skuid 4242 counter",
        "iifname eth0 meta nfproto ipv6 tcp sport 1 counter",
        "iifname lo tcp sport 1 meta skuid 0 counter",
        "iifname lo tcp sport 1 counter",
    };
    for (unsigned i = 0; i < sizeof in / sizeof in[0]; i++) {
        char b[256];
        snprintf(b, sizeof b, "add rule inet sk in %s", in[i]);
        nft_run(b);
    }
    try_connect(AF_INET, "10.0.2.2", 0);
    try_connect(AF_INET, "10.0.2.2", 4242);
    try_connect(AF_INET6, "fec0::2", 0);
    try_connect(AF_INET6, "fec0::2", 4242);
    try_connect(AF_INET, "127.0.0.1", 0);
    try_connect(AF_INET6, "::1", 0);
    nft_run("list table inet sk");
    nft_run("delete table inet sk");
    return 0;
}
