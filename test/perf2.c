// perf_event_open の PERF_TYPE_BREAKPOINT (ハードウェアのブレークポイントとウォッチポイントで数える) が Linux と同じか:
//   exec    実行のブレークポイントで関数を呼んだ数 (止めているあいだは数えない)。止まらずに続く (当たったら 1 命令越える)
//   watch   書き (W)、読み (R)、両方 (RW) のウォッチポイントと、8 バイトにそろわない 1 バイトのもの
//   inherit fork した子が呼んだ数も
//   bad     まちがった bp_type と bp_len、カーネルの番地、スロットの数より多く (ENOSPC)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <linux/hw_breakpoint.h>
#include <linux/perf_event.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char bpfn[];
__asm__(".text\n.global bpfn\nbpfn:\n  nop\n  ret\n");

static int bad;
static volatile uint64_t var;
static volatile unsigned char bytes[16] __attribute__((aligned(16)));

static long perf_open(struct perf_event_attr *a, pid_t pid) {
  long r = syscall(SYS_perf_event_open, a, pid, -1, -1, 0);
  return r < 0 ? -errno : r;
}

static long bp(uint32_t type, uint64_t addr, uint64_t len, int inherit) {
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = PERF_TYPE_BREAKPOINT;
  a.size = sizeof a;
  a.bp_type = type;
  a.bp_addr = addr;
  a.bp_len = len;
  a.disabled = !inherit;
  a.inherit = inherit;
  a.exclude_kernel = 1;
  a.exclude_hv = 1;
  return perf_open(&a, 0);
}

static uint64_t val(long fd) {
  uint64_t v = 0;
  if (fd >= 0)
    read(fd, &v, 8);
  return v;
}

static void call(int n) {
  for (int i = 0; i < n; i++)
    ((void (*)(void))bpfn)();
}

static void exec_(void) {
  long fd = bp(HW_BREAKPOINT_X, (uint64_t)bpfn, sizeof(long), 0);
  call(2);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  call(5);
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
  call(3);
  uint64_t v = val(fd);
  printf("exec: open %s, counted %llu\n", fd >= 0 ? "ok" : "no", (unsigned long long)v);
  bad |= v != 5;
  // 2 つの同じブレークポイント: どちらも数える
  long a = bp(HW_BREAKPOINT_X, (uint64_t)bpfn, 4, 0), b = bp(HW_BREAKPOINT_X, (uint64_t)bpfn, 4, 0);
  ioctl(a, PERF_EVENT_IOC_ENABLE, 0);
  ioctl(b, PERF_EVENT_IOC_ENABLE, 0);
  call(4);
  printf("exec: two on the same place %llu %llu\n", (unsigned long long)val(a), (unsigned long long)val(b));
  close(a);
  close(b);
  close(fd);
}

static void watch(void) {
  long w = bp(HW_BREAKPOINT_W, (uint64_t)&var, 8, 0);
  long r = bp(HW_BREAKPOINT_R, (uint64_t)&var, 8, 0);
  long rw = bp(HW_BREAKPOINT_RW, (uint64_t)&var, 8, 0);
  ioctl(w, PERF_EVENT_IOC_ENABLE, 0);
  ioctl(r, PERF_EVENT_IOC_ENABLE, 0);
  ioctl(rw, PERF_EVENT_IOC_ENABLE, 0);
  var = 1;
  var = 2;
  var = 3;
  uint64_t x = var + var;
  (void)x;
  ioctl(w, PERF_EVENT_IOC_DISABLE, 0);
  ioctl(r, PERF_EVENT_IOC_DISABLE, 0);
  ioctl(rw, PERF_EVENT_IOC_DISABLE, 0);
  printf("watch: write %llu, read %llu, both %llu, value %llu\n", (unsigned long long)val(w), (unsigned long long)val(r), (unsigned long long)val(rw), (unsigned long long)var);
  bad |= !(val(w) == 3 && val(r) == 2 && val(rw) == 5 && var == 3);
  close(w);
  close(r);
  close(rw);
  // 1 バイト (bytes[3]): となりのバイトの書きこみは数えない
  long one = bp(HW_BREAKPOINT_W, (uint64_t)&bytes[3], 1, 0);
  ioctl(one, PERF_EVENT_IOC_ENABLE, 0);
  bytes[2] = 1;
  bytes[3] = 1;
  bytes[4] = 1;
  bytes[3] = 2;
  printf("watch: one byte %s %llu\n", one >= 0 ? "ok" : "no", (unsigned long long)val(one));
  close(one);
}

static void inherit(void) {
  long fd = bp(HW_BREAKPOINT_X, (uint64_t)bpfn, 4, 1);
  pid_t c = fork();
  if (c == 0) {
    call(3);
    _exit(0);
  }
  int st;
  waitpid(c, &st, 0);
  call(1);
  uint64_t v = val(fd);
  printf("inherit: open %s, child and self %llu, child exit %d\n", fd >= 0 ? "ok" : "no", (unsigned long long)v, WEXITSTATUS(st));
  bad |= v != 4;
  close(fd);
}

static void badbp(void) {
  printf("bad:");
  struct {
    const char *name;
    uint32_t type;
    uint64_t addr, len;
  } t[] = {
      {"type empty", HW_BREAKPOINT_EMPTY, (uint64_t)&var, 8},
      {"type 5", 5, (uint64_t)&var, 8},
      {"type x|w", HW_BREAKPOINT_X | HW_BREAKPOINT_W, (uint64_t)&var, 8},
      {"len 0", HW_BREAKPOINT_W, (uint64_t)&var, 0},
      {"len 3", HW_BREAKPOINT_W, (uint64_t)&var, 3},
      {"len 9", HW_BREAKPOINT_W, (uint64_t)&var, 9},
      {"x len 2", HW_BREAKPOINT_X, (uint64_t)bpfn, 2},
      {"w +6 len 4", HW_BREAKPOINT_W, (uint64_t)&var + 6, 4},
      {"kernel", HW_BREAKPOINT_W, 0xffff800080000000ull, 8},
  };
  for (unsigned k = 0; k < sizeof t / sizeof *t; k++) {
    long r = bp(t[k].type, t[k].addr, t[k].len, 0);
    printf(" [%s %s", t[k].name, r >= 0 ? "ok" : "");
    if (r < 0)
      printf("%ld", r);
    else
      close(r);
    printf("]");
  }
  // カーネルを数える (exclude_kernel なし) は paranoid 2 で EACCES
  struct perf_event_attr a;
  memset(&a, 0, sizeof a);
  a.type = PERF_TYPE_BREAKPOINT;
  a.size = sizeof a;
  a.bp_type = HW_BREAKPOINT_W;
  a.bp_addr = (uint64_t)&var;
  a.bp_len = 8;
  a.disabled = 1;
  printf(" [with kernel %ld]", perf_open(&a, 0));
  // スロットの数まで (実行 6、読み書き 4: QEMU の cortex-a72 と max)
  long fds[8];
  int n = 0;
  long last = 0;
  for (int i = 0; i < 8; i++) {
    last = bp(HW_BREAKPOINT_X, (uint64_t)bpfn + 4 * (i % 2), 4, 0);
    if (last < 0)
      break;
    fds[n++] = last;
  }
  printf(", exec slots %d then %ld", n, last);
  for (int i = 0; i < n; i++)
    close(fds[i]);
  n = 0;
  for (int i = 0; i < 8; i++) {
    last = bp(HW_BREAKPOINT_W, (uint64_t)&var, 8, 0);
    if (last < 0)
      break;
    fds[n++] = last;
  }
  printf(", watch slots %d then %ld", n, last);
  for (int i = 0; i < n; i++)
    close(fds[i]);
  // 閉じれば空く
  long again = bp(HW_BREAKPOINT_W, (uint64_t)&var, 8, 0);
  printf(", after close %s\n", again >= 0 ? "ok" : "no");
  close(again);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  exec_();
  watch();
  inherit();
  badbp();
  printf("perf2: %d bad\n", bad);
  return bad;
}
