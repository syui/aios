// tick (10 ms) より細かいタイマ (Linux の HIGH_RES_TIMERS: timerfd、POSIX タイマー、setitimer) が Linux と同じか:
//   timerfd  1 回きり 300 us で read が 5 ms より前に戻る。間 1 ms で 50 回ぶん待つと 200 ms より前 (tick なら 500 ms)
//   posix    timer_create (SIGEV_SIGNAL) を間 1 ms で 50 回 (sigwaitinfo で受ける)。timer_gettime ののこりは間より短い
//   itimer   setitimer (ITIMER_REAL) を間 1 ms で 50 回 (SIGALRM)。getitimer ののこりは間より短い
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

static int bad;

static uint64_t now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000000ull + t.tv_nsec;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // timerfd: 1 回きり
  int fd = timerfd_create(CLOCK_MONOTONIC, 0);
  uint64_t best = UINT64_MAX;
  for (int i = 0; i < 5; i++) {
    struct itimerspec its = {{0, 0}, {0, 300000}};
    uint64_t t0 = now();
    timerfd_settime(fd, 0, &its, NULL);
    uint64_t n;
    read(fd, &n, sizeof n);
    uint64_t d = now() - t0;
    best = d < best ? d : best;
  }
  printf("timerfd one-shot 300 us: not early %d, under 5 ms %d\n", best >= 300000, best < 5000000);
  // timerfd: 間 1 ms で 50 回
  struct itimerspec its = {{0, 1000000}, {0, 1000000}};
  uint64_t t0 = now(), got = 0;
  timerfd_settime(fd, 0, &its, NULL);
  while (got < 50) {
    uint64_t n;
    if (read(fd, &n, sizeof n) == 8)
      got += n;
  }
  uint64_t el = now() - t0;
  struct itimerspec cur;
  timerfd_gettime(fd, &cur);
  printf("timerfd 1 ms x 50: at least 50 ms %d, under 200 ms %d, left <= interval %d\n", el >= 50000000, el < 200000000, cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec <= 1000000);
  close(fd);
  // POSIX タイマー
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGRTMIN);
  sigaddset(&s, SIGALRM);
  sigprocmask(SIG_BLOCK, &s, NULL);
  struct sigevent sev;
  memset(&sev, 0, sizeof sev);
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGRTMIN;
  timer_t tm;
  long cr = timer_create(CLOCK_MONOTONIC, &sev, &tm) < 0 ? -errno : 0;
  sigset_t rt;
  sigemptyset(&rt);
  sigaddset(&rt, SIGRTMIN);
  struct itimerspec pits = {{0, 1000000}, {0, 1000000}};
  t0 = now();
  timer_settime(tm, 0, &pits, NULL);
  int sigs = 0;
  uint64_t overruns = 0;
  while (sigs < 50) {
    siginfo_t si;
    if (sigwaitinfo(&rt, &si) == SIGRTMIN) {
      sigs++;
      overruns += si.si_overrun;
    }
  }
  el = now() - t0;
  struct itimerspec pcur;
  timer_gettime(tm, &pcur);
  timer_delete(tm);
  printf("posix 1 ms x 50: create %ld, at least 49 ms %d, under 200 ms %d, left <= interval %d\n", cr, el >= 49000000, el + overruns * 1000000 < 200000000, pcur.it_value.tv_sec == 0 && pcur.it_value.tv_nsec <= 1000000);
  // setitimer
  sigset_t al;
  sigemptyset(&al);
  sigaddset(&al, SIGALRM);
  struct itimerval iv = {{0, 1000}, {0, 1000}};
  t0 = now();
  setitimer(ITIMER_REAL, &iv, NULL);
  int alrm = 0;
  while (alrm < 50) {
    int sig;
    if (sigwait(&al, &sig) == 0 && sig == SIGALRM)
      alrm++;
  }
  el = now() - t0;
  struct itimerval icur;
  getitimer(ITIMER_REAL, &icur);
  struct itimerval off = {{0, 0}, {0, 0}};
  setitimer(ITIMER_REAL, &off, NULL);
  printf("itimer 1 ms x 50: at least 49 ms %d, under 200 ms %d, left <= interval %d, interval %ld us\n", el >= 49000000, el < 200000000, icur.it_value.tv_sec == 0 && icur.it_value.tv_usec <= 1000, (long)icur.it_interval.tv_usec);
  printf("hrtimers: %d bad\n", bad);
  return bad;
}
