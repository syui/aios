// 大きなロックなしの mmap / munmap (syscall::fast、vm::PageTable::fast_map / fast_unmap) のたしかめ:
//   スレッドが 1 つのプロセスが無名の領域を作って外すのを何千回もくりかえすあいだに、ほかのプロセスが
//   /proc/PID/maps、stat、status を読みつづける (大きなロックを持ってほかの表を読む道: PageTable::hold)。
//   外した真ん中だけ (領域を分ける)、いくつもの領域にかかる munmap、fork したあとの COW のページを外す、
//   ファイルの写しのまざる範囲 (ふつうの道へ回る)、外したところに触ると SIGSEGV、のこりの中身が変わらないこと
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
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
