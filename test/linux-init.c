// test/linux-vm.sh の Linux の /init: /dev/vda (ext4) を /work に付け、/args のとおりに nobody で動かす (このマシンの
// Linux で setpriv --reuid=65534 --regid=65534 --clear-groups とするのと同じ):
//   OPS home|tmp    ロボット (/sysdiff) を、home なら /work/sd (ext4)、tmp なら /tmp/sd (tmpfs) で
//   exec ARG...     /prog を ARG... で (テストのプログラム。/work と /tmp はだれでも書ける)
// 出力は /work/out.txt、エラーは /work/err.txt、終わりの番号は /work/status。それから電源を切る
#define _GNU_SOURCE
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void put(const char *path, const char *s) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) {
        if (write(fd, s, strlen(s)) < 0)
            perror(path);
        fsync(fd);
        close(fd);
    }
}

int main(void) {
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    mount("tmpfs", "/tmp", "tmpfs", 0, "mode=1777");
    if (mount("/dev/vda", "/work", "ext4", 0, NULL) < 0) {
        perror("mount /dev/vda");
        reboot(RB_POWER_OFF);
    }
    // /args の言葉 (空白で分ける)
    char line[4096] = "", *argv[64];
    int argc = 0;
    FILE *a = fopen("/args", "r");
    if (!a || !fgets(line, sizeof line, a))
        fprintf(stderr, "init: no /args\n");
    for (char *t = strtok(line, " \n"); t && argc < 63; t = strtok(NULL, " \n"))
        argv[argc++] = t;
    argv[argc] = NULL;
    int exec_mode = argc > 0 && !strcmp(argv[0], "exec");
    const char *ops = argc > 0 ? argv[0] : "30", *fs = argc > 1 ? argv[1] : "home";
    const char *work = strcmp(fs, "tmp") ? "/work/sd" : "/tmp/sd";
    chmod("/work", 01777);
    if (!exec_mode) {
        mkdir(work, 0777);
        chmod(work, 0777);
    }
    int out = open("/work/out.txt", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    int err = open("/work/err.txt", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    pid_t pid = fork();
    if (pid == 0) {
        dup2(out, 1);
        dup2(err, 2);
        if (setgroups(0, NULL) < 0 || setgid(65534) < 0 || setuid(65534) < 0) {
            perror("init: nobody");
            _exit(126);
        }
        if (chdir("/tmp") < 0)
            _exit(126);
        if (exec_mode) {
            argv[0] = "/prog";
            execv("/prog", argv);
            perror("init: exec /prog");
            _exit(127);
        }
        execl("/sysdiff", "sysdiff", "-n", ops, work, "@/cases.txt", (char *)NULL);
        perror("init: exec /sysdiff");
        _exit(127);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    char s[32];
    snprintf(s, sizeof s, "%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    fsync(out);
    fsync(err);
    put("/work/status", s);
    sync();
    umount("/work");
    reboot(RB_POWER_OFF);
    return 0;
}
