// いくつもの CPU でいっしょに動いたときの速さ (aish の bench kind smp)。aios のカーネルは大きなロック (BKL、smp.rs) 1 つで
// 守っていて、ロックなしの道 (syscall::fast、パイプやファイルの read の速い道、自分の無名のページのフォルト) のほかは、
// カーネルの中を走れるのは 1 度に 1 つの CPU だけ。Linux は細かいロックで、ちがうものをさわるなら CPU の数だけ同時に動く。
// op ごとに、1 つのプロセスで MS ミリ秒まわしたものと、CPU の数 (P) だけのプロセスがいっしょに MS ミリ秒まわしたものの
// 1 回あたりの µs (P のほうは P こ あわせて: 1 / (それぞれの 1 ms あたりの回数の合計)) を、1 行 1 つの JSON で出す:
//   {"bench":"smp","run":0,"op":"stat","procs":1,"n":8000,"ms":200.0,"us":25.00}
//   {"bench":"smp","run":0,"op":"stat-x4","procs":4,"n":30000,"ms":200.0,"us":6.67,"speedup":3.75}
// speedup は 1 つのときの何倍まわせたか (P なら CPU の数だけ伸びた。1 なら 1 つずつしか動けていない)。
// プロセスはそれぞれ自分のディレクトリ、ファイル、パイプ、ソケットを使う (同じものを取りあわない。Linux なら伸びるもの)
//   smpbench [-r RUNS] [-o OP,OP...] [-a] [DIR] [MS]   RUNS 回くりかえす (run は 0 から)。-a は P のほうだけ
//   (/proc/bkl を数えるとき)。DIR (既定 .) の下に smpbench.PID を作って使い、おわったら消す。MS は既定 200
// aios でも Linux でも動く。aish の bench (kind smp) が aios と Linux の VM で同じように動かす
// gcc -O2 -static -o smpbench test/smpbench.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAXP 64

static const char *only;
static char base[4096];
static int run, fails, nproc;

// プロセスのあいだで分けあうもの (MAP_SHARED)。それぞれの数は別のキャッシュラインに
struct slot {
  long n;
  double ms;
  char pad[48];
};
static struct shared {
  int go;
  int stop;
  int ready;
  int fails;
  char pad[48];
  struct slot s[MAXP];
} *sh;

static double now_ms(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static void sleep_ms(double ms) {
  struct timespec t = {(time_t)(ms / 1e3), (long)((ms - (long)(ms / 1e3) * 1e3) * 1e6)};
  while (nanosleep(&t, &t) < 0 && errno == EINTR) {
  }
}

static int want(const char *op) {
  if (!only)
    return 1;
  size_t n = strlen(op);
  for (const char *p = only; (p = strstr(p, op)); p += n)
    if ((p == only || p[-1] == ',') && (p[n] == 0 || p[n] == ','))
      return 1;
  return 0;
}

// ワーカーの中のしくじりも親が数えられるように、分けあうところに数える
static void bad(const char *what) {
  int *f = sh ? &sh->fails : &fails;
  if (__atomic_fetch_add(f, 1, __ATOMIC_RELAXED) < 5)
    fprintf(stderr, "smpbench: %s: %s\n", what, strerror(errno));
}

static long futex(int *a, int op, int v) { return syscall(SYS_futex, a, op, v, NULL, NULL, 0); }

// map-file で写すファイルの大きさ (プログラムがライブラリを写してさわるように)
#define MAPLEN (1 << 20)

// プロセス 1 つが使うもの (はじめに作る)
struct own {
  char dir[4200], file[4300], tmp[4300], map[4300];
  int fd, mfd, pipe[2], sock[2];
  int word;
};

static void setup(struct own *o, int id, const char *op) {
  snprintf(o->dir, sizeof o->dir, "%s/w%d", base, id);
  snprintf(o->file, sizeof o->file, "%s/f", o->dir);
  snprintf(o->tmp, sizeof o->tmp, "%s/t", o->dir);
  snprintf(o->map, sizeof o->map, "%s/m", o->dir);
  if (mkdir(o->dir, 0755) < 0 && errno != EEXIST)
    bad("mkdir");
  static char page[4096];
  memset(page, 'a', sizeof page);
  o->fd = open(o->file, O_CREAT | O_RDWR, 0644);
  if (o->fd < 0 || pwrite(o->fd, page, sizeof page, 0) != sizeof page)
    bad("file");
  // map-file のときだけ (ほかの op の /proc/bkl に書きこみがまざらないように)
  o->mfd = strcmp(op, "map-file") ? -1 : open(o->map, O_CREAT | O_RDWR, 0644);
  for (int off = 0; o->mfd >= 0 && off < MAPLEN; off += sizeof page)
    if (pwrite(o->mfd, page, sizeof page, off) != sizeof page) {
      bad("map file");
      break;
    }
  if (pipe(o->pipe) < 0)
    bad("pipe");
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, o->sock) < 0)
    bad("socketpair");
  o->word = 0;
}

static void teardown(struct own *o) {
  close(o->fd);
  if (o->mfd >= 0)
    close(o->mfd);
  close(o->pipe[0]);
  close(o->pipe[1]);
  close(o->sock[0]);
  close(o->sock[1]);
}

// op の 1 回
static void step(const char *op, struct own *o) {
  static char buf[4096];
  struct stat st;
  char c = 'x';
  if (!strcmp(op, "getppid")) {
    syscall(SYS_getppid);
  } else if (!strcmp(op, "umask")) {
    syscall(SYS_umask, 022);
  } else if (!strcmp(op, "futex-wake")) {
    futex(&o->word, FUTEX_WAKE_PRIVATE, 1);
  } else if (!strcmp(op, "stat")) {
    if (stat(o->file, &st) < 0)
      bad("stat");
  } else if (!strcmp(op, "open-close")) {
    int fd = open(o->file, O_RDONLY);
    if (fd < 0)
      bad("open");
    else
      close(fd);
  } else if (!strcmp(op, "read-4k")) {
    if (pread(o->fd, buf, sizeof buf, 0) != sizeof buf)
      bad("pread");
  } else if (!strcmp(op, "write-4k")) {
    if (pwrite(o->fd, buf, sizeof buf, 0) != sizeof buf)
      bad("pwrite");
  } else if (!strcmp(op, "create-unlink")) {
    int fd = open(o->tmp, O_CREAT | O_EXCL | O_WRONLY, 0644);
    if (fd < 0)
      bad("create");
    else
      close(fd);
    if (unlink(o->tmp) < 0)
      bad("unlink");
  } else if (!strcmp(op, "pipe")) {
    if (write(o->pipe[1], &c, 1) != 1 || read(o->pipe[0], &c, 1) != 1)
      bad("pipe");
  } else if (!strcmp(op, "unix")) {
    if (write(o->sock[0], &c, 1) != 1 || read(o->sock[1], &c, 1) != 1)
      bad("unix");
  } else if (!strcmp(op, "mmap")) {
    char *p = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
      bad("mmap");
      return;
    }
    p[0] = 1;
    p[32768] = 1;
    munmap(p, 65536);
  } else if (!strcmp(op, "map-file")) {
    // 自分のファイル (1 MiB、ページキャッシュにある) を読むだけで写して、ページをみなさわって外す
    volatile char *p = mmap(NULL, MAPLEN, PROT_READ, MAP_PRIVATE, o->mfd, 0);
    if (p == MAP_FAILED) {
      bad("mmap file");
      return;
    }
    for (int off = 0; off < MAPLEN; off += 4096)
      c += p[off];
    munmap((void *)p, MAPLEN);
  } else if (!strcmp(op, "fork")) {
    pid_t pid = fork();
    if (pid == 0)
      _exit(0);
    if (pid < 0 || waitpid(pid, NULL, 0) != pid)
      bad("fork");
  }
}

// op を k こ のプロセスで ms ミリ秒。1 回あたりの µs (k こ あわせて)。回数の合計を *total に
static double measure(const char *op, int k, double ms, long *total) {
  int f = sh->fails;
  memset(sh, 0, sizeof *sh);
  sh->fails = f;
  pid_t pids[MAXP];
  for (int i = 0; i < k; i++) {
    pids[i] = fork();
    if (pids[i] < 0) {
      bad("fork worker");
      k = i;
      break;
    }
    if (pids[i] == 0) {
      struct own o;
      setup(&o, i, op);
      __atomic_add_fetch(&sh->ready, 1, __ATOMIC_SEQ_CST);
      while (!__atomic_load_n(&sh->go, __ATOMIC_ACQUIRE))
        futex(&sh->go, FUTEX_WAIT, 0);
      long n = 0;
      double t0 = now_ms();
      while (!__atomic_load_n(&sh->stop, __ATOMIC_RELAXED)) {
        step(op, &o);
        n++;
      }
      sh->s[i].n = n;
      sh->s[i].ms = now_ms() - t0;
      teardown(&o);
      _exit(0);
    }
  }
  while (__atomic_load_n(&sh->ready, __ATOMIC_SEQ_CST) < k)
    sleep_ms(1);
  __atomic_store_n(&sh->go, 1, __ATOMIC_RELEASE);
  futex(&sh->go, FUTEX_WAKE, MAXP);
  sleep_ms(ms);
  __atomic_store_n(&sh->stop, 1, __ATOMIC_RELEASE);
  for (int i = 0; i < k; i++)
    waitpid(pids[i], NULL, 0);
  double rate = 0;
  *total = 0;
  for (int i = 0; i < k; i++) {
    *total += sh->s[i].n;
    if (sh->s[i].ms > 0)
      rate += sh->s[i].n / sh->s[i].ms;
  }
  return rate > 0 ? 1e3 / rate : 0;
}

static const char *OPS[] = {"getppid", "umask", "futex-wake", "stat", "open-close", "read-4k", "write-4k", "create-unlink", "pipe", "unix", "mmap", "map-file", "fork"};

int main(int argc, char **argv) {
  int runs = 1, all_only = 0, i = 1;
  for (; i < argc && argv[i][0] == '-'; i++) {
    if (!strcmp(argv[i], "-r") && i + 1 < argc)
      runs = atoi(argv[++i]);
    else if (!strcmp(argv[i], "-o") && i + 1 < argc)
      only = argv[++i];
    else if (!strcmp(argv[i], "-a"))
      all_only = 1;
    else {
      fprintf(stderr, "usage: smpbench [-r RUNS] [-o OP,OP...] [-a] [DIR] [MS]\n");
      return 2;
    }
  }
  const char *dir = i < argc ? argv[i++] : ".";
  double ms = i < argc ? atof(argv[i]) : 200;
  if (ms < 10)
    ms = 10;
  nproc = (int)sysconf(_SC_NPROCESSORS_ONLN);
  if (nproc < 1)
    nproc = 1;
  if (nproc > MAXP)
    nproc = MAXP;
  sh = mmap(NULL, sizeof *sh, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (sh == MAP_FAILED) {
    bad("mmap shared");
    return 1;
  }
  snprintf(base, sizeof base, "%s/smpbench.%d", dir, (int)getpid());
  if (mkdir(base, 0755) < 0) {
    bad("mkdir base");
    return 1;
  }
  for (run = 0; run < runs; run++) {
    for (size_t j = 0; j < sizeof OPS / sizeof *OPS; j++) {
      const char *op = OPS[j];
      if (!want(op))
        continue;
      long n1 = 0, np = 0;
      double one = all_only ? 0 : measure(op, 1, ms, &n1);
      if (one > 0)
        printf("{\"bench\":\"smp\",\"run\":%d,\"op\":\"%s\",\"procs\":1,\"n\":%ld,\"ms\":%.1f,\"us\":%.3f}\n", run, op, n1, ms, one);
      if (nproc > 1) {
        double par = measure(op, nproc, ms, &np);
        printf("{\"bench\":\"smp\",\"run\":%d,\"op\":\"%s-x%d\",\"procs\":%d,\"n\":%ld,\"ms\":%.1f,\"us\":%.3f", run, op, nproc, nproc, np, ms, par);
        if (one > 0 && par > 0)
          printf(",\"speedup\":%.2f", one / par);
        printf("}\n");
      }
      fflush(stdout);
    }
  }
  // 片づけ
  for (int w = 0; w < nproc; w++) {
    char p[4400];
    snprintf(p, sizeof p, "%s/w%d/f", base, w);
    unlink(p);
    snprintf(p, sizeof p, "%s/w%d/t", base, w);
    unlink(p);
    snprintf(p, sizeof p, "%s/w%d/m", base, w);
    unlink(p);
    snprintf(p, sizeof p, "%s/w%d", base, w);
    rmdir(p);
  }
  rmdir(base);
  return fails || sh->fails ? 1 : 0;
}
