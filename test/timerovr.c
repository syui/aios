// POSIX タイマーのシグナルはタイマーごとに 1 つだけ (Linux の posixtimer の sigqueue) か:
//   blocked   間 1 ms のタイマーのシグナルを止めたまま 100 ms 待つと、来ているのは 1 つで、si_overrun はとばした数 (100 ほど)。
//             timer_getoverrun はさいごに受けとったものの数
//   again     受けとったら次から数えなおす (受けとってすぐ、もう 1 つは来ていない)
//   settime   来ているあいだに timer_settime で止めると、来ていたものは受けとれない (古いものは捨てる)
//   delete    来ているあいだに timer_delete しても、来ていたものは受けとれない
//   rt queue  2 つのタイマーが同じ RT シグナルなら、それぞれ 1 つずつ (2 つ)
//   signalfd  signalfd で読んでも ssi_overrun にとばした数
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/signalfd.h>
#include <time.h>
#include <unistd.h>

static int bad;

static timer_t mk(int sig, int val) {
  struct sigevent sev;
  memset(&sev, 0, sizeof sev);
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = sig;
  sev.sigev_value.sival_int = val;
  timer_t t;
  if (timer_create(CLOCK_MONOTONIC, &sev, &t) < 0)
    bad++;
  return t;
}

static void arm(timer_t t, long ns, long iv) {
  struct itimerspec its = {{0, iv}, {0, ns}};
  timer_settime(t, 0, &its, NULL);
}

static void msleep(int ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
  }
}

static int last_ovr;

// いま来ている sig を、待たずに受けとれるだけ受けとる。はじめのものの si_overrun と si_value を *ovr、*val に、
// さいごのものの si_overrun を last_ovr に
static int drain(int sig, int *ovr, int *val) {
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, sig);
  struct timespec z = {0, 0};
  int n = 0;
  siginfo_t si;
  while (sigtimedwait(&s, &si, &z) == sig) {
    if (n == 0) {
      if (ovr)
        *ovr = si.si_overrun;
      if (val)
        *val = si.si_value.sival_int;
    }
    last_ovr = si.si_overrun;
    n++;
  }
  return n;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int rt = SIGRTMIN;
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, rt);
  sigaddset(&s, rt + 1);
  sigprocmask(SIG_BLOCK, &s, NULL);

  // blocked: 1 ms ごと、100 ms 止めたまま
  timer_t t = mk(rt, 7);
  arm(t, 1000000, 1000000);
  msleep(100);
  int ovr = -1, val = 0;
  int n = drain(rt, &ovr, &val);
  int gov = timer_getoverrun(t);
  // 受けとるとすぐ次を鳴らすので、受けとるあいだにもう 1 つ来ることがある
  printf("blocked: signals 1..2 %d, value %d, overrun 80..120 %d, getoverrun is the last si_overrun %d\n", n >= 1 && n <= 2, val, ovr >= 80 && ovr <= 120, gov == last_ovr);
  // again: すぐにはもう来ていない。10 ms 待つと 1 つ来ていて、si_overrun は 10 ほど
  int n2 = drain(rt, NULL, NULL);
  msleep(10);
  int ovr2 = -1;
  int n3 = drain(rt, &ovr2, NULL);
  printf("again: right after %d, after 10 ms %d, overrun 5..15 %d\n", n2 <= 1, n3, ovr2 >= 5 && ovr2 <= 15);
  // settime: 来ているあいだに止める
  msleep(5);
  struct itimerspec off = {{0, 0}, {0, 0}};
  timer_settime(t, 0, &off, NULL);
  sigset_t p;
  sigpending(&p);
  int pend = sigismember(&p, rt);
  int n4 = drain(rt, NULL, NULL);
  printf("settime while pending: pending bit %d, received %d\n", pend, n4);
  // delete: 1 回きりのものが来ているあいだに消す
  arm(t, 1000000, 0);
  msleep(5);
  timer_delete(t);
  int n5 = drain(rt, NULL, NULL);
  printf("delete while pending: received %d\n", n5);
  // rt queue: 2 つのタイマーが同じシグナル
  timer_t a = mk(rt + 1, 1), b = mk(rt + 1, 2);
  arm(a, 1000000, 1000000);
  arm(b, 1000000, 1000000);
  msleep(30);
  int n6 = drain(rt + 1, NULL, NULL);
  timer_delete(a);
  timer_delete(b);
  printf("two timers same signal: received %d\n", n6);
  // signalfd: 間 1 ms で 30 ms 待って読む
  sigset_t r;
  sigemptyset(&r);
  sigaddset(&r, rt);
  int sfd = signalfd(-1, &r, 0);
  timer_t d = mk(rt, 9);
  arm(d, 1000000, 1000000);
  msleep(30);
  struct signalfd_siginfo ss;
  long got = read(sfd, &ss, sizeof ss);
  timer_delete(d);
  close(sfd);
  drain(rt, NULL, NULL);
  printf("signalfd: read %ld, code SI_TIMER %d, value %d, overrun 20..40 %d\n", got, ss.ssi_code == SI_TIMER, ss.ssi_int, ss.ssi_overrun >= 20 && ss.ssi_overrun <= 40);
  // one-shot: 1 回きりのものは overrun 0
  timer_t c = mk(rt, 3);
  arm(c, 1000000, 0);
  msleep(5);
  int ovr7 = -1;
  int n7 = drain(rt, &ovr7, NULL);
  printf("one-shot: received %d, overrun %d, getoverrun %d\n", n7, ovr7, timer_getoverrun(c));
  timer_delete(c);
  printf("timerovr: %d bad\n", bad);
  return bad;
}
