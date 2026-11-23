// perf の CPU ごとのイベント (pid -1、cpu 0: perf stat -a、perf top) が Linux と同じか (root で):
//   count   CPU 0 に固定して、page-faults、context-switches、cpu-clock、task-clock を CPU 0 のみなで数える
//           (ほかのプロセスの分も入るので、少なくともの数で)
//   task    pid と cpu の両方: そのスレッドが CPU 0 で動いたときだけ
//   sample  cpu-clock のサンプリング (間 1 ms。高分解能のタイマーのない手本では tick ごと) と mmap の輪: 自分が忙しく
//           動いたあいだのサンプル
//   bad     ない CPU は EINVAL、pid も cpu も -1 は EINVAL、root でなければ EACCES (paranoid 2)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <linux/perf_event.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int bad;
static long PG;

static long open_ev(uint32_t type, uint64_t config, int pid, int cpu, uint64_t period, int disabled) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = type;
  a.size = sizeof a;
  a.config = config;
  a.sample_period = period;
  a.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_CPU;
  a.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING;
  a.disabled = disabled;
  a.wakeup_events = 1;
  long r = syscall(SYS_perf_event_open, &a, pid, cpu, -1, 0);
  return r < 0 ? -errno : r;
}

struct val {
  uint64_t v, en, run;
};
static struct val rd(long fd) {
  struct val x = {0, 0, 0};
  read(fd, &x, sizeof x);
  return x;
}

static uint64_t now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000000ull + t.tv_nsec;
}

static void touch(int n) {
  volatile char *p = mmap(0, n * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  for (int i = 0; i < n; i++)
    p[i * PG] = 1;
  munmap((void *)p, n * PG);
}

static void spin(uint64_t ns) {
  uint64_t end = now() + ns;
  while (now() < end)
    ;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  PG = sysconf(_SC_PAGESIZE);
  cpu_set_t set;
  CPU_ZERO(&set);
  CPU_SET(0, &set);
  printf("pin: %d, root %d\n", sched_setaffinity(0, sizeof set, &set), getuid() == 0);
  // CPU 0 のみなで数える
  long pf = open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_PAGE_FAULTS, -1, 0, 0, 0);
  long cs = open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CONTEXT_SWITCHES, -1, 0, 0, 0);
  long cc = open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, -1, 0, 0, 0);
  long tc = open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_TASK_CLOCK, -1, 0, 0, 0);
  printf("open: page-faults %s, context-switches %s, cpu-clock %s, task-clock %s\n", pf >= 0 ? "ok" : strerror(-pf), cs >= 0 ? "ok" : strerror(-cs), cc >= 0 ? "ok" : strerror(-cc), tc >= 0 ? "ok" : strerror(-tc));
  uint64_t t0 = now();
  touch(16);
  for (int i = 0; i < 10; i++)
    usleep(2000);
  spin(30 * 1000 * 1000);
  uint64_t el = now() - t0;
  struct val vpf = rd(pf), vcs = rd(cs), vcc = rd(cc), vtc = rd(tc);
  printf("count: page-faults >= 16 %d, context-switches >= 10 %d\n", vpf.v >= 16, vcs.v >= 10);
  printf("count: cpu-clock near the time %d, task-clock near the time %d, running == enabled %d %d\n", vcc.v * 10 >= el * 8, vtc.v * 10 >= el * 8, vcc.run == vcc.en, vpf.run == vpf.en);
  close(pf);
  close(cs);
  close(cc);
  close(tc);
  // そのスレッドが CPU 0 で動いたときだけ
  long tp = open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_PAGE_FAULTS, 0, 0, 0, 1);
  ioctl(tp, PERF_EVENT_IOC_ENABLE, 0);
  touch(8);
  ioctl(tp, PERF_EVENT_IOC_DISABLE, 0);
  printf("task on cpu 0: open %s, page-faults %llu\n", tp >= 0 ? "ok" : strerror(-tp), (unsigned long long)rd(tp).v);
  close(tp);
  // サンプリング: cpu-clock を 1 ms ごとに
  long sp = open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, -1, 0, 1000000, 1);
  struct perf_event_mmap_page *h = sp >= 0 ? mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, sp, 0) : MAP_FAILED;
  ioctl(sp, PERF_EVENT_IOC_ENABLE, 0);
  spin(100 * 1000 * 1000);
  ioctl(sp, PERF_EVENT_IOC_DISABLE, 0);
  int samples = 0, mine = 0, cpu0 = 1;
  if (h != MAP_FAILED) {
    uint64_t head = __atomic_load_n(&h->data_head, __ATOMIC_ACQUIRE), tail = h->data_tail;
    unsigned char *data = (unsigned char *)h + h->data_offset;
    while (tail < head) {
      struct perf_event_header eh;
      for (unsigned i = 0; i < sizeof eh; i++)
        ((unsigned char *)&eh)[i] = data[(tail + i) % h->data_size];
      if (eh.type == PERF_RECORD_SAMPLE) {
        uint32_t pid, cpu;
        for (unsigned i = 0; i < 4; i++) {
          ((unsigned char *)&pid)[i] = data[(tail + 8 + 8 + i) % h->data_size];
          ((unsigned char *)&cpu)[i] = data[(tail + 8 + 16 + i) % h->data_size];
        }
        samples++;
        mine += pid == (uint32_t)getpid();
        cpu0 &= cpu == 0;
      }
      tail += eh.size;
    }
  }
  printf("sample: open %s, samples >= 3 %d, mostly mine %d, all on cpu 0 %d\n", sp >= 0 ? "ok" : strerror(-sp), samples >= 3, mine * 10 >= samples * 8, cpu0);
  close(sp);
  // まちがい
  printf("bad: cpu 4096 %ld, both -1 %ld", open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, -1, 4096, 0, 1), open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, -1, -1, 0, 1));
  pid_t c = fork();
  if (c == 0) {
    setgid(65534);
    setuid(65534);
    long r = open_ev(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, -1, 0, 0, 1);
    _exit(r < 0 ? -r : 0);
  }
  int st;
  waitpid(c, &st, 0);
  printf(", not root -%d\n", WEXITSTATUS(st));
  printf("perf7: %d bad\n", bad);
  return bad;
}
