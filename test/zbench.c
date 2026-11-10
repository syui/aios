// zbench BASE [P...]: 本物のビルドの速さ。BASE/zig/zig cc で BASE/src/*.c をみなコンパイル (-c) する。P こずつ並べて
// (既定は 1 と 4)、それぞれ新しいキャッシュ (BASE/zc、BASE/zl) で。かかった時間と、/proc/stat の CPU の時間 (user、
// system、idle) を出す。aios と Linux で同じものを動かしてくらべる (doc/aios.md の「本物のビルド」):
//   zig は PyPI の ziglang (aarch64 の wheel) の zig と lib を BASE/zig/ に、test/*.c を BASE/src/ に置く
//   aios の中で: ./zbench $HOME           Linux: mkfs.ext4 -d で BASE を作ったディスクを /work に付けて ./zbench /work
//   aarch64-linux-gnu-gcc -O2 -static -o zbench test/zbench.c
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <ftw.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static char base[512];
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static int rmf(const char *p, const struct stat *s, int f, struct FTW *w) { (void)s; (void)f; (void)w; return remove(p); }
static void rmrf(const char *p) { nftw(p, rmf, 16, FTW_DEPTH | FTW_PHYS); }
static int cmp(const void *a, const void *b) { return strcmp(*(char **)a, *(char **)b); }
static void cpustat(unsigned long long *v) {
  FILE *f = fopen("/proc/stat", "r");
  memset(v, 0, 4 * sizeof *v);
  if (f) { if (fscanf(f, "cpu %llu %llu %llu %llu", &v[0], &v[1], &v[2], &v[3]) != 4) memset(v, 0, 4 * sizeof *v); fclose(f); }
}
static pid_t spawn(const char *src, const char *name) {
  pid_t p = fork();
  if (p == 0) {
    char zig[600], in[800], out[800];
    snprintf(zig, sizeof zig, "%s/zig/zig", base);
    snprintf(in, sizeof in, "%s/src/%s", base, src);
    snprintf(out, sizeof out, "%s/out/%s.o", base, name);
    int fd = open("/dev/null", O_WRONLY);
    if (fd >= 0) { dup2(fd, 1); dup2(fd, 2); }
    execl(zig, "zig", "cc", "-target", "aarch64-linux-musl", "-O2", "-c", in, "-o", out, (char *)0);
    _exit(127);
  }
  return p;
}
int main(int argc, char **argv) {
  snprintf(base, sizeof base, "%s", argc > 1 ? argv[1] : ".");
  char d[600];
  snprintf(d, sizeof d, "%s/zc", base); setenv("ZIG_GLOBAL_CACHE_DIR", d, 1);
  snprintf(d, sizeof d, "%s/zl", base); setenv("ZIG_LOCAL_CACHE_DIR", d, 1);
  setenv("HOME", base, 1);
  char *names[1024]; int n = 0;
  snprintf(d, sizeof d, "%s/src", base);
  DIR *dir = opendir(d);
  struct dirent *e;
  while (dir && (e = readdir(dir)) && n < 1024) { size_t l = strlen(e->d_name); if (l > 2 && !strcmp(e->d_name + l - 2, ".c")) names[n++] = strdup(e->d_name); }
  if (dir) closedir(dir);
  qsort(names, n, sizeof *names, cmp);
  // ならし (zig を読みこむ)
  snprintf(d, sizeof d, "%s/out", base); mkdir(d, 0755);
  int st; waitpid(spawn(names[0], "warm"), &st, 0);
  for (int a = 2; a < (argc > 2 ? argc : 4); a++) {
    int P = argc > 2 ? atoi(argv[a]) : (a == 2 ? 1 : 4);
    snprintf(d, sizeof d, "%s/zc", base); rmrf(d);
    snprintf(d, sizeof d, "%s/zl", base); rmrf(d);
    snprintf(d, sizeof d, "%s/out", base); rmrf(d); mkdir(d, 0755);
    sync();
    unsigned long long c0[4], c1[4];
    cpustat(c0);
    double t0 = now();
    int running = 0, next = 0, fails = 0;
    while (next < n || running) {
      if (next < n && running < P) {
        char name[512]; snprintf(name, sizeof name, "%s", names[next]); name[strlen(name) - 2] = 0;
        spawn(names[next], name); next++; running++;
        continue;
      }
      wait(&st); running--;
      if (!WIFEXITED(st) || WEXITSTATUS(st)) fails++;
    }
    double t1 = now();
    cpustat(c1);
    int objs = 0;
    snprintf(d, sizeof d, "%s/out", base);
    dir = opendir(d);
    while (dir && (e = readdir(dir))) if (e->d_name[0] != '.') objs++;
    if (dir) closedir(dir);
    printf("P=%d %.1f s objs %d fails %d cpu user %llu sys %llu idle %llu (ticks)\n", P, t1 - t0, objs, fails, c1[0] - c0[0], c1[2] - c0[2], c1[3] - c0[3]);
    fflush(stdout);
  }
  return 0;
}
