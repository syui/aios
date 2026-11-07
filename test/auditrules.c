// audit のルール (Linux の kernel/auditfilter.c と auditsc.c の exit のフィルタ): AUDIT_ADD_RULE / DEL_RULE /
// LIST_RULES を struct audit_rule_data で (auditctl -a / -d / -l と同じもの)。まちがったルール (古い ENTRY の組、
// 知らない組と action、多すぎる欄、知らない欄と比べかた、MSGTYPE は user と exclude だけ、bit の比べかたは引数だけ、
// LOGINUID_SET は 0 か 1、key の長さ、WATCH と DIR と EXE の道) は EINVAL、親のディレクトリがなければ ENOENT、同じもの
// は EEXIST、ないものを消すと ENOENT。組ごとの一覧 (PREPEND は前に)、組の印のビット (クラス) は Linux のクラスに。
// ふるまい: user の組の NEVER (その種類のユーザーの記録を捨てる)、exclude の組 (その種類の記録を出さない)、exit の組
// (合うシステムコールの出口で SYSCALL と PROCTITLE と EOE、key=。PREPEND の NEVER が先に合えば出さない)。設定の記録
// (op=add_rule key=... list=N res=1)。記録は組 READLOG で読む。pid= ppid= は *、子の pid は child、sessionid は S+N
// 出すだけのテスト: root で (test/ldiff.sh --root auditrules)
//   aios の中で: sudo ./auditrules      aarch64-linux-gnu-gcc -O2 -static -o auditrules test/auditrules.c
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
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static char buf[65536];
static int seqno = 1;
static long s0 = -1;
static int m = -1;
static pid_t main_pid, child_pid;
static char hidden[64];

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
        // 子の pid は child、このテストの pid は parent に
        int named = 0;
        for (int k = 0; k < 2 && t[i] == '=' && !named; k++) {
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
        out[o++] = t[i++];
    }
    out[o] = 0;
    strcpy(t, out);
}

// 答え (ACK、ルールの一覧) と、組 READLOG に来た記録を ms ミリ秒まで読んで出す
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
            } else if (h->nlmsg_type == NLMSG_DONE) {
                printf("  DONE flags=%#x\n", h->nlmsg_flags);
            } else if (h->nlmsg_type == AUDIT_LIST_RULES) {
                struct audit_rule_data *r = NLMSG_DATA(h);
                printf("  rule len=%d flags=%#x list=%u action=%u fields=%u mask=", dl, h->nlmsg_flags, r->flags, r->action, r->field_count);
                int any = 0;
                for (int i = 0; i < AUDIT_BITMASK_SIZE; i++)
                    if (r->mask[i]) {
                        printf("%s%d:%x", any ? "," : "", i, r->mask[i]);
                        any = 1;
                    }
                if (!any)
                    printf("-");
                for (unsigned i = 0; i < r->field_count && i < AUDIT_MAX_FIELDS; i++)
                    printf(" [%u %#x %u]", r->fields[i], r->fieldflags[i], r->values[i]);
                printf(" buf=%.*s\n", (int)r->buflen, r->buf);
            } else {
                char t[4096];
                snprintf(t, sizeof t, "%.*s", dl, (char *)NLMSG_DATA(h));
                // システムコールの記録は、このテストの子のものだけ見せる (はじめのプロセスは、audit を前に on にした
                // ことのある機械ではシステムコールの入れものがある。機械のほかのプロセスのものもまざる)
                char stamp[64] = "";
                sscanf(t, "%63[^ ]", stamp);
                char kid[32];
                snprintf(kid, sizeof kid, " pid=%d ", child_pid);
                if (h->nlmsg_type == AUDIT_SYSCALL && (child_pid <= 0 || !strstr(t, kid))) {
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

static void list(int s, const char *what) {
    req(s, AUDIT_LIST_RULES, NLM_F_ACK, NULL, 0);
    replies(s, 300, what);
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

// それぞれのルール (消すときにも同じものを作る)
static void r_getpid(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_getpid);
    field(AUDIT_ARCH, AUDIT_EQUAL, 0xc00000b7);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "getpid");
}

static void r_killfail(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_kill);
    field(AUDIT_SUCCESS, AUDIT_EQUAL, 0);
    field(AUDIT_ARG1, AUDIT_EQUAL, 0);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "killfail");
}

static void r_never(void) {
    rule(AUDIT_FILTER_EXIT | AUDIT_FILTER_PREPEND, AUDIT_NEVER);
    sys(SYS_getpid);
    field(AUDIT_ARG0, AUDIT_EQUAL, 99);
}

static void r_user(void) {
    rule(AUDIT_FILTER_USER, AUDIT_NEVER);
    field(AUDIT_MSGTYPE, AUDIT_EQUAL, 1107);
}

static void r_exclude(void) {
    rule(AUDIT_FILTER_EXCLUDE, AUDIT_ALWAYS);
    field(AUDIT_MSGTYPE, AUDIT_EQUAL, AUDIT_FEATURE_CHANGE);
}

static void r_watch(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    for (int i = 0; i < AUDIT_BITMASK_SIZE; i++)
        R->mask[i] = ~0u;
    str(AUDIT_WATCH, AUDIT_EQUAL, "/tmp/auditrules-w");
    field(AUDIT_PERM, AUDIT_EQUAL, AUDIT_PERM_WRITE | AUDIT_PERM_ATTR);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "watch");
}

static void r_task(void) {
    rule(AUDIT_FILTER_TASK, AUDIT_NEVER);
    field(AUDIT_PID, AUDIT_EQUAL, 1);
}

static void r_classes(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    // クラスの印 (いちばん上のビットから: 0 dir_write、2 chattr、4 read、6 write、8 signal)
    R->mask[63] = 1u << 31 | 1u << 29 | 1u << 27 | 1u << 25 | 1u << 23;
    field(AUDIT_UID, AUDIT_EQUAL, 4242);
}

static void r_exe(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_getppid);
    str(AUDIT_EXE, AUDIT_EQUAL, "/tmp/auditrules-exe");
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    main_pid = getpid();
    // 組は audit を on にする前に (入ったことの記録が出ない)
    m = asock(1);
    int s = asock(0);
    set_status(s, AUDIT_STATUS_ENABLED, 1);
    list(s, "list (empty)");

    // まちがったルール
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    req(s, AUDIT_ADD_RULE, NLM_F_ACK, R, 100);
    replies(s, 300, "short");
    rule(AUDIT_FILTER_ENTRY, AUDIT_ALWAYS);
    add(s, "ENTRY list");
    rule(9, AUDIT_ALWAYS);
    add(s, "list 9");
    rule(AUDIT_FILTER_EXIT, AUDIT_POSSIBLE);
    add(s, "action POSSIBLE");
    rule(AUDIT_FILTER_EXIT, 3);
    add(s, "action 3");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    R->field_count = 65;
    add(s, "65 fields");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_PID, 0, 1);
    add(s, "op 0");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_MSGTYPE, AUDIT_EQUAL, 1107);
    add(s, "msgtype on exit");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_UID, AUDIT_BIT_MASK, 1);
    add(s, "bitmask on uid");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(999, AUDIT_EQUAL, 1);
    add(s, "field 999");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_LOGINUID_SET, AUDIT_EQUAL, 2);
    add(s, "loginuid_set 2");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_ARCH, AUDIT_LESS_THAN, 1);
    add(s, "arch <");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_FILTERKEY, AUDIT_EQUAL, 0);
    add(s, "key len 0");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_FILTERKEY, AUDIT_EQUAL, 10);
    add(s, "key past the end");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    for (int i = 0; i < 300; i++)
        R->buf[i] = 'k';
    R->buflen = 300;
    field(AUDIT_FILTERKEY, AUDIT_EQUAL, 300);
    add(s, "key 300");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "a");
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "b");
    add(s, "two keys");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_WATCH, AUDIT_EQUAL, "tmp/x");
    add(s, "watch relative");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_WATCH, AUDIT_EQUAL, "/tmp/");
    add(s, "watch trailing slash");
    rule(AUDIT_FILTER_USER, AUDIT_ALWAYS);
    str(AUDIT_WATCH, AUDIT_EQUAL, "/tmp/x");
    add(s, "watch on user");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_WATCH, AUDIT_NOT_EQUAL, "/tmp/x");
    add(s, "watch !=");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_WATCH, AUDIT_EQUAL, "/nonexistent-auditrules/x");
    add(s, "watch no parent");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_DIR, AUDIT_EQUAL, "/nonexistent-auditrules");
    add(s, "dir missing");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_EXE, AUDIT_EQUAL, "bin/true");
    add(s, "exe relative");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    str(AUDIT_EXE, AUDIT_EQUAL, "/nonexistent-auditrules/x");
    add(s, "exe no parent");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_PERM, AUDIT_EQUAL, 16);
    add(s, "perm 16");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_FSTYPE, AUDIT_EQUAL, 1);
    add(s, "fstype on exit");
    records("records (invalid)");

    // ルールを足す
    r_getpid();
    add(s, "add getpid");
    r_getpid();
    add(s, "add getpid again");
    r_killfail();
    add(s, "add killfail");
    r_never();
    add(s, "add never (prepend)");
    r_user();
    add(s, "add user never 1107");
    r_exclude();
    add(s, "add exclude 1328");
    r_watch();
    add(s, "add watch");
    r_task();
    add(s, "add task never");
    r_classes();
    add(s, "add classes");
    r_exe();
    add(s, "add exe");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_getppid);
    field(AUDIT_LOGINUID, AUDIT_EQUAL, 4294967295u);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "legacy");
    add(s, "add loginuid=-1 (legacy)");
    records("records (add)");
    list(s, "list");

    // ふるまい
    req(s, AUDIT_FIRST_USER_MSG + 7, 0, "dropped", 7);
    req(s, AUDIT_FIRST_USER_MSG, 0, "kept", 4);
    set_feature(s, 1, 1);
    set_feature(s, 1, 0);
    records("records (user and exclude)");
    child_pid = fork();
    if (child_pid == 0) {
        long self = syscall(SYS_getpid, 1, 2, 3, 4);
        syscall(SYS_getpid, 99, 2, 3, 4);
        syscall(SYS_getppid, 1, 2, 3, 4);
        syscall(SYS_kill, 999999, 0, 7, 8);
        syscall(SYS_kill, self, 0, 7, 8);
        _exit(0);
    }
    waitpid(child_pid, NULL, 0);
    records("records (syscalls)");

    // 消す
    r_getpid();
    send_rule(s, AUDIT_DEL_RULE, "del getpid");
    r_getpid();
    send_rule(s, AUDIT_DEL_RULE, "del getpid again");
    r_killfail();
    send_rule(s, AUDIT_DEL_RULE, "del killfail");
    r_never();
    send_rule(s, AUDIT_DEL_RULE, "del never");
    r_user();
    send_rule(s, AUDIT_DEL_RULE, "del user");
    r_exclude();
    send_rule(s, AUDIT_DEL_RULE, "del exclude");
    r_watch();
    send_rule(s, AUDIT_DEL_RULE, "del watch");
    r_task();
    send_rule(s, AUDIT_DEL_RULE, "del task");
    r_classes();
    send_rule(s, AUDIT_DEL_RULE, "del classes (as sent)");
    r_exe();
    send_rule(s, AUDIT_DEL_RULE, "del exe");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_getppid);
    field(AUDIT_LOGINUID, AUDIT_EQUAL, 4294967295u);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "legacy");
    send_rule(s, AUDIT_DEL_RULE, "del legacy");
    records("records (del)");
    list(s, "list (after del)");
    // のこったもの (PREPEND は覚えるときに消えるので、つけずに。値を見ずに足されたもの) を消す
    rule(AUDIT_FILTER_EXIT, AUDIT_NEVER);
    sys(SYS_getpid);
    field(AUDIT_ARG0, AUDIT_EQUAL, 99);
    send_rule(s, AUDIT_DEL_RULE, "del never (without prepend)");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_LOGINUID_SET, AUDIT_EQUAL, 2);
    send_rule(s, AUDIT_DEL_RULE, "del loginuid_set 2");
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    field(AUDIT_PERM, AUDIT_EQUAL, 16);
    send_rule(s, AUDIT_DEL_RULE, "del perm 16");
    records("records (cleanup)");
    list(s, "list (clean)");
    child_pid = fork();
    if (child_pid == 0) {
        syscall(SYS_getpid, 1, 2, 3, 4);
        _exit(0);
    }
    waitpid(child_pid, NULL, 0);
    records("records (no rules)");
    set_status(s, AUDIT_STATUS_ENABLED, 0);
    records("records (disable)");
    return 0;
}
