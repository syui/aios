// 待つものが、たのんだ時間より早く終わらないこと (Linux と同じ: hrtimer はかならず期限のあと)。aios は tick (10 ms)
// の境めで起きるので、前は tick の途中から待つと 1 tick 近く早く終わった (timer::ticks_after)。
// 13 ms (tick の倍数でない) を、tick の中のいろいろなところから 12 回ずつ: nanosleep、clock_nanosleep (相対と
// TIMER_ABSTIME)、ppoll、pselect6、epoll_wait、epoll_pwait2、futex (相対と WAIT_BITSET の時刻)、sigtimedwait、
// SO_RCVTIMEO (AF_UNIX と TCP)、timer_settime、setitimer。0 の時間の ppoll と epoll_wait は待たずに返ること。終わりに sleeps: N checks, M failed
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#define WANT_MS 13
static int checks, fails;

static long now_us(clockid_t c) {
    struct timespec t;
    clock_gettime(c, &t);
    return t.tv_sec * 1000000L + t.tv_nsec / 1000;
}

static void check(const char *name, int ok, long got_us) {
    checks++;
    if (!ok) {
        fails++;
        printf("FAIL %s (%ld us)\n", name, got_us);
    }
}

static struct timespec rel_ts(void) { return (struct timespec){0, WANT_MS * 1000000L}; }

static struct timespec abs_ts(clockid_t c) {
    struct timespec t;
    clock_gettime(c, &t);
    t.tv_nsec += WANT_MS * 1000000L;
    if (t.tv_nsec >= 1000000000L) {
        t.tv_sec++;
        t.tv_nsec -= 1000000000L;
    }
    return t;
}

static volatile sig_atomic_t fired;
static void on_sig(int s) { (void)s; fired = 1; }

// 1 回: f を動かして、たった時間が 13 ms より短くないか。start の前に、tick の中の場所をずらす (i ms の 0.7 倍)
#define ROUND(name, ...)                                                          \
    for (int i = 0; i < 12; i++) {                                                \
        usleep(700 * i % 9000 + 300);                                             \
        long t0 = now_us(CLOCK_MONOTONIC);                                        \
        __VA_ARGS__;                                                              \
        long dt = now_us(CLOCK_MONOTONIC) - t0;                                   \
        check(name, dt >= WANT_MS * 1000, dt);                                    \
    }

int main(void) {
    int sv[2], ep = epoll_create1(0), fx = 0;
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(ep, EPOLL_CTL_ADD, sv[0], &ev);

    ROUND("nanosleep", { struct timespec r = rel_ts(); nanosleep(&r, NULL); });
    ROUND("clock_nanosleep", { struct timespec r = rel_ts(); clock_nanosleep(CLOCK_MONOTONIC, 0, &r, NULL); });
    // TIMER_ABSTIME: 期限は t0 より前に読むので、t0 からは 13 ms より少し短くてよい。期限そのものと比べる
    for (int i = 0; i < 12; i++) {
        usleep(700 * i % 9000 + 300);
        struct timespec a = abs_ts(CLOCK_MONOTONIC);
        clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &a, NULL);
        long late = now_us(CLOCK_MONOTONIC) - (a.tv_sec * 1000000L + a.tv_nsec / 1000);
        check("clock_nanosleep abstime", late >= 0, late);
    }
    ROUND("ppoll", { struct pollfd p = {sv[0], POLLIN, 0}; struct timespec r = rel_ts(); ppoll(&p, 1, &r, NULL); });
    ROUND("pselect6", { fd_set s; FD_ZERO(&s); FD_SET(sv[0], &s); struct timespec r = rel_ts(); pselect(sv[0] + 1, &s, NULL, NULL, &r, NULL); });
    ROUND("epoll_wait", { struct epoll_event e; epoll_wait(ep, &e, 1, WANT_MS); });
#ifdef SYS_epoll_pwait2
    // ないもの (qemu-aarch64 のユーザーモードなど: ENOSYS) は飛ばす
    struct timespec z0 = {0, 0};
    struct epoll_event e0;
    if (syscall(SYS_epoll_pwait2, ep, &e0, 1, &z0, NULL, 8) >= 0 || errno != ENOSYS)
        ROUND("epoll_pwait2", { struct epoll_event e; struct timespec r = rel_ts(); syscall(SYS_epoll_pwait2, ep, &e, 1, &r, NULL, 8); });
#endif
    ROUND("futex wait", { struct timespec r = rel_ts(); syscall(SYS_futex, &fx, FUTEX_WAIT_PRIVATE, 0, &r, NULL, 0); });
    for (int i = 0; i < 12; i++) {
        usleep(700 * i % 9000 + 300);
        struct timespec a = abs_ts(CLOCK_MONOTONIC);
        syscall(SYS_futex, &fx, FUTEX_WAIT_BITSET_PRIVATE, 0, &a, NULL, FUTEX_BITSET_MATCH_ANY);
        long late = now_us(CLOCK_MONOTONIC) - (a.tv_sec * 1000000L + a.tv_nsec / 1000);
        check("futex wait_bitset abstime", late >= 0, late);
    }
    sigset_t usr;
    sigemptyset(&usr);
    sigaddset(&usr, SIGUSR1);
    sigprocmask(SIG_BLOCK, &usr, NULL);
    ROUND("sigtimedwait", { struct timespec r = rel_ts(); sigtimedwait(&usr, NULL, &r); });
    struct timeval tv = {0, WANT_MS * 1000};
    setsockopt(sv[0], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    ROUND("SO_RCVTIMEO unix", { char c; recv(sv[0], &c, 1, 0); });
    int l = socket(AF_INET, SOCK_STREAM, 0), c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 1);
    getsockname(l, (struct sockaddr *)&a, &al);
    connect(c, (struct sockaddr *)&a, sizeof a);
    setsockopt(c, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    ROUND("SO_RCVTIMEO tcp", { char ch; recv(c, &ch, 1, 0); });
    signal(SIGALRM, on_sig);
    timer_t tm;
    struct sigevent se = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGALRM};
    timer_create(CLOCK_MONOTONIC, &se, &tm);
    ROUND("timer_settime", {
        fired = 0;
        struct itimerspec it = {.it_value = rel_ts()};
        timer_settime(tm, 0, &it, NULL);
        while (!fired)
            pause();
    });
    ROUND("setitimer", {
        fired = 0;
        struct itimerval it = {.it_value = {0, WANT_MS * 1000}};
        setitimer(ITIMER_REAL, &it, NULL);
        while (!fired)
            pause();
    });
    // 時間 0 の ppoll と epoll_wait は待たない (tick の境めまでも待たない: 前の aios は 1 tick 待つことがあった)。
    // 1 回ずつ測って、5 ms をこえたものが 40 回のうち 2 回まで (QEMU の TCG では、ときどき遅れる)
    int slow = 0;
    long worst = 0;
    for (int i = 0; i < 20; i++) {
        usleep(700 * i % 9000 + 300);
        struct timespec z = {0, 0};
        struct pollfd p = {sv[0], POLLIN, 0};
        struct epoll_event e;
        long t1 = now_us(CLOCK_MONOTONIC);
        ppoll(&p, 1, &z, NULL);
        long t2 = now_us(CLOCK_MONOTONIC);
        epoll_wait(ep, &e, 1, 0);
        long t3 = now_us(CLOCK_MONOTONIC);
        slow += (t2 - t1 >= 5000) + (t3 - t2 >= 5000);
        worst = t2 - t1 > worst ? t2 - t1 : worst;
        worst = t3 - t2 > worst ? t3 - t2 : worst;
    }
    check("zero timeouts do not wait", slow <= 2, worst);
    printf("sleeps: %d checks, %d failed\n", checks, fails);
    return fails != 0;
}
