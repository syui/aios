// setns が Linux と同じか: /proc/PID/ns/X を開いた fd (nsfs: 0444 のふつうのファイルで st_ino が番号、/proc/self/fd/N は
// "uts:[N]") と pidfd で namespace に入る。fd の種類と flags のちがいは EINVAL、pidfd の flags は 0 や知らないものが EINVAL、
// 自分の user にもう一度は EINVAL、ほかは CAP_SYS_ADMIN (mnt は CAP_SYS_CHROOT も) がなければ EPERM。mnt に入ると cwd と
// root はその / に。pid は自分の (いまの) ものか、その下のものだけ (上は EINVAL)。root でなければ子の user の namespace から。
// 手本の Linux の sit0 (CONFIG_IPV6_SIT) は数えない
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static long sn(int fd, int t) { return setns(fd, t) < 0 ? -errno : 0; }
static void lnk(const char *p, char *o) {
  ssize_t n = readlink(p, o, 63);
  o[n < 0 ? 0 : n] = 0;
}
static int same(const char *a, const char *b) {
  char x[64], y[64];
  lnk(a, x);
  lnk(b, y);
  return x[0] && !strcmp(x, y);
}
static int nifs(void) {
  struct if_nameindex *i = if_nameindex(), *j = i;
  int n = 0;
  for (; j && j->if_index; j++)
    n += strcmp(j->if_name, "sit0") != 0;
  if_freenameindex(i);
  return n;
}
static int host_is(const char *h) {
  char b[65] = {0};
  gethostname(b, 64);
  return !strcmp(b, h);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char *k[] = {"uts", "ipc", "net", "mnt", "pid", "user"};
  int fd[6];
  for (int i = 0; i < 6; i++) {
    char p[64], l[64], f[64], fl[64], want[64];
    struct stat st;
    snprintf(p, sizeof p, "/proc/self/ns/%s", k[i]);
    lnk(p, l);
    fd[i] = open(p, O_RDONLY | O_CLOEXEC);
    int ok = fd[i] >= 0 && fstat(fd[i], &st) == 0;
    snprintf(f, sizeof f, "/proc/self/fd/%d", fd[i]);
    lnk(f, fl);
    snprintf(want, sizeof want, "%s:[%lu]", k[i], ok ? (unsigned long)st.st_ino : 0);
    printf("%s (open %s, ino %d, mode %o, nlink %d, fd link %d)\n", k[i], ok ? "ok" : strerror(errno), ok && !strcmp(l, want), ok ? st.st_mode : 0, ok ? (int)st.st_nlink : 0,
           !strcmp(fl, l));
  }
  int reg = open("/proc/self/stat", O_RDONLY);
  int me = syscall(SYS_pidfd_open, getpid(), 0);
  printf("checks (bad fd %ld, not a namespace %ld, wrong type %ld, pidfd no flags %ld, pidfd bad flag %ld)\n", sn(-1, 0), sn(reg, 0), sn(fd[0], CLONE_NEWNET), sn(me, 0),
         sn(me, 1));
  printf("own (uts %ld, ipc %ld, net %ld, pid %ld, user %ld, pidfd uts %ld, pidfd user %ld)\n", sn(fd[0], CLONE_NEWUTS), sn(fd[1], 0), sn(fd[2], 0), sn(fd[4], CLONE_NEWPID),
         sn(fd[5], CLONE_NEWUSER), sn(me, CLONE_NEWUTS), sn(me, CLONE_NEWUSER));
  if (chdir("/tmp"))
    return 1;
  long r = sn(fd[3], CLONE_NEWNS);
  char cwd[64] = {0};
  if (!getcwd(cwd, sizeof cwd))
    strcpy(cwd, "?");
  printf("own mnt %ld (cwd %s)\n", r, cwd);
  if (chdir("/"))
    return 1;

  int p[2], q[2];
  if (pipe(p) || pipe(q))
    return 1;
  int root = getuid() == 0;
  pid_t c = fork();
  if (c == 0) {
    close(p[0]);
    close(q[1]);
    char y = unshare(CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | (root ? 0 : CLONE_NEWUSER)) == 0 && sethostname("inner", 5) == 0 ? 'y' : 'n';
    if (write(p[1], &y, 1) != 1 || read(q[0], &y, 1) < 0)
      _exit(1);
    _exit(0);
  }
  close(p[1]);
  close(q[0]);
  char y = 0;
  if (read(p[0], &y, 1) != 1)
    return 1;
  printf("child made its namespaces: %c\n", y);
  char cp[64];
  int cf[6];
  for (int i = 0; i < 6; i++) {
    snprintf(cp, sizeof cp, "/proc/%d/ns/%s", c, k[i]);
    cf[i] = open(cp, O_RDONLY | O_CLOEXEC);
  }
  int kid = syscall(SYS_pidfd_open, c, 0);
  char cnet[64], cipc[64], cuser[64];
  snprintf(cnet, sizeof cnet, "/proc/%d/ns/net", c);
  snprintf(cipc, sizeof cipc, "/proc/%d/ns/ipc", c);
  snprintf(cuser, sizeof cuser, "/proc/%d/ns/user", c);
  if (!root) {
    r = sn(cf[5], CLONE_NEWUSER);
    printf("child user by fd %ld (same %d)\n", r, same("/proc/self/ns/user", cuser));
  }
  r = sn(cf[0], CLONE_NEWUTS);
  printf("child uts by fd %ld (inner %d)\n", r, host_is("inner"));
  r = sn(fd[0], 0);
  printf("back to my uts %ld (inner %d)\n", r, host_is("inner"));
  r = sn(kid, CLONE_NEWUTS | CLONE_NEWNET | CLONE_NEWIPC);
  printf("child by pidfd %ld (inner %d, net %d, ipc %d, links %d)\n", r, host_is("inner"), same("/proc/self/ns/net", cnet), same("/proc/self/ns/ipc", cipc), nifs());
  r = sn(fd[2], CLONE_NEWNET);
  long r2 = sn(fd[1], CLONE_NEWIPC);
  printf("back to my net %ld ipc %ld (net %d, ipc %d)\n", r, r2, same("/proc/self/ns/net", cnet), same("/proc/self/ns/ipc", cipc));
  if (write(q[1], "x", 1) != 1)
    return 1;
  waitpid(c, NULL, 0);
  r = sn(kid, CLONE_NEWUTS);
  printf("pidfd of a zombie %ld\n", r);

  if (root) {
    r = unshare(CLONE_NEWPID) ? -errno : 0;
    int pfc = open("/proc/self/ns/pid_for_children", O_RDONLY | O_CLOEXEC);
    char a[64], b[64];
    lnk("/proc/self/ns/pid_for_children", a);
    lnk("/proc/self/ns/pid", b);
    printf("unshare pid %ld (children differ %d)\n", r, strcmp(a, b) != 0);
    r = sn(fd[4], CLONE_NEWPID);
    lnk("/proc/self/ns/pid_for_children", a);
    printf("back to my pid %ld (children same %d)\n", r, !strcmp(a, b));
    r = sn(pfc, CLONE_NEWPID);
    lnk("/proc/self/ns/pid_for_children", a);
    printf("down to the new pid %ld (children differ %d)\n", r, strcmp(a, b) != 0);
    pid_t g = fork();
    if (g == 0)
      _exit(-sn(fd[4], CLONE_NEWPID));
    int st;
    waitpid(g, &st, 0);
    printf("up from the new pid -%d (pid in it %d)\n", WEXITSTATUS(st), g > 0);
  }
  return 0;
}
