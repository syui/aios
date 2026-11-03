// seccomp の user notification (kernel/src/seccomp.rs の NEW_LISTENER と RET_USER_NOTIF、listener の ioctl): フィルタが
// SECCOMP_RET_USER_NOTIF と答えたシステムコールを、listener の fd を持つ別のプロセスが受け (NOTIF_RECV)、答えを返す
// (NOTIF_SEND: 値、エラー、CONTINUE)。ID_VALID、ADDFD (相手に fd を入れる)、poll、listener を閉じたとき、シグナルで
// 割りこまれたとき (7.4 の SECCOMP_FILTER_FLAG_RESTART_BEFORE_RECV と WAIT_KILLABLE_RECV)、fork した子。
// 出すだけのテスト: 手本の Linux (test/linux-vm.sh exec mainline test/bin/seccompnotify) の出力とくらべる
//   aios の中で: ./seccompnotify      gcc -O2 -static -o seccompnotify test/seccompnotify.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SECCOMP_FILTER_FLAG_TSYNC_ESRCH
#define SECCOMP_FILTER_FLAG_TSYNC_ESRCH (1UL << 4)
#endif
#ifndef SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV
#define SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV (1UL << 5)
#endif
#define RESTART_BEFORE_RECV (1UL << 6)
#define ID_VALID_WRONG_DIR SECCOMP_IOR(2, __u64)
#ifndef SECCOMP_IOCTL_NOTIF_SET_FLAGS
#define SECCOMP_IOCTL_NOTIF_SET_FLAGS SECCOMP_IOW(4, __u64)
#endif

static const char *err(int e) {
    switch (e) {
    case 0: return "0";
    case EINVAL: return "EINVAL";
    case ENOENT: return "ENOENT";
    case EBUSY: return "EBUSY";
    case EBADF: return "EBADF";
    case ENOSYS: return "ENOSYS";
    case EINTR: return "EINTR";
    case EPERM: return "EPERM";
    case EACCES: return "EACCES";
    case EINPROGRESS: return "EINPROGRESS";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case EFAULT: return "EFAULT";
    case ENOTTY: return "ENOTTY";
    default: return strerror(e);
    }
}

static void msleep(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR)
        ;
}

static long seccomp_(unsigned op, unsigned flags, void *a) { return syscall(SYS_seccomp, op, flags, a); }

// getppid を ret にして、ほかは許す
static long install(unsigned flags, unsigned ret) {
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, ret),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog p = {4, f};
    return seccomp_(SECCOMP_SET_MODE_FILTER, flags, &p);
}

static void line(const char *name, long r) {
    if (r < 0)
        printf("%-48s %s\n", name, err(errno));
    else
        printf("%-48s %ld\n", name, r);
}

// ---- 子: フィルタをかけて listener を親にわたし、親の言うとおりにする ----

static volatile int handled;
static void on_usr1(int s) {
    (void)s;
    handled++;
}

static void send_fd(int s, int fd) {
    char c[CMSG_SPACE(sizeof(int))];
    struct iovec iov = {"L", 1};
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = c;
    m.msg_controllen = sizeof c;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &fd, sizeof fd);
    if (sendmsg(s, &m, 0) < 0)
        _exit(90);
}

static int recv_fd(int s) {
    char c[CMSG_SPACE(sizeof(int))], b;
    struct iovec iov = {&b, 1};
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = c;
    m.msg_controllen = sizeof c;
    if (recvmsg(s, &m, 0) != 1)
        return -1;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    int fd = -1;
    if (cm && cm->cmsg_type == SCM_RIGHTS)
        memcpy(&fd, CMSG_DATA(cm), sizeof fd);
    return fd;
}

static void say(int s, const char *fmt, long a, long b, long c) {
    char out[96];
    int n = snprintf(out, sizeof out, fmt, a, b, c);
    if (write(s, out, n) != n)
        _exit(91);
}

// 子の本当の親 (CONTINUE で getppid が本当に動いたか): 子が fork したときの親
static pid_t parent_pid;

static void child(int s, unsigned flags) {
    alarm(30);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;  // SA_RESTART なし
    sigaction(SIGUSR1, &sa, NULL);
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    long fd = install(flags, SECCOMP_RET_USER_NOTIF);
    if (fd < 0) {
        say(s, "install %ld %ld %ld\n", -1, errno, 0);
        _exit(1);
    }
    send_fd(s, (int)fd);
    close((int)fd);
    for (;;) {
        char cmd;
        if (read(s, &cmd, 1) != 1)
            _exit(0);
        if (cmd == 'g' || cmd == 'a' || cmd == 'b') {
            handled = 0;
            errno = 0;
            long r = syscall(SYS_getppid);
            int e = r < 0 ? errno : 0;
            if (cmd == 'g') {
                char out[96];
                int m = snprintf(out, sizeof out, "%ld %s handled %d\n", r == parent_pid ? 1000000 : r, err(e), handled);
                if (write(s, out, m) != m)
                    _exit(91);
            } else {
                // 入れてもらった fd に書く (b は close-on-exec かも)
                long w = r >= 0 ? write((int)r, "z", 1) : -1;
                long ce = r >= 0 ? (fcntl((int)r, F_GETFD) & FD_CLOEXEC) : -1;
                if (r >= 0)
                    close((int)r);
                say(s, "%ld %ld %ld\n", r >= 0 ? 1 : r, w, ce);
            }
        } else if (cmd == 'l') {
            long r = install(SECCOMP_FILTER_FLAG_NEW_LISTENER, SECCOMP_RET_USER_NOTIF);
            say(s, "%ld %ld %ld\n", r < 0 ? -1 : 0, r < 0 ? errno : 0, 0);
        } else if (cmd == 'e') {
            long r = install(0, SECCOMP_RET_ERRNO | EACCES);
            say(s, "%ld %ld %ld\n", r, r < 0 ? errno : 0, 0);
        } else if (cmd == 'k') {
            pid_t g = fork();
            if (g == 0) {
                long r = syscall(SYS_getppid);
                say(s, "%ld %ld %ld\n", (long)getpid(), r, 0);
                _exit(0);
            }
            waitpid(g, NULL, 0);
        } else if (cmd == 'x') {
            _exit(0);
        }
    }
}

struct kid {
    pid_t pid;
    int s;
    int l;
};

static struct kid start(unsigned flags) {
    struct kid k = {0, -1, -1};
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    parent_pid = getpid();
    k.pid = fork();
    if (k.pid == 0) {
        close(sv[0]);
        child(sv[1], flags);
        _exit(0);
    }
    close(sv[1]);
    k.s = sv[0];
    k.l = recv_fd(k.s);
    return k;
}

// 子の答えの 1 行 (3 秒まで。来なければ "(none)")
static void reply(const char *name, struct kid *k) {
    char b[128];
    struct pollfd p = {k->s, POLLIN, 0};
    int n = 0;
    while (n < (int)sizeof b - 1 && poll(&p, 1, 3000) == 1) {
        if (read(k->s, b + n, 1) != 1)
            break;
        if (b[n++] == '\n')
            break;
    }
    b[n] = 0;
    if (n && b[n - 1] == '\n')
        b[n - 1] = 0;
    printf("%-48s %s\n", name, n ? b : "(none)");
}

// 子がもう答えたか (待たずに)
static int replied(struct kid *k, int ms) {
    struct pollfd p = {k->s, POLLIN, 0};
    return poll(&p, 1, ms) == 1;
}

static const char *events(int fd, int ms) {
    static char s[32];
    struct pollfd p = {fd, POLLIN | POLLOUT, 0};
    int r = poll(&p, 1, ms);
    snprintf(s, sizeof s, "%s%s%s%s", r <= 0 ? "-" : "", p.revents & POLLIN ? "IN " : "", p.revents & POLLOUT ? "OUT " : "",
             p.revents & POLLHUP ? "HUP" : "");
    return s;
}

static void cmd(struct kid *k, char c) {
    if (write(k->s, &c, 1) != 1)
        printf("cmd %c: %s\n", c, err(errno));
}

static long recv_(int l, struct seccomp_notif *n) {
    memset(n, 0, sizeof *n);
    return ioctl(l, SECCOMP_IOCTL_NOTIF_RECV, n);
}

static long send_(int l, __u64 id, long val, int error, unsigned flags) {
    struct seccomp_notif_resp r = {id, val, error, flags};
    return ioctl(l, SECCOMP_IOCTL_NOTIF_SEND, &r);
}

static long addfd(int l, __u64 id, unsigned flags, int src, unsigned newfd, unsigned nfl) {
    struct seccomp_notif_addfd a = {id, flags, (unsigned)src, newfd, nfl};
    return ioctl(l, SECCOMP_IOCTL_NOTIF_ADDFD, &a);
}

static void finish(struct kid *k) {
    cmd(k, 'x');
    waitpid(k->pid, NULL, 0);
    close(k->s);
    if (k->l >= 0)
        close(k->l);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);

    // ---- 何も持たないもの ----
    unsigned act = SECCOMP_RET_USER_NOTIF;
    line("GET_ACTION_AVAIL USER_NOTIF", seccomp_(SECCOMP_GET_ACTION_AVAIL, 0, &act));
    struct seccomp_notif_sizes sz = {0};
    long r = seccomp_(SECCOMP_GET_NOTIF_SIZES, 0, &sz);
    printf("%-48s %ld: notif %u resp %u data %u\n", "GET_NOTIF_SIZES", r, sz.seccomp_notif, sz.seccomp_notif_resp, sz.seccomp_data);
    pid_t c = fork();
    if (c == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        line("WAIT_KILLABLE_RECV without NEW_LISTENER", install(SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV, SECCOMP_RET_ALLOW));
        line("RESTART_BEFORE_RECV without NEW_LISTENER", install(RESTART_BEFORE_RECV, SECCOMP_RET_ALLOW));
        line("TSYNC | NEW_LISTENER", install(SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_NEW_LISTENER, SECCOMP_RET_ALLOW));
        line("unknown flag 1<<7", install(1u << 7, SECCOMP_RET_ALLOW));
        long l = install(SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_NEW_LISTENER | SECCOMP_FILTER_FLAG_TSYNC_ESRCH, SECCOMP_RET_ALLOW);
        printf("%-48s %s\n", "TSYNC | NEW_LISTENER | TSYNC_ESRCH", l >= 0 ? "a listener fd" : err(errno));
        char path[64], name[64] = "";
        snprintf(path, sizeof path, "/proc/self/fd/%ld", l);
        ssize_t n = readlink(path, name, sizeof name - 1);
        if (n > 0)
            name[n] = 0;
        printf("%-48s cloexec %d, %s\n", "  the listener", (fcntl((int)l, F_GETFD) & FD_CLOEXEC) != 0, name);
        line("  a second NEW_LISTENER", install(SECCOMP_FILTER_FLAG_NEW_LISTENER, SECCOMP_RET_ALLOW));
        line("  RET_USER_NOTIF without a listener: getppid", install(0, SECCOMP_RET_USER_NOTIF) == 0 ? syscall(SYS_getppid) : -2);
        _exit(0);
    }
    waitpid(c, NULL, 0);

    // ---- 答える: 値、エラー、CONTINUE ----
    struct kid k = start(SECCOMP_FILTER_FLAG_NEW_LISTENER);
    printf("%-48s %s\n", "listener from the child", k.l >= 0 ? "ok" : "none");
    printf("%-48s %s\n", "poll, nothing pending", events(k.l, 0));
    __u64 zero = 0;
    line("ID_VALID 0", ioctl(k.l, SECCOMP_IOCTL_NOTIF_ID_VALID, &zero));
    cmd(&k, 'g');
    printf("%-48s %s\n", "poll, the child called getppid", events(k.l, 3000));
    struct seccomp_notif n;
    memset(&n, 0, sizeof n);
    n.pid = 1;
    line("RECV with a buffer not zeroed", ioctl(k.l, SECCOMP_IOCTL_NOTIF_RECV, &n));
    line("RECV", recv_(k.l, &n));
    __u64 id1 = n.id;
    printf("%-48s pid %s, nr %s, arch %s, flags %u\n", "  the notification", n.pid == (__u32)k.pid ? "the child" : "other",
           n.data.nr == SYS_getppid ? "getppid" : "other", n.data.arch == AUDIT_ARCH_AARCH64 ? "aarch64" : "other", n.flags);
    printf("%-48s %s\n", "poll, received", events(k.l, 0));
    line("ID_VALID", ioctl(k.l, SECCOMP_IOCTL_NOTIF_ID_VALID, &id1));
    line("ID_VALID (old wrong direction)", ioctl(k.l, ID_VALID_WRONG_DIR, &id1));
    line("SEND flags 2", send_(k.l, id1, 0, 0, 2));
    line("SEND CONTINUE with a value", send_(k.l, id1, 1, 0, SECCOMP_USER_NOTIF_FLAG_CONTINUE));
    line("SEND another id", send_(k.l, id1 + 1000, 0, 0, 0));
    line("SEND val 42", send_(k.l, id1, 42, 0, 0));
    reply("  the child's getppid (ret errno handled)", &k);
    line("SEND again", send_(k.l, id1, 43, 0, 0));
    line("ID_VALID after the reply", ioctl(k.l, SECCOMP_IOCTL_NOTIF_ID_VALID, &id1));
    cmd(&k, 'g');
    events(k.l, 3000);
    recv_(k.l, &n);
    printf("%-48s %lld\n", "next id - first id", (long long)(n.id - id1));
    line("SEND error -EPERM", send_(k.l, n.id, 0, -EPERM, 0));
    reply("  the child's getppid", &k);
    cmd(&k, 'g');
    events(k.l, 3000);
    recv_(k.l, &n);
    line("SEND CONTINUE", send_(k.l, n.id, 0, 0, SECCOMP_USER_NOTIF_FLAG_CONTINUE));
    reply("  the child's getppid (1000000: the real one)", &k);
    line("SET_FLAGS SYNC_WAKE_UP", ioctl(k.l, SECCOMP_IOCTL_NOTIF_SET_FLAGS, SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP));
    line("SET_FLAGS 2", ioctl(k.l, SECCOMP_IOCTL_NOTIF_SET_FLAGS, 2));
    line("unknown ioctl", ioctl(k.l, _IO('!', 9), 0));
    cmd(&k, 'l');
    reply("a second NEW_LISTENER in the child", &k);

    // ---- ADDFD ----
    int p[2];
    if (pipe(p) < 0)
        return 1;
    cmd(&k, 'a');
    events(k.l, 3000);
    recv_(k.l, &n);
    line("ADDFD newfd_flags O_NONBLOCK", addfd(k.l, n.id, 0, p[1], 0, O_NONBLOCK));
    line("ADDFD flags 4", addfd(k.l, n.id, 4, p[1], 0, 0));
    line("ADDFD newfd without SETFD", addfd(k.l, n.id, 0, p[1], 50, 0));
    line("ADDFD srcfd -1", addfd(k.l, n.id, 0, -1, 0, 0));
    line("ADDFD another id", addfd(k.l, n.id + 1000, 0, p[1], 0, 0));
    long fd = addfd(k.l, n.id, SECCOMP_ADDFD_FLAG_SEND, p[1], 0, 0);
    printf("%-48s %s\n", "ADDFD SEND", fd >= 0 ? "an fd number" : err(errno));
    reply("  the child (fd, write, cloexec, b)", &k);
    char z = 0;
    printf("%-48s %c\n", "  read from the pipe", read(p[0], &z, 1) == 1 ? z : '-');
    cmd(&k, 'b');
    events(k.l, 3000);
    recv_(k.l, &n);
    line("ADDFD SETFD 50 O_CLOEXEC", addfd(k.l, n.id, SECCOMP_ADDFD_FLAG_SETFD, p[1], 50, O_CLOEXEC));
    line("SEND val 50", send_(k.l, n.id, 50, 0, 0));
    reply("  the child (fd, write, cloexec, b)", &k);
    printf("%-48s %c\n", "  read from the pipe", read(p[0], &z, 1) == 1 ? z : '-');
    line("ADDFD after the reply", addfd(k.l, n.id, 0, p[1], 0, 0));
    close(p[0]);
    close(p[1]);

    // ---- fork した子 (孫) にも同じフィルタ ----
    cmd(&k, 'k');
    events(k.l, 3000);
    recv_(k.l, &n);
    pid_t npid = n.pid;
    send_(k.l, n.id, 5, 0, 0);
    char b[64] = "";
    struct pollfd pf = {k.s, POLLIN, 0};
    int got = 0;
    while (got < (int)sizeof b - 1 && poll(&pf, 1, 3000) == 1 && read(k.s, b + got, 1) == 1 && b[got] != '\n')
        got++;
    b[got] = 0;
    long gpid = 0, gr = 0;
    sscanf(b, "%ld %ld", &gpid, &gr);
    printf("%-48s pid %s, getppid %ld\n", "a grandchild", (long)npid == gpid ? "the grandchild" : "other", gr);

    // ---- ほかのフィルタの ERRNO が勝つ ----
    cmd(&k, 'e');
    reply("child adds ERRNO(EACCES) for getppid", &k);
    cmd(&k, 'g');
    reply("  the child's getppid", &k);
    printf("%-48s %s\n", "  poll", events(k.l, 0));
    finish(&k);

    // ---- listener を閉じる ----
    k = start(SECCOMP_FILTER_FLAG_NEW_LISTENER);
    cmd(&k, 'g');
    events(k.l, 3000);
    close(k.l);
    k.l = -1;
    reply("close the listener while the child waits", &k);
    cmd(&k, 'g');
    reply("  getppid after the listener is gone", &k);
    finish(&k);

    // ---- 子が終わると POLLHUP ----
    k = start(SECCOMP_FILTER_FLAG_NEW_LISTENER);
    cmd(&k, 'x');
    waitpid(k.pid, NULL, 0);
    printf("%-48s %s\n", "poll after the child was reaped", events(k.l, 0));
    line("  RECV", recv_(k.l, &n));
    close(k.s);
    close(k.l);

    // ---- シグナル ----
    const char *names[] = {"no flag", "RESTART_BEFORE_RECV", "WAIT_KILLABLE_RECV"};
    unsigned fl[] = {0, RESTART_BEFORE_RECV, SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV};
    for (int i = 0; i < 3; i++) {
        char nm[80];
        k = start(SECCOMP_FILTER_FLAG_NEW_LISTENER | fl[i]);
        // 受ける前
        cmd(&k, 'g');
        events(k.l, 3000);
        kill(k.pid, SIGUSR1);
        msleep(200);
        snprintf(nm, sizeof nm, "%s: signal before RECV", names[i]);
        if (replied(&k, 0)) {
            reply(nm, &k);
        } else {
            // やりなおした: 新しい通知に答える
            printf("%-48s %s\n", nm, "no answer yet");
            printf("%-48s %s\n", "  poll", events(k.l, 3000));
            long rr = recv_(k.l, &n);
            line("  RECV (the restarted one)", rr);
            if (rr == 0)
                send_(k.l, n.id, 7, 0, 0);
            reply("  the child's getppid", &k);
        }
        printf("%-48s %s\n", "  poll", events(k.l, 0));
        // 受けたあと
        cmd(&k, 'g');
        events(k.l, 3000);
        recv_(k.l, &n);
        kill(k.pid, SIGUSR1);
        msleep(200);
        snprintf(nm, sizeof nm, "%s: signal after RECV", names[i]);
        if (replied(&k, 0)) {
            reply(nm, &k);
            line("  SEND to it", send_(k.l, n.id, 8, 0, 0));
        } else {
            printf("%-48s %s\n", nm, "still waiting");
            line("  SEND val 9", send_(k.l, n.id, 9, 0, 0));
            reply("  the child's getppid", &k);
        }
        finish(&k);
    }
    return 0;
}
