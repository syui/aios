// 大きなロックなしの作る・消す (カーネルの fastdir.rs: O_CREAT の openat と unlinkat を、ext4 のロックだけで) があっても、
// Linux と同じ答えになるか: aios の中で動かす (aarch64-linux-gnu-gcc -O2 -static -o createrace test/createrace.c)。
//   basic     作ったものの mode (umask のあと)、持ち主、リンクの数、大きさ。O_EXCL であれば EEXIST、O_CREAT だけなら同じ
//             inode を開いて中身は残る。消したら stat も open も ENOENT、もう一度作れる。作る・消すのあとも、親の statx の
//             mask は前と同じ
//   dirents   作って一部を消したディレクトリの getdents が、残したものだけを 1 度ずつ出す
//   openunlink 開いたまま消したファイルは fd から読めて nlink は 0、閉じるとディスクの空きがもとに戻る
//   link      ハードリンクの片方を消しても、もう片方は読めて nlink が減る
//   perm      書けないディレクトリ (0555) には作れない・消せない (root でなければ EACCES)
//   sgid      setgid のディレクトリに作ったものは、そのディレクトリのグループ
//   bigdir    600 の名前 (ext4 は htree になる) を作って消すと空になる
//   stale     ほかのプロセスが作る・消すをくりかえすあいだ stat しつづけ、終わったあとの stat は最後の形 (ない、か、
//             決まった大きさ) を答える (古い答えを覚えていない)
//   race      4 つのプロセスがそれぞれのディレクトリで、4 つが同じディレクトリでちがう名前を、作って消すのをくりかえす。
//             そのあいだ 1 つが同じディレクトリを読みつづける。終わったら空で、作った数と消した数が合う
//   mixrace   4 つのプロセスが同時に、中身のあるファイルとハードリンクのあるファイルを作って消す。終わったら空で、
//             空いている inode とブロックの数 (statfs) がはじめと同じ
//   inotify   IN_CREATE と IN_DELETE の見張りを置いたディレクトリでは、作った数と消した数だけできごとが来る
// aios なら /proc/bkl の「作った」「消した」の数が増えたか (ロックなしの道を通ったか) も見る。Linux でも同じに通る。
// 1 行に ok NAME か FAIL NAME、おしまいに createrace: N failed
//   createrace [DIR...]   DIR (既定 /tmp) ごとに、その下に cr-PID を作って
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
static char label[64];
static char base[512];
#define CHECK(name, cond) do { if (cond) printf("ok %s %s\n", label, name); else { printf("FAIL %s %s\n", label, name); fails++; } fflush(stdout); } while (0)

static char pbuf[8][1024];
static const char *at(int k, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static const char *at(int k, const char *fmt, ...) {
  char tail[256];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(tail, sizeof tail, fmt, ap);
  va_end(ap);
  snprintf(pbuf[k], sizeof pbuf[k], "%s/%s", base, tail);
  return pbuf[k];
}

static int touch(const char *p) {
  int fd = open(p, O_CREAT | O_EXCL | O_WRONLY, 0644);
  if (fd < 0) return -errno;
  close(fd);
  return 0;
}

// /proc/bkl のロックなしの作る・消すの数 (aios でなければ -1)
static long fast_count(const char *what) {
  FILE *f = fopen("/proc/bkl", "r");
  if (!f) return -1;
  char line[1024];
  long n = -1;
  while (fgets(line, sizeof line, f)) {
    if (!strstr(line, "ロックなしの作る・消す")) continue;
    char key[64];
    snprintf(key, sizeof key, "%s ", what);
    char *p = strstr(line, key);
    if (p) n = atol(p + strlen(key));
  }
  fclose(f);
  return n;
}

static int count_entries(const char *dir, int *bad) {
  DIR *d = opendir(dir);
  if (!d) return -1;
  int n = 0;
  struct dirent *e;
  while ((e = readdir(d))) {
    if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
    n++;
    if (bad && e->d_type != DT_REG && e->d_type != DT_UNKNOWN) (*bad)++;
  }
  closedir(d);
  return n;
}

// statx の mask (STATX_TYPE | STATX_MODE だけ聞いたとき)。Linux の ext4 と tmpfs (multigrain の時刻) は mtime と ctime を
// 入れない。作る・消すが親を覚えさせても (aios の statc) 同じ答えか
static unsigned stx_mask(const char *p) {
  struct statx x;
  return statx(AT_FDCWD, p, 0, STATX_TYPE | STATX_MODE, &x) == 0 ? x.stx_mask : 0;
}

static void basic(void) {
  unsigned m0 = stx_mask(base);
  mode_t old = umask(027);
  const char *p = at(0, "b1");
  int fd = open(p, O_CREAT | O_EXCL | O_RDWR, 0666);
  struct stat st, st2;
  int ok = fd >= 0 && fstat(fd, &st) == 0;
  CHECK("create", ok);
  if (!ok) { umask(old); return; }
  CHECK("mode", (st.st_mode & 07777) == 0640 && S_ISREG(st.st_mode));
  CHECK("owner", st.st_uid == geteuid() && st.st_nlink == 1 && st.st_size == 0);
  CHECK("write", write(fd, "hello", 5) == 5);
  close(fd);
  umask(old);
  CHECK("stat", stat(p, &st2) == 0 && st2.st_ino == st.st_ino && st2.st_size == 5);
  CHECK("excl", open(p, O_CREAT | O_EXCL | O_WRONLY, 0644) < 0 && errno == EEXIST);
  fd = open(p, O_CREAT | O_RDWR, 0644);
  char b[8] = {0};
  CHECK("reopen", fd >= 0 && fstat(fd, &st2) == 0 && st2.st_ino == st.st_ino && read(fd, b, 8) == 5 && !memcmp(b, "hello", 5));
  if (fd >= 0) close(fd);
  CHECK("unlink", unlink(p) == 0);
  CHECK("gone", stat(p, &st2) < 0 && errno == ENOENT && open(p, O_RDONLY) < 0 && errno == ENOENT);
  CHECK("unlink-again", unlink(p) < 0 && errno == ENOENT);
  CHECK("recreate", touch(p) == 0 && stat(p, &st2) == 0 && st2.st_size == 0 && unlink(p) == 0);
  // 名前だけ (いまのディレクトリから) と、dirfd からも
  char cwd[512];
  if (getcwd(cwd, sizeof cwd) && chdir(base) == 0) {
    CHECK("relative", touch("rel") == 0 && access("rel", F_OK) == 0 && unlink("rel") == 0 && access("rel", F_OK) < 0);
    if (chdir(cwd) != 0) perror("chdir");
  }
  int dfd = open(base, O_RDONLY | O_DIRECTORY);
  int f2 = dfd >= 0 ? openat(dfd, "atf", O_CREAT | O_EXCL | O_WRONLY, 0600) : -1;
  CHECK("dirfd", f2 >= 0 && close(f2) == 0 && unlinkat(dfd, "atf", 0) == 0 && faccessat(dfd, "atf", F_OK, 0) < 0);
  if (dfd >= 0) close(dfd);
  // ディレクトリは unlink できない、ないディレクトリの下には作れない
  mkdir(at(1, "sub"), 0755);
  CHECK("eisdir", unlink(at(1, "sub")) < 0 && (errno == EISDIR || errno == EPERM));
  CHECK("enoent-dir", touch(at(2, "nodir/x")) == -ENOENT);
  CHECK("enotdir", touch(at(2, "b2")) == 0 && touch(at(3, "b2/x")) == -ENOTDIR && unlink(at(2, "b2")) == 0);
  rmdir(at(1, "sub"));
  CHECK("statx-mask", m0 != 0 && stx_mask(base) == m0);
}

static void dirents(void) {
  const char *d = at(0, "de");
  mkdir(d, 0755);
  int ok = 1;
  for (int i = 0; i < 40; i++) ok &= touch(at(1, "de/f%02d", i)) == 0;
  for (int i = 0; i < 40; i += 3) ok &= unlink(at(1, "de/f%02d", i)) == 0;
  int seen[40] = {0}, extra = 0;
  DIR *dp = opendir(d);
  struct dirent *e;
  while (dp && (e = readdir(dp))) {
    if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
    int k;
    if (sscanf(e->d_name, "f%d", &k) == 1 && k >= 0 && k < 40) seen[k]++;
    else extra++;
  }
  if (dp) closedir(dp);
  for (int i = 0; i < 40; i++) ok &= seen[i] == (i % 3 ? 1 : 0);
  CHECK("list", ok && extra == 0);
  for (int i = 0; i < 40; i++) if (i % 3) unlink(at(1, "de/f%02d", i));
  CHECK("empty", count_entries(d, NULL) == 0 && rmdir(d) == 0);
}

static void openunlink(void) {
  const char *p = at(0, "ou");
  int fd = open(p, O_CREAT | O_EXCL | O_RDWR, 0644);
  static char big[256 * 1024];
  memset(big, 'z', sizeof big);
  struct statvfs v0, v1, v2;
  int ok = fd >= 0 && statvfs(base, &v0) == 0 && write(fd, big, sizeof big) == (ssize_t)sizeof big && fsync(fd) == 0;
  struct stat st;
  ok = ok && unlink(p) == 0 && fstat(fd, &st) == 0 && st.st_nlink == 0;
  char b[4];
  CHECK("read-after", ok && pread(fd, b, 4, 1000) == 4 && b[0] == 'z' && access(p, F_OK) < 0);
  statvfs(base, &v1);
  close(fd);
  sync();
  statvfs(base, &v2);
  // tmpfs も ext4 も、閉じたら空きが戻る (書いていたあいだより増える)
  CHECK("space", v2.f_bfree >= v1.f_bfree && v2.f_bfree + 8 >= v0.f_bfree);
}

static void link_case(void) {
  const char *a = at(0, "la"), *b = at(1, "lb");
  struct stat st;
  int ok = touch(a) == 0 && link(a, b) == 0 && stat(a, &st) == 0 && st.st_nlink == 2;
  ok = ok && unlink(a) == 0 && stat(b, &st) == 0 && st.st_nlink == 1 && access(a, F_OK) < 0;
  CHECK("hardlink", ok && unlink(b) == 0 && access(b, F_OK) < 0);
}

static void perm(void) {
  const char *d = at(0, "ro");
  mkdir(d, 0755);
  int ok = touch(at(1, "ro/keep")) == 0 && chmod(d, 0555) == 0;
  if (geteuid() != 0) {
    ok = ok && touch(at(2, "ro/new")) == -EACCES && unlink(at(1, "ro/keep")) < 0 && errno == EACCES && access(at(1, "ro/keep"), F_OK) == 0;
  } else {
    ok = ok && touch(at(2, "ro/new")) == 0 && unlink(at(2, "ro/new")) == 0;
  }
  CHECK("readonly-dir", ok);
  chmod(d, 0755);
  CHECK("writable-again", touch(at(2, "ro/new")) == 0 && unlink(at(2, "ro/new")) == 0 && unlink(at(1, "ro/keep")) == 0 && rmdir(d) == 0);
}

static void sgid(void) {
  const char *d = at(0, "sg");
  struct stat ds, fs;
  int ok = mkdir(d, 0755) == 0 && chmod(d, 02775) == 0 && stat(d, &ds) == 0 && touch(at(1, "sg/f")) == 0 && stat(at(1, "sg/f"), &fs) == 0;
  CHECK("gid", ok && fs.st_gid == ds.st_gid && S_ISREG(fs.st_mode));
  unlink(at(1, "sg/f"));
  rmdir(d);
}

static void bigdir(void) {
  const char *d = at(0, "big");
  mkdir(d, 0755);
  int ok = 1;
  for (int i = 0; i < 600; i++) ok &= touch(at(1, "big/a_rather_long_name_for_entry_%04d", i)) == 0;
  CHECK("create600", ok && count_entries(d, NULL) == 600);
  ok = 1;
  for (int i = 0; i < 600; i++) ok &= unlink(at(1, "big/a_rather_long_name_for_entry_%04d", i)) == 0;
  CHECK("unlink600", ok && count_entries(d, NULL) == 0 && rmdir(d) == 0);
}

static void stale(void) {
  const char *p = at(0, "st");
  pid_t c = fork();
  if (c == 0) {
    for (int i = 0; i < 3000; i++) {
      if (touch(p) == 0) unlink(p);
    }
    // おわりは 7 バイトのファイル
    int fd = open(p, O_CREAT | O_EXCL | O_WRONLY, 0644);
    if (fd >= 0) {
      if (write(fd, "the end", 7) != 7) _exit(2);
      close(fd);
    }
    _exit(0);
  }
  int bad = 0, n = 0;
  int status = 0;
  while (waitpid(c, &status, WNOHANG) == 0) {
    struct stat st;
    int r = stat(p, &st);
    if (r < 0 && errno != ENOENT) bad++;
    if (r == 0 && !S_ISREG(st.st_mode)) bad++;
    n++;
  }
  struct stat st;
  CHECK("during", bad == 0 && WIFEXITED(status) && WEXITSTATUS(status) == 0);
  CHECK("after", stat(p, &st) == 0 && st.st_size == 7);
  unlink(p);
  CHECK("after-unlink", stat(p, &st) < 0 && errno == ENOENT && open(p, O_RDONLY) < 0 && errno == ENOENT);
  (void)n;
}

// 4 つは自分のディレクトリで、4 つは同じディレクトリでちがう名前を。1 つは同じディレクトリを読みつづける
static void race(void) {
  enum { W = 4, N = 1500 };
  const char *shared = at(0, "shared");
  mkdir(shared, 0755);
  for (int i = 0; i < W; i++) mkdir(at(1, "own%d", i), 0755);
  int *res = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  memset(res, 0, 4096);
  pid_t pids[2 * W + 1];
  for (int w = 0; w < 2 * W; w++) {
    pids[w] = fork();
    if (pids[w] == 0) {
      int made = 0, gone = 0;
      for (int i = 0; i < N; i++) {
        const char *p = w < W ? at(2, "own%d/t", w) : at(2, "shared/w%d-%d", w, i % 7);
        int fd = open(p, O_CREAT | O_EXCL | O_WRONLY, 0644);
        if (fd >= 0) {
          made++;
          close(fd);
        }
        if (unlink(p) == 0) gone++;
      }
      res[w * 2] = made;
      res[w * 2 + 1] = gone;
      _exit(0);
    }
  }
  pids[2 * W] = fork();
  if (pids[2 * W] == 0) {
    int bad = 0;
    for (int i = 0; i < 300; i++) {
      int b = 0;
      if (count_entries(shared, &b) < 0) bad++;
      bad += b;
    }
    res[100] = bad;
    _exit(0);
  }
  int ok = 1;
  for (int w = 0; w <= 2 * W; w++) {
    int st;
    ok &= waitpid(pids[w], &st, 0) == pids[w] && WIFEXITED(st) && WEXITSTATUS(st) == 0;
  }
  int sum_ok = 1;
  for (int w = 0; w < 2 * W; w++) sum_ok &= res[w * 2] == N && res[w * 2 + 1] == N;
  CHECK("workers", ok && sum_ok);
  CHECK("reader", res[100] == 0);
  int e = count_entries(shared, NULL);
  for (int i = 0; i < W; i++) e += count_entries(at(1, "own%d", i), NULL);
  CHECK("empty", e == 0);
  for (int i = 0; i < W; i++) rmdir(at(1, "own%d", i));
  rmdir(shared);
  munmap(res, 4096);
}

// 4 つのプロセスが同時に、自分のディレクトリと同じディレクトリで、中身のあるファイル (消すとブロックを返す) と、
// ハードリンクのあるファイル (リンクが 2 つ) を作って消す。終わったら空で、空いている inode とブロックの数 (statfs) が
// はじめと同じ (取る・返すの数を取りあうと、ここがずれる)
static void mixrace(void) {
  enum { W = 4, N = 400 };
  mkdir(at(0, "mix"), 0755);
  for (int i = 0; i < W; i++) mkdir(at(1, "mix%d", i), 0755);
  struct statfs s0, s1;
  int have0 = statfs(base, &s0) == 0;
  int *res = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  memset(res, 0, 4096);
  pid_t pids[W];
  for (int w = 0; w < W; w++) {
    pids[w] = fork();
    if (pids[w] == 0) {
      static char buf[9000];
      memset(buf, 'a' + w, sizeof buf);
      int bad = 0;
      for (int i = 0; i < N; i++) {
        const char *own = at(2, "mix%d/f", w), *lnk = at(3, "mix/l%d-%d", w, i % 5);
        int fd = open(own, O_CREAT | O_EXCL | O_WRONLY, 0644);
        if (fd < 0) {
          bad++;
          continue;
        }
        ssize_t size = (i % 3) * 4500;
        if (size && write(fd, buf, size) != size) bad++;
        close(fd);
        int linked = i % 2;
        if (linked && link(own, lnk) != 0) bad++;
        if (unlink(own) != 0) bad++;
        if (linked) {
          struct stat st;
          if (stat(lnk, &st) != 0 || st.st_size != size || st.st_nlink != 1) bad++;
          if (unlink(lnk) != 0) bad++;
        }
      }
      res[w] = bad;
      _exit(0);
    }
  }
  int ok = 1;
  for (int w = 0; w < W; w++) {
    int st;
    ok &= waitpid(pids[w], &st, 0) == pids[w] && WIFEXITED(st) && WEXITSTATUS(st) == 0 && res[w] == 0;
  }
  CHECK("mix-workers", ok);
  int e = count_entries(at(0, "mix"), NULL);
  for (int i = 0; i < W; i++) e += count_entries(at(1, "mix%d", i), NULL);
  CHECK("mix-empty", e == 0);
  CHECK("mix-counts", have0 && statfs(base, &s1) == 0 && s1.f_ffree == s0.f_ffree && s1.f_bfree == s0.f_bfree);
  for (int i = 0; i < W; i++) rmdir(at(1, "mix%d", i));
  rmdir(at(0, "mix"));
  munmap(res, 4096);
}

static void inotify_case(void) {
  const char *d = at(0, "ino");
  mkdir(d, 0755);
  int in = inotify_init1(IN_NONBLOCK);
  int wd = in >= 0 ? inotify_add_watch(in, d, IN_CREATE | IN_DELETE) : -1;
  int ok = wd >= 0;
  for (int i = 0; i < 20; i++) ok &= touch(at(1, "ino/f%d", i)) == 0 && unlink(at(1, "ino/f%d", i)) == 0;
  int creates = 0, deletes = 0;
  char buf[8192] __attribute__((aligned(8)));
  for (;;) {
    ssize_t n = read(in, buf, sizeof buf);
    if (n <= 0) break;
    for (char *q = buf; q < buf + n;) {
      struct inotify_event *ev = (struct inotify_event *)q;
      if (ev->mask & IN_CREATE) creates++;
      if (ev->mask & IN_DELETE) deletes++;
      q += sizeof *ev + ev->len;
    }
  }
  CHECK("events", ok && creates == 20 && deletes == 20);
  if (in >= 0) close(in);
  rmdir(d);
}

int main(int argc, char **argv) {
  const char *dirs_def[] = {"/tmp"};
  const char **dirs = argc > 1 ? (const char **)argv + 1 : dirs_def;
  int nd = argc > 1 ? argc - 1 : 1;
  for (int k = 0; k < nd; k++) {
    snprintf(base, sizeof base, "%s/cr-%d", dirs[k], (int)getpid());
    if (mkdir(base, 0755) < 0) {
      perror(base);
      return 1;
    }
    struct statfs sf;
    int ext4 = statfs(base, &sf) == 0 && sf.f_type == 0xef53;
    snprintf(label, sizeof label, "%s", ext4 ? "ext4" : "tmp");
    long c0 = fast_count("作った"), u0 = fast_count("消した");
    basic();
    dirents();
    openunlink();
    link_case();
    perm();
    sgid();
    bigdir();
    stale();
    race();
    mixrace();
    long c1 = fast_count("作った"), u1 = fast_count("消した");
    // aios の ext4 なら、ロックなしの道を通ったはず (inotify の見張りを置くと、それからはふつうの道)
    if (ext4 && c0 >= 0) CHECK("fast-path", c1 > c0 && u1 > u0);
    inotify_case();
    rmdir(base);
  }
  printf("createrace: %d failed\n", fails);
  return fails != 0;
}
