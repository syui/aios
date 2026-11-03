// Landlock の ABI 12 (Linux 7.4): kernel/src/landlock.rs。出すだけのテスト: 手本の Linux (test/linux-vm.sh exec mainline
// test/bin/landlockabi) の出力とくらべる。ふつうの人 (root なら nobody になる) で /tmp か . で動く
//   入口: landlock_create_ruleset (VERSION、ERRATA、attr の 7 つの欄と大きさ、quiet、permissions)、add_rule (QUIET、
//         PATH_BENEATH、NET_PORT の UDP、NAMESPACE、CAPABILITY)、restrict_self (LOG_*、TSYNC、NO_NEW_PRIVS、fd -1)
//   止めかた: UDP の bind / connect / sendto (と autobind)、RESOLVE_UNIX (名前のある unix ソケット)、IOCTL_DEV と
//         TRUNCATE (開いたときの許し)、scope (抽象 unix ソケット、シグナル)、namespace と capability の permission、
//         TSYNC (ほかのスレッドにも)
//   aios の中で: ./landlockabi      gcc -O2 -static -pthread -o landlockabi test/landlockabi.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/landlock.h>
#include <netinet/in.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

// 7.4 の欄と印 (古いヘッダにないもの)
struct attr74 {
    uint64_t fs, net, scoped, quiet_fs, quiet_net, quiet_scoped, perms;
};
struct ns_attr {
    uint64_t permissions, allowed, quiet;
};
#define ERRATA (1U << 1)
#define ADD_QUIET (1U << 0)
#define RS_LOG_SAME_EXEC_OFF (1U << 0)
#define RS_LOG_NEW_EXEC_ON (1U << 1)
#define RS_LOG_SUBDOMAINS_OFF (1U << 2)
#define RS_TSYNC (1U << 3)
#define RS_NNP (1U << 4)
#define RULE_PATH 1
#define RULE_PORT 2
#define RULE_NS 3
#define RULE_CAP 4
#define FS_EXEC (1ULL << 0)
#define FS_WRITE (1ULL << 1)
#define FS_READ (1ULL << 2)
#define FS_READ_DIR (1ULL << 3)
#define FS_TRUNCATE (1ULL << 14)
#define FS_IOCTL_DEV (1ULL << 15)
#define FS_RESOLVE_UNIX (1ULL << 16)
#define NET_BIND_TCP (1ULL << 0)
#define NET_CONNECT_TCP (1ULL << 1)
#define NET_BIND_UDP (1ULL << 2)
#define NET_SEND_UDP (1ULL << 3)
#define SCOPE_UNIX (1ULL << 0)
#define SCOPE_SIGNAL (1ULL << 1)
#define PERM_NS (1ULL << 0)
#define PERM_CAP (1ULL << 1)

static const char *err(int e) {
    switch (e) {
    case 0: return "0";
    case EINVAL: return "EINVAL";
    case E2BIG: return "E2BIG";
    case EFAULT: return "EFAULT";
    case ENOMSG: return "ENOMSG";
    case EBADF: return "EBADF";
    case EBADFD: return "EBADFD";
    case EPERM: return "EPERM";
    case EACCES: return "EACCES";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ENOTTY: return "ENOTTY";
    case ECONNREFUSED: return "ECONNREFUSED";
    case ENOENT: return "ENOENT";
    default: return strerror(e);
    }
}

static long create(const void *a, size_t size, unsigned flags) { return syscall(SYS_landlock_create_ruleset, a, size, flags); }
static long add(int fd, int type, const void *a, unsigned flags) { return syscall(SYS_landlock_add_rule, fd, type, a, flags); }
static long restrict_(int fd, unsigned flags) { return syscall(SYS_landlock_restrict_self, fd, flags); }

static void res(const char *name, long r) {
    if (r < 0)
        printf("%-56s %s\n", name, err(errno));
    else
        printf("%-56s %ld\n", name, r);
}

// 0 かどうかだけ (fd の番号は出さない)
static void ok(const char *name, long r) {
    printf("%-56s %s\n", name, r < 0 ? err(errno) : "0");
}

// 止められたか (EACCES / EPERM) だけ。ほかのしくじり (ネットワークがないなど) は "not denied"
static void denied(const char *name, long r) {
    int e = errno;
    printf("%-56s %s\n", name, r < 0 && (e == EACCES || e == EPERM) ? err(e) : "not denied");
}

static int ruleset(uint64_t fs, uint64_t net, uint64_t scoped, uint64_t perms) {
    struct attr74 a = {fs, net, scoped, 0, 0, 0, perms};
    return (int)create(&a, sizeof a, 0);
}

static int path_rule(int rs, const char *path, uint64_t allowed) {
    int fd = open(path, O_PATH);
    struct landlock_path_beneath_attr p = {allowed, fd};
    long r = add(rs, RULE_PATH, &p, 0);
    int e = errno;
    close(fd);
    errno = e;
    return (int)r;
}

static int port_rule(int rs, uint64_t port, uint64_t allowed) {
    struct landlock_net_port_attr p = {allowed, port};
    return (int)add(rs, RULE_PORT, &p, 0);
}

// 子で f を動かして待つ (層は外せないので)
static void child(void (*f)(void)) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        f();
        fflush(stdout);
        _exit(0);
    }
    waitpid(p, NULL, 0);
}

static char base[256];

static void enforce(int rs) {
    long r = restrict_(rs, 0);
    if (r < 0)
        printf("restrict_self: %s\n", err(errno));
    close(rs);
}

// ---- 入口 ----

static void t_restrict(void) {
    int rs = ruleset(FS_READ, 0, 0, 0);
    int nf = open("/dev/null", O_RDONLY);
    res("restrict_self flags 1<<5", restrict_(rs, 1U << 5));
    ok("restrict_self LOG_SAME_EXEC_OFF|LOG_NEW_EXEC_ON", restrict_(rs, RS_LOG_SAME_EXEC_OFF | RS_LOG_NEW_EXEC_ON));
    res("restrict_self fd -1, LOG_SUBDOMAINS_OFF", restrict_(-1, RS_LOG_SUBDOMAINS_OFF));
    res("restrict_self fd -1, LOG_SUBDOMAINS_OFF|TSYNC", restrict_(-1, RS_LOG_SUBDOMAINS_OFF | RS_TSYNC));
    res("restrict_self fd -1, LOG_SAME_EXEC_OFF", restrict_(-1, RS_LOG_SAME_EXEC_OFF));
    res("restrict_self fd -1, 0", restrict_(-1, 0));
    res("restrict_self a /dev/null fd", restrict_(nf, 0));
    close(nf);
    close(rs);
}

// ---- UDP ----

static void t_udp(void) {
    int rs = ruleset(0, NET_BIND_UDP | NET_SEND_UDP, 0, 0);
    port_rule(rs, 45001, NET_BIND_UDP | NET_SEND_UDP);
    port_rule(rs, 45002, NET_SEND_UDP);
    enforce(rs);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_ANY)};
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    a.sin_port = htons(45003);
    denied("udp bind 45003 (no rule)", bind(s, (void *)&a, sizeof a));
    a.sin_port = htons(45001);
    denied("udp bind 45001 (BIND_UDP)", bind(s, (void *)&a, sizeof a));
    struct sockaddr_in d = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    d.sin_port = htons(45002);
    denied("udp sendto 45002 from a bound socket", sendto(s, "x", 1, 0, (void *)&d, sizeof d));
    d.sin_port = htons(45004);
    denied("udp sendto 45004 (no rule)", sendto(s, "x", 1, 0, (void *)&d, sizeof d));
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    d.sin_port = htons(45002);
    denied("udp sendto 45002 unbound (autobind needs port 0)", sendto(u, "x", 1, 0, (void *)&d, sizeof d));
    denied("udp connect 45002 unbound (autobind)", connect(u, (void *)&d, sizeof d));
    struct sockaddr un = {.sa_family = AF_UNSPEC};
    denied("udp connect AF_UNSPEC", connect(u, &un, sizeof un));
    d.sin_port = htons(45002);
    denied("udp connect 45002 bound", connect(s, (void *)&d, sizeof d));
    denied("udp send on the connected socket", send(s, "x", 1, 0));
    int t = socket(AF_INET, SOCK_STREAM, 0);
    a.sin_port = htons(45005);
    denied("tcp bind 45005 (TCP not handled)", bind(t, (void *)&a, sizeof a));
    close(t);
    close(s);
    close(u);
}

static void t_udp_port0(void) {
    int rs = ruleset(0, NET_BIND_UDP | NET_SEND_UDP | NET_BIND_TCP, 0, 0);
    port_rule(rs, 0, NET_BIND_UDP);
    port_rule(rs, 45002, NET_SEND_UDP);
    enforce(rs);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in d = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK), .sin_port = htons(45002)};
    denied("with a port 0 rule: udp sendto 45002 unbound", sendto(u, "x", 1, 0, (void *)&d, sizeof d));
    int t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_ANY), .sin_port = htons(45006)};
    denied("tcp bind 45006 (BIND_TCP, no rule)", bind(t, (void *)&a, sizeof a));
    close(t);
    close(u);
}

// ---- 名前のある unix ソケット (RESOLVE_UNIX) ----

static void un_addr(struct sockaddr_un *a, const char *dir, const char *name) {
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    snprintf(a->sun_path, sizeof a->sun_path, "%s/%s/%s", base, dir, name);
}

static void t_resolve_unix(void) {
    char p[300];
    int rs = ruleset(FS_RESOLVE_UNIX, 0, 0, 0);
    snprintf(p, sizeof p, "%s/A", base);
    path_rule(rs, p, FS_RESOLVE_UNIX);
    enforce(rs);
    struct sockaddr_un a;
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    un_addr(&a, "A", "s");
    denied("connect A/s (RESOLVE_UNIX on A)", connect(c, (void *)&a, sizeof a));
    close(c);
    c = socket(AF_UNIX, SOCK_STREAM, 0);
    un_addr(&a, "B", "s");
    denied("connect B/s (made outside the domain)", connect(c, (void *)&a, sizeof a));
    close(c);
    int d = socket(AF_UNIX, SOCK_DGRAM, 0);
    un_addr(&a, "B", "d");
    denied("sendto B/d (dgram, outside)", sendto(d, "x", 1, 0, (void *)&a, sizeof a));
    // この中で作ったものは、同じドメインなのでよい
    int sv = socket(AF_UNIX, SOCK_STREAM, 0);
    un_addr(&a, "B", "mine");
    bind(sv, (void *)&a, sizeof a);
    listen(sv, 1);
    c = socket(AF_UNIX, SOCK_STREAM, 0);
    denied("connect B/mine (made in the same domain)", connect(c, (void *)&a, sizeof a));
    close(c);
    close(sv);
    close(d);
}

// ---- IOCTL_DEV と TRUNCATE (開いたときの許し) ----

static int pre_null, pre_file;

static void t_ioctl(void) {
    int rs = ruleset(FS_IOCTL_DEV | FS_TRUNCATE | FS_READ | FS_WRITE, 0, 0, 0);
    path_rule(rs, "/dev", FS_READ | FS_WRITE);
    path_rule(rs, base, FS_READ | FS_WRITE);
    enforce(rs);
    struct termios t;
    int n = open("/dev/null", O_RDWR);
    res("open /dev/null after", n < 0 ? -1 : 0);
    denied("  TCGETS on it (no IOCTL_DEV)", ioctl(n, TCGETS, &t));
    ok("  FIOCLEX on it (always allowed)", ioctl(n, FIOCLEX));
    int on = 1;
    ok("  FIONBIO on it (always allowed)", ioctl(n, FIONBIO, &on));
    res("TCGETS on /dev/null opened before", ioctl(pre_null, TCGETS, &t));
    char p[300];
    snprintf(p, sizeof p, "%s/A/f", base);
    int f = open(p, O_RDWR);
    int q = 0;
    res("FIONREAD on a regular file opened after", ioctl(f, FIONREAD, &q));
    denied("ftruncate on a file opened after (no TRUNCATE)", ftruncate(f, 1));
    ok("ftruncate on a file opened before", ftruncate(pre_file, 2));
    denied("truncate(path)", truncate(p, 3));
    denied("open O_TRUNC", open(p, O_RDWR | O_TRUNC));
    close(f);
    close(n);
}

static void t_ioctl_allowed(void) {
    int rs = ruleset(FS_IOCTL_DEV, 0, 0, 0);
    path_rule(rs, "/dev", FS_IOCTL_DEV);
    enforce(rs);
    struct termios t;
    int n = open("/dev/null", O_RDWR);
    res("with IOCTL_DEV on /dev: TCGETS on /dev/null", ioctl(n, TCGETS, &t));
    close(n);
}

// ---- scope ----

static struct sockaddr_un abs_addr(const char *name, socklen_t *len) {
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path + 1, sizeof a.sun_path - 1, "llabi-%d-%s", getppid(), name);
    *len = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(a.sun_path + 1);
    return a;
}

static void t_scope_unix(void) {
    enforce(ruleset(0, 0, SCOPE_UNIX, 0));
    socklen_t len;
    struct sockaddr_un a = abs_addr("outside", &len);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    denied("scoped: connect an abstract socket made outside", connect(c, (void *)&a, len));
    close(c);
    a = abs_addr("dgram", &len);
    int d = socket(AF_UNIX, SOCK_DGRAM, 0);
    denied("scoped: sendto an abstract dgram socket outside", sendto(d, "x", 1, 0, (void *)&a, len));
    // 中で作ったもの
    a = abs_addr("inside", &len);
    int sv = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(sv, (void *)&a, len);
    listen(sv, 1);
    c = socket(AF_UNIX, SOCK_STREAM, 0);
    denied("scoped: connect an abstract socket made inside", connect(c, (void *)&a, len));
    close(c);
    close(sv);
    // 名前のあるものは scope にかからない
    struct sockaddr_un p;
    un_addr(&p, "B", "s");
    c = socket(AF_UNIX, SOCK_STREAM, 0);
    denied("scoped: connect a pathname socket outside", connect(c, (void *)&p, sizeof p));
    close(c);
    close(d);
}

static void t_scope_signal(void) {
    pid_t outside = getppid();
    enforce(ruleset(0, 0, SCOPE_SIGNAL, 0));
    denied("scoped: kill(parent, 0)", kill(outside, 0));
    denied("scoped: kill(self, 0)", kill(getpid(), 0));
    pid_t g = fork();
    if (g == 0) {
        pause();
        _exit(0);
    }
    denied("scoped: kill(a child made inside, 0)", kill(g, 0));
    kill(g, SIGKILL);
    waitpid(g, NULL, 0);
}

static void t_scope_signal_nested(void) {
    // 子 (ドメインあり) が、さらに中の層を作った孫から見て外
    enforce(ruleset(0, 0, SCOPE_SIGNAL, 0));
    pid_t mid = getpid();
    pid_t g = fork();
    if (g == 0) {
        enforce(ruleset(0, 0, SCOPE_SIGNAL, 0));
        denied("nested: kill(the outer domain's process, 0)", kill(mid, 0));
        fflush(stdout);
        _exit(0);
    }
    waitpid(g, NULL, 0);
    g = fork();
    if (g == 0) {
        pause();
        _exit(0);
    }
    pid_t h = fork();
    if (h == 0) {
        enforce(ruleset(FS_READ, 0, 0, 0));
        pause();
        _exit(0);
    }
    usleep(100000);
    denied("nested: kill(a process in an inner domain, 0)", kill(h, 0));
    kill(g, SIGKILL);
    kill(h, SIGKILL);
    waitpid(g, NULL, 0);
    waitpid(h, NULL, 0);
}

// ---- namespace と capability ----

// unshare と sethostname は errno まで出す (ユーザーの namespace のないカーネルの EINVAL と見わける)
static void t_ns(void) {
    int rs = ruleset(0, 0, 0, PERM_NS);
    struct ns_attr n = {PERM_NS, CLONE_NEWUSER, 0};
    add(rs, RULE_NS, &n, 0);
    enforce(rs);
    ok("ns: unshare(CLONE_NEWUSER) (allowed)", unshare(CLONE_NEWUSER));
    ok("ns: then unshare(CLONE_NEWUTS)", unshare(CLONE_NEWUTS));
}

static void t_ns_none(void) {
    enforce(ruleset(0, 0, 0, PERM_NS));
    ok("ns: no rule: unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
}

static void t_ns_uts(void) {
    int rs = ruleset(0, 0, 0, PERM_NS);
    struct ns_attr n = {PERM_NS, CLONE_NEWUSER | CLONE_NEWUTS, 0};
    add(rs, RULE_NS, &n, 0);
    enforce(rs);
    ok("ns USER|UTS: unshare(NEWUSER|NEWUTS)", unshare(CLONE_NEWUSER | CLONE_NEWUTS));
    ok("ns USER|UTS: then sethostname", sethostname("ll", 2));
}

static void t_cap_sysadmin(void) {
    int rs = ruleset(0, 0, 0, PERM_CAP);
    struct ns_attr c = {PERM_CAP, 1ULL << 21 /* CAP_SYS_ADMIN */, 0};
    add(rs, RULE_CAP, &c, 0);
    enforce(rs);
    ok("cap SYS_ADMIN: unshare(NEWUSER|NEWUTS)", unshare(CLONE_NEWUSER | CLONE_NEWUTS));
    ok("cap SYS_ADMIN: sethostname", sethostname("ll", 2));
}

static void t_cap_netraw(void) {
    int rs = ruleset(0, 0, 0, PERM_CAP);
    struct ns_attr c = {PERM_CAP, 1ULL << 13 /* CAP_NET_RAW */, 0};
    add(rs, RULE_CAP, &c, 0);
    enforce(rs);
    ok("cap NET_RAW only: unshare(NEWUSER|NEWUTS)", unshare(CLONE_NEWUSER | CLONE_NEWUTS));
    ok("cap NET_RAW only: unshare(NEWUSER)", unshare(CLONE_NEWUSER));
    ok("cap NET_RAW only: then unshare(NEWUTS)", unshare(CLONE_NEWUTS));
    ok("cap NET_RAW only: then sethostname", sethostname("ll", 2));
}

// ---- TSYNC ----

static volatile int t_go, t_done;
static volatile long t_res;
static volatile int t_errno;
static void *opener(void *a) {
    (void)a;
    while (!t_go)
        usleep(1000);
    char p[300];
    snprintf(p, sizeof p, "%s/A/f", base);
    t_res = open(p, O_RDONLY);
    t_errno = errno;
    if (t_res >= 0)
        close((int)t_res);
    t_done = 1;
    return NULL;
}

static void tsync(unsigned flags, const char *name) {
    pthread_t th;
    t_go = t_done = 0;
    pthread_create(&th, NULL, opener, NULL);
    long r = restrict_(ruleset(FS_READ, 0, 0, 0), flags);
    if (r < 0)
        printf("restrict_self: %s\n", err(errno));
    t_go = 1;
    pthread_join(th, NULL);
    errno = t_errno;
    denied(name, t_res);
}

static void t_tsync(void) { tsync(RS_TSYNC, "TSYNC: the other thread opens a file"); }
static void t_notsync(void) { tsync(0, "no TSYNC: the other thread opens a file"); }

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (geteuid() == 0 && (setgid(65534) < 0 || setuid(65534) < 0))
        return 1;
    snprintf(base, sizeof base, "/tmp/llabiXXXXXX");
    if (!mkdtemp(base)) {
        snprintf(base, sizeof base, "./llabiXXXXXX");
        if (!mkdtemp(base))
            return 1;
    }
    char p[300];
    snprintf(p, sizeof p, "%s/A", base);
    mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/B", base);
    mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/A/f", base);
    close(open(p, O_CREAT | O_RDWR, 0644));

    // ---- create_ruleset ----
    res("VERSION", create(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION));
    res("ERRATA", create(NULL, 0, ERRATA));
    struct attr74 a = {FS_READ, 0, 0, 0, 0, 0, 0};
    res("VERSION with attr", create(&a, 0, LANDLOCK_CREATE_RULESET_VERSION));
    res("VERSION with size", create(NULL, 8, LANDLOCK_CREATE_RULESET_VERSION));
    res("VERSION|ERRATA", create(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION | ERRATA));
    res("flags 1<<2", create(NULL, 0, 4));
    res("size 0", create(&a, 0, 0));
    res("size 7", create(&a, 7, 0));
    ok("size 8 (handled_access_fs only)", create(&a, 8, 0));
    ok("size 56", create(&a, sizeof a, 0));
    char big[80];
    memset(big, 0, sizeof big);
    memcpy(big, &a, sizeof a);
    ok("size 80, the rest 0", create(big, sizeof big, 0));
    big[70] = 1;
    res("size 80, the rest not 0", create(big, sizeof big, 0));
    static char page[8192];
    memcpy(page, &a, sizeof a);
    res("size 4097", create(page, 4097, 0));
    res("attr NULL, size 56", create(NULL, sizeof a, 0));
    struct attr74 b;
    b = (struct attr74){1ULL << 17, 0, 0, 0, 0, 0, 0};
    res("handled_access_fs 1<<17", create(&b, sizeof b, 0));
    b = (struct attr74){(1ULL << 17) - 1, 0, 0, 0, 0, 0, 0};
    ok("handled_access_fs all 17 rights", create(&b, sizeof b, 0));
    b = (struct attr74){0, 1ULL << 4, 0, 0, 0, 0, 0};
    res("handled_access_net 1<<4", create(&b, sizeof b, 0));
    b = (struct attr74){0, 15, 0, 0, 0, 0, 0};
    ok("handled_access_net all 4", create(&b, sizeof b, 0));
    b = (struct attr74){0, 0, 1ULL << 2, 0, 0, 0, 0};
    res("scoped 1<<2", create(&b, sizeof b, 0));
    b = (struct attr74){0, 0, 3, 0, 0, 0, 0};
    ok("scoped only", create(&b, sizeof b, 0));
    b = (struct attr74){0, 0, 0, 0, 0, 0, 1ULL << 2};
    res("handled_permissions 1<<2", create(&b, sizeof b, 0));
    b = (struct attr74){0, 0, 0, 0, 0, 0, 3};
    ok("handled_permissions only", create(&b, sizeof b, 0));
    b = (struct attr74){FS_READ, 0, 0, FS_WRITE, 0, 0, 0};
    res("quiet_access_fs not in handled", create(&b, sizeof b, 0));
    b = (struct attr74){FS_READ, 1, 0, FS_READ, 2, 0, 0};
    res("quiet_access_net not in handled", create(&b, sizeof b, 0));
    b = (struct attr74){0, 0, 1, 0, 0, 2, 0};
    res("quiet_scoped not in scoped", create(&b, sizeof b, 0));
    b = (struct attr74){FS_READ, 1, 1, FS_READ, 1, 1, 0};
    ok("quiet masks inside handled", create(&b, sizeof b, 0));
    b = (struct attr74){0, 0, 0, 0, 0, 0, 0};
    res("all empty", create(&b, sizeof b, 0));
    int rs = ruleset(FS_READ | FS_READ_DIR | FS_RESOLVE_UNIX | FS_TRUNCATE, NET_BIND_UDP | NET_SEND_UDP, 0, PERM_NS | PERM_CAP);
    printf("%-56s cloexec %d\n", "the ruleset fd", (fcntl(rs, F_GETFD) & FD_CLOEXEC) != 0);
    char buf[8];
    res("read on the ruleset fd", read(rs, buf, sizeof buf));
    char link[64] = "", lp[64];
    snprintf(lp, sizeof lp, "/proc/self/fd/%d", rs);
    ssize_t ln = readlink(lp, link, sizeof link - 1);
    if (ln > 0)
        link[ln] = 0;
    printf("%-56s %s\n", "its /proc/self/fd link", link);

    // ---- add_rule ----
    snprintf(p, sizeof p, "%s/A", base);
    int dfd = open(p, O_PATH);
    snprintf(p, sizeof p, "%s/A/f", base);
    int ffd = open(p, O_PATH);
    struct landlock_path_beneath_attr pb = {FS_READ, dfd};
    res("add_rule flags 2", add(rs, RULE_PATH, &pb, 2));
    res("add_rule bad fd", add(99, RULE_PATH, &pb, 0));
    int nf = open("/dev/null", O_RDONLY);
    res("add_rule a /dev/null fd", add(nf, RULE_PATH, &pb, 0));
    res("add_rule type 5", add(rs, 5, &pb, 0));
    res("add_rule type 0", add(rs, 0, &pb, 0));
    ok("path: READ on a dir", add(rs, RULE_PATH, &pb, 0));
    pb.allowed_access = FS_RESOLVE_UNIX;
    ok("path: RESOLVE_UNIX on a dir", add(rs, RULE_PATH, &pb, 0));
    pb.parent_fd = ffd;
    ok("path: RESOLVE_UNIX on a file", add(rs, RULE_PATH, &pb, 0));
    pb.allowed_access = FS_READ_DIR;
    res("path: READ_DIR on a file", add(rs, RULE_PATH, &pb, 0));
    pb.parent_fd = dfd;
    pb.allowed_access = FS_WRITE;
    res("path: WRITE not handled", add(rs, RULE_PATH, &pb, 0));
    pb.allowed_access = 0;
    res("path: allowed 0", add(rs, RULE_PATH, &pb, 0));
    res("path: allowed 0, QUIET, no quiet_access_fs", add(rs, RULE_PATH, &pb, ADD_QUIET));
    pb.parent_fd = nf;
    pb.allowed_access = FS_READ;
    ok("path: a /dev/null fd as the parent", add(rs, RULE_PATH, &pb, 0));
    pb.parent_fd = rs;
    res("path: the ruleset fd as the parent", add(rs, RULE_PATH, &pb, 0));
    pb.parent_fd = 99;
    res("path: a closed fd as the parent", add(rs, RULE_PATH, &pb, 0));
    res("path: attr NULL", add(rs, RULE_PATH, NULL, 0));
    struct attr74 qa = {FS_READ, NET_BIND_UDP, 0, FS_READ, NET_BIND_UDP, 0, 0};
    int qrs = (int)create(&qa, sizeof qa, 0);
    pb = (struct landlock_path_beneath_attr){0, dfd};
    ok("path: allowed 0 with QUIET (quiet_access_fs set)", add(qrs, RULE_PATH, &pb, ADD_QUIET));
    struct landlock_net_port_attr np = {0, 53};
    ok("port: allowed 0 with QUIET (quiet_access_net set)", add(qrs, RULE_PORT, &np, ADD_QUIET));
    close(qrs);
    np = (struct landlock_net_port_attr){NET_BIND_UDP | NET_SEND_UDP, 53};
    ok("port: UDP bind and send", add(rs, RULE_PORT, &np, 0));
    np.allowed_access = NET_BIND_TCP;
    res("port: BIND_TCP not handled", add(rs, RULE_PORT, &np, 0));
    np = (struct landlock_net_port_attr){NET_BIND_UDP, 65536};
    res("port: 65536", add(rs, RULE_PORT, &np, 0));
    np = (struct landlock_net_port_attr){0, 53};
    res("port: allowed 0", add(rs, RULE_PORT, &np, 0));
    int fsonly = ruleset(FS_READ, 0, 0, 0);
    np = (struct landlock_net_port_attr){NET_BIND_UDP, 53};
    res("port: a ruleset without net", add(fsonly, RULE_PORT, &np, 0));
    struct ns_attr ns = {PERM_NS, CLONE_NEWUTS, 0};
    ok("ns: CLONE_NEWUTS", add(rs, RULE_NS, &ns, 0));
    res("ns: QUIET", add(rs, RULE_NS, &ns, ADD_QUIET));
    ns.permissions = 0;
    res("ns: permissions 0", add(rs, RULE_NS, &ns, 0));
    ns.permissions = PERM_CAP;
    res("ns: permissions CAPABILITY_USE", add(rs, RULE_NS, &ns, 0));
    ns.permissions = PERM_NS | PERM_CAP;
    res("ns: permissions both", add(rs, RULE_NS, &ns, 0));
    ns = (struct ns_attr){PERM_NS, 0, 0};
    res("ns: allowed 0, quiet 0", add(rs, RULE_NS, &ns, 0));
    ns = (struct ns_attr){PERM_NS, 0, CLONE_NEWNET};
    ok("ns: quiet only", add(rs, RULE_NS, &ns, 0));
    ns = (struct ns_attr){PERM_NS, 1ULL << 40, 0};
    ok("ns: unknown bits only", add(rs, RULE_NS, &ns, 0));
    ns = (struct ns_attr){PERM_NS, CLONE_NEWUTS, 0};
    res("ns: a ruleset without permissions", add(fsonly, RULE_NS, &ns, 0));
    struct ns_attr cap = {PERM_CAP, 1ULL << 21, 0};
    ok("cap: CAP_SYS_ADMIN", add(rs, RULE_CAP, &cap, 0));
    res("cap: QUIET", add(rs, RULE_CAP, &cap, ADD_QUIET));
    cap.permissions = PERM_NS;
    res("cap: permissions NAMESPACE_USE", add(rs, RULE_CAP, &cap, 0));
    cap = (struct ns_attr){PERM_CAP, 0, 0};
    res("cap: allowed 0, quiet 0", add(rs, RULE_CAP, &cap, 0));
    cap = (struct ns_attr){PERM_CAP, 1ULL << 63, 0};
    ok("cap: bit 63 only", add(rs, RULE_CAP, &cap, 0));
    res("cap: attr NULL", add(rs, RULE_CAP, NULL, 0));
    close(fsonly);
    close(nf);
    close(ffd);
    close(dfd);

    // ---- restrict_self ----
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        res("restrict_self without no_new_privs", restrict_(rs, 0));
        res("restrict_self NO_NEW_PRIVS", restrict_(rs, RS_NNP));
        printf("%-56s %d\n", "  no_new_privs afterwards", prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0));
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    close(rs);
    child(t_restrict);

    // ---- 止めかた ----
    // 外で作っておくもの: B/s (stream)、B/d (dgram)、A/s、抽象の outside と dgram
    struct sockaddr_un ua;
    int servers[6], ns_ = 0;
    const char *names[][2] = {{"A", "s"}, {"B", "s"}, {"B", "d"}};
    for (int i = 0; i < 3; i++) {
        int type = names[i][1][0] == 'd' ? SOCK_DGRAM : SOCK_STREAM;
        int s = socket(AF_UNIX, type, 0);
        un_addr(&ua, names[i][0], names[i][1]);
        bind(s, (void *)&ua, sizeof ua);
        if (type == SOCK_STREAM)
            listen(s, 8);
        servers[ns_++] = s;
    }
    for (int i = 0; i < 2; i++) {
        struct sockaddr_un aa = {.sun_family = AF_UNIX};
        snprintf(aa.sun_path + 1, sizeof aa.sun_path - 1, "llabi-%d-%s", getpid(), i ? "dgram" : "outside");
        socklen_t al = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(aa.sun_path + 1);
        int s = socket(AF_UNIX, i ? SOCK_DGRAM : SOCK_STREAM, 0);
        bind(s, (void *)&aa, al);
        if (!i)
            listen(s, 8);
        servers[ns_++] = s;
    }
    child(t_udp);
    child(t_udp_port0);
    child(t_resolve_unix);
    pre_null = open("/dev/null", O_RDWR);
    snprintf(p, sizeof p, "%s/A/f", base);
    pre_file = open(p, O_RDWR);
    child(t_ioctl);
    child(t_ioctl_allowed);
    child(t_scope_unix);
    child(t_scope_signal);
    child(t_scope_signal_nested);
    child(t_ns);
    child(t_ns_none);
    child(t_ns_uts);
    child(t_cap_sysadmin);
    child(t_cap_netraw);
    child(t_tsync);
    child(t_notsync);
    for (int i = 0; i < ns_; i++)
        close(servers[i]);
    return 0;
}
