// madvise の答えが Linux と同じか: そろっていない番地と知らない値は EINVAL、写していないところは ENOMEM、長さ 0 は 0、
// DONTNEED (無名は 0 に)、FREE (無名だけ: ファイルは EINVAL)、WIPEONFORK と KEEPONFORK (私有の無名だけ: 子では 0)、
// DONTFORK と DOFORK (子には領域がない)、NORMAL から WILLNEED、DONTDUMP と DODUMP、COLD と PAGEOUT、POPULATE_READ と
// POPULATE_WRITE (PROT_NONE は ENOMEM、書けないものの WRITE は EFAULT)、REMOVE (私有は EINVAL)、HWPOISON (EPERM)、
// HUGEPAGE や MERGEABLE (手本の設定にないので EINVAL)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int adv(void *a, size_t n, int v) { return madvise(a, n, v) ? -errno : 0; }

// 子で p[0] を読む: 値、領域がなければ -1 (SIGSEGV)
static int child_reads(char *p) {
  int fds[2];
  if (pipe(fds))
    return -9;
  pid_t k = fork();
  if (k == 0) {
    signal(SIGSEGV, SIG_DFL);
    char v = p[0];
    if (write(fds[1], &v, 1) != 1)
      _exit(1);
    _exit(0);
  }
  close(fds[1]);
  char v;
  int st, r = read(fds[0], &v, 1) == 1 ? (unsigned char)v : -1;
  waitpid(k, &st, 0);
  close(fds[0]);
  return r;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  char *a = mmap(NULL, 4 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  munmap(a + 3 * 4096, 4096);
  memset(a, 7, 3 * 4096);
  printf("checks (unaligned %d, unknown 999 %d, unmapped %d, len 0 %d, len 0 unaligned %d, huge len %d)\n", adv(a + 1, 4096, MADV_NORMAL), adv(a, 4096, 999),
         adv(a + 2 * 4096, 2 * 4096, MADV_NORMAL), adv(a, 0, MADV_NORMAL), adv(a + 1, 0, MADV_NORMAL), adv(a, (size_t)-4096, MADV_NORMAL));
  printf("hints (NORMAL %d, RANDOM %d, SEQUENTIAL %d, WILLNEED %d, DONTDUMP %d, DODUMP %d, COLD %d, PAGEOUT %d)\n", adv(a, 4096, MADV_NORMAL), adv(a, 4096, MADV_RANDOM),
         adv(a, 4096, MADV_SEQUENTIAL), adv(a, 4096, MADV_WILLNEED), adv(a, 4096, MADV_DONTDUMP), adv(a, 4096, MADV_DODUMP), adv(a, 4096, MADV_COLD),
         adv(a, 4096, MADV_PAGEOUT));
  printf("not built in (HUGEPAGE %d, NOHUGEPAGE %d, MERGEABLE %d, COLLAPSE %d, HWPOISON %d, SOFT_OFFLINE %d)\n", adv(a, 4096, MADV_HUGEPAGE), adv(a, 4096, MADV_NOHUGEPAGE),
         adv(a, 4096, MADV_MERGEABLE), adv(a, 4096, 25), adv(a, 4096, MADV_HWPOISON), adv(a, 4096, 101));
  int d = adv(a, 4096, MADV_DONTNEED);
  printf("DONTNEED (%d, now %d)\n", d, a[0]);
  memset(a, 7, 4096);
  int f = adv(a, 4096, MADV_FREE);
  int fd = open("/proc/self/exe", O_RDONLY);
  char *file = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
  int ff = adv(file, 4096, MADV_FREE), fr = adv(file, 4096, MADV_REMOVE), fp = adv(a, 4096, MADV_REMOVE);
  printf("FREE (anonymous %d, file %d), REMOVE (file %d, private anonymous %d)\n", f, ff, fr, fp);
  // WIPEONFORK / KEEPONFORK
  memset(a + 4096, 9, 4096);
  int w1 = adv(a + 4096, 4096, MADV_WIPEONFORK), w2 = adv(file, 4096, MADV_WIPEONFORK);
  char *sh = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  int w3 = adv(sh, 4096, MADV_WIPEONFORK);
  int c1 = child_reads(a + 4096), c0 = child_reads(a);
  int k1 = adv(a + 4096, 4096, MADV_KEEPONFORK);
  int c2 = child_reads(a + 4096);
  printf("WIPEONFORK (%d, file %d, shared %d, child %d, the page before %d, KEEPONFORK %d, child %d)\n", w1, w2, w3, c1, c0, k1, c2);
  // DONTFORK / DOFORK
  memset(a + 2 * 4096, 5, 4096);
  int df = adv(a + 2 * 4096, 4096, MADV_DONTFORK);
  int c3 = child_reads(a + 2 * 4096);
  int dof = adv(a + 2 * 4096, 4096, MADV_DOFORK);
  int c4 = child_reads(a + 2 * 4096);
  printf("DONTFORK (%d, child %d, DOFORK %d, child %d)\n", df, c3, dof, c4);
  // POPULATE
  char *none = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  char *ro = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  // 外したすぐあとのところ (Linux は外した番地を次の mmap で使いなおすことがある)
  char *hole = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  munmap(hole + 4096, 4096);
  int unmapped = adv(hole + 4096, 4096, MADV_POPULATE_READ);
  printf("POPULATE (READ %d, WRITE %d, PROT_NONE READ %d, read-only WRITE %d, unmapped %d)\n", adv(a, 4096, MADV_POPULATE_READ), adv(a, 4096, MADV_POPULATE_WRITE),
         adv(none, 4096, MADV_POPULATE_READ), adv(ro, 4096, MADV_POPULATE_WRITE), unmapped);
  return 0;
}
