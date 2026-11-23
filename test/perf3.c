// perf_event_open のサンプリング (perf record の形) が Linux と同じか:
//   mmap    輪 (1 + 2^n ページ、MAP_SHARED) の頭のページ (perf_event_mmap_page: version、data_offset、data_size ほか)
//           と、まちがった大きさや MAP_PRIVATE
//   fault   page-faults を 1 回ずつ (sample_period 1): PERF_RECORD_SAMPLE の IP、TID、ADDR、ID、PERIOD を読む
//   bp      ブレークポイントを 1 回ずつ: IP はその番地
//   clock   task-clock を 1 ms ごと: 100 ms 走って 3 こより多く (手本の Linux は高分解能のタイマーがなく jiffies
//           ごと)、時刻はふえる
//   task    task の記録 (fork した子の PERF_RECORD_FORK。inherit のものはスレッドごとだと mmap できない: EINVAL)
//   sigtrap sigtrap: 当たると SIGTRAP (TRAP_PERF、si_perf_data、si_perf_type)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <linux/hw_breakpoint.h>
#include <linux/perf_event.h>
#include <poll.h>
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

#ifndef TRAP_PERF
#define TRAP_PERF 6
#endif

extern char bpfn[];
__asm__(".text\n.global bpfn\nbpfn:\n  nop\n  ret\n");

static int bad;
static long PG;

static long perf_open(struct perf_event_attr *a, pid_t pid) {
  long r = syscall(SYS_perf_event_open, a, pid, -1, -1, PERF_FLAG_FD_CLOEXEC);
  return r < 0 ? -errno : r;
}

static void attr(struct perf_event_attr *a, uint32_t type, uint64_t config, uint64_t period, uint64_t st) {
  memset(a, 0, sizeof *a);
  a->type = type;
  a->size = sizeof *a;
  a->config = config;
  a->sample_period = period;
  a->sample_type = st;
  a->disabled = 1;
  a->exclude_kernel = 1;
  a->exclude_hv = 1;
}

static void spin(int ms) {
  struct timespec t0, t;
  clock_gettime(CLOCK_THREAD_CPUTIME_ID, &t0);
  do
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &t);
  while ((t.tv_sec - t0.tv_sec) * 1000 + (t.tv_nsec - t0.tv_nsec) / 1000000 < ms);
}

// 輪の記録をひとつずつ (頭の data_tail から data_head まで)
struct rec {
  uint32_t type;
  uint16_t misc, size;
  unsigned char body[512];
};
static int records(struct perf_event_mmap_page *h, struct rec *out, int max) {
  uint64_t head = __atomic_load_n(&h->data_head, __ATOMIC_ACQUIRE);
  uint64_t tail = h->data_tail;
  unsigned char *data = (unsigned char *)h + h->data_offset;
  uint64_t size = h->data_size;
  int n = 0;
  while (tail < head && n < max) {
    struct perf_event_header eh;
    for (unsigned i = 0; i < sizeof eh; i++)
      ((unsigned char *)&eh)[i] = data[(tail + i) % size];
    if (eh.size < sizeof eh || eh.size > sizeof(struct rec))
      break;
    out[n].type = eh.type;
    out[n].misc = eh.misc;
    out[n].size = eh.size;
    for (unsigned i = 0; i < eh.size - sizeof eh; i++)
      out[n].body[i] = data[(tail + sizeof eh + i) % size];
    n++;
    tail += eh.size;
  }
  __atomic_store_n(&h->data_tail, tail, __ATOMIC_RELEASE);
  return n;
}
static uint64_t u64at(const unsigned char *b, int o) {
  uint64_t v;
  memcpy(&v, b + o, 8);
  return v;
}
static uint32_t u32at(const unsigned char *b, int o) {
  uint32_t v;
  memcpy(&v, b + o, 4);
  return v;
}

static void mmaps(void) {
  struct perf_event_attr a;
  attr(&a, PERF_TYPE_SOFTWARE, PERF_COUNT_SW_PAGE_FAULTS, 1, PERF_SAMPLE_IP);
  int fd = perf_open(&a, 0);
  void *m = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  struct perf_event_mmap_page *h = m;
  printf("mmap: %s", m == MAP_FAILED ? "no" : "ok");
  if (m != MAP_FAILED) {
    printf(", version %u compat %u, data_offset %llu size %llu, head %llu tail %llu, size field %u, cap_user_time %u zero %u rdpmc %u, index %u",
           h->version, h->compat_version, (unsigned long long)h->data_offset, (unsigned long long)h->data_size, (unsigned long long)h->data_head, (unsigned long long)h->data_tail,
           h->size, (unsigned)h->cap_user_time, (unsigned)h->cap_user_time_zero, (unsigned)h->cap_user_rdpmc, h->index);
    // カウンタを ns にする (arm64 の sched_clock: CPU のタイマ。周波数は VM しだいなので、1 刻みの ns と短い時間の印)
    if (h->cap_user_time_zero) {
      uint64_t one = ((uint64_t)1 << 20) * h->time_mult >> h->time_shift;
      uint64_t cnt, frq;
      __asm__ volatile("mrs %0, cntvct_el0" : "=r"(cnt));
      __asm__ volatile("mrs %0, cntfrq_el0" : "=r"(frq));
      printf(", ns per 2^20 ticks right %d, short %u, mask bits %d", one == ((uint64_t)1 << 20) * 1000000000 / frq, (unsigned)h->cap_user_time_short, 64 - __builtin_clzll(h->time_mask));
      (void)cnt;
    }
  }
  // 2 度目は同じ大きさなら (同じ輪)、ちがえば EINVAL
  void *m2 = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  void *m3 = mmap(0, 5 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  printf(", again %s, other size %d\n", m2 == MAP_FAILED ? "no" : "ok", m3 == MAP_FAILED ? -errno : 0);
  if (m2 != MAP_FAILED)
    munmap(m2, 9 * PG);
  munmap(m, 9 * PG);
  close(fd);
  // まちがった大きさ (1 + 2^n ページでない)、MAP_PRIVATE、ずれた位置、頭だけ (1 ページ)
  int sizes[] = {2, 4, 10};
  printf("mmap bad:");
  for (unsigned k = 0; k < 3; k++) {
    fd = perf_open(&a, 0);
    void *x = mmap(0, sizes[k] * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    printf(" %d pages %d,", sizes[k], x == MAP_FAILED ? -errno : 0);
    if (x != MAP_FAILED)
      munmap(x, sizes[k] * PG);
    close(fd);
  }
  fd = perf_open(&a, 0);
  void *p = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
  printf(" private %d,", p == MAP_FAILED ? -errno : 0);
  void *o = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, PG);
  printf(" offset %d,", o == MAP_FAILED ? -errno : 0);
  void *one = mmap(0, PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  printf(" header only %s\n", one == MAP_FAILED ? "no" : "ok");
  if (one != MAP_FAILED) {
    h = one;
    printf("mmap header only: data_offset %llu size %llu\n", (unsigned long long)h->data_offset, (unsigned long long)h->data_size);
    munmap(one, PG);
  }
  close(fd);
}

static void fault(void) {
  struct perf_event_attr a;
  attr(&a, PERF_TYPE_SOFTWARE, PERF_COUNT_SW_PAGE_FAULTS_MIN, 1, PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_ADDR | PERF_SAMPLE_ID | PERF_SAMPLE_PERIOD);
  int fd = perf_open(&a, 0);
  struct perf_event_mmap_page *h = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  uint64_t id = 0;
  ioctl(fd, PERF_EVENT_IOC_ID, &id);
  char *mem = mmap(0, 16 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  for (int i = 0; i < 16; i++)
    mem[i * PG + 8] = 1;
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  static struct rec r[64];
  int n = records(h, r, 64);
  int samples = 0, addrs = 0, mine = 0, ids = 0, periods = 0, user = 0;
  for (int i = 0; i < n; i++) {
    if (r[i].type != PERF_RECORD_SAMPLE)
      continue;
    samples++;
    // IP、pid と tid、ADDR、ID、PERIOD
    uint64_t addr = u64at(r[i].body, 16);
    addrs += addr >= (uint64_t)mem && addr < (uint64_t)mem + 16 * PG && (addr & (PG - 1)) == 8;
    mine += u32at(r[i].body, 8) == (uint32_t)getpid() && u32at(r[i].body, 12) == (uint32_t)gettid();
    ids += u64at(r[i].body, 24) == id;
    periods += u64at(r[i].body, 32) == 1;
    user += (r[i].misc & PERF_RECORD_MISC_CPUMODE_MASK) == PERF_RECORD_MISC_USER;
    if (r[i].size != 8 + 40)
      printf("(size %u) ", r[i].size);
  }
  uint64_t v = 0;
  read(fd, &v, 8);
  printf("fault: records %d samples %d, addr in the pages %d, pid and tid %d, id %d, period %d, user %d, count %llu\n", n, samples, addrs, mine, ids, periods, user, (unsigned long long)v);
  bad |= !(samples == 16 && addrs == 16);
  munmap(h, 9 * PG);
  munmap(mem, 16 * PG);
  close(fd);
}

static void bp(void) {
  struct perf_event_attr a;
  attr(&a, PERF_TYPE_BREAKPOINT, 0, 1, PERF_SAMPLE_IP | PERF_SAMPLE_ADDR);
  a.bp_type = HW_BREAKPOINT_X;
  a.bp_addr = (uint64_t)bpfn;
  a.bp_len = 4;
  int fd = perf_open(&a, 0);
  struct perf_event_mmap_page *h = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  for (int i = 0; i < 3; i++)
    ((void (*)(void))bpfn)();
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  static struct rec r[16];
  int n = records(h, r, 16);
  int ip = 0, addr = 0;
  for (int i = 0; i < n; i++) {
    ip += r[i].type == PERF_RECORD_SAMPLE && u64at(r[i].body, 0) == (uint64_t)bpfn;
    addr += r[i].type == PERF_RECORD_SAMPLE && u64at(r[i].body, 8) == (uint64_t)bpfn;
  }
  printf("bp: records %d, ip at the breakpoint %d, addr %d\n", n, ip, addr);
  bad |= ip != 3;
  munmap(h, 9 * PG);
  close(fd);
}

static void clock_(void) {
  struct perf_event_attr a;
  attr(&a, PERF_TYPE_SOFTWARE, PERF_COUNT_SW_TASK_CLOCK, 1000000, PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD);
  a.wakeup_events = 1;
  int fd = perf_open(&a, 0);
  struct perf_event_mmap_page *h = mmap(0, 33 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  struct pollfd pf = {fd, POLLIN, 0};
  int p0 = poll(&pf, 1, 0);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  spin(100);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  int p1 = poll(&pf, 1, 0);
  short ev = pf.revents;
  static struct rec r[256];
  int n = records(h, r, 256);
  int samples = 0, rising = 1, mine = 0, periods = 0, cpu0 = 0;
  uint64_t last = 0;
  for (int i = 0; i < n; i++) {
    if (r[i].type != PERF_RECORD_SAMPLE)
      continue;
    samples++;
    uint64_t t = u64at(r[i].body, 16);
    rising &= t > last;
    last = t;
    mine += u32at(r[i].body, 8) == (uint32_t)getpid();
    cpu0 += u32at(r[i].body, 24) < 64 && u32at(r[i].body, 28) == 0;
    periods += u64at(r[i].body, 32) >= 1000000;
  }
  printf("clock: poll before %d, after %d %#x, samples >= 3 %d (< 200 %d), time rising %d, pid %d, cpu %d, period %d\n", p0, p1, ev, samples >= 3, samples < 200, rising, mine == samples,
         cpu0 == samples, periods == samples);
  bad |= !(samples >= 3 && rising);
  munmap(h, 33 * PG);
  close(fd);
}

static void task(void) {
  struct perf_event_attr a;
  attr(&a, PERF_TYPE_SOFTWARE, PERF_COUNT_SW_DUMMY, 0, PERF_SAMPLE_TID);
  a.task = 1;
  a.comm = 1;
  a.inherit = 1;
  a.disabled = 0;
  a.sample_id_all = 1;
  int fi = perf_open(&a, 0);
  void *mi = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fi, 0);
  printf("task: inherit mmap %d\n", mi == MAP_FAILED ? -errno : 0);
  close(fi);
  a.inherit = 0;
  int fd = perf_open(&a, 0);
  struct perf_event_mmap_page *h = mmap(0, 9 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (h == MAP_FAILED) {
    printf("task: mmap %d\n", -errno);
    close(fd);
    return;
  }
  pid_t c = fork();
  if (c == 0) {
    execl("/proc/self/exe", "perf3", "--exit", (char *)0);
    _exit(9);
  }
  int st;
  waitpid(c, &st, 0);
  static struct rec r[32];
  int n = records(h, r, 32);
  printf("task:");
  for (int i = 0; i < n; i++) {
    const unsigned char *b = r[i].body;
    if (r[i].type == PERF_RECORD_FORK || r[i].type == PERF_RECORD_EXIT) {
      // pid、ppid、tid、ptid、time、と sample_id (pid、tid)
      printf(" [%s child %d parent %d sid %d]", r[i].type == PERF_RECORD_FORK ? "fork" : "exit", u32at(b, 0) == (uint32_t)c && u32at(b, 8) == (uint32_t)c,
             u32at(b, 4) == (uint32_t)getpid() || r[i].type == PERF_RECORD_EXIT, u32at(b, 24) == (uint32_t)c);
    } else if (r[i].type == PERF_RECORD_COMM) {
      printf(" [comm %s exec %d pid %d \"%s\"]", "", (r[i].misc & PERF_RECORD_MISC_COMM_EXEC) != 0, u32at(b, 0) == (uint32_t)c, (const char *)b + 8);
    } else {
      printf(" [type %u]", r[i].type);
    }
  }
  printf("\n");
  munmap(h, 9 * PG);
  close(fd);
}

static volatile int got_code, got_type, got_n;
static int trap_fd = -1;
static volatile uint64_t got_data, got_addr;
static void on_trap(int s, siginfo_t *si, void *u) {
  (void)s;
  (void)u;
  // musl の siginfo_t には si_perf_* がない: _sigfault の _addr (16) のあとの _perf の _data (24) と _type (32)
  got_code = si->si_code;
  got_data = u64at((const unsigned char *)si, 24);
  got_type = u32at((const unsigned char *)si, 32);
  got_addr = (uint64_t)si->si_addr;
  got_n++;
  // arm64 のブレークポイントは命令の前で止まるので、止めないと戻ったところでまた当たる
  ioctl(trap_fd, PERF_EVENT_IOC_DISABLE, 0);
}

static void sigtrap_(void) {
  struct sigaction sa = {0};
  sa.sa_sigaction = on_trap;
  sa.sa_flags = SA_SIGINFO;
  sigaction(SIGTRAP, &sa, 0);
  struct perf_event_attr a;
  attr(&a, PERF_TYPE_BREAKPOINT, 0, 1, 0);
  a.bp_type = HW_BREAKPOINT_X;
  a.bp_addr = (uint64_t)bpfn;
  a.bp_len = 4;
  a.sigtrap = 1;
  a.remove_on_exec = 1;
  a.sig_data = 0x1234;
  int fd = perf_open(&a, 0);
  trap_fd = fd;
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  ((void (*)(void))bpfn)();
  ((void (*)(void))bpfn)();
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  uint64_t v = 0;
  read(fd, &v, 8);
  printf("sigtrap: open %s, signals %d code %d data %#llx type %d addr at bp %d, count %llu\n", fd >= 0 ? "ok" : "no", got_n, got_code, (unsigned long long)got_data, got_type,
         got_addr == (uint64_t)bpfn, (unsigned long long)v);
  bad |= got_code != TRAP_PERF;
  close(fd);
  // sigtrap は remove_on_exec がいる
  a.remove_on_exec = 0;
  printf("sigtrap: without remove_on_exec %ld\n", perf_open(&a, 0));
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc > 1 && !strcmp(argv[1], "--exit"))
    return 0;
  PG = sysconf(_SC_PAGESIZE);
  mmaps();
  fault();
  bp();
  clock_();
  task();
  sigtrap_();
  printf("perf3: %d bad\n", bad);
  return bad;
}
