// ファイルまわりの速さ (kernel.json の bench: sysfile、vfs、extfs、vm、block、jbd2 を通るよくある操作)。
// 操作ごとに n 回して 1 回あたりの µs (大きなファイルは MB/s) を、1 行 1 つの JSON で出す:
//   {"bench":"fs","op":"create","n":1000,"ms":52.1,"us":52.1}
// aios でも Linux (このマシン) でも動く。aish の bench (aish-vm) が VM で、/proc/ai/prof の part ごとの時間といっしょに動かす
//   fsbench [DIR] [SCALE]   DIR (既定 .) の下に fsbench.PID を作って使い、おわったら消す。SCALE (既定 1) で回数を何倍にも
// gcc -O2 -static -o fsbench test/fsbench.c
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static char base[4096];
static int fails;

static double now_ms(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static void report(const char *op, long n, double ms) {
  printf("{\"bench\":\"fs\",\"op\":\"%s\",\"n\":%ld,\"ms\":%.1f,\"us\":%.2f}\n", op, n, ms, ms * 1e3 / n);
  fflush(stdout);
}

static void report_mb(const char *op, long bytes, double ms) {
  printf("{\"bench\":\"fs\",\"op\":\"%s\",\"bytes\":%ld,\"ms\":%.1f,\"mb_s\":%.1f}\n", op, bytes, ms, bytes / 1048576.0 / (ms / 1e3));
  fflush(stdout);
}

static void bad(const char *what, const char *path) {
  if (fails++ < 5) fprintf(stderr, "fsbench: %s %s: %s\n", what, path, strerror(errno));
}

static char *p(const char *fmt, long i) {
  static char buf[8192];
  int k = snprintf(buf, sizeof buf, "%s/", base);
  snprintf(buf + k, sizeof buf - k, fmt, i);
  return buf;
}

int main(int argc, char **argv) {
  const char *dir = argc > 1 ? argv[1] : ".";
  long scale = argc > 2 ? atol(argv[2]) : 1;
  if (scale < 1) scale = 1;
  long n = 1000 * scale;
  snprintf(base, sizeof base, "%s/fsbench.%d", dir, (int)getpid());
  if (mkdir(base, 0755) < 0) {
    bad("mkdir", base);
    return 1;
  }
  static char block[65536];
  memset(block, 'a', sizeof block);
  double t0 = now_ms(), t;
  long i;

  // 空のファイルを作る、名前で引く、ないものを引く、開いて閉じる
  t = now_ms();
  for (i = 0; i < n; i++) {
    int fd = open(p("f%ld", i), O_CREAT | O_EXCL | O_WRONLY, 0644);
    if (fd < 0) bad("create", p("f%ld", i)); else close(fd);
  }
  report("create", n, now_ms() - t);
  struct stat st;
  t = now_ms();
  for (i = 0; i < n; i++)
    if (stat(p("f%ld", i), &st) < 0) bad("stat", p("f%ld", i));
  report("stat", n, now_ms() - t);
  t = now_ms();
  for (i = 0; i < n; i++)
    if (stat(p("missing%ld", i), &st) == 0) bad("stat-missing", p("missing%ld", i));
  report("stat-missing", n, now_ms() - t);
  t = now_ms();
  for (i = 0; i < n; i++) {
    int fd = open(p("f%ld", i), O_RDONLY);
    if (fd < 0) bad("open", p("f%ld", i)); else close(fd);
  }
  report("open-close", n, now_ms() - t);

  // 小さなファイルに書いて、読む (ページキャッシュにある)
  t = now_ms();
  for (i = 0; i < n; i++) {
    int fd = open(p("f%ld", i), O_WRONLY);
    if (fd < 0 || write(fd, block, 4096) != 4096) bad("write-4k", p("f%ld", i));
    if (fd >= 0) close(fd);
  }
  report("write-4k", n, now_ms() - t);
  t = now_ms();
  for (i = 0; i < n; i++) {
    int fd = open(p("f%ld", i), O_RDONLY);
    if (fd < 0 || read(fd, block, 4096) != 4096) bad("read-4k", p("f%ld", i));
    if (fd >= 0) close(fd);
  }
  report("read-4k", n, now_ms() - t);

  // n こ入ったディレクトリを読む (20 回)
  long passes = 20 * scale, ents = 0;
  t = now_ms();
  for (i = 0; i < passes; i++) {
    DIR *d = opendir(base);
    if (!d) {
      bad("opendir", base);
      break;
    }
    while (readdir(d)) ents++;
    closedir(d);
  }
  report("readdir", passes, now_ms() - t);

  // 深いパス (10 段) の先を引く
  char deep[8192];
  snprintf(deep, sizeof deep, "%s", base);
  for (i = 0; i < 10; i++) {
    strncat(deep, "/d", sizeof deep - strlen(deep) - 1);
    if (mkdir(deep, 0755) < 0) bad("mkdir", deep);
  }
  strncat(deep, "/leaf", sizeof deep - strlen(deep) - 1);
  int lf = open(deep, O_CREAT | O_WRONLY, 0644);
  if (lf >= 0) close(lf);
  t = now_ms();
  for (i = 0; i < 2 * n; i++)
    if (stat(deep, &st) < 0) bad("deep-stat", deep);
  report("deep-stat", 2 * n, now_ms() - t);

  // 名前を変える、消す、ディレクトリを作って消す
  t = now_ms();
  for (i = 0; i < n; i++) {
    char from[8192];
    snprintf(from, sizeof from, "%s", p("f%ld", i));
    if (rename(from, p("g%ld", i)) < 0) bad("rename", from);
  }
  report("rename", n, now_ms() - t);
  t = now_ms();
  for (i = 0; i < n; i++)
    if (unlink(p("g%ld", i)) < 0) bad("unlink", p("g%ld", i));
  report("unlink", n, now_ms() - t);
  t = now_ms();
  for (i = 0; i < n / 2; i++) {
    if (mkdir(p("m%ld", i), 0755) < 0) bad("mkdir", p("m%ld", i));
    if (rmdir(p("m%ld", i)) < 0) bad("rmdir", p("m%ld", i));
  }
  report("mkdir-rmdir", n / 2, now_ms() - t);

  // 大きなファイル (16 MiB) を 64 KiB ずつ書いて fsync、読みなおす (ページキャッシュにある)
  long big = 16L * 1048576 * scale;
  int fd = open(p("big%ld", 0), O_CREAT | O_TRUNC | O_RDWR, 0644);
  if (fd < 0) bad("open", p("big%ld", 0));
  t = now_ms();
  for (i = 0; fd >= 0 && i < big / (long)sizeof block; i++)
    if (write(fd, block, sizeof block) != (ssize_t)sizeof block) bad("write-seq", "big");
  if (fd >= 0 && fsync(fd) < 0) bad("fsync", "big");
  report_mb("write-seq", big, now_ms() - t);
  t = now_ms();
  if (fd >= 0) lseek(fd, 0, SEEK_SET);
  for (i = 0; fd >= 0 && i < big / (long)sizeof block; i++)
    if (read(fd, block, sizeof block) != (ssize_t)sizeof block) bad("read-seq", "big");
  report_mb("read-seq", big, now_ms() - t);
  if (fd >= 0) close(fd);
  unlink(p("big%ld", 0));

  // 小さく書いて fsync (ジャーナルの commit) を 50 回
  long ns = 50 * scale;
  fd = open(p("sync%ld", 0), O_CREAT | O_TRUNC | O_WRONLY, 0644);
  t = now_ms();
  for (i = 0; fd >= 0 && i < ns; i++)
    if (write(fd, block, 4096) != 4096 || fsync(fd) < 0) bad("fsync", "sync");
  report("write-fsync", ns, now_ms() - t);
  if (fd >= 0) close(fd);
  unlink(p("sync%ld", 0));

  // 片づけ
  for (i = 10; i > 0; i--) {
    char *e = strrchr(deep, '/');
    *e = 0;
    if (i == 10) {
      char leaf[8192];
      snprintf(leaf, sizeof leaf, "%s/leaf", deep);
      unlink(leaf);
    }
    rmdir(deep);
  }
  if (rmdir(base) < 0) bad("rmdir", base);
  printf("{\"bench\":\"fs\",\"total_ms\":%.1f,\"entries_read\":%ld,\"fails\":%d}\n", now_ms() - t0, ents, fails);
  return fails != 0;
}
