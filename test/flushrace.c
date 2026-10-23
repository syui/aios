// 書き出し (fsync、sync) がディスクを眠って待つあいだも、ほかのプロセスのファイルの読み書きがこわれないか
// (カーネルの extfs.rs の flush_sleeping: 中身とメタデータの写しを眠る前にとり、書いているブロックを writeback に置く)。
// aios の中で動かす (aarch64-linux-gnu-gcc -O2 -static -o flushrace test/flushrace.c)。Linux でも同じに通る。
//   W この書き手が、それぞれ自分のディレクトリの F このファイルを、決まった中身 (種 seed と位置から作る) で作る・上書き
//   する・のばす・切りつめる・消す・名前を入れかえる・MAP_SHARED で書く (msync はしない: sync が書き出す) をくりかえす。
//   S この同期係が fsync、fdatasync、sync、syncfs をくりかえす。T 秒たったら、書き手がみな自分のファイルを読みなおして
//   中身と大きさとあるなしをくらべ、覚えている形を DIR/manifest.N に書いて fsync する。
//   flushrace [-w W] [-s S] [-t T] DIR    走らせて、くらべる (おしまいに sync)
//   flushrace -v DIR                      manifest のとおりか、くらべるだけ (再起動のあと: ディスクに残ったか)
// 1 行に ok NAME か FAIL NAME、おしまいに flushrace: N failed
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAXF 24
#define MAXLEN (512 * 1024)

struct file {
  int exists;
  uint32_t seed;
  long len;
};

static const char *dir;

static unsigned char pat(uint32_t seed, long off) {
  uint32_t x = seed * 2654435761u ^ (uint32_t)off * 40503u ^ (uint32_t)(off >> 12) * 977u;
  x ^= x >> 13;
  return (unsigned char)(x * 0x9e3779b1u >> 24);
}

static uint64_t rng = 88172645463325252ull;
static uint32_t rnd(void) {
  rng ^= rng << 13;
  rng ^= rng >> 7;
  rng ^= rng << 17;
  return (uint32_t)rng;
}

static void path(char *out, size_t n, int w, int k) {
  snprintf(out, n, "%s/w%d/f%d", dir, w, k);
}

static unsigned char buf[MAXLEN];

// [from, to) を seed の中身で書く
static int fill(int fd, uint32_t seed, long from, long to) {
  for (long o = from; o < to; o++) buf[o - from] = pat(seed, o);
  long done = 0;
  while (done < to - from) {
    ssize_t r = pwrite(fd, buf + done, to - from - done, from + done);
    if (r <= 0) return -1;
    done += r;
  }
  return 0;
}

// 1 つのファイルが覚えている形か。ちがえば理由を書いて 0
static int check(int w, int k, const struct file *f, char *why, size_t n) {
  char p[512];
  path(p, sizeof p, w, k);
  int fd = open(p, O_RDONLY);
  if (!f->exists) {
    if (fd >= 0) {
      close(fd);
      snprintf(why, n, "%s exists but was removed", p);
      return 0;
    }
    return 1;
  }
  if (fd < 0) {
    snprintf(why, n, "%s: open: %s", p, strerror(errno));
    return 0;
  }
  struct stat st;
  fstat(fd, &st);
  if (st.st_size != f->len) {
    snprintf(why, n, "%s: size %ld, want %ld", p, (long)st.st_size, f->len);
    close(fd);
    return 0;
  }
  long got = 0;
  while (got < f->len) {
    ssize_t r = pread(fd, buf, f->len - got < MAXLEN ? f->len - got : MAXLEN, got);
    if (r <= 0) break;
    for (ssize_t i = 0; i < r; i++) {
      if (buf[i] != pat(f->seed, got + i)) {
        snprintf(why, n, "%s: byte %ld is %02x, want %02x (seed %u, len %ld)", p, got + i, buf[i], pat(f->seed, got + i), f->seed, f->len);
        close(fd);
        return 0;
      }
    }
    got += r;
  }
  close(fd);
  if (got != f->len) {
    snprintf(why, n, "%s: read %ld of %ld", p, got, f->len);
    return 0;
  }
  return 1;
}

static int writer(int w, int secs) {
  struct file fs[MAXF] = {0};
  char p[512], q[512], why[1024];
  rng ^= (uint64_t)(w + 1) * 0x9e3779b97f4a7c15ull;
  snprintf(p, sizeof p, "%s/w%d", dir, w);
  mkdir(p, 0755);
  time_t end = time(NULL) + secs;
  long ops = 0;
  while (time(NULL) < end) {
    int k = rnd() % MAXF;
    struct file *f = &fs[k];
    path(p, sizeof p, w, k);
    int op = rnd() % 10;
    if (!f->exists || op == 0) {
      // 作る (または、まるごと上書き: O_TRUNC)
      int fd = open(p, O_CREAT | O_TRUNC | O_WRONLY, 0644);
      if (fd < 0) return fprintf(stderr, "flushrace: create %s: %s\n", p, strerror(errno)), 1;
      f->seed = rnd();
      f->len = rnd() % 8 == 0 ? rnd() % MAXLEN : rnd() % 16384;
      if (fill(fd, f->seed, 0, f->len) < 0) return fprintf(stderr, "flushrace: write %s: %s\n", p, strerror(errno)), 1;
      f->exists = 1;
      if (rnd() % 16 == 0) fsync(fd);
      close(fd);
    } else if (op <= 2) {
      // のばす
      long to = f->len + rnd() % 65536;
      if (to > MAXLEN) to = MAXLEN;
      int fd = open(p, O_WRONLY);
      if (fd < 0 || fill(fd, f->seed, f->len, to) < 0) return fprintf(stderr, "flushrace: append %s: %s\n", p, strerror(errno)), 1;
      f->len = to;
      close(fd);
    } else if (op == 3) {
      // 切りつめる
      long to = f->len ? rnd() % (f->len + 1) : 0;
      if (truncate(p, to) < 0) return fprintf(stderr, "flushrace: truncate %s: %s\n", p, strerror(errno)), 1;
      f->len = to;
    } else if (op == 4) {
      // 中身を新しい種で書きなおす (大きさはそのまま: すでにあるブロックの上書き)
      int fd = open(p, O_WRONLY);
      uint32_t s = rnd();
      if (fd < 0 || fill(fd, s, 0, f->len) < 0) return fprintf(stderr, "flushrace: rewrite %s: %s\n", p, strerror(errno)), 1;
      f->seed = s;
      close(fd);
    } else if (op == 5) {
      unlink(p);
      f->exists = 0;
    } else if (op == 6) {
      // 名前を入れかえる (k2 のほうへ。あったものは消える)
      int k2 = rnd() % MAXF;
      if (k2 != k) {
        path(q, sizeof q, w, k2);
        if (rename(p, q) < 0) return fprintf(stderr, "flushrace: rename %s: %s\n", p, strerror(errno)), 1;
        fs[k2] = *f;
        f->exists = 0;
      }
    } else if (op == 7 && f->len > 0) {
      // MAP_SHARED で書きなおす (書き出しは sync がする: その中でファイルへ書くところを通る)
      int fd = open(p, O_RDWR);
      if (fd < 0) return fprintf(stderr, "flushrace: open %s: %s\n", p, strerror(errno)), 1;
      unsigned char *m = mmap(NULL, f->len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
      if (m == MAP_FAILED) return fprintf(stderr, "flushrace: mmap %s: %s\n", p, strerror(errno)), 1;
      uint32_t s = rnd();
      for (long o = 0; o < f->len; o++) m[o] = pat(s, o);
      f->seed = s;
      munmap(m, f->len);
      close(fd);
    } else {
      // 読んでたしかめる (書き出しのとちゅうでも、書いたとおりに読めるか)
      if (!check(w, k, f, why, sizeof why)) {
        printf("FAIL during writer %d: %s\n", w, why);
        fflush(stdout);
        return 1;
      }
    }
    ops++;
  }
  int bad = 0;
  for (int k = 0; k < MAXF; k++) {
    if (!check(w, k, &fs[k], why, sizeof why)) {
      printf("FAIL after writer %d: %s\n", w, why);
      bad++;
    }
  }
  // 覚えている形を書いておく (再起動のあとに -v でくらべる)
  snprintf(p, sizeof p, "%s/manifest.%d", dir, w);
  FILE *m = fopen(p, "w");
  for (int k = 0; k < MAXF; k++) fprintf(m, "%d %d %u %ld\n", k, fs[k].exists, fs[k].seed, fs[k].len);
  fflush(m);
  fsync(fileno(m));
  fclose(m);
  printf("%s writer-%d (%ld ops)\n", bad ? "FAIL" : "ok", w, ops);
  fflush(stdout);
  return bad != 0;
}

static int syncer(int s, int secs) {
  char p[512];
  rng ^= (uint64_t)(s + 101) * 0x9e3779b97f4a7c15ull;
  time_t end = time(NULL) + secs;
  long n = 0;
  int dfd = open(dir, O_RDONLY | O_DIRECTORY);
  while (time(NULL) < end) {
    int op = rnd() % 4;
    if (op == 0) {
      sync();
    } else if (op == 1) {
      syncfs(dfd);
    } else {
      // だれかのファイルを fsync (なくてもよい)
      snprintf(p, sizeof p, "%s/w%u/f%u", dir, rnd() % 4, rnd() % MAXF);
      int fd = open(p, O_RDONLY);
      if (fd >= 0) {
        if ((op == 2 ? fsync(fd) : fdatasync(fd)) < 0) return fprintf(stderr, "flushrace: fsync %s: %s\n", p, strerror(errno)), 1;
        close(fd);
      }
    }
    n++;
  }
  close(dfd);
  printf("ok syncer-%d (%ld syncs)\n", s, n);
  fflush(stdout);
  return 0;
}

static int verify(void) {
  char p[512], why[1024];
  int bad = 0, files = 0;
  for (int w = 0; w < 64; w++) {
    snprintf(p, sizeof p, "%s/manifest.%d", dir, w);
    FILE *m = fopen(p, "r");
    if (!m) break;
    struct file f;
    int k;
    while (fscanf(m, "%d %d %u %ld", &k, &f.exists, &f.seed, &f.len) == 4) {
      files += f.exists;
      if (!check(w, k, &f, why, sizeof why)) {
        printf("FAIL verify %s\n", why);
        bad++;
      }
    }
    fclose(m);
  }
  printf("%s verify (%d files)\n", bad ? "FAIL" : "ok", files);
  return bad;
}

int main(int argc, char **argv) {
  int W = 4, S = 2, T = 20, v = 0, c;
  while ((c = getopt(argc, argv, "w:s:t:v")) != -1) {
    if (c == 'w') W = atoi(optarg);
    else if (c == 's') S = atoi(optarg);
    else if (c == 't') T = atoi(optarg);
    else if (c == 'v') v = 1;
    else return 2;
  }
  if (optind >= argc) return fprintf(stderr, "usage: flushrace [-w W] [-s S] [-t T] [-v] DIR\n"), 2;
  dir = argv[optind];
  int fails = 0;
  if (v) {
    fails = verify();
  } else {
    mkdir(dir, 0755);
    pid_t pids[64];
    int n = 0;
    for (int w = 0; w < W && n < 64; w++)
      if ((pids[n++] = fork()) == 0) _exit(writer(w, T));
    for (int s = 0; s < S && n < 64; s++)
      if ((pids[n++] = fork()) == 0) _exit(syncer(s, T));
    for (int i = 0; i < n; i++) {
      int st;
      waitpid(pids[i], &st, 0);
      if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) fails++;
    }
    sync();
  }
  printf("flushrace: %d failed\n", fails);
  return fails > 125 ? 125 : fails;
}
