// スレッドで分けた fd の表の、大きなロックなしの close (file::fast_close_shared) のたしかめ。Linux と同じ答えになるか:
//   semantics  スレッドのあるプロセスで: 閉じた fd の read は EBADF、2 度目の close も EBADF、閉じた fd は
//              /proc/self/fd から消え、いちばん小さい空いた fd がまた使われる
//   churn      4 つのスレッドが 64 のファイルをかわるがわる開いて、読んで、閉じる (3000 回ずつ)。読んだものはいつも
//              そのファイルの中身。そのあいだに、ほかのスレッドが dup2 と close、fstat、/proc/self/fd を読む、fork と _exit
//   racing     1 つのスレッドが同じ fd を閉じては開きなおすあいだに、ほかのスレッドがその fd を read する: 答えは
//              そのファイルの中身か EBADF (ほかのファイルの中身や、落ちることはない)
//   unlink     閉じてから消したファイルの場所は返る (statvfs の空きがもどる)
// aios なら、/proc/bkl のスレッドで分けた表のロックなしの close の数が増えること
// 1 行に ok NAME か FAIL NAME、おしまいに closeshared: N failed
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/wait.h>
#include <unistd.h>

#define NFILES 64
#define ROUNDS 3000

static int fails;
static char dir[256];
static volatile int stop, idle_stop;
static int bad;
static char why[256];

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static void note(const char *fmt, long a, long b) {
  if (__atomic_fetch_add(&bad, 1, __ATOMIC_RELAXED) == 0)
    snprintf(why, sizeof why, fmt, a, b);
}

static void path(char *out, size_t n, int i) {
  snprintf(out, n, "%s/f%02d", dir, i);
}

// /proc/bkl の「スレッドで分けた表のロックなしの close: N」(なければ -1)
static long fast_count(void) {
  FILE *f = fopen("/proc/bkl", "r");
  if (!f)
    return -1;
  char line[512];
  long n = -1;
  const char *key = "スレッドで分けた表のロックなしの close: ";
  while (fgets(line, sizeof line, f))
    if (!strncmp(line, key, strlen(key))) {
      n = atol(line + strlen(key));
      break;
    }
  fclose(f);
  return n;
}

static void *idle(void *a) {
  (void)a;
  while (!idle_stop)
    usleep(1000);
  return NULL;
}

static void *churn(void *a) {
  long me = (long)a;
  char p[300], buf[16];
  for (int r = 0; r < ROUNDS && !stop; r++) {
    int i = (me * 7 + r) % NFILES;
    path(p, sizeof p, i);
    int fd = open(p, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
      note("churn: open errno %ld", errno, 0);
      continue;
    }
    ssize_t n = read(fd, buf, sizeof buf);
    if (n != 3 || (buf[0] - '0') * 10 + (buf[1] - '0') != i)
      note("churn: file %ld read something else (%ld bytes)", i, n);
    if (close(fd))
      note("churn: close errno %ld", errno, 0);
  }
  return NULL;
}

static void *meddler(void *a) {
  (void)a;
  char p[300];
  path(p, sizeof p, 0);
  int src = open(p, O_RDONLY);
  struct stat st;
  for (int k = 0; !stop; k++) {
    int hi = 200 + k % 16;
    if (dup2(src, hi) != hi)
      note("meddler: dup2 errno %ld", errno, 0);
    if (close(hi))
      note("meddler: close errno %ld", errno, 0);
    // ほかのスレッドのものかもしれない fd: 開いているか EBADF
    if (fstat(3 + k % 32, &st) && errno != EBADF)
      note("meddler: fstat errno %ld", errno, 0);
    if (k % 50 == 0) {
      DIR *d = opendir("/proc/self/fd");
      if (d) {
        while (readdir(d))
          ;
        closedir(d);
      }
    }
    if (k % 200 == 0) {
      pid_t c = fork();
      if (c == 0)
        _exit(0);
      if (c > 0)
        waitpid(c, NULL, 0);
    }
  }
  close(src);
  return NULL;
}

static volatile int racing_fd = -1;

static void *racer(void *a) {
  (void)a;
  char buf[16];
  while (!stop) {
    int fd = racing_fd;
    if (fd < 0)
      continue;
    ssize_t n = pread(fd, buf, sizeof buf, 0);
    if (n < 0 && errno != EBADF)
      note("racing: pread errno %ld", errno, 0);
    // 開いていれば、いつも f05 か f06 の中身 (閉じては同じ番号で開きなおす)
    if (n == 3 && !(buf[0] == '0' && (buf[1] == '5' || buf[1] == '6')))
      note("racing: read another file (%ld)", buf[1], 0);
  }
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  snprintf(dir, sizeof dir, "%s/closeshared.%d", getenv("HOME") ? getenv("HOME") : "/tmp", getpid());
  if (mkdir(dir, 0755)) {
    perror("mkdir");
    return 1;
  }
  char p[300], buf[16];
  for (int i = 0; i < NFILES; i++) {
    path(p, sizeof p, i);
    FILE *f = fopen(p, "w");
    fprintf(f, "%02d\n", i);
    fclose(f);
  }
  // スレッドのあるプロセスにする (fd の表を分ける)
  pthread_t idler;
  pthread_create(&idler, NULL, idle, NULL);
  long c0 = fast_count();

  // semantics
  {
    int ok = 1;
    path(p, sizeof p, 1);
    int a = open(p, O_RDONLY);
    int b = open(p, O_RDONLY);
    char link[64], target[300];
    snprintf(link, sizeof link, "/proc/self/fd/%d", a);
    ok &= readlink(link, target, sizeof target) > 0;
    ok &= close(a) == 0;
    ok &= read(a, buf, 1) == -1 && errno == EBADF;
    ok &= close(a) == -1 && errno == EBADF;
    ok &= readlink(link, target, sizeof target) == -1 && errno == ENOENT;
    int c = open(p, O_RDONLY);
    ok &= c == a;
    close(b);
    close(c);
    check("semantics (EBADF after close, /proc/self/fd, lowest fd reused)", ok);
  }

  // churn
  {
    pthread_t t[4], m;
    bad = 0;
    stop = 0;
    for (long i = 0; i < 4; i++)
      pthread_create(&t[i], NULL, churn, (void *)i);
    pthread_create(&m, NULL, meddler, NULL);
    for (int i = 0; i < 4; i++)
      pthread_join(t[i], NULL);
    stop = 1;
    pthread_join(m, NULL);
    char name[400];
    snprintf(name, sizeof name, "churn (4 threads x %d open-read-close, with dup2, fstat, fork)%s%s", ROUNDS, bad ? ": " : "", bad ? why : "");
    check(name, bad == 0);
  }

  // racing
  {
    pthread_t r[2];
    bad = 0;
    stop = 0;
    for (int i = 0; i < 2; i++)
      pthread_create(&r[i], NULL, racer, NULL);
    for (int k = 0; k < 3000; k++) {
      path(p, sizeof p, 5 + k % 2);
      int fd = open(p, O_RDONLY);
      racing_fd = fd;
      for (int j = 0; j < 5; j++)
        sched_yield();
      close(fd);
    }
    stop = 1;
    for (int i = 0; i < 2; i++)
      pthread_join(r[i], NULL);
    char name[400];
    snprintf(name, sizeof name, "racing (pread on an fd another thread closes and reopens)%s%s", bad ? ": " : "", bad ? why : "");
    check(name, bad == 0);
  }

  // unlink: 8 MiB のファイルを読んで閉じ、消すと、空きがもどる
  {
    snprintf(p, sizeof p, "%s/big", dir);
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    static char chunk[1 << 20];
    memset(chunk, 'x', sizeof chunk);
    for (int i = 0; i < 8; i++)
      if (write(fd, chunk, sizeof chunk) != sizeof chunk)
        perror("write");
    close(fd);
    sync();
    struct statvfs s0, s1, s2;
    statvfs(dir, &s0);
    fd = open(p, O_RDONLY);
    if (read(fd, chunk, 4096) != 4096)
      perror("read");
    close(fd);
    unlink(p);
    sync();
    statvfs(dir, &s1);
    // 次のシステムコールのあと (落とすのを待っていたものも落ちた)
    getppid();
    sync();
    statvfs(dir, &s2);
    long freed = (long)((s2.f_bfree - s0.f_bfree) * s0.f_frsize);
    char name[200];
    snprintf(name, sizeof name, "unlink (8 MiB returns after close and unlink: %ld KiB)", freed / 1024);
    check(name, freed >= 7 * 1024 * 1024);
    (void)s1;
  }

  idle_stop = 1;
  pthread_join(idler, NULL);
  long c1 = fast_count();
  if (c0 >= 0) {
    char name[200];
    snprintf(name, sizeof name, "lockless (%ld closes without the big lock)", c1 - c0);
    check(name, c1 - c0 > 1000);
  }
  snprintf(p, sizeof p, "rm -rf %s", dir);
  if (system(p))
    ;
  printf("closeshared: %d failed\n", fails);
  return fails != 0;
}
