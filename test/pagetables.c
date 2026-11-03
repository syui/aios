// munmap で空になったページ表を返すか (kernel/src/vm.rs の unlink_tables: Linux の free_pgtables)。mmap と munmap を
// 何万回もくりかえしても /proc/meminfo の PageTables がふえないこと (前の aios は 2 MiB ごとにテーブルをふやした)、
// スレッドがいるとき (大きなロックの道)、ファイルの写し、テーブルを返したところをまた写すと 0 のページ、となりの
// 2 MiB のまどの中身はそのまま、ほかのスレッドが読みつづけても、0 か書いたものか SIGSEGV しか見えないこと。
// 出すだけのテスト: 手本の Linux (test/linux-vm.sh exec mainline test/bin/pagetables) とくらべる
//   aios の中で: ./pagetables      gcc -O2 -static -pthread -o pagetables test/pagetables.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#define MB (1UL << 20)

static long pagetables_kb(void) {
    char b[8192];
    int fd = open("/proc/meminfo", O_RDONLY);
    if (fd < 0)
        return -1;
    ssize_t n = read(fd, b, sizeof b - 1);
    close(fd);
    if (n <= 0)
        return -1;
    b[n] = 0;
    char *p = strstr(b, "\nPageTables:");
    return p ? strtol(p + 12, NULL, 10) : -1;
}

static void grew(const char *name, long before) {
    long after = pagetables_kb();
    printf("%-52s %s\n", name, before < 0 || after < 0 ? "no PageTables" : after - before < 256 ? "grew less than 256 kB" : "grew 256 kB or more");
}

static int loop_anon(int rounds) {
    int bad = 0;
    for (int i = 0; i < rounds; i++) {
        char *p = mmap(NULL, MB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED)
            return -1;
        p[0] = 1;
        p[MB - 1] = 2;
        bad += p[0] != 1 || p[MB - 1] != 2;
        if (munmap(p, MB) != 0)
            bad++;
    }
    return bad;
}

static volatile int stop;
static void *sleeper(void *a) {
    (void)a;
    while (!stop)
        usleep(1000);
    return NULL;
}

// ほかのスレッドが x を読みつづける: 写っていれば 0 か 'P'、外れていれば SIGSEGV のはず
static volatile char *x;
static volatile long seen_other, seen_ok, seen_segv;
static __thread sigjmp_buf jb;
static __thread int armed;
static void on_segv(int s) {
    (void)s;
    if (armed)
        siglongjmp(jb, 1);
    _exit(99);
}
static void *reader(void *a) {
    (void)a;
    while (!stop) {
        armed = 1;
        if (sigsetjmp(jb, 1)) {
            seen_segv++;
            continue;
        }
        char c = x[100];
        armed = 0;
        if (c == 0 || c == 'P')
            seen_ok++;
        else
            seen_other++;
    }
    armed = 0;
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("%-52s %s\n", "/proc/meminfo has PageTables", pagetables_kb() >= 0 ? "yes" : "no");

    // 1. ひとりのプロセスが 1 MiB を 20000 回 (20 GiB 分のアドレス)
    loop_anon(100);
    long b = pagetables_kb();
    printf("%-52s %d\n", "anon 1 MiB x 20000: bad", loop_anon(20000));
    grew("  PageTables", b);

    // 2. ファイルの写し
    char path[] = "/tmp/pagetablesXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0 || ftruncate(fd, MB) != 0)
        return 1;
    unlink(path);
    b = pagetables_kb();
    int bad = 0;
    for (int i = 0; i < 5000; i++) {
        volatile char *p = mmap(NULL, MB, PROT_READ, MAP_PRIVATE, fd, 0);
        if (p == MAP_FAILED)
            return 1;
        bad += p[0] != 0 || p[MB - 1] != 0;
        munmap((void *)p, MB);
    }
    printf("%-52s %d\n", "file 1 MiB x 5000: bad", bad);
    grew("  PageTables", b);
    close(fd);

    // 3. スレッドがいる (表を分けている)
    pthread_t t;
    stop = 0;
    pthread_create(&t, NULL, sleeper, NULL);
    b = pagetables_kb();
    printf("%-52s %d\n", "with a thread, anon 1 MiB x 5000: bad", loop_anon(5000));
    grew("  PageTables", b);
    stop = 1;
    pthread_join(t, NULL);

    // 4. 2 MiB のそろった場所に、X [base, +1 MiB) と Y [base + 3 MiB, +1 MiB) (同じ 1 GiB のちがう 2 MiB)。X を外して
    //    写しなおす。Y はそのまま
    char *area = mmap(NULL, 8 * MB, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *base = (char *)(((uintptr_t)area + 2 * MB - 1) & ~(2 * MB - 1));
    munmap(area, 8 * MB);
    char *X = mmap(base, MB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    char *Y = mmap(base + 3 * MB, MB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (X == MAP_FAILED || Y == MAP_FAILED)
        return 1;
    memset(X, 'x', MB);
    memset(Y, 'y', MB);
    munmap(X, MB);
    X = mmap(base, MB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    printf("%-52s %s\n", "X mapped again: zero", X != MAP_FAILED && X[0] == 0 && X[MB - 1] == 0 ? "yes" : "no");
    int ok = 1;
    for (size_t i = 0; i < MB; i += 4096)
        ok &= Y[i] == 'y';
    printf("%-52s %s\n", "Y untouched", ok ? "yes" : "no");
    munmap(X, MB);

    // 5. ほかのスレッドが X を読みつづけるあいだに、2000 回写して書いて外す
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_segv;
    sa.sa_flags = SA_NODEFER;
    sigaction(SIGSEGV, &sa, NULL);
    x = base;
    stop = 0;
    pthread_create(&t, NULL, reader, NULL);
    bad = 0;
    for (int i = 0; i < 2000; i++) {
        char *p = mmap(base, MB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        if (p != base) {
            bad++;
            break;
        }
        memset(p, 'P', 4096);
        if (i % 100 == 0) {
            struct timespec ts = {0, 200000};
            nanosleep(&ts, NULL);
        }
        munmap(p, MB);
    }
    stop = 1;
    pthread_join(t, NULL);
    printf("%-52s %d\n", "map / write / unmap x 2000 under a reader: bad", bad);
    printf("%-52s %s\n", "  the reader saw only 0, 'P' or SIGSEGV", seen_other == 0 ? "yes" : "no");
    printf("%-52s %s\n", "  the reader read something", seen_ok + seen_segv > 0 ? "yes" : "no");
    ok = 1;
    for (size_t i = 0; i < MB; i += 4096)
        ok &= Y[i] == 'y';
    printf("%-52s %s\n", "Y still untouched", ok ? "yes" : "no");
    return 0;
}
