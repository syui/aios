// mseal (Linux 6.10) が Linux と同じか: flags は 0 だけ (EINVAL)、そろっていない番地と、長さを 1 ページにそろえると
// あふれるものは EINVAL、長さ 0 は 0、すきまがあれば ENOMEM (どこも封じない)。封じた領域は munmap、mprotect、mremap、
// MAP_FIXED の mmap が EPERM (いっしょに頼んだ封じていないところも変えない)、読むだけの無名の領域は捨てる madvise
// (DONTNEED、FREE) も EPERM (書ける領域や NORMAL はよい)。2 度封じても 0。smaps の VmFlags に sl、fork した子もそのまま
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096

static long ms(void *a, size_t n, unsigned long f) { return syscall(462, a, n, f) ? -errno : 0; }
static long e(long r) { return r ? -errno : 0; }

static int sealed_flag(void *a) {
  FILE *f = fopen("/proc/self/smaps", "r");
  char l[512], want[32];
  snprintf(want, sizeof want, "%lx-", (unsigned long)a);
  int in = 0, sl = -1;
  while (f && fgets(l, sizeof l, f)) {
    if ((l[0] >= '0' && l[0] <= '9') || (l[0] >= 'a' && l[0] <= 'f'))
      in = !strncmp(l, want, strlen(want));
    if (in && !strncmp(l, "VmFlags:", 8))
      sl = strstr(l, " sl") != NULL;
  }
  if (f)
    fclose(f);
  return sl;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  char *a = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  munmap(a + 3 * PG, PG);
  printf("checks (flags 1 %ld, unaligned %ld, len 0 %ld, len -1 %ld, gap %ld)\n", ms(a, PG, 1), ms(a + 1, PG, 0), ms(a, 0, 0), ms(a, (size_t)-1, 0), ms(a, 4 * PG, 0));
  printf("after the gap error, munmap of the first page %ld\n", e(munmap(a, PG)));
  char *b = a + PG;
  printf("seal 2 pages %ld, again %ld, smaps sl %d\n", ms(b, 2 * PG, 0), ms(b, 2 * PG, 0), sealed_flag(b));
  printf("sealed (munmap %ld, mprotect %ld, mremap grow %d, mremap move %d, MAP_FIXED %d)\n", e(munmap(b, PG)), e(mprotect(b, PG, PROT_READ)),
         mremap(b, 2 * PG, 4 * PG, 0) == MAP_FAILED ? -errno : 0, mremap(b, PG, PG, MREMAP_MAYMOVE | MREMAP_FIXED, (void *)0x30000000) == MAP_FAILED ? -errno : 0,
         mmap(b, PG, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == MAP_FAILED ? -errno : 0);
  b[0] = 5;
  printf("writable sealed (DONTNEED %ld now %d, FREE %ld, NORMAL %ld)\n", e(madvise(b, PG, MADV_DONTNEED)), b[0], e(madvise(b, PG, MADV_FREE)), e(madvise(b, PG, MADV_NORMAL)));
  char *r = mmap(NULL, PG, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ms(r, PG, 0);
  printf("read-only sealed (DONTNEED %ld, FREE %ld, WILLNEED %ld, DONTFORK %ld)\n", e(madvise(r, PG, MADV_DONTNEED)), e(madvise(r, PG, MADV_FREE)), e(madvise(r, PG, MADV_WILLNEED)),
         e(madvise(r, PG, MADV_DONTFORK)));
  // 封じたものと封じていないものをいっしょに外す
  char *c = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ms(c + PG, PG, 0);
  c[0] = 7;
  long um = e(munmap(c, 2 * PG));
  printf("mixed munmap %ld, unsealed page still there %d\n", um, c[0] == 7);
  pid_t k = fork();
  if (k == 0)
    _exit(munmap(b, PG) == -1 && errno == EPERM ? 0 : 1);
  int st;
  waitpid(k, &st, 0);
  printf("fork child keeps the seal %d\n", WIFEXITED(st) && WEXITSTATUS(st) == 0);
  return 0;
}
