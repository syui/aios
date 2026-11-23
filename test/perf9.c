// perf のあふれすぎの止め (PERF_RECORD_THROTTLE / UNTHROTTLE) が Linux と同じか (root で):
//   sysctl  perf_event_max_sample_rate を 1000 に (1 tick に ceil(1000 / HZ) まで)。perf_cpu_time_max_percent が 0 なら
//           max_sample_rate は書けない (EINVAL)
//   clock   task-clock を間 100 us で: 1 tick に上限まであふれたら止まり (THROTTLE、そのサンプルは書く)、次の tick で
//           また動く (UNTHROTTLE)。止まっているあいだも数は進む。記録の id と stream_id はイベントの id。止まるまでの
//           サンプルは平均で上限ほど (Linux は HZ 250 で 4、aios は HZ 100 で 10。ほかの CPU へ移ったり順番をゆずったり
//           すると、そのあいだに tick が進んで数えなおすので、1 回ずつは上限をこえることがある)
//   faults  page-faults を間 1 で: 1 回ずつのあふれは止めない (Linux の perf_swevent_overflow の throttle 0)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
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

static long rd_sysctl(const char *name) {
  char p[128];
  snprintf(p, sizeof p, "/proc/sys/kernel/%s", name);
  FILE *f = fopen(p, "r");
  long v = -1;
  if (f) {
    fscanf(f, "%ld", &v);
    fclose(f);
  }
  return v;
}
static long wr_sysctl(const char *name, long v) {
  char p[128], b[32];
  snprintf(p, sizeof p, "/proc/sys/kernel/%s", name);
  int f = open(p, O_WRONLY);
  if (f < 0)
    return -errno;
  int n = snprintf(b, sizeof b, "%ld\n", v);
  long r = write(f, b, n) < 0 ? -errno : 0;
  close(f);
  return r;
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

static long open_ev(uint64_t config, uint64_t period, struct perf_event_mmap_page **h) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = PERF_TYPE_SOFTWARE;
  a.size = sizeof a;
  a.config = config;
  a.sample_period = period;
  a.sample_type = PERF_SAMPLE_TID;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  a.disabled = 1;
  long fd = syscall(SYS_perf_event_open, &a, 0, -1, -1, 0);
  if (fd < 0)
    return -errno;
  *h = mmap(0, 257 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  return fd;
}

struct rec {
  uint32_t type;
  uint64_t a, b, c;
};
static int records(struct perf_event_mmap_page *h, struct rec *out, int max) {
  uint64_t head = __atomic_load_n(&h->data_head, __ATOMIC_ACQUIRE), tail = h->data_tail;
  unsigned char *data = (unsigned char *)h + h->data_offset;
  int n = 0;
  while (tail < head && n < max) {
    struct perf_event_header eh;
    unsigned char b[64];
    for (unsigned i = 0; i < sizeof eh; i++)
      ((unsigned char *)&eh)[i] = data[(tail + i) % h->data_size];
    for (unsigned i = 0; i < sizeof b && i + sizeof eh < eh.size; i++)
      b[i] = data[(tail + sizeof eh + i) % h->data_size];
    out[n].type = eh.type;
    memcpy(&out[n].a, b, 8);
    memcpy(&out[n].b, b + 8, 8);
    memcpy(&out[n].c, b + 16, 8);
    n++;
    tail += eh.size;
  }
  __atomic_store_n(&h->data_tail, tail, __ATOMIC_RELEASE);
  return n;
}

static struct rec r[200000];

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  PG = sysconf(_SC_PAGESIZE);
  long rate0 = rd_sysctl("perf_event_max_sample_rate"), pct0 = rd_sysctl("perf_cpu_time_max_percent");
  printf("sysctl: max_sample_rate set %d, cpu_time_max_percent %ld\n", rate0 > 0, pct0);
  // 0 (止めない) なら max_sample_rate は書けない
  long w0 = wr_sysctl("perf_cpu_time_max_percent", 0);
  long wr = wr_sysctl("perf_event_max_sample_rate", 1000);
  wr_sysctl("perf_cpu_time_max_percent", pct0);
  long w1 = wr_sysctl("perf_event_max_sample_rate", 1000);
  printf("sysctl: percent 0 %ld, then rate %ld, percent back and rate 1000 %ld, now %ld\n", w0, wr, w1, rd_sysctl("perf_event_max_sample_rate"));
  // 時計: 止まる
  struct perf_event_mmap_page *h;
  long fd = open_ev(PERF_COUNT_SW_TASK_CLOCK, 100000, &h);
  uint64_t id = 0;
  ioctl(fd, PERF_EVENT_IOC_ID, &id);
  uint64_t t0 = now();
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  spin(200 * 1000 * 1000);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  uint64_t el = now() - t0, cnt = 0;
  read(fd, &cnt, sizeof cnt);
  int n = records(h, r, 200000);
  int samples = 0, thr = 0, unthr = 0, alternate = 1, ids = 1, last = 0, sample_after_thr = 1;
  for (int i = 0; i < n; i++) {
    if (r[i].type == PERF_RECORD_SAMPLE) {
      samples++;
    } else if (r[i].type == PERF_RECORD_THROTTLE || r[i].type == PERF_RECORD_UNTHROTTLE) {
      int t = r[i].type;
      if (t == last)
        alternate = 0;
      last = t;
      ids &= r[i].b == id && r[i].c == id && r[i].a != 0;
      if (t == PERF_RECORD_THROTTLE) {
        thr++;
        // 止めたサンプルはそのあとに書く
        if (i + 1 >= n || r[i + 1].type != PERF_RECORD_SAMPLE)
          sample_after_thr = 0;
      } else {
        unthr++;
      }
    }
  }
  printf("clock: open %s, throttle >= 3 %d, unthrottle >= 3 %d, alternate %d, ids %d\n", fd >= 0 ? "ok" : strerror(-fd), thr >= 3, unthr >= 3, alternate, ids);
  printf("clock: the sample after throttle %d, samples per throttle <= 20 %d, fewer than 1 per 100 us %d, count near the time %d\n", sample_after_thr, thr > 0 && samples <= 20 * thr,
         (uint64_t)samples * 100000 < el / 2, cnt * 10 >= el * 8);
  munmap(h, 257 * PG);
  close(fd);
  // ページフォルト: 1 回ずつは止めない
  fd = open_ev(PERF_COUNT_SW_PAGE_FAULTS, 1, &h);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  volatile char *p = mmap(0, 3000 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  for (int i = 0; i < 3000; i++)
    p[i * PG] = 1;
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  munmap((void *)p, 3000 * PG);
  cnt = 0;
  read(fd, &cnt, sizeof cnt);
  n = records(h, r, 200000);
  samples = thr = 0;
  for (int i = 0; i < n; i++) {
    samples += r[i].type == PERF_RECORD_SAMPLE;
    thr += r[i].type == PERF_RECORD_THROTTLE;
  }
  printf("faults: no throttle %d, samples == count %d, count >= 3000 %d\n", thr == 0, (uint64_t)samples == cnt, cnt >= 3000);
  munmap(h, 257 * PG);
  close(fd);
  printf("restore: %ld\n", wr_sysctl("perf_event_max_sample_rate", rate0));
  printf("perf9: %d bad\n", bad);
  return bad;
}
