// futex (kernel/src/futex.rs): ロボット (test/sysdiff.c のスレッドと futex の op) の見ないところをじかに確かめる:
// 来た順に起こす、プロセスをまたぐ (MAP_SHARED) 共有の futex、PRIVATE と共有の key は会わない、時間切れ (相対と
// 時刻)、シグナル (SA_RESTART ならやり直し、なければ EINTR、LOCK_PI はいつもやり直し)、PI のロックのゆずりあい、
// REQUEUE で移ったものを起こす、futex_waitv の起こされた番号、pthread_join (スレッドの終わりの共有の起こし)。
// aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o futex test/futex.c; ./futex。Linux でも同じ答え
// (test/linux-vm.sh exec latest ...)。しくじった数を終了コードで
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
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

static long fx(uint32_t *u, int op, uint32_t val, const void *t, uint32_t *u2, uint32_t val3) {
    long r = syscall(SYS_futex, u, op, val, t, u2, val3);
    return r < 0 ? -errno : r;
}

static void msleep(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR)
        ;
}

static long now_ms(clockid_t c) {
    struct timespec ts;
    clock_gettime(c, &ts);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// スレッドの状態 (/proc/self/task/TID/stat)
static char tstate(int tid) {
    char p[64], b[512];
    snprintf(p, sizeof p, "/proc/self/task/%d/stat", tid);
    int fd = open(p, O_RDONLY);
    if (fd < 0)
        return 'X';
    ssize_t n = read(fd, b, sizeof b - 1);
    close(fd);
    if (n <= 0)
        return 'X';
    b[n] = 0;
    char *q = strrchr(b, ')');
    return q && q[1] == ' ' ? q[2] : '?';
}

// 待つスレッド: op で w を待ち、答えと終わった順を書く
struct waiter {
    uint32_t *w;
    int op;
    uint32_t val, bitset;
    volatile int tid, done, order, got;
    long r;
};

static volatile int finished, taken;

static void *waiter_main(void *arg) {
    struct waiter *x = arg;
    x->tid = (int)syscall(SYS_gettid);
    if ((x->op & FUTEX_CMD_MASK) == FUTEX_LOCK_PI) {
        x->r = fx(x->w, x->op, 0, NULL, NULL, 0);
        // 取った順 (ゆずった相手は、ゆずったものより先に走ることがあるので、終わった順ではなく)
        x->got = __atomic_add_fetch(&taken, 1, __ATOMIC_SEQ_CST);
        if (x->r == 0)
            fx(x->w, FUTEX_UNLOCK_PI | (x->op & FUTEX_PRIVATE_FLAG), 0, NULL, NULL, 0);
    } else
        x->r = fx(x->w, x->op, x->val, NULL, NULL, x->bitset);
    x->order = __atomic_add_fetch(&finished, 1, __ATOMIC_SEQ_CST);
    __atomic_store_n(&x->done, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

// スレッドを作り、眠るまで (2 秒まで) 待つ
static pthread_t start(struct waiter *x) {
    pthread_t t;
    x->done = 0;
    x->tid = 0;
    pthread_create(&t, NULL, waiter_main, x);
    for (int i = 0; i < 400 && (!x->tid || tstate(x->tid) != 'S') && !x->done; i++)
        msleep(5);
    msleep(5);
    return t;
}

static int waitdone(struct waiter *x) {
    for (int i = 0; i < 400 && !x->done; i++)
        msleep(5);
    return x->done;
}

static uint32_t w0, w1, pi;

// 来た順: 3 つが待ち、1 つずつ起こすと来た順に起きる。数はそのとおり、0 でも 1 つ
static void fifo(void) {
    struct waiter x[3];
    pthread_t t[3];
    w0 = 0;
    finished = 0;
    for (int i = 0; i < 3; i++) {
        x[i] = (struct waiter){&w0, FUTEX_WAIT_PRIVATE, 0, 0};
        t[i] = start(&x[i]);
    }
    check("wake 1 of 3", fx(&w0, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), 1);
    waitdone(&x[0]);
    check("the first one woke", x[0].done && !x[1].done && !x[2].done, 1);
    check("wake 0 wakes 1", fx(&w0, FUTEX_WAKE_PRIVATE, 0, NULL, NULL, 0), 1);
    waitdone(&x[1]);
    check("then the second", x[1].done && !x[2].done, 1);
    check("wake INT_MAX", fx(&w0, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL, 0), 1);
    for (int i = 0; i < 3; i++) {
        pthread_join(t[i], NULL);
        check("woken one returns 0", x[i].r, 0);
    }
    check("order", x[0].order * 100 + x[1].order * 10 + x[2].order, 123);
    check("nobody left", fx(&w0, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL, 0), 0);
}

// PRIVATE と共有の key は、同じアドレスでも会わない。bitset の重ならないものは起きない
static void keys(void) {
    struct waiter x = {&w0, FUTEX_WAIT_PRIVATE, 0, 0}, y = {&w1, FUTEX_WAIT_BITSET_PRIVATE, 0, 2};
    w0 = w1 = 0;
    pthread_t t = start(&x), u = start(&y);
    check("shared wake does not meet a private waiter", fx(&w0, FUTEX_WAKE, 1, NULL, NULL, 0), 0);
    check("private wake", fx(&w0, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), 1);
    check("bitset 1 misses bitset 2", fx(&w1, FUTEX_WAKE_BITSET_PRIVATE, 1, NULL, NULL, 1), 0);
    check("bitset 3 meets bitset 2", fx(&w1, FUTEX_WAKE_BITSET_PRIVATE, 1, NULL, NULL, 3), 1);
    pthread_join(t, NULL);
    pthread_join(u, NULL);
}

// MAP_SHARED のページの共有の futex は、別のプロセスからも (別のアドレスに写していても) 会う
static void cross_process(void) {
    uint32_t *a = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    uint32_t *b = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    a[0] = 0;
    pid_t pid = fork();
    if (pid == 0) {
        long r = fx(&a[0], FUTEX_WAIT, 0, NULL, NULL, 0);
        _exit(r == 0 ? 0 : 1);
    }
    for (int i = 0; i < 400; i++) {
        char p[64], s[512];
        snprintf(p, sizeof p, "/proc/%d/stat", pid);
        int fd = open(p, O_RDONLY);
        ssize_t n = fd >= 0 ? read(fd, s, sizeof s - 1) : -1;
        if (fd >= 0)
            close(fd);
        if (n > 0) {
            s[n] = 0;
            char *q = strrchr(s, ')');
            if (q && q[2] == 'S')
                break;
        }
        msleep(5);
    }
    msleep(10);
    check("other mapping does not meet", fx(&b[0], FUTEX_WAKE, 1, NULL, NULL, 0), 0);
    check("private wake does not cross processes", fx(&a[0], FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), 0);
    check("shared wake across fork", fx(&a[0], FUTEX_WAKE, 1, NULL, NULL, 0), 1);
    int st = -1;
    waitpid(pid, &st, 0);
    check("the child woke with 0", st, 0);
    munmap(a, 4096);
    munmap(b, 4096);
}

// 時間切れ: FUTEX_WAIT は相対、WAIT_BITSET は CLOCK_MONOTONIC (か CLOCK_REALTIME) の時刻
static void timeouts(void) {
    w0 = 0;
    struct timespec rel = {0, 50 * 1000000};
    long t0 = now_ms(CLOCK_MONOTONIC);
    check("relative timeout", fx(&w0, FUTEX_WAIT_PRIVATE, 0, &rel, NULL, 0), -ETIMEDOUT);
    check("waited at least 50 ms", now_ms(CLOCK_MONOTONIC) - t0 >= 49, 1);
    struct timespec abs;
    clock_gettime(CLOCK_MONOTONIC, &abs);
    abs.tv_nsec += 50 * 1000000;
    if (abs.tv_nsec >= 1000000000) {
        abs.tv_sec++;
        abs.tv_nsec -= 1000000000;
    }
    t0 = now_ms(CLOCK_MONOTONIC);
    check("absolute monotonic", fx(&w0, FUTEX_WAIT_BITSET_PRIVATE, 0, &abs, NULL, FUTEX_BITSET_MATCH_ANY), -ETIMEDOUT);
    check("waited until then", now_ms(CLOCK_MONOTONIC) - t0 >= 49, 1);
    clock_gettime(CLOCK_REALTIME, &abs);
    abs.tv_sec -= 1;
    check("absolute realtime in the past", fx(&w0, FUTEX_WAIT_BITSET_PRIVATE | FUTEX_CLOCK_REALTIME, 0, &abs, NULL, FUTEX_BITSET_MATCH_ANY), -ETIMEDOUT);
    struct timespec bad = {0, 1000000000};
    check("bad timespec", fx(&w0, FUTEX_WAIT_PRIVATE, 0, &bad, NULL, 0), -EINVAL);
    check("the value differs", fx(&w0, FUTEX_WAIT_PRIVATE, 1, &rel, NULL, 0), -EAGAIN);
}

// シグナル: SA_RESTART のハンドラなら待ちなおし (起こされて 0)、なければ EINTR
static volatile int handled;

static void on_usr1(int s) {
    (void)s;
    handled++;
}

static void signals(void) {
    for (int restart = 0; restart < 2; restart++) {
        struct sigaction sa = {0};
        sa.sa_handler = on_usr1;
        sa.sa_flags = restart ? SA_RESTART : 0;
        sigaction(SIGUSR1, &sa, NULL);
        handled = 0;
        w0 = 0;
        struct waiter x = {&w0, FUTEX_WAIT_PRIVATE, 0, 0};
        pthread_t t = start(&x);
        syscall(SYS_tgkill, getpid(), x.tid, SIGUSR1);
        for (int i = 0; i < 200 && !handled; i++)
            msleep(5);
        msleep(20);
        check(restart ? "SA_RESTART: the handler ran" : "no SA_RESTART: the handler ran", handled, 1);
        if (restart) {
            check("SA_RESTART: still waiting", x.done, 0);
            check("SA_RESTART: wake", fx(&w0, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), 1);
            pthread_join(t, NULL);
            check("SA_RESTART: woken with 0", x.r, 0);
        } else {
            pthread_join(t, NULL);
            check("no SA_RESTART: EINTR", x.r, -EINTR);
        }
    }
    // LOCK_PI はハンドラが動いても待ちなおす
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    pi = 0;
    handled = 0;
    check("lock pi", fx(&pi, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0), 0);
    struct waiter x = {&pi, FUTEX_LOCK_PI_PRIVATE, 0, 0};
    pthread_t t = start(&x);
    syscall(SYS_tgkill, getpid(), x.tid, SIGUSR1);
    for (int i = 0; i < 200 && !handled; i++)
        msleep(5);
    msleep(20);
    check("LOCK_PI after a handler: still waiting", x.done, 0);
    check("unlock pi hands over", fx(&pi, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0), 0);
    pthread_join(t, NULL);
    check("the waiter got it", x.r, 0);
    check("and gave it back", pi, 0);
}

// PI: 持ち主の番号と WAITERS、来た順にゆずる、自分のものは EDEADLK、持ち主でなければ EPERM
static void pi_lock(void) {
    uint32_t me = (uint32_t)syscall(SYS_gettid);
    pi = 0;
    check("lock", fx(&pi, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0), 0);
    check("the word is my tid", pi, me);
    check("again: EDEADLK", fx(&pi, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0), -EDEADLK);
    struct waiter x[2];
    pthread_t t[2];
    taken = 0;
    for (int i = 0; i < 2; i++) {
        x[i] = (struct waiter){&pi, FUTEX_LOCK_PI_PRIVATE, 0, 0};
        t[i] = start(&x[i]);
    }
    check("WAITERS is set", pi, me | FUTEX_WAITERS);
    check("trylock by the owner: EDEADLK", fx(&pi, FUTEX_TRYLOCK_PI_PRIVATE, 0, NULL, NULL, 0), -EDEADLK);
    check("unlock", fx(&pi, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0), 0);
    for (int i = 0; i < 2; i++)
        pthread_join(t[i], NULL);
    check("both got it", x[0].r * 10 + x[1].r, 0);
    check("taken in order", x[0].got * 10 + x[1].got, 12);
    check("free again", pi, 0);
    check("unlock what I do not own: EPERM", fx(&pi, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0), -EPERM);
}

// REQUEUE: 移ったものは移った先で起きる。答えは起こした数と移した数
static void requeue(void) {
    struct waiter x[3];
    pthread_t t[3];
    w0 = 7;
    w1 = 0;
    finished = 0;
    for (int i = 0; i < 3; i++) {
        x[i] = (struct waiter){&w0, FUTEX_WAIT_PRIVATE, 7, 0};
        t[i] = start(&x[i]);
    }
    check("cmp_requeue with another value: EAGAIN", fx(&w0, FUTEX_CMP_REQUEUE_PRIVATE, 1, (void *)1L, &w1, 6), -EAGAIN);
    check("cmp_requeue: 1 woken + 2 moved", fx(&w0, FUTEX_CMP_REQUEUE_PRIVATE, 1, (void *)2L, &w1, 7), 3);
    waitdone(&x[0]);
    check("the first woke", x[0].done && !x[1].done && !x[2].done, 1);
    check("none left on w0", fx(&w0, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL, 0), 0);
    check("the moved ones on w1", fx(&w1, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL, 0), 2);
    for (int i = 0; i < 3; i++)
        pthread_join(t[i], NULL);
}

// futex_waitv: どれかで起こされると、その番号
struct waitv_arg {
    uint32_t *a, *b;
    volatile int tid;
    long r;
};

static void *waitv_main(void *arg) {
    struct waitv_arg *x = arg;
    x->tid = (int)syscall(SYS_gettid);
    struct {
        uint64_t val, uaddr;
        uint32_t flags, reserved;
    } v[2] = {{0, (uintptr_t)x->a, 2 | FUTEX_PRIVATE_FLAG, 0}, {0, (uintptr_t)x->b, 2 | FUTEX_PRIVATE_FLAG, 0}};
    long r = syscall(449, v, 2, 0, NULL, 0);
    x->r = r < 0 ? -errno : r;
    return NULL;
}

static void waitv(void) {
    w0 = w1 = 0;
    struct waitv_arg x = {&w0, &w1, 0, -1};
    pthread_t t;
    pthread_create(&t, NULL, waitv_main, &x);
    for (int i = 0; i < 400 && (!x.tid || tstate(x.tid) != 'S'); i++)
        msleep(5);
    msleep(5);
    check("wake the second", fx(&w1, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), 1);
    pthread_join(t, NULL);
    check("waitv returns its index", x.r, 1);
    check("the first is not queued any more", fx(&w0, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), 0);
    w0 = 1;
    struct {
        uint64_t val, uaddr;
        uint32_t flags, reserved;
    } v[1] = {{0, (uintptr_t)&w0, 2 | FUTEX_PRIVATE_FLAG, 0}};
    long r = syscall(449, v, 1, 0, NULL, 0);
    check("waitv: the value differs", r < 0 ? -errno : r, -EAGAIN);
}

// pthread_join はスレッドの終わりの (共有の) 起こしで返る
static void *quick(void *a) {
    msleep(20);
    return a;
}

static void join(void) {
    pthread_t t;
    void *r = NULL;
    pthread_create(&t, NULL, quick, (void *)42L);
    pthread_join(t, &r);
    check("pthread_join", (long)r, 42);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    fifo();
    keys();
    cross_process();
    timeouts();
    signals();
    pi_lock();
    requeue();
    waitv();
    join();
    printf("futex: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
