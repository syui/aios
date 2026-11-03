// 大きなロックなしの mmap / munmap (syscall::fast、vm::PageTable::fast_map / fast_unmap) のたしかめ:
//   スレッドが 1 つのプロセスが無名の領域を作って外すのを何千回もくりかえすあいだに、ほかのプロセスが
//   /proc/PID/maps、stat、status を読みつづける (大きなロックを持ってほかの表を読む道: PageTable::hold)。
//   外した真ん中だけ (領域を分ける)、いくつもの領域にかかる munmap、fork したあとの COW のページを外す、
//   ファイルの写しのまざる範囲、外したところに触ると SIGSEGV、のこりの中身が変わらないこと。
//   ファイルの写し (MAP_PRIVATE) の munmap: まるごと (大きなロックなし。inode と名前の Rc は vm::drop_unmapped があとで
//   捨てる)、一部 (ふつうの道)、書いた写し、穴の 0 のページ、消したファイル (外したあと inode が片づく)、何千回も
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/statvfs.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096
static int fails;
#define CHECK(c, ...)                                                                                                  \
  do {                                                                                                                 \
    if (!(c)) {                                                                                                        \
      fails++;                                                                                                         \
      printf("FAIL " __VA_ARGS__);                                                                                     \
      printf(" (line %d, errno %d)\n", __LINE__, errno);                                                               \
    }                                                                                                                  \
  } while (0)

static sigjmp_buf jb;
static void on_segv(int s) { siglongjmp(jb, s); }

// p に触ると SIGSEGV になるか
static int faults(volatile char *p) {
  if (sigsetjmp(jb, 1))
    return 1;
  (void)*p;
  return 0;
}

// 作って、分けて、外すのをくりかえす (自分の中身をたしかめながら)
static int churn(int rounds) {
  int bad = 0;
  for (int i = 0; i < rounds; i++) {
    char *p = mmap(NULL, 8 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
      return 1;
    for (int k = 0; k < 8; k++)
      p[k * PG] = (char)(i + k);
    // 真ん中の 2 ページだけ外す (領域が 2 つに分かれる)
    if (munmap(p + 3 * PG, 2 * PG) != 0)
      bad++;
    for (int k = 0; k < 8; k++)
      if ((k < 3 || k >= 5) && p[k * PG] != (char)(i + k))
        bad++;
    // 2 つの領域と、そのあいだの穴にかかる munmap
    if (munmap(p, 8 * PG) != 0)
      bad++;
    // MAP_STACK と MAP_NORESERVE もおなじ道
    char *s = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK | MAP_NORESERVE, -1, 0);
    if (s == MAP_FAILED)
      return 1;
    s[2 * PG] = 1;
    if (munmap(s, 3 * PG) != 0)
      bad++;
  }
  return bad;
}

int main(void) {
  signal(SIGSEGV, on_segv);

  // 1. 外したところに触ると SIGSEGV、のこりはそのまま
  char *p = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(p != MAP_FAILED, "mmap");
  memset(p, 'x', 4 * PG);
  CHECK(munmap(p + PG, PG) == 0, "munmap middle");
  CHECK(faults(p + PG), "the unmapped page does not fault");
  CHECK(p[0] == 'x' && p[2 * PG] == 'x' && p[3 * PG] == 'x', "the rest changed");
  CHECK(munmap(p, 4 * PG) == 0, "munmap all");
  CHECK(faults(p) && faults(p + 3 * PG), "unmapped pages do not fault");
  // 長さ 0 と、そろっていない場所は EINVAL (ふつうの道と同じ答え)
  CHECK(munmap(p, 0) == -1 && errno == EINVAL, "munmap len 0");
  CHECK(munmap(p + 1, PG) == -1 && errno == EINVAL, "munmap unaligned");
  CHECK(mmap(NULL, 0, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED && errno == EINVAL, "mmap len 0");

  // 2. fork したあとの COW のページを、親と子がそれぞれ外す (子の中身は親の書きかえに影響されない)
  char *c = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(c != MAP_FAILED, "mmap cow");
  c[0] = 'p';
  c[PG] = 'q';
  int pipefd[2];
  CHECK(pipe(pipefd) == 0, "pipe");
  pid_t k = fork();
  if (k == 0) {
    char b;
    int ok = read(pipefd[0], &b, 1) == 1 && c[0] == 'p' && c[PG] == 'q' && munmap(c, PG) == 0 && c[PG] == 'q' && faults(c);
    _exit(ok ? 0 : 1);
  }
  c[0] = 'P';
  CHECK(munmap(c + PG, PG) == 0, "parent munmap cow");
  CHECK(write(pipefd[1], "g", 1) == 1, "write pipe");
  int st;
  CHECK(waitpid(k, &st, 0) == k && WIFEXITED(st) && WEXITSTATUS(st) == 0, "cow child");
  CHECK(c[0] == 'P' && faults(c + PG), "parent cow pages");
  munmap(c, PG);

  // 3. ファイルの写しをまたぐ munmap (ふつうの道へ回る) も外れる
  char path[] = "/tmp/fastmapXXXXXX";
  int fd = mkstemp(path);
  CHECK(fd >= 0 && ftruncate(fd, PG) == 0, "file");
  char *a = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(a != MAP_FAILED, "mmap a");
  CHECK(mmap(a + PG, PG, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0) == a + PG, "mmap file fixed");
  a[0] = 1;
  CHECK(munmap(a, 3 * PG) == 0, "munmap across a file mapping");
  CHECK(faults(a) && faults(a + PG) && faults(a + 2 * PG), "file range not unmapped");
  close(fd);
  unlink(path);

  // 5. ファイルの写し (MAP_PRIVATE) の munmap
  char fpath[] = "fastmap-fileXXXXXX";
  int ff = mkstemp(fpath);
  CHECK(ff >= 0, "file 5");
  char page[PG];
  for (int i = 0; i < 8; i++) {
    memset(page, 'A' + i, PG);
    CHECK(pwrite(ff, page, PG, (off_t)i * PG) == PG, "pwrite %d", i);
  }
  // まるごと外す
  char *m = mmap(NULL, 8 * PG, PROT_READ, MAP_PRIVATE, ff, 0);
  CHECK(m != MAP_FAILED, "mmap file");
  int same = 1;
  for (int i = 0; i < 8; i++)
    same &= m[i * PG] == 'A' + i && m[i * PG + PG - 1] == 'A' + i;
  CHECK(same, "file contents");
  CHECK(munmap(m, 8 * PG) == 0, "munmap whole file mapping");
  CHECK(faults(m) && faults(m + 7 * PG), "whole file mapping still there");
  // 一部を外す (領域を分ける: ふつうの道)。のこりは同じ中身
  m = mmap(NULL, 8 * PG, PROT_READ, MAP_PRIVATE, ff, 0);
  CHECK(m != MAP_FAILED, "mmap file 2");
  CHECK(munmap(m + 2 * PG, 2 * PG) == 0, "munmap part of a file mapping");
  CHECK(faults(m + 2 * PG) && faults(m + 3 * PG), "part still there");
  CHECK(m[0] == 'A' && m[PG] == 'B' && m[4 * PG] == 'E' && m[7 * PG] == 'H', "rest of the file mapping");
  CHECK(munmap(m, 8 * PG) == 0, "munmap the rest");
  // 書いた写し (自分のページ) を外しても、ファイルは変わらない
  m = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, ff, 0);
  CHECK(m != MAP_FAILED, "mmap file rw");
  m[0] = 'z';
  CHECK(munmap(m, 2 * PG) == 0, "munmap written private mapping");
  CHECK(pread(ff, page, 1, 0) == 1 && page[0] == 'A', "private write reached the file");
  // 無名の領域とファイルの写しがならぶ範囲を一度に
  char *n = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(n != MAP_FAILED, "mmap anon 5");
  CHECK(mmap(n + PG, 2 * PG, PROT_READ, MAP_PRIVATE | MAP_FIXED, ff, 2 * PG) == n + PG, "mmap file fixed 5");
  n[0] = 1;
  n[3 * PG] = 1;
  CHECK(n[PG] == 'C' && n[2 * PG] == 'D', "file in the middle");
  CHECK(munmap(n, 4 * PG) == 0, "munmap anon + file");
  CHECK(faults(n) && faults(n + PG) && faults(n + 3 * PG), "anon + file still there");
  // 穴 (0 のページ) を読んで外し、ファイルに書いてから写しなおすと、書いたもの
  CHECK(ftruncate(ff, 12 * PG) == 0, "ftruncate holes");
  m = mmap(NULL, 4 * PG, PROT_READ, MAP_PRIVATE, ff, 8 * PG);
  CHECK(m != MAP_FAILED, "mmap holes");
  CHECK(m[0] == 0 && m[3 * PG] == 0, "holes are not 0");
  CHECK(munmap(m, 4 * PG) == 0, "munmap holes");
  memset(page, 'h', PG);
  CHECK(pwrite(ff, page, PG, 9 * PG) == PG, "pwrite into a hole");
  m = mmap(NULL, 4 * PG, PROT_READ, MAP_PRIVATE, ff, 8 * PG);
  CHECK(m != MAP_FAILED && m[0] == 0 && m[PG] == 'h', "hole written after the mapping went");
  munmap(m, 4 * PG);
  // 何千回も (外したものがたまっても)
  int bad = 0;
  for (int i = 0; i < 3000; i++) {
    m = mmap(NULL, 8 * PG, PROT_READ, MAP_PRIVATE, ff, 0);
    if (m == MAP_FAILED || m[(i % 8) * PG] != 'A' + i % 8 || munmap(m, 8 * PG) != 0)
      bad++;
  }
  CHECK(bad == 0, "file map/unmap loop: %d bad", bad);
  close(ff);
  // 消したファイル: 写しだけが持っているものを外すと、inode が片づく (ファイルの数がもとにもどる)
  struct statvfs v0, v1;
  CHECK(statvfs(".", &v0) == 0, "statvfs");
  char gpath[] = "fastmap-goneXXXXXX";
  int gf = mkstemp(gpath);
  CHECK(gf >= 0 && pwrite(gf, page, PG, 0) == PG, "file gone");
  m = mmap(NULL, PG, PROT_READ, MAP_PRIVATE, gf, 0);
  CHECK(m != MAP_FAILED && m[0] == 'h', "mmap gone");
  close(gf);
  unlink(gpath);
  CHECK(munmap(m, PG) == 0, "munmap gone");
  CHECK(statvfs(".", &v1) == 0 && v1.f_ffree == v0.f_ffree, "inode of the unlinked file not freed: %lu -> %lu", (unsigned long)v0.f_ffree, (unsigned long)v1.f_ffree);
  unlink(fpath);

  // 4. いくつものプロセスが作って外すあいだに、親がそれぞれの /proc/PID/maps、stat、status を読む
  enum { N = 3, ROUNDS = 3000 };
  pid_t kids[N];
  for (int i = 0; i < N; i++) {
    kids[i] = fork();
    if (kids[i] == 0)
      _exit(churn(ROUNDS) ? 1 : 0);
  }
  char buf[65536];
  int left = N, reads = 0;
  for (int r = 0; left > 0; r++) {
    for (int i = 0; i < N; i++) {
      if (kids[i] == 0)
        continue;
      // 終わった子は集める (ゾンビの /proc/PID も開けるので、読めるかでは見ない)
      if (waitpid(kids[i], &st, WNOHANG) == kids[i]) {
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "churn child %d", i);
        kids[i] = 0;
        left--;
        continue;
      }
      const char *names[] = {"maps", "stat", "status"};
      char f[64];
      snprintf(f, sizeof f, "/proc/%d/%s", kids[i], names[r % 3]);
      int pf = open(f, O_RDONLY);
      if (pf >= 0) {
        while (read(pf, buf, sizeof buf) > 0)
          ;
        close(pf);
        reads++;
      }
    }
  }
  CHECK(reads > 0, "no /proc reads while the children ran");

  printf("fastmap: %d failed\n", fails);
  return fails != 0;
}
