// CPU の PMU (ARMv8 PMUv3: perf の cycles ほか) が Linux と同じか (QEMU の virt。数えるのはユーザーのところだけ):
//   sysfs   /sys/bus/event_source/devices の一覧と番号、armv8_pmuv3 の cpus、format、events (PMCEID にあるもの)、caps
//   open    PERF_TYPE_HARDWARE の cycles (PMCCNTR)、PMCEID にないもの・表にないものは ENOENT、範囲の外は EINVAL。
//           HW_CACHE、RAW (event=config の下 16 ビット。CHAIN は EINVAL)、PMU の番号、拡げた番号 (config の上 32 ビット)、
//           exclude_idle は EOPNOTSUPP、threshold は EINVAL
//   count   cycles は動いたあいだだけ進む (QEMU は 1 ns に 1)
//   group   カウンタの数 (cycles と 6 つ) をこえると EINVAL
//   sample  cycles の間 1000000 でサンプリング (あふれの割り込み)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <dirent.h>
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
static int pmu_type = -1;
static const char *pmu_name;
static char pmu_dir[256];

static int cmp(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void readfile(const char *p, char *b, size_t n) {
  int f = open(p, O_RDONLY);
  b[0] = 0;
  if (f < 0) {
    snprintf(b, n, "(%d)", -errno);
    return;
  }
  long r = read(f, b, n - 1);
  close(f);
  b[r > 0 ? r : 0] = 0;
  b[strcspn(b, "\n")] = 0;
}

static int list(const char *dir, char **names, int max) {
  DIR *d = opendir(dir);
  int n = 0;
  struct dirent *e;
  while (d && (e = readdir(d)) && n < max)
    if (e->d_name[0] != '.')
      names[n++] = strdup(e->d_name);
  if (d)
    closedir(d);
  qsort(names, n, sizeof *names, cmp);
  return n;
}

static long try(uint32_t type, uint64_t config, uint64_t config1);

static void sysfs(void) {
  char *names[64], b[256], p[512];
  int n = list("/sys/bus/event_source/devices", names, 64);
  printf("devices:");
  for (int i = 0; i < n; i++) {
    snprintf(p, sizeof p, "/sys/bus/event_source/devices/%s/type", names[i]);
    readfile(p, b, sizeof b);
    printf(" %s=%s", names[i], b);
    if (!strncmp(names[i], "armv8_", 6)) {
      pmu_type = atoi(b);
      pmu_name = strdup(names[i]);
    }
  }
  printf("\n");
  if (pmu_type < 0)
    return;
  snprintf(pmu_dir, sizeof pmu_dir, "/sys/bus/event_source/devices/%s", pmu_name);
  int m = list(pmu_dir, names, 64);
  printf("pmu %s:", pmu_name);
  for (int i = 0; i < m; i++)
    printf(" %s", names[i]);
  printf("\n");
  // CPU の数は VM しだい: 動いている CPU みなか
  snprintf(p, sizeof p, "%s/cpus", pmu_dir);
  readfile(p, b, sizeof b);
  long nc = sysconf(_SC_NPROCESSORS_ONLN);
  char all[32];
  if (nc > 1)
    snprintf(all, sizeof all, "0-%ld", nc - 1);
  else
    snprintf(all, sizeof all, "0");
  printf("cpus: all online %d", !strcmp(b, all));
  snprintf(p, sizeof p, "%s/perf_event_mux_interval_ms", pmu_dir);
  readfile(p, b, sizeof b);
  printf(", mux interval ok %d\n", atoi(b) > 0);
  const char *sub[] = {"format", "caps"};
  for (int s = 0; s < 2; s++) {
    char d[300];
    snprintf(d, sizeof d, "%s/%s", pmu_dir, sub[s]);
    int k = list(d, names, 64);
    printf("%s:", sub[s]);
    for (int i = 0; i < k; i++) {
      snprintf(p, sizeof p, "%s/%s", d, names[i]);
      readfile(p, b, sizeof b);
      printf(" %s=%s", names[i], b);
    }
    printf("\n");
  }
  char d[300];
  snprintf(d, sizeof d, "%s/events", pmu_dir);
  static char *ev[256];
  int k = list(d, ev, 256);
  // 見せるものは CPU しだい (PMCEID。QEMU の cortex-a72 は cpu_cycles だけ、max は stall も): cpu_cycles と、一覧にある
  // ものと PERF_TYPE_HARDWARE で開けるものが合っているか
  int cyc = 0, front = 0, back = 0;
  for (int i = 0; i < k; i++) {
    snprintf(p, sizeof p, "%s/%s", d, ev[i]);
    readfile(p, b, sizeof b);
    if (!strcmp(ev[i], "cpu_cycles"))
      cyc = !strcmp(b, "event=0x0011");
    front |= !strcmp(ev[i], "stall_frontend") && !strcmp(b, "event=0x0023");
    back |= !strcmp(ev[i], "stall_backend") && !strcmp(b, "event=0x0024");
  }
  long f = try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_STALLED_CYCLES_FRONTEND, 0), bk = try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_STALLED_CYCLES_BACKEND, 0);
  printf("events: cpu_cycles=event=0x0011 %d, stall_frontend listed iff it opens %d, stall_backend %d\n", cyc, front == (f >= 0), back == (bk >= 0));
}

static long open_ev(uint32_t type, uint64_t config, uint64_t config1, int disabled, int group, uint64_t period, int excl_idle) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = type;
  a.size = sizeof a;
  a.config = config;
  a.config1 = config1;
  a.disabled = disabled;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  a.exclude_idle = excl_idle;
  a.sample_period = period;
  a.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_TID;
  a.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING;
  long r = syscall(SYS_perf_event_open, &a, 0, -1, group, 0);
  return r < 0 ? -errno : r;
}
static const char *res(long r) {
  static char b[8][32];
  static int i;
  i = (i + 1) % 8;
  if (r >= 0)
    return "ok";
  snprintf(b[i], sizeof b[i], "%s", r == -ENOENT ? "ENOENT" : r == -EINVAL ? "EINVAL" : r == -EOPNOTSUPP ? "EOPNOTSUPP" : r == -EACCES ? "EACCES" : strerror(-r));
  return b[i];
}
static long try(uint32_t type, uint64_t config, uint64_t config1) {
  long r = open_ev(type, config, config1, 1, -1, 0, 0);
  if (r >= 0)
    close(r);
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

struct val {
  uint64_t v, en, run;
};
static struct val rd(long fd) {
  struct val x = {0, 0, 0};
  if (read(fd, &x, sizeof x) != sizeof x)
    x.v = (uint64_t)-1;
  return x;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  PG = sysconf(_SC_PAGESIZE);
  sysfs();
  // 開けるか
  printf("hardware: cycles %s, instructions %s, cache-references %s, branch-instructions %s, branch-misses %s, ref-cycles %s, config 99 %s\n", res(try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES, 0)),
         res(try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS, 0)), res(try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CACHE_REFERENCES, 0)), res(try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_BRANCH_INSTRUCTIONS, 0)),
         res(try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_BRANCH_MISSES, 0)), res(try(PERF_TYPE_HARDWARE, PERF_COUNT_HW_REF_CPU_CYCLES, 0)), res(try(PERF_TYPE_HARDWARE, 99, 0)));
  uint64_t l1d_read = PERF_COUNT_HW_CACHE_L1D | PERF_COUNT_HW_CACHE_OP_READ << 8 | PERF_COUNT_HW_CACHE_RESULT_ACCESS << 16;
  uint64_t l1d_write = PERF_COUNT_HW_CACHE_L1D | PERF_COUNT_HW_CACHE_OP_WRITE << 8 | PERF_COUNT_HW_CACHE_RESULT_ACCESS << 16;
  printf("hw_cache: l1d read %s, l1d write %s, cache 99 %s\n", res(try(PERF_TYPE_HW_CACHE, l1d_read, 0)), res(try(PERF_TYPE_HW_CACHE, l1d_write, 0)), res(try(PERF_TYPE_HW_CACHE, 99, 0)));
  printf("raw: 0x11 %s, 0x08 %s, 0x00 %s, 0x1e %s, 0x10011 %s\n", res(try(PERF_TYPE_RAW, 0x11, 0)), res(try(PERF_TYPE_RAW, 0x08, 0)), res(try(PERF_TYPE_RAW, 0x00, 0)), res(try(PERF_TYPE_RAW, 0x1e, 0)),
         res(try(PERF_TYPE_RAW, 0x10011, 0)));
  if (pmu_type >= 0) {
    printf("pmu type: 0x11 %s, extended cycles %s, extended instructions %s, long %s, threshold %s\n", res(try(pmu_type, 0x11, 0)),
           res(try(PERF_TYPE_HARDWARE, (uint64_t)pmu_type << 32 | PERF_COUNT_HW_CPU_CYCLES, 0)), res(try(PERF_TYPE_HARDWARE, (uint64_t)pmu_type << 32 | PERF_COUNT_HW_INSTRUCTIONS, 0)), res(try(pmu_type, 0x11, 1)),
           res(try(pmu_type, 0x11, 1 << 5)));
  }
  long ei = open_ev(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES, 0, 1, -1, 0, 1);
  printf("exclude_idle: %s\n", res(ei));
  if (ei >= 0)
    close(ei);
  // 数える
  long fd = open_ev(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES, 0, 1, -1, 0, 0);
  if (fd >= 0) {
    struct val v0 = rd(fd);
    ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
    uint64_t t0 = now();
    spin(50 * 1000 * 1000);
    uint64_t el = now() - t0;
    struct val v1 = rd(fd);
    ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
    spin(20 * 1000 * 1000);
    struct val v2 = rd(fd);
    printf("count: before enable 0 %d, cycles per ns 0.5..2 %d, stops when disabled %d, running == enabled %d\n", v0.v == 0, v1.v * 2 >= el && v1.v <= el * 2, v2.v - v1.v < v1.v / 10, v1.run == v1.en);
    ioctl(fd, PERF_EVENT_IOC_RESET, 0);
    printf("reset: %d\n", rd(fd).v == 0);
    close(fd);
  } else {
    printf("count: open %s\n", res(fd));
  }
  // グループ: cycles と、ほかのカウンタ (raw 0x08) を足していく
  long lead = open_ev(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES, 0, 0, -1, 0, 0);
  long m[16];
  int ok = 0;
  long last = 0;
  for (int i = 0; i < 16 && lead >= 0; i++) {
    m[i] = open_ev(PERF_TYPE_RAW, 0x08, 0, 0, lead, 0, 0);
    if (m[i] < 0) {
      last = m[i];
      break;
    }
    ok++;
  }
  printf("group: leader %s, members %d, then %s\n", res(lead), ok, res(last));
  for (int i = 0; i < ok; i++)
    close(m[i]);
  // 止めたメンバーは数えない (Linux の validate_group)
  long dm = lead >= 0 ? open_ev(PERF_TYPE_RAW, 0x08, 0, 1, lead, 0, 0) : -1;
  printf("group: disabled member when full %s\n", res(dm));
  if (dm >= 0)
    close(dm);
  if (lead >= 0)
    close(lead);
  // サンプリング
  fd = open_ev(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES, 0, 1, -1, 1000000, 0);
  if (fd >= 0) {
    struct perf_event_mmap_page *h = mmap(0, 17 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
    spin(50 * 1000 * 1000);
    ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
    int samples = 0, mine = 0;
    if (h != MAP_FAILED) {
      uint64_t head = __atomic_load_n(&h->data_head, __ATOMIC_ACQUIRE), tail = h->data_tail;
      unsigned char *data = (unsigned char *)h + h->data_offset;
      while (tail < head) {
        struct perf_event_header eh;
        for (unsigned i = 0; i < sizeof eh; i++)
          ((unsigned char *)&eh)[i] = data[(tail + i) % h->data_size];
        if (eh.type == PERF_RECORD_SAMPLE) {
          uint32_t pid;
          for (unsigned i = 0; i < 4; i++)
            ((unsigned char *)&pid)[i] = data[(tail + 8 + 8 + i) % h->data_size];
          samples++;
          mine += pid == (uint32_t)getpid();
        }
        tail += eh.size;
      }
    }
    printf("sample: mmap %d, samples >= 10 %d, mine %d\n", h != MAP_FAILED, samples >= 10, samples > 0 && mine == samples);
    close(fd);
  } else {
    printf("sample: open %s\n", res(fd));
  }
  printf("pmu: %d bad\n", bad);
  return bad;
}
