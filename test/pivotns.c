// pivot_root で namespace の / そのものを入れかえる (bwrap、runc、systemd-nspawn の形。root で): 新しいマウントの
// namespace で tmpfs を / に、もとの / を old に (old/tmp の目印、old/proc が見える。mountinfo に / と /old)、
// umount2("/old", MNT_DETACH) で下のマウントごと外れる。runc の pivot_root(".", ".") と umount2(".", MNT_DETACH) も。
// はじめの namespace の / からは、aios は EINVAL にする (えらんだちがい: 速い道がはじめの / を見ているので。doc/aios.md)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond)                                \
  do {                                                   \
    if (cond)                                            \
      printf("ok %s\n", name);                           \
    else {                                               \
      printf("FAIL %s (errno %d)\n", name, errno);       \
      fails++;                                           \
    }                                                    \
  } while (0)

static long pr(const char *n, const char *o) { return syscall(SYS_pivot_root, n, o) < 0 ? -errno : 0; }
static int has(const char *p) { return access(p, F_OK) == 0; }
static int lines_with(const char *path, const char *s) {
  FILE *f = fopen(path, "r");
  char l[512];
  int n = 0;
  while (f && fgets(l, sizeof l, f))
    n += strstr(l, s) != NULL;
  if (f)
    fclose(f);
  return n;
}
static int run(int (*fn)(void)) {
  pid_t c = fork();
  if (c == 0)
    _exit(fn());
  int st;
  waitpid(c, &st, 0);
  return WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st);
}

static int bwrap(void) {
  if (unshare(CLONE_NEWNS))
    return 1;
  mkdir("/tmp/pvn", 0755);
  if (mount("tmpfs", "/tmp/pvn", "tmpfs", 0, NULL))
    return 2;
  mkdir("/tmp/pvn/old", 0755);
  mkdir("/tmp/pvn/proc", 0755);
  close(open("/tmp/pvn/mine", O_CREAT | O_WRONLY, 0644));
  if (chdir("/tmp/pvn"))
    return 3;
  if (pr(".", "old"))
    return 4;
  char cwd[64] = {0};
  if (!getcwd(cwd, sizeof cwd) || strcmp(cwd, "/"))
    return 5;
  if (!has("/mine") || !has("/old/tmp/pvmark") || !has("/old/proc/self"))
    return 6;
  if (mount("proc", "/proc", "proc", 0, NULL))
    return 7;
  if (lines_with("/proc/self/mountinfo", " / / ") != 1 || lines_with("/proc/self/mountinfo", " /old ") != 1)
    return 8;
  if (umount2("/old", MNT_DETACH))
    return 9;
  if (has("/old/tmp/pvmark") || lines_with("/proc/self/mountinfo", " /old") != 0)
    return 10;
  if (chdir("/old") || chdir("..") || !getcwd(cwd, sizeof cwd) || strcmp(cwd, "/"))
    return 11;
  return 0;
}

static int runc(void) {
  if (unshare(CLONE_NEWNS))
    return 1;
  mkdir("/tmp/pvr", 0755);
  if (mount("tmpfs", "/tmp/pvr", "tmpfs", 0, NULL))
    return 2;
  close(open("/tmp/pvr/mine", O_CREAT | O_WRONLY, 0644));
  if (chdir("/tmp/pvr"))
    return 3;
  if (pr(".", "."))
    return 4;
  if (umount2(".", MNT_DETACH))
    return 5;
  if (chdir("/"))
    return 6;
  return has("/mine") && !has("/tmp") ? 0 : 7;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("pivotns: not root, skipped\n");
    return 0;
  }
  close(open("/tmp/pvmark", O_CREAT | O_WRONLY, 0644));
  int r = run(bwrap);
  printf("bwrap step %d\n", r);
  CHECK("pivot-ns-root", r == 0);
  r = run(runc);
  printf("runc step %d\n", r);
  CHECK("pivot-dot-dot", r == 0);
  CHECK("outside-unchanged", has("/tmp/pvmark") && !has("/tmp/pvn/mine") && !has("/tmp/pvr/mine"));
  unlink("/tmp/pvmark");
  printf("pivotns: %d failed\n", fails);
  return fails;
}
