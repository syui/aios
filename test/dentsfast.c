// 大きなロックなしの getdents64 (file::fast_getdents) のたしかめ。ふつうの道 (OpenFile::getdents) と同じ答えか:
//   same      400 の名前 (長さ 1〜60) のディレクトリを、バッファの大きさを変えて読み、項目 (inode、d_off、種類、名前) の
//             並びが、IN_ACCESS の見張りをつけたとき (見張りがあると、ふつうの道だけで読む) とみな同じ
//   seek      とちゅうの d_off へ lseek して読みついだものが、はじめから読んだものの残りと同じ (どちらの道でも)
//   eof       読み終えたあとの getdents64 は 0、lseek(0) で読みなおせる
//   small     名前より小さいバッファは EINVAL (ふつうの道と同じ)
//   notdir    ふつうのファイルの fd は ENOTDIR、O_PATH は EBADF
//   unlink    読むとちゅうで消した名前は、まだ読んでいない位置なら出ない (どちらの道でも同じ並び)
//   threads   4 つのスレッドがそれぞれ開いたディレクトリを読みつづけるあいだ、ほかのスレッドが名前を作って消す: 作り
//             消ししない名前はどの読みにもみなある
//   big       3000 の名前のディレクトリを 2 KiB ずつ (musl の readdir) 読んで、見張りをつけたときと同じ。それから名前を
//             消す・作る・名前を変えるたびに、読みなおしたものが見張りをつけたときと同じ (覚えた並びが古くならない)
// aios なら、/proc/bkl のロックなしの getdents64 の数が増えること
// 1 行に ok NAME か FAIL NAME、おしまいに dentsfast: N failed
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#define N 400
#define BIG 3000
#define MAXE 4096

struct ent {
  uint64_t ino;
  int64_t off;
  unsigned char type;
  char name[256];
};

static int fails;

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

// fd を bufsz ずつ読んで out に (数を返す。しくじれば -errno)
static int readall(int fd, size_t bufsz, struct ent *out, int max) {
  char buf[65536];
  int n = 0;
  for (;;) {
    long r = syscall(SYS_getdents64, fd, buf, bufsz);
    if (r < 0)
      return -errno;
    if (r == 0)
      return n;
    for (long o = 0; o < r;) {
      struct dirent64 *d = (struct dirent64 *)(buf + o);
      if (n < max) {
        out[n].ino = d->d_ino;
        out[n].off = d->d_off;
        out[n].type = d->d_type;
        snprintf(out[n].name, sizeof out[n].name, "%s", d->d_name);
        n++;
      }
      o += d->d_reclen;
    }
  }
}

static int same(const struct ent *a, int na, const struct ent *b, int nb) {
  if (na != nb)
    return 0;
  for (int i = 0; i < na; i++)
    if (a[i].ino != b[i].ino || a[i].off != b[i].off || a[i].type != b[i].type || strcmp(a[i].name, b[i].name))
      return 0;
  return 1;
}

// /proc/bkl の「ディレクトリのロックなしの getdents64: N」(なければ -1)
static long fast_count(void) {
  FILE *f = fopen("/proc/bkl", "r");
  if (!f)
    return -1;
  char line[512];
  long n = -1;
  const char *key = "ディレクトリのロックなしの getdents64: ";
  while (fgets(line, sizeof line, f))
    if (!strncmp(line, key, strlen(key))) {
      n = atol(line + strlen(key));
      break;
    }
  fclose(f);
  return n;
}

static char dir[256];
static struct ent fast[MAXE], slow[MAXE], tmp[MAXE];

static int opendirfd(void) {
  return open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
}

static volatile int stop;
static int thread_bad;

static void *reader(void *a) {
  (void)a;
  struct ent *got = malloc(sizeof(struct ent) * MAXE);
  while (!stop) {
    int fd = opendirfd();
    int n = readall(fd, 4096, got, MAXE);
    close(fd);
    // f0..f399 (作り消ししない名前) がみなあるか
    int seen = 0;
    for (int i = 0; i < n; i++)
      if (got[i].name[0] == 'f' && got[i].name[1] != 0)
        seen++;
    if (n < 0 || seen != N)
      __atomic_fetch_add(&thread_bad, 1, __ATOMIC_RELAXED);
  }
  free(got);
  return NULL;
}

static void *churner(void *a) {
  (void)a;
  char p[300];
  for (int i = 0; !stop; i++) {
    snprintf(p, sizeof p, "%s/tmp%d", dir, i % 50);
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    if (fd >= 0)
      close(fd);
    unlink(p);
  }
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  snprintf(dir, sizeof dir, "%s/dentsfast.%d", getenv("HOME") ? getenv("HOME") : "/tmp", getpid());
  if (mkdir(dir, 0755)) {
    perror("mkdir");
    return 1;
  }
  char p[512], name[64];
  for (int i = 0; i < N; i++) {
    int len = 1 + i % 60;
    snprintf(name, sizeof name, "f%d", i);
    while ((int)strlen(name) < len)
      strcat(name, "x");
    snprintf(p, sizeof p, "%s/%s", dir, name);
    if (i % 7 == 0) {
      if (mkdir(p, 0755))
        perror("mkdir");
    } else if (i % 11 == 0) {
      if (symlink("target", p))
        perror("symlink");
    } else {
      int fd = open(p, O_CREAT | O_WRONLY, 0644);
      if (fd < 0)
        perror("open");
      close(fd);
    }
  }
  long c0 = fast_count();

  // same: バッファの大きさごとに、見張りなし (速い道) と見張りあり (ふつうの道)
  size_t sizes[] = {80, 100, 280, 1024, 4096, 32768};
  int all_same = 1, nfast = 0;
  for (unsigned k = 0; k < sizeof sizes / sizeof sizes[0]; k++) {
    int fd = opendirfd();
    nfast = readall(fd, sizes[k], fast, MAXE);
    close(fd);
    int in = inotify_init1(IN_CLOEXEC);
    inotify_add_watch(in, dir, IN_ACCESS);
    fd = opendirfd();
    int nslow = readall(fd, sizes[k], slow, MAXE);
    close(fd);
    close(in);
    if (nfast != N + 2 || !same(fast, nfast, slow, nslow)) {
      printf("  size %zu: fast %d slow %d\n", sizes[k], nfast, nslow);
      all_same = 0;
    }
  }
  check("same (6 buffer sizes, fast and watched)", all_same);

  // seek: とちゅうの d_off から
  int ok = 1;
  for (int half = 1; half < nfast - 1; half += 97) {
    for (int watched = 0; watched < 2; watched++) {
      int in = watched ? inotify_init1(IN_CLOEXEC) : -1;
      if (watched)
        inotify_add_watch(in, dir, IN_ACCESS);
      int fd = opendirfd();
      lseek(fd, fast[half - 1].off, SEEK_SET);
      int n = readall(fd, 1024, tmp, MAXE);
      close(fd);
      if (watched)
        close(in);
      if (!same(tmp, n, fast + half, nfast - half))
        ok = 0;
    }
  }
  check("seek (lseek to a d_off and read on)", ok);

  // eof
  {
    char buf[4096];
    int fd = opendirfd();
    readall(fd, 4096, tmp, MAXE);
    long r1 = syscall(SYS_getdents64, fd, buf, sizeof buf);
    lseek(fd, 0, SEEK_SET);
    int n = readall(fd, 4096, tmp, MAXE);
    close(fd);
    check("eof (0 after the end, lseek 0 reads again)", r1 == 0 && same(tmp, n, fast, nfast));
  }

  // small
  {
    char buf[16];
    int fd = opendirfd();
    long r = syscall(SYS_getdents64, fd, buf, sizeof buf);
    int e = errno;
    close(fd);
    check("small (buffer smaller than an entry: EINVAL)", r == -1 && e == EINVAL);
  }

  // notdir
  {
    char buf[4096];
    snprintf(p, sizeof p, "%s/f1", dir);
    int fd = open(p, O_RDONLY);
    long r = syscall(SYS_getdents64, fd, buf, sizeof buf);
    int e = errno;
    close(fd);
    fd = open(dir, O_PATH | O_DIRECTORY);
    long r2 = syscall(SYS_getdents64, fd, buf, sizeof buf);
    int e2 = errno;
    close(fd);
    check("notdir (ENOTDIR for a file, EBADF for O_PATH)", r == -1 && e == ENOTDIR && r2 == -1 && e2 == EBADF);
  }

  // unlink: 半分読んでから、まだ読んでいない名前を 10 消す。どちらの道でも同じ並び
  {
    struct ent a[MAXE], b[MAXE];
    int na = 0, nb = 0;
    for (int watched = 0; watched < 2; watched++) {
      char buf[2048];
      int in = watched ? inotify_init1(IN_CLOEXEC) : -1;
      if (watched)
        inotify_add_watch(in, dir, IN_ACCESS);
      int fd = opendirfd();
      long r = syscall(SYS_getdents64, fd, buf, sizeof buf);
      // 読んだ数
      int got = 0;
      for (long o = 0; o < r; got++)
        o += ((struct dirent64 *)(buf + o))->d_reclen;
      // まだ読んでいない、ふつうのファイルの名前を 10 消して、作りなおしておく (つぎの回のため)
      char gone[10][256];
      int ng = 0;
      for (int i = got; i < nfast && ng < 10; i++)
        if (fast[i].type == DT_REG)
          snprintf(gone[ng++], 256, "%s", fast[i].name);
      for (int i = 0; i < ng; i++) {
        snprintf(p, sizeof p, "%s/%.255s", dir, gone[i]);
        unlink(p);
      }
      int n = readall(fd, 2048, watched ? b : a, MAXE);
      close(fd);
      if (watched) {
        nb = n;
        close(in);
      } else
        na = n;
      for (int i = 0; i < ng; i++) {
        snprintf(p, sizeof p, "%s/%.255s", dir, gone[i]);
        close(open(p, O_CREAT | O_WRONLY, 0644));
      }
    }
    // 消した名前は出ない: 読んだ数は全部 - 半分 - 10。並びも同じ (inode は作りなおしたので見ない)
    int eq = na == nb;
    for (int i = 0; eq && i < na; i++)
      eq = a[i].off == b[i].off && !strcmp(a[i].name, b[i].name);
    check("unlink (names removed ahead of the reader are not returned)", eq && na > 0 && na < nfast);
  }

  // threads
  {
    pthread_t r[4], c;
    stop = 0;
    for (int i = 0; i < 4; i++)
      pthread_create(&r[i], NULL, reader, NULL);
    pthread_create(&c, NULL, churner, NULL);
    usleep(1500 * 1000);
    stop = 1;
    for (int i = 0; i < 4; i++)
      pthread_join(r[i], NULL);
    pthread_join(c, NULL);
    check("threads (4 readers while names come and go)", thread_bad == 0);
  }

  // big
  {
    static struct ent a[MAXE], b[MAXE];
    char big[300];
    snprintf(big, sizeof big, "%s/big", dir);
    mkdir(big, 0755);
    for (int i = 0; i < BIG; i++) {
      snprintf(p, sizeof p, "%s/n%05d", big, i);
      close(open(p, O_CREAT | O_WRONLY, 0644));
    }
    int ok = 1;
    for (int round = 0; round < 4 && ok; round++) {
      // 2 度読む (2 度目は覚えから)
      for (int again = 0; again < 2; again++) {
        int fd = open(big, O_RDONLY | O_DIRECTORY);
        int na = readall(fd, 2048, a, MAXE);
        close(fd);
        int in = inotify_init1(IN_CLOEXEC);
        inotify_add_watch(in, big, IN_ACCESS);
        fd = open(big, O_RDONLY | O_DIRECTORY);
        int nb = readall(fd, 2048, b, MAXE);
        close(fd);
        close(in);
        if (na < BIG - 100 || !same(a, na, b, nb)) {
          printf("  round %d: fast %d watched %d\n", round, na, nb);
          ok = 0;
        }
      }
      // 変える: 消す、作る、名前を変える (同じディレクトリの中で)
      char q[600];
      for (int i = 0; i < 30; i++) {
        snprintf(p, sizeof p, "%s/n%05d", big, round * 100 + i);
        if (round == 0)
          unlink(p);
        else if (round == 1) {
          snprintf(q, sizeof q, "%s/new%d", big, i);
          close(open(q, O_CREAT | O_WRONLY, 0644));
        } else {
          snprintf(q, sizeof q, "%s/moved%d_%d", big, round, i);
          rename(p, q);
        }
      }
    }
    check("big (3000 names in 2 KiB reads, after unlink, create and rename)", ok);
  }

  long c1 = fast_count();
  if (c0 >= 0) {
    snprintf(name, sizeof name, "lockless (%ld getdents64 without the big lock)", c1 - c0);
    check(name, c1 - c0 > 100);
  }

  // 片づけ
  char cmd[600];
  snprintf(cmd, sizeof cmd, "rm -rf %s", dir);
  if (system(cmd))
    ;
  printf("dentsfast: %d failed\n", fails);
  return fails != 0;
}
