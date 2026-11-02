// close_range (kernel/src/sysfile.rs): Linux 7.4 の印 (CLOSE_RANGE_EXCEPT、CLOSE_RANGE_CLOEXEC_ONLY) と、前からのもの
// (UNSHARE、CLOEXEC、知らない印、first > last)。出すだけのテスト: 手本の Linux (test/linux-vm.sh exec mainline) の出力と
// くらべる。fd 3〜12 を開き、4、7、10 に FD_CLOEXEC をつけてから close_range を呼び、答え (0 か errno の名前) と、どの fd
// が開いているか (o: 開いている、c: 開いていて FD_CLOEXEC、.: 閉じている) を 1 行に出す
//   aios の中で: ./closerange     Linux で: test/linux-vm.sh exec mainline test/bin/closerange
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#define UNSHARE (1U << 1)
#define CLOEXEC (1U << 2)
#define EXCEPT (1U << 3)
#define CLOEXEC_ONLY (1U << 4)

static const char *err(int e) {
    switch (e) {
    case 0: return "0";
    case EINVAL: return "EINVAL";
    case EBADF: return "EBADF";
    case EMFILE: return "EMFILE";
    case ENOMEM: return "ENOMEM";
    default: return strerror(e);
    }
}

// 0 (閉じられていれば開きなおす)、1、2 はそのまま、3〜12 を /dev/null に。4、7、10 は FD_CLOEXEC。
// EXCEPT の範囲は、出力の fd 1 をいつもふくむようにする (範囲の外は閉じる)
static void setup(void) {
    if (fcntl(0, F_GETFD) < 0 && open("/dev/null", O_RDONLY) != 0)
        printf("setup: cannot reopen 0\n");
    fcntl(0, F_SETFD, 0);
    fcntl(1, F_SETFD, 0);
    fcntl(2, F_SETFD, 0);
    for (int fd = 3; fd <= 15; fd++)
        close(fd);
    for (int fd = 3; fd <= 12; fd++) {
        int f = open("/dev/null", O_RDONLY | ((fd == 4 || fd == 7 || fd == 10) ? O_CLOEXEC : 0));
        if (f != fd) {
            printf("setup: got %d for %d\n", f, fd);
            return;
        }
    }
}

static void state(char *out) {
    for (int fd = 0; fd <= 12; fd++) {
        int fl = fcntl(fd, F_GETFD);
        out[fd] = fl < 0 ? '.' : (fl & FD_CLOEXEC) ? 'c' : 'o';
    }
    out[13] = 0;
}

static void one(const char *name, unsigned first, unsigned last, unsigned flags) {
    setup();
    long r = syscall(SYS_close_range, first, last, flags);
    int e = r < 0 ? errno : 0;
    char s[16];
    state(s);
    printf("%-34s %-7s %s\n", name, err(e), s);
}

// スレッド (fd の表を分けている) から見た fd 3〜12
static char seen[16];
static int go, done_;
static void *peer(void *arg) {
    (void)arg;
    while (!__atomic_load_n(&go, __ATOMIC_ACQUIRE))
        usleep(1000);
    state(seen);
    __atomic_store_n(&done_, 1, __ATOMIC_RELEASE);
    return NULL;
}

static void shared(const char *name, unsigned first, unsigned last, unsigned flags) {
    setup();
    go = done_ = 0;
    pthread_t t;
    pthread_create(&t, NULL, peer, NULL);
    long r = syscall(SYS_close_range, first, last, flags);
    int e = r < 0 ? errno : 0;
    char mine[16];
    state(mine);
    __atomic_store_n(&go, 1, __ATOMIC_RELEASE);
    pthread_join(t, NULL);
    printf("%-34s %-7s %s thread %s\n", name, err(e), mine, seen);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    one("close 5..8", 5, 8, 0);
    one("close 9..~0", 9, ~0U, 0);
    one("cloexec 5..8", 5, 8, CLOEXEC);
    one("first > last", 8, 5, 0);
    one("unknown flag 1<<5", 5, 8, 1U << 5);
    one("unknown flag 1<<0", 5, 8, 1U << 0);
    one("except 1..8", 1, 8, EXCEPT);
    one("except 0..~0 (closes nothing)", 0, ~0U, EXCEPT);
    one("except 0..2", 0, 2, EXCEPT);
    one("except 1..6 cloexec", 1, 6, EXCEPT | CLOEXEC);
    one("cloexec_only 0..~0", 0, ~0U, CLOEXEC_ONLY);
    one("cloexec_only 5..8", 5, 8, CLOEXEC_ONLY);
    one("except cloexec_only 6..8", 6, 8, EXCEPT | CLOEXEC_ONLY);
    one("except cloexec_only ~0..~0", ~0U, ~0U, EXCEPT | CLOEXEC_ONLY);
    one("cloexec + cloexec_only", 0, ~0U, CLOEXEC | CLOEXEC_ONLY);
    one("except, first > last", 8, 5, EXCEPT);
    one("unshare close 5..8 (alone)", 5, 8, UNSHARE);
    shared("shared close 5..8", 5, 8, 0);
    shared("shared unshare close 5..8", 5, 8, UNSHARE);
    shared("shared unshare cloexec 5..8", 5, 8, UNSHARE | CLOEXEC);
    shared("shared unshare except 0..8", 0, 8, UNSHARE | EXCEPT);
    shared("shared unshare cloexec_only 0..~0", 0, ~0U, UNSHARE | CLOEXEC_ONLY);
    return 0;
}
