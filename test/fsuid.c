// fsuid と fsgid (Linux の setfsuid / setfsgid、cap_task_fix_setuid、generic_permission、inode_init_owner): set*uid で euid
// に、setfsuid で変える (4 つの id のどれかか CAP_SETUID。答えはいつも前の fsuid、-1 は変えずに問うだけ)。fsuid が 0 から
// ほかになるとファイルの capability (DAC_OVERRIDE、CHOWN、FOWNER など) が外れ、0 にもどると permitted から入る
// (euid が 1000 でも)。ファイルの許し、作ったもの (ファイル、ソケット、パイプ、memfd) の持ち主、sticky、chown、
// グループ (fsgid) は fsuid と fsgid で。access(2) は実の id。/proc/self/status の 4 つめ、dumpable、exec で euid にもどる
// 出すだけのテスト: root で (test/ldiff.sh --root fsuid)
//   aios の中で: sudo ./fsuid      aarch64-linux-gnu-gcc -O2 -static -o fsuid test/fsuid.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fsuid.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case EACCES: return "EACCES";
    case EINVAL: return "EINVAL";
    case ENOENT: return "ENOENT";
    default: snprintf(b, sizeof b, "e%d", e); return b;
    }
}

static const char *R(int r) { return r < 0 ? en(errno) : "ok"; }

// /proc/self/status の Uid と Gid の行
static void ids(const char *tag) {
    char b[4096], u[128] = "", g[128] = "";
    int fd = open("/proc/self/status", O_RDONLY);
    int n = read(fd, b, sizeof b - 1);
    close(fd);
    b[n > 0 ? n : 0] = 0;
    char *p = strstr(b, "\nUid:");
    if (p)
        sscanf(p + 1, "Uid:%127[^\n]", u);
    p = strstr(b, "\nGid:");
    if (p)
        sscanf(p + 1, "Gid:%127[^\n]", g);
    for (char *q = u; *q; q++)
        if (*q == '\t') *q = ' ';
    for (char *q = g; *q; q++)
        if (*q == '\t') *q = ' ';
    printf("%s: Uid:%s Gid:%s dumpable=%d\n", tag, u, g, prctl(PR_GET_DUMPABLE, 0, 0, 0, 0));
}

static const char *try_open(const char *p, int fl) {
    int fd = open(p, fl);
    if (fd < 0)
        return en(errno);
    close(fd);
    return "ok";
}

static void owner_of(const char *tag, int fd) {
    struct stat st;
    if (fstat(fd, &st))
        printf("  %s: fstat %s\n", tag, en(errno));
    else
        printf("  %s: %u:%u\n", tag, st.st_uid, st.st_gid);
}

static void mkfile(const char *p, int mode, uid_t u, gid_t g) {
    unlink(p);
    int fd = open(p, O_CREAT | O_WRONLY, mode);
    if (write(fd, "x", 1) < 0) {}
    close(fd);
    chown(p, u, g);
    chmod(p, mode);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "--show")) {
        ids("after exec");
        printf("  after exec open root600: %s, u600: %s\n", try_open("/tmp/fsu/root600", O_RDONLY), try_open("/tmp/fsu/u600", O_RDONLY));
        return 0;
    }
    if (getuid() != 0) {
        printf("run as root\n");
        return 2;
    }
    umask(022);
    mkdir("/tmp/fsu", 0777);
    chmod("/tmp/fsu", 0777);
    mkfile("/tmp/fsu/root600", 0600, 0, 0);
    mkfile("/tmp/fsu/u600", 0600, 1500, 1500);
    mkfile("/tmp/fsu/g40", 0040, 0, 1001);
    mkfile("/tmp/fsu-sticky", 0644, 0, 0);
    char self[256];
    ssize_t sn = readlink("/proc/self/exe", self, sizeof self - 1);
    self[sn > 0 ? sn : 0] = 0;

    printf("== root\n");
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        ids("start");
        printf("setfsuid(-1) = %d\n", setfsuid(-1));
        printf("setfsuid(1000) = %d\n", setfsuid(1000));
        ids("fsuid 1000");
        printf("  geteuid %d, setfsuid(-1) = %d\n", geteuid(), setfsuid(-1));
        unlink("/tmp/fsu/f1");
        int fd = open("/tmp/fsu/f1", O_CREAT | O_WRONLY, 0644);
        owner_of("new file", fd);
        close(fd);
        printf("  open root600: %s\n", try_open("/tmp/fsu/root600", O_RDONLY));
        printf("  access root600 R_OK (real uid): %s\n", R(access("/tmp/fsu/root600", R_OK)));
        printf("  chmod root600: %s\n", R(chmod("/tmp/fsu/root600", 0644)));
        printf("  chown f1 to 0: %s\n", R(chown("/tmp/fsu/f1", 0, -1)));
        printf("  chmod f1 (own): %s\n", R(chmod("/tmp/fsu/f1", 0600)));
        printf("  utimes root600 now: %s\n", R(utimes("/tmp/fsu/root600", NULL)));
        printf("  unlink root file in sticky /tmp: %s\n", R(unlink("/tmp/fsu-sticky")));
        int sv[2];
        if (pipe(sv) == 0) {
            owner_of("pipe", sv[0]);
            close(sv[0]);
            close(sv[1]);
        }
        int s = socket(AF_UNIX, SOCK_STREAM, 0);
        owner_of("socket", s);
        struct sockaddr_un a = {.sun_family = AF_UNIX};
        strcpy(a.sun_path, "/tmp/fsu/sock");
        unlink(a.sun_path);
        bind(s, (struct sockaddr *)&a, sizeof a);
        struct stat st;
        if (stat(a.sun_path, &st) == 0)
            printf("  bound socket file: %u:%u\n", st.st_uid, st.st_gid);
        close(s);
        int m = memfd_create("fsu", 0);
        owner_of("memfd", m);
        close(m);
        printf("setfsgid(1001) = %d\n", setfsgid(1001));
        ids("fsgid 1001");
        printf("  open g40 (group 1001): %s\n", try_open("/tmp/fsu/g40", O_RDONLY));
        printf("  chown f1 group 1001 (own, in fsgid): %s\n", R(chown("/tmp/fsu/f1", -1, 1001)));
        printf("setfsgid(1002) = %d\n", setfsgid(1002));
        printf("  open g40 (group 1002): %s\n", try_open("/tmp/fsu/g40", O_RDONLY));
        printf("setfsuid(0) = %d\n", setfsuid(0));
        printf("  open root600 again: %s, u600: %s\n", try_open("/tmp/fsu/root600", O_RDONLY), try_open("/tmp/fsu/u600", O_RDONLY));
        printf("setfsuid(2000) = %d\n", setfsuid(2000));
        printf("  setfsuid(-1) = %d\n", setfsuid(-1));
        // euid は 0 のまま: setresuid で fsuid は 0 にもどるが、外れた capability はもどらない (Linux のくせ)
        printf("  setresuid(-1, 0, -1): %s\n", R(setresuid(-1, 0, -1)));
        ids("after setresuid");
        printf("  open root600 (fsuid 0 owns it): %s\n", try_open("/tmp/fsu/root600", O_RDONLY));
        printf("  open u600 (needs CAP_DAC_OVERRIDE): %s\n", try_open("/tmp/fsu/u600", O_RDONLY));
        fflush(stdout);
        setfsuid(1000);
        char *av[] = {self, "--show", 0};
        execv(self, av);
        printf("execv %s\n", en(errno));
        _exit(1);
    }
    int st;
    waitpid(c, &st, 0);

    printf("== euid 1000 (uid and suid 0)\n");
    fflush(stdout);
    c = fork();
    if (c == 0) {
        setresuid(-1, 1000, -1);
        ids("euid 1000");
        printf("  open root600: %s\n", try_open("/tmp/fsu/root600", O_RDONLY));
        printf("setfsuid(0) = %d\n", setfsuid(0));
        ids("fsuid 0");
        printf("  open root600 (fs caps from permitted): %s, u600: %s\n", try_open("/tmp/fsu/root600", O_RDONLY), try_open("/tmp/fsu/u600", O_RDONLY));
        printf("  chown f1 to 0 (CAP_CHOWN): %s\n", R(chown("/tmp/fsu/f1", 0, -1)));
        printf("setfsuid(3000) = %d\n", setfsuid(3000));
        printf("  setfsuid(-1) = %d\n", setfsuid(-1));
        _exit(0);
    }
    waitpid(c, &st, 0);

    printf("== user 1000\n");
    fflush(stdout);
    c = fork();
    if (c == 0) {
        setgroups(0, NULL);
        setresgid(1000, 1000, 1000);
        setresuid(1000, 1000, 1000);
        ids("user");
        printf("setfsuid(0) = %d\n", setfsuid(0));
        printf("  setfsuid(-1) = %d\n", setfsuid(-1));
        printf("setfsuid(1000) = %d\n", setfsuid(1000));
        printf("setfsgid(0) = %d\n", setfsgid(0));
        printf("  setfsgid(-1) = %d\n", setfsgid(-1));
        printf("setfsuid(5000) = %d\n", setfsuid(5000));
        ids("user end");
        _exit(0);
    }
    waitpid(c, &st, 0);

    printf("== dumpable\n");
    fflush(stdout);
    c = fork();
    if (c == 0) {
        setfsgid(1000);
        ids("setfsgid 1000");
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
        setfsgid(0);
        setfsuid(1000);
        ids("setfsuid 1000");
        _exit(0);
    }
    waitpid(c, &st, 0);
    unlink("/tmp/fsu/root600");
    unlink("/tmp/fsu/u600");
    unlink("/tmp/fsu/g40");
    unlink("/tmp/fsu/f1");
    unlink("/tmp/fsu/sock");
    unlink("/tmp/fsu-sticky");
    rmdir("/tmp/fsu");
    return 0;
}
