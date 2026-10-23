// fsync のあいだ、ほかのプロセスがどれだけ止められるか。aios の中で動かす (aarch64-linux-gnu-gcc -O2 -static -o fsyncstall
// test/fsyncstall.c)。Linux でも動く。
//   1 つのプロセスが自分のファイルに 4 KiB を書いて fsync するのをくりかえし (書き手)、もう 1 つが、カーネルの大きなロックを
//   取るシステムコール (uname) をくりかえして、1 回にかかった時間を測る (見はり)。前の aios は、fsync が書き終わるまで
//   大きなロックを持ってディスクを回って待ったので、見はりはそのあいだ止まった。おそいディスク (bin/run.sh の
//   AIOS_DISK_OPTS=throttling.iops-write=50 など) ほど長い。Linux の fsync はほかのプロセスを止めない
//   fsyncstall [-t SEC] [DIR]   はじめに見はりだけで SEC 秒、つぎに書き手といっしょに SEC 秒。1 行の JSON で:
//   {"alone":{"calls":N,"max_us":U,"over_1ms":A,"over_10ms":B},"with_fsync":{...,"fsyncs":F}}
#define _GNU_SOURCE
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct res {
  long calls, max_us, over1, over10, fsyncs;
};

static long now_us(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000L + t.tv_nsec / 1000;
}

static void watch(int secs, struct res *r) {
  struct utsname u;
  long end = now_us() + secs * 1000000L;
  for (;;) {
    long t0 = now_us();
    if (t0 >= end) break;
    uname(&u);
    long dt = now_us() - t0;
    r->calls++;
    if (dt > r->max_us) r->max_us = dt;
    if (dt > 1000) r->over1++;
    if (dt > 10000) r->over10++;
  }
}

int main(int argc, char **argv) {
  int secs = 5, c;
  while ((c = getopt(argc, argv, "t:")) != -1) {
    if (c == 't') secs = atoi(optarg);
    else return 2;
  }
  const char *dir = optind < argc ? argv[optind] : ".";
  char path[512];
  snprintf(path, sizeof path, "%s/fsyncstall.%d", dir, getpid());
  struct res alone = {0}, with = {0};
  watch(secs, &alone);
  // 書き手の fsync の数は、分けたページで返してもらう
  long *shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  *shared = 0;
  pid_t w = fork();
  if (w == 0) {
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    char buf[4096];
    memset(buf, 'x', sizeof buf);
    long end = now_us() + (secs + 1) * 1000000L;
    while (now_us() < end) {
      if (write(fd, buf, sizeof buf) != sizeof buf) break;
      fsync(fd);
      (*shared)++;
    }
    close(fd);
    _exit(0);
  }
  usleep(200000);
  watch(secs, &with);
  kill(w, SIGKILL);
  waitpid(w, NULL, 0);
  with.fsyncs = *shared;
  unlink(path);
  printf("{\"alone\":{\"calls\":%ld,\"max_us\":%ld,\"over_1ms\":%ld,\"over_10ms\":%ld},\"with_fsync\":{\"calls\":%ld,\"max_us\":%ld,\"over_1ms\":%ld,\"over_10ms\":%ld,\"fsyncs\":%ld}}\n",
         alone.calls, alone.max_us, alone.over1, alone.over10, with.calls, with.max_us, with.over1, with.over10, with.fsyncs);
  return 0;
}
