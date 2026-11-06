// ctnetlink (Linux の net/netfilter/nf_conntrack_netlink.c: NETLINK_NETFILTER の subsys 1) を conntrack (conntrack-tools)
// で: -L (dump。-p、-f ipv6、--state、-o extended)、-G (1 つ)、-D (消す)、-C (数)、-S (CPU ごとの数)、-F (みな消す)。
// つながりは中どうし (lo) の TCP と UDP (IPv4 と IPv6) で、どれもポートを決めておく。のこり時間は "*" に、行は並べる
// (Linux はハッシュの順)
// 出すだけのテスト: root で、手本の Linux は静的な conntrack と nft
//   LINUX_FILES="conntrack=build/conntrack-static nft=build/nft-static" test/ldiff.sh --root ctnl
//   aios の中で: sudo ./ctnl (conntrack-tools と nftables のパッケージがいる)
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

static const char *nft = "/opt/c/bin/nft", *ct = "/opt/c/bin/conntrack";
static int only_cpu0;

static void ms_sleep(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

static int cmpstr(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

// つながりの行なら、のこり時間 (default は 3 つめ、extended は 5 つめ) を * に
static int mask_entry(char *l) {
    char *tok[64];
    int n = 0;
    char copy[1024];
    snprintf(copy, sizeof copy, "%s", l);
    for (char *t = strtok(copy, " \t\n"); t && n < 64; t = strtok(NULL, " \t\n"))
        tok[n++] = t;
    if (n < 4)
        return 0;
    int at;
    if (!strcmp(tok[0], "ipv4") || !strcmp(tok[0], "ipv6"))
        at = 4;
    else if (!strcmp(tok[0], "tcp") || !strcmp(tok[0], "udp") || !strcmp(tok[0], "icmp") || !strcmp(tok[0], "icmpv6"))
        at = 2;
    else
        return 0;
    if (at >= n || strspn(tok[at], "0123456789") != strlen(tok[at]))
        return 0;
    char out[1024] = "";
    for (int i = 0; i < n; i++) {
        strcat(out, i ? " " : "");
        strcat(out, i == at ? "*" : tok[i]);
    }
    strcpy(l, out);
    return 1;
}

// prog ARGS (空白で分ける) を動かし、出力 (つながりの行は並べて) と終わりの番号を出す
static void run(const char *prog, const char *line) {
    char buf[1024], *argv[64];
    int n = 0;
    snprintf(buf, sizeof buf, "%s", line);
    argv[n++] = (char *)prog;
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
        execv(prog, argv);
        printf("exec %s: %s\n", prog, strerror(errno));
        _exit(127);
    }
    close(p[1]);
    printf("-- %s %s\n", strrchr(prog, '/') ? strrchr(prog, '/') + 1 : prog, line);
    FILE *f = fdopen(p[0], "r");
    char l[1024], *ent[256], *rest[64];
    int ne = 0, nr = 0;
    while (fgets(l, sizeof l, f)) {
        if (strspn(l, " \t\n") == strlen(l))
            continue;
        l[strcspn(l, "\n")] = 0;
        if (only_cpu0 && !strncmp(l, "cpu=", 4) && strncmp(l, "cpu=0 ", 6))
            continue;
        if (mask_entry(l) && ne < 256)
            ent[ne++] = strdup(l);
        else if (nr < 64)
            rest[nr++] = strdup(l);
    }
    fclose(f);
    qsort(ent, ne, sizeof ent[0], cmpstr);
    for (int i = 0; i < ne; i++)
        printf("%s\n", ent[i]);
    for (int i = 0; i < nr; i++)
        printf("%s\n", rest[i]);
    int st = 0;
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st))
        printf("-- exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

static int sock_at(int fam, int type, const char *addr, int port) {
    int s = socket(fam, type, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (fam == AF_INET) {
        struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
        inet_pton(AF_INET, addr, &a.sin_addr);
        if (bind(s, (struct sockaddr *)&a, sizeof a))
            printf("bind %s:%d: %s\n", addr, port, strerror(errno));
    } else {
        struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
        inet_pton(AF_INET6, addr, &a.sin6_addr);
        if (bind(s, (struct sockaddr *)&a, sizeof a))
            printf("bind [%s]:%d: %s\n", addr, port, strerror(errno));
    }
    return s;
}

static void send_to(int s, int fam, const char *addr, int port) {
    if (fam == AF_INET) {
        struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
        inet_pton(AF_INET, addr, &a.sin_addr);
        sendto(s, "hi", 2, 0, (struct sockaddr *)&a, sizeof a);
    } else {
        struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
        inet_pton(AF_INET6, addr, &a.sin6_addr);
        sendto(s, "hi", 2, 0, (struct sockaddr *)&a, sizeof a);
    }
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1)
        ct = argv[1];
    // 手本の Linux は /conntrack と /nft (LINUX_FILES)、aios は /opt/c/bin: あるほうを
    if (access(ct, X_OK) != 0)
        ct = access("/opt/c/bin/conntrack", X_OK) == 0 ? "/opt/c/bin/conntrack" : "/conntrack";
    if (access(nft, X_OK) != 0)
        nft = "/nft";
    if (access(ct, X_OK) != 0 || access(nft, X_OK) != 0) {
        printf("ctnl: no conntrack or nft (skipped)\n");
        return 0;
    }
    run(ct, "-C");
    run(ct, "-L");
    // ct の式があるとつながりを見はる
    run(nft, "add table inet t");
    run(nft, "add chain inet t out { type filter hook output priority 0 ; }");
    run(nft, "add rule inet t out ct state new counter");

    // TCP: 127.0.0.1:47001 → 127.0.0.1:7001 (ESTABLISHED)
    int l = sock_at(AF_INET, SOCK_STREAM, "127.0.0.1", 7001);
    listen(l, 4);
    int c = sock_at(AF_INET, SOCK_STREAM, "127.0.0.1", 47001);
    struct sockaddr_in to = {.sin_family = AF_INET, .sin_port = htons(7001)};
    inet_pton(AF_INET, "127.0.0.1", &to.sin_addr);
    printf("tcp connect: %s\n", connect(c, (struct sockaddr *)&to, sizeof to) ? strerror(errno) : "ok");
    int a = accept(l, NULL, NULL);
    // UDP: 47002 → 7002 は答えがある、47003 → 7002 はない。IPv6 の [::1]:47004 → [::1]:7002
    int us = sock_at(AF_INET, SOCK_DGRAM, "127.0.0.1", 7002);
    int u1 = sock_at(AF_INET, SOCK_DGRAM, "127.0.0.1", 47002);
    int u2 = sock_at(AF_INET, SOCK_DGRAM, "127.0.0.1", 47003);
    send_to(u1, AF_INET, "127.0.0.1", 7002);
    send_to(u2, AF_INET, "127.0.0.1", 7002);
    ms_sleep(50);
    send_to(us, AF_INET, "127.0.0.1", 47002);
    int us6 = sock_at(AF_INET6, SOCK_DGRAM, "::1", 7002);
    int u6 = sock_at(AF_INET6, SOCK_DGRAM, "::1", 47004);
    send_to(u6, AF_INET6, "::1", 7002);
    ms_sleep(200);

    run(ct, "-L");
    run(ct, "-L -o extended");
    run(ct, "-L -p udp");
    run(ct, "-L -f ipv6");
    run(ct, "-L -p tcp --state ESTABLISHED");
    run(ct, "-L -p udp --dport 7002 --sport 47003");
    run(ct, "-C");
    // CPU ごとの数: 手本の Linux は CPU 1 つなので cpu=0 の行だけ
    only_cpu0 = 1;
    run(ct, "-S");
    only_cpu0 = 0;
    run(ct, "-G -p udp -s 127.0.0.1 -d 127.0.0.1 --sport 47002 --dport 7002");
    run(ct, "-G -p udp -s 127.0.0.1 -d 127.0.0.1 --sport 47009 --dport 7002");
    run(ct, "-G -p tcp -s 127.0.0.1 -d 127.0.0.1 --sport 47001 --dport 7001");
    run(ct, "-D -p udp -s 127.0.0.1 -d 127.0.0.1 --sport 47003 --dport 7002");
    run(ct, "-D -p udp -s 127.0.0.1 -d 127.0.0.1 --sport 47003 --dport 7002");
    run(ct, "-L -p udp");
    run(ct, "-D -f ipv6 -p udp");
    run(ct, "-L");
    run(ct, "-F");
    run(ct, "-C");
    run(ct, "-L");
    run(nft, "delete table inet t");
    close(a);
    close(c);
    close(l);
    close(us);
    close(u1);
    close(u2);
    close(us6);
    close(u6);
    return 0;
}
