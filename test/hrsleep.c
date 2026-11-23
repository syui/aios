// tick (10 ms) より細かい眠り (Linux の HIGH_RES_TIMERS) が Linux と同じか:
//   sleep   nanosleep、clock_nanosleep (相対と TIMER_ABSTIME)、poll、ppoll、select、epoll_wait、epoll_pwait2、futex の
//           FUTEX_WAIT、sigtimedwait を 300 us ずつ。早く起きない (頼んだ時間はたつ) こと、5 ms より短いこと
//   res     clock_getres は REALTIME、MONOTONIC、MONOTONIC_RAW、BOOTTIME、TAI が 1 ns、COARSE は tick
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int bad;
#define REQ_NS 300000ull

static uint64_t now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000000ull + t.tv_nsec;
}

// 5 回のうちいちばん短いもの (ほかのものに CPU をとられたときのため)
static void show(const char *name, void (*f)(void)) {
  uint64_t best = UINT64_MAX;
  for (int i = 0; i < 5; i++) {
    uint64_t t0 = now();
    f();
    uint64_t d = now() - t0;
    best = d < best ? d : best;
  }
  printf("%s: not early %d, under 5 ms %d\n", name, best >= REQ_NS, best < 5000000);
}

static void f_nanosleep(void) {
  struct timespec ts = {0, REQ_NS};
  nanosleep(&ts, NULL);
}
static void f_clock_nanosleep(void) {
  struct timespec ts = {0, REQ_NS};
  clock_nanosleep(CLOCK_MONOTONIC, 0, &ts, NULL);
}
static void f_abstime(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  ts.tv_nsec += REQ_NS;
  if (ts.tv_nsec >= 1000000000) {
    ts.tv_sec++;
    ts.tv_nsec -= 1000000000;
  }
  clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &ts, NULL);
}
static int pfd[2];
static void f_ppoll(void) {
  struct pollfd p = {pfd[0], POLLIN, 0};
  struct timespec ts = {0, REQ_NS};
  ppoll(&p, 1, &ts, NULL);
}
static void f_poll(void) {
  // poll はミリ秒: 1 ms
  struct pollfd p = {pfd[0], POLLIN, 0};
  poll(&p, 1, 1);
}
static void f_select(void) {
  fd_set r;
  FD_ZERO(&r);
  FD_SET(pfd[0], &r);
  struct timeval tv = {0, REQ_NS / 1000};
  select(pfd[0] + 1, &r, NULL, NULL, &tv);
}
static int ep;
static void f_epoll(void) {
  struct epoll_event e;
  epoll_wait(ep, &e, 1, 1);
}
static void f_epoll2(void) {
  struct epoll_event e;
  struct timespec ts = {0, REQ_NS};
  syscall(SYS_epoll_pwait2, ep, &e, 1, &ts, NULL, 8);
}
static int fx;
static void f_futex(void) {
  struct timespec ts = {0, REQ_NS};
  syscall(SYS_futex, &fx, FUTEX_WAIT_PRIVATE, 0, &ts, NULL, 0);
}
static void f_sigtimedwait(void) {
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGUSR1);
  struct timespec ts = {0, REQ_NS};
  sigtimedwait(&s, NULL, &ts);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  pipe(pfd);
  ep = epoll_create1(0);
  struct epoll_event e = {EPOLLIN, {0}};
  epoll_ctl(ep, EPOLL_CTL_ADD, pfd[0], &e);
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGUSR1);
  sigprocmask(SIG_BLOCK, &s, NULL);
  show("nanosleep", f_nanosleep);
  show("clock_nanosleep", f_clock_nanosleep);
  show("clock_nanosleep abstime", f_abstime);
  show("ppoll", f_ppoll);
  show("poll 1 ms", f_poll);
  show("select", f_select);
  show("epoll_wait 1 ms", f_epoll);
  show("epoll_pwait2", f_epoll2);
  show("futex wait", f_futex);
  show("sigtimedwait", f_sigtimedwait);
  const int clk[] = {0, 1, 4, 5, 6, 7, 11};
  printf("res:");
  for (unsigned i = 0; i < sizeof clk / sizeof *clk; i++) {
    struct timespec r = {0, 0};
    clock_getres(clk[i], &r);
    long ns = r.tv_sec * 1000000000L + r.tv_nsec;
    printf(" %d=%s", clk[i], ns == 1 ? "1ns" : ns > 0 && ns <= 10000000 ? "tick" : "other");
  }
  printf("\nhrsleep: %d bad\n", bad);
  return bad;
}
