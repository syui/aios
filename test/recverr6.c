// IPV6_RECVERR と IP_RECVERR のエラーの行列 (MSG_ERRQUEUE)。lo の閉じたポート (9) へ UDP を送ると、カーネルが
// ICMPv6 (IPv4 は ICMP) の port unreachable を返し、送ったソケットに知らせる (__udp6_lib_err → ipv6_icmp_error、
// __udp4_lib_err → ip_icmp_error):
//   - RECVERR なら、つないでいなくても行列に入れて sk_err も (次の recv や send、SO_ERROR がそれを返す)
//   - RECVERR でなければ、つないだものだけ sk_err に (行列には入れない)
//   - recvmsg(MSG_ERRQUEUE): 中身は送ったものの UDP の後ろ (ICMPv6 に入るのは 1280 バイトまで)、名前は送ったあて先、
//     付帯データは SOL_IPV6 / IPV6_RECVERR (IPv4 は SOL_IP / IP_RECVERR) の sock_extended_err と、知らせてきた
//     番地。IPV6_RECVPKTINFO もあれば、その ICMPv6 の IPV6_PKTINFO も先に。読んだら sk_err は次のものの errno に
//   - poll は行列があれば POLLERR
// 出すだけのテスト: test/ldiff.sh recverr6
//   aios の中で: ./recverr6      aarch64-linux-gnu-gcc -O2 -static -o recverr6 test/recverr6.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/errqueue.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define PORT 9

static const char *en(int e) {
    switch (e) {
    case 0: return "0";
    case EAGAIN: return "EAGAIN";
    case ECONNREFUSED: return "ECONNREFUSED";
    case EINVAL: return "EINVAL";
    default: {
        static char b[16];
        snprintf(b, sizeof b, "%d", e);
        return b;
    }
    }
}

static void addr_str(const struct sockaddr *sa, socklen_t len, char *out, size_t n) {
    char a[64] = "?";
    if (sa->sa_family == AF_INET6 && len >= sizeof(struct sockaddr_in6)) {
        const struct sockaddr_in6 *s = (const void *)sa;
        inet_ntop(AF_INET6, &s->sin6_addr, a, sizeof a);
        snprintf(out, n, "inet6 [%s]:%d flow=%u scope=%u", a, ntohs(s->sin6_port), ntohl(s->sin6_flowinfo), s->sin6_scope_id);
    } else if (sa->sa_family == AF_INET && len >= sizeof(struct sockaddr_in)) {
        const struct sockaddr_in *s = (const void *)sa;
        inet_ntop(AF_INET, &s->sin_addr, a, sizeof a);
        snprintf(out, n, "inet %s:%d", a, ntohs(s->sin_port));
    } else {
        snprintf(out, n, "family=%d len=%u", sa->sa_family, len);
    }
}

static int soerr(int fd) {
    int v = -1;
    socklen_t l = sizeof v;
    getsockopt(fd, SOL_SOCKET, SO_ERROR, &v, &l);
    return v;
}

static void pollerr(int fd, const char *what) {
    struct pollfd p = {fd, POLLIN | POLLPRI | POLLOUT, 0};
    int n = poll(&p, 1, 0);
    printf("  %s poll=%d revents=%s%s%s%s\n", what, n, p.revents & POLLIN ? "IN " : "", p.revents & POLLOUT ? "OUT " : "",
           p.revents & POLLERR ? "ERR " : "", p.revents & POLLPRI ? "PRI " : "");
}

// MSG_ERRQUEUE を 1 つ読んで出す
static void errq(int fd, size_t len, const char *what) {
    char data[4096], name[128], ctl[512];
    struct iovec iov = {data, len};
    struct msghdr m = {.msg_name = name, .msg_namelen = sizeof name, .msg_iov = &iov, .msg_iovlen = 1, .msg_control = ctl, .msg_controllen = sizeof ctl};
    memset(name, 0, sizeof name);
    ssize_t r = recvmsg(fd, &m, MSG_ERRQUEUE | MSG_DONTWAIT);
    if (r < 0) {
        printf("  %s errqueue -1 %s\n", what, en(errno));
        return;
    }
    char nb[128];
    addr_str((struct sockaddr *)name, m.msg_namelen, nb, sizeof nb);
    printf("  %s errqueue %zd flags=%s%s%s name=%s data=", what, r, m.msg_flags & MSG_ERRQUEUE ? "ERRQUEUE " : "",
           m.msg_flags & MSG_TRUNC ? "TRUNC " : "", m.msg_flags & MSG_CTRUNC ? "CTRUNC " : "", nb);
    if (r <= 32) {
        printf("'%.*s'\n", (int)r, data);
    } else {
        printf("%zd bytes, first '%c' last '%c'\n", r, data[0], data[r - 1]);
    }
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        size_t dl = c->cmsg_len - CMSG_LEN(0);
        printf("    cmsg level=%d type=%d len=%zu", c->cmsg_level, c->cmsg_type, dl);
        if ((c->cmsg_level == SOL_IPV6 && c->cmsg_type == IPV6_RECVERR) || (c->cmsg_level == SOL_IP && c->cmsg_type == IP_RECVERR)) {
            struct sock_extended_err *e = (void *)CMSG_DATA(c);
            char ob[128];
            addr_str(SO_EE_OFFENDER(e), dl - sizeof *e, ob, sizeof ob);
            printf(" errno=%s origin=%u type=%u code=%u pad=%u info=%u data=%u offender=%s", en(e->ee_errno), e->ee_origin, e->ee_type, e->ee_code,
                   e->ee_pad, e->ee_info, e->ee_data, ob);
        } else if (c->cmsg_level == SOL_IPV6 && c->cmsg_type == IPV6_PKTINFO) {
            struct in6_pktinfo *pi = (void *)CMSG_DATA(c);
            char a[64];
            inet_ntop(AF_INET6, &pi->ipi6_addr, a, sizeof a);
            printf(" pktinfo addr=%s ifindex=%u", a, pi->ipi6_ifindex);
        } else if (c->cmsg_level == SOL_IP && c->cmsg_type == IP_PKTINFO) {
            struct in_pktinfo *pi = (void *)CMSG_DATA(c);
            char a[32], b[32];
            inet_ntop(AF_INET, &pi->ipi_spec_dst, a, sizeof a);
            inet_ntop(AF_INET, &pi->ipi_addr, b, sizeof b);
            printf(" pktinfo spec_dst=%s addr=%s ifindex=%d", a, b, pi->ipi_ifindex);
        }
        printf("\n");
    }
}

static int sock(int fam, int recverr) {
    int fd = socket(fam, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    if (recverr) {
        int one = 1;
        if (fam == AF_INET6) {
            setsockopt(fd, SOL_IPV6, IPV6_RECVERR, &one, sizeof one);
        } else {
            setsockopt(fd, SOL_IP, IP_RECVERR, &one, sizeof one);
        }
    }
    return fd;
}

static socklen_t dest(int fam, struct sockaddr_storage *ss) {
    memset(ss, 0, sizeof *ss);
    if (fam == AF_INET6) {
        struct sockaddr_in6 *s = (void *)ss;
        s->sin6_family = AF_INET6;
        s->sin6_addr = in6addr_loopback;
        s->sin6_port = htons(PORT);
        return sizeof *s;
    }
    struct sockaddr_in *s = (void *)ss;
    s->sin_family = AF_INET;
    s->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    s->sin_port = htons(PORT);
    return sizeof *s;
}

static void send_to(int fd, int fam, const void *p, size_t n, const char *what) {
    struct sockaddr_storage ss;
    socklen_t l = dest(fam, &ss);
    ssize_t r = sendto(fd, p, n, 0, (struct sockaddr *)&ss, l);
    printf("  %s sendto %zd %s\n", what, r, r < 0 ? en(errno) : "");
}

static void run(int fam) {
    const char *f = fam == AF_INET6 ? "v6" : "v4";
    struct sockaddr_storage ss;
    socklen_t l;
    char buf[64];

    printf("%s unconnected, RECVERR\n", f);
    int fd = sock(fam, 1);
    send_to(fd, fam, "hello", 5, "a");
    pollerr(fd, "a");
    errq(fd, sizeof buf, "a");
    printf("  a SO_ERROR=%s\n", en(soerr(fd)));
    pollerr(fd, "a after");
    errq(fd, sizeof buf, "a again");
    close(fd);

    printf("%s unconnected, RECVERR, recv first\n", f);
    fd = sock(fam, 1);
    send_to(fd, fam, "first", 5, "b");
    ssize_t r = recv(fd, buf, sizeof buf, 0);
    printf("  b recv %zd %s\n", r, r < 0 ? en(errno) : "");
    r = recv(fd, buf, sizeof buf, 0);
    printf("  b recv again %zd %s\n", r, r < 0 ? en(errno) : "");
    errq(fd, sizeof buf, "b");
    close(fd);

    printf("%s unconnected, no RECVERR\n", f);
    fd = sock(fam, 0);
    send_to(fd, fam, "quiet", 5, "c");
    pollerr(fd, "c");
    printf("  c SO_ERROR=%s\n", en(soerr(fd)));
    errq(fd, sizeof buf, "c");
    close(fd);

    printf("%s connected, no RECVERR\n", f);
    fd = sock(fam, 0);
    l = dest(fam, &ss);
    printf("  d connect %d\n", connect(fd, (struct sockaddr *)&ss, l));
    r = send(fd, "one", 3, 0);
    printf("  d send %zd %s\n", r, r < 0 ? en(errno) : "");
    r = send(fd, "two", 3, 0);
    printf("  d send again %zd %s\n", r, r < 0 ? en(errno) : "");
    errq(fd, sizeof buf, "d");
    close(fd);

    printf("%s connected, RECVERR\n", f);
    fd = sock(fam, 1);
    printf("  e connect %d\n", connect(fd, (struct sockaddr *)&ss, l));
    r = send(fd, "one", 3, 0);
    printf("  e send %zd %s\n", r, r < 0 ? en(errno) : "");
    r = send(fd, "two", 3, 0);
    printf("  e send again %zd %s\n", r, r < 0 ? en(errno) : "");
    pollerr(fd, "e");
    errq(fd, sizeof buf, "e 1");
    pollerr(fd, "e");
    errq(fd, sizeof buf, "e 2");
    printf("  e SO_ERROR=%s\n", en(soerr(fd)));
    close(fd);

    printf("%s two errors, then SO_ERROR\n", f);
    fd = sock(fam, 1);
    send_to(fd, fam, "one", 3, "f");
    send_to(fd, fam, "two", 3, "f");
    printf("  f SO_ERROR=%s\n", en(soerr(fd)));
    pollerr(fd, "f");
    errq(fd, sizeof buf, "f 1");
    printf("  f SO_ERROR=%s\n", en(soerr(fd)));
    errq(fd, sizeof buf, "f 2");
    printf("  f SO_ERROR=%s\n", en(soerr(fd)));
    close(fd);

    printf("%s short buffer\n", f);
    fd = sock(fam, 1);
    send_to(fd, fam, "truncate-me", 11, "g");
    errq(fd, 4, "g");
    close(fd);

    printf("%s 2000 bytes\n", f);
    fd = sock(fam, 1);
    char big[2000];
    for (int i = 0; i < 2000; i++) {
        big[i] = 'a' + i % 26;
    }
    send_to(fd, fam, big, sizeof big, "h");
    char huge[4096];
    (void)huge;
    errq(fd, 4096, "h");
    close(fd);

    printf("%s RECVERR with PKTINFO\n", f);
    fd = sock(fam, 1);
    int one = 1;
    if (fam == AF_INET6) {
        setsockopt(fd, SOL_IPV6, IPV6_RECVPKTINFO, &one, sizeof one);
    } else {
        setsockopt(fd, SOL_IP, IP_PKTINFO, &one, sizeof one);
    }
    send_to(fd, fam, "info", 4, "i");
    errq(fd, sizeof buf, "i");
    close(fd);

    printf("%s RECVERR turned off after the error\n", f);
    fd = sock(fam, 1);
    send_to(fd, fam, "late", 4, "j");
    int zero = 0;
    if (fam == AF_INET6) {
        setsockopt(fd, SOL_IPV6, IPV6_RECVERR, &zero, sizeof zero);
    } else {
        setsockopt(fd, SOL_IP, IP_RECVERR, &zero, sizeof zero);
    }
    errq(fd, sizeof buf, "j");
    close(fd);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    run(AF_INET6);
    run(AF_INET);
    return 0;
}
