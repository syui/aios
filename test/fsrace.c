// fsrace: いくつものスレッドが同時にファイルを書き、あとで read と mmap で読みなおして比べる
// (rustc / LLVM がコード生成のスレッドごとに .o を書き、まとめて rlib にするのに似せる)。
// aios の中で: cc -O2 -pthread fsrace.c -o fsrace && ./fsrace [スレッド数] [1 スレッドのファイル数]
// 書いた中身とちがえば、場所と前後のバイトを出す。終わりに "fsrace: N bad of M" (0 ならよい)
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static int nfiles = 40;

static unsigned rnd(unsigned *x) { *x = *x * 1103515245u + 12345u; return *x >> 8; }

/// ファイル t-i の k バイト目に書くもの (あとで同じ式で確かめる)
static unsigned char byte_at(int t, int i, size_t k) { return (unsigned char)(t * 61 + i * 7 + k * 31 + (k >> 9)); }

static size_t size_of_file(int t, int i) { unsigned x = 99 + t * 1000 + i; return 1 + rnd(&x) % (2 << 20); }

static void *writer(void *arg) {
    int t = (int)(long)arg;
    unsigned x = 7 + t;
    char name[64];
    for (int i = 0; i < nfiles; i++) {
        size_t len = size_of_file(t, i);
        unsigned char *b = malloc(len);
        for (size_t k = 0; k < len; k++) b[k] = byte_at(t, i, k);
        snprintf(name, sizeof name, "fsrace-%d-%d", t, i);
        int fd = open(name, O_CREAT | O_TRUNC | O_RDWR, 0644);
        if (fd < 0) { perror("open"); exit(2); }
        // 半分は LLVM のように、先頭 (64 バイト) を 0 で書いておいて最後に pwrite で書きなおす
        int patch = i % 2 == 0 && len > 64;
        size_t off = 0;
        if (patch) {
            static const unsigned char zero[64];
            if (write(fd, zero, 64) != 64) { perror("write"); exit(2); }
            off = 64;
        }
        while (off < len) {
            size_t c = 1 + rnd(&x) % 100000;
            if (c > len - off) c = len - off;
            if (write(fd, b + off, c) != (ssize_t)c) { perror("write"); exit(2); }
            off += c;
        }
        if (patch && pwrite(fd, b, 64, 0) != 64) { perror("pwrite"); exit(2); }
        close(fd);
        free(b);
    }
    return 0;
}

static void show(const char *how, const char *name, size_t k, const unsigned char *got, int t, int i, size_t len) {
    printf("%s %s: byte %zu of %zu differs. got:", how, name, k, len);
    for (size_t j = k; j < k + 16 && j < len; j++) printf(" %02x", got[j]);
    printf("  want:");
    for (size_t j = k; j < k + 16 && j < len; j++) printf(" %02x", byte_at(t, i, j));
    printf("\n");
}

int main(int argc, char **argv) {
    int nthreads = argc > 1 ? atoi(argv[1]) : 4;
    if (argc > 2) nfiles = atoi(argv[2]);
    pthread_t th[64];
    for (long t = 0; t < nthreads; t++) pthread_create(&th[t], 0, writer, (void *)t);
    for (int t = 0; t < nthreads; t++) pthread_join(th[t], 0);
    int bad = 0, total = 0;
    char name[64];
    for (int t = 0; t < nthreads; t++) {
        for (int i = 0; i < nfiles; i++, total++) {
            size_t len = size_of_file(t, i);
            snprintf(name, sizeof name, "fsrace-%d-%d", t, i);
            int fd = open(name, O_RDONLY);
            struct stat st;
            fstat(fd, &st);
            if ((size_t)st.st_size != len) { printf("%s: size %ld, want %zu\n", name, (long)st.st_size, len); bad++; close(fd); continue; }
            unsigned char *r = malloc(len);
            size_t got = 0;
            while (got < len) { ssize_t n = read(fd, r + got, len - got); if (n <= 0) break; got += n; }
            unsigned char *m = mmap(0, len, PROT_READ, MAP_PRIVATE, fd, 0);
            int ok = 1;
            for (size_t k = 0; k < len && ok; k++)
                if (r[k] != byte_at(t, i, k)) { show("read", name, k, r, t, i, len); ok = 0; }
            for (size_t k = 0; m != MAP_FAILED && k < len; k++)
                if (m[k] != byte_at(t, i, k)) { show("mmap", name, k, m, t, i, len); ok = 0; break; }
            bad += !ok;
            if (m != MAP_FAILED) munmap(m, len);
            free(r);
            close(fd);
            unlink(name);
        }
    }
    printf("fsrace: %d bad of %d\n", bad, total);
    return bad != 0;
}
