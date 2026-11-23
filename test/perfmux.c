// perf の PMU のカウンタが足りないとき、イベントを回して数える (Linux の perf_rotate_context: multiplex) か:
//   group     cycles のグループは、カウンタの数まで入る。ひとつ多いと EINVAL (Linux の validate_group)。止めて開いた
//             (disabled) ものは数えないので、いくつでも入る
//   mux       カウンタの 2 倍より多い cycles を、べつべつに開いて 0.4 秒まわす: どれも time_enabled はほぼ同じで、
//             time_running は 0 より大きく time_enabled より小さい。count * enabled / running (perf stat の見積もり) は
//             みなほぼ同じ (25% のうち)
//   groups    カウンタいっぱいのグループ 2 つ: グループの中は time_running が同じ、どちらも回る
//   close     閉じて減れば、のこりはずっと数える (running = enabled に近づく)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <linux/perf_event.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int bad;

static int open_ev(int group, int disabled) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.size = sizeof a;
  a.type = PERF_TYPE_HARDWARE;
  a.config = PERF_COUNT_HW_CPU_CYCLES;
  a.disabled = disabled;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  a.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING;
  return syscall(SYS_perf_event_open, &a, 0, -1, group, 0);
}

static uint64_t now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000000ull + t.tv_nsec;
}

static volatile uint64_t sink;
static void spin(uint64_t ns) {
  uint64_t end = now() + ns;
  while (now() < end)
    for (int i = 0; i < 1000; i++)
      sink += i;
}

struct val {
  uint64_t c, e, r;
};

static void rd(int fd, struct val *v) {
  if (read(fd, v, sizeof *v) != sizeof *v)
    memset(v, 0, sizeof *v);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // group: 動かして開くものは、カウンタの数まで入る
  int g = open_ev(-1, 0);
  if (g < 0) {
    printf("perfmux: no PMU (%s)\n", strerror(errno));
    return 0;
  }
  int n = 1, last = 0;
  int gfd[64];
  gfd[0] = g;
  while (n < 64) {
    int f = open_ev(g, 0);
    if (f < 0) {
      last = errno;
      break;
    }
    gfd[n++] = f;
  }
  printf("group: fits some %d, one more EINVAL %d\n", n >= 2 && n < 64, last == EINVAL);
  for (int i = 0; i < n; i++)
    close(gfd[i]);
  int ncnt = n;
  // 止めて開くもの (disabled) は数えない (Linux の validate_event: PERF_EVENT_STATE_OFF はよい)
  g = open_ev(-1, 1);
  n = 1;
  gfd[0] = g;
  while (n < 32) {
    int f = open_ev(g, 1);
    if (f < 0)
      break;
    gfd[n++] = f;
  }
  printf("disabled group: more than counters %d\n", n > ncnt);
  for (int i = 0; i < n; i++)
    close(gfd[i]);

  // mux: カウンタの 2 倍 + 2 こ
  int m = 2 * ncnt + 2;
  int fds[160];
  for (int i = 0; i < m; i++)
    fds[i] = open_ev(-1, 1);
  for (int i = 0; i < m; i++)
    ioctl(fds[i], PERF_EVENT_IOC_ENABLE, 0);
  spin(400000000);
  for (int i = 0; i < m; i++)
    ioctl(fds[i], PERF_EVENT_IOC_DISABLE, 0);
  struct val v[160];
  for (int i = 0; i < m; i++)
    rd(fds[i], &v[i]);
  uint64_t emin = UINT64_MAX, emax = 0;
  int ran = 1, less = 1;
  double smin = 1e30, smax = 0;
  for (int i = 0; i < m; i++) {
    emin = v[i].e < emin ? v[i].e : emin;
    emax = v[i].e > emax ? v[i].e : emax;
    if (v[i].r == 0)
      ran = 0;
    if (v[i].r >= v[i].e)
      less = 0;
    if (v[i].r) {
      double s = (double)v[i].c * v[i].e / v[i].r;
      smin = s < smin ? s : smin;
      smax = s > smax ? s : smax;
    }
  }
  printf("mux: enabled same %d, all ran %d, all ran less than enabled %d, scaled within 25%% %d\n", emax - emin < emax / 20 + 1000000, ran, less, smax > 0 && smin > smax * 0.75);
  for (int i = 0; i < m; i++)
    close(fds[i]);

  // groups: いっぱいのグループ 2 つ
  int ga[64], gb[64];
  ga[0] = open_ev(-1, 1);
  gb[0] = open_ev(-1, 1);
  for (int i = 1; i < ncnt; i++) {
    ga[i] = open_ev(ga[0], 0);
    gb[i] = open_ev(gb[0], 0);
  }
  ioctl(ga[0], PERF_EVENT_IOC_ENABLE, 0);
  ioctl(gb[0], PERF_EVENT_IOC_ENABLE, 0);
  spin(200000000);
  ioctl(ga[0], PERF_EVENT_IOC_DISABLE, 0);
  ioctl(gb[0], PERF_EVENT_IOC_DISABLE, 0);
  struct val a0, b0;
  rd(ga[0], &a0);
  rd(gb[0], &b0);
  int same = 1;
  for (int i = 1; i < ncnt; i++) {
    struct val x, y;
    rd(ga[i], &x);
    rd(gb[i], &y);
    if (x.r != a0.r || y.r != b0.r)
      same = 0;
  }
  printf("groups: members same running %d, both ran %d, both less than enabled %d\n", same, a0.r > 0 && b0.r > 0, a0.r < a0.e && b0.r < b0.e);
  for (int i = 0; i < ncnt; i++) {
    close(ga[i]);
    close(gb[i]);
  }

  // close: 多すぎるものを開き、半分を閉じる
  for (int i = 0; i < m; i++)
    fds[i] = open_ev(-1, 0);
  spin(100000000);
  for (int i = 1; i < m; i++)
    close(fds[i]);
  struct val p0, p1;
  rd(fds[0], &p0);
  spin(200000000);
  rd(fds[0], &p1);
  printf("close: after closing the rest it runs all the time %d\n", p1.r - p0.r > (p1.e - p0.e) * 9 / 10);
  close(fds[0]);
  printf("perfmux: %d bad\n", bad);
  return bad;
}
