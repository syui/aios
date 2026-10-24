// 棚にないファイルの大きなロックなしの open (カーネルの fastdir::fast_open) と、消して作りなおす (ロックなしの unlink と
// create)、別の名前に書いて rename で置きかえる (ふつうの道) を、いくつもの CPU で同時に: aios の中で動かす
// (aarch64-linux-gnu-gcc -O2 -static -o openrace test/openrace.c)。ファイル f<i> の中身はいつも "<i> <gen>\n"。開けた
// ものを読むと、空 (作った直後) か、同じ i のものでなければならない (消えて別のファイルに使われた inode を開くと、
// ちがう i が読める)。大きさはいつも 100 + i なので、名前で stat したものも 0 か 100 + i (ほかのファイルに使われた inode の
// stat を、消した名前の答えにしていない)。開けないのは ENOENT だけ。openrace DIR [SECONDS]
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#define NF 64
static char dir[4096];
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static void put(const char *path, int i, long gen, int excl) {
  int fd = open(path, O_CREAT | O_WRONLY | (excl ? O_EXCL : O_TRUNC), 0644);
  if (fd < 0) return;
  char b[256];
  memset(b, ' ', sizeof b);
  int n = snprintf(b, sizeof b, "%d %ld\n", i, gen);
  b[n] = ' ';
  if (write(fd, b, 100 + i) != 100 + i) perror("write");
  close(fd);
}
int main(int argc, char **argv) {
  double secs = argc > 2 ? atof(argv[2]) : 5;
  snprintf(dir, sizeof dir, "%s/openrace.%d", argv[1], (int)getpid());
  mkdir(dir, 0755);
  char p[4200], q[4200];
  for (int i = 0; i < NF; i++) { snprintf(p, sizeof p, "%s/f%d", dir, i); put(p, i, 0, 1); }
  int kids[4];
  for (int k = 0; k < 4; k++) {
    if ((kids[k] = fork()) == 0) {
      srand(k * 7919 + 1);
      double end = now() + secs;
      long ops = 0, bad = 0, gone = 0, empty = 0, gen = 0;
      while (now() < end) {
        int i = rand() % NF;
        snprintf(p, sizeof p, "%s/f%d", dir, i);
        if (k >= 2 && (ops & 3) == 3) {
          // ときどき名前で stat
          struct stat st;
          if (stat(p, &st) < 0) { if (errno != ENOENT) { bad++; fprintf(stderr, "stat %s: %s\n", p, strerror(errno)); } else gone++; }
          else if (st.st_size != 0 && st.st_size != 100 + i) { bad++; fprintf(stderr, "stat %s: size %lld\n", p, (long long)st.st_size); }
          ops++;
          continue;
        }
        if (k >= 2) {
          // 2 つは開いて読む (すぐ閉じるもの、いくつか開いたまま読むもの)
          int fd = open(p, O_RDONLY);
          if (fd < 0) { if (errno != ENOENT) { bad++; fprintf(stderr, "open %s: %s\n", p, strerror(errno)); } else gone++; ops++; continue; }
          char b[64] = {0};
          int n = read(fd, b, sizeof b - 1);
          if (n == 0) empty++;
          else if (n < 0 || atoi(b) != i) { bad++; fprintf(stderr, "read %s: got %d bytes '%s'\n", p, n, b); }
          struct stat st;
          if (fstat(fd, &st) < 0 || !S_ISREG(st.st_mode)) bad++;
          close(fd);
        } else if (k == 0) {
          // 消して作りなおす (ロックなしの unlink と create)
          gen++;
          unlink(p);
          put(p, i, gen, 1);
        } else {
          // 別の名前に書いてから rename で置きかえる (ふつうの道)
          gen++;
          snprintf(q, sizeof q, "%s/t%d", dir, i);
          put(q, i, gen, 0);
          if (rename(q, p) < 0) unlink(q);
        }
        ops++;
      }
      printf("worker %d ops %ld bad %ld gone %ld empty %ld\n", k, ops, bad, gone, empty);
      fflush(stdout);
      _exit(bad ? 1 : 0);
    }
  }
  int fails = 0;
  for (int k = 0; k < 4; k++) { int s; waitpid(kids[k], &s, 0); if (!WIFEXITED(s) || WEXITSTATUS(s)) fails++; }
  printf("openrace: %s\n", fails ? "BAD" : "ok");
  return fails != 0;
}
