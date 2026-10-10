// splice と pread のテスト (カーネル): aios の中で動かす
// (zig cc -target aarch64-linux-musl -O2 -static -o splice-test test/splice.c -lpthread)。
// 大きなロックなしの道 (file.rs の fast_splice、fast_pread) とふつうの道で、中身とオフセットが同じか:
// ファイル → パイプ (off_in あり・なし)、パイプ → /dev/null、いくつものスレッドから同時の pread
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s\n", name); fails++; } } while (0)

#define SIZE (3 * 1000 * 1000 + 123)
static unsigned char *want;
static const char *path = "/tmp/.splice-test";

static int pread_all(int fd, unsigned char *b, size_t n, off_t off) {
  size_t d = 0;
  while (d < n) { ssize_t r = pread(fd, b + d, n - d, off + d); if (r <= 0) return -1; d += r; }
  return 0;
}

static void *reader(void *a) {
  long id = (long)a;
  int fd = open(path, O_RDONLY);
  unsigned char *b = malloc(70000);
  long bad = 0;
  for (int i = 0; i < 300; i++) {
    off_t off = ((id * 7919 + i * 104729) % (SIZE - 70000));
    if (pread_all(fd, b, 70000, off) != 0 || memcmp(b, want + off, 70000) != 0) bad++;
  }
  close(fd);
  free(b);
  return (void *)bad;
}

int main(void) {
  want = malloc(SIZE);
  for (int i = 0; i < SIZE; i++) want[i] = (unsigned char)(i * 2654435761u >> 13);
  int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
  write(fd, want, SIZE);
  close(fd);
  fd = open(path, O_RDONLY);
  unsigned char *got = malloc(SIZE);
  pread_all(fd, got, SIZE, 0); // ページキャッシュに入れる
  // ファイル → パイプ、オフセットはファイルの (進む)
  int p[2];
  pipe(p);
  size_t done = 0;
  int ok = 1;
  while (done < SIZE) {
    ssize_t n = splice(fd, NULL, p[1], NULL, 65536, 0);
    if (n <= 0) { ok = 0; break; }
    size_t k = 0;
    while (k < (size_t)n) { ssize_t r = read(p[0], got + done + k, n - k); if (r <= 0) { ok = 0; break; } k += r; }
    done += n;
  }
  CHECK("splice-file-pipe", ok && done == SIZE && memcmp(got, want, SIZE) == 0);
  CHECK("splice-offset-advanced", lseek(fd, 0, SEEK_CUR) == SIZE);
  // off_in あり: その値が進み、ファイルのオフセットは変わらない
  lseek(fd, 5, SEEK_SET);
  loff_t off = 1000000;
  ssize_t n = splice(fd, &off, p[1], NULL, 40000, 0);
  unsigned char b[40000];
  ssize_t r = read(p[0], b, sizeof b);
  CHECK("splice-off-in", n == 40000 && r == 40000 && off == 1040000 && memcmp(b, want + 1000000, 40000) == 0);
  CHECK("splice-off-in-keeps-offset", lseek(fd, 0, SEEK_CUR) == 5);
  // ファイルの終わり
  off = SIZE;
  CHECK("splice-eof", splice(fd, &off, p[1], NULL, 100, 0) == 0);
  off = SIZE - 10;
  n = splice(fd, &off, p[1], NULL, 100, 0);
  r = read(p[0], b, sizeof b);
  CHECK("splice-tail", n == 10 && r == 10 && memcmp(b, want + SIZE - 10, 10) == 0);
  // パイプ → /dev/null: 捨てた分だけ減る
  int null = open("/dev/null", O_WRONLY);
  write(p[1], want, 30000);
  n = splice(p[0], NULL, null, NULL, 100000, 0);
  int avail = -1;
  ioctl(p[0], 0x541B /* FIONREAD */, &avail);
  CHECK("splice-pipe-null", n == 30000 && avail == 0);
  write(p[1], want, 30000);
  n = splice(p[0], NULL, null, NULL, 1000, 0);
  r = read(p[0], b, sizeof b);
  CHECK("splice-pipe-null-part", n == 1000 && r == 29000 && memcmp(b, want + 1000, 29000) == 0);
  // pread: 範囲の外、ちょうど終わり
  CHECK("pread-eof", pread(fd, b, 100, SIZE) == 0);
  CHECK("pread-tail", pread(fd, b, 100, SIZE - 7) == 7 && memcmp(b, want + SIZE - 7, 7) == 0);
  // いくつものスレッドから同時に pread
  pthread_t t[4];
  for (long i = 0; i < 4; i++) pthread_create(&t[i], 0, reader, (void *)i);
  long bad = 0;
  for (int i = 0; i < 4; i++) { void *v; pthread_join(t[i], &v); bad += (long)v; }
  CHECK("pread-threads", bad == 0);
  close(fd);
  unlink(path);
  printf("splice: %d failed\n", fails);
  return fails;
}
