// prctl の答えが Linux と同じか: 知らない値は EINVAL、PDEATHSIG (親のスレッドが終わると子にシグナル。fork した子には
// 受けつがない)、CHILD_SUBREAPER (みなし子をひきとる: 孫の getppid がこちらに)、KEEPCAPS、CAPBSET_READ、SECUREBITS、
// TIMING、THP_DISABLE、TAGGED_ADDR_CTRL、TID_ADDRESS (CHECKPOINT_RESTORE)、手本の設定にないもの (SET_VMA、MEMORY_MERGE)、arm64 にないもの
// (TSC、ENDIAN、FPEMU)
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

static long pr(int op, unsigned long a, unsigned long b, unsigned long c, unsigned long d) {
  long r = prctl(op, a, b, c, d);
  return r < 0 ? -errno : r;
}

static int pipefd[2];
static void on_alarm(int s) { (void)s; }
static void on_usr1(int s) {
  (void)s;
  char c = 'U';
  if (write(pipefd[1], &c, 1) != 1)
    _exit(3);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  printf("unknown (999 %ld, 0 %ld, 1000 %ld)\n", pr(999, 0, 0, 0, 0), pr(0, 0, 0, 0, 0), pr(1000, 0, 0, 0, 0));
  // PDEATHSIG
  {
    int v = -1;
    long a = pr(PR_SET_PDEATHSIG, SIGUSR1, 0, 0, 0), b = pr(PR_GET_PDEATHSIG, (unsigned long)&v, 0, 0, 0);
    long c = pr(PR_SET_PDEATHSIG, 65, 0, 0, 0), d = pr(PR_GET_PDEATHSIG, 8, 0, 0, 0);
    pid_t k = fork();
    if (k == 0) {
      int cv = -1;
      prctl(PR_GET_PDEATHSIG, &cv);
      printf("pdeathsig in a fork child %d\n", cv);
      _exit(0);
    }
    waitpid(k, NULL, 0);
    pr(PR_SET_PDEATHSIG, 0, 0, 0, 0);
    printf("pdeathsig (set %ld, get %ld %d, 65 %ld, bad pointer %ld)\n", a, b, v, c, d);
    // 親 (中のプロセス) が終わると、孫が SIGUSR1 を受ける
    if (pipe(pipefd))
      return 1;
    pid_t mid = fork();
    if (mid == 0) {
      int ready[2];
      if (pipe(ready))
        _exit(1);
      pid_t g = fork();
      if (g == 0) {
        signal(SIGUSR1, on_usr1);
        prctl(PR_SET_PDEATHSIG, SIGUSR1);
        alarm(5); // 来なければ 5 秒で終わる
        char c = 'r';
        if (write(ready[1], &c, 1) != 1)
          _exit(1);
        pause();
        _exit(0);
      }
      char c;
      if (read(ready[0], &c, 1) != 1)
        _exit(1);
      _exit(0); // 孫を残して終わる
    }
    waitpid(mid, NULL, 0);
    // 3 秒で来なければ (孫がまだいれば SIGKILL で片づける)
    struct sigaction sa = {.sa_handler = on_alarm};
    sigaction(SIGALRM, &sa, NULL);
    alarm(3);
    char got = 0;
    if (read(pipefd[0], &got, 1) != 1)
      got = '?';
    alarm(0);
    printf("pdeathsig fired: %c\n", got);
  }
  // CHILD_SUBREAPER
  {
    int v = -1;
    long a = pr(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0), b = pr(PR_GET_CHILD_SUBREAPER, (unsigned long)&v, 0, 0, 0);
    int p[2];
    if (pipe(p))
      return 1;
    pid_t mid = fork();
    if (mid == 0) {
      pid_t g = fork();
      if (g == 0) {
        usleep(300000);
        pid_t pp = getppid();
        int me = pp == getpgid(0) ? 0 : 1;
        (void)me;
        if (write(p[1], &pp, sizeof pp) != sizeof pp)
          _exit(1);
        _exit(0);
      }
      _exit(0);
    }
    waitpid(mid, NULL, 0);
    pid_t pp = 0;
    if (read(p[0], &pp, sizeof pp) != sizeof pp)
      pp = -1;
    int reaped = wait(NULL) > 0;
    printf("subreaper (set %ld, get %ld %d, orphan's parent is us %d, reaped by us %d)\n", a, b, v, pp == getpid(), reaped);
    pr(PR_SET_CHILD_SUBREAPER, 0, 0, 0, 0);
  }
  // そのほか
  {
    int sb = pr(PR_GET_SECUREBITS, 0, 0, 0, 0);
    printf("keepcaps (get %ld, set 2 %ld, set 1 %ld, get %ld)\n", pr(PR_GET_KEEPCAPS, 0, 0, 0, 0), pr(PR_SET_KEEPCAPS, 2, 0, 0, 0), pr(PR_SET_KEEPCAPS, 1, 0, 0, 0),
           pr(PR_GET_KEEPCAPS, 0, 0, 0, 0));
    printf("capbset (0 %ld, 40 %ld, 100 %ld), securebits (get %d, set %ld)\n", pr(PR_CAPBSET_READ, 0, 0, 0, 0), pr(PR_CAPBSET_READ, 40, 0, 0, 0), pr(PR_CAPBSET_READ, 100, 0, 0, 0),
           sb, pr(PR_SET_SECUREBITS, 0, 0, 0, 0));
    printf("timing (get %ld, set 1 %ld, set 0 %ld)\n", pr(PR_GET_TIMING, 0, 0, 0, 0), pr(PR_SET_TIMING, 1, 0, 0, 0), pr(PR_SET_TIMING, 0, 0, 0, 0));
    printf("thp (get %ld, set 1 %ld, get %ld, set with arg3 %ld)\n", pr(PR_GET_THP_DISABLE, 0, 0, 0, 0), pr(PR_SET_THP_DISABLE, 1, 0, 0, 0), pr(PR_GET_THP_DISABLE, 0, 0, 0, 0),
           pr(PR_SET_THP_DISABLE, 1, 1, 0, 0));
    printf("tagged addr (get %ld)\n", pr(56, 0, 0, 0, 0));
    printf("not built in (TID_ADDRESS %ld, SET_VMA %ld, MEMORY_MERGE %ld, IO_FLUSHER %ld)\n", pr(PR_GET_TID_ADDRESS, 0, 0, 0, 0), pr(0x53564d41, 0, 0, 0, 0), pr(68, 0, 0, 0, 0),
           pr(58, 0, 0, 0, 0));
    printf("not on arm64 (TSC %ld, ENDIAN %ld, FPEMU %ld, FPEXC %ld, UNALIGN %ld)\n", pr(PR_GET_TSC, 0, 0, 0, 0), pr(PR_GET_ENDIAN, 0, 0, 0, 0), pr(PR_GET_FPEMU, 0, 0, 0, 0),
           pr(PR_GET_FPEXC, 0, 0, 0, 0), pr(PR_GET_UNALIGN, 0, 0, 0, 0));
    printf("mce kill (get %ld)\n", pr(PR_MCE_KILL_GET, 0, 0, 0, 0));
  }
  return 0;
}
