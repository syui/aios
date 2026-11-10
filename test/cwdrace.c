// fd の表 (いまのディレクトリもそこにある) をスレッドで分けているときの、大きなロックなしの open と stat (カーネルの
// file::fast_open と statc.rs: 門を通って表と cwd を読む) が、ほかのスレッドの chdir や dup2、close と同時でも
// Linux と同じ答えになるか: aios の中で動かす (aarch64-linux-gnu-gcc -O2 -static -pthread -o cwdrace test/cwdrace.c)。
// DIR/A と DIR/B に同じ名前のファイル f0 .. f15 (中身は "A i" と "B i") を作り、A に chdir してから:
//   4 つのスレッドが、名前 (cwd から) で open して読み、fstat し、閉じる。stat もする。中身は A か B のどちらかの
//   ファイルのもので、fstat の inode もそのファイルのもの (2 つのスレッドが同じ fd に置くと、ほかのファイルが読める)。
//   ないと言われることはない。もう 1 つのスレッドが A と B を chdir で行き来し、dup2 で高い fd に置いては閉じる
// 何秒かまわしたあと、開いたままの fd が残っていない (閉じた数が開いた数と同じ) ことも見る
// aios なら、/proc/bkl のロックなしの open の数が増えたか (表を分けていても速い道を通ったか) も見る。
// 1 行に ok NAME か FAIL NAME、おしまいに cwdrace: N failed
//   cwdrace [DIR] [SECONDS]   DIR (既定 /tmp) の下に .cwdrace.PID を作って
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define NF 16

static int fails;
static ino_t inoA[NF], inoB[NF];
static volatile int stop;
static int bad, opens;
static char why[256];

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static void note(const char *fmt, int i, long v) {
  if (__atomic_fetch_add(&bad, 1, __ATOMIC_RELAXED) == 0)
    snprintf(why, sizeof why, fmt, i, v);
}

static void *opener(void *a) {
  unsigned r = (unsigned)(long)a * 2654435761u + 1;
  char name[16], buf[32];
  while (!stop) {
    int i = rand_r(&r) % NF;
    snprintf(name, sizeof name, "f%d", i);
    int fd = open(name, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
      note("open f%d: errno %ld", i, errno);
      continue;
    }
    __atomic_fetch_add(&opens, 1, __ATOMIC_RELAXED);
    ssize_t n = read(fd, buf, sizeof buf - 1);
    struct stat st;
    if (n < 3 || fstat(fd, &st)) {
      note("read f%d: %ld", i, n);
    } else {
      buf[n] = 0;
      char want[16];
      snprintf(want, sizeof want, "%c %d\n", buf[0], i);
      if ((buf[0] != 'A' && buf[0] != 'B') || strcmp(buf, want))
        note("f%d has the wrong content (%ld bytes)", i, n);
      else if (st.st_ino != (buf[0] == 'A' ? inoA[i] : inoB[i]))
        note("f%d: fstat inode %ld is not the file that was read", i, (long)st.st_ino);
    }
    if (close(fd))
      note("close f%d: errno %ld", i, errno);
    if (stat(name, &st))
      note("stat f%d: errno %ld", i, errno);
    else if (st.st_ino != inoA[i] && st.st_ino != inoB[i])
      note("stat f%d: inode %ld", i, (long)st.st_ino);
  }
  return NULL;
}

static void *mover(void *a) {
  (void)a;
  int k = 0;
  while (!stop) {
    if (chdir(k & 1 ? "../A" : "../B"))
      note("chdir %d: errno %ld", k, errno);
    int hi = 200 + k % 8;
    if (dup2(0, hi) != hi)
      note("dup2 %d: errno %ld", hi, errno);
    if (close(hi))
      note("close %d: errno %ld", hi, errno);
    k++;
  }
  return NULL;
}

static long fast_opens(void) {
  FILE *f = fopen("/proc/bkl", "r");
  char line[512];
  long n = -1;
  const char *key = "ロックなしの open / close (file の棚): 棚から開いた ";
  while (f && fgets(line, sizeof line, f)) {
    char *p = strstr(line, key);
    if (p) {
      long shelf = atol(p + strlen(key));
      char *q = strstr(p, "新しく開いた ");
      n = shelf + (q ? atol(q + strlen("新しく開いた ")) : 0);
    }
  }
  if (f)
    fclose(f);
  return n;
}

static int count_fds(void) {
  int n = 0;
  for (int fd = 0; fd < 1024; fd++)
    if (fcntl(fd, F_GETFD) >= 0)
      n++;
  return n;
}

int main(int argc, char **argv) {
  const char *dir = argc > 1 ? argv[1] : "/tmp";
  int secs = argc > 2 ? atoi(argv[2]) : 3;
  char base[512], p[600];
  snprintf(base, sizeof base, "%s/.cwdrace.%d", dir, getpid());
  if (mkdir(base, 0755)) {
    perror(base);
    return 1;
  }
  for (int d = 0; d < 2; d++) {
    snprintf(p, sizeof p, "%s/%c", base, "AB"[d]);
    mkdir(p, 0755);
    for (int i = 0; i < NF; i++) {
      snprintf(p, sizeof p, "%s/%c/f%d", base, "AB"[d], i);
      int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
      char c[16];
      int n = snprintf(c, sizeof c, "%c %d\n", "AB"[d], i);
      struct stat st;
      if (fd < 0 || write(fd, c, n) != n || fstat(fd, &st)) {
        perror(p);
        return 1;
      }
      (d ? inoB : inoA)[i] = st.st_ino;
      close(fd);
    }
  }
  snprintf(p, sizeof p, "%s/A", base);
  if (chdir(p)) {
    perror(p);
    return 1;
  }
  int fds0 = count_fds();
  long fast0 = fast_opens();
  pthread_t t[5];
  for (int i = 0; i < 4; i++)
    pthread_create(&t[i], NULL, opener, (void *)(long)i);
  pthread_create(&t[4], NULL, mover, NULL);
  struct timespec ts = {secs, 0};
  nanosleep(&ts, NULL);
  stop = 1;
  for (int i = 0; i < 5; i++)
    pthread_join(t[i], NULL);
  long fast1 = fast_opens();
  char name[700];
  snprintf(name, sizeof name, "race %s (%d opens)%s%s", dir, opens, bad ? ": " : "", bad ? why : "");
  check(name, bad == 0 && opens > 0);
  check("no fds left open", count_fds() == fds0);
  // aios の ext4: 表を分けていても速い道で開けたか (半分より多く。/proc/bkl がなければ Linux なので見ない)
  if (fast0 >= 0 && strncmp(dir, "/tmp", 4)) {
    snprintf(name, sizeof name, "fast %s (%ld of %d opens lockless)", dir, fast1 - fast0, opens);
    check(name, fast1 - fast0 > opens / 2);
  }
  if (chdir(dir) == 0) {
    for (int d = 0; d < 2; d++)
      for (int i = 0; i < NF; i++) {
        snprintf(p, sizeof p, "%s/%c/f%d", base, "AB"[d], i);
        unlink(p);
      }
    for (int d = 0; d < 2; d++) {
      snprintf(p, sizeof p, "%s/%c", base, "AB"[d]);
      rmdir(p);
    }
    rmdir(base);
  }
  printf("cwdrace: %d failed\n", fails);
  return fails != 0;
}
