// execveat (Linux の do_execveat_common、alloc_bprm、do_open_execat、begin_new_exec): dirfd からの名前、AT_EMPTY_PATH の
// fd (O_PATH も)、AT_SYMLINK_NOFOLLOW (ELOOP)、AT_EXECVE_CHECK (たしかめるだけ。スクリプトも中を見ない)、まちがいの順
// (空の名前の ENOENT、知らない flags の EINVAL、EBADF、ENOTDIR、ふつうのファイルでない fd の EACCES)。exec したあとの
// comm (fd からの名前の終わり。名前が空ならファイルの名前)、AT_EXECFN (/dev/fd/N か /dev/fd/N/NAME)、#! のスクリプトに
// わたる名前、O_CLOEXEC の dirfd の #! は ENOENT
// 出すだけのテスト: test/ldiff.sh execveat
//   aios の中で: ./execveat      aarch64-linux-gnu-gcc -O2 -static -o execveat test/execveat.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef AT_EXECVE_CHECK
#define AT_EXECVE_CHECK 0x10000
#endif

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "ok";
    case ENOENT: return "ENOENT";
    case EINVAL: return "EINVAL";
    case EBADF: return "EBADF";
    case ENOTDIR: return "ENOTDIR";
    case EACCES: return "EACCES";
    case ELOOP: return "ELOOP";
    case EFAULT: return "EFAULT";
    case ENOEXEC: return "ENOEXEC";
    default: snprintf(b, sizeof b, "e%d", e); return b;
    }
}

static char buf[65536];

static void copy(const char *from, const char *to, int mode) {
    int a = open(from, O_RDONLY), b = open(to, O_WRONLY | O_CREAT | O_TRUNC, mode);
    long n;
    while ((n = read(a, buf, sizeof buf)) > 0)
        if (write(b, buf, n) != n)
            break;
    close(a);
    close(b);
    chmod(to, mode);
}

static void put(const char *p, const char *s, int mode) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, mode);
    if (write(fd, s, strlen(s)) < 0) {}
    close(fd);
    chmod(p, mode);
}

static const char *files[] = {"prog", "lnk", "plain", "script", "noexec"};

static void clean(void) {
    for (unsigned i = 0; i < sizeof files / sizeof *files; i++) {
        char p[64];
        snprintf(p, sizeof p, "/tmp/xa/%s", files[i]);
        unlink(p);
    }
    rmdir("/tmp/xa");
}

// 子で fd を 3 からにして /tmp/xa で execveat。exec できれば新しいプログラム (--show) が出す
static void run(const char *what, int how) {
    printf("== %s\n", what);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        syscall(SYS_close_range, 3, ~0u, 0, 0);
        if (chdir("/tmp/xa"))
            _exit(2);
        char *av[] = {"argv0", "--show", NULL}, *ev[] = {"A=1", NULL};
        int fd = -1, p[2];
        long r = 0;
        switch (how) {
        case 0: r = syscall(SYS_execveat, AT_FDCWD, "/tmp/xa/prog", av, ev, 0); break;
        case 1:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "prog", av, ev, 0);
            break;
        case 2:
            fd = open("/tmp/xa/prog", O_RDONLY);
            r = syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH);
            break;
        case 3:
            fd = open("/tmp/xa/prog", O_PATH);
            r = syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH);
            break;
        case 4:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "lnk", av, ev, AT_SYMLINK_NOFOLLOW);
            break;
        case 5:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "lnk", av, ev, 0);
            break;
        case 6:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "", av, ev, 0);
            break;
        case 7:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "prog", av, ev, 0x8000);
            break;
        case 8:
            fd = open("/tmp/xa/plain", O_RDONLY);
            r = syscall(SYS_execveat, fd, "prog", av, ev, 0);
            break;
        case 9: r = syscall(SYS_execveat, 99, "prog", av, ev, 0); break;
        case 10:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH);
            break;
        case 11:
            fd = open("/tmp/xa/prog", O_RDONLY);
            r = syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH | AT_EXECVE_CHECK);
            break;
        case 12:
            fd = open("/tmp/xa/noexec", O_RDONLY);
            r = syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH | AT_EXECVE_CHECK);
            break;
        case 13:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "script", av, ev, AT_EXECVE_CHECK);
            break;
        case 14:
            if (pipe(p))
                _exit(3);
            r = syscall(SYS_execveat, p[0], "", av, ev, AT_EMPTY_PATH);
            break;
        case 15:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "script", av, ev, 0);
            break;
        case 16:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
            r = syscall(SYS_execveat, fd, "script", av, ev, 0);
            break;
        case 17: r = syscall(SYS_execveat, AT_FDCWD, "prog", av, ev, 0); break;
        case 18: r = syscall(SYS_execveat, AT_FDCWD, "missing", av, ev, 0); break;
        case 19: r = syscall(SYS_execveat, AT_FDCWD, (char *)8, av, ev, 0); break;
        case 20:
            fd = open("/tmp/xa/prog", O_RDONLY);
            r = syscall(SYS_execveat, fd, "/tmp/xa/prog", av, ev, AT_EMPTY_PATH);
            break;
        case 21:
            fd = open("/tmp/xa/plain", O_RDONLY);
            r = syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH);
            break;
        case 22:
            fd = open("/tmp/xa", O_RDONLY | O_DIRECTORY);
            r = syscall(SYS_execveat, fd, "script", (char **)8, ev, 0);
            break;
        }
        printf("  returned %ld: %s\n", r, r ? en(errno) : "ok");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st))
        printf("  status %x\n", st);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "--show")) {
        char comm[32] = "";
        int fd = open("/proc/self/comm", O_RDONLY);
        int n = read(fd, comm, sizeof comm - 1);
        comm[n > 0 ? n - 1 : 0] = 0;
        printf("  exec'd: comm=%s execfn=%s argc=%d", comm, (char *)getauxval(AT_EXECFN), argc);
        for (int i = 0; i < argc; i++)
            printf(" [%s]", argv[i]);
        printf("\n");
        return 0;
    }
    clean();
    mkdir("/tmp/xa", 0755);
    copy("/proc/self/exe", "/tmp/xa/prog", 0755);
    if (symlink("prog", "/tmp/xa/lnk")) {}
    put("/tmp/xa/plain", "plain\n", 0644);
    put("/tmp/xa/noexec", "\x7f" "ELF", 0644);
    put("/tmp/xa/script", "#!/tmp/xa/prog --show\n", 0755);

    run("AT_FDCWD + absolute", 0);
    run("dirfd + name", 1);
    run("fd AT_EMPTY_PATH", 2);
    run("O_PATH fd AT_EMPTY_PATH", 3);
    run("symlink with AT_SYMLINK_NOFOLLOW", 4);
    run("symlink followed", 5);
    run("empty name without AT_EMPTY_PATH", 6);
    run("unknown flag", 7);
    run("dirfd is a file", 8);
    run("bad dirfd", 9);
    run("directory fd AT_EMPTY_PATH", 10);
    run("AT_EXECVE_CHECK executable", 11);
    run("AT_EXECVE_CHECK not executable", 12);
    run("AT_EXECVE_CHECK script", 13);
    run("pipe fd AT_EMPTY_PATH", 14);
    run("dirfd + #! script", 15);
    run("O_CLOEXEC dirfd + #! script", 16);
    run("AT_FDCWD + relative", 17);
    run("missing", 18);
    run("bad name pointer", 19);
    run("AT_EMPTY_PATH with a name", 20);
    run("fd of a file without x", 21);
    run("bad argv after open", 22);
    clean();
    return 0;
}
