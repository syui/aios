// setitimer (ITIMER_REAL) のくり返しは SIGALRM を受けとったときに鳴らしなおす (Linux の dequeue_signal と it_real_fn) か:
//   blocked   SIGALRM を止めたまま間 20 ms で 50 ms 待つと、getitimer ののこりは 0 (止まっている)、間は 20000 us
//   dequeue   受けとると、また動く (のこりは 0 より大きく間まで)
//   ignored   SIGALRM を無視 (SIG_IGN) していると、1 回鳴って止まる (のこり 0)
//   handler   ハンドラで受けとれば動きつづける (間 2 ms で 40 ms に 10 回より多く)
//   posix     POSIX タイマーは、来ているあいだも timer_gettime ののこりは 0 より大きく間まで (Linux の common_timer_get)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int bad;
static volatile int hits;

static void on_alrm(int s) {
  (void)s;
  hits++;
}

static void msleep(int ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
  }
}

static void set(long us) {
  struct itimerval iv = {{0, us}, {0, us}};
  if (setitimer(ITIMER_REAL, &iv, NULL) < 0)
    bad++;
}

static long left_us(long *iv) {
  struct itimerval c;
  getitimer(ITIMER_REAL, &c);
  if (iv)
    *iv = c.it_interval.tv_sec * 1000000 + c.it_interval.tv_usec;
  return c.it_value.tv_sec * 1000000 + c.it_value.tv_usec;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  sigset_t al;
  sigemptyset(&al);
  sigaddset(&al, SIGALRM);
  sigprocmask(SIG_BLOCK, &al, NULL);

  // blocked
  set(20000);
  msleep(50);
  long iv = 0;
  long l = left_us(&iv);
  sigset_t p;
  sigpending(&p);
  printf("blocked: pending %d, left %ld, interval %ld\n", sigismember(&p, SIGALRM), l, iv);
  // dequeue
  int sig = 0;
  sigwait(&al, &sig);
  long l2 = left_us(NULL);
  printf("dequeue: sig %d, left in (0, 20000] %d\n", sig, l2 > 0 && l2 <= 20000);
  msleep(25);
  sigwait(&al, &sig);
  struct itimerval off = {{0, 0}, {0, 0}};
  setitimer(ITIMER_REAL, &off, NULL);

  // ignored
  signal(SIGALRM, SIG_IGN);
  sigprocmask(SIG_UNBLOCK, &al, NULL);
  set(1000);
  msleep(10);
  long l3 = left_us(&iv);
  printf("ignored: left %ld, interval %ld\n", l3, iv);
  setitimer(ITIMER_REAL, &off, NULL);

  // handler
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alrm;
  sigaction(SIGALRM, &sa, NULL);
  hits = 0;
  set(2000);
  msleep(40);
  setitimer(ITIMER_REAL, &off, NULL);
  if (getenv("ITIMERQ_V"))
    fprintf(stderr, "hits %d\n", hits);
  printf("handler: more than 10 in 40 ms %d\n", hits > 10);

  // posix
  sigset_t rt;
  sigemptyset(&rt);
  sigaddset(&rt, SIGRTMIN);
  sigprocmask(SIG_BLOCK, &rt, NULL);
  struct sigevent sev;
  memset(&sev, 0, sizeof sev);
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGRTMIN;
  timer_t t;
  timer_create(CLOCK_MONOTONIC, &sev, &t);
  struct itimerspec its = {{0, 1000000}, {0, 1000000}};
  timer_settime(t, 0, &its, NULL);
  msleep(10);
  struct itimerspec cur;
  timer_gettime(t, &cur);
  printf("posix pending: left in (0, 1 ms] %d, interval %ld\n", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 1000000, cur.it_interval.tv_nsec);
  timer_delete(t);
  printf("itimerq: %d bad\n", bad);
  return bad;
}
