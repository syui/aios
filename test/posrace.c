// 開いたファイルの位置 (offset) を、いくつものスレッドとプロセスが同時に使っても、read が同じところを 2 回読んだり
// 飛ばしたりしないこと (Linux の f_pos_lock: 位置を使うシステムコールは、同じ開いたファイルでは 1 つずつ)。aios は
// fd の表をほかのスレッドと分けていても、fork で開いたファイルを分けていても、大きなロックなしの read
// (file::fast_read_file) が位置の札 (file::pos_fast) を取って読む。ふつうの道 (readv、lseek) は札を待つ
//   threads  4 つのスレッドが同じ fd を read (2 つ)、readv (1 つ)、read と lseek(SEEK_CUR) (1 つ) で 4 KiB ずつ読む
//   procs    fork で同じ開いたファイルを持つ 4 つのプロセスが read で読む
//   splice   2 つのスレッドが read、1 つが splice (ファイル → パイプ) で読む
//   own      4 つのスレッドが自分で開いた fd で読む (rg のように: 表は分けていて、開いたファイルは分けていない)。
//            どのスレッドもブロックを順に、みな読めたか
// どれも、ファイルのブロック (中身はブロックの番号) がちょうど 1 回ずつ、まるごと読めたかを見る。何回かくりかえす。
// ただし Linux 7.4 の splice は位置の札を取らない (fdget_pos でなく fdget: read と同時なら同じところを 2 回読む
// ことがある) ので、splice は読んだものがまるごとか (こわれていないか) だけを見る。aios は splice でも札を取るので、
// aios (/proc/bkl がある) ならちょうど 1 回ずつかも見る
// aios の ext4 なら、own で /proc/bkl のロックなしの read の数が増えたか (表を分けていても速い道を通ったか) も見る
// 1 行に ok NAME か FAIL NAME、おしまいに posrace: N failed
//   posrace [DIR...]   DIR (既定 /tmp) の下に .posrace.PID を作って
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

#define BS 4096
#define NB 2048 // 8 MiB
#define ROUNDS 6

static int fails;
static char path[512];
static int fd;
static uint32_t *seen; // ブロックごとの読めた回数 (MAP_SHARED: 子のプロセスも書く)
static int *bad;       // まるごとでない読み (MAP_SHARED)

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

// 読んだ 4 KiB を数える: 中身がみな同じブロックの番号か
static void count(const uint32_t *b, ssize_t n) {
  if (n != BS) {
    __atomic_fetch_add(bad, 1, __ATOMIC_RELAXED);
    return;
  }
  uint32_t k = b[0];
  for (int i = 1; i < BS / 4; i++)
    if (b[i] != k) {
      __atomic_fetch_add(bad, 1, __ATOMIC_RELAXED);
      return;
    }
  if (k >= NB) {
    __atomic_fetch_add(bad, 1, __ATOMIC_RELAXED);
    return;
  }
  __atomic_fetch_add(&seen[k], 1, __ATOMIC_RELAXED);
}

// how: 0 read、1 readv、2 read と lseek(SEEK_CUR)、3 splice (ファイル → パイプ → 読む)
static void reader(int how) {
  static __thread uint32_t buf[BS / 4];
  int p[2] = {-1, -1};
  if (how == 3 && pipe(p))
    return;
  for (;;) {
    ssize_t n;
    if (how == 1) {
      struct iovec v = {buf, BS};
      n = readv(fd, &v, 1);
    } else if (how == 3) {
      n = splice(fd, NULL, p[1], NULL, BS, 0);
      if (n > 0) {
        ssize_t got = 0;
        while (got < n) {
          ssize_t r = read(p[0], (char *)buf + got, n - got);
          if (r <= 0)
            break;
          got += r;
        }
      }
    } else {
      n = read(fd, buf, BS);
      if (how == 2 && lseek(fd, 0, SEEK_CUR) < 0)
        __atomic_fetch_add(bad, 1, __ATOMIC_RELAXED);
    }
    if (n <= 0)
      break;
    count(buf, n);
  }
  if (p[0] >= 0) {
    close(p[0]);
    close(p[1]);
  }
}

static void *thread(void *a) {
  reader((int)(intptr_t)a);
  return NULL;
}

// own: 自分で開いて、はじめから順に読む。順でないもの、欠けたものは bad に
static void *own(void *a) {
  (void)a;
  static __thread uint32_t buf[BS / 4];
  int f = open(path, O_RDONLY);
  if (f < 0) {
    __atomic_fetch_add(bad, 1, __ATOMIC_RELAXED);
    return NULL;
  }
  uint32_t next = 0;
  ssize_t n;
  while ((n = read(f, buf, BS)) > 0) {
    if (n != BS || buf[0] != next || buf[BS / 4 - 1] != next)
      __atomic_fetch_add(bad, 1, __ATOMIC_RELAXED);
    else
      __atomic_fetch_add(&seen[next], 1, __ATOMIC_RELAXED);
    next++;
  }
  if (next != NB)
    __atomic_fetch_add(bad, 1, __ATOMIC_RELAXED);
  close(f);
  return NULL;
}

static int verify(void) {
  int ok = *bad == 0;
  for (int i = 0; i < NB; i++)
    if (seen[i] != 1)
      ok = 0;
  return ok;
}

static void reset(void) {
  memset(seen, 0, NB * sizeof *seen);
  *bad = 0;
}

static long fast_reads(void) {
  FILE *f = fopen("/proc/bkl", "r");
  char line[512];
  long n = -1;
  while (f && fgets(line, sizeof line, f))
    if (!strncmp(line, "ファイルのロックなしの read: ", strlen("ファイルのロックなしの read: ")))
      n = atol(line + strlen("ファイルのロックなしの read: "));
  if (f)
    fclose(f);
  return n;
}

static void run(const char *dir) {
  snprintf(path, sizeof path, "%s/.posrace.%d", dir, getpid());
  int w = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (w < 0) {
    perror(path);
    fails++;
    return;
  }
  static uint32_t blk[BS / 4];
  for (int i = 0; i < NB; i++) {
    for (int j = 0; j < BS / 4; j++)
      blk[j] = i;
    if (write(w, blk, BS) != BS) {
      perror("write");
      fails++;
      close(w);
      return;
    }
  }
  close(w);
  long fast0 = fast_reads();
  // threads: 表を分けたスレッドが同じ fd を
  int ok = 1;
  for (int r = 0; r < ROUNDS; r++) {
    reset();
    fd = open(path, O_RDONLY);
    pthread_t t[4];
    int how[4] = {0, 0, 1, 2};
    for (int i = 0; i < 4; i++)
      pthread_create(&t[i], NULL, thread, (void *)(intptr_t)how[i]);
    for (int i = 0; i < 4; i++)
      pthread_join(t[i], NULL);
    close(fd);
    if (!verify())
      ok = 0;
  }
  char name[600];
  snprintf(name, sizeof name, "threads %s", dir);
  check(name, ok);
  // procs: fork で同じ開いたファイルを
  ok = 1;
  for (int r = 0; r < ROUNDS; r++) {
    reset();
    fd = open(path, O_RDONLY);
    pid_t kids[3];
    for (int i = 0; i < 3; i++)
      if ((kids[i] = fork()) == 0) {
        reader(0);
        _exit(0);
      }
    reader(0);
    for (int i = 0; i < 3; i++)
      waitpid(kids[i], NULL, 0);
    close(fd);
    if (!verify())
      ok = 0;
  }
  snprintf(name, sizeof name, "procs %s", dir);
  check(name, ok);
  // splice: read と splice (大きなロックなしの splice も位置を使う)。Linux は札を取らないので、まるごとかだけ
  ok = 1;
  int strict = fast0 >= 0;
  for (int r = 0; r < ROUNDS; r++) {
    reset();
    fd = open(path, O_RDONLY);
    pthread_t t[3];
    int how[3] = {0, 0, 3};
    for (int i = 0; i < 3; i++)
      pthread_create(&t[i], NULL, thread, (void *)(intptr_t)how[i]);
    for (int i = 0; i < 3; i++)
      pthread_join(t[i], NULL);
    close(fd);
    if (strict ? !verify() : *bad != 0)
      ok = 0;
  }
  snprintf(name, sizeof name, "splice %s", dir);
  check(name, ok);
  // own: スレッドごとに自分で開いて
  ok = 1;
  long fast1 = fast_reads();
  for (int r = 0; r < ROUNDS; r++) {
    reset();
    pthread_t t[4];
    for (int i = 0; i < 4; i++)
      pthread_create(&t[i], NULL, own, NULL);
    for (int i = 0; i < 4; i++)
      pthread_join(t[i], NULL);
    if (*bad)
      ok = 0;
    for (int i = 0; i < NB; i++)
      if (seen[i] != 4)
        ok = 0;
  }
  snprintf(name, sizeof name, "own %s", dir);
  check(name, ok);
  // aios の ext4: own で、表を分けていても速い道を通ったか (/proc/bkl がなければ Linux なので見ない。tmpfs はページ
  // キャッシュを通らないので、いつもふつうの道)。読んだ 4 * NB * ROUNDS のうち 9 割
  long fast2 = fast_reads();
  struct statfs sf;
  if (fast0 >= 0 && statfs(dir, &sf) == 0 && sf.f_type == 0xEF53) {
    snprintf(name, sizeof name, "fast %s (%ld of %d reads lockless)", dir, fast2 - fast1, 4 * NB * ROUNDS);
    check(name, fast2 - fast1 > 4 * NB * ROUNDS / 10 * 9);
  }
  unlink(path);
}

int main(int argc, char **argv) {
  seen = mmap(NULL, NB * sizeof *seen + 64, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (seen == MAP_FAILED)
    return 1;
  bad = (int *)(seen + NB);
  for (int i = 1; i < argc; i++)
    run(argv[i]);
  if (argc < 2)
    run("/tmp");
  printf("posrace: %d failed\n", fails);
  return fails != 0;
}
