// exec のまえに記録を出したスレッドの、exec のあとの PROCTITLE (Linux は新しいプログラムの引数: 前のものを使いつづけない)。
// 子が openat (記録) してから execve し、新しいプログラムがまた openat する。記録は組 READLOG で。はじめのプログラムの
// PROCTITLE (動かす道具で名前がちがう) は *、新しいもの (NEW-TITLE をふくむ) はそのまま
// 出すだけのテスト: root で (test/ldiff.sh --root auditexec)
//   aios の中で: sudo ./auditexec      aarch64-linux-gnu-gcc -O2 -static -o auditexec test/auditexec.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/netlink.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static char buf[65536];
static int seqno = 1;
static long s0 = -1;
static int m = -1;
static pid_t main_pid, child_pid;
// いま見せている SYSCALL の記録の audit(...) (なかまの CWD、PATH、EXECVE、PROCTITLE、EOE も見せる)
static char shown[64];
static struct stat st;
static char lbuf[64];

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case EINVAL: return "EINVAL";
    case ERANGE: return "ERANGE";
    case EACCES: return "EACCES";
    case ENOENT: return "ENOENT";
    case EEXIST: return "EEXIST";
    case ESRCH: return "ESRCH";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "errno%d", e);
        return b;
    }
    }
}

// sessionid を S+N に (4294967295 はそのまま)
static const char *ses(unsigned long v) {
    static char b[4][32];
    static int k;
    char *o = b[k++ % 4];
    if (v == 4294967295UL) {
        snprintf(o, 32, "4294967295");
        return o;
    }
    if (s0 < 0)
        s0 = v;
    snprintf(o, 32, "S+%ld", (long)v - s0);
    return o;
}

static int asock(unsigned groups) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_AUDIT);
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = groups};
    if (s < 0 || bind(s, (struct sockaddr *)&a, sizeof a)) {
        printf("audit socket: %s\n", en(errno));
        exit(0);
    }
    return s;
}

static void req(int s, int type, int flags, const void *data, int len) {
    static struct {
        struct nlmsghdr h;
        char d[8192];
    } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = NLMSG_LENGTH(len);
    q.h.nlmsg_type = type;
    q.h.nlmsg_flags = NLM_F_REQUEST | flags;
    q.h.nlmsg_seq = ++seqno;
    memcpy(q.d, data, len);
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &q, q.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
}

// 記録の文字: audit(...) と pid= の数、tty= comm= exe= proctitle= を *、ses= と old-ses= を S+N に
static void mask_text(char *t) {
    char out[4096];
    int o = 0;
    for (int i = 0; t[i] && o < 4000;) {
        int start = i == 0 || t[i - 1] == ' ';
        if (!strncmp(t + i, "audit(", 6)) {
            o += sprintf(out + o, "audit(*)");
            while (t[i] && t[i] != ')')
                i++;
            if (t[i])
                i++;
            continue;
        }
        if (start && (!strncmp(t + i, "pid=", 4) || !strncmp(t + i, "ppid=", 5))) {
            int kl = t[i] == 'p' && t[i + 1] == 'p' ? 5 : 4;
            o += sprintf(out + o, "%.*s*", kl, t + i);
            i += kl;
            while (t[i] >= '0' && t[i] <= '9')
                i++;
            continue;
        }
        const char *tok[] = {"tty=", "comm=", "exe=", "inode=", "dev=", "XXX"};
        int done = 0;
        for (int k = 0; k < 6 && start && !done; k++) {
            int kl = strlen(tok[k]);
            if (!strncmp(t + i, tok[k], kl)) {
                o += sprintf(out + o, "%s*", tok[k]);
                i += kl;
                while (t[i] && t[i] != ' ')
                    i++;
                done = 1;
            }
        }
        if (done)
            continue;
        if (start && (!strncmp(t + i, "ses=", 4) || !strncmp(t + i, "old-ses=", 8))) {
            int kl = t[i] == 's' ? 4 : 8;
            o += sprintf(out + o, "%.*s%s", kl, t + i, ses(strtoul(t + i + kl, NULL, 10)));
            i += kl;
            while (t[i] >= '0' && t[i] <= '9')
                i++;
            continue;
        }
        // 子の pid は child、このテストの pid は parent に
        int named = 0;
        // exit= の値だけ (syscall= の番号とまちがえない)
        int at_exit = i >= 4 && !strncmp(t + i - 4, "exit", 4);
        for (int k = 0; k < 2 && t[i] == '=' && at_exit && !named; k++) {
            pid_t who = k ? main_pid : child_pid;
            char c[16];
            int cl = snprintf(c, sizeof c, "%d", who);
            if (who > 0 && !strncmp(t + i + 1, c, cl) && !(t[i + 1 + cl] >= '0' && t[i + 1 + cl] <= '9')) {
                o += sprintf(out + o, "=%s", k ? "parent" : "child");
                i += 1 + cl;
                named = 1;
            }
        }
        if (named)
            continue;
        // 引数の a0= から a3= のうち、スタックなどの大きな番地 (9 けた以上で ffffffff で始まらないもの) は *
        if (start && t[i] == 'a' && t[i + 1] >= '0' && t[i + 1] <= '3' && t[i + 2] == '=') {
            int k = i + 3, n = 0;
            while ((t[k + n] >= '0' && t[k + n] <= '9') || (t[k + n] >= 'a' && t[k + n] <= 'f'))
                n++;
            if (n >= 9 && strncmp(t + k, "ffffffff", 8)) {
                o += sprintf(out + o, "%.3s*", t + i);
                i = k + n;
                continue;
            }
        }
        out[o++] = t[i++];
    }
    out[o] = 0;
    strcpy(t, out);
}

static void replies(int s, int ms, const char *what) {
    if (what)
        printf("%s:\n", what);
    for (;;) {
        struct pollfd p = {s, POLLIN, 0};
        if (poll(&p, 1, ms) <= 0)
            break;
        long n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (n <= 0)
            break;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            int dl = h->nlmsg_len - NLMSG_HDRLEN;
            if (h->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *e = NLMSG_DATA(h);
                printf("  ERROR %s (for type %u)\n", en(-e->error), e->msg.nlmsg_type);
                continue;
            }
            char t[4096];
            snprintf(t, sizeof t, "%.*s", dl, (char *)NLMSG_DATA(h));
            char stamp[64] = "";
            sscanf(t, "%63[^ ]", stamp);
            if (h->nlmsg_type == AUDIT_SYSCALL) {
                char kid[32];
                snprintf(kid, sizeof kid, " pid=%d ", child_pid);
                if (child_pid <= 0 || !strstr(t, kid)) {
                    shown[0] = 0;
                    continue;
                }
                snprintf(shown, sizeof shown, "%s", stamp);
            } else if (h->nlmsg_type >= 1300 && h->nlmsg_type < 1400 && h->nlmsg_type != AUDIT_CONFIG_CHANGE) {
                if (strcmp(stamp, shown))
                    continue;
            }
            // PROCTITLE は新しいプログラムのもの (NEW-TITLE の 16 進をふくむ) だけそのまま
            if (h->nlmsg_type == AUDIT_PROCTITLE && !strstr(t, "4E45572D5449544C45")) {
                printf("  record type=%u: proctitle=*\n", h->nlmsg_type);
                continue;
            }
            mask_text(t);
            printf("  record type=%u: %s\n", h->nlmsg_type, t);
        }
    }
}

static void records(const char *what) {
    replies(m, 300, what);
}

// ルールを組み立てる
static char rbuf[sizeof(struct audit_rule_data) + 4096];
static struct audit_rule_data *R = (void *)rbuf;

static void rule(unsigned flags, unsigned action) {
    memset(rbuf, 0, sizeof rbuf);
    R->flags = flags;
    R->action = action;
}

static void sys(int nr) {
    R->mask[nr / 32] |= 1u << (nr % 32);
}

static void field(unsigned type, unsigned op, unsigned val) {
    unsigned i = R->field_count++;
    R->fields[i] = type;
    R->fieldflags[i] = op;
    R->values[i] = val;
}

static void str(unsigned type, unsigned op, const char *s) {
    int len = strlen(s);
    memcpy(R->buf + R->buflen, s, len);
    R->buflen += len;
    field(type, op, len);
}

static void send_rule(int s, int type, const char *what) {
    req(s, type, NLM_F_ACK, R, sizeof *R + R->buflen);
    replies(s, 300, what);
}

static void add(int s, const char *what) {
    send_rule(s, AUDIT_ADD_RULE, what);
}


static void set_status(int s, unsigned mask, unsigned enabled) {
    struct audit_status a;
    memset(&a, 0, sizeof a);
    a.mask = mask;
    a.enabled = enabled;
    req(s, AUDIT_SET, NLM_F_ACK, &a, sizeof a);
    replies(s, 300, NULL);
}




// exec のまえに記録を出したスレッドの、exec のあとの PROCTITLE
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--exec")) {
        // 新しいプログラム: もう 1 度 openat (記録)
        syscall(SYS_openat, AT_FDCWD, "/tmp/pt-x", O_RDONLY, 0);
        _exit(0);
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    main_pid = getpid();
    // 動かす道具でちがうもの (はじめのディレクトリ、プログラムの持ち主) をそろえる
    if (chdir("/tmp"))
        return 1;
    int fd = open("/tmp/pt-x", O_WRONLY | O_CREAT, 0644);
    close(fd);
    int a = open("/proc/self/exe", O_RDONLY), b = open("/tmp/pt-prog", O_WRONLY | O_CREAT | O_TRUNC, 0755);
    long n;
    while ((n = read(a, buf, sizeof buf)) > 0)
        if (write(b, buf, n) != n)
            break;
    close(a);
    close(b);
    chmod("/tmp/pt-prog", 0755);
    m = asock(1);
    int s = asock(0);
    set_status(s, AUDIT_STATUS_ENABLED, 1);
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_openat);
    sys(SYS_execve);
    field(AUDIT_PPID, AUDIT_EQUAL, main_pid);
    add(s, "add");
    child_pid = fork();
    if (child_pid == 0) {
        syscall(SYS_close_range, 3, ~0u, 0, 0);
        syscall(SYS_openat, AT_FDCWD, "/tmp/pt-x", O_RDONLY, 0);
        char *av[] = {"/tmp/pt-prog", "--exec", "NEW-TITLE", NULL}, *ev[] = {NULL};
        syscall(SYS_execve, "/tmp/pt-prog", av, ev, 0);
        _exit(1);
    }
    waitpid(child_pid, NULL, 0);
    records("records");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_openat);
    sys(SYS_execve);
    field(AUDIT_PPID, AUDIT_EQUAL, main_pid);
    send_rule(s, AUDIT_DEL_RULE, "del");
    set_status(s, AUDIT_STATUS_ENABLED, 0);
    unlink("/tmp/pt-x");
    unlink("/tmp/pt-prog");
    return 0;
}
