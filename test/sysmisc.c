// 小さなシステムコールが Linux と同じか: brk (-1 やユーザーの外は今の brk のまま)、getcpu、sched_rr_get_interval、
// restart_syscall (やりなおすものがなければ EINTR)、setdomainname (root でなければ EPERM、新しい UTS の中なら変えられて
// uname と /proc/sys/kernel/domainname に)、vhangup (root でなければ EPERM)
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long sc(long n, long a, long b, long c) {
  long r = syscall(n, a, b, c);
  return r < 0 ? -errno : r;
}

static void domain(const char *who) {
  struct utsname u;
  uname(&u);
  char f[80] = "";
  FILE *p = fopen("/proc/sys/kernel/domainname", "r");
  if (p) {
    if (!fgets(f, sizeof f, p))
      f[0] = 0;
    fclose(p);
  }
  f[strcspn(f, "\n")] = 0;
  printf("%s domainname (uname \"%s\", proc \"%s\")\n", who, u.domainname, f);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  long cur = sc(SYS_brk, 0, 0, 0);
  long b1 = sc(SYS_brk, -1, 0, 0), b2 = sc(SYS_brk, 1L << 48, 0, 0), b3 = sc(SYS_brk, -4096, 0, 0);
  char *m = malloc(100000);
  memset(m, 1, 100000);
  printf("brk (-1 same %d, 1<<48 same %d, -4096 same %d, malloc after %d)\n", b1 == cur, b2 == cur, b3 == cur, m != NULL);
  unsigned c = 99, n = 99;
  long g = sc(SYS_getcpu, (long)&c, (long)&n, 0);
  printf("getcpu (%ld, cpu ok %d, node %u, NULL %ld, bad %ld)\n", g, c < (unsigned)sysconf(_SC_NPROCESSORS_ONLN), n, sc(SYS_getcpu, 0, 0, 0), sc(SYS_getcpu, 1, 0, 0));
  struct timespec ts = {-1, -1};
  long r = sc(SYS_sched_rr_get_interval, 0, (long)&ts, 0);
  printf("sched_rr_get_interval (%ld, sec %ld, nsec %ld ok %d, pid -1 %ld, no pid %ld, bad %ld)\n", r, (long)ts.tv_sec, (long)ts.tv_nsec, ts.tv_nsec >= 0 && ts.tv_nsec < 1000000000,
         sc(SYS_sched_rr_get_interval, -1, (long)&ts, 0), sc(SYS_sched_rr_get_interval, 999999, (long)&ts, 0), sc(SYS_sched_rr_get_interval, 0, 1, 0));
  printf("restart_syscall %ld\n", sc(SYS_restart_syscall, 0, 0, 0));
  domain("before");
  printf("setdomainname (%ld, len 65 %ld, len -1 %ld), vhangup %ld\n", sc(SYS_setdomainname, (long)"x", 1, 0), sc(SYS_setdomainname, (long)"x", 65, 0),
         sc(SYS_setdomainname, (long)"x", -1, 0), sc(SYS_vhangup, 0, 0, 0));
  pid_t k = fork();
  if (k == 0) {
    if (unshare(CLONE_NEWUSER | CLONE_NEWUTS)) {
      printf("unshare %d\n", errno);
      _exit(1);
    }
    long a = sc(SYS_setdomainname, (long)"ai.example", 10, 0), b = sc(SYS_setdomainname, (long)"x", 65, 0), z = sc(SYS_setdomainname, (long)"", 0, 0);
    printf("in a new UTS (set %ld, len 65 %ld, empty %ld)\n", a, b, z);
    sc(SYS_setdomainname, (long)"ai.example", 10, 0);
    domain("in a new UTS");
    _exit(0);
  }
  waitpid(k, NULL, 0);
  domain("after");
  return 0;
}
