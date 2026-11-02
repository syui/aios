// robust futex (kernel/src/futex.rs の exit_robust_list と get_robust_list、proc.rs の Proc::robust_list): 持ったまま
// 終わったスレッド (exec したプロセス) の robust な futex に FUTEX_OWNER_DIED がつき、待っているものが起きて EOWNERDEAD を
// 受けること。出すだけのテスト: 手本の Linux (test/linux-vm.sh exec mainline test/bin/robust) の出力とくらべる
//   aios の中で: ./robust      gcc -O2 -static -pthread -o robust test/robust.c
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *err(int e) {
    switch (e) {
    case 0: return "0";
    case EOWNERDEAD: return "EOWNERDEAD";
    case ENOTRECOVERABLE: return "ENOTRECOVERABLE";
    case ETIMEDOUT: return "ETIMEDOUT";
    case EBUSY: return "EBUSY";
    case EINVAL: return "EINVAL";
    case EPERM: return "EPERM";
    case ESRCH: return "ESRCH";
    case EFAULT: return "EFAULT";
    case EDEADLK: return "EDEADLK";
    default: return strerror(e);
    }
}

// 5 秒まで待って取る (起こしが来なければ ETIMEDOUT)
static int lock5(pthread_mutex_t *m) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_sec += 5;
    return pthread_mutex_timedlock(m, &ts);
}

static void robust_init(pthread_mutex_t *m, int pshared) {
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setrobust(&a, PTHREAD_MUTEX_ROBUST);
    if (pshared)
        pthread_mutexattr_setpshared(&a, PTHREAD_PROCESS_SHARED);
    pthread_mutex_init(m, &a);
}

// 取ってから、どうなったかを 1 行に: EOWNERDEAD なら consistent にして放し、もういちど取れるか
static void report(const char *name, pthread_mutex_t *m, int r) {
    int r2 = -1, r3 = -1;
    if (r == EOWNERDEAD) {
        r2 = pthread_mutex_consistent(m);
        pthread_mutex_unlock(m);
        r3 = pthread_mutex_trylock(m);
        if (r3 == 0)
            pthread_mutex_unlock(m);
    } else if (r == 0) {
        pthread_mutex_unlock(m);
    }
    printf("%-44s lock %s", name, err(r));
    if (r == EOWNERDEAD)
        printf(", consistent %s, again %s", err(r2), err(r3));
    printf("\n");
}

static pthread_mutex_t m1;
static void *hold_and_exit(void *arg) {
    pthread_mutex_lock(&m1);
    if (arg)
        usleep(200 * 1000);  // 待つものが眠るまで
    return NULL;  // 放さずに終わる
}

static void *wait_m1(void *arg) {
    int *r = arg;
    *r = lock5(&m1);
    if (*r == EOWNERDEAD)
        pthread_mutex_consistent(&m1);
    if (*r == 0 || *r == EOWNERDEAD)
        pthread_mutex_unlock(&m1);
    return NULL;
}

// ---- 手で作った robust list (Linux の handle_futex_death の決まり) ----

struct rhead {
    void *next;
    long off;
    void *pending;
};
struct item {
    void *next;
    uint32_t lock;
};
static struct rhead H;
static struct item I1, I2, I3;
static uint32_t pend_word;

static long gettid_(void) { return syscall(SYS_gettid); }

// 自分の tid を使う語をそろえてから、手で作った list に替えて終わる
static void *raw_thread(void *arg) {
    (void)arg;
    uint32_t me = gettid_();
    I1.lock = me | FUTEX_WAITERS;  // 自分が持っている、待っているものあり
    I2.lock = 12345;                // ほかのもの
    I3.lock = me;                   // PI の印つきで自分のもの
    H.off = offsetof(struct item, lock);
    I1.next = &I2;
    I2.next = (void *)((uintptr_t)&I3 | 1);
    I3.next = &H;
    H.next = &I1;
    H.pending = NULL;
    syscall(SYS_set_robust_list, &H, sizeof H);
    return NULL;
}

// 取りかけ (pending) で持ち主のない語は変えない
static void *pending_thread(void *arg) {
    (void)arg;
    pend_word = FUTEX_WAITERS;  // 持ち主 0、待っているものの印
    H.off = 0;
    H.next = &H;
    H.pending = &pend_word;
    syscall(SYS_set_robust_list, &H, sizeof H);
    return NULL;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && !strcmp(argv[1], "exit"))
        return 0;

    // set / get の答え
    struct rhead *head = NULL;
    size_t len = 0;
    long r = syscall(SYS_get_robust_list, 0, &head, &len);
    printf("get_robust_list(0)                           %s, len %zu, head %s\n", err(r ? errno : 0), len, head ? "set (glibc)" : "null");
    r = syscall(SYS_set_robust_list, head, 23);
    printf("set_robust_list(len 23)                      %s\n", err(r ? errno : 0));
    r = syscall(SYS_get_robust_list, 0, NULL, &len);
    printf("get_robust_list(head_ptr NULL)               %s\n", err(r ? errno : 0));
    r = syscall(SYS_get_robust_list, 999999, &head, &len);
    printf("get_robust_list(no such pid)                 %s\n", err(r ? errno : 0));
    r = syscall(SYS_get_robust_list, 1, &head, &len);
    printf("get_robust_list(pid 1, not ours)             %s\n", err(r ? errno : 0));
    pid_t c = fork();
    if (c == 0) {
        pause();
        _exit(0);
    }
    usleep(100 * 1000);
    head = NULL;
    r = syscall(SYS_get_robust_list, c, &head, &len);
    printf("get_robust_list(our child)                   %s, %s\n", err(r ? errno : 0), head ? "set" : "null");
    kill(c, SIGKILL);
    waitpid(c, NULL, 0);

    // スレッドが持ったまま終わる
    robust_init(&m1, 0);
    pthread_t t;
    pthread_create(&t, NULL, hold_and_exit, NULL);
    pthread_join(t, NULL);
    report("thread exits holding it", &m1, lock5(&m1));

    // 待っているものがいる
    int wr = -1;
    pthread_t w;
    pthread_create(&t, NULL, hold_and_exit, (void *)1);
    usleep(50 * 1000);
    pthread_create(&w, NULL, wait_m1, &wr);
    pthread_join(t, NULL);
    pthread_join(w, NULL);
    printf("%-44s lock %s\n", "a waiter, the holder thread exits", err(wr));

    // プロセスどうし (MAP_SHARED): _exit、SIGKILL、exec、待っているもの
    pthread_mutex_t *sm = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    const char *how[] = {"_exit", "SIGKILL", "exec"};
    for (int i = 0; i < 3; i++) {
        robust_init(sm, 1);
        int p[2];
        if (pipe(p) < 0)
            return 1;
        c = fork();
        if (c == 0) {
            pthread_mutex_lock(sm);
            if (write(p[1], "x", 1) < 0)
                _exit(1);
            if (i == 0)
                _exit(0);
            if (i == 1)
                pause();
            execl("/proc/self/exe", "robust", "exit", (char *)NULL);
            _exit(127);
        }
        char b;
        if (read(p[0], &b, 1) != 1)
            return 1;
        if (i == 1)
            kill(c, SIGKILL);
        int st;
        waitpid(c, &st, 0);
        char name[64];
        snprintf(name, sizeof name, "process %s holding a shared one", how[i]);
        report(name, sm, lock5(sm));
        close(p[0]);
        close(p[1]);
    }
    robust_init(sm, 1);
    c = fork();
    if (c == 0) {
        pthread_mutex_lock(sm);
        usleep(300 * 1000);
        _exit(0);
    }
    usleep(100 * 1000);
    report("a waiting process, the holder exits", sm, lock5(sm));
    waitpid(c, NULL, 0);

    // 手で作った list
    pthread_create(&t, NULL, raw_thread, NULL);
    pthread_join(t, NULL);
    printf("%-44s ours+waiters %s, not ours %s, pi %s\n", "raw list after the thread exits",
           I1.lock == (FUTEX_WAITERS | FUTEX_OWNER_DIED) ? "WAITERS|OWNER_DIED" : "changed?",
           I2.lock == 12345 ? "unchanged" : "changed",
           I3.lock == FUTEX_OWNER_DIED ? "OWNER_DIED" : "other");
    pthread_create(&t, NULL, pending_thread, NULL);
    pthread_join(t, NULL);
    printf("%-44s %s\n", "pending op with no owner", pend_word == FUTEX_WAITERS ? "unchanged" : "changed");
    return 0;
}
