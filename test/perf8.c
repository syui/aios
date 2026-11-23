// perf のサンプリングの間 (attr.freq と PERF_EVENT_IOC_PERIOD) が Linux と同じか:
//   clock   task-clock を freq 1000 で: 時計のものは 1e9 / freq ns の決まった間 (Linux の perf_swevent_init_hrtimer は freq を
//           やめて間にする)。IOC_PERIOD はそのあと間 (ns) を変える (freq の上限を見ない)。サンプルの PERIOD ははじめの間のまま
//   faults  page-faults を freq で: はじめの間は 1、数と時間を見て間をなおしていく (Linux の perf_adjust_period)。
//           サンプルの PERIOD をみな足すと、数えた数とだいたい合う。freq の上限より大きい IOC_PERIOD と 0 は EINVAL
//   fixed   page-faults を決まった間で: 間ごとに 1 つ。IOC_PERIOD で間を変える。PERIOD を読むなら、間で割らずに
//           1 回ごとに 1 つ (PERIOD はその数: Linux の perf_swevent_event、2011 年の「don't divide events」)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <linux/perf_event.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int bad;
static long PG;

static long open_ev(uint64_t config, int freq, uint64_t v, uint64_t st, struct perf_event_mmap_page **h) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = PERF_TYPE_SOFTWARE;
  a.size = sizeof a;
  a.config = config;
  a.freq = freq;
  a.sample_period = v;
  a.sample_type = st;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  a.disabled = 1;
  long fd = syscall(SYS_perf_event_open, &a, 0, -1, -1, 0);
  if (fd < 0)
    return -errno;
  *h = mmap(0, 65 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  return fd;
}

static uint64_t now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000000ull + t.tv_nsec;
}

static void spin(uint64_t ns) {
  uint64_t end = now() + ns;
  while (now() < end)
    ;
}

static void touch(int n) {
  volatile char *p = mmap(0, n * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  for (int i = 0; i < n; i++)
    p[i * PG] = 1;
  munmap((void *)p, n * PG);
}

// サンプルの PERIOD を読む (読んだところまで tail を進める)
static int periods(struct perf_event_mmap_page *h, uint64_t *out, int max) {
  uint64_t head = __atomic_load_n(&h->data_head, __ATOMIC_ACQUIRE), tail = h->data_tail;
  unsigned char *data = (unsigned char *)h + h->data_offset;
  int n = 0;
  while (tail < head) {
    struct perf_event_header eh;
    for (unsigned i = 0; i < sizeof eh; i++)
      ((unsigned char *)&eh)[i] = data[(tail + i) % h->data_size];
    if (eh.type == PERF_RECORD_SAMPLE && n < max) {
      uint64_t p;
      for (unsigned i = 0; i < 8; i++)
        ((unsigned char *)&p)[i] = data[(tail + 8 + 8 + i) % h->data_size];
      out[n++] = p;
    }
    tail += eh.size;
  }
  __atomic_store_n(&h->data_tail, tail, __ATOMIC_RELEASE);
  return n;
}

static int all(uint64_t *p, int n, uint64_t v) {
  for (int i = 0; i < n; i++)
    if (p[i] != v)
      return 0;
  return 1;
}

static uint64_t count(long fd) {
  uint64_t v = 0;
  read(fd, &v, sizeof v);
  return v;
}

static uint64_t ps[100000];

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  PG = sysconf(_SC_PAGESIZE);
  struct perf_event_mmap_page *h;
  // 時計: freq 1000 は 1 ms の決まった間
  const uint64_t TP = PERF_SAMPLE_TID | PERF_SAMPLE_PERIOD;
  long fd = open_ev(PERF_COUNT_SW_TASK_CLOCK, 1, 1000, TP, &h);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  spin(60 * 1000 * 1000);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  int n = periods(h, ps, 100000);
  printf("clock: open %s, samples >= 10 %d, all 1000000 %d\n", fd >= 0 ? "ok" : strerror(-fd), n >= 10, all(ps, n, 1000000));
  // IOC_PERIOD は間 (freq ではない): 上限より大きくてもよい
  long r1 = ioctl(fd, PERF_EVENT_IOC_PERIOD, &(uint64_t){200000}) < 0 ? -errno : 0;
  long r2 = ioctl(fd, PERF_EVENT_IOC_PERIOD, &(uint64_t){20000000}) < 0 ? -errno : 0;
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  spin(80 * 1000 * 1000);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  n = periods(h, ps, 100000);
  printf("clock: IOC_PERIOD 200000 %ld, 20000000 %ld, samples 2..6 %d, still all 1000000 %d\n", r1, r2, n >= 2 && n <= 6, all(ps, n, 1000000));
  munmap(h, 65 * PG);
  close(fd);
  // ページフォルト: freq で間をなおしていく
  fd = open_ev(PERF_COUNT_SW_PAGE_FAULTS, 1, 1000, TP, &h);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  touch(4000);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  uint64_t c = count(fd);
  n = periods(h, ps, 100000);
  uint64_t sum = 0, max = 0;
  for (int i = 0; i < n; i++) {
    sum += ps[i];
    max = ps[i] > max ? ps[i] : max;
  }
  uint64_t last = n ? ps[n - 1] : 0;
  printf("faults: open %s, first 1 %d, grows %d, fewer samples than faults %d\n", fd >= 0 ? "ok" : strerror(-fd), n > 0 && ps[0] == 1, max > 1, (uint64_t)n * 2 < c);
  printf("faults: sum of periods near the count %d\n", c <= sum && c + last >= sum + 1);
  long r3 = ioctl(fd, PERF_EVENT_IOC_PERIOD, &(uint64_t){500}) < 0 ? -errno : 0;
  long r4 = ioctl(fd, PERF_EVENT_IOC_PERIOD, &(uint64_t){1ull << 40}) < 0 ? -errno : 0;
  long r5 = ioctl(fd, PERF_EVENT_IOC_PERIOD, &(uint64_t){0}) < 0 ? -errno : 0;
  printf("faults: IOC_PERIOD freq 500 %ld, too high %ld, 0 %ld\n", r3, r4, r5);
  munmap(h, 65 * PG);
  close(fd);
  // 決まった間: 間ごとに 1 つ (PERIOD なし)
  fd = open_ev(PERF_COUNT_SW_PAGE_FAULTS, 0, 50, PERF_SAMPLE_TID, &h);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  touch(1000);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  c = count(fd);
  n = periods(h, ps, 100000);
  printf("fixed: samples == count / 50 %d\n", (uint64_t)n == c / 50);
  long r6 = ioctl(fd, PERF_EVENT_IOC_PERIOD, &(uint64_t){200}) < 0 ? -errno : 0;
  ioctl(fd, PERF_EVENT_IOC_RESET, 0);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  touch(1000);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  c = count(fd);
  n = periods(h, ps, 100000);
  long r7 = ioctl(fd, PERF_EVENT_IOC_PERIOD, &(uint64_t){1ull << 63}) < 0 ? -errno : 0;
  printf("fixed: IOC_PERIOD 200 %ld, samples == count / 200 %d, 1<<63 %ld\n", r6, (uint64_t)n == c / 200, r7);
  munmap(h, 65 * PG);
  close(fd);
  // PERIOD を読むなら 1 回ごと
  fd = open_ev(PERF_COUNT_SW_PAGE_FAULTS, 0, 50, TP, &h);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  touch(500);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  c = count(fd);
  n = periods(h, ps, 100000);
  printf("fixed with PERIOD: samples == count %d, all 1 %d\n", (uint64_t)n == c, all(ps, n, 1));
  munmap(h, 65 * PG);
  close(fd);
  printf("perf8: %d bad\n", bad);
  return bad;
}
