// カーネルの memcpy / memmove / memset (kernel/src/mem.S) を外でためす。名前を変えて (aios_*) いっしょに作り、
// qemu-aarch64 (ユーザーモード) で動かして、libc のものとくらべる (名前を変えるのは mem.S だけ。sh test/mem.sh):
//   aarch64-linux-gnu-gcc -c -o build/mem.o -x assembler-with-cpp kernel/src/mem.S \
//     -Dmemcpy=aios_memcpy -Dmemmove=aios_memmove -Dmemset=aios_memset
//   aarch64-linux-gnu-gcc -O2 -static -fno-builtin -o build/mem test/mem.c build/mem.o && qemu-aarch64 -cpu cortex-a72 build/mem
// 長さ 0〜600 とページの大きさのまわり、src と dst のずれ 0〜15 のすべて、memmove は重なりかたのすべて (前と後ろ)、
// memset は 0 (DC ZVA の道) と 0 でないもの。まわりの見張りのバイトがこわれていないことも見る。終わりに mem: N failed
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void *aios_memcpy(void *, const void *, size_t);
void *aios_memmove(void *, const void *, size_t);
void *aios_memset(void *, int, size_t);

#define BIG (70 * 1024)
#define GUARD 64
static unsigned char a[BIG + 6 * GUARD], b[BIG + 6 * GUARD], want[BIG + 6 * GUARD];
static int fail;

static void fill(unsigned char *p, size_t n, unsigned seed) {
  for (size_t i = 0; i < n; i++) {
    seed = seed * 1103515245u + 12345u;
    p[i] = (unsigned char)(seed >> 16);
  }
}

static void bad(const char *what, size_t n, size_t x, size_t y) {
  if (fail++ < 20)
    printf("FAIL %s n=%zu %zu %zu\n", what, n, x, y);
}

// 見るところ: 書くところとその前後の GUARD バイト (入れ物の頭から n + 3 * GUARD まで)
static void cpy(size_t n, size_t so, size_t dof) {
  size_t m = n + 3 * GUARD;
  fill(a, m, (unsigned)(n * 31 + so));
  fill(b, m, (unsigned)(n * 7 + dof + 1));
  memcpy(want, b, m);
  memcpy(want + GUARD + dof, a + GUARD + so, n);
  void *r = aios_memcpy(b + GUARD + dof, a + GUARD + so, n);
  if (r != b + GUARD + dof || memcmp(b, want, m))
    bad("memcpy", n, so, dof);
}

static void move(size_t n, long d) {
  // 同じ入れ物の中で、src から d ずれたところへ
  size_t src = GUARD + 2 * 16 + (d < 0 ? (size_t)-d : 0);
  size_t dst = src + d, m = n + 4 * GUARD + 80;
  fill(a, m, (unsigned)(n * 13 + d + 100));
  memcpy(want, a, m);
  memmove(want + dst, want + src, n);
  void *r = aios_memmove(a + dst, a + src, n);
  if (r != a + dst || memcmp(a, want, m))
    bad("memmove", n, src, dst);
}

static void set(size_t n, size_t off, int c) {
  size_t m = n + 3 * GUARD;
  fill(b, m, (unsigned)(n + off + c));
  memcpy(want, b, m);
  memset(want + GUARD + off, c, n);
  void *r = aios_memset(b + GUARD + off, c, n);
  if (r != b + GUARD + off || memcmp(b, want, m))
    bad(c ? "memset" : "memset0", n, off, (size_t)c);
}

int main(void) {
  static const size_t big[] = {4095, 4096, 4097, 8191, 8192, 12288, 65535, 65536, 65537, BIG};
  unsigned long dczid;
  __asm__ volatile("mrs %0, dczid_el0" : "=r"(dczid));
  printf("mem: dczid_el0 %#lx (DC ZVA %s)\n", dczid, (dczid & 31) == 4 ? "used" : "not used");
  long checks = 0;
  for (size_t n = 0; n <= 600; n++)
    for (size_t so = 0; so < 16; so++)
      for (size_t dof = 0; dof < 16; dof++, checks++)
        cpy(n, so, dof);
  for (size_t i = 0; i < sizeof big / sizeof big[0]; i++)
    for (size_t so = 0; so < 16; so += 3)
      for (size_t dof = 0; dof < 16; dof += 5, checks++)
        cpy(big[i], so, dof);
  for (size_t n = 0; n <= 300; n++)
    for (long d = -40; d <= 40; d++, checks++)
      move(n, d);
  for (size_t i = 0; i < sizeof big / sizeof big[0]; i++)
    for (long d = -70; d <= 70; d += 7, checks++)
      move(big[i] - GUARD * 3, d);
  for (size_t n = 0; n <= 1200; n++)
    for (size_t off = 0; off < 64; off += (n < 300 ? 1 : 9))
      for (int c = 0; c < 2; c++, checks++)
        set(n, off, c ? 0xa5 : 0);
  for (size_t i = 0; i < sizeof big / sizeof big[0]; i++)
    for (size_t off = 0; off < 64; off += 13, checks += 2) {
      set(big[i], off, 0);
      set(big[i], off, 0x3c);
    }
  printf("mem: %ld checks, %d failed\n", checks, fail);
  return fail != 0;
}
