// perf_event_open (241) のソフトウェアの数が Linux と同じか:
//   knobs   /proc/sys/kernel/perf_event_paranoid ほかと、/sys/bus/event_source/devices の種類の番号
//   open    task-clock を止めたまま開く (exclude_kernel がなければ paranoid 2 で EACCES)、fd の印 (close-on-exec)、
//           /proc/self/fd のリンクの名前、fdinfo
//   count   ENABLE / DISABLE / RESET (ioctl) と read (TOTAL_TIME_ENABLED / RUNNING、ID)、ページフォルトと
//           コンテキストスイッチの数、PR_TASK_PERF_EVENTS_DISABLE / ENABLE
//   group   リーダーとメンバー (group_fd)、PERF_FORMAT_GROUP の read
//   inherit inherit で fork した子の分も数える。perf stat の形: exec を待つ子に enable_on_exec で開き、exec してから数える
//   bad     まちがった attr (size、type、config、flags、pid、cpu、read_format)、短い read、書きこみ、ハードウェアの数 (手本の
//           Linux は ARM_PMU を切っているので、aios と同じく PMU がない: ENOENT)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/perf_event.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int bad;

static long perf_open(struct perf_event_attr *a, pid_t pid, int cpu, int group, unsigned long flags) {
  long r = syscall(SYS_perf_event_open, a, pid, cpu, group, flags);
  return r < 0 ? -errno : r;
}

static void sw(struct perf_event_attr *a, uint64_t config) {
  memset(a, 0, sizeof *a);
  a->type = PERF_TYPE_SOFTWARE;
  a->size = sizeof *a;
  a->config = config;
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

static void readfile(const char *p, char *b, size_t n) {
  int fd = open(p, O_RDONLY);
  b[0] = 0;
  if (fd < 0) {
    snprintf(b, n, "(%d)", -errno);
    return;
  }
  long k = read(fd, b, n - 1);
  close(fd);
  b[k > 0 ? k : 0] = 0;
  char *e = strchr(b, '\n');
  if (e)
    *e = 0;
}

static int cmp(const void *a, const void *b) {
  return strcmp(*(char *const *)a, *(char *const *)b);
}

static void knobs(void) {
  const char *k[] = {"perf_event_paranoid", "perf_event_max_sample_rate", "perf_event_mlock_kb", "perf_event_max_stack", "perf_cpu_time_max_percent", "perf_event_max_contexts_per_stack"};
  char b[64];
  printf("knobs:");
  for (unsigned i = 0; i < sizeof k / sizeof *k; i++) {
    char p[128];
    snprintf(p, sizeof p, "/proc/sys/kernel/%s", k[i]);
    readfile(p, b, sizeof b);
    printf(" %s=%s", k[i], b);
  }
  printf("\n");
  // 種類 (PERF_TYPE_*) の一覧: CPU のもの (armv8_* など) は機械しだいなので、ソフトウェアのものだけ
  printf("event_source:");
  DIR *d = opendir("/sys/bus/event_source/devices");
  if (!d) {
    printf(" (%d)\n", -errno);
    return;
  }
  char *names[32];
  int n = 0;
  struct dirent *e;
  while ((e = readdir(d)) && n < 32)
    if (e->d_name[0] != '.')
      names[n++] = strdup(e->d_name);
  closedir(d);
  qsort(names, n, sizeof *names, cmp);
  for (int i = 0; i < n; i++) {
    const char *sw_names[] = {"software", "tracepoint", "breakpoint", "kprobe", "uprobe"};
    int known = 0;
    for (unsigned j = 0; j < 5; j++)
      known |= !strcmp(names[i], sw_names[j]);
    if (!known)
      continue;
    char p[128];
    snprintf(p, sizeof p, "/sys/bus/event_source/devices/%s/type", names[i]);
    readfile(p, b, sizeof b);
    printf(" %s=%s", names[i], b);
  }
  printf("\n");
  for (int i = 0; i < n; i++)
    free(names[i]);
  // ディレクトリの中 (並べて) とリンク
  const char *dirs[] = {"/sys/bus/event_source", "/sys/bus/event_source/devices/software", "/sys/devices/software", "/sys/devices/breakpoint"};
  for (unsigned k = 0; k < 4; k++) {
    printf("ls %s:", dirs[k]);
    d = opendir(dirs[k]);
    if (!d) {
      printf(" (%d)\n", -errno);
      continue;
    }
    n = 0;
    while ((e = readdir(d)) && n < 32)
      if (e->d_name[0] != '.')
        names[n++] = strdup(e->d_name);
    closedir(d);
    qsort(names, n, sizeof *names, cmp);
    for (int i = 0; i < n; i++) {
      printf(" %s", names[i]);
      free(names[i]);
    }
    printf("\n");
  }
  char l[128];
  long ln = readlink("/sys/bus/event_source/devices/software", l, sizeof l - 1);
  l[ln > 0 ? ln : 0] = 0;
  readfile("/sys/devices/software/perf_event_mux_interval_ms", b, sizeof b);
  printf("link %s, mux %s\n", l, b);
}

static void openit(void) {
  struct perf_event_attr a;
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.exclude_kernel = 0;
  long k = perf_open(&a, 0, -1, -1, 0);
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  long fd = perf_open(&a, 0, -1, -1, PERF_FLAG_FD_CLOEXEC);
  int fl = fd >= 0 ? fcntl(fd, F_GETFD) : -1;
  int fl2 = fd >= 0 ? fcntl(fd, F_GETFL) : -1;
  char p[64], l[128] = "";
  snprintf(p, sizeof p, "/proc/self/fd/%ld", fd);
  long ln = readlink(p, l, sizeof l - 1);
  l[ln > 0 ? ln : 0] = 0;
  printf("open: with kernel %ld, fd %s, cloexec %d, flags %#o, link %s\n", k, fd >= 0 ? "ok" : "no", fl, fl2 & ~O_LARGEFILE, l);
  bad |= fd < 0;
  close(fd);
}

struct rf {
  uint64_t value, enabled, running, id;
};

static void count(void) {
  struct perf_event_attr a;
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING | PERF_FORMAT_ID;
  int fd = perf_open(&a, 0, -1, -1, PERF_FLAG_FD_CLOEXEC);
  struct rf r0, r1, r2, r3;
  long n0 = read(fd, &r0, sizeof r0);
  long e = ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  spin(50);
  long d = ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  long n1 = read(fd, &r1, sizeof r1);
  spin(20);
  read(fd, &r2, sizeof r2);
  uint64_t id = 0;
  long ir = ioctl(fd, PERF_EVENT_IOC_ID, &id);
  long rs = ioctl(fd, PERF_EVENT_IOC_RESET, 0);
  read(fd, &r3, sizeof r3);
  printf("count: before %ld value %llu, enable %ld disable %ld, read %ld, 50 ms of task-clock %s (enabled %d running %d), stopped %d, id %ld same %d, reset %ld value %llu enabled kept %d\n", n0,
         (unsigned long long)r0.value, e, d, n1, r1.value >= 40000000 && r1.value < 1000000000 ? "ok" : "no", r1.enabled >= r1.value, r1.running == r1.enabled, r2.value == r1.value, ir, id == r1.id,
         rs, (unsigned long long)r3.value, r3.enabled == r1.enabled);
  bad |= !(r1.value >= 40000000 && r2.value == r1.value && r3.value == 0);
  close(fd);
  // ページフォルト (はじめてさわるページの数) とコンテキストスイッチ (眠った回数)
  sw(&a, PERF_COUNT_SW_PAGE_FAULTS);
  int pf = perf_open(&a, 0, -1, -1, 0);
  sw(&a, PERF_COUNT_SW_PAGE_FAULTS_MIN);
  int pfmin = perf_open(&a, 0, -1, -1, 0);
  sw(&a, PERF_COUNT_SW_CONTEXT_SWITCHES);
  int cs = perf_open(&a, 0, -1, -1, 0);
  char *m = mmap(0, 64 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ioctl(pf, PERF_EVENT_IOC_ENABLE, 0);
  ioctl(pfmin, PERF_EVENT_IOC_ENABLE, 0);
  ioctl(cs, PERF_EVENT_IOC_ENABLE, 0);
  for (int i = 0; i < 64; i++)
    m[i * 4096] = 1;
  for (int i = 0; i < 5; i++)
    usleep(2000);
  ioctl(pf, PERF_EVENT_IOC_DISABLE, 0);
  ioctl(pfmin, PERF_EVENT_IOC_DISABLE, 0);
  ioctl(cs, PERF_EVENT_IOC_DISABLE, 0);
  uint64_t vpf = 0, vmin = 0, vcs = 0;
  read(pf, &vpf, 8);
  read(pfmin, &vmin, 8);
  read(cs, &vcs, 8);
  // コンテキストスイッチはカーネルの中で起きるので、exclude_kernel では数えない (paranoid 2 では root でないと外せない)
  printf("count: page faults >= 64 %d (min %d), context switches with exclude_kernel %llu", vpf >= 64 && vpf < 80, vmin == vpf, (unsigned long long)vcs);
  bad |= !(vpf >= 64 && vcs == 0);
  // PR_TASK_PERF_EVENTS_DISABLE / ENABLE: 自分が開いたものをみな止める・動かす
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  int tc = perf_open(&a, 0, -1, -1, 0);
  ioctl(tc, PERF_EVENT_IOC_ENABLE, 0);
  long pd = prctl(PR_TASK_PERF_EVENTS_DISABLE, 0, 0, 0, 0);
  uint64_t v1 = 0, v2 = 0, v3 = 0;
  read(tc, &v1, 8);
  spin(20);
  read(tc, &v2, 8);
  long pe = prctl(PR_TASK_PERF_EVENTS_ENABLE, 0, 0, 0, 0);
  spin(20);
  read(tc, &v3, 8);
  printf(", prctl disable %ld: stopped %d, enable %ld: more %d\n", pd, v2 == v1, pe, v3 >= v2 + 15000000);
  bad |= !(v2 == v1 && v3 >= v2 + 15000000);
  close(tc);
  close(pf);
  close(pfmin);
  close(cs);
  munmap(m, 64 * 4096);
}

static void group(void) {
  struct perf_event_attr a;
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.read_format = PERF_FORMAT_GROUP | PERF_FORMAT_ID;
  int lead = perf_open(&a, 0, -1, -1, 0);
  sw(&a, PERF_COUNT_SW_CONTEXT_SWITCHES);
  a.disabled = 0;
  a.read_format = PERF_FORMAT_GROUP | PERF_FORMAT_ID;
  int mem = perf_open(&a, 0, -1, lead, 0);
  ioctl(lead, PERF_EVENT_IOC_ENABLE, 0);
  spin(20);
  usleep(2000);
  ioctl(lead, PERF_EVENT_IOC_DISABLE, 0);
  uint64_t g[8] = {0}, id0 = 0, id1 = 0;
  long n = read(lead, g, sizeof g);
  ioctl(lead, PERF_EVENT_IOC_ID, &id0);
  ioctl(mem, PERF_EVENT_IOC_ID, &id1);
  uint64_t small[3];
  long sn = read(lead, small, sizeof small);
  sn = sn < 0 ? -errno : sn;
  printf("group: member %s, read %ld nr %llu, leader %d id %d, member %d id %d, short read %ld\n", mem >= 0 ? "ok" : "no", n, (unsigned long long)g[0], g[1] >= 15000000, g[2] == id0, g[3] >= 1,
         g[4] == id1, sn);
  bad |= !(n == 40 && g[0] == 2);
  // GROUP でない read はメンバーのものだけ
  uint64_t one[2] = {0};
  long mn = read(mem, one, sizeof one);
  mn = mn < 0 ? -errno : mn;
  printf("group: member read %ld same %d\n", mn, one[0] == g[3]);
  close(mem);
  close(lead);
}

static void badattr(void) {
  struct perf_event_attr a;
  printf("bad:");
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.size = 8;
  printf(" size 8 %ld (size %u)", perf_open(&a, 0, -1, -1, 0), a.size);
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.size = 0;
  long z = perf_open(&a, 0, -1, -1, 0);
  printf(" size 0 %s", z >= 0 ? "ok" : "no");
  if (z >= 0)
    close(z);
  // 大きい size: 知らないところが 0 でなければ E2BIG (size に知っている大きさを書く)
  unsigned char big[sizeof a + 16];
  memset(big, 0, sizeof big);
  sw((struct perf_event_attr *)big, PERF_COUNT_SW_TASK_CLOCK);
  ((struct perf_event_attr *)big)->size = sizeof big;
  long b1 = perf_open((struct perf_event_attr *)big, 0, -1, -1, 0);
  printf(" big zero %s", b1 >= 0 ? "ok" : "no");
  if (b1 >= 0)
    close(b1);
  big[sizeof a + 4] = 1;
  long b2 = perf_open((struct perf_event_attr *)big, 0, -1, -1, 0);
  printf(" big junk %s (size %u)", b2 >= 0 ? "ok" : "no", ((struct perf_event_attr *)big)->size);
  if (b2 >= 0)
    close(b2);
  struct {
    const char *name;
    uint32_t type;
    uint64_t config;
    int pid, cpu;
    unsigned long flags;
    uint64_t rf;
  } t[] = {
      {"type 99", 99, 0, 0, -1, 0, 0},
      {"sw config 99", PERF_TYPE_SOFTWARE, 99, 0, -1, 0, 0},
      {"cycles (no PMU)", PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES, 0, -1, 0, 0},
      {"cache (no PMU)", PERF_TYPE_HW_CACHE, 0, 0, -1, 0, 0},
      {"raw (no PMU)", PERF_TYPE_RAW, 0x11, 0, -1, 0, 0},
      {"pid -1 cpu -1", PERF_TYPE_SOFTWARE, 0, -1, -1, 0, 0},
      {"no such pid", PERF_TYPE_SOFTWARE, 0, 999999, -1, 0, 0},
      {"cpu 99", PERF_TYPE_SOFTWARE, 0, 0, 99, 0, 0},
      {"cpu 0 (all tasks)", PERF_TYPE_SOFTWARE, 0, -1, 0, 0, 0},
      {"flags 0x40", PERF_TYPE_SOFTWARE, 0, 0, -1, 0x40, 0},
      {"read_format 0x100", PERF_TYPE_SOFTWARE, 0, 0, -1, 0, 0x100},
      {"dummy", PERF_TYPE_SOFTWARE, PERF_COUNT_SW_DUMMY, 0, -1, 0, 0},
      {"bpf-output", PERF_TYPE_SOFTWARE, PERF_COUNT_SW_BPF_OUTPUT, 0, -1, 0, 0},
  };
  for (unsigned k = 0; k < sizeof t / sizeof *t; k++) {
    sw(&a, t[k].config);
    a.type = t[k].type;
    a.read_format = t[k].rf;
    long r = perf_open(&a, t[k].pid, t[k].cpu, -1, t[k].flags);
    printf(", %s %s", t[k].name, r >= 0 ? "ok" : "");
    if (r < 0)
      printf("%ld", r);
    else
      close(r);
  }
  // 短い read と write
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  int fd = perf_open(&a, 0, -1, -1, 0);
  char c[4];
  long sr = read(fd, c, 4);
  sr = sr < 0 ? -errno : sr;
  long w = write(fd, c, 4);
  w = w < 0 ? -errno : w;
  long ub = ioctl(fd, 0x1234, 0);
  ub = ub < 0 ? -errno : ub;
  long lk = lseek(fd, 0, SEEK_SET);
  lk = lk < 0 ? -errno : lk;
  close(fd);
  printf(", read 4 %ld, write %ld, ioctl %ld, lseek %ld\n", sr, w, ub, lk);
}

static void inherit(void) {
  struct perf_event_attr a;
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.disabled = 0;
  a.inherit = 1;
  int fd = perf_open(&a, 0, -1, -1, 0);
  uint64_t v0 = 0, v1 = 0;
  read(fd, &v0, 8);
  pid_t c = fork();
  if (c == 0) {
    spin(40);
    _exit(0);
  }
  int st;
  waitpid(c, &st, 0);
  read(fd, &v1, 8);
  // 自分はほとんど走っていないので、子の 40 ms が入っているか
  printf("inherit: child's time counted %d", v1 - v0 >= 35000000 && v1 - v0 < 1000000000);
  bad |= !(v1 - v0 >= 35000000);
  close(fd);
  // inherit がないと子の分は入らない
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.disabled = 0;
  fd = perf_open(&a, 0, -1, -1, 0);
  read(fd, &v0, 8);
  c = fork();
  if (c == 0) {
    spin(40);
    _exit(0);
  }
  waitpid(c, &st, 0);
  read(fd, &v1, 8);
  printf(", without inherit %d", v1 - v0 < 30000000);
  close(fd);
  // enable_on_exec: 子は exec の前に 100 ms 走り、exec したプログラムで 40 ms 走る (前のものが入れば 140 ms をこえる。
  // ほかのテストといっしょで遅い VM でも、exec と 40 ms は 100 ms より下)
  int pp[2];
  pipe(pp);
  c = fork();
  if (c == 0) {
    char b;
    close(pp[1]);
    read(pp[0], &b, 1);
    spin(100);
    execl("/proc/self/exe", "perf", "--spin", (char *)0);
    _exit(9);
  }
  close(pp[0]);
  sw(&a, PERF_COUNT_SW_TASK_CLOCK);
  a.enable_on_exec = 1;
  a.inherit = 1;
  a.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED;
  fd = perf_open(&a, c, -1, -1, PERF_FLAG_FD_CLOEXEC);
  write(pp[1], "x", 1);
  close(pp[1]);
  waitpid(c, &st, 0);
  uint64_t r[2] = {0};
  long n = read(fd, r, sizeof r);
  printf(", enable_on_exec: open %s, read %ld, counted after exec %d (not before %d), exit %d\n", fd >= 0 ? "ok" : "no", n, r[0] >= 35000000, r[0] < 100000000, WEXITSTATUS(st));
  bad |= !(r[0] >= 35000000 && r[0] < 100000000);
  close(fd);
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc > 1 && !strcmp(argv[1], "--spin")) {
    spin(40);
    return 0;
  }
  knobs();
  openit();
  count();
  group();
  inherit();
  badattr();
  printf("perf: %d bad\n", bad);
  return bad;
}
