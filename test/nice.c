// nice が Linux と同じか: getpriority (システムコールは 20 - nice)、setpriority (PRIO_PROCESS / PGRP / USER、下げるのは
// だれでも、上げるのは CAP_SYS_NICE か RLIMIT_NICE の中だけ: EACCES、19 より上と -20 より下はそこまで、知らない which は
// EINVAL、いない pid は ESRCH)、nice(3)、fork した子は受けつぐ、/proc/self/stat の priority と nice、sched_getattr と
// sched_setattr (SCHED_OTHER の nice、FIFO / RR は EPERM、大きさの答え)
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

struct attr {
  uint32_t size, policy;
  uint64_t flags;
  int32_t nice;
  uint32_t prio;
  uint64_t runtime, deadline, period;
  uint32_t util_min, util_max;
};

static long sc(long n, long a, long b, long c, long d) {
  long r = syscall(n, a, b, c, d);
  return r < 0 ? -errno : r;
}

static void stat_nice(const char *who) {
  FILE *f = fopen("/proc/self/stat", "r");
  char b[1024];
  if (!f || !fgets(b, sizeof b, f))
    return;
  fclose(f);
  char *p = strrchr(b, ')') + 2;
  long v[20];
  int i = 0;
  for (char *t = strtok(p, " "); t && i < 20; t = strtok(NULL, " "))
    v[i++] = atol(t);
  // ) のあとの 3 番目から: state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime priority nice
  printf("%s: stat priority %ld nice %ld\n", who, v[15], v[16]);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  printf("getpriority syscall %ld\n", sc(SYS_getpriority, PRIO_PROCESS, 0, 0, 0));
  stat_nice("start");
  printf("set 5 %ld, get %ld, set 3 (raise) %ld, set 25 %ld, get %ld\n", sc(SYS_setpriority, PRIO_PROCESS, 0, 5, 0), sc(SYS_getpriority, PRIO_PROCESS, 0, 0, 0),
         sc(SYS_setpriority, PRIO_PROCESS, 0, 3, 0), sc(SYS_setpriority, PRIO_PROCESS, 0, 25, 0), sc(SYS_getpriority, PRIO_PROCESS, 0, 0, 0));
  stat_nice("after 19");
  // 自分だけのプロセスグループで (PRIO_PGRP 0 が自分だけを見るように)。PRIO_USER の自分は、ほかのプロセスも数える
  setpgid(0, 0);
  printf("which 7 %ld, no pid %ld, pid -1 %ld, PGRP %ld, PGRP no group %ld, USER positive %d, USER uid 12345 %ld\n", sc(SYS_getpriority, 7, 0, 0, 0),
         sc(SYS_getpriority, PRIO_PROCESS, 999999, 0, 0), sc(SYS_getpriority, PRIO_PROCESS, -1, 0, 0), sc(SYS_getpriority, PRIO_PGRP, 0, 0, 0), sc(SYS_getpriority, PRIO_PGRP, 999999, 0, 0),
         sc(SYS_getpriority, PRIO_USER, 0, 0, 0) >= 1, sc(SYS_getpriority, PRIO_USER, 12345, 0, 0));
  int p[2];
  if (pipe(p))
    return 1;
  pid_t k = fork();
  if (k == 0) {
    long v = sc(SYS_getpriority, PRIO_PROCESS, 0, 0, 0);
    if (write(p[1], &v, sizeof v) != sizeof v)
      _exit(1);
    pause();
    _exit(0);
  }
  long cv = 0;
  if (read(p[0], &cv, sizeof cv) != sizeof cv)
    cv = -99;
  printf("fork child %ld, set child 19 %ld, child now %ld\n", cv, sc(SYS_setpriority, PRIO_PROCESS, k, 19, 0), sc(SYS_getpriority, PRIO_PROCESS, k, 0, 0));
  kill(k, 9);
  waitpid(k, NULL, 0);
  // RLIMIT_NICE (20 - rlim が上げられるところ)
  struct rlimit rl;
  getrlimit(RLIMIT_NICE, &rl);
  printf("RLIMIT_NICE %ld %ld\n", (long)rl.rlim_cur, (long)rl.rlim_max);
  // sched_getattr / sched_setattr
  struct attr a;
  memset(&a, 0xff, sizeof a);
  long g = sc(SYS_sched_getattr, 0, (long)&a, sizeof a, 0);
  // slice の既定は 0.7 ms × (1 + log2 (CPU の数、8 まで))
  long cpus = sysconf(_SC_NPROCESSORS_ONLN), factor = 1;
  for (long c = cpus < 8 ? cpus : 8; c > 1; c /= 2)
    factor++;
  printf("sched_getattr (%ld, size %u, policy %u, flags %lu, nice %d, prio %u, runtime / factor %lu, util %u %u)\n", g, a.size, a.policy, (unsigned long)a.flags, a.nice, a.prio,
         (unsigned long)a.runtime / factor, a.util_min, a.util_max);
  printf("getattr (size 47 %ld, size 4097 %ld, flags 1 %ld, pid -1 %ld, no pid %ld, NULL %ld)\n", sc(SYS_sched_getattr, 0, (long)&a, 47, 0), sc(SYS_sched_getattr, 0, (long)&a, 4097, 0),
         sc(SYS_sched_getattr, 0, (long)&a, sizeof a, 1), sc(SYS_sched_getattr, -1, (long)&a, sizeof a, 0), sc(SYS_sched_getattr, 999999, (long)&a, sizeof a, 0),
         sc(SYS_sched_getattr, 0, 0, sizeof a, 0));
  memset(&a, 0, sizeof a);
  a.size = 48;
  a.policy = SCHED_OTHER;
  a.nice = 19;
  long s1 = sc(SYS_sched_setattr, 0, (long)&a, 0, 0);
  a.nice = 10;
  long s2 = sc(SYS_sched_setattr, 0, (long)&a, 0, 0);
  a.policy = SCHED_FIFO;
  a.prio = 1;
  long s3 = sc(SYS_sched_setattr, 0, (long)&a, 0, 0);
  a.policy = 77;
  a.prio = 0;
  long s4 = sc(SYS_sched_setattr, 0, (long)&a, 0, 0);
  a.policy = SCHED_OTHER;
  a.size = 8;
  long s5 = sc(SYS_sched_setattr, 0, (long)&a, 0, 0);
  unsigned sz = a.size;
  a.size = 48;
  a.nice = 30;
  long s6 = sc(SYS_sched_setattr, 0, (long)&a, 0, 0);
  printf("sched_setattr (nice 19 %ld, nice 10 %ld, FIFO %ld, policy 77 %ld, size 8 %ld size now %u, nice 30 %ld, flags 1 %ld), nice now %ld\n", s1, s2, s3, s4, s5, sz, s6,
         sc(SYS_sched_setattr, 0, (long)&a, 1, 0), sc(SYS_getpriority, PRIO_PROCESS, 0, 0, 0));
  printf("sched_getscheduler %d, IDLE %ld then %d, back OTHER %ld\n", sched_getscheduler(0), (long)(sched_setscheduler(0, SCHED_IDLE, &(struct sched_param){0}) ? -errno : 0),
         sched_getscheduler(0), (long)(sched_setscheduler(0, SCHED_OTHER, &(struct sched_param){0}) ? -errno : 0));
  return 0;
}
