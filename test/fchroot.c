// fchroot (Linux 7.4) が Linux と同じか (root で): たしかめる順 (flags、fd、ディレクトリか、実行の許可、CAP_SYS_CHROOT)、
// O_PATH の口でもよい、chroot のあとは / がそこ、前に開いた口で外へもどれる。FD_FAILFS_ROOT はなにも見えない根 (絶対パスは
// みな EOPNOTSUPP、cwd からの相対パスはそのまま)、root でなくても no_new_privs で (fs を分けたスレッドがいれば EINVAL、
// chroot の中からは EPERM)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define FD_FAILFS_ROOT (-10004)
#define D "/tmp/fcr"

static long fcr(int fd, unsigned fl) { return syscall(472, fd, fl) ? -errno : 0; }
static long op(const char *p, int fl) {
  int fd = open(p, fl);
  if (fd < 0)
    return -errno;
  close(fd);
  return 0;
}
static long st(const char *p) {
  struct stat s;
  return stat(p, &s) ? -errno : 0;
}
static int run(int (*fn)(void)) {
  pid_t c = fork();
  if (c == 0)
    _exit(fn());
  int s;
  waitpid(c, &s, 0);
  return WIFEXITED(s) ? WEXITSTATUS(s) : 100 + WTERMSIG(s);
}

static const char *cwd(void) {
  static char b[128];
  return getcwd(b, sizeof b) ? b : strerror(errno);
}

static int as_root(void) {
  int out = open("/", O_RDONLY | O_DIRECTORY);
  int in = open(D, O_PATH | O_DIRECTORY);
  if (chdir("/"))
    return 1;
  long r = fcr(in, 0);
  printf("root: fchroot (O_PATH) %ld, /mark %ld, /tmp %ld, cwd %s", r, st("/mark"), st("/tmp"), cwd());
  // 前に開いた口で外へ
  long r2 = fcr(out, 0);
  printf(", back by the old fd %ld, /tmp/fcr/mark %ld, cwd %s\n", r2, st(D "/mark"), cwd());
  return 0;
}

static int failfs_root(void) {
  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  if (chdir(D))
    return 1;
  long r = fcr(FD_FAILFS_ROOT, 0);
  char l[64] = {0};
  long n = readlinkat(proc, "self/root", l, sizeof l - 1);
  printf("failfs as root: %ld, / %ld, /tmp %ld, stat / %ld, relative mark %ld, .. %ld, root link %ld %s, cwd %s\n", r, op("/", O_RDONLY), op("/tmp", O_RDONLY), st("/"),
         st("mark"), st(".."), n < 0 ? (long)-errno : n, n > 0 ? l : "", cwd());
  // / で始まるリンクも、根から
  symlink("/tmp", "abs");
  long a = st("abs/fcr/mark");
  unlink("abs");
  long c = chroot("/tmp") ? -errno : 0;
  long d = chdir("/") ? -errno : 0;
  long u = chdir("..") ? -errno : 0;
  printf("  through a link to /tmp: %ld, chroot /tmp %ld, chdir / %ld, chdir .. %ld, cwd %s\n", a, c, d, u, cwd());
  return 0;
}

static int nobody(void) {
  if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
    return 1;
  int in = open(D, O_RDONLY | O_DIRECTORY), locked = open(D "/locked", O_PATH | O_DIRECTORY);
  printf("nobody: dir %ld, no exec %ld, failfs without nnp %ld", fcr(in, 0), fcr(locked, 0), fcr(FD_FAILFS_ROOT, 0));
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  long r = fcr(FD_FAILFS_ROOT, 0);
  printf(", with nnp %ld, / %ld, again %ld\n", r, st("/"), fcr(FD_FAILFS_ROOT, 0));
  return 0;
}

static void *idle(void *a) {
  pause();
  return a;
}
static int shared_fs(void) {
  if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
    return 1;
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  pthread_t t;
  pthread_create(&t, NULL, idle, NULL);
  printf("nobody with a thread: failfs %ld\n", fcr(FD_FAILFS_ROOT, 0));
  return 0;
}

static int chrooted_nobody(void) {
  if (chroot(D))
    return 1;
  if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
    return 2;
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  printf("nobody in a chroot: failfs %ld\n", fcr(FD_FAILFS_ROOT, 0));
  return 0;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("fchroot: not root, skipped\n");
    return 0;
  }
  mkdir(D, 0755);
  mkdir(D "/locked", 0700);
  close(open(D "/mark", O_CREAT | O_WRONLY, 0644));
  int f = open(D "/mark", O_RDONLY);
  printf("checks: flags %ld, bad fd %ld, a file %ld, flags with failfs %ld\n", fcr(open("/", O_RDONLY), 1), fcr(999, 0), fcr(f, 0), fcr(FD_FAILFS_ROOT, 1));
  int r[5] = {run(as_root), run(failfs_root), run(nobody), run(shared_fs), run(chrooted_nobody)};
  printf("children %d %d %d %d %d\n", r[0], r[1], r[2], r[3], r[4]);
  unlink(D "/mark");
  rmdir(D "/locked");
  rmdir(D);
  return 0;
}
