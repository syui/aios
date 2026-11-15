// mincore が Linux と同じか: 無名のページは触れたものだけ 1、ファイルの写しはページキャッシュにあれば触れなくても 1、
// 書けないほかの人のファイル (/proc/self/exe) はみな 1、そろっていない番地は EINVAL、長さ 0 は 0、すきまは ENOMEM
// (前のところは書く)、vec がおかしければ EFAULT、ユーザーの外は ENOMEM、PROT_NONE にしたページも 1
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define PG 4096

static int mc(void *a, size_t len, unsigned char *v) { return mincore(a, len, v) ? -errno : 0; }

static void show(const char *what, int r, unsigned char *v, int n) {
  printf("%s %d [", what, r);
  for (int i = 0; i < n; i++)
    printf("%d", v[i] & 1);
  printf("]\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  unsigned char v[8];
  char *a = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  a[PG] = 1;
  a[3 * PG] = 1;
  memset(v, 9, sizeof v);
  show("anonymous", mc(a, 4 * PG, v), v, 4);
  memset(v, 9, sizeof v);
  show("len 5000", mc(a, 5000, v), v, 3);
  printf("unaligned %d, len 0 %d, len 0 unmapped %d, bad vec %d, beyond user %d\n", mc(a + 1, PG, v), mc(a, 0, v), mc((void *)0x1000, 0, v), mc(a, PG, (void *)8),
         mc((void *)(1UL << 60), PG, v));
  mprotect(a + PG, PG, PROT_NONE);
  memset(v, 9, sizeof v);
  show("PROT_NONE page", mc(a, 2 * PG, v), v, 2);
  munmap(a + 2 * PG, PG);
  memset(v, 9, sizeof v);
  show("gap", mc(a, 4 * PG, v), v, 4);
  // 自分のファイル: 書いたあとはページキャッシュに
  char path[] = "/tmp/mincoreXXXXXX";
  int fd = mkstemp(path);
  char buf[3 * PG];
  memset(buf, 'x', sizeof buf);
  if (write(fd, buf, sizeof buf) != sizeof buf)
    return 1;
  char *f = mmap(NULL, 4 * PG, PROT_READ, MAP_PRIVATE, fd, 0);
  memset(v, 9, sizeof v);
  show("own file (written, not touched)", mc(f, 4 * PG, v), v, 4);
  unlink(path);
  // ほかの人のファイル
  int ex = open("/proc/self/exe", O_RDONLY);
  char *e = mmap(NULL, 2 * PG, PROT_READ, MAP_PRIVATE, ex, 0);
  memset(v, 9, sizeof v);
  show("not writable file", mc(e, 2 * PG, v), v, 2);
  return 0;
}
