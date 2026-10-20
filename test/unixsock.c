// AF_UNIX のソケット (kernel/src/unix.rs) を、プロセスをまたいで: 待って起こされる read / accept / poll / epoll、
// SO_RCVTIMEO、MSG_WAITALL、大きな stream (1 MiB)、SCM_RIGHTS で fd を渡す、SO_PEERCRED、shutdown と閉じたあと
// (EOF、EPIPE、ECONNRESET)、SOCK_SEQPACKET の区切り、dgram のいっぱいの行列と POLLOUT、送られている途中の輪の
// 片づけ (gc)。ロボット (test/sysdiff.c) は 1 つのプロセスで待たないものだけなので、こちらで。
// aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o unixsock test/unixsock.c; ./unixsock。
// Linux でも同じ答え (test/linux-vm.sh exec latest ...)。しくじった数を終了コードで
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails, checks;

static void check(const char *what, long got, long want) {
    checks++;
    if (got != want) {
        fails++;
        printf("FAIL %s: got %ld, want %ld\n", what, got, want);
    }
}

static void msleep(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR)
        ;
}

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static int reap(pid_t pid) {
    int st;
    while (waitpid(pid, &st, 0) < 0 && errno == EINTR)
        ;
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

// 抽象名前空間の名前 (先頭が NUL)
static socklen_t abstract(struct sockaddr_un *a, const char *name) {
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    strcpy(a->sun_path + 1, name);
    return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name));
}

static int send_fd(int s, int fd) {
    char c = 'f';
    struct iovec iov = {&c, 1};
    union {
        char b[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } u;
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    struct cmsghdr *c1 = CMSG_FIRSTHDR(&m);
    c1->cmsg_level = SOL_SOCKET;
    c1->cmsg_type = SCM_RIGHTS;
    c1->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c1), &fd, sizeof fd);
    return (int)sendmsg(s, &m, 0);
}

static int recv_fd(int s) {
    char c;
    struct iovec iov = {&c, 1};
    union {
        char b[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } u;
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    if (recvmsg(s, &m, MSG_CMSG_CLOEXEC) != 1)
        return -1;
    struct cmsghdr *c1 = CMSG_FIRSTHDR(&m);
    if (!c1 || c1->cmsg_type != SCM_RIGHTS)
        return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(c1), sizeof fd);
    return fd;
}

// 子が眠って待つ read を、親の write が起こす
static void wake_read(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        char b[16];
        long t = now_ms();
        ssize_t n = read(sv[1], b, sizeof b);
        _exit(n == 5 && memcmp(b, "hello", 5) == 0 && now_ms() - t >= 50 ? 0 : 1);
    }
    msleep(100);
    check("write to a sleeping reader", write(sv[0], "hello", 5), 5);
    check("the reader woke with the data", reap(pid), 0);
    close(sv[0]);
    close(sv[1]);
}

// accept が待ち、つないできた子の名前と資格情報
static void wake_accept(void) {
    struct sockaddr_un a, p;
    socklen_t al = abstract(&a, "aios-unixsock-accept");
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    check("bind", bind(l, (struct sockaddr *)&a, al), 0);
    check("listen", listen(l, 4), 0);
    pid_t pid = fork();
    if (pid == 0) {
        msleep(100);
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un me;
        socklen_t ml = abstract(&me, "aios-unixsock-client");
        bind(c, (struct sockaddr *)&me, ml);
        if (connect(c, (struct sockaddr *)&a, al) < 0)
            _exit(1);
        char b;
        _exit(read(c, &b, 1) == 1 && b == 'k' ? 0 : 2);
    }
    socklen_t pl = sizeof p;
    int s = accept(l, (struct sockaddr *)&p, &pl);
    check("accept waited for the client", s >= 0, 1);
    check("accept: the client's name", pl == offsetof(struct sockaddr_un, sun_path) + 1 + strlen("aios-unixsock-client") && strcmp(p.sun_path + 1, "aios-unixsock-client") == 0, 1);
    struct ucred cr;
    socklen_t cl = sizeof cr;
    check("SO_PEERCRED", getsockopt(s, SOL_SOCKET, SO_PEERCRED, &cr, &cl), 0);
    check("SO_PEERCRED pid is the client", cr.pid, pid);
    check("SO_PEERCRED uid", cr.uid, geteuid());
    check("write to the client", write(s, "k", 1), 1);
    check("client", reap(pid), 0);
    close(s);
    close(l);
}

// poll と epoll (EPOLLET) が、ほかのプロセスの write で起きる
static void wake_poll(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        msleep(100);
        write(sv[1], "x", 1);
        msleep(100);
        write(sv[1], "y", 1);
        _exit(0);
    }
    struct pollfd p = {sv[0], POLLIN, 0};
    check("poll waits for POLLIN", poll(&p, 1, 5000), 1);
    check("poll revents", p.revents, POLLIN);
    char b[4];
    check("read", read(sv[0], b, sizeof b), 1);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.fd = sv[0]}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, sv[0], &ev);
    check("epoll waits", epoll_wait(ep, &out, 1, 5000), 1);
    check("epoll event", out.events, EPOLLIN);
    check("EPOLLET: nothing new", epoll_wait(ep, &out, 1, 50), 0);
    check("child", reap(pid), 0);
    close(ep);
    close(sv[0]);
    close(sv[1]);
}

// SO_RCVTIMEO: 来なければ時間切れで EAGAIN
static void rcvtimeo(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);
    struct timeval tv = {0, 150000};
    check("SO_RCVTIMEO", setsockopt(sv[0], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv), 0);
    char b[4];
    long t = now_ms();
    errno = 0;
    check("recv times out", recv(sv[0], b, sizeof b, 0), -1);
    check("errno EAGAIN", errno, EAGAIN);
    long dt = now_ms() - t;
    check("after about 150 ms", dt >= 140 && dt < 2000, 1);
    close(sv[0]);
    close(sv[1]);
}

// MSG_WAITALL は、何回かに分けて来たものを全部そろえる。1 MiB の stream は欠けずに届く
static void big_stream(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        close(sv[0]);
        char b[4000];
        memset(b, 'w', sizeof b);
        for (int i = 0; i < 3; i++) {
            msleep(30);
            write(sv[1], b, sizeof b);
        }
        static unsigned char big[1 << 20];
        for (size_t i = 0; i < sizeof big; i++)
            big[i] = (unsigned char)(i * 131 + 7);
        _exit(write(sv[1], big, sizeof big) == (ssize_t)sizeof big ? 0 : 1);
    }
    close(sv[1]);
    static char w[12000];
    check("MSG_WAITALL gets all 12000", recv(sv[0], w, sizeof w, MSG_WAITALL), 12000);
    static unsigned char got[1 << 20];
    size_t n = 0;
    while (n < sizeof got) {
        ssize_t k = read(sv[0], got + n, sizeof got - n);
        if (k <= 0)
            break;
        n += (size_t)k;
    }
    check("1 MiB arrived", (long)n, 1 << 20);
    int bad = 0;
    for (size_t i = 0; i < n; i++)
        bad += got[i] != (unsigned char)(i * 131 + 7);
    check("1 MiB in order", bad, 0);
    check("EOF after the writer exits", read(sv[0], got, 1), 0);
    check("writer", reap(pid), 0);
    close(sv[0]);
}

// SCM_RIGHTS: 親がパイプの書く口を子へ渡し、子がそれに書く
static void pass_fd(void) {
    int sv[2], pp[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pipe(pp);
    pid_t pid = fork();
    if (pid == 0) {
        close(pp[0]);
        close(pp[1]);
        int fd = recv_fd(sv[1]);
        if (fd < 0)
            _exit(1);
        if (!(fcntl(fd, F_GETFD) & FD_CLOEXEC))
            _exit(2);
        _exit(write(fd, "via-fd", 6) == 6 ? 0 : 3);
    }
    check("send the pipe's write end", send_fd(sv[0], pp[1]), 1);
    close(pp[1]);
    char b[16];
    check("read what the child wrote through the passed fd", read(pp[0], b, sizeof b), 6);
    check("EOF once the child's copy closes", read(pp[0], b, sizeof b), 0);
    check("child", reap(pid), 0);
    close(pp[0]);
    close(sv[0]);
    close(sv[1]);
}

// 閉じたあと: 読むほうは EOF、書くほうは EPIPE (SIGPIPE を無視していれば)。読まれずに閉じたら ECONNRESET
static void after_close(void) {
    signal(SIGPIPE, SIG_IGN);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    check("shutdown SHUT_WR", shutdown(sv[0], SHUT_WR), 0);
    char b[4];
    check("peer reads EOF after SHUT_WR", read(sv[1], b, sizeof b), 0);
    errno = 0;
    check("write after SHUT_WR", write(sv[0], "a", 1), -1);
    check("EPIPE", errno, EPIPE);
    check("the other way still works", write(sv[1], "b", 1), 1);
    check("read it", read(sv[0], b, sizeof b), 1);
    close(sv[0]);
    close(sv[1]);
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    write(sv[0], "unread", 6);
    close(sv[1]);
    errno = 0;
    check("read after the peer closed with unread data", read(sv[0], b, sizeof b), -1);
    check("ECONNRESET", errno, ECONNRESET);
    check("then EOF", read(sv[0], b, sizeof b), 0);
    close(sv[0]);
    // listen しているものが accept せずに閉じた: つないだほうは ECONNRESET
    struct sockaddr_un a;
    socklen_t al = abstract(&a, "aios-unixsock-gone");
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(l, (struct sockaddr *)&a, al);
    listen(l, 1);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    check("connect", connect(c, (struct sockaddr *)&a, al), 0);
    close(l);
    errno = 0;
    check("read after the listener closed", read(c, b, sizeof b), -1);
    check("ECONNRESET", errno, ECONNRESET);
    close(c);
    signal(SIGPIPE, SIG_DFL);
}

// SOCK_SEQPACKET はメッセージの区切りを保つ (入れ物が大きくても 1 つずつ、小さければ切れる)
static void seqpacket(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        write(sv[1], "one", 3);
        write(sv[1], "three", 5);
        write(sv[1], "", 0);
        write(sv[1], "truncated", 9);
        _exit(0);
    }
    char b[64];
    check("seqpacket 1", read(sv[0], b, sizeof b), 3);
    check("seqpacket 2", read(sv[0], b, sizeof b), 5);
    check("seqpacket empty message", read(sv[0], b, sizeof b), 0);
    struct iovec iov = {b, 4};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1};
    check("seqpacket truncated", recvmsg(sv[0], &m, 0), 4);
    check("MSG_TRUNC", m.msg_flags & MSG_TRUNC, MSG_TRUNC);
    check("child", reap(pid), 0);
    close(sv[0]);
    close(sv[1]);
}

// dgram: つないだ相手の行列がいっぱい (max_dgram_qlen 10 をこえる) なら POLLOUT なしで EAGAIN、読まれたら書ける
static void dgram_full(void) {
    struct sockaddr_un a;
    socklen_t al = abstract(&a, "aios-unixsock-full");
    int r = socket(AF_UNIX, SOCK_DGRAM, 0);
    bind(r, (struct sockaddr *)&a, al);
    int w = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    check("connect", connect(w, (struct sockaddr *)&a, al), 0);
    int sent = 0;
    while (send(w, "d", 1, 0) == 1 && sent < 100)
        sent++;
    check("queue stops after max_dgram_qlen + 1", sent, 11);
    check("then EAGAIN", errno, EAGAIN);
    struct pollfd p = {w, POLLOUT, 0};
    check("no POLLOUT while the peer's queue is full", poll(&p, 1, 0), 0);
    char b[4];
    pid_t pid = fork();
    if (pid == 0) {
        msleep(100);
        _exit(recv(r, b, sizeof b, 0) == 1 ? 0 : 1);
    }
    check("POLLOUT once the peer reads", poll(&p, 1, 5000), 1);
    check("revents", p.revents, POLLOUT);
    check("reader", reap(pid), 0);
    close(w);
    close(r);
}

// 自分の fd を自分の行列に入れて閉じたソケットは、ほかのソケットが閉じたときに片づく (名前が使えるようになる)
static void gc(void) {
    struct sockaddr_un a;
    socklen_t al = abstract(&a, "aios-unixsock-gc");
    int s = socket(AF_UNIX, SOCK_DGRAM, 0);
    check("bind", bind(s, (struct sockaddr *)&a, al), 0);
    char c = 'g';
    struct iovec iov = {&c, 1};
    union {
        char b[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } u;
    struct msghdr m = {.msg_name = &a, .msg_namelen = al, .msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    struct cmsghdr *c1 = CMSG_FIRSTHDR(&m);
    c1->cmsg_level = SOL_SOCKET;
    c1->cmsg_type = SCM_RIGHTS;
    c1->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c1), &s, sizeof s);
    check("send itself to itself", sendmsg(s, &m, 0), 1);
    close(s);
    // aios は最後の fd を閉じたシステムコールの終わりに片づける。Linux はほかのソケットが閉じたときに (うしろで)
    // 片づけるので、それまでは EADDRINUSE
    int t = socket(AF_UNIX, SOCK_DGRAM, 0);
    errno = 0;
    int ok = bind(t, (struct sockaddr *)&a, al);
    if (ok < 0) {
        check("until gc the name is held by the socket in flight", errno, EADDRINUSE);
        close(socket(AF_UNIX, SOCK_STREAM, 0));
        for (int i = 0; i < 50 && ok < 0; i++) {
            msleep(20);
            ok = bind(t, (struct sockaddr *)&a, al);
        }
    }
    check("after gc the name is free", ok, 0);
    close(t);
}

int main(void) {
    wake_read();
    wake_accept();
    wake_poll();
    rcvtimeo();
    big_stream();
    pass_fd();
    after_close();
    seqpacket();
    dgram_full();
    gc();
    printf("unixsock: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
