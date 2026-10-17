// ファイルのロック (kernel/src/locks.rs): flock と fcntl のレコードロック (POSIX と OFD) が Linux と同じか。
// 1 つのプロセスの中 (POSIX と OFD のぶつかり、まとめと切り分け、閉じると外れる、dup) と、fork した子との取り合い
// (待つ、EDEADLK、子の終わりで外れる)。aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o rlock test/rlock.c;
// ./rlock $HOME /tmp。Linux でも同じ答え (test/linux-vm.sh exec latest ... /work /tmp)。しくじった数を終了コードで
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef F_OFD_GETLK
#define F_OFD_GETLK 36
#define F_OFD_SETLK 37
#define F_OFD_SETLKW 38
#endif

static int fails, checks;

static void check(const char *dir, const char *what, long got, long want) {
    checks++;
    if (got != want) {
        fails++;
        printf("FAIL %s: %s: got %ld, want %ld\n", dir, what, got, want);
    }
}

static int lk(int fd, int cmd, short type, off_t start, off_t len) {
    struct flock l;
    memset(&l, 0, sizeof l);
    l.l_type = type;
    l.l_whence = SEEK_SET;
    l.l_start = start;
    l.l_len = len;
    return fcntl(fd, cmd, &l) < 0 ? -errno : 0;
}

// GETLK の答え: 0 ならだれもかけていない、でなければ 種類*1000000 + 始まり*1000 + 長さ (pid は別に)
static long getlk(int fd, int cmd, short type, off_t start, off_t len, int *pid) {
    struct flock l;
    memset(&l, 0, sizeof l);
    l.l_type = type;
    l.l_whence = SEEK_SET;
    l.l_start = start;
    l.l_len = len;
    if (fcntl(fd, cmd, &l) < 0)
        return -errno;
    if (pid)
        *pid = l.l_pid;
    if (l.l_type == F_UNLCK)
        return 0;
    return (l.l_type + 1) * 1000000 + (long)l.l_start * 1000 + (long)l.l_len;
}

static void msleep(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

static void run(const char *dir) {
    char path[512];
    snprintf(path, sizeof path, "%s/rlock.%d", dir, (int)getpid());
    unlink(path);
    int a = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (a < 0) {
        printf("FAIL %s: cannot create %s: %s\n", dir, path, strerror(errno));
        fails++;
        return;
    }
    check(dir, "write 100", write(a, "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789", 100), 100);
    int b = open(path, O_RDWR), ro = open(path, O_RDONLY), wo = open(path, O_WRONLY);
    int pid = 0;
    // --- POSIX どうし (同じプロセス) はぶつからない: まとめ、切りかえ、切り分け ---
    check(dir, "SETLK WR 0-9", lk(a, F_SETLK, F_WRLCK, 0, 10), 0);
    check(dir, "SETLK WR 10-19 (merges)", lk(b, F_SETLK, F_WRLCK, 10, 10), 0);
    check(dir, "GETLK from own process sees nothing", getlk(a, F_GETLK, F_WRLCK, 0, 0, NULL), 0);
    // OFD で聞くと、プロセスのロックが見える (pid は自分)。まとまって 0-19
    check(dir, "OFD_GETLK sees the merged POSIX lock", getlk(b, F_OFD_GETLK, F_WRLCK, 0, 0, &pid), 2 * 1000000 + 0 * 1000 + 20);
    check(dir, "its pid is me", pid, getpid());
    check(dir, "SETLK RD 5-9 inside (splits)", lk(a, F_SETLK, F_RDLCK, 5, 5), 0);
    check(dir, "OFD_GETLK first piece 0-4 WR", getlk(b, F_OFD_GETLK, F_WRLCK, 0, 0, NULL), 2 * 1000000 + 0 * 1000 + 5);
    check(dir, "OFD_GETLK at 5: RD 5-9", getlk(b, F_OFD_GETLK, F_WRLCK, 5, 1, NULL), 1 * 1000000 + 5 * 1000 + 5);
    check(dir, "OFD_GETLK at 10: WR 10-19", getlk(b, F_OFD_GETLK, F_WRLCK, 10, 1, NULL), 2 * 1000000 + 10 * 1000 + 10);
    check(dir, "OFD_GETLK RD at 5 does not conflict with RD", getlk(b, F_OFD_GETLK, F_RDLCK, 5, 5, NULL), 0);
    check(dir, "UNLCK 0-19", lk(a, F_SETLK, F_UNLCK, 0, 20), 0);
    check(dir, "OFD_GETLK nothing", getlk(b, F_OFD_GETLK, F_WRLCK, 0, 0, NULL), 0);
    // --- OFD と POSIX、OFD と OFD (別の open) はぶつかる。同じ open (dup) はぶつからない ---
    check(dir, "OFD_SETLK WR 0-9 on a", lk(a, F_OFD_SETLK, F_WRLCK, 0, 10), 0);
    check(dir, "SETLK (POSIX) WR 5-14 EAGAIN", lk(b, F_SETLK, F_WRLCK, 5, 10), -EAGAIN);
    check(dir, "GETLK (POSIX) shows the OFD lock with pid -1", getlk(b, F_GETLK, F_WRLCK, 0, 0, &pid), 2 * 1000000 + 0 * 1000 + 10);
    check(dir, "OFD pid is -1", pid, -1);
    check(dir, "OFD_SETLK on another open EAGAIN", lk(b, F_OFD_SETLK, F_RDLCK, 9, 1), -EAGAIN);
    check(dir, "OFD_SETLK beside it ok", lk(b, F_OFD_SETLK, F_WRLCK, 10, 10), 0);
    int a2 = dup(a);
    check(dir, "dup shares the OFD lock: no conflict", lk(a2, F_OFD_SETLK, F_RDLCK, 0, 10), 0);
    check(dir, "OFD_GETLK F_UNLCK reports own locks", getlk(a2, F_OFD_GETLK, F_UNLCK, 0, 0, NULL), 1 * 1000000 + 0 * 1000 + 10);
    check(dir, "OFD_GETLK with l_pid != 0 EINVAL", ({ struct flock l; memset(&l, 0, sizeof l); l.l_type = F_WRLCK; l.l_pid = 5; fcntl(a, F_OFD_GETLK, &l) < 0 ? errno : 0; }), EINVAL);
    check(dir, "OFD_SETLK with l_pid != 0 EINVAL", ({ struct flock l; memset(&l, 0, sizeof l); l.l_type = F_WRLCK; l.l_pid = 5; fcntl(a, F_OFD_SETLK, &l) < 0 ? errno : 0; }), EINVAL);
    close(a2);
    check(dir, "closing the dup keeps the OFD lock", getlk(b, F_OFD_GETLK, F_WRLCK, 0, 10, NULL), 1 * 1000000 + 0 * 1000 + 10);
    check(dir, "OFD UNLCK 0-9", lk(a, F_OFD_SETLK, F_UNLCK, 0, 10), 0);
    check(dir, "OFD UNLCK 10-19 (b)", lk(b, F_OFD_SETLK, F_UNLCK, 10, 10), 0);
    // --- 口: 読めない fd に F_RDLCK は EBADF、書けない fd に F_WRLCK は EBADF。範囲のしらべ ---
    check(dir, "RDLCK on O_WRONLY EBADF", lk(wo, F_SETLK, F_RDLCK, 0, 1), -EBADF);
    check(dir, "WRLCK on O_RDONLY EBADF", lk(ro, F_SETLK, F_WRLCK, 0, 1), -EBADF);
    check(dir, "UNLCK on O_RDONLY ok", lk(ro, F_SETLK, F_UNLCK, 0, 1), 0);
    check(dir, "type 3 EINVAL", lk(a, F_SETLK, 3, 0, 1), -EINVAL);
    check(dir, "start -1 EINVAL", lk(a, F_SETLK, F_WRLCK, -1, 1), -EINVAL);
    check(dir, "negative len before 0 EINVAL", lk(a, F_SETLK, F_WRLCK, 0, -1), -EINVAL);
    check(dir, "negative len ok (locks 5-9)", lk(a, F_SETLK, F_WRLCK, 10, -5), 0);
    check(dir, "OFD_GETLK sees 5-9", getlk(b, F_OFD_GETLK, F_WRLCK, 0, 0, NULL), 2 * 1000000 + 5 * 1000 + 5);
    check(dir, "GETLK with F_UNLCK (POSIX) EINVAL", getlk(a, F_GETLK, F_UNLCK, 0, 0, NULL), -EINVAL);
    check(dir, "whence 7 EINVAL", ({ struct flock l; memset(&l, 0, sizeof l); l.l_type = F_WRLCK; l.l_whence = 7; fcntl(a, F_SETLK, &l) < 0 ? errno : 0; }), EINVAL);
    check(dir, "huge start EOVERFLOW", ({ struct flock l; memset(&l, 0, sizeof l); l.l_type = F_WRLCK; l.l_whence = SEEK_END; l.l_start = 0x7fffffffffffffffLL; fcntl(a, F_SETLK, &l) < 0 ? errno : 0; }), EOVERFLOW);
    // SEEK_CUR は fd の位置から、SEEK_END は終わりから
    lseek(b, 50, SEEK_SET);
    check(dir, "SETLK RD at SEEK_CUR+2 (52-53) on b", ({ struct flock l; memset(&l, 0, sizeof l); l.l_type = F_RDLCK; l.l_whence = SEEK_CUR; l.l_start = 2; l.l_len = 2; fcntl(b, F_OFD_SETLK, &l) < 0 ? -errno : 0; }), 0);
    check(dir, "GETLK (POSIX) sees it at 52", getlk(a, F_GETLK, F_WRLCK, 52, 1, NULL), 1 * 1000000 + 52 * 1000 + 2);
    check(dir, "OFD_SETLK WR at SEEK_END-10 (90-99)", ({ struct flock l; memset(&l, 0, sizeof l); l.l_type = F_WRLCK; l.l_whence = SEEK_END; l.l_start = -10; l.l_len = 10; fcntl(b, F_OFD_SETLK, &l) < 0 ? -errno : 0; }), 0);
    check(dir, "GETLK (POSIX) sees it at 90", getlk(a, F_GETLK, F_RDLCK, 95, 1, NULL), 2 * 1000000 + 90 * 1000 + 10);
    // --- どれか 1 つの fd を閉じると、プロセスの POSIX のロックはみな外れる (OFD は残る) ---
    check(dir, "POSIX WR 20-29", lk(a, F_SETLK, F_WRLCK, 20, 10), 0);
    check(dir, "OFD_GETLK sees POSIX 20-29", getlk(b, F_OFD_GETLK, F_WRLCK, 20, 10, NULL), 2 * 1000000 + 20 * 1000 + 10);
    close(ro);
    check(dir, "close(ro) released the POSIX locks", getlk(b, F_OFD_GETLK, F_WRLCK, 0, 0, NULL), 0);
    check(dir, "OFD locks on b stay (52)", getlk(a, F_GETLK, F_WRLCK, 52, 1, NULL), 1 * 1000000 + 52 * 1000 + 2);
    int b2 = dup(b);
    close(b);
    check(dir, "close(b) with a dup keeps OFD locks", getlk(a, F_GETLK, F_WRLCK, 52, 1, NULL), 1 * 1000000 + 52 * 1000 + 2);
    close(b2);
    check(dir, "last close releases OFD locks", getlk(a, F_GETLK, F_WRLCK, 0, 0, NULL), 0);
    close(wo);
    // --- flock: 別の open どうしはぶつかる。dup は同じ。切りかえ。パイプにもかけられる。だめな op は fd より先に EINVAL ---
    b = open(path, O_RDWR);
    check(dir, "flock EX a", flock(a, LOCK_EX | LOCK_NB), 0);
    check(dir, "flock SH b EAGAIN", flock(b, LOCK_SH | LOCK_NB) < 0 ? errno : 0, EWOULDBLOCK);
    check(dir, "flock EX a again ok", flock(a, LOCK_EX | LOCK_NB), 0);
    a2 = dup(a);
    check(dir, "dup of the holder may take SH (converts)", flock(a2, LOCK_SH | LOCK_NB), 0);
    check(dir, "then b may take SH", flock(b, LOCK_SH | LOCK_NB), 0);
    check(dir, "a cannot go back to EX while b holds SH (and loses its SH)", flock(a, LOCK_EX | LOCK_NB) < 0 ? errno : 0, EWOULDBLOCK);
    check(dir, "b takes EX now", flock(b, LOCK_EX | LOCK_NB), 0);
    check(dir, "flock UN b", flock(b, LOCK_UN), 0);
    close(a2);
    check(dir, "flock SH|EX EINVAL before EBADF", flock(999, LOCK_SH | LOCK_EX) < 0 ? errno : 0, EINVAL);
    check(dir, "flock bad fd EBADF", flock(999, LOCK_EX | LOCK_NB) < 0 ? errno : 0, EBADF);
    int pf = open(path, O_PATH);
    check(dir, "flock on O_PATH EBADF", flock(pf, LOCK_EX | LOCK_NB) < 0 ? errno : 0, EBADF);
    check(dir, "flock UN on O_PATH EBADF too (fdget skips O_PATH)", flock(pf, LOCK_UN) < 0 ? errno : 0, EBADF);
    check(dir, "fcntl lock on O_PATH EBADF", lk(pf, F_SETLK, F_WRLCK, 0, 1), -EBADF);
    close(pf);
    int p[2];
    check(dir, "pipe", pipe(p), 0);
    check(dir, "flock EX on the pipe's read end", flock(p[0], LOCK_EX | LOCK_NB), 0);
    check(dir, "flock SH on the write end EAGAIN (same pipe)", flock(p[1], LOCK_SH | LOCK_NB) < 0 ? errno : 0, EWOULDBLOCK);
    check(dir, "POSIX lock on a pipe ok", lk(p[1], F_SETLK, F_WRLCK, 0, 0), 0);
    close(p[0]);
    close(p[1]);
    // --- fork: 子は POSIX のロックを受けつがない、OFD と flock は受けつぐ。取り合いと待ち、EDEADLK、子の終わりで外れる ---
    check(dir, "POSIX WR 0-9 before fork", lk(a, F_SETLK, F_WRLCK, 0, 10), 0);
    check(dir, "OFD WR 40-49 on b before fork", lk(b, F_OFD_SETLK, F_WRLCK, 40, 10), 0);
    int sp[2];
    check(dir, "socketpair-ish pipe for sync", pipe(sp), 0);
    pid_t c = fork();
    if (c == 0) {
        int bad = 0;
        // 親の POSIX のロックは子には見えるがぶつかる (持ち主がちがう)。OFD は同じ持ち主 (同じ open)
        bad += getlk(b, F_GETLK, F_WRLCK, 0, 10, NULL) != 2 * 1000000 + 0 * 1000 + 10;
        bad += lk(a, F_SETLK, F_WRLCK, 0, 10) != -EAGAIN;
        bad += lk(b, F_OFD_SETLK, F_WRLCK, 40, 10) != 0;
        // 子が POSIX で 60-69 をとり、親に知らせ、親が待っているあいだに外す
        bad += lk(a, F_SETLK, F_WRLCK, 60, 10) != 0;
        if (write(sp[1], "x", 1) != 1)
            bad++;
        msleep(300);
        bad += lk(a, F_SETLK, F_UNLCK, 60, 10) != 0;
        // 行きどまり: 子は 0-9 (親のもの) を待つ。親は子の 70-79 を待つ (親が先に待ちはじめるようにしてから)
        bad += lk(a, F_SETLK, F_WRLCK, 70, 10) != 0;
        if (write(sp[1], "y", 1) != 1)
            bad++;
        msleep(300);
        int r = lk(a, F_SETLKW, F_WRLCK, 0, 10);
        bad += r != -EDEADLK;
        // 70-79 は持ったまま終わる: 子の終わりで外れる
        _exit(bad);
    }
    char ch;
    check(dir, "child locked 60-69", read(sp[0], &ch, 1), 1);
    check(dir, "GETLK sees the child's lock (pid other)", getlk(b, F_GETLK, F_WRLCK, 60, 10, &pid), 2 * 1000000 + 60 * 1000 + 10);
    check(dir, "its pid is the child", pid, c);
    check(dir, "SETLKW waits until the child unlocks", lk(a, F_SETLKW, F_WRLCK, 60, 10), 0);
    check(dir, "then it is ours", getlk(b, F_OFD_GETLK, F_WRLCK, 60, 10, &pid), 2 * 1000000 + 60 * 1000 + 10);
    check(dir, "owner is me", pid, getpid());
    check(dir, "child locked 70-79", read(sp[0], &ch, 1), 1);
    // 親が子の 70-79 を待つ。子はそのあと親の 0-9 を待って EDEADLK になり、70-79 を持ったまま終わるので、親は通る
    check(dir, "SETLKW on the child's lock returns when the child exits", lk(a, F_SETLKW, F_WRLCK, 70, 10), 0);
    int st = -1;
    waitpid(c, &st, 0);
    check(dir, "child checks passed", WIFEXITED(st) ? WEXITSTATUS(st) : 99, 0);
    // 子のロックは消え、親の 60-69 と 70-79 は隣りあう同じ種類なので 60-79 にまとまっている
    check(dir, "child's locks are gone after exit (ours merged 60-79)", getlk(b, F_OFD_GETLK, F_WRLCK, 70, 10, &pid), 2 * 1000000 + 60 * 1000 + 20);
    check(dir, "it is ours now", pid, getpid());
    close(sp[0]);
    close(sp[1]);
    close(a);
    close(b);
    unlink(path);
}

int main(int argc, char **argv) {
    signal(SIGPIPE, SIG_IGN);
    if (argc < 2)
        run("/tmp");
    for (int i = 1; i < argc; i++)
        run(argv[i]);
    printf("rlock: %d checks, %d failed\n", checks, fails);
    return fails;
}
