// nftables (kernel/src/nftables.rs): 本物の nft で決まりを入れて、`nft list ruleset` と `nft -j list ruleset` を出す。
// 出すだけのテスト: 手本の Linux 7.3.1 (test/linux.config の nf_tables) の出力とくらべる。root で
//   aios の中で: sudo ./nftables [NFT] (NFT の既定は /opt/c/bin/nft)
//   Linux で: LINUX_FILES="nft=$(nft のバイナリ)" test/linux-vm.sh rootexec latest test/bin/nftables /nft
//   ./nftables NFT --trace NAME   その case を nft --debug=mnl で (netlink のやりとりを見る)
// case ごとに: nft -f (終わりの番号と出力)、list ruleset、list ruleset -a (handle つき)、-j list ruleset、あとで flush ruleset
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *nft = "/opt/c/bin/nft";

struct tc {
    const char *name;
    const char *text;
    // あとで続けて動かす nft の引数 (空白で分ける。NULL なら list ruleset など)
    const char *cmds[8];
};

static const struct tc cases[] = {
    {"empty-table", "table ip t {\n}\n", {NULL}},
    {"tables", "table ip a {\n}\ntable inet b {\n\tcomment \"hello\"\n}\ntable ip6 c {\n}\ntable arp d {\n}\ntable bridge e {\n}\ntable netdev f {\n}\n", {NULL}},
    {"chains",
     "table inet filter {\n"
     "\tchain input {\n\t\ttype filter hook input priority filter; policy drop;\n\t}\n"
     "\tchain forward {\n\t\ttype filter hook forward priority 0; policy accept;\n\t}\n"
     "\tchain output {\n\t\ttype filter hook output priority filter + 10; policy accept;\n\t\tcomment \"out\"\n\t}\n"
     "\tchain mine {\n\t}\n"
     "}\n",
     {NULL}},
    {"rules",
     "table inet filter {\n"
     "\tchain input {\n"
     "\t\ttype filter hook input priority filter; policy drop;\n"
     "\t\tiif \"lo\" accept\n"
     "\t\tip saddr 10.0.0.0/8 tcp dport 22 accept comment \"ssh from lan\"\n"
     "\t\ttcp dport { 80, 443 } counter accept\n"
     "\t\tudp dport 53 accept\n"
     "\t\tip protocol icmp icmp type echo-request limit rate 5/second accept\n"
     "\t\tmeta l4proto tcp tcp flags & (syn|ack) == syn counter jump mine\n"
     "\t\tlog prefix \"drop: \" level info\n"
     "\t\treject with icmp port-unreachable\n"
     "\t}\n"
     "\tchain mine {\n"
     "\t\tip daddr 127.0.0.1 return\n"
     "\t\tip ttl < 5 drop\n"
     "\t\tip daddr 192.168.0.1-192.168.0.9 counter packets 3 bytes 120 accept\n"
     "\t\tquota over 10 mbytes drop\n"
     "\t}\n"
     "}\n",
     {NULL}},
    {"sets",
     "table ip t {\n"
     "\tset blocked {\n\t\ttype ipv4_addr\n\t\telements = { 10.0.0.1, 10.0.0.2 }\n\t}\n"
     "\tset nets {\n\t\ttype ipv4_addr\n\t\tflags interval\n\t\telements = { 10.0.0.0/8, 172.16.0.0/12, 192.168.1.10 }\n\t}\n"
     "\tset ports {\n\t\ttype inet_service\n\t\tflags constant\n\t\telements = { 22, 80, 443 }\n\t}\n"
     "\tset tmp {\n\t\ttype ipv4_addr\n\t\tflags timeout\n\t\ttimeout 1h\n\t\tsize 1024\n\t}\n"
     "\tmap portmap {\n\t\ttype inet_service : verdict\n\t\telements = { 22 : accept, 23 : drop }\n\t}\n"
     "\tchain c {\n"
     "\t\tip saddr @blocked drop\n"
     "\t\tip daddr @nets accept\n"
     "\t\ttcp dport @ports accept\n"
     "\t\ttcp dport vmap @portmap\n"
     "\t\tip saddr { 1.2.3.4, 5.6.7.8 } accept\n"
     "\t}\n"
     "}\n",
     {NULL}},
    {"incremental", "",
     {"add table ip x", "add chain ip x c", "add rule ip x c tcp dport 1 accept", "add rule ip x c tcp dport 3 accept",
      "insert rule ip x c position 3 tcp dport 2 accept", "delete rule ip x c handle 2", "add element ip x s { 1.1.1.1 }", NULL}},
    {"errors",
     "table ip t {\n\tchain c {\n\t\tjump nowhere\n\t}\n}\n",
     {"add table ip t", "create table ip t", "delete table ip nope", "add chain ip nope c", "add rule ip t nope accept",
      "delete chain ip t c", "add rule ip t c ct state established accept", NULL}},
    {"replace",
     "table ip t {\n\tchain c {\n\t\taccept\n\t}\n}\ndelete table ip t\ntable ip t {\n\tchain d {\n\t\tdrop\n\t}\n}\n",
     {"flush table ip t", "delete chain ip t d", NULL}},
};

// nft ARGS を動かし、出力 (標準出力と標準エラー) と終わりの番号を出す
static void run(const char *what, char *const args[]) {
    int p[2];
    if (pipe(p) < 0)
        return;
    pid_t c = fork();
    if (c == 0) {
        dup2(p[1], 1);
        dup2(p[1], 2);
        close(p[0]);
        execv(nft, args);
        printf("exec %s: %s\n", nft, strerror(errno));
        _exit(127);
    }
    close(p[1]);
    printf("-- %s\n", what);
    char b[4096];
    long n;
    while ((n = read(p[0], b, sizeof b)) > 0)
        fwrite(b, 1, n, stdout);
    close(p[0]);
    int st = 0;
    waitpid(c, &st, 0);
    printf("-- exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

static void nft_line(const char *line, const char *debug) {
    char buf[512], *argv[32];
    int n = 0;
    snprintf(buf, sizeof buf, "%s", line);
    argv[n++] = (char *)nft;
    if (debug)
        argv[n++] = (char *)debug;
    for (char *t = strtok(buf, " "); t && n < 31; t = strtok(NULL, " "))
        argv[n++] = t;
    argv[n] = NULL;
    run(line, argv);
}

static void one(const struct tc *t, const char *debug) {
    printf("== %s\n", t->name);
    nft_line("flush ruleset", NULL);
    if (t->text[0]) {
        int fd = open("/tmp/nft-case.nft", O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (write(fd, t->text, strlen(t->text)) < 0)
            perror("write");
        close(fd);
        nft_line("-f /tmp/nft-case.nft", debug);
    }
    for (int i = 0; t->cmds[i]; i++)
        nft_line(t->cmds[i], debug);
    nft_line("list ruleset", debug);
    nft_line("-a list ruleset", NULL);
    nft_line("-j list ruleset", NULL);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1)
        nft = argv[1];
    // nftables のパッケージが入っていなければ何もしない (ap -S nftables)
    if (access(nft, X_OK) != 0) {
        printf("nftables: no %s (skipped)\n", nft);
        return 0;
    }
    if (argc > 3 && !strcmp(argv[2], "--trace")) {
        for (size_t i = 0; i < sizeof cases / sizeof *cases; i++)
            if (!strcmp(cases[i].name, argv[3]))
                one(&cases[i], "--debug=mnl");
        return 0;
    }
    for (size_t i = 0; i < sizeof cases / sizeof *cases; i++)
        one(&cases[i], NULL);
    nft_line("flush ruleset", NULL);
    return 0;
}
