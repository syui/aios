// 閉じたファイルの棚 (カーネルの file::Shelf: 読むだけで開いた ext4 のファイルを閉じるとき OpenFile をしまい、同じパスを
// また開くとき大きなロックなしで出す) があっても、Linux と同じ答えになるか: aios の中で動かす
// (zig cc -target aarch64-linux-musl -O2 -static -o opencache test/opencache.c)。
//   reopen    開きなおすと位置は 0 から、fd はいちばん小さい空いたもの、O_CLOEXEC はそのときのもの
//   unlink    棚にあるファイルを消すと、ディスクの空きがもとに戻る。開いたままの fd からは前の中身が読める
//   rename    rename で上書きしたら、開きなおすと新しい中身。開いていた fd は前の中身
//   chmod     chmod 000 にすると (root でなければ) EACCES、もどせば開ける
//   inotify   IN_OPEN と IN_CLOSE_NOWRITE が、開いて閉じた回数だけ来る
//   flock     flock したものを閉じるとロックが外れる (ほかの開いたもので取れる)
//   posix     fcntl のロックは、同じファイルのほかの fd を閉じても外れる (子が取れる)
//   dup       dup したものは片方を閉じても、もう片方で読める
//   fork      子も同じパスを開ける
//   missing   ないパスは ENOENT、作ったら開ける
//   procfd    閉じた fd は /proc/self/fd に出ない
//   race      4 つのプロセスが開いて読んで閉じるあいだ、ほかの人が rename で中身を入れかえつづける: 読んだ版は前より
//             古くならない
// aios なら /proc/bkl の棚から開いた数が増えたか (棚を通ったか) も見る。Linux でも同じに通る。
// 1 行に ok NAME か FAIL NAME、おしまいに opencache: N failed
//   opencache [DIR...]   DIR (既定 /tmp) ごとに、その下に oc-PID-* を作って
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
static char label[64];
// いまの DIR が ext4 か、IN_OPEN の見張りをもう置いたか
static int ext4, watched;
#define CHECK(name, cond) do { if (cond) printf("ok %s %s\n", label, name); else { printf("FAIL %s %s\n", label, name); fails++; } } while (0)

static char pbuf[4][512];
static const char *name(int k, const char *dir, const char *what) {
  snprintf(pbuf[k], sizeof pbuf[k], "%s/oc-%d-%s", dir, (int)getpid(), what);
  return pbuf[k];
}

static int put(const char *p, const char *s) {
  int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (fd < 0) return -1;
  int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1;
  close(fd);
  return r;
}

// 開いて読んで閉じる (読んだもの、なければ "")
static int slurp(const char *p, char *out, size_t n) {
  int fd = open(p, O_RDONLY);
  if (fd < 0) return -errno;
  ssize_t r = read(fd, out, n - 1);
  close(fd);
  out[r > 0 ? r : 0] = 0;
  return r < 0 ? -1 : 0;
}

// /proc/bkl の棚から開いた数 (aios でなければ -1)
static long shelf_hits(void) {
  FILE *f = fopen("/proc/bkl", "r");
  if (!f) return -1;
  char line[512];
  long n = -1;
  while (fgets(line, sizeof line, f)) {
    char *p = strstr(line, "棚から開いた ");
    if (p) n = atol(p + strlen("棚から開いた "));
  }
  fclose(f);
  return n;
}

static void reopen(const char *dir) {
  const char *p = name(0, dir, "reopen");
  int ok = put(p, "0123456789abcdef") == 0;
  long h0 = shelf_hits();
  for (int k = 0; k < 200 && ok; k++) {
    int fd = open(p, O_RDONLY | (k % 2 ? O_CLOEXEC : 0));
    char b[8] = {0};
    // 位置はいつも 0 から
    if (fd < 0 || read(fd, b, 4) != 4 || memcmp(b, "0123", 4) != 0) ok = 0;
    if (ok && read(fd, b, 4) != 4) ok = 0;
    if (ok && memcmp(b, "4567", 4) != 0) ok = 0;
    int fl = fcntl(fd, F_GETFD);
    if (fl < 0 || !!(fl & FD_CLOEXEC) != (k % 2)) ok = 0;
    close(fd);
  }
  CHECK("reopen", ok);
  // fd はいちばん小さい空いたもの
  int a = open(p, O_RDONLY);
  int b = open("/dev/null", O_RDONLY);
  close(a);
  int c = open(p, O_RDONLY);
  int d = open(p, O_RDONLY);
  CHECK("reopen-lowest-fd", a >= 0 && c == a && d > b);
  close(b);
  close(c);
  close(d);
  // aios の ext4 なら (IN_OPEN の見張りを置く前は) 棚を通る
  long h1 = shelf_hits();
  if (h0 >= 0 && ext4 && !watched) {
    if (h1 - h0 < 100) printf("  shelf: %ld of 200 opens\n", h1 - h0);
    CHECK("reopen-shelf", h1 - h0 >= 100);
  }
  unlink(p);
}

static void unlinked(const char *dir) {
  const char *p = name(0, dir, "unlink");
  struct statvfs s0, s1, s2;
  sync();
  statvfs(dir, &s0);
  int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  static char big[1 << 20];
  memset(big, 'u', sizeof big);
  int ok = fd >= 0;
  for (int i = 0; i < 8 && ok; i++)
    if (write(fd, big, sizeof big) != sizeof big) ok = 0;
  close(fd);
  // 何度も開いて閉じる (棚に入る)
  for (int k = 0; k < 20 && ok; k++) {
    int r = open(p, O_RDONLY);
    if (r < 0) ok = 0;
    close(r);
  }
  // 開いたままのものは、消したあとも前の中身
  int keep = open(p, O_RDONLY);
  sync();
  statvfs(dir, &s1);
  ok = ok && unlink(p) == 0;
  char b[4];
  ok = ok && read(keep, b, 4) == 4 && memcmp(b, "uuuu", 4) == 0;
  close(keep);
  int again = open(p, O_RDONLY);
  ok = ok && again < 0 && errno == ENOENT;
  sync();
  statvfs(dir, &s2);
  // 8 MiB 使ったものが、消したら (ほぼ) もとに戻る
  long used = (long)(s0.f_bfree - s1.f_bfree) * (long)s0.f_frsize;
  long back = (long)(s2.f_bfree - s1.f_bfree) * (long)s0.f_frsize;
  // 空きを数えないファイルシステム (使っても f_bfree が変わらない) なら、中身と ENOENT だけ
  int counted = used > 0;
  if (ok && counted && !(used >= 8 << 20 && back >= used - (64 << 10)))
    printf("  unlink: used %ld bytes, %ld came back\n", used, back);
  CHECK("unlink", ok && (!counted || (used >= 8 << 20 && back >= used - (64 << 10))));
}

static void renamed(const char *dir) {
  const char *a = name(0, dir, "ren-a"), *b = name(1, dir, "ren-b");
  int ok = put(a, "old") == 0 && put(b, "new") == 0;
  char got[16];
  for (int k = 0; k < 10 && ok; k++)
    if (slurp(a, got, sizeof got) != 0 || strcmp(got, "old") != 0) ok = 0;
  int keep = open(a, O_RDONLY);
  ok = ok && rename(b, a) == 0;
  ok = ok && slurp(a, got, sizeof got) == 0 && strcmp(got, "new") == 0;
  char k3[4] = {0};
  ok = ok && read(keep, k3, 3) == 3 && strcmp(k3, "old") == 0;
  close(keep);
  CHECK("rename", ok);
  unlink(a);
}

static void chmodded(const char *dir) {
  if (geteuid() == 0) {
    printf("ok %s chmod (root: skipped)\n", label);
    return;
  }
  const char *p = name(0, dir, "chmod");
  int ok = put(p, "x") == 0;
  char got[4];
  for (int k = 0; k < 5 && ok; k++)
    if (slurp(p, got, sizeof got) != 0) ok = 0;
  ok = ok && chmod(p, 0) == 0;
  int fd = open(p, O_RDONLY);
  ok = ok && fd < 0 && errno == EACCES;
  if (fd >= 0) close(fd);
  ok = ok && chmod(p, 0644) == 0 && slurp(p, got, sizeof got) == 0;
  CHECK("chmod", ok);
  unlink(p);
}

static void notified(const char *dir) {
  const char *p = name(0, dir, "notify");
  int ok = put(p, "n") == 0;
  char got[4];
  for (int k = 0; k < 5 && ok; k++)
    if (slurp(p, got, sizeof got) != 0) ok = 0;
  int in = inotify_init1(IN_NONBLOCK);
  ok = ok && in >= 0 && inotify_add_watch(in, p, IN_OPEN | IN_CLOSE_NOWRITE) >= 0;
  watched = 1;
  for (int k = 0; k < 3 && ok; k++)
    if (slurp(p, got, sizeof got) != 0) ok = 0;
  int opens = 0, closes = 0;
  char ev[4096] __attribute__((aligned(8)));
  struct pollfd pf = {in, POLLIN, 0};
  while (poll(&pf, 1, 300) == 1) {
    ssize_t n = read(in, ev, sizeof ev);
    if (n <= 0) break;
    for (char *q = ev; q < ev + n;) {
      struct inotify_event *e = (struct inotify_event *)q;
      if (e->mask & IN_OPEN) opens++;
      if (e->mask & IN_CLOSE_NOWRITE) closes++;
      q += sizeof *e + e->len;
    }
  }
  if (ok && (opens != 3 || closes != 3)) printf("  inotify: %d opens, %d closes (want 3, 3)\n", opens, closes);
  CHECK("inotify", ok && opens == 3 && closes == 3);
  close(in);
  unlink(p);
}

static void flocked(const char *dir) {
  const char *p = name(0, dir, "flock");
  int ok = put(p, "f") == 0;
  char got[4];
  for (int k = 0; k < 5 && ok; k++)
    if (slurp(p, got, sizeof got) != 0) ok = 0;
  int a = open(p, O_RDONLY);
  ok = ok && a >= 0 && flock(a, LOCK_EX) == 0;
  int b = open(p, O_RDONLY);
  ok = ok && b >= 0 && flock(b, LOCK_EX | LOCK_NB) != 0 && errno == EWOULDBLOCK;
  close(a);
  ok = ok && flock(b, LOCK_EX | LOCK_NB) == 0;
  close(b);
  CHECK("flock", ok);
  unlink(p);
}

static void posixlocked(const char *dir) {
  const char *p = name(0, dir, "posix");
  int ok = put(p, "p") == 0;
  int a = open(p, O_RDWR);
  struct flock l = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  ok = ok && a >= 0 && fcntl(a, F_SETLK, &l) == 0;
  // ほかの fd を開いて閉じる: このプロセスのロックはみな外れる (POSIX)
  int b = open(p, O_RDONLY);
  ok = ok && b >= 0;
  close(b);
  pid_t pid = fork();
  if (pid == 0) {
    int c = open(p, O_RDWR);
    struct flock m = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
    _exit(c >= 0 && fcntl(c, F_SETLK, &m) == 0 ? 0 : 1);
  }
  int ws;
  waitpid(pid, &ws, 0);
  CHECK("posix", ok && WIFEXITED(ws) && WEXITSTATUS(ws) == 0);
  close(a);
  unlink(p);
}

static void dupped(const char *dir) {
  const char *p = name(0, dir, "dup");
  int ok = put(p, "dupdup") == 0;
  char got[8];
  for (int k = 0; k < 5 && ok; k++)
    if (slurp(p, got, sizeof got) != 0) ok = 0;
  int a = open(p, O_RDONLY);
  int b = dup(a);
  close(a);
  char x[4] = {0};
  ok = ok && b >= 0 && read(b, x, 3) == 3 && strcmp(x, "dup") == 0;
  close(b);
  ok = ok && slurp(p, got, sizeof got) == 0 && strcmp(got, "dupdup") == 0;
  CHECK("dup", ok);
  unlink(p);
}

static void forked(const char *dir) {
  const char *p = name(0, dir, "fork");
  int ok = put(p, "fk") == 0;
  char got[4];
  for (int k = 0; k < 5 && ok; k++)
    if (slurp(p, got, sizeof got) != 0) ok = 0;
  pid_t pid = fork();
  if (pid == 0) {
    char g[4];
    int r = 0;
    for (int k = 0; k < 20; k++)
      if (slurp(p, g, sizeof g) != 0 || strcmp(g, "fk") != 0) r = 1;
    _exit(r);
  }
  int ws;
  waitpid(pid, &ws, 0);
  ok = ok && slurp(p, got, sizeof got) == 0 && strcmp(got, "fk") == 0;
  CHECK("fork", ok && WIFEXITED(ws) && WEXITSTATUS(ws) == 0);
  unlink(p);
}

static void missing(const char *dir) {
  const char *p = name(0, dir, "missing");
  unlink(p);
  int ok = 1;
  for (int k = 0; k < 50 && ok; k++) {
    int fd = open(p, O_RDONLY);
    if (fd >= 0 || errno != ENOENT) ok = 0;
  }
  ok = ok && put(p, "here") == 0;
  char got[8];
  ok = ok && slurp(p, got, sizeof got) == 0 && strcmp(got, "here") == 0;
  unlink(p);
  int fd = open(p, O_RDONLY);
  ok = ok && fd < 0 && errno == ENOENT;
  CHECK("missing", ok);
}

static void procfd(const char *dir) {
  const char *p = name(0, dir, "procfd");
  int ok = put(p, "q") == 0;
  int fd = open(p, O_RDONLY);
  close(fd);
  fd = open(p, O_RDONLY);
  close(fd);
  // 閉じた fd は /proc/self/fd に出ない
  DIR *d = opendir("/proc/self/fd");
  int seen = 0;
  struct dirent *e;
  char want[16];
  snprintf(want, sizeof want, "%d", fd);
  while (d && (e = readdir(d)))
    if (!strcmp(e->d_name, want) && dirfd(d) != fd) seen = 1;
  if (d) closedir(d);
  CHECK("procfd", ok && !seen);
  unlink(p);
}

// ---- race ----
static void race(const char *dir) {
  const char *p = name(0, dir, "race");
  const char *tmp = name(1, dir, "race-tmp");
  int ok = put(p, "gen 0") == 0;
  pid_t r[4];
  for (int i = 0; i < 4; i++) {
    r[i] = fork();
    if (r[i] == 0) {
      alarm(120);
      long last = -1;
      char got[32];
      for (int k = 0; k < 4000; k++) {
        if (slurp(p, got, sizeof got) != 0) continue;  // 入れかえのあいだは無いことはない (rename) が、念のため
        long g;
        if (sscanf(got, "gen %ld", &g) != 1) _exit(2);
        if (g < last) _exit(3);
        last = g;
      }
      _exit(0);
    }
  }
  pid_t w = fork();
  if (w == 0) {
    alarm(120);
    for (int g = 1; g <= 300; g++) {
      char s[32];
      snprintf(s, sizeof s, "gen %d", g);
      if (put(tmp, s) != 0 || rename(tmp, p) != 0) _exit(1);
      usleep(500);
    }
    _exit(0);
  }
  for (int i = 0; i < 4; i++) {
    int ws;
    waitpid(r[i], &ws, 0);
    if (!WIFEXITED(ws) || WEXITSTATUS(ws) != 0) {
      printf("  race: reader %d status %d\n", i, WIFEXITED(ws) ? WEXITSTATUS(ws) : -1);
      ok = 0;
    }
  }
  int ws;
  waitpid(w, &ws, 0);
  ok = ok && WIFEXITED(ws) && WEXITSTATUS(ws) == 0;
  CHECK("race", ok);
  unlink(p);
  unlink(tmp);
}

int main(int argc, char **argv) {
  setvbuf(stdout, 0, _IOLBF, 0);
  int n = argc > 1 ? argc - 1 : 1;
  for (int i = 0; i < n; i++) {
    const char *dir = argc > 1 ? argv[i + 1] : "/tmp";
    snprintf(label, sizeof label, "%s", dir);
    struct statfs sf;
    ext4 = statfs(dir, &sf) == 0 && sf.f_type == 0xEF53;
    reopen(dir);
    unlinked(dir);
    renamed(dir);
    chmodded(dir);
    flocked(dir);
    posixlocked(dir);
    dupped(dir);
    forked(dir);
    missing(dir);
    procfd(dir);
    race(dir);
    // inotify はおしまいに (見張りを置くと、それからは棚を使わない)
    notified(dir);
  }
  printf("opencache: %d failed\n", fails);
  return fails != 0;
}
