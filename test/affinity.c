// CPU の割りあて (sched_setaffinity / sched_getaffinity) が Linux と同じか (CPU の数によらないように CPU 0 で):
//   get     はじめは動いている CPU みな。len は 8 の倍数で CPU の数より多く (ちがえば EINVAL)、答えは 8 (cpumask の大きさ)
//   set     CPU 0 だけにすると、そこで動く (sched_getcpu)。/proc/self/status の Cpus_allowed_list。空や、ない CPU だけは
//           EINVAL、いないスレッドは ESRCH、ほかの人のものは EPERM (root でなければ)
//   inherit fork した子とスレッドは受けつぐ。スレッドごと (ほかのスレッドは変わらない)。ほかのプロセスのものも変えられる
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int bad;

static long setaff(pid_t pid, size_t len, const void *m) {
  long r = syscall(SYS_sched_setaffinity, pid, len, m);
  return r < 0 ? -errno : r;
}
static long getaff(pid_t pid, size_t len, void *m) {
  long r = syscall(SYS_sched_getaffinity, pid, len, m);
  return r < 0 ? -errno : r;
}

static unsigned long long all;

static const char *desc(unsigned long long m) {
  return m == all ? "all" : m == 1 ? "cpu0" : m == 0 ? "none" : "other";
}

static unsigned long long mine(pid_t pid) {
  unsigned long long m = 0;
  getaff(pid, sizeof m, &m);
  return m;
}

static void status_list(char *out, size_t n) {
  FILE *f = fopen("/proc/self/status", "r");
  char line[256];
  out[0] = 0;
  while (f && fgets(line, sizeof line, f))
    if (!strncmp(line, "Cpus_allowed_list:", 18)) {
      char *v = line + 18;
      while (*v == ' ' || *v == '\t')
        v++;
      v[strcspn(v, "\n")] = 0;
      snprintf(out, n, "%s", v);
    }
  if (f)
    fclose(f);
}

static volatile unsigned long long thr_mask;
static void *thr(void *a) {
  (void)a;
  thr_mask = mine(0);
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  long n = sysconf(_SC_NPROCESSORS_ONLN);
  all = n >= 64 ? ~0ull : (1ull << n) - 1;
  unsigned long long m = 0;
  unsigned char big[128];
  memset(big, 0xaa, sizeof big);
  long r8 = getaff(0, 8, &m);
  printf("get: len 8 %ld %s, len 128 %ld, len 4 %ld, len 12 %ld, len 0 %ld\n", r8, desc(m), getaff(0, 128, big), getaff(0, 4, &m), getaff(0, 12, big), getaff(0, 0, &m));
  printf("get: bytes past the answer untouched %d\n", big[8] == 0xaa && big[127] == 0xaa);
  // CPU 0 だけに
  unsigned long long one = 1;
  long s = setaff(0, sizeof one, &one);
  sched_yield();
  char list[64];
  status_list(list, sizeof list);
  printf("set: cpu0 %ld, now %s, running on cpu %d, Cpus_allowed_list %s\n", s, desc(mine(0)), sched_getcpu(), list);
  unsigned long long zero = 0, far = 1ull << 63;
  printf("set: empty %ld, only a missing cpu %ld, with a missing cpu %ld now %s, short len 4 %ld now %s\n", setaff(0, 8, &zero), setaff(0, 8, &far), setaff(0, 8, &(unsigned long long){1 | far}), desc(mine(0)), setaff(0, 4, &one),
         desc(mine(0)));
  printf("set: no such thread %ld, get no such thread %ld\n", setaff(4000000, 8, &one), getaff(4000000, 8, &m));
  // fork した子とスレッドは受けつぐ
  pid_t c = fork();
  if (c == 0)
    _exit(mine(0) == 1 ? 0 : 1);
  int st;
  waitpid(c, &st, 0);
  pthread_t t;
  pthread_create(&t, NULL, thr, NULL);
  pthread_join(t, NULL);
  printf("inherit: child %s, thread %s\n", WEXITSTATUS(st) == 0 ? "cpu0" : "other", desc(thr_mask));
  // ほかのプロセスのものを変える
  int p[2];
  pipe(p);
  c = fork();
  if (c == 0) {
    char b;
    read(p[0], &b, 1);
    _exit(mine(0) == all ? 0 : 1);
  }
  long so = setaff(c, sizeof all, &all);
  printf("other: set child %ld, child sees by get %s", so, desc(mine(c)));
  write(p[1], "x", 1);
  waitpid(c, &st, 0);
  printf(", child sees itself %s\n", WEXITSTATUS(st) == 0 ? "all" : "other");
  // root でないなら、ほかの人のもの (init) は変えられない
  c = fork();
  if (c == 0) {
    if (getuid() == 0) {
      setgid(65534);
      setuid(65534);
    }
    _exit(-setaff(1, 8, &one));
  }
  waitpid(c, &st, 0);
  printf("perm: other user's %d\n", -WEXITSTATUS(st));
  // もとに
  printf("restore: %ld now %s\n", setaff(0, sizeof all, &all), desc(mine(0)));
  printf("affinity: %d bad\n", bad);
  return bad;
}
