// 大きなファイルを切りつめる・消すのにかかる時間 (ext4 がブロックを放す速さ。カーネル: extfs.rs の free_blocks)。
// N MiB を書いて fsync してから ftruncate で 0 に、もう一度書いて close してから unlink し、それぞれの ms を 1 行の JSON で。
// Linux でも動く (test/linux-vm.sh exec latest test/bin/trunc 64 /work)。aios の中は ~ (ext4) で
//   aarch64-linux-gnu-gcc -O2 -static -o test/bin/trunc test/trunc.c
//   trunc [MiB (既定 64)] [DIR (既定 .)]
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static int fill(const char *p, int mb) {
  int fd = open(p, O_CREAT | O_TRUNC | O_RDWR, 0644);
  static char buf[1 << 20];
  memset(buf, 'x', sizeof buf);
  for (int i = 0; i < mb; i++) if (write(fd, buf, sizeof buf) != sizeof buf) { perror("write"); exit(1); }
  fsync(fd);
  return fd;
}
int main(int argc, char **argv) {
  int mb = argc > 1 ? atoi(argv[1]) : 64;
  const char *dir = argc > 2 ? argv[2] : ".";
  char p[512];
  snprintf(p, sizeof p, "%s/trunc.%d", dir, getpid());
  int fd = fill(p, mb);
  double t0 = now();
  if (ftruncate(fd, 0)) perror("ftruncate");
  double t1 = now();
  fsync(fd);
  close(fd);
  fd = fill(p, mb);
  close(fd);
  double t2 = now();
  unlink(p);
  double t3 = now();
  sync();
  printf("{\"mb\":%d,\"truncate_ms\":%.1f,\"unlink_ms\":%.1f}\n", mb, t1 - t0, t3 - t2);
  return 0;
}
