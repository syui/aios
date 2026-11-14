// 伸ばす書きこみ (ファイルの終わりに 4 KiB ずつ) の 1 回の µs: 1 つのプロセスと P つのプロセス (それぞれ自分のファイル。
// 8 MiB で ftruncate して 0 から)。P つのときは、合わせて 1 回あたり (伸びは 1 つのときの何倍まわせたか)。aios と Linux
// (test/linux-vm.sh exec) で同じものを動かしてくらべる。doc/aios.md の「ファイルの終わりへの書きたし」
//   ./appendbench [DIR] [P] [MS]   (MS: 1 つと P つのそれぞれを動かす長さ。既定 300。ゆれが大きいときは 2000 など)
//   aarch64-linux-gnu-gcc -O2 -static -o appendbench test/appendbench.c
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static long run(const char *dir, int id, double ms) {
  char p[256]; snprintf(p, sizeof p, "%s/append.%d.%d", dir, getpid(), id);
  int fd = open(p, O_CREAT | O_TRUNC | O_WRONLY, 0644);
  char buf[4096]; memset(buf, 'a', sizeof buf);
  long n = 0, off = 0; double end = now() + ms / 1000;
  while (now() < end) {
    if (write(fd, buf, sizeof buf) != sizeof buf) { perror("write"); break; }
    n++; off += sizeof buf;
    if (off >= 8 << 20) {
      if (ftruncate(fd, 0) || lseek(fd, 0, SEEK_SET)) { perror("ftruncate"); break; }
      off = 0;
    }
  }
  close(fd); unlink(p);
  return n;
}
int main(int argc, char **argv) {
  const char *dir = argc > 1 ? argv[1] : ".";
  int P = argc > 2 ? atoi(argv[2]) : 4;
  double ms = argc > 3 ? atof(argv[3]) : 300;
  long n1 = run(dir, 0, ms);
  printf("append-4k procs 1: %.2f us\n", ms * 1000 / n1);
  int pipefd[2];
  if (pipe(pipefd)) { perror("pipe"); return 1; }
  for (int i = 0; i < P; i++) if (fork() == 0) { long n = run(dir, i + 1, ms); _exit(write(pipefd[1], &n, sizeof n) != sizeof n); }
  long tot = 0;
  for (int i = 0; i < P; i++) { long n = 0; if (read(pipefd[0], &n, sizeof n) == sizeof n) tot += n; wait(NULL); }
  printf("append-4k procs %d: %.2f us (speedup %.2f)\n", P, ms * 1000 / tot, (double)tot / n1);
  return 0;
}
