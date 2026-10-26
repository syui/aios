// RAM より大きいファイルを写して読む: MAP_SHARED と MAP_PRIVATE で、ページごとの印をみな 2 回ずつ確かめる。
// カーネルは、写しているファイルのページでしばらく触れていないものを外して捨て、触れればまた読む (vm.rs reclaim_file)。
// 前は写しているあいだ捨てなかったので、RAM より大きい写し (3.4 GB のモデルを写した llama-server) は RAM を
// 使いきり、ほかのプロセスのフォルトも取れずに init が殺されてカーネルが止まった。
// ファイルはディスクに置く (tmpfs ではなく)。大きさは MiB で (既定は /proc/meminfo の MemTotal の 1.5 倍)
//   cc -O2 -static -o bigmap bigmap.c && ./bigmap [MiB] [DIR]
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static long pg;

static unsigned long mark(unsigned long i) {
    return i * 0x9e3779b97f4a7c15UL + 1;
}

static size_t memtotal_mib(void) {
    FILE *f = fopen("/proc/meminfo", "r");
    char line[256];
    size_t kb = 0;
    while (f && fgets(line, sizeof line, f))
        if (sscanf(line, "MemTotal: %zu kB", &kb) == 1)
            break;
    if (f)
        fclose(f);
    return kb / 1024;
}

// 写して、ページごとの印を 2 回 (2 回目は捨てたものを読みなおす)。ちがう最初のページ、なければ -1
static long check(int fd, size_t len, size_t n, int flags) {
    unsigned char *m = mmap(0, len, PROT_READ, flags, fd, 0);
    if (m == MAP_FAILED) {
        perror("bigmap: mmap");
        return 0;
    }
    long bad = -1;
    for (int round = 0; round < 2 && bad < 0; round++)
        for (size_t i = 0; i < n; i++) {
            unsigned long v;
            memcpy(&v, m + i * pg, sizeof v);
            if (v != mark(i)) {
                bad = (long)i;
                break;
            }
        }
    munmap(m, len);
    return bad;
}

int main(int argc, char **argv) {
    pg = sysconf(_SC_PAGESIZE);
    size_t mib = argc > 1 ? strtoul(argv[1], 0, 10) : memtotal_mib() * 3 / 2;
    const char *dir = argc > 2 ? argv[2] : ".";
    size_t len = mib << 20, n = len / pg;
    char path[4096];
    snprintf(path, sizeof path, "%s/bigmap.XXXXXX", dir);
    int fd = mkstemp(path);
    if (fd < 0) {
        perror("bigmap: mkstemp");
        return 2;
    }
    unlink(path);
    // 1 MiB ずつ書く (ページの頭に印)
    char *buf = calloc(1, 1 << 20);
    for (size_t i = 0; i < n; i++) {
        unsigned long v = mark(i);
        memcpy(buf + (i * pg) % (1 << 20), &v, sizeof v);
        if ((i + 1) * pg % (1 << 20) == 0 && write(fd, buf, 1 << 20) != 1 << 20) {
            perror("bigmap: write");
            return 2;
        }
    }
    fsync(fd);
    int fails = 0;
    long bad = check(fd, len, n, MAP_SHARED);
    if (bad >= 0) {
        printf("FAIL: MAP_SHARED page %ld of %zu\n", bad, n);
        fails++;
    }
    bad = check(fd, len, n, MAP_PRIVATE);
    if (bad >= 0) {
        printf("FAIL: MAP_PRIVATE page %ld of %zu\n", bad, n);
        fails++;
    }
    close(fd);
    if (fails)
        return 1;
    printf("bigmap: ok (%zu MiB, %zu pages, RAM %zu MiB)\n", mib, n, memtotal_mib());
    return 0;
}
