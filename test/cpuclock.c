// CPU の時計 (Linux の kernel/time/posix-cpu-timers.c と posix-timers.c): clock_gettime / clock_getres の時計ごとの答え
// (知らない時計は EINVAL)、CLOCK_PROCESS_CPUTIME_ID と CLOCK_THREAD_CPUTIME_ID が CPU を使ったぶんだけ進むか (眠っている
// あいだは進まない、ほかのスレッドが使えばプロセスの時計だけ進む)、clock_getcpuclockid と pthread_getcpuclockid の時計
// (自分、子、いないもの)、timer_create の CPU の時計 (CPU を使ったら鳴る、眠っていれば鳴らない)。時間の値はじかに出さず、
// 決まりにあうかを出す。分解能は 1 ns か tick (10 ms まで) か (手本の Linux は高精度タイマーなしで tick)。ALARM の時計
// (8、9) は RTC があるかしだいなので見ない
// 出すだけのテスト: test/ldiff.sh cpuclock
//   aios の中で: ./cpuclock      aarch64-linux-gnu-gcc -O2 -static -pthread -o cpuclock test/cpuclock.c
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case ESRCH: return "ESRCH";
    case EPERM: return "EPERM";
    case EFAULT: return "EFAULT";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    default: snprintf(b, sizeof b, "e%d", e); return b;
    }
}

static long long ns(clockid_t c) {
    struct timespec t;
    if (syscall(SYS_clock_gettime, c, &t))
        return -1;
    return t.tv_sec * 1000000000LL + t.tv_nsec;
}

// CPU を ms ミリ秒 (壁の時計で) 使う
static volatile unsigned long sink;
static void spin_ms(int ms) {
    long long end = ns(CLOCK_MONOTONIC) + ms * 1000000LL;
    while (ns(CLOCK_MONOTONIC) < end)
        for (int i = 0; i < 10000; i++)
            sink += i;
}

static volatile int stop_thread;
static void *spinner(void *a) {
    (void)a;
    while (!stop_thread)
        sink++;
    return NULL;
}

static volatile int fired;
static void on_sig(int s) {
    (void)s;
    fired++;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    // 1. 時計ごとの clock_gettime と clock_getres (syscall で: vDSO を通らない)
    printf("== clocks\n");
    for (int c = 0; c <= 16; c++) {
        if (c == CLOCK_REALTIME_ALARM || c == CLOCK_BOOTTIME_ALARM)
            continue;
        struct timespec t, r;
        long g = syscall(SYS_clock_gettime, c, &t);
        int ge = g ? errno : 0;
        long s = syscall(SYS_clock_getres, c, &r);
        int se = s ? errno : 0;
        char res[48] = "-";
        long long rv = r.tv_sec * 1000000000LL + r.tv_nsec;
        if (!s)
            snprintf(res, sizeof res, "%s", rv == 1 ? "1ns" : rv > 0 && rv <= 10000000 ? "tick" : "other");
        printf("  clock %d: gettime %s getres %s res %s\n", c, en(ge), en(se), res);
    }
    long g = syscall(SYS_clock_getres, CLOCK_MONOTONIC, NULL);
    printf("  getres NULL: %s\n", g ? en(errno) : "ok");
    g = syscall(SYS_clock_gettime, CLOCK_MONOTONIC, (void *)8);
    printf("  gettime bad pointer: %s\n", g ? en(errno) : "ok");

    // 2. CPU を使ったぶんだけ進む
    printf("== cputime\n");
    long long p0 = ns(CLOCK_PROCESS_CPUTIME_ID), t0 = ns(CLOCK_THREAD_CPUTIME_ID), w0 = ns(CLOCK_MONOTONIC);
    spin_ms(100);
    long long p1 = ns(CLOCK_PROCESS_CPUTIME_ID), t1 = ns(CLOCK_THREAD_CPUTIME_ID), w1 = ns(CLOCK_MONOTONIC);
    printf("  spin 100 ms: process %s, thread %s, not more than wall %s\n", p1 - p0 >= 60000000 ? "advanced" : "stuck",
           t1 - t0 >= 60000000 ? "advanced" : "stuck", (t1 - t0) <= (w1 - w0) + 20000000 ? "yes" : "no");
    const struct timespec sl = {0, 200000000};
    struct timespec scratch;
    nanosleep(&sl, NULL);
    long long p2 = ns(CLOCK_PROCESS_CPUTIME_ID), t2 = ns(CLOCK_THREAD_CPUTIME_ID);
    printf("  sleep 200 ms: process %s, thread %s\n", p2 - p1 < 30000000 ? "still" : "advanced", t2 - t1 < 30000000 ? "still" : "advanced");
    // 2 つの読みがいつも前へ (同じか大きい)
    int mono = 1;
    long long prev = ns(CLOCK_THREAD_CPUTIME_ID);
    for (int i = 0; i < 1000; i++) {
        long long v = ns(CLOCK_THREAD_CPUTIME_ID);
        if (v < prev)
            mono = 0;
        prev = v;
    }
    printf("  thread clock never goes back: %s\n", mono ? "yes" : "no");

    // ほかのスレッドが使うと、プロセスの時計だけ進む
    pthread_t th;
    long long pa = ns(CLOCK_PROCESS_CPUTIME_ID), ta = ns(CLOCK_THREAD_CPUTIME_ID);
    pthread_create(&th, NULL, spinner, NULL);
    nanosleep(&sl, NULL);
    clockid_t tc;
    int e = pthread_getcpuclockid(th, &tc);
    long long other = e ? -1 : ns(tc);
    stop_thread = 1;
    pthread_join(th, NULL);
    long long pb = ns(CLOCK_PROCESS_CPUTIME_ID), tb = ns(CLOCK_THREAD_CPUTIME_ID);
    printf("  other thread spins 200 ms: process %s, this thread %s\n", pb - pa >= 100000000 ? "advanced" : "stuck",
           tb - ta < 30000000 ? "still" : "advanced");
    printf("  pthread_getcpuclockid: %s, its clock %s, id is thread-sched %s\n", en(e),
           other >= 100000000 ? "advanced" : "stuck", !e && (tc & 7) == 6 ? "yes" : "no");
    // 終わったスレッドの時計は EINVAL
    long r = e ? -1 : syscall(SYS_clock_gettime, tc, &scratch);
    printf("  joined thread clock: %s\n", r ? en(errno) : "ok");

    // 3. clock_getcpuclockid: 自分、子、いないもの
    printf("== getcpuclockid\n");
    clockid_t mc;
    e = clock_getcpuclockid(getpid(), &mc);
    long long via = e ? -1 : ns(mc), direct = ns(CLOCK_PROCESS_CPUTIME_ID);
    printf("  self: %s, id is process-sched %s, same as PROCESS_CPUTIME %s\n", en(e), !e && (mc & 7) == 2 ? "yes" : "no",
           via >= 0 && direct - via < 50000000 && direct >= via ? "yes" : "no");
    int pp[2];
    if (pipe(pp))
        return 1;
    pid_t c = fork();
    if (c == 0) {
        spin_ms(100);
        char ch;
        close(pp[1]);
        if (read(pp[0], &ch, 1) < 0)
            _exit(1);
        _exit(0);
    }
    close(pp[0]);
    nanosleep(&sl, NULL);
    clockid_t cc;
    e = clock_getcpuclockid(c, &cc);
    long long cv = e ? -1 : ns(cc);
    printf("  child: %s, clock %s\n", en(e), cv >= 60000000 ? "advanced" : cv < 0 ? "error" : "stuck");
    // ほかのプロセスのスレッドの時計は EINVAL (同じスレッドグループだけ)
    clockid_t ctc = ((~(clockid_t)c) << 3) | 6;
    r = syscall(SYS_clock_gettime, ctc, &scratch);
    printf("  child as thread clock: %s\n", r ? en(errno) : "ok");
    close(pp[1]);
    int st;
    waitpid(c, &st, 0);
    r = syscall(SYS_clock_gettime, cc, &scratch);
    printf("  reaped child clock: %s\n", r ? en(errno) : "ok");
    e = clock_getcpuclockid(999999, &cc);
    printf("  no such pid: %s\n", en(e));
    // CPUCLOCK_WHICH が 3 (CPUCLOCK_MAX) は EINVAL
    r = syscall(SYS_clock_gettime, ((~(clockid_t)0) << 3) | 3, &scratch);
    printf("  which 3: %s\n", r ? en(errno) : "ok");
    r = syscall(SYS_clock_getres, mc, &scratch);
    printf("  getres self clock: %s %lld\n", r ? en(errno) : "ok", r ? -1LL : scratch.tv_sec * 1000000000LL + scratch.tv_nsec);
    // PROF (0) と VIRT (1) の時計も読める
    r = syscall(SYS_clock_gettime, ((~(clockid_t)0) << 3) | 0, &scratch);
    printf("  prof clock of self (pid 0): %s\n", r ? en(errno) : "ok");
    r = syscall(SYS_clock_gettime, ((~(clockid_t)0) << 3) | 1, &scratch);
    printf("  virt clock of self (pid 0): %s\n", r ? en(errno) : "ok");

    // 4. timer_create の CPU の時計
    printf("== timer\n");
    signal(SIGALRM, on_sig);
    timer_t tid;
    e = timer_create(CLOCK_PROCESS_CPUTIME_ID, NULL, &tid) ? errno : 0;
    printf("  timer_create PROCESS_CPUTIME: %s\n", en(e));
    if (!e) {
        struct itimerspec its = {{0, 0}, {0, 50000000}};
        timer_settime(tid, 0, &its, NULL);
        fired = 0;
        nanosleep(&sl, NULL);
        printf("  sleep 200 ms: fired %d\n", fired);
        long long start = ns(CLOCK_MONOTONIC);
        while (!fired && ns(CLOCK_MONOTONIC) - start < 2000000000LL)
            sink++;
        printf("  spin: fired %d\n", fired);
        timer_delete(tid);
    }
    e = timer_create(CLOCK_THREAD_CPUTIME_ID, NULL, &tid) ? errno : 0;
    printf("  timer_create THREAD_CPUTIME: %s\n", en(e));
    if (!e) {
        struct itimerspec its = {{0, 20000000}, {0, 20000000}};
        timer_settime(tid, 0, &its, NULL);
        fired = 0;
        spin_ms(150);
        struct itimerspec left;
        timer_gettime(tid, &left);
        printf("  periodic 20 ms over 150 ms of spin: fired at least 3 %s, interval kept %s\n", fired >= 3 ? "yes" : "no",
               left.it_interval.tv_nsec == 20000000 ? "yes" : "no");
        timer_delete(tid);
    }
    // タイマのない時計 (MONOTONIC_RAW、COARSE) と、知らない時計 (ALARM は RTC しだいなので見ない)
    int ks[] = {4, 5, 6, 10, 12, 99};
    for (unsigned k = 0; k < sizeof ks / sizeof *ks; k++) {
        e = syscall(SYS_timer_create, ks[k], NULL, &tid) ? errno : 0;
        printf("  timer_create clock %d: %s\n", ks[k], en(e));
        if (!e)
            timer_delete(tid);
    }
    return 0;
}
