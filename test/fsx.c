// ファイルの中身のテスト (カーネル。手本は xfstests の fsx): 1 つのファイルに、種から決まるでたらめな op (pwrite、
// ftruncate、fallocate (場所をとる、KEEP_SIZE、PUNCH_HOLE、ZERO_RANGE)) をして、手もとの写しとくらべる。
// ext4 の extent の木 (まん中を外して分かれる、未初期化のブロックを書いて初期化ずみにする、深くなる) がこわれると、
// 読んだ中身がちがう。SEEK_DATA が飛ばしたところは 0 か、SEEK_HOLE はファイルの中か、も見る。Linux と aios の
// どちらでも 0 failed になること
//   cc -O2 -o fsx test/fsx.c (aios の中は zig cc -target aarch64-linux-musl -O2 -static)
//   ./fsx FILE [SEED] [OPS] [frag]
//   ./fsx --suite DIR...   DIR ごとに、ふつう (種 1、2000 op) と frag (種 2、2000 op) を (vm-ctests.sh)
// frag: ブロックひとつずつの書きこみを多くして、extent をたくさん作る (木が 2 段 3 段になる)。
// FSX_KEEP があれば、終わってもファイルを消さない (外から e2fsck で木を見るとき)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#ifndef FALLOC_FL_KEEP_SIZE
#define FALLOC_FL_KEEP_SIZE 0x01
#endif
#ifndef FALLOC_FL_PUNCH_HOLE
#define FALLOC_FL_PUNCH_HOLE 0x02
#endif
#ifndef FALLOC_FL_ZERO_RANGE
#define FALLOC_FL_ZERO_RANGE 0x10
#endif
#ifndef SEEK_DATA
#define SEEK_DATA 3
#define SEEK_HOLE 4
#endif

// 大きさの上限: 32 MiB (ブロックひとつずつの書きこみで extent がたくさんでき、木が深くなる)
#define MAXSIZE (32L << 20)

static uint64_t rng;
static uint64_t next(void) {
    // splitmix64
    uint64_t z = (rng += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}
static long pick(long n) { return (long)(next() % (uint64_t)n); }

static unsigned char *model, *buf;
static long size;
static int fd, fails, opno, frag;
static const char *path;

static void fail(const char *what, long a, long b) {
    printf("FAIL op %d: %s (%ld %ld)\n", opno, what, a, b);
    fails++;
}

// [off, off+len) を読んで写しとくらべる
static void verify(long off, long len) {
    if (off >= size)
        return;
    if (off + len > size)
        len = size - off;
    long got = pread(fd, buf, len, off);
    if (got != len) {
        fail("short read", got, len);
        return;
    }
    if (memcmp(buf, model + off, len) != 0) {
        long i = 0;
        while (buf[i] == model[off + i])
            i++;
        fail("content differs at", off + i, buf[i] - model[off + i]);
    }
}

// SEEK_DATA が飛ばしたところは 0、SEEK_HOLE はファイルの中
static void seeks(long off) {
    if (off >= size)
        return;
    long d = lseek(fd, off, SEEK_DATA);
    long end = d < 0 ? size : d;
    if (d < 0 && errno != ENXIO)
        fail("SEEK_DATA errno", errno, off);
    for (long i = off; i < end; i++)
        if (model[i]) {
            fail("SEEK_DATA skipped data at", i, d);
            break;
        }
    long h = lseek(fd, off, SEEK_HOLE);
    if (h < off || h > size)
        fail("SEEK_HOLE out of the file", h, off);
}

// 大きさを変えたとき: 伸びたところは 0
static void resize(long n) {
    if (n > size)
        memset(model + size, 0, n - size);
    size = n;
}

// 位置と長さ: ブロックのさかい目に寄せたものと、はしたのもの
static long where(void) {
    if (frag)
        return pick(MAXSIZE / 4096) * 4096 + (pick(4) ? 0 : pick(4096));
    long lim = size + (MAXSIZE - size) / 8 + 1;
    long x = pick(lim < MAXSIZE ? lim : MAXSIZE);
    return pick(3) ? x : x & ~4095L;
}
static long length(void) {
    if (frag)
        return pick(8) ? 1 + pick(4096) : 4096 * (1 + pick(64));
    static const long lens[] = {1, 100, 4095, 4096, 4097, 8192, 65536, 300000, 1 << 20};
    long n = lens[pick(9)];
    return pick(4) ? n : n + pick(4096);
}

// 1 つのファイルで ops 回。しくじりの数
static int run(const char *file, uint64_t seed, int ops, int fragmode) {
    path = file;
    rng = seed;
    frag = fragmode;
    fails = 0;
    size = 0;
    memset(model, 0, MAXSIZE);
    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) {
        perror(path);
        return 1;
    }
    long counts[8] = {0};
    // frag: はじめに 1 ブロックおきに書いて、extent を 4096 こ (木は 2 段以上に)
    if (frag) {
        for (long k = 0; k < 4096; k++) {
            long off = 2 * k * 4096;
            for (int i = 0; i < 100; i++)
                buf[i] = (unsigned char)(next() | 1);
            if (pwrite(fd, buf, 100, off) != 100) {
                fail("comb pwrite", off, errno);
                break;
            }
            if (off + 100 > size)
                resize(off + 100);
            memcpy(model + off, buf, 100);
        }
        verify(0, size);
    }
    for (opno = 0; opno < ops; opno++) {
        long off = where(), len = length();
        if (off + len > MAXSIZE)
            len = MAXSIZE - off;
        if (len <= 0)
            continue;
        // frag: 書くのを 3/4 に
        int op = frag && pick(4) ? 0 : (int)pick(8);
        counts[op]++;
        switch (op) {
        case 0:
        case 1: {
            // 書く (0 のない中身)
            for (long i = 0; i < len; i++)
                buf[i] = (unsigned char)(next() | 1);
            if (pwrite(fd, buf, len, off) != len) {
                fail("pwrite", off, len);
                break;
            }
            if (off + len > size)
                resize(off + len);
            memcpy(model + off, buf, len);
            break;
        }
        case 2: {
            // 切りつめる / 伸ばす
            long n = pick(2) ? off : off + len;
            if (ftruncate(fd, n) < 0) {
                fail("ftruncate", n, errno);
                break;
            }
            if (n < size)
                memset(model + n, 0, size - n);
            resize(n);
            break;
        }
        case 3:
        case 4: {
            // 場所をとる (中身は変わらない)
            int keep = op == 4;
            if (fallocate(fd, keep ? FALLOC_FL_KEEP_SIZE : 0, off, len) < 0) {
                fail("fallocate", off, errno);
                break;
            }
            if (!keep && off + len > size)
                resize(off + len);
            break;
        }
        case 5: {
            // 穴をあける
            if (fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, off, len) < 0) {
                fail("punch", off, errno);
                break;
            }
            if (off < size)
                memset(model + off, 0, (off + len < size ? off + len : size) - off);
            break;
        }
        case 6: {
            // 0 にする
            int keep = (int)pick(2);
            if (fallocate(fd, FALLOC_FL_ZERO_RANGE | (keep ? FALLOC_FL_KEEP_SIZE : 0), off, len) < 0) {
                // Linux の tmpfs にはない (aios はある: aios がえらんだちがい zero-range-tmpfs)
                if (errno == EOPNOTSUPP)
                    break;
                fail("zero range", off, errno);
                break;
            }
            if (!keep && off + len > size)
                resize(off + len);
            if (off < size)
                memset(model + off, 0, (off + len < size ? off + len : size) - off);
            break;
        }
        case 7:
            seeks(pick(size + 1));
            break;
        }
        // 大きさと、さわったところと、ほかのどこか
        struct stat st;
        if (fstat(fd, &st) < 0 || st.st_size != size)
            fail("size", (long)st.st_size, size);
        verify(off > 4096 ? off - 4096 : 0, len + 8192);
        verify(pick(size + 1), 65536);
        if (fails > 20)
            break;
    }
    // おしまいに、閉じて開きなおして全部
    close(fd);
    fd = open(path, O_RDONLY);
    verify(0, size);
    for (int i = 0; i < 50; i++)
        seeks(pick(size + 1));
    close(fd);
    if (!getenv("FSX_KEEP"))
        unlink(path);
    printf("%s%s: ops %d: write %ld truncate %ld falloc %ld keep %ld punch %ld zero %ld seek %ld, size %ld, failed %d\n", path,
           frag ? " (frag)" : "", opno, counts[0] + counts[1], counts[2], counts[3], counts[4], counts[5], counts[6], counts[7], size,
           fails);
    return fails;
}

int main(int argc, char **argv) {
    model = calloc(MAXSIZE, 1);
    buf = malloc(MAXSIZE);
    if (!model || !buf)
        return 2;
    int total = 0;
    if (argc > 1 && !strcmp(argv[1], "--suite")) {
        for (int i = 2; i < argc; i++) {
            char f[4096];
            snprintf(f, sizeof f, "%s/.fsx", argv[i]);
            total += run(f, 1, 2000, 0);
            total += run(f, 2, 2000, 1);
        }
    } else {
        total = run(argc > 1 ? argv[1] : "/tmp/.fsx", argc > 2 ? strtoull(argv[2], 0, 0) : 1, argc > 3 ? atoi(argv[3]) : 3000,
                    argc > 4 && !strcmp(argv[4], "frag"));
    }
    printf("fsx failed: %d\n", total);
    return total != 0;
}
