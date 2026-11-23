// perf のあふれの SIGIO (O_ASYNC: perf_fasync) と PERF_EVENT_IOC_REFRESH が Linux と同じか:
//   async   サンプリングのイベント (ページフォルト、間 1) に O_ASYNC と持ち主と F_SETSIG: あふれるたびに POLL_IN と si_fd
//           (ブレークポイントは、シグナルで 1 命令越えが切れて同じところでまた当たるので使わない: perf3.c と同じ)
//   refresh IOC_REFRESH(n): n 回あふれたら POLL_HUP を送って止まる (数えるのもやめる)。もう一度 refresh すると続く
//   sigio   F_SETSIG なしなら SIGIO (SI_KERNEL)
//   bad     数えるだけのもの (sample_period 0) と inherit のものの REFRESH は EINVAL
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/perf_event.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int bad;

struct ent {
  int signo, code, fd;
  long band;
};
static struct ent ents[64];
static volatile int nent;
static void on_sig(int s, siginfo_t *si, void *u) {
  (void)u;
  int n = nent;
  if (n < 64) {
    ents[n].signo = s;
    ents[n].code = si->si_code;
    ents[n].fd = si->si_fd;
    ents[n].band = si->si_band;
    nent = n + 1;
  }
}

static int evfd;
static void show(const char *what) {
  struct timespec t = {0, 20 * 1000 * 1000};
  while (nanosleep(&t, &t) < 0 && errno == EINTR)
    ;
  int n = nent;
  printf("%s: %d", what, n);
  for (int i = 0; i < n; i++) {
    const char *s = ents[i].signo == SIGIO ? "SIGIO" : ents[i].signo == SIGRTMIN + 1 ? "RT+1" : "other";
    if (ents[i].code == SI_KERNEL)
      printf(" [%s kernel]", s);
    else
      printf(" [%s code %d band %#lx fd %s]", s, ents[i].code, ents[i].band, ents[i].fd == evfd ? "ok" : "other");
  }
  printf("\n");
  nent = 0;
}

static long bp(uint64_t period, int inherit) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = PERF_TYPE_SOFTWARE;
  a.size = sizeof a;
  a.config = PERF_COUNT_SW_PAGE_FAULTS;
  a.sample_period = period;
  a.wakeup_events = 1;
  a.disabled = 1;
  a.inherit = inherit;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  long r = syscall(SYS_perf_event_open, &a, 0, -1, -1, 0);
  return r < 0 ? -errno : r;
}

// 新しい無名のページを n 枚さわる (1 枚に 1 度のページフォルト)
static void call(int n) {
  long pg = sysconf(_SC_PAGESIZE);
  volatile char *p = mmap(0, n * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  for (int i = 0; i < n; i++)
    p[i * pg] = 1;
  munmap((void *)p, n * pg);
}

static uint64_t count(int fd) {
  uint64_t v = 0;
  read(fd, &v, 8);
  return v;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_sig;
  sa.sa_flags = SA_SIGINFO | SA_RESTART;
  sigaction(SIGIO, &sa, NULL);
  sigaction(SIGRTMIN + 1, &sa, NULL);
  long fd = bp(1, 0);
  evfd = (int)fd;
  long e1 = fcntl(fd, F_SETFL, O_ASYNC) < 0 ? -errno : 0;
  fcntl(fd, F_SETOWN, getpid());
  fcntl(fd, F_SETSIG, SIGRTMIN + 1);
  printf("async: open %s, setfl %ld, O_ASYNC kept %d\n", fd >= 0 ? "ok" : "no", e1, (fcntl(fd, F_GETFL) & O_ASYNC) != 0);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  call(3);
  show("async: three calls");
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  call(2);
  show("async: disabled");
  printf("async: count %llu\n", (unsigned long long)count(fd));
  // REFRESH: 2 回で止まる
  ioctl(fd, PERF_EVENT_IOC_RESET, 0);
  long r = ioctl(fd, PERF_EVENT_IOC_REFRESH, 2) < 0 ? -errno : 0;
  call(5);
  printf("refresh: %ld, count %llu\n", r, (unsigned long long)count(fd));
  show("refresh: five calls");
  r = ioctl(fd, PERF_EVENT_IOC_REFRESH, 1) < 0 ? -errno : 0;
  call(3);
  printf("refresh again: %ld, count %llu\n", r, (unsigned long long)count(fd));
  show("refresh again: three calls");
  // F_SETSIG なし: SIGIO
  fcntl(fd, F_SETSIG, 0);
  ioctl(fd, PERF_EVENT_IOC_REFRESH, 1);
  call(1);
  show("sigio: one call");
  // O_ASYNC なし: 知らせない
  fcntl(fd, F_SETFL, 0);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  call(2);
  show("no async: two calls");
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  close(fd);
  long c = bp(0, 0), in = bp(1, 1);
  long rc = ioctl(c, PERF_EVENT_IOC_REFRESH, 1) < 0 ? -errno : 0;
  long ri = ioctl(in, PERF_EVENT_IOC_REFRESH, 1) < 0 ? -errno : 0;
  printf("bad: counting %ld, inherit %ld\n", rc, ri);
  close(c);
  close(in);
  printf("perf5: %d bad\n", bad);
  return bad;
}
