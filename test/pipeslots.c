// パイプの箱 (slot) のテスト (カーネル): Linux と aios の両方で動かし、同じ答えになるか
// (cc -O2 -o pipeslots test/pipeslots.c、aios の中は zig cc -target aarch64-linux-musl -O2 -static)。
// Linux のパイプは 4KiB のページの箱 16 こ (pipe_buffer)。write は書く長さの 4KiB の端数が前の箱にまるごと
// 入るときだけそこへ足し、ほかは新しい箱へ。splice / sendfile で入れたもの (ファイルのページのかけら) は足さない箱。
// だから入る量は「あいたバイト」ではなく「あいた箱」で決まり、4KiB (PIPE_BUF) までの write は分かれない
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/sendfile.h>
#include <unistd.h>

static int fails;
static char buf[1 << 20];

static void check(const char *name, long got, long want) {
    if (got == want)
        printf("ok %s\n", name);
    else {
        printf("FAIL %s: %ld (want %ld)\n", name, got, want);
        fails++;
    }
}

// 書けたバイト数か -errno
static long wr(int fd, size_t n) {
    long r = write(fd, buf, n);
    return r < 0 ? -errno : r;
}

static long rd(int fd, size_t n) {
    long r = read(fd, buf, n);
    return r < 0 ? -errno : r;
}

static long sf(int out, int in, off_t off, size_t n) {
    long r = sendfile(out, in, &off, n);
    return r < 0 ? -errno : r;
}

static void newpipe(int p[2]) {
    if (pipe2(p, O_NONBLOCK) < 0) {
        perror("pipe2");
        exit(2);
    }
}

static int pollout(int fd) {
    struct pollfd q = {fd, POLLOUT, 0};
    return poll(&q, 1, 0) == 1 && (q.revents & POLLOUT);
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/tmp/.pipeslots";
    int p[2];
    memset(buf, 'x', sizeof buf);
    int f = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (f < 0 || write(f, buf, 200000) != 200000) {
        perror(path);
        return 2;
    }

    // 端数 100 の箱のあと、4096 はつぎの箱へ (端数がないので足さない)。残りは 14 箱
    newpipe(p);
    check("merge-100", wr(p[1], 100), 100);
    check("merge-4096", wr(p[1], 4096), 4096);
    check("merge-rest", wr(p[1], 65536), 14 * 4096);
    close(p[0]);
    close(p[1]);

    // いっぱいから 100 読んでも、はじめの箱はまだ使っている: 100 は入らない。箱を読みきると入る
    newpipe(p);
    check("full-64k", wr(p[1], 65536), 65536);
    check("full-read-100", rd(p[0], 100), 100);
    check("full-write-100", wr(p[1], 100), -EAGAIN);
    check("full-read-rest", rd(p[0], 3996), 3996);
    check("full-write-again", wr(p[1], 100), 100);
    check("full-write-4000", wr(p[1], 4000), -EAGAIN);
    close(p[0]);
    close(p[1]);

    // 4KiB (PIPE_BUF) までの write は分かれない: 3996 バイトあいていても、箱がなければ 4000 は EAGAIN
    newpipe(p);
    check("atomic-fill", wr(p[1], 15 * 4096 + 100), 15 * 4096 + 100);
    check("atomic-4000", wr(p[1], 4000), -EAGAIN);
    check("atomic-3996", wr(p[1], 3996), 3996);
    close(p[0]);
    close(p[1]);

    // sendfile はファイルのページのかけらごとに箱ひとつ: 3 からなら 4093 + 15 × 4096
    newpipe(p);
    check("sendfile-unaligned", sf(p[1], f, 3, sizeof buf), 65536 - 3);
    check("sendfile-full", sf(p[1], f, 0, 100), -EAGAIN);
    close(p[0]);
    close(p[1]);
    newpipe(p);
    check("sendfile-after-write-w", wr(p[1], 100), 100);
    check("sendfile-after-write", sf(p[1], f, 0, sizeof buf), 15 * 4096);
    close(p[0]);
    close(p[1]);

    // sendfile で入れた箱には足さない: 1 バイトの箱 16 こでいっぱい (POLLOUT もない)。1 つ読むとあく
    newpipe(p);
    long n = 0;
    for (int i = 0; i < 16; i++)
        n += sf(p[1], f, i * 4096, 1);
    check("spliced-16", n, 16);
    check("spliced-write", wr(p[1], 1), -EAGAIN);
    check("spliced-pollout", pollout(p[1]), 0);
    int q = 0;
    ioctl(p[0], FIONREAD, &q);
    check("spliced-fionread", q, 16);
    check("spliced-read", rd(p[0], 1), 1);
    check("spliced-pollout-again", pollout(p[1]), 1);
    check("spliced-write-again", wr(p[1], 1), 1);
    // 箱が 15 こ使われているので、1 箱 (4096) にはできない
    check("setpipe-busy", fcntl(p[1], F_SETPIPE_SZ, 4096) < 0 ? -errno : 0, -EBUSY);
    close(p[0]);
    close(p[1]);

    close(f);
    unlink(path);
    printf("pipeslots failed: %d\n", fails);
    return fails != 0;
}
