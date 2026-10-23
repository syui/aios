// 消したファイルが、だれも使わなくなったところで片づくか (inode とブロックが返るか): aios の中で動かす
// (zig cc -target aarch64-linux-musl -O2 -static -o orphan test/orphan.c)。どれも、ファイルを作って下のことをし、
// 閉じて消す (か、開いたまま消してから閉じる) と、使える inode の数 (statvfs の f_ffree) がもとに戻る:
//   plain      書いて読む
//   hole       ftruncate でのばした穴を read で読む
//   zero       fallocate の ZERO_RANGE でのばしたところを read で読む
//   open-hole  穴を読み、開いたまま消してから閉じる
//   child      子が穴を読んで終わる。親が消す
//   map-priv   MAP_PRIVATE で穴を読み、munmap してから消す
//   map-shared MAP_SHARED で穴を読み、munmap してから消す
//   alias      リンクを通ったパスで stat してから、本当のパスで消す
//   exchange   2 つを stat してから RENAME_EXCHANGE で入れかえ、両方消す
//   moved-dir  ディレクトリの中のファイルを stat してから、ディレクトリの名前を変え、新しいパスで消す
// (read で穴を読むと、写しの表に inode を持つ 0 のページができて、消しても片づかなかった。入れかえた名前の覚え
// (カーネルのパスの覚え: Linux の dcache) も、消したときに捨てずに inode を持ちつづけていた)
// aios なら /proc/ai/fs の orphans (消したが使われているもの) が増えないかも見る。Linux でも同じに通る。
// 1 行に ok NAME か FAIL NAME、おしまいに orphan: N failed
//   orphan [DIR...]   DIR (既定 .) ごとに、その下に orphan-PID を作って
#define _GNU_SOURCE
#include <fcntl.h>
#include <linux/falloc.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

// musl (zig cc) には renameat2 がないので、じかに
#ifndef RENAME_EXCHANGE
#define RENAME_EXCHANGE (1 << 1)
#endif

static int fails;
static char path[512], sub[512], lnk[512];
static const char *dir;
#define CHECK(name, cond) do { if (cond) printf("ok %s %s\n", dir, name); else { printf("FAIL %s %s\n", dir, name); fails++; } } while (0)

// aios の /proc/ai/fs の、はじめのマウント (ルート) の orphans。なければ -1
static long orphans(void) {
    char b[16384];
    int fd = open("/proc/ai/fs", O_RDONLY);
    if (fd < 0)
        return -1;
    ssize_t n = read(fd, b, sizeof b - 1);
    close(fd);
    if (n <= 0)
        return -1;
    b[n] = 0;
    char *p = strstr(b, "\"orphans\":");
    return p ? atol(p + 10) : -1;
}

static long free_inodes(void) {
    struct statvfs s;
    return statvfs(dir, &s) == 0 ? (long)s.f_ffree : -1;
}

static int make(void) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "hello\n", 6) != 6) {
        perror(path);
        exit(2);
    }
    return fd;
}

static void read_all(int fd) {
    static char buf[65536];
    lseek(fd, 0, SEEK_SET);
    while (read(fd, buf, sizeof buf) > 0)
        ;
}

// 穴かゼロの区間を 1 MiB のばす (zero なら ZERO_RANGE で、でなければ ftruncate の穴)
static void grow(int fd, int zero) {
    if (zero)
        fallocate(fd, FALLOC_FL_ZERO_RANGE, 4096, 1 << 20);
    else if (ftruncate(fd, 4096 + (1 << 20)) != 0)
        perror("ftruncate");
}

static void touch_map(int fd, int flags) {
    size_t len = 4096 + (1 << 20);
    volatile char *m = mmap(NULL, len, PROT_READ, flags, fd, 0);
    if (m == MAP_FAILED)
        return;
    long sum = 0;
    for (size_t i = 0; i < len; i += 4096)
        sum += m[i];
    (void)sum;
    munmap((void *)m, len);
}

static void one(const char *name) {
    long fi = free_inodes(), o = orphans();
    int fd = make();
    if (!strcmp(name, "plain")) {
        read_all(fd);
    } else if (!strcmp(name, "hole") || !strcmp(name, "zero")) {
        grow(fd, name[0] == 'z');
        read_all(fd);
    } else if (!strcmp(name, "open-hole")) {
        grow(fd, 0);
        read_all(fd);
        unlink(path);
    } else if (!strcmp(name, "child")) {
        grow(fd, 0);
        close(fd);
        fd = -1;
        pid_t k = fork();
        if (k == 0) {
            int f = open(path, O_RDONLY);
            read_all(f);
            _exit(0);
        }
        waitpid(k, NULL, 0);
    } else if (!strcmp(name, "map-priv") || !strcmp(name, "map-shared")) {
        grow(fd, 0);
        touch_map(fd, name[4] == 'p' ? MAP_PRIVATE : MAP_SHARED);
    }
    if (fd >= 0)
        close(fd);
    unlink(path);
    CHECK(name, free_inodes() == fi && (o < 0 || orphans() == o));
}

// 名前で覚えたあとに名前が動くもの: alias はリンクを通ったパス (DIR/orphan-PID.l/f) で stat してから、本当のパス
// (DIR/orphan-PID.d/f) で消す。exchange は 2 つを stat してから RENAME_EXCHANGE で入れかえ、両方消す。moved-dir は
// DIR/orphan-PID.d/f を stat してから、ディレクトリを DIR/orphan-PID.l に変えて、そこで消す
static void moved(const char *name) {
    char f[600], g[600];
    struct stat st;
    long fi = free_inodes(), o = orphans();
    mkdir(sub, 0755);
    if (!strcmp(name, "alias")) {
        if (symlink(sub, lnk) != 0)
            perror(lnk);
        snprintf(f, sizeof f, "%s/f", sub);
        snprintf(g, sizeof g, "%s/f", lnk);
        close(open(f, O_WRONLY | O_CREAT, 0644));
        for (int i = 0; i < 3; i++)
            stat(g, &st);
        unlink(f);
        unlink(lnk);
    } else if (!strcmp(name, "moved-dir")) {
        snprintf(f, sizeof f, "%s/f", sub);
        snprintf(g, sizeof g, "%s/f", lnk);
        close(open(f, O_WRONLY | O_CREAT, 0644));
        stat(f, &st);
        if (rename(sub, lnk) != 0)
            perror("rename");
        unlink(g);
        rename(lnk, sub);
    } else {
        snprintf(f, sizeof f, "%s/x", sub);
        snprintf(g, sizeof g, "%s/y", sub);
        close(open(f, O_WRONLY | O_CREAT, 0644));
        close(open(g, O_WRONLY | O_CREAT, 0644));
        stat(f, &st);
        stat(g, &st);
        if (syscall(SYS_renameat2, AT_FDCWD, f, AT_FDCWD, g, RENAME_EXCHANGE) != 0)
            perror("renameat2");
        unlink(f);
        unlink(g);
    }
    rmdir(sub);
    CHECK(name, free_inodes() == fi && (o < 0 || orphans() == o));
}

int main(int argc, char **argv) {
    static const char *names[] = { "plain", "hole", "zero", "open-hole", "child", "map-priv", "map-shared" };
    for (int a = 1; a < argc || a == 1; a++) {
        dir = a < argc ? argv[a] : ".";
        snprintf(path, sizeof path, "%s/orphan-%d", dir, getpid());
        snprintf(sub, sizeof sub, "%s/orphan-%d.d", dir, getpid());
        snprintf(lnk, sizeof lnk, "%s/orphan-%d.l", dir, getpid());
        for (size_t i = 0; i < sizeof names / sizeof *names; i++)
            one(names[i]);
        moved("alias");
        moved("exchange");
        moved("moved-dir");
    }
    printf("orphan: %d failed\n", fails);
    return fails;
}
