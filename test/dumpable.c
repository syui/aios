// dumpable (Linux の commit_creds の suid_dumpable、begin_new_exec、task_dump_owner、__ptrace_may_access):
// euid / egid が変わると dumpable が fs.suid_dumpable (既定 0) に、exec で 1 にもどる (exec の前の euid と uid が
// ちがう、読めないプログラム、setuid で euid が変わったときは suid_dumpable)。dumpable が 1 でないプロセスの
// /proc/PID の中は root のもの (dr-xr-xr-x のディレクトリはのぞく)、自分の /proc/self/fd は入れる、ほかの人
// (同じ uid でも) からは environ、cwd、fd、fdinfo、ns、maps、pidfd_getfd が見られない。持ち主の uid と gid は
// 0 か 1000 で出す (uid 1000 の人に、なる)。
// 出すだけのテスト: root で (test/ldiff.sh --root dumpable)
//   aios の中で: sudo ./dumpable      aarch64-linux-gnu-gcc -O2 -static -o dumpable test/dumpable.c
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case EINVAL: return "EINVAL";
    case EACCES: return "EACCES";
    case ENOENT: return "ENOENT";
    case EBADF: return "EBADF";
    case ESRCH: return "ESRCH";
    default: snprintf(b, sizeof b, "e%d", e); return b;
    }
}

static void dump(const char *tag) {
    printf("%s: dumpable=%d\n", tag, prctl(PR_GET_DUMPABLE, 0, 0, 0, 0));
}

static const char *ENTRIES[] = {".", "status", "environ", "comm", "cmdline", "maps", "loginuid", "sessionid", "uid_map",
                                "mountinfo", "fd", "fdinfo", "ns", "task", "cwd", "exe", "fd/50", "fdinfo/50", "ns/net", 0};

// /proc/self の中の持ち主 (modes なら mode も 1 行ずつ)
static void owners(const char *tag, int modes) {
    char p[64];
    if (!modes)
        printf("%s: owners", tag);
    for (int i = 0; ENTRIES[i]; i++) {
        struct stat st;
        snprintf(p, sizeof p, "/proc/self/%s", ENTRIES[i]);
        int r = lstat(p, &st);
        if (modes) {
            if (r)
                printf("  %s %s\n", ENTRIES[i], en(errno));
            else
                printf("  %s %o %u:%u\n", ENTRIES[i], st.st_mode, st.st_uid, st.st_gid);
        } else if (r)
            printf(" %s=%s", ENTRIES[i], en(errno));
        else
            printf(" %s=%u:%u", ENTRIES[i], st.st_uid, st.st_gid);
    }
    if (!modes)
        printf("\n");
}

static const char *try_open(const char *p, int fl) {
    int fd = open(p, fl);
    if (fd < 0)
        return en(errno);
    close(fd);
    return "ok";
}

static const char *try_link(const char *p) {
    char b[256];
    return readlink(p, b, sizeof b) < 0 ? en(errno) : "ok";
}

static const char *try_dir(const char *p) {
    DIR *d = opendir(p);
    if (!d)
        return en(errno);
    int n = 0;
    while (readdir(d))
        n++;
    closedir(d);
    return n > 2 ? "ok" : "empty";
}

// 自分の /proc/self の口
static void self_access(const char *tag) {
    printf("%s: loginuid-w=%s environ=%s uid_map-w=%s fd=%s fd/50=%s fdinfo/50=%s ns/net=%s status=%s maps=%s\n", tag,
           try_open("/proc/self/loginuid", O_WRONLY), try_open("/proc/self/environ", O_RDONLY),
           try_open("/proc/self/uid_map", O_WRONLY), try_dir("/proc/self/fd"), try_link("/proc/self/fd/50"),
           try_open("/proc/self/fdinfo/50", O_RDONLY), try_link("/proc/self/ns/net"),
           try_open("/proc/self/status", O_RDONLY), try_open("/proc/self/maps", O_RDONLY));
}

// ほかのプロセス (pid) の /proc/PID の口
static void other_access(const char *tag, pid_t pid) {
    char p[9][64];
    snprintf(p[0], 64, "/proc/%d/environ", pid);
    snprintf(p[1], 64, "/proc/%d/status", pid);
    snprintf(p[2], 64, "/proc/%d/cwd", pid);
    snprintf(p[3], 64, "/proc/%d/fd", pid);
    snprintf(p[4], 64, "/proc/%d/fdinfo/50", pid);
    snprintf(p[5], 64, "/proc/%d/ns/net", pid);
    snprintf(p[6], 64, "/proc/%d/maps", pid);
    snprintf(p[7], 64, "/proc/%d/exe", pid);
    snprintf(p[8], 64, "/proc/%d/fdinfo", pid);
    const char *gf = "ok";
    int pfd = syscall(SYS_pidfd_open, pid, 0);
    if (pfd < 0)
        gf = en(errno);
    else {
        int fd = syscall(SYS_pidfd_getfd, pfd, 50, 0);
        if (fd < 0)
            gf = en(errno);
        else
            close(fd);
        close(pfd);
    }
    printf("%s: environ=%s status=%s cwd=%s exe=%s fd=%s fdinfo=%s fdinfo/50=%s ns/net=%s maps=%s getfd=%s\n", tag,
           try_open(p[0], O_RDONLY), try_open(p[1], O_RDONLY), try_link(p[2]), try_link(p[7]), try_dir(p[3]),
           try_dir(p[8]), try_open(p[4], O_RDONLY), try_link(p[5]), try_open(p[6], O_RDONLY), gf);
}

static void become_user(void) {
    if (setgroups(0, NULL) || setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) {
        perror("become_user");
        _exit(1);
    }
}

static int wait_child(pid_t c) {
    int st;
    waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static void copy(const char *from, const char *to, int mode) {
    char b[65536];
    int in = open(from, O_RDONLY), out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0700);
    ssize_t n;
    if (in < 0 || out < 0) {
        perror("copy");
        exit(1);
    }
    while ((n = read(in, b, sizeof b)) > 0)
        if (write(out, b, n) != n) {
            perror("copy write");
            exit(1);
        }
    close(in);
    close(out);
    chmod(to, mode);
}

static const char *suid_path = "/proc/sys/fs/suid_dumpable";

static void set_suid_dumpable(const char *v) {
    int fd = open(suid_path, O_WRONLY);
    if (fd < 0 || write(fd, v, strlen(v)) < 0)
        printf("suid_dumpable %s: %s\n", v, en(errno));
    if (fd >= 0)
        close(fd);
}

static void show_suid_dumpable(void) {
    char b[16] = "";
    int fd = open(suid_path, O_RDONLY);
    if (fd < 0) {
        printf("suid_dumpable: %s\n", en(errno));
        return;
    }
    int n = read(fd, b, sizeof b - 1);
    close(fd);
    b[n > 0 ? n : 0] = 0;
    printf("suid_dumpable: %s", b);
}

// exec のあと: execv(prog, "--show") で、新しいプログラムが dumpable と id を出す
static void exec_case(const char *tag, const char *prog, void (*before)(void)) {
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        if (before)
            before();
        printf("%s: before exec dumpable=%d\n", tag, prctl(PR_GET_DUMPABLE, 0, 0, 0, 0));
        fflush(stdout);
        char *argv[] = {(char *)prog, "--show", (char *)tag, 0};
        execv(prog, argv);
        printf("%s: execv %s\n", tag, en(errno));
        _exit(1);
    }
    wait_child(c);
}

static void as_user(void) { become_user(); }
static void euid_user(void) { setresuid(-1, 1000, -1); }
static void uid_user_euid_root(void) { setresuid(1000, 0, 0); }
static void dumpable0(void) { prctl(PR_SET_DUMPABLE, 0, 0, 0, 0); }

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 2 && !strcmp(argv[1], "--show")) {
        printf("%s: after exec uid=%d euid=%d dumpable=%d status=%s\n", argv[2], getuid(), geteuid(),
               prctl(PR_GET_DUMPABLE, 0, 0, 0, 0), try_open("/proc/self/status", O_RDONLY));
        int fd = open("/proc/self/environ", O_RDONLY);
        printf("%s: environ=%s\n", argv[2], fd < 0 ? en(errno) : "ok");
        return 0;
    }
    if (getuid() != 0) {
        printf("run as root\n");
        return 2;
    }
    // fd 50 は /dev/null (読むだけ)。0 から 2 のほかは閉じる (動かす道具でちがう)
    int nf = open("/dev/null", O_RDONLY);
    dup2(nf, 50);
    close(nf);
    syscall(SYS_close_range, 3, 49, 0);
    syscall(SYS_close_range, 51, ~0U, 0);
    chdir("/tmp");
    show_suid_dumpable();

    // 1. 自分の資格を変える
    printf("== self\n");
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        dump("root");
        owners("root", 1);
        setgroups(0, NULL);
        dump("setgroups");
        setresgid(1000, 1000, 1000);
        dump("setresgid 1000");
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
        dump("set 1");
        setresuid(1000, 1000, 1000);
        dump("setresuid 1000");
        owners("user", 0);
        self_access("user");
        printf("set 2: %s\n", prctl(PR_SET_DUMPABLE, 2, 0, 0, 0) ? en(errno) : "ok");
        printf("set 1: %s\n", prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) ? en(errno) : "ok");
        dump("user");
        owners("user dumpable", 0);
        self_access("user dumpable");
        prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
        owners("user set 0", 0);
        _exit(0);
    }
    wait_child(c);

    // 2. 変わらない・もどすだけのとき
    printf("== ids\n");
    fflush(stdout);
    c = fork();
    if (c == 0) {
        setuid(0);
        dump("setuid 0 (same)");
        setresuid(0, 0, 0);
        dump("setresuid 0 0 0 (same)");
        setreuid(-1, 0);
        dump("setreuid -1 0 (same)");
        setresuid(1000, -1, -1);
        dump("ruid 1000");
        setresuid(-1, -1, 1000);
        dump("suid 1000");
        setresuid(0, -1, 0);
        dump("back");
        setresuid(-1, 1000, -1);
        dump("euid 1000");
        setresuid(-1, 0, -1);
        dump("euid 0 again");
        owners("euid 0 again", 0);
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
        setresgid(-1, 1000, -1);
        dump("egid 1000");
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
        gid_t g[] = {1000, 1001};
        setgroups(2, g);
        dump("setgroups 1000 1001");
        _exit(0);
    }
    wait_child(c);

    // 3. ほかのプロセスから: X (uid 1000、dumpable 0 → 1) を Y (uid 1000) が見る
    printf("== other\n");
    int a[2], b[2];
    pipe(a);
    pipe(b);
    fflush(stdout);
    pid_t x = fork();
    if (x == 0) {
        become_user();
        char ch;
        write(a[1], "r", 1);
        read(b[0], &ch, 1);
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
        write(a[1], "r", 1);
        read(b[0], &ch, 1);
        _exit(0);
    }
    char ch;
    read(a[0], &ch, 1);
    pid_t y = fork();
    if (y == 0) {
        become_user();
        other_access("user -> user (not dumpable)", x);
        _exit(0);
    }
    wait_child(y);
    write(b[1], "g", 1);
    read(a[0], &ch, 1);
    y = fork();
    if (y == 0) {
        become_user();
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
        other_access("user -> user (dumpable)", x);
        other_access("user -> root", getppid());
        _exit(0);
    }
    wait_child(y);
    other_access("root -> user", x);
    write(b[1], "g", 1);
    wait_child(x);

    // 4. exec のあと
    printf("== exec\n");
    char self[256];
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    self[n > 0 ? n : 0] = 0;
    copy(self, "/tmp/dmp", 0755);
    copy(self, "/tmp/dmp-x", 0711);
    copy(self, "/tmp/dmp-s", 0755);
    chown("/tmp/dmp-s", 1000, 1000);
    chmod("/tmp/dmp-s", 04755);
    exec_case("user readable", "/tmp/dmp", as_user);
    exec_case("user unreadable", "/tmp/dmp-x", as_user);
    exec_case("root unreadable", "/tmp/dmp-x", NULL);
    exec_case("root set 0", "/tmp/dmp", dumpable0);
    exec_case("euid 1000", "/tmp/dmp", euid_user);
    exec_case("root setuid 1000", "/tmp/dmp-s", NULL);
    exec_case("uid 1000 euid 0 setuid 1000", "/tmp/dmp-s", uid_user_euid_root);

    // 5. fs.suid_dumpable = 2 (root の権利で作る)
    printf("== suid_dumpable 2\n");
    set_suid_dumpable("2");
    show_suid_dumpable();
    fflush(stdout);
    c = fork();
    if (c == 0) {
        setresuid(-1, 1000, -1);
        dump("euid 1000");
        owners("euid 1000", 0);
        _exit(0);
    }
    wait_child(c);
    exec_case("user unreadable", "/tmp/dmp-x", as_user);
    set_suid_dumpable("3");
    set_suid_dumpable("0");
    show_suid_dumpable();
    unlink("/tmp/dmp");
    unlink("/tmp/dmp-x");
    unlink("/tmp/dmp-s");
    return 0;
}
