// CPU 時間ののこり (Linux の clock_nanosleep、getrusage、times、/proc/PID/stat の utime と stime): clock_nanosleep の時計
// ごとの答え (タイマのない時計は EOPNOTSUPP、知らない時計は EINVAL、自分のスレッドの CPU の時計は EINVAL)、プロセスの
// CPU の時計で眠る (ほかのスレッドが CPU を使えば起きる)、ユーザーで回せば utime、カーネルで回せば (/dev/zero を読む)
// stime が増え、utime + stime は CLOCK_PROCESS_CPUTIME_ID と合う、子の時間 (RUSAGE_CHILDREN、tms_cutime)。時間の値は
// じかに出さず、決まりにあうかを出す
// 出すだけのテスト: test/ldiff.sh cputime
//   aios の中で: ./cputime      aarch64-linux-gnu-gcc -O2 -static -pthread -o cputime test/cputime.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/times.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "ok";
    case EINVAL: return "EINVAL";
    case EFAULT: return "EFAULT";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case EINTR: return "EINTR";
    default: snprintf(b, sizeof b, "e%d", e); return b;
    }
}

static long long ns(clockid_t c) {
    struct timespec t;
    if (syscall(SYS_clock_gettime, c, &t))
        return -1;
    return t.tv_sec * 1000000000LL + t.tv_nsec;
}

static volatile unsigned long sink;
static void spin_ms(int ms) {
    long long end = ns(CLOCK_MONOTONIC) + ms * 1000000LL;
    while (ns(CLOCK_MONOTONIC) < end)
        for (int i = 0; i < 10000; i++)
            sink += i;
}

// カーネルで ms ミリ秒 (壁の時計で) 回す: /dev/zero を大きく読む
static char zbuf[1 << 20];
static void kernel_ms(int ms) {
    int fd = open("/dev/zero", O_RDONLY);
    long long end = ns(CLOCK_MONOTONIC) + ms * 1000000LL;
    while (ns(CLOCK_MONOTONIC) < end)
        if (read(fd, zbuf, sizeof zbuf) < 0)
            break;
    close(fd);
}

static volatile int stop_thread;
static void *spinner(void *a) {
    (void)a;
    while (!stop_thread)
        sink++;
    return NULL;
}

static long long tv_ns(struct timeval t) {
    return t.tv_sec * 1000000000LL + t.tv_usec * 1000LL;
}

static void rusage_line(const char *tag, int who, long long *u, long long *s) {
    struct rusage r;
    getrusage(who, &r);
    *u = tv_ns(r.ru_utime);
    *s = tv_ns(r.ru_stime);
    (void)tag;
}

// /proc/self/stat の utime と stime (14 と 15 番め、tick)
static void stat_times(long long *u, long long *s) {
    char b[1024];
    int fd = open("/proc/self/stat", O_RDONLY);
    int n = read(fd, b, sizeof b - 1);
    close(fd);
    b[n > 0 ? n : 0] = 0;
    char *p = strrchr(b, ')');
    unsigned long long f[20] = {0};
    // ) のあとは 3 番め (state) から
    char st;
    sscanf(p + 2, "%c %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu", &st, &f[4], &f[5], &f[6], &f[7], &f[8],
           &f[9], &f[10], &f[11], &f[12], &f[13], &f[14], &f[15]);
    *u = f[14];
    *s = f[15];
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    // 1. clock_nanosleep の時計 (読めないところを渡す: 眠れる時計は EFAULT、タイマのないものは EOPNOTSUPP、知らないものは
    // EINVAL。ALARM の時計は RTC しだいなので見ない)
    printf("== clock_nanosleep clocks\n");
    for (int c = 0; c <= 16; c++) {
        if (c == CLOCK_REALTIME_ALARM || c == CLOCK_BOOTTIME_ALARM)
            continue;
        long r = syscall(SYS_clock_nanosleep, c, 0, (void *)8, NULL);
        printf("  clock %d: %s\n", c, r ? en(errno) : "ok");
    }
    struct timespec tiny = {0, 1000};
    // 自分のスレッドの CPU の時計 (pid 0 と自分の tid) は EINVAL
    clockid_t self_thread = ((~(clockid_t)0) << 3) | 6;
    long r = syscall(SYS_clock_nanosleep, self_thread, 0, &tiny, NULL);
    printf("  own thread cpu clock (pid 0): %s\n", r ? en(errno) : "ok");
    clockid_t own_tid = ((~(clockid_t)gettid()) << 3) | 6;
    r = syscall(SYS_clock_nanosleep, own_tid, 0, &tiny, NULL);
    printf("  own thread cpu clock (tid): %s\n", r ? en(errno) : "ok");
    struct timespec bad = {0, 1000000000};
    r = syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, &bad, NULL);
    printf("  monotonic nsec 1e9: %s\n", r ? en(errno) : "ok");
    struct timespec past = {0, 1};
    long long w0 = ns(CLOCK_MONOTONIC);
    r = syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, TIMER_ABSTIME, &past, NULL);
    printf("  monotonic abstime in the past: %s, at once %s\n", r ? en(errno) : "ok", ns(CLOCK_MONOTONIC) - w0 < 50000000 ? "yes" : "no");

    // 2. プロセスの CPU の時計で眠る (ほかのスレッドが回す)
    printf("== process cpu sleep\n");
    pthread_t th;
    pthread_create(&th, NULL, spinner, NULL);
    long long c0 = ns(CLOCK_PROCESS_CPUTIME_ID);
    w0 = ns(CLOCK_MONOTONIC);
    struct timespec fifty = {0, 50000000};
    r = syscall(SYS_clock_nanosleep, CLOCK_PROCESS_CPUTIME_ID, 0, &fifty, NULL);
    long long c1 = ns(CLOCK_PROCESS_CPUTIME_ID), w1 = ns(CLOCK_MONOTONIC);
    printf("  relative 50 ms: %s, cpu advanced 50 ms %s, woke within 2 s %s\n", r ? en(errno) : "ok", c1 - c0 >= 50000000 ? "yes" : "no",
           w1 - w0 < 2000000000LL ? "yes" : "no");
    long long target = ns(CLOCK_PROCESS_CPUTIME_ID) + 50000000;
    struct timespec abs = {target / 1000000000LL, target % 1000000000LL};
    r = syscall(SYS_clock_nanosleep, CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME, &abs, NULL);
    printf("  absolute +50 ms: %s, reached %s\n", r ? en(errno) : "ok", ns(CLOCK_PROCESS_CPUTIME_ID) >= target ? "yes" : "no");
    stop_thread = 1;
    pthread_join(th, NULL);

    // 3. utime と stime
    printf("== utime and stime\n");
    long long u0, s0, u1, s1, u2, s2;
    rusage_line("start", RUSAGE_SELF, &u0, &s0);
    spin_ms(200);
    rusage_line("user", RUSAGE_SELF, &u1, &s1);
    printf("  spin 200 ms in user: utime up %s, stime up little %s\n", u1 - u0 >= 120000000 ? "yes" : "no", s1 - s0 < 60000000 ? "yes" : "no");
    kernel_ms(200);
    rusage_line("kernel", RUSAGE_SELF, &u2, &s2);
    printf("  read /dev/zero 200 ms: stime up %s, utime up little %s\n", s2 - s1 >= 100000000 ? "yes" : "no", u2 - u1 < 100000000 ? "yes" : "no");
    long long cpu = ns(CLOCK_PROCESS_CPUTIME_ID);
    long long sum = u2 + s2;
    printf("  utime + stime near the cpu clock: %s\n", sum <= cpu + 20000000 && sum >= cpu - 40000000 ? "yes" : "no");
    long long tu, ts;
    rusage_line("thread", RUSAGE_THREAD, &tu, &ts);
    printf("  RUSAGE_THREAD: utime %s, stime %s\n", tu >= 120000000 ? "counted" : "missing", ts >= 100000000 ? "counted" : "missing");
    struct tms t;
    clock_t ticks = times(&t);
    long hz = sysconf(_SC_CLK_TCK);
    printf("  times: utime %s, stime %s, returns ticks %s\n", t.tms_utime * (1000000000LL / hz) >= 120000000 ? "counted" : "missing",
           t.tms_stime * (1000000000LL / hz) >= 100000000 ? "counted" : "missing", ticks != (clock_t)-1 ? "yes" : "no");
    long long su, ss;
    stat_times(&su, &ss);
    printf("  /proc/self/stat utime and stime: %s\n", su * (1000000000LL / hz) >= 120000000 && ss * (1000000000LL / hz) >= 100000000 ? "counted" : "missing");

    // 4. 子の時間 (回収したあと)
    printf("== children\n");
    pid_t c = fork();
    if (c == 0) {
        spin_ms(100);
        kernel_ms(100);
        _exit(0);
    }
    struct rusage before;
    getrusage(RUSAGE_CHILDREN, &before);
    int st;
    waitpid(c, &st, 0);
    struct rusage cr;
    getrusage(RUSAGE_CHILDREN, &cr);
    printf("  before reaping: %s\n", tv_ns(before.ru_utime) + tv_ns(before.ru_stime) == 0 ? "zero" : "not zero");
    printf("  RUSAGE_CHILDREN: utime %s, stime %s\n", tv_ns(cr.ru_utime) >= 50000000 ? "counted" : "missing",
           tv_ns(cr.ru_stime) >= 40000000 ? "counted" : "missing");
    times(&t);
    printf("  times: cutime %s, cstime %s\n", t.tms_cutime * (1000000000LL / hz) >= 50000000 ? "counted" : "missing",
           t.tms_cstime * (1000000000LL / hz) >= 40000000 ? "counted" : "missing");
    return 0;
}
