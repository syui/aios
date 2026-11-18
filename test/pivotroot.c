// pivot_root が Linux と同じか: root でなければ EPERM。新しいマウントの namespace で、chroot したマウント (R) の中の
// マウント (N) を新しい / に、もとの R を put_old に。たしかめの順 (ない ENOENT、ディレクトリでない ENOTDIR、N か put_old が
// R のもの EBUSY、マウントでない・put_old が N の下にない EINVAL)。そのあと / と cwd は N、mountinfo は / と /old、
// /old を外せる。runc の pivot_root(".", ".") と umount2(".", MNT_DETACH) も
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

static long pr(const char *n, const char *o) { return syscall(SYS_pivot_root, n, o) < 0 ? -errno : 0; }
static int has(const char *p) { return access(p, F_OK) == 0; }
static void touch(const char *p) { close(open(p, O_CREAT | O_WRONLY | O_CLOEXEC, 0644)); }
static void mounts(void) {
  FILE *f = fopen("/proc/self/mountinfo", "r");
  char l[512], at[256], fs[64];
  printf("  mountinfo:");
  while (f && fgets(l, sizeof l, f)) {
    char *d = strstr(l, " - ");
    if (sscanf(l, "%*s %*s %*s %*s %255s", at) == 1 && d && sscanf(d + 3, "%63s", fs) == 1)
      printf(" %s(%s)", at, fs);
  }
  printf("\n");
  if (f)
    fclose(f);
}
static void setup(const char *r) {
  char p[128];
  mkdir(r, 0755);
  mount("tmpfs", r, "tmpfs", 0, NULL);
  snprintf(p, sizeof p, "%s/new", r);
  mkdir(p, 0755);
  snprintf(p, sizeof p, "%s/plain", r);
  mkdir(p, 0755);
  snprintf(p, sizeof p, "%s/file", r);
  touch(p);
  snprintf(p, sizeof p, "%s/in-r", r);
  touch(p);
  snprintf(p, sizeof p, "%s/new", r);
  mount("tmpfs", p, "tmpfs", 0, NULL);
  snprintf(p, sizeof p, "%s/new/old", r);
  mkdir(p, 0755);
  snprintf(p, sizeof p, "%s/new/in-new", r);
  touch(p);
}
static int wait_kid(pid_t c) {
  int st;
  waitpid(c, &st, 0);
  return WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("not root (pivot_root %ld)\n", pr("/tmp", "/tmp"));
    return 0;
  }
  pid_t c = fork();
  if (c == 0) {
    if (unshare(CLONE_NEWNS))
      _exit(1);
    printf("private %ld\n", mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) ? -(long)errno : 0L);
    setup("/tmp/pr");
    if (chroot("/tmp/pr") || chdir("/"))
      _exit(2);
    printf("checks (no such %ld, not a dir %ld, new in root %ld, old in root %ld, new not a mount %ld, old not under new %ld, old not a dir %ld)\n", pr("/nope", "/new/old"),
           pr("/file", "/new/old"), pr("/plain", "/new/old"), pr("/new", "/plain"), pr("/new/old", "/new/old"), pr("/new", "/"), pr("/new", "/new/in-new"));
    long r = pr("/new", "/new/old");
    char cwd[64] = {0};
    if (!getcwd(cwd, sizeof cwd))
      strcpy(cwd, "?");
    printf("pivot %ld (cwd %s, in-new %d, old/in-r %d, old/new %d, old/new/in-new %d)\n", r, cwd, has("/in-new"), has("/old/in-r"), has("/old/new"), has("/old/new/in-new"));
    mounts();
    r = umount2("/old", MNT_DETACH) ? -errno : 0;
    printf("umount old %ld (old/in-r %d)\n", r, has("/old/in-r"));
    mounts();
    _exit(0);
  }
  printf("first child %d\n", wait_kid(c));
  c = fork();
  if (c == 0) {
    if (unshare(CLONE_NEWNS) || mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL))
      _exit(1);
    setup("/tmp/pr2");
    if (chroot("/tmp/pr2") || chdir("/new"))
      _exit(2);
    long r = pr(".", ".");
    long u = umount2(".", MNT_DETACH) ? -errno : 0;
    long d = chdir("/") ? -errno : 0;
    printf("runc (pivot %ld, umount %ld, chdir %ld, in-new %d, old %d, in-r %d)\n", r, u, d, has("/in-new"), has("/old"), has("/in-r"));
    mounts();
    _exit(0);
  }
  printf("second child %d\n", wait_kid(c));
  printf("outside (pr %d, pr2 %d)\n", has("/tmp/pr/in-r"), has("/tmp/pr2/in-r"));
  return 0;
}
