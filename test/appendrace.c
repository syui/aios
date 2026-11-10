// 大きなロックなしの書きたし (ExtFs::fast_append: ファイルの終わりにつづけて書く write) がこわれないこと。
// 4 人の子が、同じ O_APPEND のファイルと自分のファイル (O_APPEND なしで、つづけて。ときどき ftruncate で 0 にもどす) に、
// 大きさのちがう記録 (1 回の write) を秒数だけ書きたす。そのあいだ、読む子 (read と、MAP_SHARED の mmap) が途中を読んで、
// 読めたところに記録でないもの (0 や、ほかの記録のかけら) がないかを見る (終わりの記録は書きかけでもよい: その頭だけ見る)。
// 終わったら全部を読んで、どの子の記録も 0 から順に欠けずにあるか、大きさが合うかを見る。おしまいに IN_MODIFY の見張りを
// 足してから書きたすと、できごとが来ること (見張りがあると、ロックなしの道を通らない)。
// 出すだけのテスト: ./appendrace [秒] DIR...
//   aarch64-linux-gnu-gcc -O2 -static -o appendrace test/appendrace.c
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define P 4
#define MAGIC 0x43455241u // "AREC"
#define MAXREC (24 * 1024)
#define BUDGET (4 << 20) // 1 人が同じファイルに書く大きさ (読む子が毎回全部を読むので。そのあとは自分のファイルだけ)

struct hdr {
  uint32_t magic;
  uint16_t id;
  uint16_t pad;
  uint32_t seq;
  uint32_t len;
};

static char shared[512], own[P][512];
static int fails;

static double now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec + t.tv_nsec / 1e9;
}

static unsigned char pat(unsigned id, unsigned seq, unsigned i) { return (id * 131 + seq * 17 + i * 3 + 1) & 0xff; }

static size_t make(char *buf, unsigned id, unsigned seq, unsigned *rnd) {
  unsigned r = rand_r(rnd), len;
  switch (r % 8) {
  case 0: len = 4096 - sizeof(struct hdr); break;   // ちょうど 1 ページ
  case 1: len = 1 + r / 8 % (MAXREC - sizeof(struct hdr)); break;
  case 2: len = 4000 + r / 8 % 300; break;           // ページの境をまたぐ
  default: len = r / 8 % 300; break;               // 小さいもの (ブロックの中に何回も)
  }
  struct hdr h = {MAGIC, id, 0, seq, len};
  memcpy(buf, &h, sizeof h);
  for (unsigned i = 0; i < len; i++)
    buf[sizeof h + i] = pat(id, seq, i);
  return sizeof h + len;
}

// buf[0..n] を記録として読む。終わりの書きかけの記録は頭だけ見る (tail が 0 ならそれもだめ)。
// しくじったら、その場所を返す。うまくいけば -1。seen[id] に次に来るはずの seq (順と欠けを見るとき)
static long parse(const unsigned char *buf, size_t n, int tail, unsigned *seen, long *count) {
  size_t off = 0;
  while (off < n) {
    struct hdr h;
    size_t have = n - off;
    if (have < sizeof h) {
      unsigned char want[sizeof h];
      memcpy(want, buf + off, have);
      // 頭のかけら: magic のところまでは合うはず
      uint32_t m = MAGIC;
      if (!tail || memcmp(want, &m, have < 4 ? have : 4))
        return off;
      return -1;
    }
    memcpy(&h, buf + off, sizeof h);
    if (h.magic != MAGIC || h.id >= P || h.len > MAXREC)
      return off;
    if (seen && h.seq != seen[h.id])
      return off;
    size_t len = h.len, body = have - sizeof h < len ? have - sizeof h : len;
    if (body < len && !tail)
      return off;
    for (size_t i = 0; i < body; i++)
      if (buf[off + sizeof h + i] != pat(h.id, h.seq, i))
        return off + sizeof h + i;
    if (body < len)
      return -1;
    if (seen)
      seen[h.id]++;
    if (count)
      (*count)++;
    off += sizeof h + len;
  }
  return -1;
}

static void bad(const char *what, const char *path, long off) {
  printf("FAIL appendrace: %s %s at %ld\n", what, path, off);
  fflush(stdout);
}

// 書く子: 同じファイル (O_APPEND) と自分のファイルに交互に。自分のファイルはときどき 0 にもどす
static void writer(int id, double end, int out) {
  int a = open(shared, O_WRONLY | O_APPEND);
  int b = open(own[id], O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (a < 0 || b < 0) {
    perror("open");
    _exit(1);
  }
  static char buf[MAXREC];
  unsigned rnd = id * 7919 + 1, seq = 0, oseq = 0, obase = 0;
  long off = 0, sent = 0;
  while (now() < end) {
    size_t n;
    if (sent < BUDGET) {
      n = make(buf, id, seq, &rnd);
      if (write(a, buf, n) != (ssize_t)n) {
        perror("write shared");
        _exit(1);
      }
      seq++;
      sent += n;
    }
    if (rand_r(&rnd) % 64 == 0) {
      if (ftruncate(b, 0) || lseek(b, 0, SEEK_SET)) {
        perror("ftruncate");
        _exit(1);
      }
      off = 0;
      obase = oseq;
    }
    n = make(buf, id, oseq, &rnd);
    if (write(b, buf, n) != (ssize_t)n) {
      perror("write own");
      _exit(1);
    }
    oseq++;
    off += n;
  }
  unsigned r[5] = {(unsigned)id, seq, oseq, obase, (unsigned)off};
  if (write(out, r, sizeof r) != sizeof r)
    _exit(1);
  close(a);
  close(b);
  _exit(0);
}

static unsigned char *slurp(const char *path, size_t *n) {
  int fd = open(path, O_RDONLY);
  if (fd < 0)
    return NULL;
  struct stat st;
  fstat(fd, &st);
  unsigned char *buf = malloc(st.st_size + 1);
  size_t got = 0;
  while (got < (size_t)st.st_size) {
    ssize_t r = read(fd, buf + got, st.st_size - got);
    if (r <= 0)
      break;
    got += r;
  }
  close(fd);
  *n = got;
  return buf;
}

// 読む子: read で全部、または MAP_SHARED で fstat の大きさまで
static void reader(int use_mmap, double end) {
  long rounds = 0;
  while (now() < end) {
    size_t n = 0;
    unsigned char *buf;
    void *map = NULL;
    if (use_mmap) {
      int fd = open(shared, O_RDONLY);
      struct stat st;
      if (fd < 0 || fstat(fd, &st)) {
        bad("open/fstat", shared, 0);
        _exit(1);
      }
      n = st.st_size;
      if (n == 0) {
        close(fd);
        continue;
      }
      map = mmap(NULL, n, PROT_READ, MAP_SHARED, fd, 0);
      close(fd);
      if (map == MAP_FAILED) {
        bad("mmap", shared, n);
        _exit(1);
      }
      buf = map;
    } else if (!(buf = slurp(shared, &n))) {
      bad("open", shared, 0);
      _exit(1);
    }
    long o = parse(buf, n, 1, NULL, NULL);
    if (o >= 0) {
      bad(use_mmap ? "mmap sees garbage in" : "read sees garbage in", shared, o);
      _exit(1);
    }
    if (map)
      munmap(map, n);
    else
      free(buf);
    rounds++;
  }
  _exit(rounds ? 0 : 1);
}

static void run(const char *dir, double secs) {
  snprintf(shared, sizeof shared, "%s/.appendrace.%d", dir, getpid());
  for (int i = 0; i < P; i++)
    snprintf(own[i], sizeof own[i], "%s/.appendrace.%d.%d", dir, getpid(), i);
  int fd = open(shared, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (fd < 0) {
    perror(shared);
    fails++;
    return;
  }
  close(fd);
  int pfd[2];
  if (pipe(pfd)) {
    perror("pipe");
    fails++;
    return;
  }
  double end = now() + secs;
  for (int i = 0; i < P; i++)
    if (fork() == 0)
      writer(i, end, pfd[1]);
  for (int m = 0; m < 2; m++)
    if (fork() == 0)
      reader(m, end);
  int st, ok = 1;
  for (int i = 0; i < P + 2; i++) {
    wait(&st);
    if (!WIFEXITED(st) || WEXITSTATUS(st))
      ok = 0;
  }
  // 書く子の知らせ: id、同じファイルに書いた数、自分のファイルに書いた数、最後に 0 にもどしたときの数、そのあとの大きさ
  unsigned r[P][4] = {{0}};
  for (int i = 0; i < P; i++) {
    unsigned m[5];
    if (read(pfd[0], m, sizeof m) != sizeof m || m[0] >= P)
      ok = 0;
    else
      memcpy(r[m[0]], m + 1, sizeof r[0]);
  }
  close(pfd[0]);
  close(pfd[1]);
  if (!ok) {
    printf("FAIL appendrace: a child failed in %s\n", dir);
    fails++;
  }
  // 同じファイル: どの子の記録も 0 から順に、欠けずに、大きさが合う
  size_t n;
  unsigned char *buf = slurp(shared, &n);
  unsigned seen[P] = {0};
  long count = 0, total = 0;
  long o = buf ? parse(buf, n, 0, seen, &count) : 0;
  for (int i = 0; i < P; i++)
    total += r[i][0];
  if (o >= 0) {
    bad("broken record in", shared, o);
    fails++;
  } else if (count != total) {
    printf("FAIL appendrace: %s has %ld records, wrote %ld\n", shared, count, total);
    fails++;
  }
  free(buf);
  // 自分のファイル: 最後に 0 にもどしてからの記録が順に、大きさも合う
  for (int i = 0; i < P; i++) {
    unsigned char *b = slurp(own[i], &n);
    if (!b) {
      bad("missing", own[i], 0);
      fails++;
      continue;
    }
    unsigned s[P] = {0};
    s[i] = r[i][2];
    long c = 0;
    long e = parse(b, n, 0, s, &c);
    if (e >= 0 || n != r[i][3] || (unsigned)c != r[i][1] - r[i][2]) {
      printf("FAIL appendrace: %s (bad at %ld, %ld records, size %zu)\n", own[i], e, c, n);
      fails++;
    }
    free(b);
    unlink(own[i]);
  }
  // IN_MODIFY の見張りを足したあとの書きたしは、できごとを出す
  int in = inotify_init1(IN_NONBLOCK);
  int w = in >= 0 ? inotify_add_watch(in, shared, IN_MODIFY) : -1;
  fd = open(shared, O_WRONLY | O_APPEND);
  static char rec[MAXREC];
  unsigned rnd = 1;
  make(rec, 0, 0, &rnd);
  int ev = 0;
  if (w >= 0 && fd >= 0 && write(fd, rec, sizeof(struct hdr)) == sizeof(struct hdr)) {
    char eb[256];
    ssize_t k = read(in, eb, sizeof eb);
    if (k >= (ssize_t)sizeof(struct inotify_event) && (((struct inotify_event *)eb)->mask & IN_MODIFY))
      ev = 1;
  }
  if (!ev) {
    printf("FAIL appendrace: no IN_MODIFY for an append to %s\n", shared);
    fails++;
  }
  if (fd >= 0)
    close(fd);
  if (in >= 0)
    close(in);
  unlink(shared);
  printf("appendrace %s: %ld records in the shared file, %d writers, %.0f s\n", dir, count, P, secs);
}

int main(int argc, char **argv) {
  double secs = argc > 1 ? atof(argv[1]) : 3;
  for (int i = 2; i < argc; i++)
    run(argv[i], secs);
  if (argc < 3)
    run(".", secs);
  if (fails)
    return 1;
  printf("ok\n");
  return 0;
}
