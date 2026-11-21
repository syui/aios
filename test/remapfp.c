// remap_file_pages (Linux 4.0 からは mmap でまねるもの) が Linux と同じか: prot が 0 でなければ EINVAL、長さ 0 や pgoff の
// あふれは EINVAL、写していない・MAP_PRIVATE の領域は EINVAL。MAP_SHARED のファイルの領域なら、その一部をファイルの pgoff
// から写しなおす (中身と /proc/self/maps の位置、書けばファイルのそのページへ)。となりあった同じファイルの領域 (前に
// 写しなおして分かれたもの) はまたいでよく、すきまや別のファイルをまたげば EINVAL。読むだけの写しは読むだけのまま。
// mseal した領域は EPERM
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static long rfp(void *a, size_t n, int prot, size_t pgoff, int fl) {
  return syscall(SYS_remap_file_pages, a, n, prot, pgoff, fl) ? -errno : 0;
}
static long pg;
// p から n ページのそれぞれの頭の字
static void heads(const char *what, char *p, int n) {
  printf("%s:", what);
  for (int i = 0; i < n; i++)
    printf(" %c", p[i * pg]);
  printf("\n");
}
// /proc/self/maps の、p から始まる行の位置 (ページ)
static long off_of(void *p) {
  FILE *f = fopen("/proc/self/maps", "r");
  char l[512];
  long r = -1;
  while (f && fgets(l, sizeof l, f)) {
    unsigned long a, b, o;
    char perm[8];
    if (sscanf(l, "%lx-%lx %7s %lx", &a, &b, perm, &o) == 4 && a == (unsigned long)p)
      r = (long)(o / pg);
  }
  if (f)
    fclose(f);
  return r;
}
static sigjmp_buf jb;
static void onsegv(int s) { siglongjmp(jb, s); }

static int one(const char *dir) {
  char path[256], path2[256];
  snprintf(path, sizeof path, "%s/rfp-a", dir);
  snprintf(path2, sizeof path2, "%s/rfp-b", dir);
  int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644), fd2 = open(path2, O_CREAT | O_RDWR | O_TRUNC, 0644);
  char *buf = malloc(8 * pg);
  for (int i = 0; i < 8; i++)
    memset(buf + i * pg, 'A' + i, pg);
  if (write(fd, buf, 8 * pg) != 8 * pg || write(fd2, buf, 8 * pg) != 8 * pg)
    return 1;
  // 6 ページの場所をとっておき、頭の 4 ページにファイルを写す (5 ページめは穴、6 ページめはあとで別のファイル)
  char *rsv = mmap(NULL, 6 * pg, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  char *m = mmap(rsv, 4 * pg, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, fd, 0);
  munmap(rsv + 4 * pg, 2 * pg);
  char *pr = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
  char *anon = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  printf("checks: prot %ld, size 0 %ld, pgoff wraps %ld, not mapped %ld, private %ld, anon private %ld\n", rfp(m, pg, PROT_READ, 0, 0), rfp(m, 0, 0, 0, 0),
         rfp(m, 2 * pg, 0, (size_t)-1, 0), rfp((void *)0x10000, pg, 0, 0, 0), rfp(pr, pg, 0, 1, 0), rfp(anon, pg, 0, 1, 0));
  heads("before", m, 4);
  printf("remap page 0 to 3: %ld\n", rfp(m, pg, 0, 3, 0));
  heads("after", m, 4);
  printf("  maps offsets: %ld %ld\n", off_of(m), off_of(m + pg));
  printf("unaligned start and size, page 2 to 6: %ld\n", rfp(m + 2 * pg + 100, pg + 100, 0, 6, MAP_NONBLOCK | MAP_SHARED));
  heads("after", m, 4);
  // 分かれた領域をまたいで
  printf("across the pieces, pages 0-2 to 5: %ld\n", rfp(m, 3 * pg, 0, 5, 0));
  heads("after", m, 4);
  // 書くとファイルのそのページへ
  m[0] = 'z';
  char c = 0;
  pread(fd, &c, 1, 5 * pg);
  printf("write through: file page 5 starts %c\n", c);
  // 終わりの先、すきま、別のファイル
  printf("past the end (a hole) %ld\n", rfp(m + 3 * pg, 2 * pg, 0, 0, 0));
  char *n2 = mmap(m + 4 * pg, pg, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, fd2, 0);
  printf("  into another file's mapping %ld (mapped %d)\n", rfp(m + 3 * pg, 2 * pg, 0, 0, 0), n2 == m + 4 * pg);
  // 読むだけ
  char *ro = mmap(NULL, 2 * pg, PROT_READ, MAP_SHARED, fd, 0);
  long r = rfp(ro, pg, 0, 7, 0);
  struct sigaction sa = {0};
  sa.sa_handler = onsegv;
  sigaction(SIGSEGV, &sa, NULL);
  int sig = sigsetjmp(jb, 1);
  if (!sig)
    ro[0] = 'x';
  printf("read-only: %ld, first %c, write gives signal %d\n", r, ro[0], sig);
  // mseal
  char *sl = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  long ms = syscall(462, sl, 2 * pg, 0);
  printf("sealed: mseal %ld, remap %ld, first %c\n", ms ? (long)-errno : 0L, rfp(sl, pg, 0, 4, 0), sl[0]);
  unlink(path);
  unlink(path2);
  return 0;
}

// 引数のディレクトリで順に (なければ /tmp)
int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  pg = sysconf(_SC_PAGESIZE);
  int r = 0;
  for (int i = 1; i < argc; i++)
    r |= one(argv[i]);
  return argc > 1 ? r : one("/tmp");
}
