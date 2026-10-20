// AF_INET のソケット (kernel/src/inet.rs) を、ループバックで、プロセスをまたいで: 待って起こされる accept / read /
// poll / epoll、SO_RCVTIMEO、MSG_WAITALL の大きな stream (4 MiB)、listen の行列がいっぱいのとき (Linux と同じく SYN を
// 1 秒後にやりなおす)、ECONNREFUSED、SO_LINGER 0 の RST、shutdown(RD) のあとに来たデータの RST、SIGPIPE、TIME_WAIT と
// ポート、/proc/net/tcp、SCM_RIGHTS で渡した TCP、UDP の待つ recvfrom。ロボット (test/sysdiff.c) は 1 つのプロセスで
// 待たないものだけなので、こちらで。
// aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o inetsock test/inetsock.c; ./inetsock。
// Linux でも同じ答え (test/linux-vm.sh exec latest ...)。しくじった数を終了コードで
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
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

static struct sockaddr_in lo(int port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    a.sin_port = htons((unsigned short)port);
    return a;
}

static int port_of(int s) {
    struct sockaddr_in a;
    socklen_t l = sizeof a;
    if (getsockname(s, (struct sockaddr *)&a, &l) < 0)
        return -1;
    return ntohs(a.sin_port);
}

// 127.0.0.1 の空いたポートで listen (reuse なら SO_REUSEADDR)。ポートは *port に
static int listener(int backlog, int reuse, int *port) {
    int l = socket(AF_INET, SOCK_STREAM, 0), one = 1;
    struct sockaddr_in a = lo(*port);
    if (reuse)
        setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, backlog) < 0) {
        close(l);
        return -1;
    }
    *port = port_of(l);
    return l;
}

static int dial(int port, int flags) {
    int c = socket(AF_INET, SOCK_STREAM | flags, 0);
    struct sockaddr_in a = lo(port);
    if (connect(c, (struct sockaddr *)&a, sizeof a) < 0 && errno != EINPROGRESS) {
        int e = errno;
        close(c);
        errno = e;
        return -1;
    }
    return c;
}

// つながった TCP の 2 つ (sv[0] がつないだほう、sv[1] が accept したほう)
static int tcppair(int sv[2]) {
    int port = 0, l = listener(1, 0, &port);
    if (l < 0)
        return -1;
    sv[0] = dial(port, 0);
    sv[1] = accept(l, NULL, NULL);
    close(l);
    return sv[0] >= 0 && sv[1] >= 0 ? 0 : -1;
}

// accept が待ち、つないできた子の名前。子の read は親の write を待つ
static void wake_accept(void) {
    int port = 0, l = listener(4, 0, &port);
    check("listen", l >= 0, 1);
    pid_t pid = fork();
    if (pid == 0) {
        msleep(100);
        int c = dial(port, 0);
        if (c < 0 || write(c, "hi", 2) != 2)
            _exit(1);
        char b;
        _exit(read(c, &b, 1) == 1 && b == 'k' ? 0 : 2);
    }
    struct sockaddr_in p, q;
    socklen_t pl = sizeof p, ql = sizeof q;
    long t = now_ms();
    int s = accept(l, (struct sockaddr *)&p, &pl);
    check("accept waited for the client", s >= 0 && now_ms() - t >= 50, 1);
    check("accept: the client's name is 127.0.0.1", pl == sizeof p && p.sin_family == AF_INET && p.sin_addr.s_addr == htonl(INADDR_LOOPBACK), 1);
    check("getpeername is the name from accept", getpeername(s, (struct sockaddr *)&q, &ql) == 0 && q.sin_port == p.sin_port, 1);
    check("the accepted socket has the listener's port", port_of(s), port);
    check("the client's port is ephemeral (32768-60999)", ntohs(p.sin_port) >= 32768 && ntohs(p.sin_port) <= 60999, 1);
    char b[4];
    check("read what the client wrote", read(s, b, sizeof b), 2);
    check("write to the client", write(s, "k", 1), 1);
    check("client", reap(pid), 0);
    close(s);
    close(l);
}

// 4 MiB を 4000 バイトずつ書いて閉じる子と、MSG_WAITALL で 1 MiB ずつそろえる親 (欠けず、順に)
static void big_stream(void) {
    int sv[2];
    check("tcp pair", tcppair(sv), 0);
    pid_t pid = fork();
    if (pid == 0) {
        close(sv[1]);
        static unsigned char b[4000];
        long n = 0, total = 4L << 20;
        while (n < total) {
            int k = (int)(total - n < (long)sizeof b ? total - n : (long)sizeof b);
            for (int i = 0; i < k; i++)
                b[i] = (unsigned char)((n + i) * 7 + ((n + i) >> 12));
            if (write(sv[0], b, (size_t)k) != k)
                _exit(1);
            n += k;
        }
        _exit(0);
    }
    close(sv[0]);
    static unsigned char r[1 << 20];
    int good = 1;
    for (long chunk = 0; chunk < 4; chunk++) {
        ssize_t got = recv(sv[1], r, sizeof r, MSG_WAITALL);
        if (got != (ssize_t)sizeof r) {
            printf("chunk %ld: %zd\n", chunk, got);
            good = 0;
            break;
        }
        for (long i = 0; i < (long)sizeof r && good; i++) {
            long at = chunk * (long)sizeof r + i;
            if (r[i] != (unsigned char)(at * 7 + (at >> 12)))
                good = 0;
        }
    }
    check("4 MiB in order (MSG_WAITALL 1 MiB x 4)", good, 1);
    check("then EOF", read(sv[1], r, 1), 0);
    check("writer", reap(pid), 0);
    close(sv[1]);
}

// poll と epoll (EPOLLET、EPOLLRDHUP) が、ほかのプロセスの write と shutdown で起きる
static void wake_poll(void) {
    int sv[2];
    check("tcp pair", tcppair(sv), 0);
    pid_t pid = fork();
    if (pid == 0) {
        msleep(100);
        if (write(sv[0], "x", 1) != 1)
            _exit(1);
        msleep(100);
        shutdown(sv[0], SHUT_WR);
        _exit(0);
    }
    struct pollfd p = {sv[1], POLLIN, 0};
    check("poll waits for POLLIN", poll(&p, 1, 5000), 1);
    check("poll revents", p.revents, POLLIN);
    char b[4];
    check("read", read(sv[1], b, sizeof b), 1);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLRDHUP | EPOLLET, .data.fd = sv[1]}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &ev);
    check("epoll waits for the FIN", epoll_wait(ep, &out, 1, 5000), 1);
    check("epoll event", out.events, EPOLLIN | EPOLLRDHUP);
    check("EPOLLET: nothing new", epoll_wait(ep, &out, 1, 50), 0);
    check("read EOF", read(sv[1], b, sizeof b), 0);
    check("child", reap(pid), 0);
    close(ep);
    close(sv[0]);
    close(sv[1]);
}

// SO_RCVTIMEO: 来なければ時間切れで EAGAIN (TCP と UDP)
static void rcvtimeo(void) {
    int sv[2];
    check("tcp pair", tcppair(sv), 0);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = lo(0);
    check("udp bind", bind(u, (struct sockaddr *)&a, sizeof a), 0);
    int fds[2] = {sv[1], u};
    for (int i = 0; i < 2; i++) {
        struct timeval tv = {0, 150000};
        check("SO_RCVTIMEO", setsockopt(fds[i], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv), 0);
        char b[4];
        long t = now_ms();
        errno = 0;
        check(i ? "udp recv times out" : "tcp recv times out", recv(fds[i], b, sizeof b, 0), -1);
        check("errno EAGAIN", errno, EAGAIN);
        long dt = now_ms() - t;
        check("after about 150 ms", dt >= 140 && dt < 2000, 1);
    }
    close(u);
    close(sv[0]);
    close(sv[1]);
}

// listen(l, 0) の行列には 1 つ入る。2 つめの SYN は落とされ (Linux に syncookies がなければ)、1 秒後のやりなおしで、
// accept で空いた行列に入る
static void backlog_full(void) {
    int port = 0, l = listener(0, 0, &port);
    int c1 = dial(port, SOCK_NONBLOCK);
    struct pollfd p = {c1, POLLOUT, 0};
    check("the first fits in the queue", poll(&p, 1, 1000), 1);
    long t = now_ms();
    int c2 = dial(port, SOCK_NONBLOCK);
    p.fd = c2;
    check("the second waits (SYN dropped)", poll(&p, 1, 300), 0);
    int s1 = accept(l, NULL, NULL);
    check("accept the first", s1 >= 0, 1);
    check("the second gets in by the SYN sent again", poll(&p, 1, 5000), 1);
    long dt = now_ms() - t;
    check("after about 1 s", dt >= 700 && dt < 3000, 1);
    int e = -1;
    socklen_t el = sizeof e;
    getsockopt(c2, SOL_SOCKET, SO_ERROR, &e, &el);
    check("SO_ERROR 0", e, 0);
    int s2 = accept(l, NULL, NULL);
    check("accept the second", s2 >= 0, 1);
    close(s1);
    close(s2);
    close(c1);
    close(c2);
    close(l);
}

// だれもいないポートへの connect は (待つものでも) ECONNREFUSED。SO_LINGER 0 で閉じれば RST で、相手は ECONNRESET
static void refused_and_reset(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(0);
    bind(s, (struct sockaddr *)&a, sizeof a);
    int port = port_of(s);
    close(s);
    errno = 0;
    check("connect to a closed port", dial(port, 0), -1);
    check("ECONNREFUSED", errno, ECONNREFUSED);

    int sv[2];
    check("tcp pair", tcppair(sv), 0);
    struct linger lg = {1, 0};
    check("SO_LINGER 0", setsockopt(sv[0], SOL_SOCKET, SO_LINGER, &lg, sizeof lg), 0);
    close(sv[0]);
    char b[4];
    errno = 0;
    check("read after RST", read(sv[1], b, sizeof b), -1);
    check("ECONNRESET", errno, ECONNRESET);
    check("then EOF", read(sv[1], b, sizeof b), 0);
    close(sv[1]);

    // shutdown(SHUT_RDWR) のあとにデータが来たら (RFC 1122)、受け手はこわれ (ECONNRESET)、送り手には RST (CLOSE_WAIT
    // なので EPIPE)
    check("tcp pair", tcppair(sv), 0);
    check("shutdown RDWR", shutdown(sv[1], SHUT_RDWR), 0);
    check("write after the peer's shutdown", write(sv[0], "x", 1), 1);
    errno = 0;
    check("the peer was reset", read(sv[1], b, sizeof b), -1);
    check("ECONNRESET", errno, ECONNRESET);
    int e = -1;
    socklen_t el = sizeof e;
    getsockopt(sv[0], SOL_SOCKET, SO_ERROR, &e, &el);
    check("the writer got the RST (EPIPE)", e, EPIPE);
    close(sv[0]);
    close(sv[1]);
}

static volatile sig_atomic_t pipes;

static void onpipe(int sig) {
    (void)sig;
    pipes++;
}

// 閉じた相手に書く: 1 回めは届いて RST が返り、2 回めは EPIPE と SIGPIPE
static void sigpipe(void) {
    int sv[2];
    check("tcp pair", tcppair(sv), 0);
    signal(SIGPIPE, onpipe);
    close(sv[1]);
    check("the first write after the peer closed", write(sv[0], "a", 1), 1);
    errno = 0;
    check("the second write", write(sv[0], "b", 1), -1);
    check("EPIPE", errno, EPIPE);
    check("one SIGPIPE", pipes, 1);
    check("MSG_NOSIGNAL: no SIGPIPE", send(sv[0], "c", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE && pipes == 1, 1);
    signal(SIGPIPE, SIG_DFL);
    close(sv[0]);
}

// 先に閉じたほう (ここでは accept したほう) は TIME_WAIT に残り、そのポートには SO_REUSEADDR がなければ bind できない。
// /proc/net/tcp にも出る (LISTEN は 0A、TIME_WAIT は 06)
static int in_proc(int port, const char *st) {
    FILE *f = fopen("/proc/net/tcp", "r");
    if (!f)
        return 0;
    char line[512], want[32];
    snprintf(want, sizeof want, "0100007F:%04X", port);
    int found = 0;
    while (fgets(line, sizeof line, f)) {
        char local[64], remote[64], state[8];
        if (sscanf(line, " %*s %63s %63s %7s", local, remote, state) == 3 && !strcmp(local, want) && !strcmp(state, st))
            found = 1;
    }
    fclose(f);
    return found;
}

static void time_wait(void) {
    int port = 0, l = listener(1, 1, &port);
    check("LISTEN in /proc/net/tcp", in_proc(port, "0A"), 1);
    int c = dial(port, 0), s = accept(l, NULL, NULL);
    check("pair", c >= 0 && s >= 0, 1);
    close(l);
    close(s);
    char b;
    check("the client reads EOF", read(c, &b, 1), 0);
    close(c);
    check("TIME_WAIT in /proc/net/tcp", in_proc(port, "06"), 1);
    int x = socket(AF_INET, SOCK_STREAM, 0), one = 1;
    struct sockaddr_in a = lo(port);
    errno = 0;
    check("bind over TIME_WAIT", bind(x, (struct sockaddr *)&a, sizeof a), -1);
    check("EADDRINUSE", errno, EADDRINUSE);
    setsockopt(x, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    check("bind over TIME_WAIT with SO_REUSEADDR", bind(x, (struct sockaddr *)&a, sizeof a), 0);
    check("listen again", listen(x, 1), 0);
    int c2 = dial(port, 0), s2 = accept(x, NULL, NULL);
    check("a new client on the same port", c2 >= 0 && s2 >= 0, 1);
    close(c2);
    close(s2);
    close(x);
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
    if (recvmsg(s, &m, 0) != 1)
        return -1;
    struct cmsghdr *c1 = CMSG_FIRSTHDR(&m);
    if (!c1 || c1->cmsg_type != SCM_RIGHTS)
        return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(c1), sizeof fd);
    return fd;
}

// TCP のつながりを AF_UNIX で子に渡し、子がそれで書く。accept4 の SOCK_NONBLOCK は受けた口に、listen の O_NONBLOCK は
// 受け継がない
static void pass_tcp(void) {
    int port = 0, l = listener(1, 0, &port);
    fcntl(l, F_SETFL, O_NONBLOCK);
    int c = dial(port, 0);
    int s = accept(l, NULL, NULL);
    check("accept does not inherit O_NONBLOCK", fcntl(s, F_GETFL) & O_NONBLOCK, 0);
    close(s);
    int c3 = dial(port, 0);
    int s3 = accept4(l, NULL, NULL, SOCK_NONBLOCK);
    check("accept4 SOCK_NONBLOCK", (fcntl(s3, F_GETFL) & O_NONBLOCK) != 0, 1);
    close(c3);
    close(s3);
    int u[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, u);
    pid_t pid = fork();
    if (pid == 0) {
        int t = recv_fd(u[1]);
        _exit(t >= 0 && write(t, "via child", 9) == 9 ? 0 : 1);
    }
    int s2 = dial(port, 0);
    int a2 = -1;
    for (int i = 0; i < 100 && a2 < 0; i++) {
        a2 = accept(l, NULL, NULL);
        if (a2 < 0)
            msleep(10);
    }
    check("send the TCP socket", send_fd(u[0], a2), 1);
    close(a2);
    char b[16];
    check("read what the child wrote through it", read(s2, b, sizeof b), 9);
    check("child", reap(pid), 0);
    close(s2);
    close(c);
    close(u[0]);
    close(u[1]);
    close(l);
}

// UDP: recvfrom が待ち、子が送ったものと名前。つないだ相手がいなければ ICMP で ECONNREFUSED
static void udp(void) {
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = lo(0);
    bind(u, (struct sockaddr *)&a, sizeof a);
    int port = port_of(u);
    pid_t pid = fork();
    if (pid == 0) {
        msleep(100);
        int v = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in to = lo(port);
        _exit(sendto(v, "ping", 4, 0, (struct sockaddr *)&to, sizeof to) == 4 ? 0 : 1);
    }
    char b[16];
    struct sockaddr_in from;
    socklen_t fl = sizeof from;
    long t = now_ms();
    check("recvfrom waits", recvfrom(u, b, sizeof b, 0, (struct sockaddr *)&from, &fl), 4);
    check("after the child sent", now_ms() - t >= 50, 1);
    check("from 127.0.0.1", fl == sizeof from && from.sin_addr.s_addr == htonl(INADDR_LOOPBACK) && ntohs(from.sin_port) >= 32768, 1);
    check("child", reap(pid), 0);
    close(u);

    int x = socket(AF_INET, SOCK_DGRAM, 0);
    bind(x, (struct sockaddr *)&a, sizeof a);
    int closed = port_of(x);
    close(x);
    int c = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in to = lo(closed);
    check("udp connect", connect(c, (struct sockaddr *)&to, sizeof to), 0);
    check("send to nobody", send(c, "x", 1, 0), 1);
    errno = 0;
    check("then recv", recv(c, b, sizeof b, 0), -1);
    check("ECONNREFUSED", errno, ECONNREFUSED);
    close(c);
}

int main(void) {
    wake_accept();
    big_stream();
    wake_poll();
    rcvtimeo();
    backlog_full();
    refused_and_reset();
    sigpipe();
    time_wait();
    pass_tcp();
    udp();
    printf("inetsock: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
