// NETLINK_AUDIT (Linux の kernel/audit.c: audit_receive_msg と audit_netlink_ok): AUDIT_GET (struct audit_status)、
// AUDIT_SET (enabled、failure、rate_limit、backlog_limit、pid で auditd になる、LOST)、AUDIT_GET_FEATURE と SET_FEATURE、
// AUDIT_LIST_RULES、AUDIT_TTY_GET と TTY_SET、AUDIT_SIGNAL_INFO (auditd に SIGUSR1 を送った子)、ユーザーの記録
// (AUDIT_USER: auditd がいなければ kmsg に、いればそのソケットに)、組 AUDIT_NLGRP_READLOG (1)、AUDIT_REPLACE、
// AUDIT_CONFIG_CHANGE の記録、権限 (CAP_AUDIT_CONTROL、CAP_AUDIT_WRITE、CAP_AUDIT_READ: なければ EPERM)、知らない種類
// (EINVAL)。組に入る・出る記録 (AUDIT_EVENT_LISTENER)。kmsg の時刻と番号 audit(秒.ミリ秒:番号)、pid= ppid= ses= の数、
// tty= comm= exe= は * に。自分の pid は me に。kmsg へは 5 秒に 10 こまで (printk_ratelimit) なので、ところどころ 5 秒待つ
// 出すだけのテスト: root で (test/ldiff.sh --root audit)
//   aios の中で: sudo ./audit      aarch64-linux-gnu-gcc -O2 -static -o audit test/audit.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/netlink.h>
#include <poll.h>
#include <stdio.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static char buf[65536];

// AUDIT_SIGNAL_INFO の答え (Linux の struct audit_sig_info: uapi にない)
struct sig_info {
    unsigned uid;
    int pid;
    char ctx[];
};
static int seqno = 1;

// printk_ratelimit の窓 (5 秒) を新しく
static void new_window(void) {
    struct timespec t = {5, 200000000L};
    nanosleep(&t, NULL);
}
static pid_t child_pid;

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case EINVAL: return "EINVAL";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ECONNREFUSED: return "ECONNREFUSED";
    case EEXIST: return "EEXIST";
    case EPROTONOSUPPORT: return "EPROTONOSUPPORT";
    case EAGAIN: return "EAGAIN";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "errno%d", e);
        return b;
    }
    }
}

// groups が 0 でなければ組に入る (bind の答えを出す)
static int asock_g(unsigned groups) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_AUDIT);
    if (s < 0) {
        printf("socket: %s\n", en(errno));
        exit(0);
    }
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = groups};
    int r = bind(s, (struct sockaddr *)&a, sizeof a);
    if (groups)
        printf("bind groups=%u: %s\n", groups, r ? en(errno) : "ok");
    return s;
}

static int asock(void) {
    return asock_g(0);
}

static void req(int s, int type, int flags, const void *data, int len) {
    struct {
        struct nlmsghdr h;
        char d[1024];
    } m;
    memset(&m, 0, sizeof m);
    m.h.nlmsg_len = NLMSG_LENGTH(len);
    m.h.nlmsg_type = type;
    m.h.nlmsg_flags = NLM_F_REQUEST | flags;
    m.h.nlmsg_seq = ++seqno;
    memcpy(m.d, data, len);
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    if (sendto(s, &m, m.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k) < 0)
        printf("  send: %s\n", en(errno));
}

// audit の記録の文字: audit(秒.ミリ秒:番号)、pid= ppid= ses= の数、tty= exe= の値を * に。=自分の pid は =me に
static void mask_text(char *t) {
    char out[4096];
    int o = 0;
    char me[16];
    int ml = snprintf(me, sizeof me, "%d", getpid());
    for (int i = 0; t[i] && o < 4000;) {
        if (!strncmp(t + i, "audit(", 6)) {
            o += sprintf(out + o, "audit(*)");
            while (t[i] && t[i] != ')')
                i++;
            if (t[i])
                i++;
            continue;
        }
        int start = i == 0 || t[i - 1] == ' ';
        const char *num[] = {"pid=", "ppid=", "ses="}, *tok[] = {"tty=", "exe=", "comm="};
        int done = 0;
        for (int k = 0; k < 3 && start && !done; k++) {
            int kl = strlen(num[k]);
            if (!strncmp(t + i, num[k], kl)) {
                o += sprintf(out + o, "%s*", num[k]);
                i += kl;
                while (t[i] >= '0' && t[i] <= '9')
                    i++;
                done = 1;
            }
        }
        for (int k = 0; k < 3 && start && !done; k++) {
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
        if (t[i] == '=' && !strncmp(t + i + 1, me, ml) && !(t[i + 1 + ml] >= '0' && t[i + 1 + ml] <= '9')) {
            o += sprintf(out + o, "=me");
            i += 1 + ml;
            continue;
        }
        out[o++] = t[i++];
    }
    out[o] = 0;
    strcpy(t, out);
}

// 答えを ms ミリ秒まで読んで出す
static void replies(int s, int ms, const char *what) {
    printf("%s:\n", what);
    int any = 0;
    for (;;) {
        struct pollfd p = {s, POLLIN, 0};
        if (poll(&p, 1, ms) <= 0)
            break;
        long n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (n <= 0)
            break;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            any = 1;
            void *d = NLMSG_DATA(h);
            int dl = h->nlmsg_len - NLMSG_HDRLEN;
            if (h->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *e = d;
                printf("  ERROR %s (for type %u) flags=%#x\n", en(-e->error), e->msg.nlmsg_type, h->nlmsg_flags);
            } else if (h->nlmsg_type == NLMSG_DONE) {
                printf("  DONE flags=%#x\n", h->nlmsg_flags);
            } else if (h->nlmsg_type == AUDIT_GET && dl >= (int)sizeof(struct audit_status)) {
                struct audit_status *a = d;
                printf("  GET len=%d mask=%u enabled=%u failure=%u pid=%s rate_limit=%u backlog_limit=%u lost=%u backlog=%u feature_bitmap=%#x "
                       "backlog_wait_time=%u actual=%u\n",
                       dl, a->mask, a->enabled, a->failure, a->pid ? (a->pid == (unsigned)getpid() ? "me" : "other") : "0", a->rate_limit,
                       a->backlog_limit, a->lost, a->backlog, a->feature_bitmap, a->backlog_wait_time, a->backlog_wait_time_actual);
            } else if (h->nlmsg_type == AUDIT_GET_FEATURE) {
                struct audit_features *f = d;
                printf("  GET_FEATURE len=%d vers=%u mask=%#x features=%#x lock=%#x\n", dl, f->vers, f->mask, f->features, f->lock);
            } else if (h->nlmsg_type == AUDIT_TTY_GET) {
                struct audit_tty_status *t = d;
                printf("  TTY_GET len=%d enabled=%u log_passwd=%u\n", dl, t->enabled, t->log_passwd);
            } else if (h->nlmsg_type == AUDIT_SIGNAL_INFO) {
                struct sig_info *si = d;
                char p[16];
                snprintf(p, sizeof p, "%d", si->pid);
                printf("  SIGNAL_INFO len=%d uid=%u pid=%s ctx=%.*s\n", dl, si->uid, si->pid > 0 && si->pid == child_pid ? "child" : p,
                       dl - 8 > 0 ? dl - 8 : 0, si->ctx);
            } else if (h->nlmsg_type == AUDIT_REPLACE) {
                unsigned pid = *(unsigned *)d;
                printf("  REPLACE len=%d pid=%s\n", dl, pid == (unsigned)getpid() ? "me" : "other");
            } else if (h->nlmsg_type >= 1100 || h->nlmsg_type == AUDIT_USER) {
                char t[2048];
                snprintf(t, sizeof t, "%.*s", dl, (char *)d);
                mask_text(t);
                printf("  record type=%u pid=%s: %s\n", h->nlmsg_type, h->nlmsg_pid ? "nonzero" : "0", t);
            } else {
                printf("  type %u len %d\n", h->nlmsg_type, dl);
            }
        }
    }
    if (!any)
        printf("  (nothing)\n");
}

// kmsg のうち "audit: " の行 (前に見せたものより後)。Linux は klogctl、aios は /proc/kmsg (ためてあるものをそのまま)
static int seen;
static int klines(char *b, int size) {
    int n = klogctl(3, b, size - 1);
    if (n < 0) {
        int fd = open("/proc/kmsg", O_RDONLY);
        n = 0;
        if (fd >= 0) {
            long k;
            while ((k = read(fd, b + n, size - 1 - n)) > 0)
                n += k;
            close(fd);
        }
    }
    b[n] = 0;
    return n;
}

static void kmsg(const char *what) {
    static char b[1 << 20];
    if (what)
        printf("kmsg (%s):\n", what);
    klines(b, sizeof b);
    int idx = 0, any = 0;
    for (char *line = strtok(b, "\n"); line; line = strtok(NULL, "\n")) {
        char *p = strstr(line, "audit: ");
        if (!p || idx++ < seen || !what)
            continue;
        mask_text(p);
        printf("  %s\n", p);
        any = 1;
    }
    seen = idx;
    if (what && !any)
        printf("  (nothing)\n");
}

static void set_status(int s, unsigned mask, unsigned enabled, unsigned pid, unsigned rate, unsigned backlog, const char *what) {
    struct audit_status a;
    memset(&a, 0, sizeof a);
    a.mask = mask;
    a.enabled = enabled;
    a.pid = pid;
    a.rate_limit = rate;
    a.backlog_limit = backlog;
    req(s, AUDIT_SET, NLM_F_ACK, &a, sizeof a);
    replies(s, 300, what);
}

static void set_failure(int s, unsigned failure, const char *what) {
    struct audit_status a;
    memset(&a, 0, sizeof a);
    a.mask = AUDIT_STATUS_FAILURE;
    a.failure = failure;
    req(s, AUDIT_SET, NLM_F_ACK, &a, sizeof a);
    replies(s, 300, what);
}

static void tty_set(int s, unsigned enabled, unsigned log_passwd, const char *what) {
    struct audit_tty_status t = {enabled, log_passwd};
    req(s, AUDIT_TTY_SET, NLM_F_ACK, &t, sizeof t);
    replies(s, 300, what);
}

static void set_feature(int s, unsigned mask, unsigned features, const char *what) {
    struct audit_features f = {AUDIT_FEATURE_VERSION, mask, features, 0};
    req(s, AUDIT_SET_FEATURE, NLM_F_ACK, &f, sizeof f);
    replies(s, 300, what);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    new_window();
    kmsg(NULL);
    int s = asock();

    req(s, AUDIT_GET, 0, NULL, 0);
    replies(s, 300, "AUDIT_GET");
    req(s, AUDIT_GET, NLM_F_ACK, NULL, 0);
    replies(s, 300, "AUDIT_GET with ACK");
    req(s, AUDIT_GET_FEATURE, 0, NULL, 0);
    replies(s, 300, "AUDIT_GET_FEATURE");
    req(s, AUDIT_LIST_RULES, 0, NULL, 0);
    replies(s, 300, "AUDIT_LIST_RULES");
    req(s, AUDIT_TTY_GET, 0, NULL, 0);
    replies(s, 300, "AUDIT_TTY_GET");
    req(s, AUDIT_SIGNAL_INFO, 0, NULL, 0);
    replies(s, 300, "AUDIT_SIGNAL_INFO");
    req(s, 999, 0, NULL, 0);
    replies(s, 300, "type 999");
    req(s, AUDIT_LIST, 0, NULL, 0);
    replies(s, 300, "AUDIT_LIST (old)");
    req(s, AUDIT_FIRST_USER_MSG + 7, 0, "while disabled", 14);
    replies(s, 300, "user message (disabled)");
    req(s, AUDIT_FIRST_USER_MSG + 7, 0, "x", 1);
    replies(s, 300, "user message (1 byte)");
    req(s, AUDIT_USER, NLM_F_ACK, "dropped", 7);
    replies(s, 300, "AUDIT_USER (disabled)");
    kmsg("disabled");

    // 権限: uid 1000 の子 (cap なし)
    pid_t c = fork();
    if (c == 0) {
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000))
            _exit(1);
        int u = asock();
        req(u, AUDIT_GET, 0, NULL, 0);
        replies(u, 300, "uid 1000 AUDIT_GET");
        req(u, AUDIT_FIRST_USER_MSG + 7, 0, "from user", 9);
        replies(u, 300, "uid 1000 user message");
        req(u, 999, 0, NULL, 0);
        replies(u, 300, "uid 1000 type 999");
        close(asock_g(1));
        int one = 1;
        int g = asock();
        printf("uid 1000 NETLINK_ADD_MEMBERSHIP 1: %s\n", setsockopt(g, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &one, sizeof one) ? en(errno) : "ok");
        _exit(0);
    }
    waitpid(c, NULL, 0);

    set_status(s, AUDIT_STATUS_ENABLED, 3, 0, 0, 0, "AUDIT_SET enabled=3");
    set_status(s, AUDIT_STATUS_ENABLED, 1, 0, 0, 0, "AUDIT_SET enabled=1");
    kmsg("enabled");
    req(s, AUDIT_GET, 0, NULL, 0);
    replies(s, 300, "AUDIT_GET");

    // 組 AUDIT_NLGRP_READLOG (記録の写し: ふつうの長さ)
    int m = asock_g(1);
    int two = 2;
    printf("NETLINK_ADD_MEMBERSHIP 2: %s\n", setsockopt(m, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &two, sizeof two) ? en(errno) : "ok");
    req(s, AUDIT_FIRST_USER_MSG + 7, 0, "hello audit", 11);
    replies(s, 300, "user message");
    req(s, AUDIT_USER, 0, "plain user", 10);
    replies(s, 300, "AUDIT_USER");
    kmsg("user message");
    replies(m, 300, "readlog");
    set_status(s, AUDIT_STATUS_RATE_LIMIT | AUDIT_STATUS_BACKLOG_LIMIT, 0, 0, 100, 128, "AUDIT_SET rate_limit=100 backlog_limit=128");
    kmsg("limits");
    set_failure(s, 3, "AUDIT_SET failure=3");
    set_failure(s, 0, "AUDIT_SET failure=0");
    set_failure(s, 1, "AUDIT_SET failure=1");
    kmsg("failure");

    // TTY の設定と FEATURE
    new_window();
    tty_set(s, 1, 0, "AUDIT_TTY_SET 1 0");
    req(s, AUDIT_TTY_GET, 0, NULL, 0);
    replies(s, 300, "AUDIT_TTY_GET");
    tty_set(s, 2, 0, "AUDIT_TTY_SET 2 0");
    tty_set(s, 0, 0, "AUDIT_TTY_SET 0 0");
    kmsg("tty");
    set_feature(s, 1, 1, "AUDIT_SET_FEATURE only_unset_loginuid=1");
    req(s, AUDIT_GET_FEATURE, 0, NULL, 0);
    replies(s, 300, "AUDIT_GET_FEATURE");
    set_feature(s, 1, 0, "AUDIT_SET_FEATURE only_unset_loginuid=0");
    req(s, AUDIT_SET_FEATURE, NLM_F_ACK, "ab", 2);
    replies(s, 300, "AUDIT_SET_FEATURE short");
    kmsg("feature");
    replies(m, 300, "readlog");

    // auditd になる: 記録はこのソケットに
    set_status(s, AUDIT_STATUS_PID, 0, 1, 0, 0, "AUDIT_SET pid=1");
    set_status(s, AUDIT_STATUS_PID, 0, getpid(), 0, 0, "AUDIT_SET pid=me");
    kmsg("pid");
    req(s, AUDIT_GET, 0, NULL, 0);
    replies(s, 300, "AUDIT_GET");
    req(s, AUDIT_FIRST_USER_MSG + 7, 0, "to auditd", 9);
    replies(s, 300, "user message to auditd");
    // auditd に SIGUSR1 を送った子 (AUDIT_SIGNAL_INFO)
    signal(SIGUSR1, SIG_IGN);
    child_pid = fork();
    if (child_pid == 0) {
        kill(getppid(), SIGUSR1);
        _exit(0);
    }
    waitpid(child_pid, NULL, 0);
    req(s, AUDIT_SIGNAL_INFO, 0, NULL, 0);
    replies(s, 300, "AUDIT_SIGNAL_INFO after SIGUSR1");
    set_status(s, AUDIT_STATUS_PID, 0, getpid(), 0, 0, "AUDIT_SET pid=me again");
    set_status(s, AUDIT_STATUS_PID, 0, 0, 0, 0, "AUDIT_SET pid=0");
    replies(s, 300, "after");
    kmsg("pid 0");
    replies(m, 300, "readlog");

    set_status(s, AUDIT_STATUS_LOST, 0, 0, 0, 0, "AUDIT_SET lost");
    set_status(s, AUDIT_STATUS_RATE_LIMIT | AUDIT_STATUS_BACKLOG_LIMIT | AUDIT_STATUS_ENABLED, 0, 0, 0, 64, "AUDIT_SET back");
    kmsg("back");
    req(s, AUDIT_TRIM, NLM_F_ACK, NULL, 0);
    replies(s, 300, "AUDIT_TRIM (disabled)");
    set_status(s, AUDIT_STATUS_LOST, 0, 0, 0, 0, "AUDIT_SET lost (disabled)");
    kmsg("disabled again");
    replies(m, 300, "readlog");
    req(s, AUDIT_GET, 0, NULL, 0);
    replies(s, 300, "AUDIT_GET");
    return 0;
}
