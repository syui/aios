// 書き足しの速さ (ext4 がブロックをとり extent をのばす速さ。カーネル: extfs.rs の alloc_block_run と ext_extend_leaf)。
// N MiB を 4 KiB ずつ書き足し、書いたところに 3 回上書き (pwrite) して、1 回あたりの平均といちばん長いもの (µs) を 1 行の
// JSON で。Linux でも動く (test/linux-vm.sh exec latest test/bin/append 16 /work)。aios の中は ~ (ext4) で。append [MiB] [DIR]
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e6 + t.tv_nsec / 1e3; }
int main(int argc, char **argv) {
  int mb = argc > 1 ? atoi(argv[1]) : 32;
  const char *dir = argc > 2 ? argv[2] : ".";
  char p[512];
  snprintf(p, sizeof p, "%s/append.%d", dir, getpid());
  int n = mb * 256;
  char buf[4096];
  memset(buf, 'a', sizeof buf);
  int fd = open(p, O_CREAT | O_TRUNC | O_RDWR, 0644);
  double amax = 0, omax = 0, t0 = now();
  for (int i = 0; i < n; i++) {
    double a = now();
    if (write(fd, buf, 4096) != 4096) { perror("write"); return 1; }
    double d = now() - a; if (d > amax) amax = d;
  }
  double t1 = now();
  for (int r = 0; r < 3; r++)
    for (int i = 0; i < n; i++) {
      double a = now();
      if (pwrite(fd, buf, 4096, (off_t)i * 4096) != 4096) { perror("pwrite"); return 1; }
      double d = now() - a; if (d > omax) omax = d;
    }
  double t2 = now();
  close(fd);
  unlink(p);
  printf("{\"mb\":%d,\"append_us\":%.1f,\"append_max_us\":%.0f,\"overwrite_us\":%.1f,\"overwrite_max_us\":%.0f}\n", mb, (t1 - t0) / n, amax, (t2 - t1) / (3.0 * n), omax);
  return 0;
}
