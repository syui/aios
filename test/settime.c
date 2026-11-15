// 時計を合わせるものが Linux と同じか: settimeofday、clock_settime、adjtimex、clock_adjtime、gettimeofday の tz。
// root でなければ許しのエラー (と、許しより先に見るもの)、root なら合わせて (1 時間先へ) 読み、もとにもどす
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timex.h>
#include <time.h>
#include <unistd.h>

static long sc(long n, long a, long b) {
  long r = syscall(n, a, b);
  return r < 0 ? -errno : r;
}

static void show(const char *what, long r, struct timex *t) {
  struct timeval now;
  gettimeofday(&now, NULL);
  printf("%s %ld (status %#x, maxerror %ld, esterror %ld, constant %ld, precision %ld, tolerance %ld, tick %ld, freq %ld, offset %ld, tai %d, time now %d)\n", what, r,
         t->status, (long)t->maxerror, (long)t->esterror, (long)t->constant, (long)t->precision, (long)t->tolerance, (long)t->tick, (long)t->freq, (long)t->offset, t->tai,
         t->time.tv_sec - now.tv_sec <= 1 && now.tv_sec - t->time.tv_sec <= 1);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int root = geteuid() == 0;
  struct timeval tv;
  struct timezone tz = {-1, -1};
  gettimeofday(&tv, &tz);
  printf("root %d, gettimeofday tz (%d %d)\n", root, tz.tz_minuteswest, tz.tz_dsttime);
  struct timeval bad = {tv.tv_sec, 1000000};
  struct timezone far = {1000, 0};
  printf("settimeofday (NULL NULL %ld, bad usec %ld, bad pointer %ld, tz 1000 %ld)\n", sc(SYS_settimeofday, 0, 0), sc(SYS_settimeofday, (long)&bad, 0), sc(SYS_settimeofday, 8, 0),
         sc(SYS_settimeofday, 0, (long)&far));
  struct timespec ts, badts = {1, 2000000000};
  clock_gettime(CLOCK_REALTIME, &ts);
  printf("clock_settime (MONOTONIC %ld, 99 %ld, bad pointer %ld, bad nsec %ld, PROCESS_CPUTIME %ld, negative %ld)\n", sc(SYS_clock_settime, CLOCK_MONOTONIC, (long)&ts),
         sc(SYS_clock_settime, 99, (long)&ts), sc(SYS_clock_settime, CLOCK_REALTIME, 8), sc(SYS_clock_settime, CLOCK_REALTIME, (long)&badts), sc(SYS_clock_settime, CLOCK_PROCESS_CPUTIME_ID, (long)&ts),
         sc(SYS_clock_settime, CLOCK_REALTIME, (long)&(struct timespec){-1, 0}));
  struct timex t;
  memset(&t, 0, sizeof t);
  show("adjtimex read", sc(SYS_adjtimex, (long)&t, 0), &t);
  memset(&t, 0, sizeof t);
  t.modes = ADJ_OFFSET_SS_READ;
  show("adjtimex SS_READ", sc(SYS_adjtimex, (long)&t, 0), &t);
  memset(&t, 0, sizeof t);
  t.modes = 0x8000;
  printf("adjtimex ADJTIME alone %ld, bad pointer %ld\n", sc(SYS_adjtimex, (long)&t, 0), sc(SYS_adjtimex, 8, 0));
  memset(&t, 0, sizeof t);
  show("clock_adjtime REALTIME", sc(SYS_clock_adjtime, CLOCK_REALTIME, (long)&t), &t);
  printf("clock_adjtime (MONOTONIC %ld, 99 %ld, bad pointer %ld)\n", sc(SYS_clock_adjtime, CLOCK_MONOTONIC, (long)&t), sc(SYS_clock_adjtime, 99, (long)&t),
         sc(SYS_clock_adjtime, CLOCK_REALTIME, 8));
  if (!root) {
    memset(&t, 0xee, sizeof t);
    t.modes = ADJ_STATUS;
    long r = sc(SYS_adjtimex, (long)&t, 0);
    printf("not root (settimeofday %ld, clock_settime %ld, adjtimex ADJ_STATUS %ld untouched %d, ADJ_SETOFFSET %ld)\n", sc(SYS_settimeofday, (long)&tv, 0),
           sc(SYS_clock_settime, CLOCK_REALTIME, (long)&ts), r, t.maxerror == (long)0xeeeeeeeeeeeeeeeeUL,
           sc(SYS_adjtimex, (long)&(struct timex){.modes = ADJ_SETOFFSET}, 0));
    return 0;
  }
  // root: 1 時間先へ
  struct timeval later = {tv.tv_sec + 3600, tv.tv_usec};
  long s1 = sc(SYS_settimeofday, (long)&later, 0);
  time_t now = time(NULL);
  struct timespec rt, vd;
  syscall(SYS_clock_gettime, CLOCK_REALTIME, &rt);
  clock_gettime(CLOCK_REALTIME, &vd);
  printf("settimeofday +1h (%ld, time() moved %d, syscall moved %d, vdso moved %d)\n", s1, now - tv.tv_sec >= 3599 && now - tv.tv_sec <= 3602, rt.tv_sec - tv.tv_sec >= 3599 && rt.tv_sec - tv.tv_sec <= 3602,
         vd.tv_sec - tv.tv_sec >= 3599 && vd.tv_sec - tv.tv_sec <= 3602);
  struct timespec back = {ts.tv_sec + 1, 500000000};
  long s2 = sc(SYS_clock_settime, CLOCK_REALTIME, (long)&back);
  clock_gettime(CLOCK_REALTIME, &vd);
  printf("clock_settime back (%ld, now near %d, nsec near %d)\n", s2, vd.tv_sec - back.tv_sec <= 1, vd.tv_sec == back.tv_sec ? vd.tv_nsec >= 500000000 : 1);
  struct timezone tz60 = {60, 0};
  long z1 = sc(SYS_settimeofday, 0, (long)&tz60), z2 = sc(SYS_settimeofday, 0, (long)&far);
  gettimeofday(&tv, &tz);
  printf("tz (60 %ld, 1000 %ld, now %d %d)\n", z1, z2, tz.tz_minuteswest, tz.tz_dsttime);
  sc(SYS_settimeofday, 0, (long)&(struct timezone){0, 0});
  memset(&t, 0, sizeof t);
  t.modes = ADJ_STATUS | ADJ_MAXERROR | ADJ_ESTERROR | ADJ_TIMECONST;
  t.status = 0;
  t.maxerror = 1000;
  t.esterror = 500;
  t.constant = 4;
  show("adjtimex set (status 0)", sc(SYS_adjtimex, (long)&t, 0), &t);
  memset(&t, 0, sizeof t);
  long ra = sc(SYS_adjtimex, (long)&t, 0);
  // maxerror は 1 秒ごとに 500 増える
  printf("adjtimex read again %ld (status %#x, maxerror near %d, esterror %ld, constant %ld)\n", ra, t.status, t.maxerror >= 1000 && t.maxerror <= 1500, (long)t.esterror, (long)t.constant);
  memset(&t, 0, sizeof t);
  t.modes = ADJ_SETOFFSET | ADJ_NANO;
  t.time.tv_sec = 10;
  t.time.tv_usec = 0;
  clock_gettime(CLOCK_REALTIME, &ts);
  long so = sc(SYS_adjtimex, (long)&t, 0);
  clock_gettime(CLOCK_REALTIME, &vd);
  printf("ADJ_SETOFFSET +10s (%ld, moved %d)\n", so, vd.tv_sec - ts.tv_sec >= 10 && vd.tv_sec - ts.tv_sec <= 11);
  memset(&t, 0, sizeof t);
  t.modes = ADJ_STATUS;
  t.status = STA_UNSYNC;
  sc(SYS_adjtimex, (long)&t, 0);
  memset(&t, 0, sizeof t);
  t.modes = ADJ_TICK;
  t.tick = 5000;
  printf("ADJ_TICK 5000 %ld\n", sc(SYS_adjtimex, (long)&t, 0));
  // もとにもどす (おおよそ)
  ts.tv_sec -= 0;
  sc(SYS_clock_settime, CLOCK_REALTIME, (long)&ts);
  return 0;
}
