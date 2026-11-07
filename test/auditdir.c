// audit の DIR のルール (auditctl -w DIR -p wa: Linux の kernel/audit_tree.c と match_tree_refs) と、終わるときの記録
// (__audit_free: exit_group、シグナルで殺されたとき。success と exit はない)、ほかのパスのシステムコールの名前 (openat2、
// mknodat、utimensat、faccessat、faccessat2、chdir、fchdir、chroot、fchmod、fchown (名前なしの PATH)、statfs、
// inotify_add_watch、execveat)。DIR は木の中で作る・chmod・mkdir・深いところ・木の外へ動かす、木の外のもの、読むだけ。
// 記録は組 READLOG で読み、子のシステムコールの記録とそのなかまだけ出す。inode= dev= pid= ppid= comm= exe= proctitle= と大きな番地の引数は *、exit= の pid は child / parent
// 出すだけのテスト: root で (test/ldiff.sh --root auditdir)
//   aios の中で: sudo ./auditdir      aarch64-linux-gnu-gcc -O2 -static -o auditdir test/auditdir.c
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
#include <signal.h>
#include <time.h>
#include <unistd.h>

static char buf[65536];
static int seqno = 1;
static long s0 = -1;
static int m = -1;
static pid_t main_pid, child_pid;
// いま見せている SYSCALL の記録の audit(...) (なかまの CWD、PATH、EXECVE、PROCTITLE、EOE も見せる)
static char shown[64];

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
        const char *tok[] = {"tty=", "comm=", "exe=", "proctitle=", "inode=", "dev="};
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




static const int calls[] = {SYS_exit_group, SYS_openat2, SYS_mknodat, SYS_utimensat, SYS_faccessat, SYS_faccessat2, SYS_chdir,
                            SYS_fchdir, SYS_chroot, SYS_fchmod, SYS_fchown, SYS_statfs, SYS_inotify_add_watch, SYS_execveat};

// 子のシステムコールのルール (ppid がこのテスト)
static void r_child(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    for (unsigned i = 0; i < sizeof calls / sizeof calls[0]; i++)
        sys(calls[i]);
    field(AUDIT_PPID, AUDIT_EQUAL, main_pid);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "p");
}

static void r_dir(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    for (int i = 0; i < AUDIT_BITMASK_SIZE; i++)
        R->mask[i] = ~0u;
    str(AUDIT_DIR, AUDIT_EQUAL, "/tmp/ad/dir");
    field(AUDIT_PERM, AUDIT_EQUAL, AUDIT_PERM_WRITE | AUDIT_PERM_ATTR);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "dir");
}

// read のルール (殺される子)
static void r_read(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    sys(SYS_read);
    field(AUDIT_PPID, AUDIT_EQUAL, main_pid);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "r");
}

// 子: fd を 3 からにして (システムコールの答えの fd をそろえる)、/tmp/ad で
static void child_start(void) {
    syscall(SYS_close_range, 3, ~0u, 0, 0);
    if (chdir("/tmp/ad"))
        _exit(2);
}

// path を写す (実行するプログラム)
static void copy(const char *from, const char *to) {
    int a = open(from, O_RDONLY), b = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    long n;
    while ((n = read(a, buf, sizeof buf)) > 0)
        if (write(b, buf, n) != n)
            break;
    close(a);
    close(b);
    chmod(to, 0755);
}

// /tmp/ad を空にして消す (シェルのない手本の Linux でも)
static void clean(void) {
    const char *f[] = {"dir/sub/g", "dir/f", "dir/x", "f2", "other", "o2", "fifo", "prog", "u"};
    for (unsigned i = 0; i < sizeof f / sizeof f[0]; i++) {
        char p[64];
        snprintf(p, sizeof p, "/tmp/ad/%s", f[i]);
        unlink(p);
    }
    rmdir("/tmp/ad/dir/sub");
    rmdir("/tmp/ad/dir");
    rmdir("/tmp/ad");
}

static struct {
    unsigned long long flags, mode, resolve;
} how;
static struct statfs_buf {
    char b[256];
} sfs;
static struct timespec ts[2];

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--exec"))
        _exit(0);
    setvbuf(stdout, NULL, _IONBF, 0);
    main_pid = getpid();
    // はじめのディレクトリは動かす道具でちがうので /tmp に (子の chdir の記録の CWD)
    if (chdir("/tmp"))
        return 1;
    clean();
    mkdir("/tmp/ad", 0755);
    mkdir("/tmp/ad/dir", 0755);
    copy("/proc/self/exe", "/tmp/ad/prog");
    int fd = open("/tmp/ad/u", O_WRONLY | O_CREAT, 0644);
    close(fd);
    m = asock(1);
    int s = asock(0);
    set_status(s, AUDIT_STATUS_ENABLED, 1);

    // DIR: /tmp/ad/dir の下
    r_dir();
    add(s, "add dir");
    records("records (dir add)");
    child_pid = fork();
    if (child_pid == 0) {
        child_start();
        syscall(SYS_openat, AT_FDCWD, "/tmp/ad/dir/f", O_WRONLY | O_CREAT, 0644);
        syscall(SYS_openat, AT_FDCWD, "/tmp/ad/other", O_WRONLY | O_CREAT, 0644);
        syscall(SYS_fchmodat, AT_FDCWD, "dir/f", 0600, 0);
        syscall(SYS_mkdirat, AT_FDCWD, "dir/sub", 0755, 0);
        syscall(SYS_openat, AT_FDCWD, "/tmp/ad/dir/sub/g", O_WRONLY | O_CREAT, 0644);
        syscall(SYS_openat, AT_FDCWD, "dir/f", O_RDONLY, 0);
        syscall(SYS_renameat2, AT_FDCWD, "dir/f", AT_FDCWD, "f2", 0);
        syscall(SYS_unlinkat, AT_FDCWD, "f2", 0, 0);
        syscall(SYS_fchmodat, AT_FDCWD, "/tmp/ad/dir", 0700, 0);
        _exit(0);
    }
    waitpid(child_pid, NULL, 0);
    records("records (dir)");
    r_dir();
    send_rule(s, AUDIT_DEL_RULE, "del dir");

    // ほかのシステムコールの名前と、exit_group
    r_child();
    add(s, "add child rule");
    records("records (add)");
    child_pid = fork();
    if (child_pid == 0) {
        child_start();
        how.flags = O_WRONLY | O_CREAT;
        how.mode = 0644;
        syscall(SYS_openat2, AT_FDCWD, "o2", &how, sizeof how);
        how.flags = O_RDONLY;
        how.mode = 0;
        syscall(SYS_openat2, AT_FDCWD, "o2", &how, sizeof how);
        syscall(SYS_mknodat, AT_FDCWD, "fifo", S_IFIFO | 0644, 0);
        syscall(SYS_utimensat, AT_FDCWD, "u", ts, 0);
        syscall(SYS_utimensat, 3, 0, ts, 0);
        syscall(SYS_faccessat, AT_FDCWD, "u", R_OK, 0);
        syscall(SYS_faccessat2, AT_FDCWD, "u", R_OK, AT_SYMLINK_NOFOLLOW);
        syscall(SYS_faccessat, AT_FDCWD, "nothere", R_OK, 0);
        syscall(SYS_statfs, "/tmp/ad", &sfs, 0, 0);
        syscall(SYS_fchmod, 3, 0640, 0, 0);
        syscall(SYS_fchown, 3, 0, 0, 0);
        int in = syscall(SYS_inotify_init1, 0, 0, 0, 0);
        syscall(SYS_inotify_add_watch, in, "u", 2, 0);
        syscall(SYS_chdir, "dir", 0, 0, 0);
        int d = open("/tmp/ad", O_RDONLY | O_DIRECTORY);
        syscall(SYS_fchdir, d, 0, 0, 0);
        syscall(SYS_chroot, "/tmp/ad", 0, 0, 0);
        char *av[] = {"prog", "--exec", NULL}, *ev[] = {NULL};
        if (fork() == 0)
            syscall(SYS_execveat, AT_FDCWD, "prog", av, ev, 0);
        wait(NULL);
        syscall(SYS_exit_group, 3, 0, 0, 0);
    }
    waitpid(child_pid, NULL, 0);
    records("records (child)");
    r_child();
    send_rule(s, AUDIT_DEL_RULE, "del child rule");

    // シグナルで殺された子 (read で待っているところ)
    r_read();
    add(s, "add read rule");
    int pp[2];
    if (pipe(pp))
        return 1;
    child_pid = fork();
    if (child_pid == 0) {
        close(pp[1]);
        // 記録の a0 (fd) がそろうように 50 番で
        dup2(pp[0], 50);
        syscall(SYS_read, 50, buf, 1, 0);
        _exit(0);
    }
    close(pp[0]);
    struct timespec t = {0, 300000000L};
    nanosleep(&t, NULL);
    kill(child_pid, SIGKILL);
    waitpid(child_pid, NULL, 0);
    records("records (killed)");
    r_read();
    send_rule(s, AUDIT_DEL_RULE, "del read rule");
    close(pp[1]);

    set_status(s, AUDIT_STATUS_ENABLED, 0);
    records("records (end)");
    clean();
    return 0;
}
