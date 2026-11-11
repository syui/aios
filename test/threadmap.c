// スレッドで分けた表の、大きなロックなしの mmap / munmap (vm::PageTable::fast_map_threads、fast_unmap の threads:
// write_enter でひとりで変える) のたしかめ。Linux と同じ答えになるか:
//   churn   4 つのスレッドが、120 KiB (musl の malloc が mmap にする大きさ) の無名の領域を作り、ページごとに自分の印を
//           書いて読みなおし、外すのを 2000 回ずつ。どの読みも自分の印 (ほかのスレッドのページや、返したページを見ない)
//   kernel  1 つのスレッドが領域を作っては外し、新しい領域に印を書いて見なおす。ほかの 2 つのスレッドは、外す前の領域に
//           カーネルを書かせ (pipe からの read)、読ませる (pipe への write)。カーネルが返したページに書いて、あとで同じ
//           ページを使う新しい領域の印をこわさない。読み書きは答えがあるか EFAULT
//   faults  2 つのスレッドが自分の領域 (64 ページ) に書いては読みなおすあいだに、ほかの 2 つのスレッドが作って外す
//   maps    作って外すあいだに、ほかのスレッドが /proc/self/maps を読みつづける (大きなロックを持って同じ表を読む)
// aios なら、churn の munmap と mmap のほとんどが大きなロックを取らないこと (/proc/bkl の munmap と mmap の数)
// 1 行に ok NAME か FAIL NAME、おしまいに threadmap: N failed
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define PG 4096
#define CHURN_LEN (30 * PG)
#define CHURN_ROUNDS 2000

static int fails;
static volatile int stop;
static int bad;
static char why[256];

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static void note(const char *fmt, long a, long b) {
  if (__atomic_fetch_add(&bad, 1, __ATOMIC_RELAXED) == 0)
    snprintf(why, sizeof why, fmt, a, b);
}

static void *map(size_t len) {
  void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  return p == MAP_FAILED ? NULL : p;
}

// churn: 作って、ページごとに印を書いて、読みなおして、外す
static void *churn(void *a) {
  long me = (long)a;
  for (long i = 0; i < CHURN_ROUNDS && !stop; i++) {
    uint64_t *p = map(CHURN_LEN);
    if (!p) {
      note("churn %ld: mmap errno %ld", me, errno);
      break;
    }
    for (int k = 0; k < CHURN_LEN / PG; k++)
      p[k * PG / 8] = (uint64_t)me << 48 | (uint64_t)i << 16 | k;
    for (int k = 0; k < CHURN_LEN / PG; k++)
      if (p[k * PG / 8] != ((uint64_t)me << 48 | (uint64_t)i << 16 | k))
        note("churn %ld: page %ld has someone else's mark", me, k);
    if (munmap(p, CHURN_LEN))
      note("churn %ld: munmap errno %ld", me, errno);
  }
  return NULL;
}

// /proc/bkl (aios) の、大きなロックを取ったシステムコール name の数 (「name   回数   ms ...」の行。なければ 0)。
// /proc/bkl がなければ (Linux) -1
static long bkl_count(const char *name) {
  FILE *f = fopen("/proc/bkl", "r");
  if (!f)
    return -1;
  char line[512];
  long n = 0;
  size_t len = strlen(name);
  while (fgets(line, sizeof line, f))
    if (!strncmp(line, name, len) && line[len] == ' ') {
      n = atol(line + len);
      break;
    }
  fclose(f);
  return n;
}

static void run_churn(void) {
  long u0 = bkl_count("munmap"), m0 = bkl_count("mmap");
  bad = 0;
  pthread_t t[4];
  for (long i = 0; i < 4; i++)
    pthread_create(&t[i], NULL, churn, (void *)i);
  for (int i = 0; i < 4; i++)
    pthread_join(t[i], NULL);
  char name[400];
  snprintf(name, sizeof name, "churn (4 threads x %d)%s%s", CHURN_ROUNDS, bad ? ": " : "", bad ? why : "");
  check(name, bad == 0);
  if (u0 >= 0) {
    long du = bkl_count("munmap") - u0, dm = bkl_count("mmap") - m0;
    snprintf(name, sizeof name, "churn lockless (munmap %ld, mmap %ld of %d took the big lock)", du, dm, 4 * CHURN_ROUNDS);
    check(name, du * 4 < 4 * CHURN_ROUNDS && dm * 4 < 4 * CHURN_ROUNDS);
  }
}

// kernel: 外す前の領域に、カーネルを書かせ・読ませる
#define KLEN (16 * PG)
static char *volatile cur;
static int to_k[2], from_k[2];

static void *kwriter(void *a) {
  (void)a;
  static char junk[PG];
  memset(junk, 'X', sizeof junk);
  while (!stop) {
    // pipe をいっぱいにしておく (read が書くもの)
    if (write(to_k[1], junk, sizeof junk) < 0 && errno != EAGAIN)
      note("kernel: write junk errno %ld", errno, 0);
    char *p = cur;
    if (!p)
      continue;
    // カーネルに p へ書かせる: 答えがあるか EFAULT
    ssize_t n = read(to_k[0], p + PG, PG);
    if (n < 0 && errno != EFAULT && errno != EAGAIN)
      note("kernel: read into the area: errno %ld", errno, 0);
  }
  return NULL;
}

static void *kreader(void *a) {
  (void)a;
  static char sink[PG];
  while (!stop) {
    char *p = cur;
    if (!p)
      continue;
    // カーネルに p から読ませる
    ssize_t n = write(from_k[1], p, PG);
    if (n < 0 && errno != EFAULT && errno != EAGAIN)
      note("kernel: write from the area: errno %ld", errno, 0);
    while (read(from_k[0], sink, sizeof sink) > 0)
      ;
  }
  return NULL;
}

static void run_kernel(void) {
  bad = 0;
  stop = 0;
  if (pipe2(to_k, O_NONBLOCK) || pipe2(from_k, O_NONBLOCK)) {
    check("kernel: pipe", 0);
    return;
  }
  pthread_t w, r;
  pthread_create(&w, NULL, kwriter, NULL);
  pthread_create(&r, NULL, kreader, NULL);
  int corrupt = 0;
  for (int i = 0; i < 1500; i++) {
    char *p = map(KLEN);
    if (!p) {
      note("kernel: mmap errno %ld", errno, 0);
      break;
    }
    memset(p, 'a', KLEN);
    cur = p;
    for (int k = 0; k < 50; k++)
      sched_yield();
    // 新しい領域を先に作る (同じアドレスにならないように)。ページはまだない
    char *q = map(KLEN);
    cur = NULL;
    if (munmap(p, KLEN))
      note("kernel: munmap errno %ld", errno, 0);
    if (!q)
      continue;
    // 返したページを使うかもしれない新しい領域に印を書き、しばらくしてから見なおす
    memset(q, 'P', KLEN);
    for (int k = 0; k < 20; k++)
      sched_yield();
    for (int k = 0; k < KLEN; k++)
      if (q[k] != 'P') {
        if (!corrupt++)
          note("kernel: the new area changed at %ld: %ld", k, q[k]);
        break;
      }
    munmap(q, KLEN);
  }
  stop = 1;
  pthread_join(w, NULL);
  pthread_join(r, NULL);
  close(to_k[0]), close(to_k[1]), close(from_k[0]), close(from_k[1]);
  char name[400];
  snprintf(name, sizeof name, "kernel (read / write into areas being unmapped)%s%s", bad ? ": " : "", bad ? why : "");
  check(name, bad == 0);
}

// faults: 自分の領域に書いては読みなおす (ほかのスレッドが作って外すあいだ)
static void *owner(void *a) {
  long me = (long)a;
  uint64_t *p = map(64 * PG);
  if (!p) {
    note("faults %ld: mmap errno %ld", me, errno);
    return NULL;
  }
  for (uint64_t r = 1; !stop; r++) {
    for (int k = 0; k < 64; k++)
      p[k * PG / 8 + (r & 7)] = me << 48 | r << 8 | k;
    for (int k = 0; k < 64; k++)
      if (p[k * PG / 8 + (r & 7)] != (me << 48 | r << 8 | k))
        note("faults %ld: page %ld changed", me, k);
  }
  munmap(p, 64 * PG);
  return NULL;
}

static void *churner(void *a) {
  (void)a;
  while (!stop) {
    char *p = map(8 * PG);
    if (!p) {
      note("faults: mmap errno %ld", errno, 0);
      break;
    }
    for (int k = 0; k < 8; k++)
      p[k * PG] = 1;
    munmap(p, 8 * PG);
  }
  return NULL;
}

static void *maps_reader(void *a) {
  (void)a;
  static char buf[1 << 16];
  while (!stop) {
    int fd = open("/proc/self/maps", O_RDONLY);
    if (fd < 0) {
      note("maps: open errno %ld", errno, 0);
      break;
    }
    ssize_t n;
    while ((n = read(fd, buf, sizeof buf)) > 0)
      ;
    if (n < 0)
      note("maps: read errno %ld", errno, 0);
    close(fd);
  }
  return NULL;
}

static void timed(const char *what, void *(*a)(void *), int na, void *(*b)(void *), int nb, int ms) {
  bad = 0;
  stop = 0;
  pthread_t t[8];
  int n = 0;
  for (long i = 0; i < na; i++)
    pthread_create(&t[n++], NULL, a, (void *)i);
  for (long i = 0; i < nb; i++)
    pthread_create(&t[n++], NULL, b, (void *)i);
  usleep(ms * 1000);
  stop = 1;
  for (int i = 0; i < n; i++)
    pthread_join(t[i], NULL);
  char name[400];
  snprintf(name, sizeof name, "%s%s%s", what, bad ? ": " : "", bad ? why : "");
  check(name, bad == 0);
}

int main(void) {
  run_churn();
  run_kernel();
  timed("faults (own areas while others map and unmap)", owner, 2, churner, 2, 2000);
  timed("maps (/proc/self/maps while threads map and unmap)", churner, 3, maps_reader, 1, 2000);
  printf("threadmap: %d failed\n", fails);
  return fails != 0;
}
