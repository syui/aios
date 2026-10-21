// aios がえらんだちがい (doc/aios.md の表、test/sysdiff.c の chosen) を 1 つずつ確かめる (カーネル)。
// aios の中では、aios がえらんだとおりに動くか (vm-ctests.sh)。Linux では、Linux がまだちがうふるまいか
// (test/linux-vm.sh exec でいちばん新しい Linux の中で: Linux が aios と同じになったら SAME と出す。表を見なおすとき)。
//   cc -O2 -o chosen test/chosen.c (aios の中は zig cc -target aarch64-linux-musl -O2 -static)
//   ./chosen DIR...   DIR ごとに (ext4 のホームと tmpfs の /tmp)
// 1 行に ok NAME、SAME NAME (Linux だけ)、FAIL NAME。おしまいに chosen failed: N
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/vfs.h>
#include <unistd.h>

#ifndef FALLOC_FL_KEEP_SIZE
#define FALLOC_FL_KEEP_SIZE 0x01
#endif
#ifndef FALLOC_FL_ZERO_RANGE
#define FALLOC_FL_ZERO_RANGE 0x10
#endif
#ifndef SEEK_DATA
#define SEEK_DATA 3
#endif
#define K_TMPFS_MAGIC 0x01021994

static int on_aios, fails, same;
static char dir[4096];

// aios なら aios のふるまいか、Linux なら Linux のふるまいか。どちらでもなければ FAIL。Linux が aios と同じなら SAME
static void check(const char *name, int is_aios, int is_linux, const char *got) {
    if (on_aios ? is_aios : is_linux)
        printf("ok %s (%s): %s\n", name, dir, got);
    else if (!on_aios && is_aios) {
        printf("SAME %s (%s): Linux now does what aios chose: %s\n", name, dir, got);
        same++;
    } else {
        printf("FAIL %s (%s): %s\n", name, dir, got);
        fails++;
    }
}

static int mkfile(const char *name, const char *data, int flags) {
    char p[4200];
    snprintf(p, sizeof p, "%s/%s", dir, name);
    unlink(p);
    int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0 || write(fd, data, strlen(data)) != (ssize_t)strlen(data)) {
        perror(p);
        exit(2);
    }
    if (flags) {
        close(fd);
        fd = open(p, flags);
    }
    return fd;
}

static void rm(const char *name) {
    char p[4200];
    snprintf(p, sizeof p, "%s/%s", dir, name);
    unlink(p);
}

// ファイルの中身 (0 は . に)
static const char *body(int fd, char *b, size_t n) {
    ssize_t r = pread(fd, b, n - 1, 0);
    if (r < 0)
        r = 0;
    b[r] = 0;
    for (ssize_t i = 0; i < r; i++)
        if (!b[i])
            b[i] = '.';
    return b;
}

static void pwrite_append(void) {
    int fd = mkfile(".ch-pa", "abcd", O_RDWR | O_APPEND);
    char b[64], got[160];
    long r = pwrite(fd, "X", 1, 1);
    snprintf(got, sizeof got, "pwrite = %ld, file %s", r, body(fd, b, sizeof b));
    check("pwrite-append", r == 1 && !strcmp(b, "aXcd"), r == 1 && !strcmp(b, "abcdX"), got);
    close(fd);
    rm(".ch-pa");
}

static void seek_unwritten(int tmpfs) {
    // tmpfs は Linux でもいつも穴なので、ext4 だけ
    if (tmpfs)
        return;
    int fd = mkfile(".ch-su", "", 0);
    char b[4096], got[160];
    if (fallocate(fd, 0, 0, 8192) < 0) {
        printf("FAIL seek-unwritten (%s): fallocate: %s\n", dir, strerror(errno));
        fails++;
        close(fd);
        return;
    }
    // 読むと、Linux の ext4 ではページキャッシュに入り、そこはデータになる
    long n = pread(fd, b, sizeof b, 0);
    long d = lseek(fd, 0, SEEK_DATA);
    int e = errno;
    snprintf(got, sizeof got, "read %ld, SEEK_DATA = %ld%s%s", n, d, d < 0 ? " " : "", d < 0 ? strerror(e) : "");
    check("seek-unwritten", d < 0 && e == ENXIO, d == 0, got);
    close(fd);
    rm(".ch-su");
}

static void utimensat_omit(void) {
    struct timespec omit[2] = {{0, UTIME_OMIT}, {0, UTIME_OMIT}};
    char p[4200], got[200];
    snprintf(p, sizeof p, "%s/.ch-no-such-file", dir);
    long r1 = syscall(SYS_utimensat, AT_FDCWD, p, omit, 0);
    int e1 = errno;
    long r2 = syscall(SYS_utimensat, 999, NULL, omit, 0);
    int e2 = errno;
    // あるファイルは、どちらでも 0 で、時刻は変わらない
    int fd = mkfile(".ch-uo", "x", 0);
    struct timespec t[2] = {{1000000000, 0}, {1000000000, 0}};
    futimens(fd, t);
    struct stat a, b;
    fstat(fd, &a);
    snprintf(p, sizeof p, "%s/.ch-uo", dir);
    long r3 = syscall(SYS_utimensat, AT_FDCWD, p, omit, 0);
    fstat(fd, &b);
    int kept = a.st_mtim.tv_sec == b.st_mtim.tv_sec && a.st_mtim.tv_nsec == b.st_mtim.tv_nsec && a.st_ctim.tv_sec == b.st_ctim.tv_sec &&
               a.st_ctim.tv_nsec == b.st_ctim.tv_nsec;
    snprintf(got, sizeof got, "no file = %ld%s%s, closed fd = %ld%s%s, file = %ld%s", r1, r1 < 0 ? " " : "", r1 < 0 ? strerror(e1) : "", r2,
             r2 < 0 ? " " : "", r2 < 0 ? strerror(e2) : "", r3, kept ? " (times kept)" : " (times changed)");
    check("utimensat-omit", r1 < 0 && e1 == ENOENT && r2 < 0 && e2 == EBADF && r3 == 0 && kept, r1 == 0 && r2 == 0 && r3 == 0 && kept, got);
    close(fd);
    rm(".ch-uo");
}

static void copy_append(void) {
    int in = mkfile(".ch-ca-in", "hello", 0);
    int out = mkfile(".ch-ca-out", "xy", O_WRONLY | O_APPEND);
    lseek(in, 0, SEEK_SET);
    char b[64], got[300];
    // 位置なし: 終わりに足して、書くほうの位置は終わり
    long r1 = syscall(SYS_copy_file_range, in, NULL, out, NULL, 5, 0);
    int e1 = errno;
    long at = lseek(out, 0, SEEK_CUR);
    // 位置あり: そこへ (pwrite-append と同じ)
    long long i0 = 0, o1 = 1;
    long r2 = syscall(SYS_copy_file_range, in, &i0, out, &o1, 2, 0);
    int e2 = errno;
    // sendfile: 終わりに足す
    lseek(in, 0, SEEK_SET);
    long r3 = sendfile(out, in, NULL, 5);
    int e3 = errno;
    char p[4200];
    snprintf(p, sizeof p, "%s/.ch-ca-out", dir);
    int rd = open(p, O_RDONLY);
    body(rd, b, sizeof b);
    close(rd);
    snprintf(got, sizeof got, "copy_file_range = %ld%s%s (pos %ld), at 1 = %ld%s%s, sendfile = %ld%s%s, file %s", r1, r1 < 0 ? " " : "",
             r1 < 0 ? strerror(e1) : "", at, r2, r2 < 0 ? " " : "", r2 < 0 ? strerror(e2) : "", r3, r3 < 0 ? " " : "", r3 < 0 ? strerror(e3) : "",
             b);
    check("copy-append", r1 == 5 && at == 7 && r2 == 2 && o1 == 3 && r3 == 5 && !strcmp(b, "xheellohello"),
          r1 < 0 && e1 == EBADF && r2 < 0 && e2 == EBADF && r3 < 0 && e3 == EINVAL && !strcmp(b, "xy"), got);
    close(in);
    close(out);
    rm(".ch-ca-in");
    rm(".ch-ca-out");
}

static void sendfile_self(void) {
    char got[200];
    // 重なる: 0 からの 8 バイトを 4 へ。aios は EINVAL (copy_file_range と同じ)、Linux は写す
    int fd = mkfile(".ch-ss", "abcdefgh", 0);
    lseek(fd, 4, SEEK_SET);
    off_t at = 0;
    long r1 = sendfile(fd, fd, &at, 8);
    int e1 = errno;
    close(fd);
    // 重ならない: 終わりの先 (1000) へ 20 バイトたのむと、aios は読みはじめたときの終わりまで (8 バイト)。Linux は
    // 書いてのびたところ (穴) も読む
    fd = mkfile(".ch-ss", "abcdefgh", 0);
    lseek(fd, 1000, SEEK_SET);
    at = 0;
    long r2 = sendfile(fd, fd, &at, 20);
    struct stat st;
    fstat(fd, &st);
    snprintf(got, sizeof got, "overlapping = %ld%s%s, past the end = %ld, size %lld", r1, r1 < 0 ? " " : "", r1 < 0 ? strerror(e1) : "", r2,
             (long long)st.st_size);
    check("sendfile-self", r1 < 0 && e1 == EINVAL && r2 == 8 && st.st_size == 1008, r1 == 8 && r2 == 20 && st.st_size == 1020, got);
    close(fd);
    rm(".ch-ss");
}

static void splice_snapshot(void) {
    // 5000 バイトの z をパイプへ splice してから、ファイルの頭を Y にし、5 バイトに切りつめる。パイプから読むと、
    // aios は入れたときのまま (z が 5000)。Linux はページそのものを入れているので、Y が見え、切りつめた端のページの
    // 残り (5 から 4096) は 0
    char data[5001], b[5001], got[200];
    memset(data, 'z', 5000);
    data[5000] = 0;
    int fd = mkfile(".ch-ss2", data, 0);
    int p[2];
    if (pipe2(p, O_NONBLOCK) != 0) {
        printf("FAIL splice-snapshot (%s): pipe2\n", dir);
        fails++;
        return;
    }
    long long at = 0;
    long r1 = syscall(SYS_splice, fd, &at, p[1], NULL, 5000, 0);
    int e1 = errno;
    pwrite(fd, "Y", 1, 0);
    ftruncate(fd, 5);
    long n = read(p[0], b, 5000);
    long zeros = 0;
    for (long i = 0; i < n; i++)
        zeros += b[i] == 0;
    snprintf(got, sizeof got, "splice = %ld%s%s, read %ld: first %c, %ld zero", r1, r1 < 0 ? " " : "", r1 < 0 ? strerror(e1) : "", n,
             n > 0 ? (b[0] ? b[0] : '0') : '-', zeros);
    check("splice-snapshot", r1 == 5000 && n == 5000 && b[0] == 'z' && zeros == 0, r1 == 5000 && n == 5000 && b[0] == 'Y' && zeros > 0, got);
    close(p[0]);
    close(p[1]);
    close(fd);
    rm(".ch-ss2");
}

#ifndef SYS_preadv2
#if defined(__x86_64__)
#define SYS_preadv2 327
#else
#define SYS_preadv2 286
#endif
#endif

static void pipe_fault(void) {
    // パイプへ writev [3 バイト][さわれない 4 バイト]: aios はさわれる 3 を書く。Linux はページごとに写して足りないページを
    // 捨てるので、なにも書けず EFAULT
    int p[2];
    void *bad = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pipe2(p, O_NONBLOCK) != 0 || bad == MAP_FAILED) {
        printf("FAIL pipe-fault (%s): pipe2 / mmap\n", dir);
        fails++;
        return;
    }
    struct iovec iov[2] = {{"abc", 3}, {bad, 4}};
    long r = writev(p[1], iov, 2);
    int e = errno;
    char b[8], got[160];
    long n = read(p[0], b, sizeof b);
    snprintf(got, sizeof got, "writev = %ld%s%s, then read %ld", r, r < 0 ? " " : "", r < 0 ? strerror(e) : "", n);
    check("pipe-fault", r == 3 && n == 3, r < 0 && e == EFAULT && n < 0, got);
    close(p[0]);
    close(p[1]);
    munmap(bad, 4096);
}

static void pipe_nowait(void) {
    // パイプの両端で splice (EINVAL) をしたあとの preadv2 RWF_NOWAIT: aios は EAGAIN (空なので)。Linux は splice に使った
    // パイプの FMODE_NOWAIT を消すので EOPNOTSUPP
    int p[2];
    if (pipe2(p, O_NONBLOCK) != 0) {
        printf("FAIL pipe-nowait (%s): pipe2\n", dir);
        fails++;
        return;
    }
    long s = syscall(SYS_splice, p[0], NULL, p[1], NULL, 1, 0);
    int es = errno;
    char b[8], got[160];
    struct iovec iov = {b, sizeof b};
    long r = syscall(SYS_preadv2, p[0], &iov, 1, -1L, 0L, 8);
    int e = errno;
    snprintf(got, sizeof got, "splice = %ld%s%s, preadv2 NOWAIT = %ld%s%s", s, s < 0 ? " " : "", s < 0 ? strerror(es) : "", r, r < 0 ? " " : "",
             r < 0 ? strerror(e) : "");
    check("pipe-nowait", s < 0 && es == EINVAL && r < 0 && e == EAGAIN, s < 0 && es == EINVAL && r < 0 && e == EOPNOTSUPP, got);
    close(p[0]);
    close(p[1]);
}

static void unix_gc_now(void) {
    // socketpair の片方の fd を、もう片方からそれ自身の行列へ送って閉じる (メッセージの中にしかない輪)。aios は閉じた
    // ときに片づけるので、のこった口の read はすぐ 0 (EOF)。Linux はほかの AF_UNIX のソケットが閉じるまで片づけず、EAGAIN
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, sv) != 0) {
        printf("FAIL unix-gc-now (%s): socketpair\n", dir);
        fails++;
        return;
    }
    char c = 'x', cb[CMSG_SPACE(sizeof(int))];
    struct iovec iov = {&c, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb};
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &sv[1], sizeof(int));
    long s = sendmsg(sv[0], &m, 0);
    close(sv[1]);
    usleep(20000);
    char b[8], got[120];
    long r = read(sv[0], b, sizeof b);
    int e = errno;
    snprintf(got, sizeof got, "sendmsg = %ld, read after close = %ld%s%s", s, r, r < 0 ? " " : "", r < 0 ? strerror(e) : "");
    check("unix-gc-now", s == 1 && r == 0, s == 1 && r < 0 && e == EAGAIN, got);
    close(sv[0]);
}

static void zero_range(int tmpfs) {
    char data[8193], b[9100], got[200];
    for (int i = 0; i < 8192; i++)
        data[i] = 'a' + i % 26;
    data[8192] = 0;
    int fd = mkfile(".ch-zr", data, 0);
    // まん中を 0 に、終わりをこえて (大きくなる)、KEEP_SIZE で終わりの先 (大きさは変わらない)
    long r1 = fallocate(fd, FALLOC_FL_ZERO_RANGE, 100, 5000);
    int e1 = errno;
    long r2 = fallocate(fd, FALLOC_FL_ZERO_RANGE, 8000, 1000);
    long r3 = fallocate(fd, FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE, 8500, 4096);
    struct stat st;
    fstat(fd, &st);
    long n = pread(fd, b, sizeof b, 0);
    int ok = n == 9000;
    for (long i = 0; ok && i < n; i++)
        ok = b[i] == ((i >= 100 && i < 5100) || i >= 8000 ? 0 : data[i]);
    snprintf(got, sizeof got, "ZERO_RANGE = %ld%s%s, past the end = %ld, KEEP_SIZE = %ld, size %lld, %s", r1, r1 < 0 ? " " : "",
             r1 < 0 ? strerror(e1) : "", r2, r3, (long long)st.st_size, ok ? "zeros right" : "contents wrong");
    // ext4 はどちらでもできる。tmpfs は aios だけ
    int works = r1 == 0 && r2 == 0 && r3 == 0 && st.st_size == 9000 && ok;
    if (tmpfs)
        check("zero-range-tmpfs", works, r1 < 0 && e1 == EOPNOTSUPP && st.st_size == 8192, got);
    else if (works)
        printf("ok zero-range (ext4, the same on Linux) (%s): %s\n", dir, got);
    else {
        printf("FAIL zero-range (%s): %s\n", dir, got);
        fails++;
    }
    close(fd);
    rm(".ch-zr");
}

int main(int argc, char **argv) {
    struct utsname u;
    on_aios = uname(&u) == 0 && !strcmp(u.sysname, "aios");
    printf("chosen: on %s %s\n", u.sysname, u.release);
    for (int i = 1; i < argc; i++) {
        snprintf(dir, sizeof dir, "%s", argv[i]);
        struct statfs sf;
        int tmpfs = statfs(dir, &sf) == 0 && sf.f_type == K_TMPFS_MAGIC;
        pwrite_append();
        seek_unwritten(tmpfs);
        utimensat_omit();
        copy_append();
        zero_range(tmpfs);
        sendfile_self();
        splice_snapshot();
        pipe_fault();
        pipe_nowait();
        unix_gc_now();
    }
    if (!on_aios)
        printf("chosen: Linux does %d of them like aios now\n", same);
    printf("chosen failed: %d\n", fails);
    return fails != 0;
}
