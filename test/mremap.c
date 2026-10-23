// mremap(2) が Linux と同じか (カーネル: syscall.rs の sys_mremap と、大きなロックなしの答え vm.rs の fast_mremap_fail)。
// 動かさずに伸ばす (flags 0): 領域の終わりで、その先が空いていれば伸びる。先に領域があるか、領域のまん中なら ENOMEM、
// 古い場所に領域がなければ EFAULT。縮める、同じ大きさ、MREMAP_MAYMOVE で動かす、EINVAL も。おしまいに、いくつもの
// スレッドが伸ばせないかをたずねつづけるあいだに、ほかのスレッドがとなりを mmap / munmap する (答えはいつも
// ENOMEM か、伸びたなら縮めなおす)。Linux (このマシン) でも aios でも同じに通る
// cc -O2 -pthread -o mremap test/mremap.c
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static int n, fail;
static long P;

static void check(const char *what, void *r, void *want, int werr) {
  n++;
  int bad = werr ? !(r == MAP_FAILED && errno == werr) : r != want;
  if (bad) {
    fail++;
    printf("FAIL %s: got %p (%s), want %s\n", what, r, r == MAP_FAILED ? strerror(errno) : "ok", werr ? strerror(werr) : "the address");
  }
}

// 3 ページを取って、まん中を外す: [a][ ][c]。両はしには PROT_NONE の見張りのページ (Linux はとなりの同じ形の領域を
// 1 つにまとめるので、たまたま上にある領域とつながると、答えがまわりで変わる)
static char *three(void) {
  char *g = mmap(0, 5 * P, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (g == MAP_FAILED || mprotect(g + P, 3 * P, PROT_READ | PROT_WRITE)) {
    perror("mmap");
    exit(2);
  }
  munmap(g + 2 * P, P);
  return g + P;
}

static char *volatile shared;
static volatile int stop;
static long answers[3];

static void *asker(void *arg) {
  (void)arg;
  while (!stop) {
    char *r = mremap(shared, P, 2 * P, 0);
    if (r == shared) {
      // となりが空いていたので伸びた: 縮めなおす (となりを作るスレッドとは、作れるかどうかで競う)
      if (mremap(shared, 2 * P, P, 0) != shared) __atomic_add_fetch(&answers[2], 1, __ATOMIC_RELAXED);
      __atomic_add_fetch(&answers[1], 1, __ATOMIC_RELAXED);
    } else if (r == MAP_FAILED && errno == ENOMEM) {
      __atomic_add_fetch(&answers[0], 1, __ATOMIC_RELAXED);
    } else {
      __atomic_add_fetch(&answers[2], 1, __ATOMIC_RELAXED);
    }
  }
  return 0;
}

int main(void) {
  P = sysconf(_SC_PAGESIZE);
  char *a = three();
  char *c = a + 2 * P;
  // [a] の先は 1 ページ空き: 2 ページまでは伸びる、3 ページはぶつかる
  check("grow into the gap", mremap(a, P, 2 * P, 0), a, 0);
  a[P] = 1;
  check("grow over the next area", mremap(a, 2 * P, 3 * P, 0), 0, ENOMEM);
  check("shrink back", mremap(a, 2 * P, P, 0), a, 0);
  check("same size", mremap(a, P, P, 0), a, 0);
  // 1 ページの領域を 2 ページに、先にもう領域がある
  check("grow an unmapped page", mremap(c - P, P, 2 * P, 0), 0, EFAULT);
  char *b = mmap(a + P, P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
  check("refill the gap", b, a + P, 0);
  // [a][b][c] は別々の領域 (Linux はとなりの同じ形の領域を 1 つにまとめるので、a の終わりで伸ばすと c にぶつかる)
  check("grow a over b", mremap(a, P, 2 * P, 0), 0, ENOMEM);
  // 領域のまん中 (Linux も aios も、終わりでなければ動かさずには伸ばせない)
  char *m = mmap(0, 4 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  check("grow inside an area", mremap(m + P, P, 2 * P, 0), 0, ENOMEM);
  // 古い場所に何もない
  munmap(m, 4 * P);
  check("grow nothing", mremap(m, P, 2 * P, 0), 0, EFAULT);
  check("grow past the area", mremap(a, 4 * P, 5 * P, 0), 0, EFAULT);
  // そろっていない、0、知らないフラグ
  check("unaligned", mremap(a + 1, P, 2 * P, 0), 0, EINVAL);
  check("zero new size", mremap(a, P, 0, 0), 0, EINVAL);
  check("unknown flag", mremap(a, P, 2 * P, 8), 0, EINVAL);
  check("zero old size, private", mremap(a, 0, P, MREMAP_MAYMOVE), 0, EINVAL);
  // 同じ大きさと縮めるのは、古い場所が領域をこえていても答える (縮めるなら、こえたところも外す)。伸ばすのは EFAULT
  check("same size past the area", mremap(a, 4 * P, 4 * P, 0), a, 0);
  check("grow past the area, may move", mremap(a, 4 * P, 5 * P, MREMAP_MAYMOVE), 0, EFAULT);
  // 動かしてよければ、ぶつかっても別の場所へ (中身ごと)
  a[0] = 42;
  char *moved = mremap(a, P, 8 * P, MREMAP_MAYMOVE);
  n++;
  if (moved == MAP_FAILED || moved[0] != 42) {
    fail++;
    printf("FAIL maymove: %p\n", moved);
  }
  munmap(moved, 8 * P);
  // [b][c] が残っている: こえて縮めると、こえたところ (見張り) まで外れる
  check("shrink past the area", mremap(a + P, 3 * P, P, 0), a + P, 0);
  n++;
  if (mremap(a + 2 * P, P, P, 0) != MAP_FAILED || errno != EFAULT) {
    fail++;
    printf("FAIL shrink past the area did not unmap the rest\n");
  }
  munmap(a + P, P);

  // スレッドで: たずねるもの 3 つ、となりを作って外すもの 1 つ
  char *s = three();
  shared = s;
  pthread_t t[3];
  for (int i = 0; i < 3; i++) pthread_create(&t[i], 0, asker, 0);
  for (int i = 0; i < 2000; i++) {
    void *r = mmap(s + P, P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (r != MAP_FAILED) munmap(r, P);
  }
  stop = 1;
  for (int i = 0; i < 3; i++) pthread_join(t[i], 0);
  n++;
  if (answers[2] != 0 || answers[0] == 0) {
    fail++;
    printf("FAIL threads: enomem %ld grew %ld other %ld\n", answers[0], answers[1], answers[2]);
  }
  printf("mremap: %d checks, %d failed\n", n, fail);
  return fail != 0;
}
