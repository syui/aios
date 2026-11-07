// loginuid と sessionid (Linux の fs/proc/base.c の proc_loginuid_write と kernel/auditsc.c の audit_set_loginuid):
// /proc/self/loginuid に書く (決まっていなければだれでも、決まっていれば CAP_AUDIT_CONTROL、外すのは 4294967295)、
// 書くたびに新しい sessionid、fork で受けつぐ、ほかのプロセスのものは EPERM、途中からの書きこみは EINVAL、数でない
// ものは EINVAL、大きすぎるものは ERANGE、AUDIT_SET_FEATURE の only_unset_loginuid と loginuid_immutable、uid 1000 の
// 子。記録 (AUDIT_LOGIN、ユーザーの記録の auid= ses=) は組 AUDIT_NLGRP_READLOG で読む (kmsg のかぎりがない)。
// 記録を出したシステムコールの終わりの SYSCALL、PROCTITLE、EOE も (audit を on にしたあとに fork した子だけ)。
// sessionid は、はじめに見たものを S にして S+N と出す。pid= と tty= comm= exe= proctitle= は *
// 出すだけのテスト: root で (test/ldiff.sh --root loginuid)
//   aios の中で: sudo ./loginuid      aarch64-linux-gnu-gcc -O2 -static -o loginuid test/loginuid.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/netlink.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static char buf[65536];
static int seqno = 1;
static long s0 = -1;
static int m = -1;
static pid_t main_pid;
// 見せない SYSCALL の記録の audit(...) (そのあとの PROCTITLE と EOE も見せない)
static char hidden[64];

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case EINVAL: return "EINVAL";
    case ERANGE: return "ERANGE";
    case EACCES: return "EACCES";
    case ENOENT: return "ENOENT";
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
    struct {
        struct nlmsghdr h;
        char d[1024];
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
        const char *tok[] = {"tty=", "comm=", "exe=", "proctitle="};
        int done = 0;
        for (int k = 0; k < 4 && start && !done; k++) {
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
        out[o++] = t[i++];
    }
    out[o] = 0;
    strcpy(t, out);
}

// 組 READLOG に来た記録 (と、ほかのソケットなら ACK) を ms ミリ秒まで読んで出す
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
            } else {
                char t[4096];
                snprintf(t, sizeof t, "%.*s", dl, (char *)NLMSG_DATA(h));
                // このテストのはじめのプロセスのシステムコールの記録は見せない: audit を前に on にしたことのある
                // 機械 (同じ VM で audit のテストのあと) では、そのプロセスにもシステムコールの入れものがある
                char stamp[64] = "";
                sscanf(t, "%63[^ ]", stamp);
                char me[32];
                snprintf(me, sizeof me, " pid=%d ", main_pid);
                if (h->nlmsg_type == AUDIT_SYSCALL && strstr(t, me)) {
                    snprintf(hidden, sizeof hidden, "%s", stamp);
                    continue;
                }
                if ((h->nlmsg_type == AUDIT_PROCTITLE || h->nlmsg_type == AUDIT_EOE) && !strcmp(stamp, hidden))
                    continue;
                mask_text(t);
                printf("  record type=%u: %s\n", h->nlmsg_type, t);
            }
        }
    }
}

static void records(const char *what) {
    replies(m, 300, what);
}

static void show(const char *who) {
    char a[64] = "", b[64] = "";
    int fd = open("/proc/self/loginuid", O_RDONLY);
    int n = fd >= 0 ? read(fd, a, sizeof a - 1) : -1;
    if (n >= 0)
        a[n] = 0;
    close(fd);
    fd = open("/proc/self/sessionid", O_RDONLY);
    n = fd >= 0 ? read(fd, b, sizeof b - 1) : -1;
    if (n >= 0)
        b[n] = 0;
    close(fd);
    printf("%s: loginuid=%s sessionid=%s%s\n", who, a, ses(strtoul(b, NULL, 10)), strchr(a, '\n') || strchr(b, '\n') ? " (newline)" : "");
}

// path に s を書く (off なら pwrite)
static void put(const char *path, const char *s, long off) {
    int fd0 = open(path, O_WRONLY);
    if (fd0 < 0) {
        printf("open %s: %s\n", path, en(errno));
        return;
    }
    // SYSCALL の記録の a0 (fd) がそろうように、いつも 50 番で (はじめから開いている fd の数は動かす道具でちがう)
    int fd = dup2(fd0, 50);
    close(fd0);
    long r = off ? pwrite(fd, s, strlen(s), off) : write(fd, s, strlen(s));
    char shown[64];
    snprintf(shown, sizeof shown, "%s", s);
    for (char *c = shown; *c; c++)
        if (*c == '\n')
            *c = '|';
    printf("write \"%s\"%s: %s\n", shown, off ? " at 1" : "", r < 0 ? en(errno) : r == (long)strlen(s) ? "ok" : "short");
    close(fd);
}

static void self(const char *s) {
    put("/proc/self/loginuid", s, 0);
}

static void set_status(int s, unsigned mask, unsigned enabled) {
    struct audit_status a;
    memset(&a, 0, sizeof a);
    a.mask = mask;
    a.enabled = enabled;
    req(s, AUDIT_SET, NLM_F_ACK, &a, sizeof a);
    replies(s, 300, NULL);
}

static void set_feature(int s, unsigned mask, unsigned features) {
    struct audit_features f = {AUDIT_FEATURE_VERSION, mask, features, 0};
    req(s, AUDIT_SET_FEATURE, NLM_F_ACK, &f, sizeof f);
    replies(s, 300, NULL);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    main_pid = getpid();
    show("start");
    char a[64] = "";
    int fd = open("/proc/1/loginuid", O_RDONLY);
    int n = fd >= 0 ? read(fd, a, sizeof a - 1) : -1;
    printf("/proc/1/loginuid: %s\n", n >= 0 ? (a[n] = 0, a) : en(errno));
    close(fd);
    // 組は audit を on にする前に (入ったことの記録が出ない)
    m = asock(1);
    int s = asock(0);
    set_status(s, AUDIT_STATUS_ENABLED, 1);

    pid_t parent = getpid();
    pid_t c = fork();
    if (c == 0) {
        self("1000");
        show("set 1000");
        self("1001");
        show("set 1001");
        self("abc");
        self("1002x");
        self("99999999999");
        self("4294967296");
        self("+1002\n");
        show("set +1002");
        put("/proc/self/loginuid", "1003", 1);
        char path[64];
        snprintf(path, sizeof path, "/proc/%d/loginuid", parent);
        put(path, "1004", 0);
        pid_t g = fork();
        if (g == 0) {
            show("grandchild");
            _exit(0);
        }
        waitpid(g, NULL, 0);
        int u = asock(0);
        req(u, AUDIT_FIRST_USER_MSG + 7, 0, "with auid", 9);
        close(u);
        self("4294967295");
        show("unset");
        self("2000");
        show("set 2000 (unset before)");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    records("records (set, change, unset)");
    show("parent");

    // only_unset_loginuid: 決まっていれば外すだけ
    set_feature(s, 1, 1);
    c = fork();
    if (c == 0) {
        self("2000");
        self("2001");
        self("4294967295");
        self("2002");
        show("only_unset");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    set_feature(s, 1, 0);
    records("records (only_unset_loginuid)");

    // loginuid_immutable: 決まっていれば変えられない (外すのも)
    set_feature(s, 2, 2);
    c = fork();
    if (c == 0) {
        self("2003");
        self("2004");
        self("4294967295");
        show("immutable");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    set_feature(s, 2, 0);
    records("records (loginuid_immutable)");

    // uid 1000 の子 (cap なし): 決まっていなければ書ける、決まっていれば EPERM
    c = fork();
    if (c == 0) {
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000))
            _exit(1);
        prctl(PR_SET_DUMPABLE, 1);
        self("3000");
        show("uid 1000");
        self("3001");
        self("4294967295");
        show("uid 1000 after");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    records("records (uid 1000)");

    set_status(s, AUDIT_STATUS_ENABLED, 0);
    records("records (disable)");
    // off のときは AUDIT_LOGIN を出さない
    c = fork();
    if (c == 0) {
        self("4000");
        show("disabled");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    records("records (disabled)");
    show("end");
    return 0;
}
