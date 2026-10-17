// ファイルのページは 1 枚 (vm.rs): mmap の写しと read / write / truncate のまぜこぜが Linux (ext4) と同じになるか。
// ext4 (ホーム) と tmpfs (/tmp) で。aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o mmapcoh test/mmapcoh.c;
// ./mmapcoh $HOME /tmp。Linux でも動く (test/linux-vm.sh exec latest ... /work /tmp): tmpfs の「伸ばしたら 0」だけは
// Linux の tmpfs がちがう (手本の Linux のまちがい tmpfs-extend) ので、Linux では LINUX と出して数えない。
// しくじった数を終了コードで
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails, checks, on_linux;
static sigjmp_buf jb;
static volatile int sig;

static void onsig(int s) {
    sig = s;
    siglongjmp(jb, 1);
}

// p に触れる (w なら書く)。シグナルならその番号、でなければ 0
static int touch(volatile unsigned char *p, int w) {
    sig = 0;
    if (sigsetjmp(jb, 1) == 0) {
        if (w)
            *p = 'X';
        else
            (void)*p;
        return 0;
    }
    return sig;
}

static void check(const char *dir, const char *what, long got, long want) {
    checks++;
    if (got != want) {
        fails++;
        printf("FAIL %s: %s: got %ld, want %ld\n", dir, what, got, want);
    }
}

static long rd(int fd, long off) {
    unsigned char c = 0;
    return pread(fd, &c, 1, off) == 1 ? c : -1;
}

static void run(const char *dir) {
    char path[512];
    snprintf(path, sizeof path, "%s/mmapcoh.%d", dir, (int)getpid());
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        printf("FAIL %s: cannot create %s: %s\n", dir, path, strerror(errno));
        fails++;
        return;
    }
    char buf[100];
    memset(buf, 'a', sizeof buf);
    check(dir, "write 100", write(fd, buf, 100), 100);
    unsigned char *m = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(dir, "mmap shared", m != MAP_FAILED, 1);
    // 終わりより先のページは SIGBUS。同じページの中の終わりの先は 0
    check(dir, "page 1 beyond EOF -> SIGBUS", touch(m + 4096, 0), SIGBUS);
    check(dir, "m[100] beyond EOF in the page is 0", m[100], 0);
    check(dir, "m[4095] is 0", m[4095], 0);
    // write() はすぐ写しに映り、写しに書いたものはすぐ read() に見える
    check(dir, "pwrite Q", pwrite(fd, "Q", 1, 10), 1);
    check(dir, "pwrite -> mapping", m[10], 'Q');
    m[20] = 'R';
    check(dir, "mapping -> pread", rd(fd, 20), 'R');
    // 終わりの先に写しで書いたものは、伸ばすと 0 (ftruncate でも、終わりより先への write でも)
    m[200] = 'X';
    check(dir, "ftruncate 4096", ftruncate(fd, 4096), 0);
    if (on_linux && strcmp(dir, "/tmp") == 0)
        printf("LINUX %s: tmpfs keeps mapped bytes beyond EOF on extension: m[200]=%d\n", dir, m[200]);
    else {
        check(dir, "ftruncate zeroes the gap (pread)", rd(fd, 200), 0);
        check(dir, "ftruncate zeroes the gap (mapping)", m[200], 0);
    }
    check(dir, "ftruncate 100", ftruncate(fd, 100), 0);
    m[300] = 'Z';
    check(dir, "pwrite W at 500", pwrite(fd, "W", 1, 500), 1);
    if (!(on_linux && strcmp(dir, "/tmp") == 0)) {
        check(dir, "write beyond EOF zeroes the gap (pread)", rd(fd, 300), 0);
        check(dir, "write beyond EOF zeroes the gap (mapping)", m[300], 0);
    }
    check(dir, "size 501", lseek(fd, 0, SEEK_END), 501);
    // 伸びればページ 1 が見える。縮めればまた SIGBUS
    check(dir, "page 1 still SIGBUS at 501", touch(m + 4096, 0), SIGBUS);
    check(dir, "ftruncate 5000", ftruncate(fd, 5000), 0);
    check(dir, "page 1 readable at 5000", touch(m + 4096, 0), 0);
    m[4100] = 'P';
    check(dir, "mapping write in page 1 -> pread", rd(fd, 4100), 'P');
    check(dir, "ftruncate 100 again", ftruncate(fd, 100), 0);
    check(dir, "page 1 SIGBUS after shrink", touch(m + 4096, 0), SIGBUS);
    check(dir, "pread beyond EOF after shrink is EOF", rd(fd, 4100), -1);
    // MAP_PRIVATE: write() は COW の前なら見え、COW のあとは見えない。写しに書いてもファイルは変わらない
    unsigned char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    check(dir, "mmap private", p != MAP_FAILED, 1);
    check(dir, "pwrite 1 at 30", pwrite(fd, "1", 1, 30), 1);
    check(dir, "private sees write() before COW", p[30], '1');
    p[31] = '2';
    check(dir, "pwrite 3 at 32", pwrite(fd, "3", 1, 32), 1);
    check(dir, "private after COW keeps its copy", p[32], 'a');
    check(dir, "private write does not reach the file", rd(fd, 31), 'a');
    check(dir, "shared mapping still sees the file", m[32], '3');
    // 穴 (別のファイルで): 写しで読んだあと write() すると映り、写しに書けば read() に見える (ブロックをとるときに同じページを使う)
    char path2[520];
    snprintf(path2, sizeof path2, "%s.holes", path);
    int fd2 = open(path2, O_RDWR | O_CREAT | O_TRUNC, 0644);
    check(dir, "ftruncate 3 pages", ftruncate(fd2, 3 * 4096), 0);
    unsigned char *q = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, 0);
    check(dir, "mmap holes", q != MAP_FAILED, 1);
    check(dir, "hole reads 0", q[8192], 0);
    check(dir, "pwrite H into the hole", pwrite(fd2, "H", 1, 8192), 1);
    check(dir, "hole write() -> mapping", q[8192], 'H');
    q[8193] = 'I';
    check(dir, "hole mapping write -> pread", rd(fd2, 8193), 'I');
    unsigned char *q2 = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd2, 4096);
    check(dir, "mmap private of a hole", q2 != MAP_FAILED, 1);
    check(dir, "private hole reads 0", q2[5], 0);
    q[4096 + 5] = 'J';
    check(dir, "shared write into a hole -> private mapping", q2[5], 'J');
    // fork: 子が共有の写しに書いたものを親が見る。子は private の写しを自分のものに
    pid_t c = fork();
    if (c == 0) {
        q[7] = 'K';
        p[33] = '4';
        _exit(q[8193] == 'I' && p[31] == '2' ? 0 : 1);
    }
    int st = -1;
    waitpid(c, &st, 0);
    check(dir, "child saw the shared and private pages", WIFEXITED(st) && WEXITSTATUS(st) == 0, 1);
    check(dir, "child's shared write -> parent", q[7], 'K');
    check(dir, "child's shared write -> pread", rd(fd2, 7), 'K');
    check(dir, "child's private write stays in the child", p[33], 'a');
    // msync: フラグのしらべ、領域のないところは ENOMEM。munmap の長さ 0 は EINVAL
    check(dir, "msync MS_SYNC|MS_ASYNC EINVAL", msync(q, 4096, MS_SYNC | MS_ASYNC) < 0 ? errno : 0, EINVAL);
    check(dir, "msync bad flag EINVAL", msync(q, 4096, 0x100) < 0 ? errno : 0, EINVAL);
    check(dir, "msync 0 ok", msync(q, 4096, 0), 0);
    check(dir, "msync MS_SYNC ok", msync(q, 3 * 4096, MS_SYNC), 0);
    check(dir, "munmap len 0 EINVAL", munmap(q, 0) < 0 ? errno : 0, EINVAL);
    // mmap のしらべの順: 位置、fd (O_PATH は EBADF)、長さ 0、許可 (読めない fd は EACCES)、種類 (ディレクトリは ENODEV)
    check(dir, "mmap off 3 EINVAL", mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 3) == MAP_FAILED ? errno : 0, EINVAL);
    check(dir, "mmap bad fd EBADF before len 0", mmap(NULL, 0, PROT_READ, MAP_PRIVATE, 999, 0) == MAP_FAILED ? errno : 0, EBADF);
    check(dir, "mmap len 0 EINVAL", mmap(NULL, 0, PROT_READ, MAP_PRIVATE, fd, 0) == MAP_FAILED ? errno : 0, EINVAL);
    int pf = open(path, O_PATH);
    check(dir, "mmap O_PATH EBADF", mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, pf, 0) == MAP_FAILED ? errno : 0, EBADF);
    close(pf);
    int wo = open(path, O_WRONLY);
    check(dir, "mmap O_WRONLY EACCES", mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, wo, 0) == MAP_FAILED ? errno : 0, EACCES);
    close(wo);
    int ro = open(path, O_RDONLY);
    check(dir, "mmap shared write on O_RDONLY EACCES", mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, ro, 0) == MAP_FAILED ? errno : 0, EACCES);
    unsigned char *pr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, ro, 0);
    check(dir, "mmap private write on O_RDONLY ok", pr != MAP_FAILED, 1);
    if (pr != MAP_FAILED) {
        pr[0] = 'y';
        check(dir, "private write on O_RDONLY does not reach the file", rd(ro, 0), 'a');
        munmap(pr, 4096);
    }
    close(ro);
    int df = open(dir, O_RDONLY | O_DIRECTORY);
    check(dir, "mmap directory ENODEV", mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, df, 0) == MAP_FAILED ? errno : 0, ENODEV);
    close(df);
    // PROT_WRITE だけの写しも読める (arm64 と x86 の Linux と同じ: 書けるページは読める)。書けば映る
    unsigned char *wo1 = mmap(NULL, 4096, PROT_WRITE, MAP_SHARED, fd2, 0);
    check(dir, "mmap PROT_WRITE shared", wo1 != MAP_FAILED, 1);
    check(dir, "PROT_WRITE shared is readable", touch(wo1 + 7, 0), 0);
    check(dir, "PROT_WRITE shared reads the file", wo1[7], 'K');
    wo1[8] = 'L';
    check(dir, "PROT_WRITE shared write -> pread", rd(fd2, 8), 'L');
    unsigned char *wo2 = mmap(NULL, 4096, PROT_WRITE, MAP_PRIVATE, fd2, 0);
    check(dir, "mmap PROT_WRITE private", wo2 != MAP_FAILED, 1);
    check(dir, "PROT_WRITE private is readable", touch(wo2 + 7, 0), 0);
    check(dir, "PROT_WRITE private reads the file", wo2[8], 'L');
    munmap(wo1, 4096);
    munmap(wo2, 4096);
    // PROT_NONE は SIGSEGV
    unsigned char *n = mmap(NULL, 4096, PROT_NONE, MAP_SHARED, fd2, 0);
    check(dir, "mmap PROT_NONE", n != MAP_FAILED, 1);
    check(dir, "PROT_NONE read SIGSEGV", touch(n, 0), SIGSEGV);
    munmap(n, 4096);
    // 外してから read() で見ても、写しで書いたものはそのまま
    munmap(m, 8192);
    munmap(p, 4096);
    munmap(q, 3 * 4096);
    munmap(q2, 4096);
    check(dir, "after munmap: K", rd(fd2, 7), 'K');
    check(dir, "after munmap: I", rd(fd2, 8193), 'I');
    close(fd);
    close(fd2);
    // 閉じて開きなおしても (ページキャッシュの表を通って) 同じ
    int fd3 = open(path2, O_RDONLY);
    check(dir, "reopen: K", rd(fd3, 7), 'K');
    close(fd3);
    unlink(path);
    unlink(path2);
}

// 動かしているプログラムは書くために開けない (ETXTBSY: ファイルのページは 1 枚なので、書けば動いているコードが変わる)。
// 自分の写しを dir に作って動かし (--sleep)、開く
static void txtbsy(const char *dir, const char *self) {
    char path[512];
    snprintf(path, sizeof path, "%s/mmapcoh.exe.%d", dir, (int)getpid());
    int in = open(self, O_RDONLY), out = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (in < 0 || out < 0) {
        printf("skip %s: cannot copy %s\n", dir, self);
        return;
    }
    static char b[65536];
    ssize_t n;
    while ((n = read(in, b, sizeof b)) > 0)
        if (write(out, b, (size_t)n) != n)
            break;
    close(in);
    close(out);
    pid_t c = fork();
    if (c == 0) {
        execl(path, path, "--sleep", (char *)NULL);
        _exit(127);
    }
    usleep(200000);
    check(dir, "open a running program for write ETXTBSY", open(path, O_WRONLY) < 0 ? errno : 0, ETXTBSY);
    check(dir, "truncate a running program ETXTBSY", truncate(path, 0) < 0 ? errno : 0, ETXTBSY);
    check(dir, "open a running program for read ok", open(path, O_RDONLY) >= 0, 1);
    kill(c, SIGKILL);
    waitpid(c, NULL, 0);
    check(dir, "open after it ended ok", open(path, O_WRONLY) >= 0, 1);
    unlink(path);
}

int main(int argc, char **argv) {
    struct stat st;
    if (argc > 1 && strcmp(argv[1], "--sleep") == 0) {
        sleep(30);
        return 0;
    }
    on_linux = stat("/proc/sys/kernel/osrelease", &st) == 0 && access("/proc/ai", F_OK) != 0;
    signal(SIGBUS, onsig);
    signal(SIGSEGV, onsig);
    for (int i = 1; i < argc; i++)
        txtbsy(argv[i], argv[0]);
    if (argc < 2) {
        run("/tmp");
    } else {
        for (int i = 1; i < argc; i++)
            run(argv[i]);
    }
    printf("mmapcoh: %d checks, %d failed\n", checks, fails);
    return fails;
}
