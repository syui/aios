// MLDv2 の報告 (Linux の net/ipv6/mcast.c: igmp6_group_added / igmp6_group_dropped → mld_ifc_event → mld_send_cr):
// eth0 に新しい組ができると CHANGE_TO_EXCLUDE (4)、なくなると CHANGE_TO_INCLUDE (3) の記録を robustness (2) 回、
// ff02::16 へ、hop limit 1、Router Alert つき、送り元は link-local。ff02::1 と ff01:: の組、もうある組 (users が増える
// だけ) は報告しない。出るものをファイアウォール (nftables の output の鎖の counter) で数える
// 出すだけのテスト: root で、手本の Linux は ip=dhcp の virtio-net と静的な nft
//   LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root --net mldrep6
//   aios の中で: sudo ./mldrep6 (nftables のパッケージがいる)
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

// nft ARGS (空白で分ける) を動かし、出力 (handle の番号と空白の行はのぞく) と終わりの番号を出す
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

// eth0 の SLAAC の番地ができて DAD が終わるまで
static int wait_slaac(void) {
    for (int i = 0; i < 300; i++) {
        FILE *f = fopen("/proc/net/if_inet6", "r");
        char line[256];
        int ok = 0, tent = 0;
        while (f && fgets(line, sizeof line, f)) {
            unsigned idx, plen, scope, flags;
            char addr[40], name[32];
            if (sscanf(line, "%39s %x %x %x %x %31s", addr, &idx, &plen, &scope, &flags, name) == 6 && !strcmp(name, "eth0")) {
                if (scope != 0x20)
                    ok = 1;
                if (flags & 0x40)
                    tent = 1;
            }
        }
        if (f)
            fclose(f);
        if (ok && !tent)
            return 1;
        ms_sleep(100);
    }
    return 0;
}

static void member(int s, int opt, const char *grp, int ifindex) {
    struct ipv6_mreq m = {.ipv6mr_interface = ifindex};
    inet_pton(AF_INET6, grp, &m.ipv6mr_multiaddr);
    int r = setsockopt(s, IPPROTO_IPV6, opt, &m, sizeof m);
    printf("%s %s if%d: %s\n", opt == IPV6_JOIN_GROUP ? "join" : "leave", grp, ifindex, r ? strerror(errno) : "ok");
}

static void counters(const char *what, int ms) {
    ms_sleep(ms);
    printf("== %s\n", what);
    nft_run("list chain ip6 mld out");
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1)
        nft = argv[1];
    // 手本の Linux は /nft (LINUX_FILES)、aios は /opt/c/bin/nft: あるほうを
    if (access(nft, X_OK) != 0)
        nft = access("/opt/c/bin/nft", X_OK) == 0 ? "/opt/c/bin/nft" : "/nft";
    if (access(nft, X_OK) != 0) {
        printf("mldrep6: no %s (skipped)\n", nft);
        return 0;
    }
    if (!wait_slaac()) {
        printf("no SLAAC address on eth0\n");
        return 1;
    }
    nft_run("add table ip6 mld");
    nft_run("add chain ip6 mld out { type filter hook output priority 0 ; }");
    nft_run("add rule ip6 mld out icmpv6 type mld2-listener-report counter");
    nft_run("add rule ip6 mld out ip6 daddr ff02::16 ip6 hoplimit 1 ip6 saddr fe80::/64 counter");
    nft_run("add rule ip6 mld out ip6 nexthdr 0 counter");
    nft_run("add rule ip6 mld out icmpv6 type mld2-listener-report @th,64,8 4 counter");
    nft_run("add rule ip6 mld out icmpv6 type mld2-listener-report @th,64,8 3 counter");
    nft_run("add rule ip6 mld out icmpv6 type mld2-listener-report @th,48,16 1 counter");
    counters("baseline", 1500);

    int r1 = socket(AF_INET6, SOCK_DGRAM, 0), r2 = socket(AF_INET6, SOCK_DGRAM, 0);
    member(r1, IPV6_JOIN_GROUP, "ff02::1234", 2);
    counters("join ff02::1234", 2500);
    // もうある組 (ff02::1) と interface-local (ff01::99) と、同じ組にもう 1 つ: 報告しない
    member(r1, IPV6_JOIN_GROUP, "ff02::1", 2);
    member(r1, IPV6_JOIN_GROUP, "ff01::99", 2);
    member(r2, IPV6_JOIN_GROUP, "ff02::1234", 2);
    counters("no new group", 2000);
    // 1 つ出ても組は残る。2 つめが出ると組がなくなる
    member(r1, IPV6_LEAVE_GROUP, "ff02::1234", 2);
    counters("one left", 2000);
    member(r2, IPV6_LEAVE_GROUP, "ff02::1234", 2);
    counters("group gone", 2500);
    // 閉じると出る (ff05::1:3 は site-local の組)
    member(r1, IPV6_JOIN_GROUP, "ff05::1:3", 2);
    counters("join ff05::1:3", 2500);
    close(r1);
    close(r2);
    counters("closed", 2500);
    nft_run("delete table ip6 mld");
    return 0;
}
