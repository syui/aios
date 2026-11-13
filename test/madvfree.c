// redis の起動のたしかめ (src/syscheck.c の checkLinuxMadvFreeForkBug) と同じこと: 3 ページの無名の写し (読むだけ・
// 書ける・読むだけ) のまん中に書き、MADV_FREE してまた書き、fork した子が /proc/self/smaps でそのページの
// Shared_Dirty を読む (4 kB なら正しい。0 なら arm64 の古いカーネルのバグ、読めなければ redis は起きない)。
// test/ldiff.sh madvfree で Linux とくらべる (fork した子の Referenced だけちがう: Linux は子の PTE を old にする。doc/aios.md の
// /proc/PID/smaps)。Shared_Dirty が読めないか 0 なら終わりの番号は 1 (vm-ctests)
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int smaps(unsigned long addr) {
  FILE *f = fopen("/proc/self/smaps", "r");
  if (!f) {
    printf("smaps: cannot open (%s)\n", strerror(errno));
    return 0;
  }
  char buf[256];
  int in = 0, found = 0;
  long sd = -1;
  unsigned long from, to;
  while (fgets(buf, sizeof buf, f)) {
    if (sscanf(buf, "%lx-%lx", &from, &to) == 2) {
      in = from <= addr && addr < to;
      if (in) printf("mapping %s", strchr(buf, ' ') ? strchr(buf, ' ') + 1 : "\n");
      continue;
    }
    if (!in) continue;
    // 行をそのまま (Linux と同じ形か: 名前は 16 文字、数は 8 文字の右よせ)
    printf("  %s", buf);
    if (!strncmp(buf, "Shared_Dirty:", 13)) {
      found = 1;
      sd = atol(buf + 13);
    }
  }
  fclose(f);
  printf("shared_dirty line %s\n", found ? "found" : "missing");
  return found && sd > 0;
}

int main(void) {
  long ps = sysconf(_SC_PAGESIZE);
  char *p = mmap(NULL, 3 * ps, PROT_READ, MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
  if (p == MAP_FAILED) return 1;
  char *q = p + ps;
  printf("mprotect %d\n", mprotect(q, ps, PROT_READ | PROT_WRITE));
  *(volatile char *)q = 0;
  int r = madvise(q, ps, MADV_FREE);
  printf("madvise MADV_FREE %d %s\n", r, r ? strerror(errno) : "");
  *(volatile char *)q = 0;
  fflush(stdout);
  pid_t pid = fork();
  if (pid == 0) {
    int ok = smaps((unsigned long)q);
    fflush(stdout);
    _exit(ok ? 0 : 1);
  }
  int st = 0;
  waitpid(pid, &st, 0);
  return WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 1;
}
