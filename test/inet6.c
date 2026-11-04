// AF_INET6 (段階 1: ::1 のループバック、::、v4 の番地を写したもの (::ffff:a.b.c.d)、IPV6_V6ONLY): ソケット、bind の
// たしかめ、::1 の TCP と UDP、名前 (sockaddr_in6)、二つで一つ (:: で待つものに IPv4 がつながる)、v4 と v6 の
// ポートのぶつかり、connect のしくじり、SOL_IPV6 の sockopt、IPV6_RECVPKTINFO。
// 出すだけのテスト: 手本の Linux (ネットワークなし: lo だけ) とくらべる (test/ldiff.sh inet6)。ふつうの人で。ポートは
// その場でとる (番号はじかに出さない)。aios の VM には eth0 の IPv6 があるので、lo のものだけを出す (eth0 の番地と道、
// 外へのあて先は test/inet6net.c で)
//   aios の中で: ./inet6      aarch64-linux-gnu-gcc -O2 -static -o inet6 test/inet6.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static const char *en(int e) {
    static char b[32];
    if (!e)
        return "0";
    const char *n = strerrorname_np(e);
    if (n)
        return n;
    snprintf(b, sizeof b, "errno%d", e);
    return b;
}

#define R(what, call)                                                                                                    \
    do {                                                                                                                 \
        errno = 0;                                                                                                       \
        long _r = (call);                                                                                                \
        printf("%s: %s\n", what, _r < 0 ? en(errno) : "ok");                                                             \
    } while (0)

static struct sockaddr_in6 a6(const char *s, int port) {
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
    inet_pton(AF_INET6, s, &a.sin6_addr);
    return a;
}

static struct sockaddr_in a4(const char *s, int port) {
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, s, &a.sin_addr);
    return a;
}

// 名前を 1 つの文字列に (ポートは want と同じか、0 か、ほか)
static const char *show(const void *sa, socklen_t len, int want) {
    static char b[4][160];
    static int i;
    i = (i + 1) % 4;
    const struct sockaddr *s = sa;
    char ip[INET6_ADDRSTRLEN];
    if (s->sa_family == AF_INET6) {
        const struct sockaddr_in6 *x = sa;
        inet_ntop(AF_INET6, &x->sin6_addr, ip, sizeof ip);
        int p = ntohs(x->sin6_port);
        snprintf(b[i], sizeof b[i], "in6 len=%u [%s]:%s flow=%u scope=%u", len, ip, p == 0 ? "0" : p == want ? "P" : "other",
                 x->sin6_flowinfo, x->sin6_scope_id);
    } else if (s->sa_family == AF_INET) {
        const struct sockaddr_in *x = sa;
        inet_ntop(AF_INET, &x->sin_addr, ip, sizeof ip);
        int p = ntohs(x->sin_port);
        snprintf(b[i], sizeof b[i], "in len=%u %s:%s", len, ip, p == 0 ? "0" : p == want ? "P" : "other");
    } else {
        snprintf(b[i], sizeof b[i], "family %d len=%u", s->sa_family, len);
    }
    return b[i];
}

static int port_of(int s) {
    struct sockaddr_storage a;
    socklen_t l = sizeof a;
    getsockname(s, (struct sockaddr *)&a, &l);
    return a.ss_family == AF_INET6 ? ntohs(((struct sockaddr_in6 *)&a)->sin6_port) : ntohs(((struct sockaddr_in *)&a)->sin_port);
}

static void names(const char *what, int s, int want) {
    struct sockaddr_storage a;
    socklen_t l = sizeof a;
    memset(&a, 0xee, sizeof a);
    int r = getsockname(s, (struct sockaddr *)&a, &l);
    printf("%s sockname: %s\n", what, r ? en(errno) : show(&a, l, want));
    l = sizeof a;
    memset(&a, 0xee, sizeof a);
    r = getpeername(s, (struct sockaddr *)&a, &l);
    printf("%s peername: %s\n", what, r ? en(errno) : show(&a, l, want));
}

static int iopt(int s, int level, int opt) {
    int v = -999;
    socklen_t l = sizeof v;
    if (getsockopt(s, level, opt, &v, &l))
        return -1000 - errno;
    return v;
}

static int tcp6(void) {
    return socket(AF_INET6, SOCK_STREAM | SOCK_CLOEXEC, 0);
}

static int udp6(void) {
    return socket(AF_INET6, SOCK_DGRAM | SOCK_CLOEXEC, 0);
}

// listen するもの (AF_INET6 で addr、v6only を決めてから)。ポートを返す
static int listener6(const char *addr, int v6only, int *port) {
    int l = tcp6();
    if (v6only >= 0)
        setsockopt(l, IPPROTO_IPV6, IPV6_V6ONLY, &v6only, sizeof v6only);
    struct sockaddr_in6 a = a6(addr, 0);
    if (bind(l, (struct sockaddr *)&a, sizeof a) || listen(l, 8)) {
        printf("listener %s: %s\n", addr, en(errno));
        return -1;
    }
    *port = port_of(l);
    return l;
}

static void sockets(void) {
    printf("-- sockets\n");
    int s = tcp6();
    printf("tcp6: %s domain=%d protocol=%d type=%d\n", s >= 0 ? "ok" : en(errno), iopt(s, SOL_SOCKET, SO_DOMAIN),
           iopt(s, SOL_SOCKET, SO_PROTOCOL), iopt(s, SOL_SOCKET, SO_TYPE));
    printf("tcp6 v6only=%d unicast_hops=%d mc_hops=%d mc_loop=%d tclass=%d recvpktinfo=%d recvtclass=%d recvhoplimit=%d "
           "mtu_discover=%d maxseg=%d ip_tos=%d ip_ttl=%d\n",
           iopt(s, IPPROTO_IPV6, IPV6_V6ONLY), iopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS), iopt(s, IPPROTO_IPV6, IPV6_MULTICAST_HOPS),
           iopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP), iopt(s, IPPROTO_IPV6, IPV6_TCLASS), iopt(s, IPPROTO_IPV6, IPV6_RECVPKTINFO),
           iopt(s, IPPROTO_IPV6, IPV6_RECVTCLASS), iopt(s, IPPROTO_IPV6, IPV6_RECVHOPLIMIT), iopt(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER),
           iopt(s, IPPROTO_TCP, TCP_MAXSEG), iopt(s, IPPROTO_IP, IP_TOS), iopt(s, IPPROTO_IP, IP_TTL));
    int v = 1;
    R("set v6only 1", setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &v, sizeof v));
    printf("v6only now %d\n", iopt(s, IPPROTO_IPV6, IPV6_V6ONLY));
    v = 7;
    R("set unicast_hops 7", setsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &v, sizeof v));
    printf("unicast_hops now %d\n", iopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS));
    v = -1;
    R("set unicast_hops -1", setsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &v, sizeof v));
    printf("unicast_hops now %d\n", iopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS));
    v = 256;
    R("set unicast_hops 256", setsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &v, sizeof v));
    v = 0x28;
    R("set tclass 0x28", setsockopt(s, IPPROTO_IPV6, IPV6_TCLASS, &v, sizeof v));
    printf("tclass now %d\n", iopt(s, IPPROTO_IPV6, IPV6_TCLASS));
    v = 256;
    R("set tclass 256", setsockopt(s, IPPROTO_IPV6, IPV6_TCLASS, &v, sizeof v));
    R("get unknown ipv6 opt 200", iopt(s, IPPROTO_IPV6, 200) < -1000 ? (errno = -(iopt(s, IPPROTO_IPV6, 200) + 1000), -1) : 0);
    close(s);
    s = udp6();
    printf("udp6: %s domain=%d protocol=%d v6only=%d mc_loop=%d\n", s >= 0 ? "ok" : en(errno), iopt(s, SOL_SOCKET, SO_DOMAIN),
           iopt(s, SOL_SOCKET, SO_PROTOCOL), iopt(s, IPPROTO_IPV6, IPV6_V6ONLY), iopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP));
    close(s);
    int sv[2];
    R("socketpair AF_INET6 stream", socketpair(AF_INET6, SOCK_STREAM, 0, sv));
    R("socket AF_INET6 seqpacket", socket(AF_INET6, SOCK_SEQPACKET, 0));
    R("socket AF_INET6 stream udp", socket(AF_INET6, SOCK_STREAM, IPPROTO_UDP));
    R("ipv6 opt on AF_INET socket", (s = socket(AF_INET, SOCK_STREAM, 0), iopt(s, IPPROTO_IPV6, IPV6_V6ONLY) < -1000 ? (errno = -(iopt(s, IPPROTO_IPV6, IPV6_V6ONLY) + 1000), -1) : 0));
    close(s);
}

static void binds(void) {
    printf("-- bind\n");
    int s = tcp6();
    struct sockaddr_in6 a = a6("::1", 0);
    R("bind len 23", bind(s, (struct sockaddr *)&a, 23));
    R("bind len 24", bind(s, (struct sockaddr *)&a, 24));
    names("bound ::1 len 24", s, 0);
    printf("v6only after bind ::1: %d\n", iopt(s, IPPROTO_IPV6, IPV6_V6ONLY));
    int v = 0;
    R("set v6only after bind", setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &v, sizeof v));
    R("bind again", bind(s, (struct sockaddr *)&a, sizeof a));
    close(s);
    s = tcp6();
    struct sockaddr_in b4 = a4("127.0.0.1", 0);
    R("bind sockaddr_in to AF_INET6 (len 16)", bind(s, (struct sockaddr *)&b4, sizeof b4));
    struct sockaddr_in6 x = a6("::1", 0);
    x.sin6_family = AF_INET;
    R("bind family AF_INET len 28", bind(s, (struct sockaddr *)&x, sizeof x));
    x = a6("2001:db8::1", 0);
    R("bind 2001:db8::1", bind(s, (struct sockaddr *)&x, sizeof x));
    x = a6("fe80::1", 0);
    R("bind fe80::1 without scope", bind(s, (struct sockaddr *)&x, sizeof x));
    x = a6("ff02::1", 0);
    R("bind ff02::1 (tcp)", bind(s, (struct sockaddr *)&x, sizeof x));
    x = a6("::ffff:10.9.9.9", 0);
    R("bind ::ffff:10.9.9.9", bind(s, (struct sockaddr *)&x, sizeof x));
    x = a6("::1", 80);
    R("bind ::1 port 80", bind(s, (struct sockaddr *)&x, sizeof x));
    v = 1;
    setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &v, sizeof v);
    x = a6("::ffff:127.0.0.1", 0);
    R("bind ::ffff:127.0.0.1 with v6only", bind(s, (struct sockaddr *)&x, sizeof x));
    close(s);
    s = tcp6();
    R("bind ::ffff:127.0.0.1", bind(s, (struct sockaddr *)&x, sizeof x));
    names("bound ::ffff:127.0.0.1", s, 0);
    printf("v6only after bind mapped: %d\n", iopt(s, IPPROTO_IPV6, IPV6_V6ONLY));
    close(s);
    s = tcp6();
    x = a6("::", 0);
    R("bind ::", bind(s, (struct sockaddr *)&x, sizeof x));
    names("bound ::", s, 0);
    printf("v6only after bind ::: %d\n", iopt(s, IPPROTO_IPV6, IPV6_V6ONLY));
    close(s);
    s = udp6();
    x = a6("ff02::1", 0);
    R("bind ff02::1 (udp)", bind(s, (struct sockaddr *)&x, sizeof x));
    close(s);
}

// v4 と v6 のポートのぶつかり: first を bind してから second を同じポートに
static void clash(const char *what, int fam1, const char *ad1, int v6only1, int fam2, const char *ad2, int v6only2, int reuse) {
    int s1 = socket(fam1, SOCK_STREAM, 0), s2 = socket(fam2, SOCK_STREAM, 0);
    if (reuse) {
        int one = 1;
        setsockopt(s1, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
        setsockopt(s2, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    }
    if (fam1 == AF_INET6)
        setsockopt(s1, IPPROTO_IPV6, IPV6_V6ONLY, &v6only1, sizeof v6only1);
    if (fam2 == AF_INET6)
        setsockopt(s2, IPPROTO_IPV6, IPV6_V6ONLY, &v6only2, sizeof v6only2);
    struct sockaddr_storage x1, x2;
    socklen_t l1, l2;
    if (fam1 == AF_INET6) {
        *(struct sockaddr_in6 *)&x1 = a6(ad1, 0);
        l1 = sizeof(struct sockaddr_in6);
    } else {
        *(struct sockaddr_in *)&x1 = a4(ad1, 0);
        l1 = sizeof(struct sockaddr_in);
    }
    if (bind(s1, (struct sockaddr *)&x1, l1)) {
        printf("%s: first %s\n", what, en(errno));
        goto out;
    }
    int p = port_of(s1);
    if (fam2 == AF_INET6) {
        *(struct sockaddr_in6 *)&x2 = a6(ad2, p);
        l2 = sizeof(struct sockaddr_in6);
    } else {
        *(struct sockaddr_in *)&x2 = a4(ad2, p);
        l2 = sizeof(struct sockaddr_in);
    }
    R(what, bind(s2, (struct sockaddr *)&x2, l2));
out:
    close(s1);
    close(s2);
}

static void clashes(void) {
    printf("-- port clashes\n");
    clash("v4 0.0.0.0 then v6 :: (dual)", AF_INET, "0.0.0.0", 0, AF_INET6, "::", 0, 0);
    clash("v4 0.0.0.0 then v6 :: v6only", AF_INET, "0.0.0.0", 0, AF_INET6, "::", 1, 0);
    clash("v6 :: (dual) then v4 0.0.0.0", AF_INET6, "::", 0, AF_INET, "0.0.0.0", 0, 0);
    clash("v6 :: v6only then v4 0.0.0.0", AF_INET6, "::", 1, AF_INET, "0.0.0.0", 0, 0);
    clash("v6 :: (dual) then v4 127.0.0.1", AF_INET6, "::", 0, AF_INET, "127.0.0.1", 0, 0);
    clash("v4 127.0.0.1 then v6 ::1", AF_INET, "127.0.0.1", 0, AF_INET6, "::1", 0, 0);
    clash("v6 ::1 then v6 ::", AF_INET6, "::1", 0, AF_INET6, "::", 0, 0);
    clash("v6 :: then v6 ::1", AF_INET6, "::", 0, AF_INET6, "::1", 0, 0);
    clash("v6 :: then v6 :: (both v6only)", AF_INET6, "::", 1, AF_INET6, "::", 1, 0);
    clash("v6 ::ffff:127.0.0.1 then v4 127.0.0.1", AF_INET6, "::ffff:127.0.0.1", 0, AF_INET, "127.0.0.1", 0, 0);
    clash("v6 ::ffff:127.0.0.1 then v4 127.0.0.2", AF_INET6, "::ffff:127.0.0.1", 0, AF_INET, "127.0.0.2", 0, 0);
    clash("v6 ::ffff:127.0.0.1 then v6 ::1", AF_INET6, "::ffff:127.0.0.1", 0, AF_INET6, "::1", 0, 0);
    clash("v6 ::1 then v6 ::1 reuseaddr", AF_INET6, "::1", 0, AF_INET6, "::1", 0, 1);
    clash("v6 :: then v4 0.0.0.0 reuseaddr", AF_INET6, "::", 0, AF_INET, "0.0.0.0", 0, 1);
}

static void tcp_loop(void) {
    printf("-- tcp ::1\n");
    int p, l = listener6("::1", -1, &p);
    names("listener ::1", l, p);
    printf("listener maxseg=%d\n", iopt(l, IPPROTO_TCP, TCP_MAXSEG));
    int c = tcp6();
    struct sockaddr_in6 a = a6("::1", p);
    R("connect ::1", connect(c, (struct sockaddr *)&a, sizeof a));
    struct sockaddr_storage pa;
    socklen_t pl = sizeof pa;
    int s = accept(l, (struct sockaddr *)&pa, &pl);
    printf("accept: %s from %s\n", s >= 0 ? "ok" : en(errno), s >= 0 ? show(&pa, pl, p) : "-");
    names("client", c, p);
    names("server", s, p);
    printf("client maxseg=%d server maxseg=%d v6only client=%d server=%d\n", iopt(c, IPPROTO_TCP, TCP_MAXSEG),
           iopt(s, IPPROTO_TCP, TCP_MAXSEG), iopt(c, IPPROTO_IPV6, IPV6_V6ONLY), iopt(s, IPPROTO_IPV6, IPV6_V6ONLY));
    char buf[64] = {0};
    printf("write: %zd\n", write(c, "hello6", 6));
    printf("read: %zd %s\n", read(s, buf, sizeof buf), buf);
    shutdown(c, SHUT_WR);
    printf("read after shutdown: %zd\n", read(s, buf, sizeof buf));
    close(c);
    close(s);
    // :: にあてたものは ::1 へ
    c = tcp6();
    a = a6("::", p);
    R("connect ::", connect(c, (struct sockaddr *)&a, sizeof a));
    names("client to ::", c, p);
    s = accept(l, NULL, NULL);
    close(s);
    close(c);
    // 名前の長さが短い accept
    c = tcp6();
    a = a6("::1", p);
    connect(c, (struct sockaddr *)&a, sizeof a);
    pl = 16;
    memset(&pa, 0xee, sizeof pa);
    s = accept(l, (struct sockaddr *)&pa, &pl);
    printf("accept with 16 bytes: %s len=%u family=%d\n", s >= 0 ? "ok" : en(errno), pl, ((struct sockaddr *)&pa)->sa_family);
    close(s);
    close(c);
    close(l);
    // だれもいない
    c = tcp6();
    R("connect ::1 closed port", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
}

static void dual(void) {
    printf("-- dual stack\n");
    int p, l = listener6("::", 0, &p);
    names("listener :: dual", l, p);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in b = a4("127.0.0.1", p);
    R("v4 connect to dual", connect(c, (struct sockaddr *)&b, sizeof b));
    struct sockaddr_storage pa;
    socklen_t pl = sizeof pa;
    int s = accept(l, (struct sockaddr *)&pa, &pl);
    printf("accept: %s from %s\n", s >= 0 ? "ok" : en(errno), s >= 0 ? show(&pa, pl, p) : "-");
    names("v4 client", c, p);
    names("v6 server (v4 peer)", s, p);
    printf("server maxseg=%d\n", iopt(s, IPPROTO_TCP, TCP_MAXSEG));
    char buf[16] = {0};
    write(c, "four", 4);
    printf("read: %zd %s\n", read(s, buf, sizeof buf), buf);
    close(s);
    close(c);
    // AF_INET6 から ::ffff:127.0.0.1 へ
    c = tcp6();
    struct sockaddr_in6 a = a6("::ffff:127.0.0.1", p);
    R("v6 connect to ::ffff:127.0.0.1", connect(c, (struct sockaddr *)&a, sizeof a));
    names("mapped client", c, p);
    s = accept(l, (struct sockaddr *)&pa, &(socklen_t){sizeof pa});
    names("mapped server", s, p);
    close(s);
    close(c);
    // ::1 からも
    c = tcp6();
    a = a6("::1", p);
    R("v6 connect ::1 to dual", connect(c, (struct sockaddr *)&a, sizeof a));
    s = accept(l, NULL, NULL);
    names("v6 server (v6 peer)", s, p);
    close(s);
    close(c);
    close(l);
    // v6only のものは IPv4 を受けない
    l = listener6("::", 1, &p);
    c = socket(AF_INET, SOCK_STREAM, 0);
    b = a4("127.0.0.1", p);
    R("v4 connect to v6only", connect(c, (struct sockaddr *)&b, sizeof b));
    close(c);
    c = tcp6();
    a = a6("::1", p);
    R("v6 connect to v6only", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
    c = tcp6();
    int one = 1;
    setsockopt(c, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    a = a6("::ffff:127.0.0.1", p);
    R("v6only socket connect to mapped", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
    close(l);
    // ::ffff:127.0.0.1 で待つものは v4 のもの
    l = listener6("::ffff:127.0.0.1", -1, &p);
    names("listener mapped", l, p);
    c = socket(AF_INET, SOCK_STREAM, 0);
    b = a4("127.0.0.1", p);
    R("v4 connect to mapped listener", connect(c, (struct sockaddr *)&b, sizeof b));
    close(c);
    c = tcp6();
    a = a6("::1", p);
    R("v6 connect ::1 to mapped listener", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
    close(l);
}

static void connects(void) {
    printf("-- connect errors\n");
    int c = tcp6();
    struct sockaddr_in6 a = a6("::1", 9);
    R("connect len 23", connect(c, (struct sockaddr *)&a, 23));
    struct sockaddr_in b = a4("127.0.0.1", 9);
    R("connect sockaddr_in", connect(c, (struct sockaddr *)&b, sizeof b));
    a = a6("ff02::1", 9);
    R("connect ff02::1", connect(c, (struct sockaddr *)&a, sizeof a));
    a = a6("fe80::1", 9);
    R("connect fe80::1 without scope", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
    c = tcp6();
    a = a6("::ffff:127.0.0.1", 0);
    bind(c, (struct sockaddr *)&a, sizeof a);
    a = a6("::1", 9);
    R("mapped-bound socket connect ::1", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
    c = tcp6();
    a = a6("::1", 0);
    bind(c, (struct sockaddr *)&a, sizeof a);
    a = a6("::ffff:127.0.0.1", 9);
    R("::1-bound socket connect mapped", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
    c = socket(AF_INET, SOCK_STREAM, 0);
    a = a6("::1", 9);
    R("AF_INET socket connect sockaddr_in6", connect(c, (struct sockaddr *)&a, sizeof a));
    close(c);
}

static void udp(void) {
    printf("-- udp ::1\n");
    int r = udp6(), w = udp6();
    struct sockaddr_in6 a = a6("::1", 0);
    bind(r, (struct sockaddr *)&a, sizeof a);
    int p = port_of(r);
    int one = 1;
    R("set recvpktinfo", setsockopt(r, IPPROTO_IPV6, IPV6_RECVPKTINFO, &one, sizeof one));
    a = a6("::1", p);
    printf("sendto: %zd\n", sendto(w, "dgram6", 6, 0, (struct sockaddr *)&a, sizeof a));
    names("sender", w, p);
    char buf[64] = {0}, cb[256];
    struct sockaddr_storage from;
    struct iovec io = {buf, sizeof buf};
    struct msghdr m = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &io, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb};
    ssize_t n = recvmsg(r, &m, MSG_DONTWAIT);
    printf("recvmsg: %zd %s from %s\n", n, buf, n >= 0 ? show(&from, m.msg_namelen, port_of(w)) : en(errno));
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level == IPPROTO_IPV6 && c->cmsg_type == IPV6_PKTINFO) {
            struct in6_pktinfo *pi = (void *)CMSG_DATA(c);
            char ip[INET6_ADDRSTRLEN];
            inet_ntop(AF_INET6, &pi->ipi6_addr, ip, sizeof ip);
            printf("  cmsg IPV6_PKTINFO len=%zu addr=%s ifindex=%u\n", (size_t)c->cmsg_len, ip, pi->ipi6_ifindex);
        } else {
            printf("  cmsg level=%d type=%d len=%zu\n", c->cmsg_level, c->cmsg_type, (size_t)c->cmsg_len);
        }
    }
    R("connect w to ::1", connect(w, (struct sockaddr *)&a, sizeof a));
    names("connected sender", w, p);
    printf("send: %zd\n", send(w, "again", 5, 0));
    memset(buf, 0, sizeof buf);
    socklen_t fl = sizeof from;
    printf("recvfrom: %zd %s\n", recvfrom(r, buf, sizeof buf, MSG_DONTWAIT, (struct sockaddr *)&from, &fl), buf);
    close(r);
    printf("send to closed: %zd\n", send(w, "x", 1, 0));
    errno = 0;
    printf("send again: %zd %s\n", send(w, "x", 1, 0), en(errno));
    close(w);

    printf("-- udp dual\n");
    r = udp6();
    a = a6("::", 0);
    bind(r, (struct sockaddr *)&a, sizeof a);
    p = port_of(r);
    w = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in b = a4("127.0.0.1", p);
    printf("v4 sendto dual: %zd\n", sendto(w, "four", 4, 0, (struct sockaddr *)&b, sizeof b));
    fl = sizeof from;
    memset(buf, 0, sizeof buf);
    n = recvfrom(r, buf, sizeof buf, MSG_DONTWAIT, (struct sockaddr *)&from, &fl);
    printf("recvfrom: %zd %s from %s\n", n, buf, n >= 0 ? show(&from, fl, port_of(w)) : en(errno));
    // v6 から ::ffff:127.0.0.1 で AF_INET のものへ
    int r4 = socket(AF_INET, SOCK_DGRAM, 0);
    b = a4("127.0.0.1", 0);
    bind(r4, (struct sockaddr *)&b, sizeof b);
    int p4 = port_of(r4);
    int w6 = udp6();
    a = a6("::ffff:127.0.0.1", p4);
    printf("v6 sendto mapped: %zd\n", sendto(w6, "map", 3, 0, (struct sockaddr *)&a, sizeof a));
    struct sockaddr_in f4;
    fl = sizeof f4;
    memset(buf, 0, sizeof buf);
    n = recvfrom(r4, buf, sizeof buf, MSG_DONTWAIT, (struct sockaddr *)&f4, &fl);
    printf("v4 recvfrom: %zd %s from %s\n", n, buf, n >= 0 ? show(&f4, fl, port_of(w6)) : en(errno));
    names("v6 sender after mapped send", w6, p4);
    a = a6("::1", p);
    printf("same socket sendto ::1: %zd %s\n", sendto(w6, "x", 1, 0, (struct sockaddr *)&a, sizeof a), en(errno));
    close(r4);
    close(w6);
    close(w);
    close(r);
    // v6only の UDP は IPv4 を受けない
    r = udp6();
    setsockopt(r, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    a = a6("::", 0);
    bind(r, (struct sockaddr *)&a, sizeof a);
    p = port_of(r);
    w = socket(AF_INET, SOCK_DGRAM, 0);
    b = a4("127.0.0.1", p);
    connect(w, (struct sockaddr *)&b, sizeof b);
    send(w, "x", 1, 0);
    errno = 0;
    printf("v4 to v6only: recv %zd %s\n", recv(r, buf, sizeof buf, MSG_DONTWAIT), en(errno));
    errno = 0;
    printf("v4 sender next send: %zd %s\n", send(w, "x", 1, 0), en(errno));
    close(w);
    close(r);
}

// netlink の RTM_GETADDR dump: lo の IPv6 のもの (と、ほかの番地は数だけ) をデータグラムごとに
static void addr_dump(const char *what, int family) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct {
        struct nlmsghdr h;
        struct rtgenmsg g;
    } req = {{NLMSG_LENGTH(sizeof(struct rtgenmsg)), RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 77, 0}, {family}};
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &req, req.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
    printf("%s\n", what);
    static char buf[65536];
    for (int dg = 0; dg < 16; dg++) {
        long got = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (got < 0)
            break;
        int v4 = 0, end = 0;
        printf(" datagram:\n");
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            if (h->nlmsg_type == NLMSG_DONE) {
                printf("  DONE\n");
                end = 1;
                continue;
            }
            if (h->nlmsg_type != RTM_NEWADDR)
                continue;
            struct ifaddrmsg *a = NLMSG_DATA(h);
            if (a->ifa_family != AF_INET6) {
                v4++;
                continue;
            }
            if (a->ifa_index != 1)
                continue;
            char line[512];
            int n = snprintf(line, sizeof line, "  inet6 lo flags=%#x prefixlen=%u ifa_flags=%#x scope=%u:", h->nlmsg_flags, a->ifa_prefixlen,
                             a->ifa_flags, a->ifa_scope);
            int len = IFA_PAYLOAD(h);
            for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
                unsigned char *d = RTA_DATA(r);
                char ip[INET6_ADDRSTRLEN];
                switch (r->rta_type) {
                case IFA_ADDRESS: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, sizeof line - n, " address %s", ip); break;
                case IFA_CACHEINFO: {
                    struct ifa_cacheinfo *c = (void *)d;
                    n += snprintf(line + n, sizeof line - n, " cacheinfo(prefered %#x valid %#x)", c->ifa_prefered, c->ifa_valid);
                    break;
                }
                case IFA_FLAGS: n += snprintf(line + n, sizeof line - n, " flags %#x", *(unsigned *)d); break;
                case 11: n += snprintf(line + n, sizeof line - n, " proto %u", d[0]); break;
                default: n += snprintf(line + n, sizeof line - n, " attr%u(%u)", r->rta_type, (unsigned)RTA_PAYLOAD(r)); break;
                }
            }
            printf("%s\n", line);
        }
        if (v4)
            printf("  (+%s IPv4)\n", v4 ? "some" : "no");
        if (end)
            break;
    }
    close(s);
}

// RTM_NEWROUTE (AF_INET6) を 1 行に
static void show_route6(struct nlmsghdr *h) {
    struct rtmsg *r = NLMSG_DATA(h);
    char line[1024];
    int n = snprintf(line, sizeof line, "  route6 flags=%#x table=%u type=%u scope=%u proto=%u dst_len=%u src_len=%u tos=%u rtm_flags=%#x:",
                     h->nlmsg_flags, r->rtm_table, r->rtm_type, r->rtm_scope, r->rtm_protocol, r->rtm_dst_len, r->rtm_src_len, r->rtm_tos,
                     r->rtm_flags);
    int len = RTM_PAYLOAD(h);
    for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        unsigned char *d = RTA_DATA(a);
        char ip[INET6_ADDRSTRLEN];
        unsigned v = RTA_PAYLOAD(a) >= 4 ? *(unsigned *)d : 0;
        switch (a->rta_type) {
        case RTA_TABLE: n += snprintf(line + n, sizeof line - n, " table %u", v); break;
        case RTA_DST: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, sizeof line - n, " dst %s", ip); break;
        case RTA_PREFSRC: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, sizeof line - n, " prefsrc %s", ip); break;
        case RTA_GATEWAY: inet_ntop(AF_INET6, d, ip, sizeof ip); n += snprintf(line + n, sizeof line - n, " via %s", ip); break;
        case RTA_OIF: n += snprintf(line + n, sizeof line - n, " oif %u", v); break;
        case RTA_PRIORITY: n += snprintf(line + n, sizeof line - n, " metric %u", v); break;
        case RTA_PREF: n += snprintf(line + n, sizeof line - n, " pref %u", d[0]); break;
        case RTA_UID: n += snprintf(line + n, sizeof line - n, " uid %s", v == getuid() ? "self" : "other"); break;
        case RTA_CACHEINFO: {
            struct rta_cacheinfo *c = (void *)d;
            n += snprintf(line + n, sizeof line - n, " cache(len %u error %u expires %d)", (unsigned)RTA_PAYLOAD(a), c->rta_error, c->rta_expires);
            break;
        }
        default: n += snprintf(line + n, sizeof line - n, " attr%u(%u)", a->rta_type, (unsigned)RTA_PAYLOAD(a)); break;
        }
    }
    printf("%s\n", line);
}

// 道の口 (RTA_OIF)
static unsigned route_oif(struct nlmsghdr *h) {
    int len = RTM_PAYLOAD(h);
    for (struct rtattr *a = RTM_RTA((struct rtmsg *)NLMSG_DATA(h)); RTA_OK(a, len); a = RTA_NEXT(a, len))
        if (a->rta_type == RTA_OIF)
            return *(unsigned *)RTA_DATA(a);
    return 0;
}

// RTM_GETROUTE: dump (dst が NULL) か 1 つの道 (dump は lo のものだけ)
static void route6(const char *what, int strict, const char *dst, int dump) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (strict) {
        int one = 1;
        setsockopt(s, SOL_NETLINK, 12, &one, sizeof one);
    }
    struct {
        struct nlmsghdr h;
        struct rtmsg r;
        char attrs[64];
    } req;
    memset(&req, 0, sizeof req);
    req.h.nlmsg_len = NLMSG_LENGTH(sizeof(struct rtmsg));
    req.h.nlmsg_type = RTM_GETROUTE;
    req.h.nlmsg_flags = NLM_F_REQUEST | (dump ? NLM_F_DUMP : 0);
    req.h.nlmsg_seq = 88;
    req.r.rtm_family = AF_INET6;
    if (dst) {
        struct rtattr *a = (struct rtattr *)((char *)&req + NLMSG_ALIGN(req.h.nlmsg_len));
        a->rta_type = RTA_DST;
        a->rta_len = RTA_LENGTH(16);
        inet_pton(AF_INET6, dst, RTA_DATA(a));
        req.h.nlmsg_len = NLMSG_ALIGN(req.h.nlmsg_len) + RTA_ALIGN(a->rta_len);
        req.r.rtm_dst_len = 128;
    }
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &req, req.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
    printf("%s\n", what);
    static char buf[65536];
    for (int dg = 0; dg < 16; dg++) {
        long got = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (got < 0) {
            printf("  (no more)\n");
            break;
        }
        int end = 0;
        printf(" datagram:\n");
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, got); h = NLMSG_NEXT(h, got)) {
            if (h->nlmsg_type == NLMSG_DONE) {
                printf("  DONE flags=%#x error=%d\n", h->nlmsg_flags, *(int *)NLMSG_DATA(h));
                end = 1;
            } else if (h->nlmsg_type == NLMSG_ERROR) {
                int e = ((struct nlmsgerr *)NLMSG_DATA(h))->error;
                printf("  ERROR %s\n", en(-e));
                end = 1;
            } else if (h->nlmsg_type == RTM_NEWROUTE && ((struct rtmsg *)NLMSG_DATA(h))->rtm_family == AF_INET6 && route_oif(h) == 1) {
                show_route6(h);
            }
        }
        if (end || !dump)
            break;
    }
    close(s);
}

static void routes6(void) {
    printf("-- ipv6 routes\n");
    route6("dump AF_INET6", 0, NULL, 1);
    route6("strict dump AF_INET6", 1, NULL, 1);
    route6("get ::1", 0, "::1", 0);
    route6("strict get ::1", 1, "::1", 0);
}

static void addrs(void) {
    printf("-- addresses\n");
    addr_dump("RTM_GETADDR AF_INET6", AF_INET6);
    addr_dump("RTM_GETADDR AF_UNSPEC", AF_UNSPEC);
    struct ifaddrs *ia;
    if (getifaddrs(&ia) == 0) {
        for (struct ifaddrs *p = ia; p; p = p->ifa_next)
            if (p->ifa_addr && p->ifa_addr->sa_family == AF_INET6 && strcmp(p->ifa_name, "lo") == 0) {
                char ip[INET6_ADDRSTRLEN], m[INET6_ADDRSTRLEN];
                inet_ntop(AF_INET6, &((struct sockaddr_in6 *)p->ifa_addr)->sin6_addr, ip, sizeof ip);
                inet_ntop(AF_INET6, &((struct sockaddr_in6 *)p->ifa_netmask)->sin6_addr, m, sizeof m);
                printf("getifaddrs: %s inet6 %s mask %s scope_id %u\n", p->ifa_name, ip, m, ((struct sockaddr_in6 *)p->ifa_addr)->sin6_scope_id);
            }
        freeifaddrs(ia);
    }
    char line[256];
    FILE *f = fopen("/proc/net/if_inet6", "r");
    while (f && fgets(line, sizeof line, f))
        if (strstr(line, " lo\n"))
            printf("if_inet6: %s", line);
    if (f)
        fclose(f);
    // /proc/net/tcp6 の listen しているもの (::1 と ::) と /proc/net/tcp に出ないこと
    int p1, l1 = listener6("::1", -1, &p1), p2, l2 = listener6("::", 0, &p2);
    const char *files[] = {"/proc/net/tcp6", "/proc/net/tcp"};
    for (int i = 0; i < 2; i++) {
        f = fopen(files[i], "r");
        int hdr = 1;
        char found[2][256] = {"", ""};
        while (f && fgets(line, sizeof line, f)) {
            if (hdr) {
                printf("%s header: %s", files[i], line);
                hdr = 0;
                continue;
            }
            char la[64], ra[64];
            unsigned lp, rp, st;
            if (sscanf(line, "%*d: %63[0-9A-F]:%x %63[0-9A-F]:%x %x", la, &lp, ra, &rp, &st) != 5)
                continue;
            // Linux の順はポートのハッシュの順なので、P1、P2 の順にならべる
            if (lp == (unsigned)p1 || lp == (unsigned)p2)
                snprintf(found[lp == (unsigned)p1 ? 0 : 1], sizeof found[0], "%s: local %s:%s remote %s:%u st %02X\n", files[i], la,
                         lp == (unsigned)p1 ? "P1" : "P2", ra, rp, st);
        }
        printf("%s%s", found[0], found[1]);
        if (f)
            fclose(f);
    }
    close(l1);
    close(l2);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    addrs();
    routes6();
    sockets();
    binds();
    clashes();
    tcp_loop();
    dual();
    connects();
    udp();
    return 0;
}
