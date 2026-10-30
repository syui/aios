// fork のページ表の写し (大きなロックの外で PTE を写す vm::PageTable::prefork) と、exit・exec で前の表を返すのを
// 大きなロックを放してから (vm::PageTable::release_deferred) のたしかめ:
//   fork したときの中身を子が見る (親があとで書いても変わらない)、子が書いても親は変わらない (COW)、
//   MAP_SHARED の無名とファイルの写しは親と子で同じページ、fork した子が exec する (写しを捨てる)、
//   いくつものプロセスが fork と書きこみをくりかえすあいだに、親がそれぞれの /proc/PID/maps、status、cmdline を読む
//   (大きなロックを持ってほかの表を読む道: PageTable::hold)
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096
#define NPG 32
static int fails;
#define CHECK(c, ...)                                                                                                  \
  do {                                                                                                                 \
    if (!(c)) {                                                                                                        \
      fails++;                                                                                                         \
      printf("FAIL " __VA_ARGS__);                                                                                     \
      printf(" (line %d, errno %d)\n", __LINE__, errno);                                                               \
    }                                                                                                                  \
  } while (0)

static void fill(volatile char *p, char v) {
  for (int k = 0; k < NPG; k++)
    p[k * PG] = (char)(v + k);
}

static int same(volatile char *p, char v) {
  for (int k = 0; k < NPG; k++)
    if (p[k * PG] != (char)(v + k))
      return 0;
  return 1;
}

// fork して、子は fork したときの中身を見て、自分で書きかえる。親は fork のすぐあとに書きかえる。rounds 回
static int churn(int rounds, int me) {
  char *p = mmap(NULL, NPG * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (p == MAP_FAILED)
    return 1;
  int bad = 0;
  for (int i = 0; i < rounds; i++) {
    char v = (char)(me * 64 + i);
    fill(p, v);
    pid_t k = fork();
    if (k < 0)
      return bad + 1;
    if (k == 0) {
      int ok = same(p, v);
      fill(p, (char)(v + 100));
      ok = ok && same(p, (char)(v + 100));
      _exit(ok ? 0 : 1);
    }
    fill(p, (char)(v + 50));
    int st;
    if (waitpid(k, &st, 0) != k || !WIFEXITED(st) || WEXITSTATUS(st) != 0)
      bad++;
    if (!same(p, (char)(v + 50)))
      bad++;
  }
  return bad;
}

int main(int argc, char **argv) {
  // 3. で exec される: なにもせずに終わる
  if (argc > 1 && !strcmp(argv[1], "noop"))
    return 0;

  // 1. 子は fork したときの中身を見る。子が書いても親は変わらない
  char *p = mmap(NULL, NPG * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(p != MAP_FAILED, "mmap");
  fill(p, 'a');
  int pipefd[2];
  CHECK(pipe(pipefd) == 0, "pipe");
  pid_t k = fork();
  if (k == 0) {
    char b;
    // 親が書きかえたあとで見る
    int ok = read(pipefd[0], &b, 1) == 1 && same(p, 'a');
    fill(p, 'c');
    _exit(ok && same(p, 'c') ? 0 : 1);
  }
  fill(p, 'b');
  CHECK(write(pipefd[1], "g", 1) == 1, "write pipe");
  int st;
  CHECK(waitpid(k, &st, 0) == k && WIFEXITED(st) && WEXITSTATUS(st) == 0, "child saw the memory as it was at fork");
  CHECK(same(p, 'b'), "the child's writes reached the parent");

  // 2. MAP_SHARED の無名の領域とファイルの写しは、親と子で同じページ (子が書いたものを親が見る)
  char *s = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  CHECK(s != MAP_FAILED, "mmap shared");
  char path[] = "/tmp/forkcowXXXXXX";
  int fd = mkstemp(path);
  CHECK(fd >= 0 && ftruncate(fd, 2 * PG) == 0, "file");
  char *f = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  CHECK(f != MAP_FAILED, "mmap file shared");
  s[0] = 's';
  f[PG] = 'f';
  k = fork();
  if (k == 0) {
    int ok = s[0] == 's' && f[PG] == 'f';
    s[PG] = 'S';
    f[0] = 'F';
    _exit(ok ? 0 : 1);
  }
  CHECK(waitpid(k, &st, 0) == k && WIFEXITED(st) && WEXITSTATUS(st) == 0, "shared child");
  CHECK(s[PG] == 'S', "MAP_SHARED anonymous write from the child");
  CHECK(f[0] == 'F', "MAP_SHARED file write from the child");
  char b[2] = {0};
  CHECK(pread(fd, b, 1, 0) == 1 && b[0] == 'F', "file content after the child exited");
  close(fd);
  unlink(path);

  // 3. fork した子が exec する (fork した写しを捨てる)。そのあいだに親が自分のページを書く
  for (int i = 0; i < 20; i++) {
    k = fork();
    if (k == 0) {
      execl("/proc/self/exe", argv[0], "noop", (char *)NULL);
      _exit(127);
    }
    fill(p, (char)('d' + i));
    CHECK(waitpid(k, &st, 0) == k && WIFEXITED(st) && WEXITSTATUS(st) == 0, "exec child %d", i);
    CHECK(same(p, (char)('d' + i)), "parent pages after exec child %d", i);
  }

  // 4. いくつものプロセスが fork と書きこみをくりかえすあいだに、親がそれぞれの /proc/PID/maps、status、cmdline を読む
  enum { N = 3, ROUNDS = 300 };
  pid_t kids[N];
  for (int i = 0; i < N; i++) {
    kids[i] = fork();
    if (kids[i] == 0)
      _exit(churn(ROUNDS, i) ? 1 : 0);
  }
  char buf[65536];
  int left = N, reads = 0;
  for (int r = 0; left > 0; r++) {
    for (int i = 0; i < N; i++) {
      if (kids[i] == 0)
        continue;
      if (waitpid(kids[i], &st, WNOHANG) == kids[i]) {
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "churn child %d", i);
        kids[i] = 0;
        left--;
        continue;
      }
      const char *names[] = {"maps", "status", "cmdline"};
      char fn[64];
      snprintf(fn, sizeof fn, "/proc/%d/%s", kids[i], names[r % 3]);
      int pf = open(fn, O_RDONLY);
      if (pf >= 0) {
        while (read(pf, buf, sizeof buf) > 0)
          ;
        close(pf);
        reads++;
      }
    }
  }
  CHECK(reads > 0, "no /proc reads while the children ran");

  printf("forkcow: %d failed\n", fails);
  return fails != 0;
}
