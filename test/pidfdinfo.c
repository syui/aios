// pidfd のことを答える口 (Linux 7.4): /proc/PID/fdinfo/N (pos、flags、mnt_id、ino と、pidfd の Pid: / NSpid:、eventfd、
// timerfd、epoll、inotify、AF_UNIX の scm_fds)、ioctl の PIDFD_GET_INFO (大きさの版、mask、終わったときの exit_code、
// coredump の印)、pidfd_getfd、pidfd_send_signal の flags と siginfo と PIDFD_SELF_*。
// 出すだけのテスト: 手本の Linux (test/linux-vm.sh exec mainline test/bin/pidfdinfo) とくらべる。ふつうの人 (root なら
// nobody になる) で /tmp か . で動く。番号 (pid、ino、mnt_id) はじかに出さず、あてはまるかを出す
//   aios の中で: ./pidfdinfo      gcc -O2 -static -pthread -o pidfdinfo test/pidfdinfo.c
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct pidfd_info {
    uint64_t mask, cgroupid;
    uint32_t pid, tgid, ppid, ruid, rgid, euid, egid, suid, sgid, fsuid, fsgid;
    int32_t exit_code;
    uint32_t coredump_mask, coredump_signal, coredump_code, coredump_pad;
    uint64_t supported_mask;
};
#define INFO_PID (1ULL << 0)
#define INFO_CREDS (1ULL << 1)
#define INFO_CGROUPID (1ULL << 2)
#define INFO_EXIT (1ULL << 3)
#define INFO_COREDUMP (1ULL << 4)
#define INFO_SUPPORTED (1ULL << 5)
#define GET_INFO_SZ(n) _IOWR(0xFF, 11, char[n])
#define PIDFD_GET_INFO GET_INFO_SZ(88)
#define PIDFD_THREAD O_EXCL
#define PIDFD_NONBLOCK O_NONBLOCK
#define SIG_THREAD 1
#define SIG_THREAD_GROUP 2
#define SIG_PROCESS_GROUP 4
#define SELF_THREAD -10000
#define SELF_THREAD_GROUP -10001

static const char *err(int e) {
    switch (e) {
    case 0: return "0";
    case EINVAL: return "EINVAL";
    case EBADF: return "EBADF";
    case ESRCH: return "ESRCH";
    case ENOENT: return "ENOENT";
    case ENOTTY: return "ENOTTY";
    case EPERM: return "EPERM";
    case EFAULT: return "EFAULT";
    case EACCES: return "EACCES";
    case EREMOTE: return "EREMOTE";
    default: return strerror(e);
    }
}

static void res(const char *name, long r) {
    if (r < 0)
        printf("%-52s %s\n", name, err(errno));
    else
        printf("%-52s %ld\n", name, r);
}

static void ok(const char *name, long r) {
    printf("%-52s %s\n", name, r < 0 ? err(errno) : "0");
}

static void yes(const char *name, int v) {
    printf("%-52s %s\n", name, v ? "yes" : "no");
}

static int pidfd_open(pid_t pid, unsigned flags) { return (int)syscall(SYS_pidfd_open, pid, flags); }
static int pidfd_getfd(int pidfd, int fd, unsigned flags) { return (int)syscall(SYS_pidfd_getfd, pidfd, fd, flags); }
static int send_sig(int pidfd, int sig, siginfo_t *info, unsigned flags) { return (int)syscall(SYS_pidfd_send_signal, pidfd, sig, info, flags); }

// /proc/PID/fdinfo/FD を buf に (なければ -errno)
static int fdinfo(pid_t pid, int fd, char *buf, size_t n) {
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/fdinfo/%d", pid, fd);
    int f = open(p, O_RDONLY);
    if (f < 0)
        return -errno;
    ssize_t k = read(f, buf, n - 1);
    int e = errno;
    close(f);
    if (k < 0)
        return -e;
    buf[k] = 0;
    return 0;
}

// key ("pos:" など) の行の値 (なければ "-")
static const char *field(const char *buf, const char *key) {
    static char v[256];
    size_t kl = strlen(key);
    for (const char *l = buf; l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        if (strncmp(l, key, kl) == 0) {
            const char *s = l + kl;
            while (*s == '\t' || *s == ' ')
                s++;
            size_t n = strcspn(s, "\n");
            if (n >= sizeof v)
                n = sizeof v - 1;
            memcpy(v, s, n);
            v[n] = 0;
            return v;
        }
    }
    return "-";
}

// st_dev (ユーザーの形) をカーネルの dev_t (major << 20 | minor) に
static unsigned kdev(dev_t d) { return major(d) << 20 | minor(d); }

// fdinfo の 4 つの行: pos と flags はそのまま、mnt_id は 0 より大きいか、ino は fstat と同じか
static void common(const char *name, int fd) {
    char b[4096], n[128];
    int r = fdinfo(getpid(), fd, b, sizeof b);
    if (r < 0) {
        printf("%-52s %s\n", name, err(-r));
        return;
    }
    struct stat st;
    fstat(fd, &st);
    snprintf(n, sizeof n, "%s: pos", name);
    printf("%-52s %s\n", n, field(b, "pos:"));
    snprintf(n, sizeof n, "%s: flags", name);
    printf("%-52s %s\n", n, field(b, "flags:"));
    snprintf(n, sizeof n, "%s: mnt_id > 0", name);
    yes(n, atol(field(b, "mnt_id:")) > 0);
    snprintf(n, sizeof n, "%s: ino is st_ino", name);
    yes(n, strtoull(field(b, "ino:"), NULL, 10) == (unsigned long long)st.st_ino);
}

static long mnt_of(int fd) {
    char b[4096];
    return fdinfo(getpid(), fd, b, sizeof b) < 0 ? -1 : atol(field(b, "mnt_id:"));
}

// ---- fdinfo ----

static void t_fdinfo(const char *dir) {
    char path[300], b[4096];
    snprintf(path, sizeof path, "%s/file", dir);
    int w = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
    write(w, "hello world", 11);
    close(w);

    int f = open(path, O_RDONLY);
    char x[8];
    read(f, x, 5);
    common("file O_RDONLY after reading 5", f);
    int g = open(path, O_RDWR | O_APPEND | O_CLOEXEC);
    common("file O_RDWR|O_APPEND|O_CLOEXEC", g);
    fcntl(g, F_SETFD, 0);
    printf("%-52s %s\n", "  after F_SETFD 0: flags", (fdinfo(getpid(), g, b, sizeof b), field(b, "flags:")));
    fcntl(g, F_SETFL, O_NONBLOCK);
    printf("%-52s %s\n", "  after F_SETFL O_NONBLOCK: flags", (fdinfo(getpid(), g, b, sizeof b), field(b, "flags:")));
    printf("%-52s 0%o\n", "  F_GETFL", fcntl(g, F_GETFL));
    yes("two files on the same fs: same mnt_id", mnt_of(f) == mnt_of(g));
    int pr = open("/proc/self/stat", O_RDONLY);
    yes("a /proc file: another mnt_id", mnt_of(pr) != mnt_of(f) && mnt_of(pr) > 0);
    close(pr);
    int op = open(dir, O_PATH | O_DIRECTORY);
    common("dir O_PATH|O_DIRECTORY", op);
    printf("%-52s 0%o\n", "  F_GETFL", fcntl(op, F_GETFL));
    int dn = open("/dev/null", O_WRONLY);
    common("/dev/null O_WRONLY", dn);
    printf("%-52s 0%o\n", "  F_GETFL", fcntl(dn, F_GETFL));

    int p[2];
    pipe2(p, O_NONBLOCK);
    common("pipe2(O_NONBLOCK) read end", p[0]);
    common("pipe2(O_NONBLOCK) write end", p[1]);
    printf("%-52s 0%o\n", "  F_GETFL of the write end", fcntl(p[1], F_GETFL));

    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    common("socketpair stream", sv[0]);
    printf("%-52s %s\n", "  scm_fds", (fdinfo(getpid(), sv[1], b, sizeof b), field(b, "scm_fds:")));
    {
        char c = 'x';
        struct iovec io = {&c, 1};
        char cb[CMSG_SPACE(2 * sizeof(int))];
        struct msghdr m = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb};
        struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
        cm->cmsg_level = SOL_SOCKET;
        cm->cmsg_type = SCM_RIGHTS;
        cm->cmsg_len = CMSG_LEN(2 * sizeof(int));
        int two[2] = {f, dn};
        memcpy(CMSG_DATA(cm), two, sizeof two);
        sendmsg(sv[0], &m, 0);
    }
    printf("%-52s %s\n", "  scm_fds after sending 2 fds", (fdinfo(getpid(), sv[1], b, sizeof b), field(b, "scm_fds:")));
    printf("%-52s %s\n", "  scm_fds of the sender", (fdinfo(getpid(), sv[0], b, sizeof b), field(b, "scm_fds:")));
    int dg = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    common("unix dgram SOCK_NONBLOCK", dg);
    printf("%-52s %s\n", "  scm_fds", (fdinfo(getpid(), dg, b, sizeof b), field(b, "scm_fds:")));
    int in = socket(AF_INET, SOCK_DGRAM, 0);
    common("inet dgram", in);
    printf("%-52s %s\n", "  scm_fds", (fdinfo(getpid(), in, b, sizeof b), field(b, "scm_fds:")));

    int ev = eventfd(5, EFD_SEMAPHORE | EFD_NONBLOCK);
    common("eventfd(5, SEMAPHORE|NONBLOCK)", ev);
    fdinfo(getpid(), ev, b, sizeof b);
    printf("%-52s [%s]\n", "  eventfd-count", field(b, "eventfd-count:"));
    printf("%-52s %s\n", "  eventfd-semaphore", field(b, "eventfd-semaphore:"));
    yes("  eventfd-id is a number", strcmp(field(b, "eventfd-id:"), "-") != 0);
    uint64_t one;
    read(ev, &one, 8);
    printf("%-52s [%s]\n", "  eventfd-count after one read", (fdinfo(getpid(), ev, b, sizeof b), field(b, "eventfd-count:")));

    int tf = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    common("timerfd MONOTONIC NONBLOCK", tf);
    fdinfo(getpid(), tf, b, sizeof b);
    printf("%-52s %s\n", "  clockid", field(b, "clockid:"));
    printf("%-52s %s\n", "  ticks", field(b, "ticks:"));
    printf("%-52s %s\n", "  settime flags", field(b, "settime flags:"));
    printf("%-52s %s\n", "  it_value (disarmed)", field(b, "it_value:"));
    struct itimerspec its = {{3, 500}, {100, 0}};
    timerfd_settime(tf, 0, &its, NULL);
    fdinfo(getpid(), tf, b, sizeof b);
    unsigned long long vs = 0, vn = 0;
    sscanf(field(b, "it_value:"), "(%llu, %llu)", &vs, &vn);
    yes("  armed 100 s: it_value is 99 or 100 s", vs == 99 || (vs == 100 && vn == 0));
    printf("%-52s %s\n", "  it_interval", field(b, "it_interval:"));
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    struct itimerspec at = {{0, 0}, {now.tv_sec + 50, 0}};
    timerfd_settime(tf, TFD_TIMER_ABSTIME, &at, NULL);
    printf("%-52s %s\n", "  ABSTIME: settime flags", (fdinfo(getpid(), tf, b, sizeof b), field(b, "settime flags:")));
    struct itimerspec soon = {{0, 0}, {0, 1000}};
    timerfd_settime(tf, 0, &soon, NULL);
    usleep(20000);
    fdinfo(getpid(), tf, b, sizeof b);
    printf("%-52s %s\n", "  expired once: ticks", field(b, "ticks:"));
    printf("%-52s %s\n", "  expired once: it_value", field(b, "it_value:"));
    int rt = timerfd_create(CLOCK_REALTIME, 0);
    printf("%-52s %s\n", "timerfd REALTIME: clockid", (fdinfo(getpid(), rt, b, sizeof b), field(b, "clockid:")));

    int ep = epoll_create1(EPOLL_CLOEXEC);
    common("epoll_create1(EPOLL_CLOEXEC)", ep);
    struct epoll_event e1 = {.events = EPOLLIN, .data.u64 = 0x1234};
    struct epoll_event e2 = {.events = EPOLLOUT | EPOLLET, .data.u64 = 0xabcdef0011ULL};
    struct epoll_event e3 = {.events = EPOLLIN | EPOLLONESHOT, .data.u64 = 7};
    epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &e1);
    epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e2);
    epoll_ctl(ep, EPOLL_CTL_ADD, f, &e3);
    fdinfo(getpid(), ep, b, sizeof b);
    // 並びは Linux の木の順 (file のアドレス) なので、fd の順にして出す
    int targets[3] = {p[0], ev, f};
    int order[3] = {0, 1, 2};
    for (int i = 0; i < 3; i++)
        for (int j = i + 1; j < 3; j++)
            if (targets[order[j]] < targets[order[i]]) {
                int s = order[i];
                order[i] = order[j];
                order[j] = s;
            }
    for (int k = 0; k < 3; k++) {
        int fd = targets[order[k]];
        char key[64];
        snprintf(key, sizeof key, "tfd: %8d", fd);
        const char *l = strstr(b, key);
        char nm[64];
        snprintf(nm, sizeof nm, "  epoll item %d", k);
        if (!l) {
            printf("%-52s missing\n", nm);
            continue;
        }
        unsigned ev_, sdev;
        unsigned long long data, ino;
        long long pos;
        int tfd;
        int got = sscanf(l, "tfd: %d events: %x data: %llx  pos:%lld ino:%llx sdev:%x", &tfd, &ev_, &data, &pos, &ino, &sdev);
        struct stat st;
        fstat(fd, &st);
        printf("%-52s got %d, events %x, data %llx, pos %lld, ino %s, sdev %s\n", nm, got, ev_, data, pos, ino == (unsigned long long)st.st_ino ? "same" : "other",
               sdev == kdev(st.st_dev) ? "same" : "other");
    }
    int lines = 0;
    for (const char *s = b; (s = strstr(s, "tfd:")); s++)
        lines++;
    printf("%-52s %d\n", "  epoll items", lines);
    epoll_ctl(ep, EPOLL_CTL_DEL, ev, NULL);
    lines = 0;
    fdinfo(getpid(), ep, b, sizeof b);
    for (const char *s = b; (s = strstr(s, "tfd:")); s++)
        lines++;
    printf("%-52s %d\n", "  epoll items after one DEL", lines);

    int ino = inotify_init1(IN_NONBLOCK);
    common("inotify_init1(IN_NONBLOCK)", ino);
    int wd = inotify_add_watch(ino, dir, IN_CREATE | IN_DELETE);
    fdinfo(getpid(), ino, b, sizeof b);
    {
        const char *l = strstr(b, "inotify wd:");
        unsigned wd_, mask, ig;
        unsigned long long ii;
        unsigned sd;
        struct stat st;
        stat(dir, &st);
        int got = l ? sscanf(l, "inotify wd:%x ino:%llx sdev:%x mask:%x ignored_mask:%x", &wd_, &ii, &sd, &mask, &ig) : 0;
        printf("%-52s got %d, wd %s, mask %x, ignored %x, ino %s, sdev %s\n", "  inotify watch", got, (int)wd_ == wd ? "same" : "other", mask, ig,
               ii == (unsigned long long)st.st_ino ? "same" : "other", sd == kdev(st.st_dev) ? "same" : "other");
    }

    int pf = pidfd_open(getpid(), 0);
    common("pidfd_open(self)", pf);
    fdinfo(getpid(), pf, b, sizeof b);
    yes("  Pid: is getpid()", atoi(field(b, "Pid:")) == getpid());
    yes("  NSpid: is getpid()", atoi(field(b, "NSpid:")) == getpid());
    int pn = pidfd_open(getpid(), PIDFD_NONBLOCK);
    printf("%-52s %s\n", "pidfd_open(self, NONBLOCK): flags", (fdinfo(getpid(), pn, b, sizeof b), field(b, "flags:")));
    printf("%-52s 0%o\n", "  F_GETFL", fcntl(pn, F_GETFL));
    int pt = pidfd_open(getpid(), PIDFD_THREAD);
    printf("%-52s %s\n", "pidfd_open(self, THREAD): flags", (fdinfo(getpid(), pt, b, sizeof b), field(b, "flags:")));

    // 子のもの: ゾンビのあいだと、片づけたあと
    pid_t c = fork();
    if (c == 0) {
        pause();
        _exit(0);
    }
    int cf = pidfd_open(c, 0);
    fdinfo(getpid(), cf, b, sizeof b);
    yes("child pidfd: Pid: is the child", atoi(field(b, "Pid:")) == c);
    kill(c, SIGKILL);
    siginfo_t si;
    waitid(P_PID, c, &si, WEXITED | WNOWAIT);
    fdinfo(getpid(), cf, b, sizeof b);
    yes("  zombie: Pid: is the child", atoi(field(b, "Pid:")) == c);
    waitpid(c, NULL, 0);
    fdinfo(getpid(), cf, b, sizeof b);
    printf("%-52s %s\n", "  reaped: Pid:", field(b, "Pid:"));
    printf("%-52s %s\n", "  reaped: NSpid:", field(b, "NSpid:"));

    // ほかのプロセスの fdinfo、ないもの、一覧
    c = fork();
    if (c == 0) {
        int k = open(path, O_RDONLY | O_NONBLOCK);
        dup2(k, 40);
        lseek(40, 3, SEEK_SET);
        pause();
        _exit(0);
    }
    usleep(100000);
    int r = fdinfo(c, 40, b, sizeof b);
    if (r < 0)
        printf("%-52s %s\n", "another process's fd 40", err(-r));
    printf("%-52s %s\n", "another process's fd 40: pos", field(b, "pos:"));
    printf("%-52s %s\n", "another process's fd 40: flags", field(b, "flags:"));
    kill(c, SIGKILL);
    waitpid(c, NULL, 0);
    r = fdinfo(getpid(), 99, b, sizeof b);
    printf("%-52s %s\n", "fdinfo of a closed fd", err(-r));
    {
        int f2 = open("/proc/self/fdinfo/abc", O_RDONLY);
        res("fdinfo/abc", f2 < 0 ? -1 : 0);
        if (f2 >= 0)
            close(f2);
    }
    {
        int n1 = 0, n2 = 0;
        DIR *d = opendir("/proc/self/fdinfo");
        struct dirent *de;
        while (d && (de = readdir(d)))
            n1 += de->d_name[0] != '.';
        if (d)
            closedir(d);
        d = opendir("/proc/self/fd");
        while (d && (de = readdir(d)))
            n2 += de->d_name[0] != '.';
        if (d)
            closedir(d);
        yes("/proc/self/fdinfo lists the same fds as fd/", n1 == n2 && n1 > 10);
        struct stat st;
        stat("/proc/self/fdinfo", &st);
        printf("%-52s 0%o\n", "/proc/self/fdinfo mode", st.st_mode);
        char q[64];
        snprintf(q, sizeof q, "/proc/self/fdinfo/%d", f);
        stat(q, &st);
        printf("%-52s 0%o\n", "/proc/self/fdinfo/N mode", st.st_mode);
        int wfd = open(q, O_WRONLY);
        res("open fdinfo/N for writing", wfd < 0 ? -1 : 0);
    }
    int fds[] = {f, g, op, dn, p[0], p[1], sv[0], sv[1], dg, in, ev, tf, rt, ep, ino, pf, pn, pt, cf};
    for (unsigned i = 0; i < sizeof fds / sizeof fds[0]; i++)
        close(fds[i]);
    unlink(path);
}

// ---- PIDFD_GET_INFO ----

static void show(const char *name, int r, struct pidfd_info *i) {
    if (r < 0) {
        printf("%-52s %s\n", name, err(errno));
        return;
    }
    printf("%-52s mask %llx", name, (unsigned long long)i->mask);
    if (i->mask & INFO_PID)
        printf(" pid %s tgid %s ppid %s", i->pid == (uint32_t)getpid() ? "self" : i->pid ? "other" : "0", i->tgid == (uint32_t)getpid() ? "self" : i->tgid ? "other" : "0",
               i->ppid == (uint32_t)getppid() ? "parent" : i->ppid == (uint32_t)getpid() ? "self" : "other");
    // 資格情報は自分のものか (uid は動かす人による)
    if (i->mask & INFO_CREDS) {
        uid_t u = getuid();
        gid_t g = getgid();
        printf(" creds %s", i->ruid == u && i->euid == u && i->suid == u && i->fsuid == u && i->rgid == g && i->egid == g && i->sgid == g && i->fsgid == g ? "mine" : "other");
    }
    if (i->mask & INFO_EXIT)
        printf(" exit_code %#x", i->exit_code);
    if (i->mask & INFO_COREDUMP)
        printf(" coredump %x/%u/%u", i->coredump_mask, i->coredump_signal, i->coredump_code);
    if (i->mask & INFO_SUPPORTED)
        printf(" supported %llx", (unsigned long long)i->supported_mask);
    printf("\n");
}

static int info(int fd, uint64_t mask, struct pidfd_info *i) {
    memset(i, 0, sizeof *i);
    i->mask = mask;
    return ioctl(fd, PIDFD_GET_INFO, i);
}

static void *quick(void *a) {
    (void)a;
    return NULL;
}

static volatile int t_stop;
static void *waiter(void *a) {
    (void)a;
    while (!t_stop)
        usleep(1000);
    return NULL;
}

static void t_info(void) {
    struct pidfd_info i;
    int pf = pidfd_open(getpid(), 0);
    show("self, mask 0", info(pf, 0, &i), &i);
    show("self, EXIT (still running)", info(pf, INFO_EXIT, &i), &i);
    show("self, COREDUMP", info(pf, INFO_COREDUMP, &i), &i);
    show("self, SUPPORTED_MASK", info(pf, INFO_SUPPORTED, &i), &i);
    show("self, all bits and unknown ones", info(pf, ~0ULL, &i), &i);
    char big[128];
    memset(big, 0x5a, sizeof big);
    ((struct pidfd_info *)big)->mask = INFO_COREDUMP | INFO_SUPPORTED;
    int r = ioctl(pf, GET_INFO_SZ(64), big);
    show("size 64 (VER0), COREDUMP|SUPPORTED", r, (struct pidfd_info *)big);
    printf("%-52s %02x\n", "  byte 64 left alone", (unsigned char)big[64]);
    memset(big, 0x5a, sizeof big);
    ((struct pidfd_info *)big)->mask = INFO_COREDUMP | INFO_SUPPORTED;
    show("size 72 (VER1)", ioctl(pf, GET_INFO_SZ(72), big), (struct pidfd_info *)big);
    memset(big, 0x5a, sizeof big);
    ((struct pidfd_info *)big)->mask = INFO_COREDUMP | INFO_SUPPORTED;
    show("size 80 (VER2)", ioctl(pf, GET_INFO_SZ(80), big), (struct pidfd_info *)big);
    memset(big, 0x5a, sizeof big);
    ((struct pidfd_info *)big)->mask = INFO_COREDUMP | INFO_SUPPORTED;
    show("size 104, the rest 0x5a", ioctl(pf, GET_INFO_SZ(104), big), (struct pidfd_info *)big);
    printf("%-52s %02x %02x\n", "  bytes 88 and 103 afterwards", (unsigned char)big[88], (unsigned char)big[103]);
    res("size 56", ioctl(pf, GET_INFO_SZ(56), big));
    res("arg NULL", ioctl(pf, PIDFD_GET_INFO, NULL));
    res("a bad address", ioctl(pf, PIDFD_GET_INFO, (void *)8));
    res("another pidfs ioctl number (_IO(0xFF, 99))", ioctl(pf, _IO(0xFF, 99), 0));
    int dn = open("/dev/null", O_RDONLY);
    res("PIDFD_GET_INFO on /dev/null", ioctl(dn, PIDFD_GET_INFO, &i));
    int ev = eventfd(0, 0);
    res("PIDFD_GET_INFO on an eventfd", ioctl(ev, PIDFD_GET_INFO, &i));
    close(dn);
    close(ev);
    prctl(PR_SET_DUMPABLE, 0);
    show("self, COREDUMP after PR_SET_DUMPABLE 0", info(pf, INFO_COREDUMP, &i), &i);
    prctl(PR_SET_DUMPABLE, 1);
    close(pf);

    // 子: 動いている、ゾンビ、片づけたあと
    pid_t c = fork();
    if (c == 0) {
        pause();
        _exit(7);
    }
    int cf = pidfd_open(c, 0);
    show("child running, EXIT", info(cf, INFO_EXIT, &i), &i);
    yes("  pid is the child", i.pid == (uint32_t)c);
    kill(c, SIGKILL);
    siginfo_t si;
    waitid(P_PID, c, &si, WEXITED | WNOWAIT);
    show("child zombie (SIGKILL), EXIT", info(cf, INFO_EXIT, &i), &i);
    waitpid(c, NULL, 0);
    show("child reaped, mask 0", info(cf, 0, &i), &i);
    show("child reaped, EXIT", info(cf, INFO_EXIT, &i), &i);
    show("child reaped, EXIT|COREDUMP|SUPPORTED", info(cf, INFO_EXIT | INFO_COREDUMP | INFO_SUPPORTED, &i), &i);
    close(cf);

    c = fork();
    if (c == 0) {
        usleep(50000);
        _exit(7);
    }
    cf = pidfd_open(c, 0);
    waitpid(c, NULL, 0);
    show("child exit(7) reaped, EXIT", info(cf, INFO_EXIT, &i), &i);
    close(cf);

    // pidfd を持たないうちに終わって片づけられたもの (exit の印は残らない)
    c = fork();
    if (c == 0)
        _exit(3);
    struct pidfd_info j;
    int early = pidfd_open(c, 0);
    waitpid(c, NULL, 0);
    show("child exit(3) with a pidfd opened before reaping", info(early, INFO_EXIT, &j), &j);
    close(early);

    // スレッド: PIDFD_THREAD の pidfd
    pthread_t th;
    t_stop = 0;
    pthread_create(&th, NULL, waiter, NULL);
    usleep(20000);
    pid_t tid = 0;
    {
        DIR *d = opendir("/proc/self/task");
        struct dirent *de;
        while (d && (de = readdir(d)))
            if (atoi(de->d_name) > 0 && atoi(de->d_name) != getpid())
                tid = atoi(de->d_name);
        if (d)
            closedir(d);
    }
    int tf = pidfd_open(tid, PIDFD_THREAD);
    show("thread pidfd, mask 0", info(tf, 0, &i), &i);
    yes("  pid is the thread", i.pid == (uint32_t)tid);
    res("pidfd_open(thread) without PIDFD_THREAD", pidfd_open(tid, 0));
    t_stop = 1;
    pthread_join(th, NULL);
    usleep(20000);
    show("thread gone, mask 0", info(tf, 0, &i), &i);
    show("thread gone, EXIT", info(tf, INFO_EXIT, &i), &i);
    close(tf);
    pthread_create(&th, NULL, quick, NULL);
    pthread_join(th, NULL);
}

// ---- pidfd_getfd ----

static void t_getfd(const char *dir) {
    char path[300], b[4096];
    snprintf(path, sizeof path, "%s/getfd", dir);
    int w = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
    write(w, "abcdefgh", 8);
    close(w);
    int go[2];
    pipe(go);
    pid_t c = fork();
    if (c == 0) {
        int k = open(path, O_RDONLY);
        dup2(k, 30);
        close(k);
        write(go[1], "r", 1);
        pause();
        _exit(0);
    }
    char ch;
    read(go[0], &ch, 1);
    int pf = pidfd_open(c, 0);
    int n = pidfd_getfd(pf, 30, 0);
    ok("pidfd_getfd(child, 30, 0)", n);
    yes("  close-on-exec", n >= 0 && (fcntl(n, F_GETFD) & FD_CLOEXEC));
    char l1[300] = "", l2[64];
    snprintf(l2, sizeof l2, "/proc/self/fd/%d", n);
    ssize_t k = readlink(l2, l1, sizeof l1 - 1);
    if (k > 0)
        l1[k] = 0;
    yes("  the same file", strcmp(l1, path) == 0);
    char rb[4];
    read(n, rb, 3);
    fdinfo(c, 30, b, sizeof b);
    printf("%-52s %s\n", "  the child's pos after reading 3 here", field(b, "pos:"));
    res("flags 1", pidfd_getfd(pf, 30, 1));
    res("a target fd that is not open", pidfd_getfd(pf, 77, 0));
    res("a negative target fd", pidfd_getfd(pf, -1, 0));
    int dn = open("/dev/null", O_RDONLY);
    res("a /dev/null fd as the pidfd", pidfd_getfd(dn, 30, 0));
    res("pidfd -1", pidfd_getfd(-1, 30, 0));
    res("PIDFD_SELF_THREAD_GROUP, fd of /dev/null", pidfd_getfd(SELF_THREAD_GROUP, dn, 0) >= 0 ? 0 : -1);
    close(dn);
    int tp = pidfd_open(c, PIDFD_THREAD);
    int m = pidfd_getfd(tp, 30, 0);
    ok("a PIDFD_THREAD pidfd", m);
    kill(c, SIGKILL);
    siginfo_t si;
    waitid(P_PID, c, &si, WEXITED | WNOWAIT);
    res("the child is a zombie", pidfd_getfd(pf, 30, 0));
    waitpid(c, NULL, 0);
    res("the child is reaped", pidfd_getfd(pf, 30, 0));
    close(n);
    if (m >= 0)
        close(m);
    close(tp);
    close(pf);
    unlink(path);
}

// ---- pidfd_send_signal ----

static int rep[2];
static void on_sig(int s, siginfo_t *i, void *u) {
    (void)u;
    char b[96];
    int n = snprintf(b, sizeof b, "%d %d %d %d\n", s, i->si_code, i->si_pid, i->si_value.sival_int);
    write(rep[1], b, n);
}

static pid_t catcher(void) {
    pid_t c = fork();
    if (c == 0) {
        struct sigaction sa = {.sa_sigaction = on_sig, .sa_flags = SA_SIGINFO};
        sigaction(SIGUSR1, &sa, NULL);
        sigaction(SIGUSR2, &sa, NULL);
        setpgid(0, 0);
        write(rep[1], "ready\n", 6);
        for (;;)
            pause();
    }
    char b[8];
    read(rep[0], b, 6);
    return c;
}

// 子のハンドラが受けたもの
static void got(const char *name, pid_t me) {
    char b[96] = "";
    int fl = fcntl(rep[0], F_GETFL);
    fcntl(rep[0], F_SETFL, fl | O_NONBLOCK);
    for (int i = 0; i < 100; i++) {
        ssize_t k = read(rep[0], b, sizeof b - 1);
        if (k > 0) {
            b[k] = 0;
            break;
        }
        usleep(10000);
    }
    fcntl(rep[0], F_SETFL, fl);
    int s = 0, code = 0, pid = 0, val = 0;
    if (sscanf(b, "%d %d %d %d", &s, &code, &pid, &val) == 4)
        printf("%-52s sig %d code %d pid %s value %d\n", name, s, code, pid == me ? "parent" : pid == 0 ? "0" : "other", val);
    else
        printf("%-52s nothing\n", name);
}

static void t_send(void) {
    pipe(rep);
    pid_t c = catcher();
    int pf = pidfd_open(c, 0);
    res("flags 8", send_sig(pf, 0, NULL, 8));
    res("flags THREAD|THREAD_GROUP", send_sig(pf, 0, NULL, SIG_THREAD | SIG_THREAD_GROUP));
    ok("PIDFD_SELF_THREAD, sig 0", send_sig(SELF_THREAD, 0, NULL, 0));
    ok("PIDFD_SELF_THREAD_GROUP, sig 0", send_sig(SELF_THREAD_GROUP, 0, NULL, 0));
    res("pidfd -10002", send_sig(-10002, 0, NULL, 0));
    int dn = open("/dev/null", O_RDONLY);
    res("a /dev/null fd", send_sig(dn, 0, NULL, 0));
    close(dn);

    ok("SIGUSR1, no info", send_sig(pf, SIGUSR1, NULL, 0));
    got("  the child got", getpid());
    ok("SIGUSR1, PIDFD_SIGNAL_THREAD", send_sig(pf, SIGUSR1, NULL, SIG_THREAD));
    got("  the child got", getpid());
    ok("SIGUSR1, PIDFD_SIGNAL_THREAD_GROUP", send_sig(pf, SIGUSR1, NULL, SIG_THREAD_GROUP));
    got("  the child got", getpid());
    ok("SIGUSR2, PIDFD_SIGNAL_PROCESS_GROUP", send_sig(pf, SIGUSR2, NULL, SIG_PROCESS_GROUP));
    got("  the child got", getpid());

    siginfo_t si;
    memset(&si, 0, sizeof si);
    si.si_signo = SIGUSR1;
    si.si_code = SI_QUEUE;
    si.si_pid = getpid();
    si.si_uid = getuid();
    si.si_value.sival_int = 42;
    ok("info SI_QUEUE value 42", send_sig(pf, SIGUSR1, &si, 0));
    got("  the child got", getpid());
    si.si_signo = SIGUSR2;
    res("info si_signo is not sig", send_sig(pf, SIGUSR1, &si, 0));
    si.si_signo = SIGUSR1;
    si.si_code = SI_USER;
    res("info SI_USER to another process", send_sig(pf, SIGUSR1, &si, 0));
    si.si_code = SI_TKILL;
    res("info SI_TKILL to another process", send_sig(pf, SIGUSR1, &si, 0));
    si.si_code = 1;
    res("info si_code 1 (a kernel code)", send_sig(pf, SIGUSR1, &si, 0));
    res("info at a bad address", send_sig(pf, SIGUSR1, (siginfo_t *)8, 0));

    // 自分へは何でも (PROCESS_GROUP はだめ)
    signal(SIGUSR2, SIG_IGN);
    int me = pidfd_open(getpid(), 0);
    si.si_signo = SIGUSR2;
    si.si_code = SI_USER;
    ok("info SI_USER to self", send_sig(me, SIGUSR2, &si, 0));
    res("info SI_USER to self, PROCESS_GROUP", send_sig(me, SIGUSR2, &si, SIG_PROCESS_GROUP));
    ok("info SI_USER to self, THREAD", send_sig(me, SIGUSR2, &si, SIG_THREAD));
    ok("info SI_USER, PIDFD_SELF_THREAD", send_sig(SELF_THREAD, SIGUSR2, &si, 0));
    close(me);

    kill(c, SIGKILL);
    waitpid(c, NULL, 0);
    res("a reaped child", send_sig(pf, 0, NULL, 0));
    close(pf);
    close(rep[0]);
    close(rep[1]);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (geteuid() == 0 && (setgid(65534) < 0 || setuid(65534) < 0))
        return 1;
    char dir[64] = "/tmp/pidfdinfoXXXXXX";
    if (!mkdtemp(dir)) {
        snprintf(dir, sizeof dir, "./pidfdinfoXXXXXX");
        if (!mkdtemp(dir))
            return 1;
    }
    // fdinfo の数をくらべるので、ほかの fd を開けたままにしない
    for (int i = 3; i < 64; i++)
        close(i);
    t_fdinfo(dir);
    t_info();
    t_getfd(dir);
    t_send();
    rmdir(dir);
    return 0;
}
