// 大きなロックなしの munmap (vm::PageTable::fast_unmap) と、大きなロックの中の open / close が、同じファイルの inode の
// Rc の数を同時にさわらないこと (todo の mem-corrupt-stress: 前は fast_unmap の split が、分けない領域も写して落とし、
// Rc の数を大きなロックなしで +1 と -1 していた。ほかの CPU の open が足した数が消え、まだ開いている inode が返った)。
// 写す子 (open、mmap、close、さわる、munmap) と、開く子 (open と close、stat) を、同じ tmpfs のファイルで秒数だけまわし、
// 終わったらファイルを読んで中身を見る。カーネルがこわれていないかは、heap poison を入れて動かし、kmsg と、このあとの
// ctests で見る。出すだけのテスト: ./rcrace [秒] [DIR]
//   aarch64-linux-gnu-gcc -O2 -static -o rcrace test/rcrace.c
#define _GNU_SOURCE
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static char path[256];

static double now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec + t.tv_nsec / 1e9;
}

static int mapper(double end) {
  long n = 0, bad = 0;
  while (now() < end) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
      continue;
    char *p = mmap(NULL, 8192, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (p == MAP_FAILED)
      continue;
    if (p[0] != 'r' || p[8191] != 'r')
      bad++;
    munmap(p, 8192);
    n++;
  }
  printf("mapper: %ld maps, %ld bad\n", n, bad);
  return bad != 0;
}

static int opener(double end) {
  long n = 0;
  struct stat st;
  while (now() < end) {
    int fd = open(path, O_RDONLY);
    if (fd >= 0)
      close(fd);
    stat(path, &st);
    n++;
  }
  printf("opener: %ld opens\n", n);
  return 0;
}

int main(int argc, char **argv) {
  int secs = argc > 1 ? atoi(argv[1]) : 20;
  const char *dir = argc > 2 ? argv[2] : "/tmp";
  setvbuf(stdout, NULL, _IONBF, 0);
  snprintf(path, sizeof path, "%s/rcrace.%d", dir, getpid());
  char buf[8192];
  memset(buf, 'r', sizeof buf);
  int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
  if (fd < 0 || write(fd, buf, sizeof buf) != sizeof buf) {
    perror("create");
    return 2;
  }
  close(fd);
  double end = now() + secs;
  pid_t kids[6];
  for (int i = 0; i < 6; i++) {
    kids[i] = fork();
    if (kids[i] == 0)
      _exit(i < 3 ? mapper(end) : opener(end));
  }
  int fail = 0;
  for (int i = 0; i < 6; i++) {
    int st;
    waitpid(kids[i], &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
      fail++;
  }
  fd = open(path, O_RDONLY);
  memset(buf, 0, sizeof buf);
  if (fd < 0 || read(fd, buf, sizeof buf) != sizeof buf || buf[0] != 'r' || buf[8191] != 'r')
    fail++;
  close(fd);
  unlink(path);
  printf("rcrace: %d failed\n", fail);
  return fail != 0;
}
