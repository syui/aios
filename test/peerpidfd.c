// 相手を pidfd で見る (kernel/src/unix.rs の SO_PEERPIDFD・SO_PASSPIDFD と SCM_PIDFD、7.4 の _THREAD のもの、
// syscall.rs の pidfd_open の PIDFD_THREAD): systemd や dbus-broker が相手のプロセスを番号でなく pidfd で持つため。
// 出すだけのテスト: 手本の Linux (test/linux-vm.sh exec mainline test/bin/peerpidfd) の出力とくらべる。pidfd がどの
// プロセスかは、子なら waitid(P_PIDFD) の si_pid、スレッドなら pidfd_send_signal で起きたハンドラの gettid で見る
//   aios の中で: ./peerpidfd      gcc -O2 -static -pthread -o peerpidfd test/peerpidfd.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SO_PASSPIDFD
#define SO_PASSPIDFD 76
#define SO_PEERPIDFD 77
#endif
#define SO_PASSPIDFD_THREAD_ 86
#define SO_PEERPIDFD_THREAD_ 87
#ifndef SCM_PIDFD
#define SCM_PIDFD 0x04
#endif
#define SCM_PIDFD_THREAD_ 0x05
#define PIDFD_THREAD_ O_EXCL

static const char *err(int e) {
    switch (e) {
    case 0: return "0";
    case ENODATA: return "ENODATA";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ENOPROTOOPT: return "ENOPROTOOPT";
    case ESRCH: return "ESRCH";
    case ENOENT: return "ENOENT";
    case EINVAL: return "EINVAL";
    case EBADF: return "EBADF";
    case EPERM: return "EPERM";
    case EAGAIN: return "EAGAIN";
    default: return strerror(e);
    }
}

static long pidfd_open_(pid_t pid, unsigned flags) { return syscall(SYS_pidfd_open, pid, flags); }
static long pidfd_signal(int fd, int sig) { return syscall(SYS_pidfd_send_signal, fd, sig, NULL, 0); }
static pid_t gettid_(void) { return (pid_t)syscall(SYS_gettid); }

static void msleep(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR)
        ;
}

// pidfd のようす: 開いているか、close-on-exec、F_GETFL の O_EXCL (PIDFD_THREAD) と O_NONBLOCK、生きているか (シグナル 0)、
// poll (POLLIN は終わった、POLLHUP は片づけられた)
static void show_pidfd(const char *name, int fd) {
    if (fd < 0) {
        printf("%-46s no pidfd\n", name);
        return;
    }
    int fl = fcntl(fd, F_GETFL), fdf = fcntl(fd, F_GETFD);
    long r = pidfd_signal(fd, 0);
    struct pollfd p = {fd, POLLIN, 0};
    poll(&p, 1, 0);
    printf("%-46s cloexec %d thread %d nonblock %d, signal 0 %s, poll%s%s\n", name, (fdf & FD_CLOEXEC) != 0, (fl & O_EXCL) != 0,
           (fl & O_NONBLOCK) != 0, err(r < 0 ? errno : 0), p.revents & POLLIN ? " IN" : "", p.revents & POLLHUP ? " HUP" : "");
}

// getsockopt で pidfd を取って (取れなければわけ)、optlen も出す
static int peerpidfd(const char *name, int s, int opt, socklen_t len0) {
    int fd = -1;
    socklen_t len = len0;
    char buf[16];
    memset(buf, 0xff, sizeof buf);
    int r = getsockopt(s, SOL_SOCKET, opt, buf, &len);
    if (r < 0) {
        printf("%-46s %s\n", name, err(errno));
        return -1;
    }
    memcpy(&fd, buf, sizeof fd);
    printf("%-46s 0, optlen %u\n", name, (unsigned)len);
    return fd;
}

static void getint(const char *name, int s, int opt) {
    int v = -1;
    socklen_t len = sizeof v;
    int r = getsockopt(s, SOL_SOCKET, opt, &v, &len);
    if (r < 0)
        printf("%-46s %s\n", name, err(errno));
    else
        printf("%-46s 0, value %d\n", name, v);
}

static void setint(const char *name, int s, int opt, int v) {
    int r = setsockopt(s, SOL_SOCKET, opt, &v, sizeof v);
    printf("%-46s %s\n", name, err(r < 0 ? errno : 0));
}

// ---- スレッド: ハンドラが動いたスレッドを覚える ----

static volatile pid_t hit_tid;
static void on_usr1(int s) {
    (void)s;
    hit_tid = gettid_();
}

static volatile int t_stop;
static volatile pid_t t_tid;
static int t_sock[2];
static int t_mode;  // 0: socketpair を作る、1: 送る、2: もう 1 つ送る
static void *helper(void *a) {
    (void)a;
    if (t_mode == 0)
        socketpair(AF_UNIX, SOCK_STREAM, 0, t_sock);
    else if (send(t_sock[0], t_mode == 1 ? "t" : "uu", t_mode == 1 ? 1 : 2, 0) < 0)
        printf("helper send: %s\n", err(errno));
    // したことが見えてから
    __atomic_store_n(&t_tid, gettid_(), __ATOMIC_RELEASE);
    while (!t_stop)
        msleep(5);
    return NULL;
}

// fd が指すスレッドに SIGUSR1 を送り、ハンドラが動いたのがそのスレッドか
static const char *signal_hits(int fd, pid_t want) {
    hit_tid = 0;
    if (pidfd_signal(fd, SIGUSR1) < 0)
        return err(errno);
    for (int i = 0; i < 200 && !hit_tid; i++)
        msleep(5);
    return hit_tid == want ? "that thread" : hit_tid ? "another thread" : "none";
}

// ---- 付帯データ ----

// recvmsg して付帯データを並べる (SCM_PIDFD のものは pidfd を返す)
static void recv_show(const char *name, int s, size_t space, int *pidfd, int *tpidfd) {
    char data[64], ctl[256];
    struct iovec iov = {data, sizeof data};
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = space ? ctl : NULL;
    m.msg_controllen = space;
    memset(ctl, 0, sizeof ctl);
    if (pidfd)
        *pidfd = -1;
    if (tpidfd)
        *tpidfd = -1;
    ssize_t n = recvmsg(s, &m, MSG_DONTWAIT);
    if (n < 0) {
        printf("%-46s recvmsg %s\n", name, err(errno));
        return;
    }
    printf("%-46s %zd bytes, flags%s, controllen %zu:", name, n, m.msg_flags & MSG_CTRUNC ? " CTRUNC" : "", (size_t)m.msg_controllen);
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level != SOL_SOCKET) {
            printf(" (%d,%d)", c->cmsg_level, c->cmsg_type);
            continue;
        }
        const char *t = c->cmsg_type == SCM_RIGHTS ? "RIGHTS" : c->cmsg_type == SCM_CREDENTIALS ? "CREDENTIALS" : c->cmsg_type == SCM_PIDFD ? "PIDFD" : c->cmsg_type == SCM_PIDFD_THREAD_ ? "PIDFD_THREAD" : "?";
        printf(" %s/%zu", t, (size_t)c->cmsg_len);
        if (c->cmsg_type == SCM_PIDFD || c->cmsg_type == SCM_PIDFD_THREAD_) {
            int fd;
            memcpy(&fd, CMSG_DATA(c), sizeof fd);
            if (fd < 0)
                printf("(%s)", err(-fd));
            if (c->cmsg_type == SCM_PIDFD && pidfd)
                *pidfd = fd;
            else if (c->cmsg_type == SCM_PIDFD_THREAD_ && tpidfd)
                *tpidfd = fd;
            else if (fd >= 0)
                close(fd);
        } else if (c->cmsg_type == SCM_RIGHTS) {
            int fd;
            memcpy(&fd, CMSG_DATA(c), sizeof fd);
            close(fd);
        }
    }
    printf("\n");
}

// pidfd の子が終わるのを待って (WNOWAIT で片づけずに)、それがその子か
static const char *waits_child(int fd, pid_t child) {
    siginfo_t si;
    memset(&si, 0, sizeof si);
    if (waitid(P_PIDFD, fd, &si, WEXITED | WNOWAIT) < 0)
        return err(errno);
    return si.si_pid == child ? "that child" : "another";
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    signal(SIGUSR1, on_usr1);
    pid_t me = getpid();
    (void)me;

    // ---- pidfd_open ----
    printf("%-46s %s\n", "pidfd_open(-1)", err(pidfd_open_(-1, 0) < 0 ? errno : 0));
    printf("%-46s %s\n", "pidfd_open(no such pid)", err(pidfd_open_(999999, 0) < 0 ? errno : 0));
    printf("%-46s %s\n", "pidfd_open(me, flag 1)", err(pidfd_open_(getpid(), 1) < 0 ? errno : 0));
    int fd = (int)pidfd_open_(getpid(), PIDFD_THREAD_ | O_NONBLOCK);
    show_pidfd("pidfd_open(me, THREAD|NONBLOCK)", fd);
    close(fd);
    t_mode = 0;
    t_stop = 0;
    pthread_t th;
    pthread_create(&th, NULL, helper, NULL);
    while (!t_tid)
        msleep(1);
    long r = pidfd_open_(t_tid, 0);
    printf("%-46s %s\n", "pidfd_open(a thread, 0)", err(r < 0 ? errno : 0));
    if (r >= 0)
        close((int)r);
    fd = (int)pidfd_open_(t_tid, PIDFD_THREAD_);
    show_pidfd("pidfd_open(a thread, THREAD)", fd);
    printf("%-46s %s\n", "  its signal goes to", signal_hits(fd, t_tid));
    int tfd = fd;

    // ---- socketpair (スレッドが作ったもの) ----
    int *sp = t_sock;
    fd = peerpidfd("socketpair: SO_PEERPIDFD", sp[0], SO_PEERPIDFD, 4);
    show_pidfd("  pidfd", fd);
    close(fd);
    fd = peerpidfd("socketpair: SO_PEERPIDFD optlen 8", sp[0], SO_PEERPIDFD, 8);
    close(fd);
    fd = peerpidfd("socketpair: SO_PEERPIDFD_THREAD", sp[1], SO_PEERPIDFD_THREAD_, 4);
    show_pidfd("  pidfd", fd);
    printf("%-46s %s\n", "  its signal goes to (the creating thread)", signal_hits(fd, t_tid));
    getint("SO_PASSPIDFD (new)", sp[0], SO_PASSPIDFD);
    getint("SO_PASSPIDFD_THREAD (new)", sp[0], SO_PASSPIDFD_THREAD_);
    setint("set SO_PASSPIDFD 1", sp[0], SO_PASSPIDFD, 1);
    setint("set SO_PASSPIDFD_THREAD 7", sp[0], SO_PASSPIDFD_THREAD_, 7);
    getint("SO_PASSPIDFD", sp[0], SO_PASSPIDFD);
    getint("SO_PASSPIDFD_THREAD", sp[0], SO_PASSPIDFD_THREAD_);
    // スレッドが終わったあとの、スレッドの pidfd
    t_stop = 1;
    pthread_join(th, NULL);
    msleep(50);
    show_pidfd("thread pidfd after the thread ended", tfd);
    show_pidfd("SO_PEERPIDFD_THREAD after the thread ended", fd);
    close(fd);
    close(tfd);
    fd = peerpidfd("SO_PEERPIDFD_THREAD again (thread gone)", sp[1], SO_PEERPIDFD_THREAD_, 4);
    show_pidfd("  pidfd", fd);
    close(fd);
    close(sp[0]);
    close(sp[1]);

    // ---- つながっていないもの、listen、AF_INET ----
    int u = socket(AF_UNIX, SOCK_STREAM, 0);
    peerpidfd("unconnected: SO_PEERPIDFD", u, SO_PEERPIDFD, 4);
    peerpidfd("unconnected: SO_PEERPIDFD_THREAD", u, SO_PEERPIDFD_THREAD_, 4);
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path + 1, sizeof a.sun_path - 1, "peerpidfd-%d", getpid());
    socklen_t alen = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(a.sun_path + 1);
    if (bind(u, (struct sockaddr *)&a, alen) < 0 || listen(u, 4) < 0)
        printf("bind/listen: %s\n", err(errno));
    fd = peerpidfd("listening: SO_PEERPIDFD", u, SO_PEERPIDFD, 4);
    show_pidfd("  pidfd", fd);
    close(fd);
    for (int t = 0; t < 2; t++) {
        int s = socket(AF_INET, t ? SOCK_DGRAM : SOCK_STREAM, 0);
        const char *n = t ? "udp" : "tcp";
        char name[64];
        snprintf(name, sizeof name, "%s: SO_PEERPIDFD", n);
        peerpidfd(name, s, SO_PEERPIDFD, 4);
        snprintf(name, sizeof name, "%s: SO_PEERPIDFD_THREAD", n);
        peerpidfd(name, s, SO_PEERPIDFD_THREAD_, 4);
        snprintf(name, sizeof name, "%s: SO_PASSPIDFD", n);
        getint(name, s, SO_PASSPIDFD);
        snprintf(name, sizeof name, "%s: set SO_PASSPIDFD", n);
        setint(name, s, SO_PASSPIDFD, 1);
        snprintf(name, sizeof name, "%s: set SO_PASSPIDFD_THREAD", n);
        setint(name, s, SO_PASSPIDFD_THREAD_, 1);
        close(s);
    }

    // ---- connect: 受けた口から見た相手は子、子から見た相手は listen したもの ----
    int go[2];
    if (pipe(go) < 0)
        return 1;
    pid_t c = fork();
    if (c == 0) {
        int s = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(s, (struct sockaddr *)&a, alen) < 0)
            _exit(1);
        int pf = -1;
        socklen_t l = sizeof pf;
        int ok = getsockopt(s, SOL_SOCKET, SO_PEERPIDFD, &pf, &l) == 0 && pidfd_signal(pf, 0) == 0;
        char b;
        if (read(go[0], &b, 1) != 1)
            _exit(2);
        _exit(ok ? 0 : 3);
    }
    int acc = accept(u, NULL, NULL);
    fd = peerpidfd("accepted: SO_PEERPIDFD", acc, SO_PEERPIDFD, 4);
    show_pidfd("  pidfd (child running)", fd);
    if (write(go[1], "g", 1) != 1)
        return 1;
    printf("%-46s %s\n", "  waitid(P_PIDFD) finds", waits_child(fd, c));
    show_pidfd("  pidfd (child exited)", fd);
    int st;
    waitpid(c, &st, 0);
    printf("%-46s %d\n", "  child's own SO_PEERPIDFD worked (0)", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    show_pidfd("  pidfd (child reaped)", fd);
    close(fd);
    fd = peerpidfd("accepted: SO_PEERPIDFD after the reap", acc, SO_PEERPIDFD, 4);
    show_pidfd("  pidfd", fd);
    close(fd);
    close(acc);
    close(u);

    // ---- SCM_PIDFD ----
    int d[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, d);
    // 何も頼んでいないときのもの: 資格情報がつかないので、あとで SO_PASSPIDFD にしても SCM_PIDFD はない
    if (send(d[1], "a", 1, 0) < 0)
        return 1;
    setint("dgram: set SO_PASSPIDFD", d[0], SO_PASSPIDFD, 1);
    recv_show("sent before SO_PASSPIDFD", d[0], 256, NULL, NULL);
    // 子が送って終わる
    c = fork();
    if (c == 0) {
        if (send(d[1], "b", 1, 0) < 0)
            _exit(1);
        _exit(0);
    }
    waitid(P_PID, c, &(siginfo_t){0}, WEXITED | WNOWAIT);
    int pf, tpf;
    recv_show("from a child (exited)", d[0], 256, &pf, NULL);
    printf("%-46s %s\n", "  waitid(P_PIDFD) finds", pf >= 0 ? waits_child(pf, c) : "-");
    waitpid(c, NULL, 0);
    show_pidfd("  pidfd (child reaped)", pf);
    close(pf);
    // 片づけたあとに受ける
    c = fork();
    if (c == 0) {
        if (send(d[1], "c", 1, 0) < 0)
            _exit(1);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    recv_show("from a reaped child", d[0], 256, &pf, NULL);
    show_pidfd("  pidfd", pf);
    close(pf);
    // 入れ物の大きさ
    setint("dgram: set SO_PASSCRED", d[0], SO_PASSCRED, 1);
    setint("dgram: set SO_PASSPIDFD_THREAD", d[0], SO_PASSPIDFD_THREAD_, 1);
    size_t spaces[] = {0, CMSG_SPACE(12), CMSG_SPACE(12) + CMSG_LEN(4) - 1, CMSG_SPACE(12) + CMSG_LEN(4), CMSG_SPACE(12) + CMSG_SPACE(4),
                       CMSG_SPACE(12) + CMSG_SPACE(4) + CMSG_LEN(4), 256};
    for (unsigned i = 0; i < sizeof spaces / sizeof spaces[0]; i++) {
        if (send(d[1], "d", 1, 0) < 0)
            return 1;
        char name[64];
        snprintf(name, sizeof name, "creds+pidfd+thread, control %zu", spaces[i]);
        recv_show(name, d[0], spaces[i], &pf, &tpf);
        close(pf);
        close(tpf);
    }
    // SCM_RIGHTS もいっしょに: 並びは CREDENTIALS、RIGHTS、PIDFD、PIDFD_THREAD
    {
        char cbuf[CMSG_SPACE(sizeof(int))];
        struct iovec iov = {"e", 1};
        struct msghdr m = {0};
        m.msg_iov = &iov;
        m.msg_iovlen = 1;
        m.msg_control = cbuf;
        m.msg_controllen = sizeof cbuf;
        struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
        cm->cmsg_level = SOL_SOCKET;
        cm->cmsg_type = SCM_RIGHTS;
        cm->cmsg_len = CMSG_LEN(sizeof(int));
        int one = 0;
        memcpy(CMSG_DATA(cm), &one, sizeof one);
        if (sendmsg(d[1], &m, 0) < 0)
            printf("sendmsg rights: %s\n", err(errno));
        recv_show("with SCM_RIGHTS", d[0], 256, &pf, &tpf);
        close(pf);
        close(tpf);
    }
    // スレッドが送ったもの: SCM_PIDFD_THREAD はそのスレッド
    t_sock[0] = d[1];
    t_mode = 1;
    t_stop = 0;
    t_tid = 0;
    pthread_create(&th, NULL, helper, NULL);
    while (!t_tid)
        msleep(1);
    msleep(50);
    recv_show("sent by a thread", d[0], 256, &pf, &tpf);
    show_pidfd("  SCM_PIDFD_THREAD pidfd", tpf);
    printf("%-46s %s\n", "  its signal goes to", signal_hits(tpf, t_tid));
    t_stop = 1;
    pthread_join(th, NULL);
    close(pf);
    close(tpf);
    close(d[0]);
    close(d[1]);

    // ---- stream: 別のスレッドが書いたものは、SO_PASSPIDFD_THREAD ならつなげない ----
    for (int thr = 0; thr < 2; thr++) {
        int s[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, s);
        setint(thr ? "stream: set SO_PASSPIDFD_THREAD" : "stream: set SO_PASSPIDFD", s[0], thr ? SO_PASSPIDFD_THREAD_ : SO_PASSPIDFD, 1);
        if (send(s[1], "mm", 2, 0) < 0)
            return 1;
        t_sock[0] = s[1];
        t_mode = 2;
        t_stop = 0;
        t_tid = 0;
        pthread_create(&th, NULL, helper, NULL);
        while (!t_tid)
            msleep(1);
        msleep(50);
        recv_show(thr ? "  main + thread writes, thread pidfd" : "  main + thread writes, pidfd", s[0], 256, &pf, &tpf);
        recv_show("  then", s[0], 256, NULL, NULL);
        t_stop = 1;
        pthread_join(th, NULL);
        close(pf);
        close(tpf);
        close(s[0]);
        close(s[1]);
    }
    return 0;
}
