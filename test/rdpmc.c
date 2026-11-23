// ユーザーから PMU のカウンタを読む (rdpmc: Linux の arm64 の perf_user_access と config1 の rdpmc) が Linux と同じか (root で):
//   sysctl   /proc/sys/kernel/perf_user_access は 0 から。1 を書ける、2 は EINVAL
//   off      0 のまま rdpmc (config1:1) の cycles を開くと、mmap のページの cap_user_rdpmc は 0
//   on       1 にして開くと cap_user_rdpmc 1、index は 0 でなく、pmc_width は 64 (cycles)。ページの offset とカウンタ
//            (PMCCNTR か PMEVCNTR) をユーザーで読んだ数は、前とあとの read のあいだ
//   plain    rdpmc の印のないものは cap_user_rdpmc 0
//   cpu      CPU ごとのもの (pid -1) に rdpmc は EINVAL
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/perf_event.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static int bad;

static int open_cyc(int rdpmc, int pid, int cpu) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.size = sizeof a;
  a.type = PERF_TYPE_HARDWARE;
  a.config = PERF_COUNT_HW_CPU_CYCLES;
  a.config1 = rdpmc ? 2 : 0;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  int fd = syscall(SYS_perf_event_open, &a, pid, cpu, -1, 0);
  return fd < 0 ? -errno : fd;
}

static int sysctl_get(void) {
  char b[16] = "";
  int f = open("/proc/sys/kernel/perf_user_access", O_RDONLY);
  if (f < 0)
    return -errno;
  read(f, b, sizeof b - 1);
  close(f);
  return atoi(b);
}

static int sysctl_set(const char *v) {
  int f = open("/proc/sys/kernel/perf_user_access", O_WRONLY);
  if (f < 0)
    return -errno;
  int r = write(f, v, strlen(v)) < 0 ? -errno : 0;
  close(f);
  return r;
}

static uint64_t read_pmc(unsigned idx) {
  uint64_t v;
  if (idx == 31) {
    __asm__ volatile("mrs %0, pmccntr_el0" : "=r"(v));
  } else {
    __asm__ volatile("msr pmselr_el0, %0; isb; mrs %1, pmxevcntr_el0" : : "r"((uint64_t)idx), "r"(v));
    __asm__ volatile("mrs %0, pmxevcntr_el0" : "=r"(v));
  }
  return v;
}

// Linux の tools/lib/perf の perf_mmap__read_self と同じ読みかた
static int user_read(struct perf_event_mmap_page *pc, uint64_t *out) {
  uint32_t seq, idx;
  uint64_t cnt;
  do {
    seq = __atomic_load_n(&pc->lock, __ATOMIC_ACQUIRE);
    idx = pc->index;
    cnt = pc->offset;
    if (pc->cap_user_rdpmc && idx) {
      uint64_t evcnt = read_pmc(idx - 1);
      uint16_t width = pc->pmc_width;
      evcnt <<= 64 - width;
      evcnt = (uint64_t)((int64_t)evcnt >> (64 - width));
      cnt += evcnt;
    } else {
      return 0;
    }
    __atomic_thread_fence(__ATOMIC_ACQUIRE);
  } while (__atomic_load_n(&pc->lock, __ATOMIC_ACQUIRE) != seq);
  *out = cnt;
  return 1;
}

static uint64_t rd(int fd) {
  uint64_t v = 0;
  read(fd, &v, sizeof v);
  return v;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  long pg = sysconf(_SC_PAGESIZE);
  int orig = sysctl_get();
  printf("sysctl: default %d, set 2 %d, set 1 %d\n", orig, sysctl_set("2"), sysctl_set("1"));
  sysctl_set("0");
  int fd = open_cyc(1, 0, -1);
  struct perf_event_mmap_page *pc = fd < 0 ? MAP_FAILED : mmap(0, pg, PROT_READ, MAP_SHARED, fd, 0);
  printf("off: open %d, cap_user_rdpmc %d\n", fd >= 0, pc != MAP_FAILED ? (int)pc->cap_user_rdpmc : -1);
  if (fd >= 0)
    close(fd);
  sysctl_set("1");
  fd = open_cyc(1, 0, -1);
  pc = fd < 0 ? MAP_FAILED : mmap(0, pg, PROT_READ, MAP_SHARED, fd, 0);
  if (pc != MAP_FAILED) {
    for (volatile int i = 0; i < 1000000; i++) {
    }
    uint64_t a = rd(fd), u = 0;
    int ok = user_read(pc, &u);
    uint64_t b = rd(fd);
    printf("on: cap_user_rdpmc %d, index set %d, pmc_width %u, user read %d, between reads %d\n", (int)pc->cap_user_rdpmc, pc->index != 0, (unsigned)pc->pmc_width, ok, ok && a <= u && u <= b);
  } else {
    printf("on: open %d\n", fd);
  }
  if (fd >= 0)
    close(fd);
  fd = open_cyc(0, 0, -1);
  pc = fd < 0 ? MAP_FAILED : mmap(0, pg, PROT_READ, MAP_SHARED, fd, 0);
  printf("plain: cap_user_rdpmc %d\n", pc != MAP_FAILED ? (int)pc->cap_user_rdpmc : -1);
  if (fd >= 0)
    close(fd);
  int c = open_cyc(1, -1, 0);
  printf("cpu: %s\n", c == -EINVAL ? "EINVAL" : c >= 0 ? "ok" : strerror(-c));
  if (c >= 0)
    close(c);
  char o[8];
  snprintf(o, sizeof o, "%d", orig < 0 ? 0 : orig);
  sysctl_set(o);
  printf("rdpmc: %d bad\n", bad);
  return bad;
}
