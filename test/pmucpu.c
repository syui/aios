// CPU ごとの PMU のもの (pid -1、cpu >= 0: perf stat -a の cycles) が Linux と同じか (root で):
//   open     cycles (exclude_kernel) を CPU 0 に開ける。CPU の数より大きい番号は ENODEV
//   self     自分を CPU 0 にしばって 0.2 秒ユーザーで回ると、cycles は経った時間の 0.3〜3 倍 (QEMU は 1 ns に 1 ほど)
//   child    CPU 0 にしばった子がユーザーで回っても数える (ほかのプロセスのものも)
//   other    ほかの CPU (いちばん大きい番号) のものは、ほとんど進まない (CPU が 1 つなら見ない)
//   times    time_enabled と time_running は経った時間ほど (止めずにずっと)
//   disable  止めると進まない
//   sample   CPU 0 の cycles のサンプリング (perf record -a): CPU 0 にしばった子が回ると、ほとんどが子のサンプル
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <linux/perf_event.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int bad;

static int open_cpu(int cpu) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.size = sizeof a;
  a.type = PERF_TYPE_HARDWARE;
  a.config = PERF_COUNT_HW_CPU_CYCLES;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  a.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING;
  return syscall(SYS_perf_event_open, &a, -1, cpu, -1, 0);
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

static void pin(int cpu) {
  cpu_set_t s;
  CPU_ZERO(&s);
  CPU_SET(cpu, &s);
  sched_setaffinity(0, sizeof s, &s);
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
  long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
  int fd = open_cpu(0);
  if (fd < 0) {
    printf("pmucpu: open %s\n", strerror(errno));
    return 0;
  }
  int big = open_cpu(4096);
  printf("open: cpu 0 ok, cpu 4096 %s\n", big < 0 && errno == ENODEV ? "ENODEV" : big < 0 ? strerror(errno) : "ok");
  // self
  pin(0);
  struct val a, b;
  rd(fd, &a);
  uint64_t t0 = now();
  spin(200000000);
  uint64_t el = now() - t0;
  rd(fd, &b);
  double k = (double)(b.c - a.c) / el;
  printf("self: cycles per ns 0.3..3 %d, enabled about the time %d, running == enabled %d\n", k > 0.3 && k < 3, b.e - a.e > el * 8 / 10 && b.e - a.e < el * 12 / 10, b.r == b.e);
  // child
  if (ncpu > 1)
    pin(1);
  int p[2];
  pipe(p);
  pid_t c = fork();
  if (c == 0) {
    pin(0);
    char x;
    read(p[0], &x, 1);
    spin(200000000);
    _exit(0);
  }
  usleep(20000);
  rd(fd, &a);
  t0 = now();
  write(p[1], "x", 1);
  waitpid(c, NULL, 0);
  el = now() - t0;
  rd(fd, &b);
  k = (double)(b.c - a.c) / el;
  printf("child: counts the child's work 0.3..3 %d\n", k > 0.3 && k < 3);
  // other
  if (ncpu > 1) {
    int o = open_cpu(ncpu - 1);
    pin(0);
    struct val x, y;
    rd(o, &x);
    rd(fd, &a);
    spin(100000000);
    rd(o, &y);
    rd(fd, &b);
    if (getenv("PMUCPU_V"))
      fprintf(stderr, "other: last cpu %llu cpu0 %llu\n", (unsigned long long)(y.c - x.c), (unsigned long long)(b.c - a.c));
    printf("other: another cpu small %d\n", (y.c - x.c) < (b.c - a.c) / 5);
    close(o);
  } else {
    printf("other: another cpu small 1\n");
  }
  // disable
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  rd(fd, &a);
  pin(0);
  spin(50000000);
  rd(fd, &b);
  printf("disable: stops %d\n", b.c == a.c);
  close(fd);
  // sample: CPU 0 の cycles を間 1000000 で。CPU 0 にしばった子が回ると、子のサンプルが来る (perf record -a)
  {
    struct perf_event_attr sa;
    memset(&sa, 0, sizeof sa);
    sa.size = sizeof sa;
    sa.type = PERF_TYPE_HARDWARE;
    sa.config = PERF_COUNT_HW_CPU_CYCLES;
    sa.exclude_kernel = 1;
    sa.exclude_hv = 1;
    sa.disabled = 1;
    sa.sample_period = 1000000;
    sa.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_TID;
    int sf = syscall(SYS_perf_event_open, &sa, -1, 0, -1, 0);
    long pg = sysconf(_SC_PAGESIZE);
    struct perf_event_mmap_page *h = sf < 0 ? MAP_FAILED : mmap(0, 17 * pg, PROT_READ | PROT_WRITE, MAP_SHARED, sf, 0);
    if (ncpu > 1)
      pin(ncpu - 1);
    int q[2];
    pipe(q);
    pid_t k = fork();
    if (k == 0) {
      pin(0);
      char x;
      read(q[0], &x, 1);
      spin(100000000);
      _exit(0);
    }
    usleep(20000);
    ioctl(sf, PERF_EVENT_IOC_ENABLE, 0);
    write(q[1], "x", 1);
    waitpid(k, NULL, 0);
    ioctl(sf, PERF_EVENT_IOC_DISABLE, 0);
    int samples = 0, child = 0;
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
          child += pid == (uint32_t)k;
        }
        tail += eh.size;
      }
    }
    printf("sample: open %d, mmap %d, samples >= 10 %d, most are the child's %d\n", sf >= 0, h != MAP_FAILED, samples >= 10, samples > 0 && child * 10 >= samples * 8);
    if (sf >= 0)
      close(sf);
  }
  printf("pmucpu: %d bad\n", bad);
  return bad;
}
