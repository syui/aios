// AppArmor (kernel/src/apparmor.rs) のたしかめ: test/apparmor/test.profile (gen.sh が apparmor_parser で作った blob.h) を
// /sys/kernel/security/apparmor/.replace で読み、このプログラムの写し /tmp/aatest/t (aatest がパスでつく) に、決まりの
// 中から open、作る・消す、rename、link、chmod、truncate、flock、exec (ix px cx ux) と change_profile をためさせる。
// aatest の hat に change_hat で入る・移る・帰る (token がちがえば SIGKILL)。aacap では capability (chown だけ許す)。それから、決まりのないプロセスから onexec、complain、動いているプロセスの
// プロファイルを外す (unconfined にもどる)。
// root で動かす (でなければ sudo -n でもう一度)。AppArmor のついた Linux でも同じに通ること
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/capability.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

#include "apparmor/blob.h"

#define AA "/sys/kernel/security/apparmor/"
#define D "/tmp/aatest"
static int fails;
#define CHECK(c, ...)                                                                                                  \
  do {                                                                                                                 \
    if (!(c)) {                                                                                                        \
      fails++;                                                                                                         \
      printf("FAIL " __VA_ARGS__);                                                                                     \
      printf(" (line %d)\n", __LINE__);                                                                                \
    }                                                                                                                  \
  } while (0)

static int e(int r) { return r < 0 ? errno : 0; }

static int op_open(const char *p, int fl) {
  int fd = open(p, fl, 0644);
  if (fd < 0)
    return errno;
  close(fd);
  return 0;
}

static int op_flock(const char *p) {
  int fd = open(p, O_RDONLY);
  if (fd < 0)
    return errno;
  int r = e(flock(fd, LOCK_SH));
  close(fd);
  return r;
}

// 書く口を 1 回だけ (.load .replace .remove と attr)。答えは 0 か errno
static int put(const char *path, const void *b, size_t n) {
  int fd = open(path, O_WRONLY);
  if (fd < 0)
    return errno;
  int r = write(fd, b, n) == (ssize_t)n ? 0 : errno;
  close(fd);
  return r;
}

static int puts_(const char *path, const char *s) { return put(path, s, strlen(s)); }

// ファイルの 1 行め (改行なし)。読めなければ "ERR errno"
static void line(const char *path, char *out, size_t n) {
  int fd = open(path, O_RDONLY);
  ssize_t k = fd < 0 ? -1 : read(fd, out, n - 1);
  if (k < 0) {
    snprintf(out, n, "ERR %d", errno);
  } else {
    out[k] = 0;
    out[strcspn(out, "\n")] = 0;
  }
  if (fd >= 0)
    close(fd);
}

// prog を arg で動かし、出力の 1 行め。exec できなければ "ERR errno"
static void run(const char *prog, const char *arg, char *out, size_t n) {
  int p[2];
  if (pipe(p) < 0) {
    snprintf(out, n, "ERR pipe");
    return;
  }
  fflush(stdout);
  pid_t k = fork();
  if (k == 0) {
    dup2(p[1], 1);
    close(p[0]);
    execl(prog, prog, arg, (char *)NULL);
    printf("ERR %d\n", errno);
    fflush(stdout);
    _exit(1);
  }
  close(p[1]);
  ssize_t r = read(p[0], out, n - 1);
  out[r > 0 ? r : 0] = 0;
  out[strcspn(out, "\n")] = 0;
  close(p[0]);
  int st;
  waitpid(k, &st, 0);
  // 何も言わずにシグナルで終わった (移った先で自分を写せない: SIGSEGV)
  if (!out[0] && WIFSIGNALED(st))
    snprintf(out, n, "SIG %d", WTERMSIG(st));
}

// prog を arg で動かし、出力をみな out に
static void collect(const char *prog, const char *arg, char *out, size_t n) {
  int p[2];
  out[0] = 0;
  if (pipe(p) < 0)
    return;
  fflush(stdout);
  pid_t c = fork();
  if (c == 0) {
    dup2(p[1], 1);
    close(p[0]);
    execl(prog, prog, arg, (char *)NULL);
    _exit(127);
  }
  close(p[1]);
  size_t got = 0;
  ssize_t k;
  while ((k = read(p[0], out + got, n - 1 - got)) > 0)
    got += k;
  out[got] = 0;
  close(p[0]);
  waitpid(c, NULL, 0);
}

// capget の effective と permitted の下の 32 ビット
static void caps(unsigned *eff, unsigned *perm) {
  struct __user_cap_header_struct h = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct d[2] = {0};
  syscall(SYS_capget, &h, d);
  *eff = d[0].effective;
  *perm = d[0].permitted;
}

// aacap (argv[1] が cap) か aacomplain (capc) で: 1 行に「名前 答え」。cap2 は uid 1 の 0600
static int capchild(int complain) {
  if (puts_("/proc/self/attr/current", complain ? "changeprofile aacomplain" : "changeprofile aacap"))
    return 2;
  const char *k = complain ? "capc" : "cap";
  unsigned eff, perm;
  caps(&eff, &perm);
  printf("%s-eff %x\n%s-perm %x\n", k, eff, k, perm);
  printf("%s-read-other %d\n", k, op_open(D "/cap2", O_RDONLY));
  printf("%s-write-other %d\n", k, op_open(D "/cap2", O_WRONLY));
  printf("%s-chmod-other %d\n", k, e(chmod(D "/cap2", 0600)));
  printf("%s-chown %d\n", k, e(chown(D "/cap", 1, 1)));
  printf("%s-chown-back %d\n", k, e(chown(D "/cap", 0, 0)));
  printf("%s-mknod %d\n", k, e(mknod(D "/dev0", S_IFCHR | 0600, makedev(1, 3))));
  unlink(D "/dev0");
  printf("%s-setuid %d\n", k, e(setuid(1)));
  fflush(stdout);
  return 0;
}

// /proc/self/attr/current に n バイト書く (changehat は hat を \0 で区切って続けられる)
static int hatw(const char *s, size_t n) { return put("/proc/self/attr/current", s, n); }
#define HAT(s) hatw(s, strlen(s))

// 子を待って、シグナルで終わったら "SIG n"、でなければ "EXIT n"
static void ended(const char *name, pid_t k) {
  int st;
  waitpid(k, &st, 0);
  if (WIFSIGNALED(st))
    printf("%s SIG %d\n", name, WTERMSIG(st));
  else
    printf("%s EXIT %d\n", name, WEXITSTATUS(st));
}

// aatest の中で change_hat (どれも fork した子で。入ったり殺されたりするので)
static void hats(void) {
#define L(name, path)                                                                                                  \
  do {                                                                                                                 \
    char b_[128];                                                                                                      \
    line(path, b_, sizeof b_);                                                                                         \
    printf("%s %s\n", name, b_);                                                                                       \
  } while (0)
  pid_t k;
  fflush(stdout);
  if ((k = fork()) == 0) {
    printf("hat-in %d\n", HAT("changehat 1234^hat1"));
    L("hat-cur", "/proc/self/attr/current");
    L("hat-prev", "/proc/self/attr/prev");
    printf("hat-open-w %d\nhat-open-r %d\n", op_open(D "/w", O_RDONLY), op_open(D "/r", O_RDONLY));
    printf("hat-sibling %d\n", HAT("changehat 1234^hat2"));
    L("hat-cur2", "/proc/self/attr/current");
    printf("hat-out %d\n", HAT("changehat 1234^"));
    L("hat-back", "/proc/self/attr/current");
    L("hat-prev2", "/proc/self/attr/prev");
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
  // 入れないもの、変えないもの
  if ((k = fork()) == 0) {
    printf("hat-none %d\n", HAT("changehat 1234^nohat"));
    printf("hat-nothat %d\n", HAT("changehat 1234^sub"));
    printf("hat-zero %d\n", HAT("changehat 0^"));
    printf("hat-junk %d\n", HAT("changehat xyz^hat1"));
    printf("hat-noprev %d\n", HAT("changehat 55^"));
    printf("hat-perm %d\n", HAT("permhat 1234^hat1"));
    L("hat-perm-cur", "/proc/self/attr/current");
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
  // いくつも (libapparmor の aa_change_hatv と同じ形: 名前ごとに \0、終わりにもう 1 つ)
  if ((k = fork()) == 0) {
    static const char v[] = "changehat 00000000000004d2^nohat\0hat1\0";
    printf("hat-vec %d\n", hatw(v, sizeof v));
    L("hat-vec-cur", "/proc/self/attr/current");
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
  // token がちがえば SIGKILL (帰るときも、兄弟へ移るときも)
  if ((k = fork()) == 0) {
    HAT("changehat 1234^hat1");
    HAT("changehat 99^");
    _exit(0);
  }
  ended("hat-badtoken", k);
  if ((k = fork()) == 0) {
    HAT("changehat 1234^hat1");
    HAT("changehat 99^hat2");
    _exit(0);
  }
  ended("hat-badtoken2", k);
  // hat のないプロファイルから
  if ((k = fork()) == 0) {
    puts_("/proc/self/attr/current", "changeprofile aachild");
    printf("hat-nohats %d\n", HAT("changehat 1234^x"));
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
#undef L
}

// aatest の中で (argv[1] が child): 1 行に「名前 答え」
static int child(void) {
#define T(name, v) printf("%s %d\n", name, v)
  struct stat st;
  T("open-r", op_open(D "/r", O_RDONLY));
  T("open-r-w", op_open(D "/r", O_WRONLY));
  T("open-w", op_open(D "/w", O_WRONLY));
  T("open-w-r", op_open(D "/w", O_RDONLY));
  T("open-rw", op_open(D "/rw", O_RDWR));
  T("open-a", op_open(D "/a", O_WRONLY | O_APPEND));
  T("open-a-w", op_open(D "/a", O_WRONLY));
  T("open-deny", op_open(D "/deny", O_RDONLY));
  T("open-x", op_open(D "/x", O_RDONLY));
  T("open-missing", op_open(D "/missing", O_RDONLY));
  T("stat-x", e(stat(D "/x", &st)));
  T("create-d", op_open(D "/d/f", O_WRONLY | O_CREAT));
  T("create-top", op_open(D "/new", O_WRONLY | O_CREAT));
  T("mkdir-d", e(mkdir(D "/d/sub", 0755)));
  T("rmdir-d", e(rmdir(D "/d/sub")));
  T("rename-in-d", e(rename(D "/d/f", D "/d/g")));
  T("rename-out", e(rename(D "/d/g", D "/out")));
  T("unlink-d", e(unlink(D "/d/g")));
  T("unlink-r", e(unlink(D "/r")));
  T("opendir-d", op_open(D "/d", O_RDONLY | O_DIRECTORY));
  T("opendir-top", op_open(D, O_RDONLY | O_DIRECTORY));
  T("open-own", op_open(D "/own", O_RDONLY));
  T("flock-k", op_flock(D "/k"));
  T("flock-r", op_flock(D "/r"));
  T("link-ok", e(link(D "/r", D "/ln")));
  T("link-bad", e(link(D "/rw", D "/ln2")));
  T("chmod-rw", e(chmod(D "/rw", 0644)));
  T("chmod-r", e(chmod(D "/r", 0644)));
  T("truncate-w", e(truncate(D "/w", 0)));
  T("truncate-r", e(truncate(D "/r", 0)));
  char b[128];
  line("/proc/self/attr/current", b, sizeof b);
  printf("self %s\n", b);
  const char *ex[] = {"ix", "px", "cx", "ux", "nox", "pnomap"};
  for (int i = 0; i < 6; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", ex[i]);
    run(p, "attr", b, sizeof b);
    printf("exec-%s %s\n", ex[i], b);
  }
  hats();
  T("change-complain", puts_("/proc/self/attr/current", "changeprofile aacomplain"));
  T("change-child", puts_("/proc/self/attr/current", "changeprofile aachild"));
  line("/proc/self/attr/current", b, sizeof b);
  printf("changed %s\n", b);
  fflush(stdout);
  return 0;
}

// 答え: 子の出力に want の行がみなあるか
static const char *want[] = {
    "open-r 0", "open-r-w 13", "open-w 0", "open-w-r 13", "open-rw 0", "open-a 0", "open-a-w 13", "open-deny 13",
    "open-x 13", "open-missing 2", "stat-x 0", "create-d 0", "create-top 13", "mkdir-d 0", "rmdir-d 0",
    "rename-in-d 0", "rename-out 13", "unlink-d 0", "unlink-r 13", "opendir-d 0", "opendir-top 13", "open-own 0",
    "flock-k 0", "flock-r 13", "link-ok 0", "link-bad 13", "chmod-rw 0", "chmod-r 13", "truncate-w 0",
    "truncate-r 13", "self aatest (enforce)", "exec-ix aatest (enforce)", "exec-px aachild (enforce)",
    "exec-cx aatest//sub (enforce)", "exec-ux unconfined", "exec-nox ERR 13", "exec-pnomap SIG 11", "change-complain 13", "change-child 0",
    "changed aachild (enforce)", "hat-in 0", "hat-cur aatest//hat1 (enforce)", "hat-prev aatest (enforce)",
    "hat-open-w 0", "hat-open-r 13", "hat-sibling 0", "hat-cur2 aatest//hat2 (enforce)", "hat-out 0",
    "hat-back aatest (enforce)", "hat-prev2 ERR 22", "hat-none 2", "hat-nothat 1", "hat-zero 22", "hat-junk 22",
    "hat-noprev 0", "hat-perm 0", "hat-perm-cur aatest (enforce)", "hat-vec 0", "hat-vec-cur aatest//hat1 (enforce)",
    "hat-badtoken SIG 9", "hat-badtoken2 SIG 9", "hat-nohats 10",
};

// aacap: chown のほかの root の力はない (capget にも chown だけ)。complain なら記録して通す
static const char *want_cap[] = {
    "cap-eff 1", "cap-perm 1", "cap-read-other 13", "cap-write-other 13", "cap-chmod-other 1", "cap-chown 0",
    "cap-chown-back 0", "cap-mknod 1", "cap-setuid 1", "capc-eff ffffffff", "capc-perm ffffffff",
    "capc-read-other 0", "capc-write-other 0", "capc-chmod-other 0", "capc-chown 0", "capc-chown-back 0",
    "capc-mknod 0", "capc-setuid 0",
};

// 作るものと、このプログラムの写し (t と、exec の行き先)。Linux の VM (test/linux-vm.sh の rootexec) には
// シェルも cp もないので、自分で
static const char *files[] = {"r", "w", "rw", "a", "deny", "x", "own", "k", "cap", "cap2"};
static const char *bins[] = {"t", "ix", "px", "cx", "ux", "nox", "pnomap"};
// テストでできるかもしれないもの
static const char *made[] = {"ln", "ln2", "new", "out", "d/f", "d/g", "dev0"};

static int copy(const char *from, const char *to) {
  int in = open(from, O_RDONLY), out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
  char b[65536];
  ssize_t n;
  int ok = in >= 0 && out >= 0;
  while (ok && (n = read(in, b, sizeof b)) > 0)
    ok = write(out, b, n) == n;
  if (in >= 0)
    close(in);
  if (out >= 0)
    close(out);
  return ok;
}

static void setup(const char *self) {
  mkdir(D, 0755);
  mkdir(D "/d", 0755);
  for (size_t i = 0; i < sizeof files / sizeof *files; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", files[i]);
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) {
      CHECK(write(fd, "x\n", 2) == 2, "write %s", p);
      close(fd);
    }
  }
  for (size_t i = 0; i < sizeof bins / sizeof *bins; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", bins[i]);
    CHECK(copy(self, p), "copy %s", bins[i]);
  }
}

static void cleanup(void) {
  puts_(AA ".remove", "aatest");
  puts_(AA ".remove", "aachild");
  puts_(AA ".remove", "aacomplain");
  puts_(AA ".remove", "aanomap");
  puts_(AA ".remove", "aacap");
  const char **lists[] = {files, bins, made};
  size_t ns[] = {sizeof files / sizeof *files, sizeof bins / sizeof *bins, sizeof made / sizeof *made};
  for (int l = 0; l < 3; l++)
    for (size_t i = 0; i < ns[l]; i++) {
      char p[64];
      snprintf(p, sizeof p, D "/%s", lists[l][i]);
      unlink(p);
    }
  rmdir(D "/d/sub");
  rmdir(D "/d");
  rmdir(D);
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "child"))
    return child();
  if (argc > 1 && (!strcmp(argv[1], "cap") || !strcmp(argv[1], "capc")))
    return capchild(argv[1][3] == 'c');
  if (argc > 1 && !strcmp(argv[1], "attr")) {
    char b[128];
    line("/proc/self/attr/current", b, sizeof b);
    printf("%s\n", b);
    return 0;
  }
  char self[256];
  ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
  self[n > 0 ? n : 0] = 0;
  if (geteuid() != 0) {
    execl("/usr/bin/sudo", "sudo", "-n", self, (char *)NULL);
    printf("apparmor: needs root (sudo -n failed)\n");
    return 1;
  }
  cleanup();
  setup(self);

  // 読みこむ。同じものの .load は EEXIST、.replace はよい
  CHECK(put(AA ".replace", aa_blob, sizeof aa_blob) == 0, ".replace");
  CHECK(put(AA ".load", aa_blob, sizeof aa_blob) == EEXIST, ".load again is EEXIST");
  CHECK(put(AA ".replace", aa_blob, sizeof aa_blob) == 0, ".replace again");
  CHECK(put(AA ".replace", "nonsense", 8) == EPROTONOSUPPORT, ".replace of junk");
  char prof[4096] = "";
  int fd = open(AA "profiles", O_RDONLY);
  ssize_t k = fd < 0 ? -1 : read(fd, prof, sizeof prof - 1);
  prof[k > 0 ? k : 0] = 0;
  if (fd >= 0)
    close(fd);
  CHECK(strstr(prof, "aatest (enforce)\n") && strstr(prof, "aatest//sub (enforce)\n") && strstr(prof, "aachild (enforce)\n") && strstr(prof, "aacomplain (complain)\n"), "profiles: %s", prof);

  // .access: aatest で /tmp/aatest/r は読める (0x4)、/tmp/aatest/w は読めない
  fd = open(AA ".access", O_RDWR);
  char q[] = "label\0aatest\0\x02" D "/r", ans[256] = "";
  CHECK(fd >= 0 && write(fd, q, sizeof q - 1) == sizeof q - 1 && read(fd, ans, sizeof ans - 1) > 0, ".access");
  unsigned allow = 0;
  sscanf(ans, "allow 0x%x", &allow);
  CHECK(allow & 4, ".access allow 0x%x", allow);
  if (fd >= 0)
    close(fd);

  // aatest の中で
  char out[8192] = "";
  collect(D "/t", "child", out, sizeof out);
  int bad = fails;
  for (size_t i = 0; i < sizeof want / sizeof *want; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want[i]);
    CHECK(strstr(out, l), "want \"%s\"", want[i]);
  }
  if (fails != bad)
    printf("child said:\n%s", out);

  // capability: aacap と aacomplain の中で (cap2 は uid 1 の 0600)
  CHECK(chown(D "/cap2", 1, 1) == 0 && chmod(D "/cap2", 0600) == 0, "cap2");
  char cout[2048];
  collect(self, "cap", out, sizeof out);
  collect(self, "capc", cout, sizeof cout);
  strncat(out, cout, sizeof out - strlen(out) - 1);
  bad = fails;
  for (size_t i = 0; i < sizeof want_cap / sizeof *want_cap; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want_cap[i]);
    CHECK(strstr(out, l), "want \"%s\"", want_cap[i]);
  }
  if (fails != bad)
    printf("cap said:\n%s", out);

  // 決まりのないプロセスから: onexec (次の exec で aachild)
  char b[128];
  int st;
  pid_t c = fork();
  if (c == 0) {
    if (puts_("/proc/self/attr/exec", "exec aachild"))
      _exit(2);
    line("/proc/self/attr/exec", b, sizeof b);
    _exit(strcmp(b, "aachild (enforce)") ? 3 : 0);
  }
  waitpid(c, &st, 0);
  CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "onexec set (%d)", WEXITSTATUS(st));
  int q2[2];
  pipe(q2);
  fflush(stdout);
  c = fork();
  if (c == 0) {
    dup2(q2[1], 1);
    puts_("/proc/self/attr/exec", "exec aachild");
    execl(D "/ix", D "/ix", "attr", (char *)NULL);
    _exit(127);
  }
  close(q2[1]);
  k = read(q2[0], b, sizeof b - 1);
  b[k > 0 ? k : 0] = 0;
  b[strcspn(b, "\n")] = 0;
  close(q2[0]);
  waitpid(c, NULL, 0);
  CHECK(!strcmp(b, "aachild (enforce)"), "onexec gave \"%s\"", b);

  // 決まりのないプロセスには hat がない
  CHECK(HAT("changehat 1^hat1") == EPERM, "unconfined changehat is EPERM");

  // complain: 決まりの外も読める
  c = fork();
  if (c == 0)
    _exit(puts_("/proc/self/attr/current", "changeprofile aacomplain") ? 2 : op_open(D "/w", O_RDONLY) ? 3 : 0);
  waitpid(c, &st, 0);
  CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "complain allows (%d)", WEXITSTATUS(st));

  // 動いているプロセスのプロファイルを外すと unconfined に
  int go[2], ready[2];
  pipe(go);
  pipe(ready);
  c = fork();
  if (c == 0) {
    char x;
    if (puts_("/proc/self/attr/current", "changeprofile aachild"))
      _exit(2);
    int before = op_open(D "/w", O_RDONLY);
    write(ready[1], "r", 1);
    read(go[0], &x, 1);
    line("/proc/self/attr/current", b, sizeof b);
    _exit(before != EACCES ? 3 : strcmp(b, "unconfined") ? 4 : op_open(D "/w", O_RDONLY) ? 5 : 0);
  }
  char x;
  read(ready[0], &x, 1);
  CHECK(puts_(AA ".remove", "aachild") == 0, ".remove aachild");
  CHECK(puts_(AA ".remove", "aachild") == ENOENT, ".remove again is ENOENT");
  write(go[1], "g", 1);
  waitpid(c, &st, 0);
  CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "removed profile is unconfined (%d)", WEXITSTATUS(st));

  cleanup();
  printf("apparmor: %d failed\n", fails);
  return fails != 0;
}
