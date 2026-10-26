// 大きな MAP_SHARED を外す: munmap と、写したまま終わるプロセス (exec/exit でアドレス空間を捨てる道)。
// どちらも、書いたページがみなファイルに書き戻され、外したあとの読み書きがふつうに続くことを確かめる。
// カーネルは外すページを少しずつ (128 枚ずつ、写しの表は 64 ずつ) 返す。前は外すページの数だけの Vec を作っていて、
// 3.4 GB のモデルを写した llama-server を止めたとき、メモリが足りずにその 12 MB が取れずにカーネルが止まった。
// ここでは 64 MiB (16384 ページ) で、まとめる数をなんども越える。
//   cc -O2 -static -o bigunmap bigunmap.c && ./bigunmap [MiB]
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static long pg;
static int fails;

#define CHECK(c, ...)                        \
    do {                                     \
        if (!(c)) {                          \
            printf("FAIL: " __VA_ARGS__);    \
            printf("\n");                    \
            fails++;                         \
        }                                    \
    } while (0)

// ページ i の印 (ページの頭とおしまいに)
static unsigned long mark(unsigned long i, unsigned long salt) {
    return (i * 2654435761UL) ^ salt;
}

static void fill(char *m, size_t n, unsigned long salt) {
    for (size_t i = 0; i < n; i++) {
        unsigned long v = mark(i, salt);
        memcpy(m + i * pg, &v, sizeof v);
        memcpy(m + (i + 1) * pg - sizeof v, &v, sizeof v);
    }
}

// read で読んで、みな印のとおりか (ちがう最初のページ、なければ -1)
static long verify(int fd, size_t n, unsigned long salt) {
    char *buf = malloc(pg);
    long bad = -1;
    for (size_t i = 0; i < n && bad < 0; i++) {
        unsigned long v = mark(i, salt), a, b;
        if (pread(fd, buf, pg, (off_t)i * pg) != pg) {
            bad = (long)i;
            break;
        }
        memcpy(&a, buf, sizeof a);
        memcpy(&b, buf + pg - sizeof b, sizeof b);
        if (a != v || b != v)
            bad = (long)i;
    }
    free(buf);
    return bad;
}

int main(int argc, char **argv) {
    pg = sysconf(_SC_PAGESIZE);
    size_t mib = argc > 1 ? strtoul(argv[1], 0, 10) : 64;
    size_t len = mib << 20, n = len / pg;
    char path[] = "/tmp/bigunmap.XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0 || ftruncate(fd, len) != 0) {
        perror("bigunmap: file");
        return 2;
    }
    unlink(path);

    // 1. munmap: 書いてから外す。表の鍵も、外すページも、まとめる数よりずっと多い
    char *m = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(m != MAP_FAILED, "mmap %zu MiB", mib);
    if (m == MAP_FAILED)
        return 1;
    fill(m, n, 1);
    CHECK(munmap(m, len) == 0, "munmap");
    long bad = verify(fd, n, 1);
    CHECK(bad < 0, "after munmap, page %ld of %zu did not reach the file", bad, n);

    // 2. 半分だけ外して、のこりは使いつづける (外すところの境目)
    m = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    fill(m, n, 2);
    CHECK(munmap(m, len / 2) == 0, "munmap half");
    fill(m + len / 2, n / 2, 3);
    unsigned long v;
    memcpy(&v, m + len / 2, sizeof v);
    CHECK(v == mark(0, 3), "the half still mapped reads back");
    CHECK(munmap(m + len / 2, len / 2) == 0, "munmap the other half");
    bad = verify(fd, n / 2, 2);
    CHECK(bad < 0, "first half: page %ld did not reach the file", bad);

    // 3. 写したまま終わるプロセス (アドレス空間を捨てる道): 子が書いて、外さずに _exit
    pid_t c = fork();
    if (c == 0) {
        char *cm = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (cm == MAP_FAILED)
            _exit(3);
        fill(cm, n, 4);
        _exit(0);
    }
    int st;
    CHECK(waitpid(c, &st, 0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 0, "child exit %d", st);
    bad = verify(fd, n, 4);
    CHECK(bad < 0, "after the child's exit, page %ld of %zu did not reach the file", bad, n);

    // 4. ふつうに続く: もう一度写して読める
    m = mmap(0, len, PROT_READ, MAP_SHARED, fd, 0);
    memcpy(&v, m + (n - 1) * pg, sizeof v);
    CHECK(v == mark(n - 1, 4), "map again reads the last page");
    munmap(m, len);
    close(fd);

    if (fails)
        return 1;
    printf("bigunmap: ok (%zu MiB, %zu pages)\n", mib, n);
    return 0;
}
