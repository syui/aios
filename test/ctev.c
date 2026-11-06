// ctnetlink の知らせと作る・変えるもの (Linux の nf_conntrack_netlink.c: ctnetlink_conntrack_event と
// ctnetlink_new_conntrack): conntrack -E (NFNLGRP_CONNTRACK_NEW / UPDATE / DESTROY に入って [NEW] [UPDATE] [DESTROY])
// を動かしたまま、中どうし (lo) の TCP (つないで閉じる) と UDP (答えあり・なし)、conntrack -I (作る。同じものは
// EEXIST)、-U (mark を変える)、-D、-F をして、知らせの並びをくらべる。のこり時間は "*" に
// 出すだけのテスト: root で、手本の Linux は静的な conntrack と nft
//   LINUX_FILES="conntrack=build/conntrack-static nft=build/nft-static" test/ldiff.sh --root ctev
//   aios の中で: sudo ./ctev (conntrack-tools と nftables のパッケージがいる)
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *nft = "/opt/c/bin/nft", *ct = "/opt/c/bin/conntrack";

static void ms_sleep(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

// つながりの行なら、のこり時間 ([NEW] などのあとの 3 つめ) を * に。知らせの portid (道具のソケットの番号) は * に、
// progname (その番号の持ち主: /proc から引けたときだけ) は消す
static void mask(char *l) {
    char *tok[64], copy[1024];
    int n = 0;
    snprintf(copy, sizeof copy, "%s", l);
    for (char *t = strtok(copy, " \t\n"); t && n < 64; t = strtok(NULL, " \t\n")) {
        if (!strncmp(t, "progname=", 9))
            continue;
        if (!strncmp(t, "portid=", 7))
            t = "portid=*";
        tok[n++] = t;
    }
    int base = n > 0 && tok[0][0] == '[' ? 1 : 0;
    if (n < base + 4 || (strcmp(tok[base], "tcp") && strcmp(tok[base], "udp")))
        return;
    int at = base + 2;
    if (strspn(tok[at], "0123456789") != strlen(tok[at]))
        return;
    char out[1024] = "";
    for (int i = 0; i < n; i++) {
        strcat(out, i ? " " : "");
        strcat(out, i == at ? "*" : tok[i]);
    }
    strcpy(l, out);
}

static int cmpstr(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

// conntrack -E をうしろで (出力はファイルに)
static pid_t ev_start(char *path) {
    int fd = mkstemp(path);
    pid_t ev = fork();
    if (ev == 0) {
        dup2(fd, 1);
        dup2(fd, 2);
        execl(ct, ct, "-E", (char *)NULL);
        _exit(127);
    }
    close(fd);
    ms_sleep(500);
    return ev;
}

// -E を止めて、知らせの行を出す (sorted なら並べる)
static void ev_stop(pid_t ev, char *path, const char *what, int sorted) {
    ms_sleep(500);
    kill(ev, SIGTERM);
    waitpid(ev, NULL, 0);
    printf("-- conntrack -E (%s)\n", what);
    FILE *f = fopen(path, "r");
    char line[1024], *lines[256];
    int n = 0;
    while (f && fgets(line, sizeof line, f) && n < 256) {
        if (strspn(line, " \t\n") == strlen(line))
            continue;
        line[strcspn(line, "\n")] = 0;
        mask(line);
        lines[n++] = strdup(line);
    }
    if (f)
        fclose(f);
    unlink(path);
    if (sorted)
        qsort(lines, n - (n > 0 && strstr(lines[n - 1], "events have been shown") ? 1 : 0), sizeof lines[0], cmpstr);
    for (int i = 0; i < n; i++)
        printf("%s\n", lines[i]);
}

// prog ARGS を動かし、出力 (つながりの行ののこり時間は *) と終わりの番号を出す
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
    char l[1024];
    while (fgets(l, sizeof l, f)) {
        if (strspn(l, " \t\n") == strlen(l))
            continue;
        l[strcspn(l, "\n")] = 0;
        mask(l);
        printf("%s\n", l);
    }
    fclose(f);
    int st = 0;
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st))
        printf("-- exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

static int sock_at(int type, int port) {
    int s = socket(AF_INET, type, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port), .sin_addr.s_addr = htonl(0x7f000001)};
    if (bind(s, (struct sockaddr *)&a, sizeof a))
        printf("bind %d: %s\n", port, strerror(errno));
    return s;
}

static void send_to(int s, int port) {
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port), .sin_addr.s_addr = htonl(0x7f000001)};
    sendto(s, "hi", 2, 0, (struct sockaddr *)&a, sizeof a);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1)
        ct = argv[1];
    if (access(ct, X_OK) != 0)
        ct = access("/opt/c/bin/conntrack", X_OK) == 0 ? "/opt/c/bin/conntrack" : "/conntrack";
    if (access(nft, X_OK) != 0)
        nft = "/nft";
    if (access(ct, X_OK) != 0 || access(nft, X_OK) != 0) {
        printf("ctev: no conntrack or nft (skipped)\n");
        return 0;
    }
    run(nft, "add table inet t");
    run(nft, "add chain inet t out { type filter hook output priority 0 ; }");
    run(nft, "add rule inet t out ct state new counter");

    char evf[] = "/tmp/ctev.XXXXXX";
    pid_t ev = ev_start(evf);

    // TCP: 47001 → 7001 をつないで、閉じる
    int l = sock_at(SOCK_STREAM, 7001);
    listen(l, 4);
    int c = sock_at(SOCK_STREAM, 47001);
    struct sockaddr_in to = {.sin_family = AF_INET, .sin_port = htons(7001), .sin_addr.s_addr = htonl(0x7f000001)};
    printf("tcp connect: %s\n", connect(c, (struct sockaddr *)&to, sizeof to) ? strerror(errno) : "ok");
    int a = accept(l, NULL, NULL);
    ms_sleep(200);
    close(c);
    ms_sleep(100);
    close(a);
    ms_sleep(200);
    // UDP: 47002 → 7002 は答えあり、47003 → 7002 は答えなし
    int us = sock_at(SOCK_DGRAM, 7002), u1 = sock_at(SOCK_DGRAM, 47002), u2 = sock_at(SOCK_DGRAM, 47003);
    send_to(u1, 7002);
    ms_sleep(100);
    send_to(us, 47002);
    ms_sleep(100);
    send_to(u2, 7002);
    ms_sleep(200);

    run(ct, "-I -p udp -s 10.9.9.1 -d 10.9.9.2 --sport 1000 --dport 2000 --timeout 100");
    run(ct, "-I -p udp -s 10.9.9.1 -d 10.9.9.2 --sport 1000 --dport 2000 --timeout 100");
    run(ct, "-I -p tcp -s 10.9.9.1 -d 10.9.9.2 --sport 1001 --dport 2001 --state ESTABLISHED --timeout 200 --mark 3");
    run(ct, "-U -p udp -s 127.0.0.1 -d 127.0.0.1 --sport 47002 --dport 7002 --mark 5");
    run(ct, "-G -p udp -s 127.0.0.1 -d 127.0.0.1 --sport 47002 --dport 7002");
    run(ct, "-G -p tcp -s 10.9.9.1 -d 10.9.9.2 --sport 1001 --dport 2001");
    run(ct, "-D -p udp -s 127.0.0.1 -d 127.0.0.1 --sport 47003 --dport 7002");
    ev_stop(ev, evf, "traffic, -I, -U, -D", 0);
    // -F の知らせはハッシュの順なので、別の -E で並べて
    char evf2[] = "/tmp/ctev.XXXXXX";
    ev = ev_start(evf2);
    run(ct, "-F");
    ev_stop(ev, evf2, "-F", 1);
    run(nft, "delete table inet t");
    close(l);
    close(us);
    close(u1);
    close(u2);
    return 0;
}
