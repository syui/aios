// AppArmor (kernel/src/apparmor.rs) のたしかめ: test/apparmor/test.profile (gen.sh が apparmor_parser で作った blob.h) を
// /sys/kernel/security/apparmor/.replace で読み、このプログラムの写し /tmp/aatest/t (aatest がパスでつく) に、決まりの
// 中から open、作る・消す、rename、link、chmod、truncate、flock、exec (ix px cx ux) と change_profile をためさせる。
// それから、決まりのないプロセスから onexec、complain、動いているプロセスのプロファイルを外す (unconfined にもどる)。
// root で動かす (でなければ sudo -n でもう一度)。AppArmor のついた Linux でも同じに通ること
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
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
  waitpid(k, NULL, 0);
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
  const char *ex[] = {"ix", "px", "cx", "ux", "nox"};
  for (int i = 0; i < 5; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", ex[i]);
    run(p, "attr", b, sizeof b);
    printf("exec-%s %s\n", ex[i], b);
  }
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
    "exec-cx aatest//sub (enforce)", "exec-ux unconfined", "exec-nox ERR 13", "change-complain 13", "change-child 0",
    "changed aachild (enforce)",
};

static void setup(const char *self) {
  mkdir(D, 0755);
  mkdir(D "/d", 0755);
  const char *files[] = {"r", "w", "rw", "a", "deny", "x", "own", "k"};
  for (size_t i = 0; i < sizeof files / sizeof *files; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", files[i]);
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) {
      write(fd, "x\n", 2);
      close(fd);
    }
  }
  // このプログラムの写し (t と、exec の行き先)
  const char *bins[] = {"t", "ix", "px", "cx", "ux", "nox"};
  for (size_t i = 0; i < sizeof bins / sizeof *bins; i++) {
    char cmd[640];
    snprintf(cmd, sizeof cmd, "cp %s " D "/%s && chmod 755 " D "/%s", self, bins[i], bins[i]);
    CHECK(system(cmd) == 0, "copy %s", bins[i]);
  }
}

static void cleanup(void) {
  puts_(AA ".remove", "aatest");
  puts_(AA ".remove", "aachild");
  puts_(AA ".remove", "aacomplain");
  system("rm -rf " D);
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "child"))
    return child();
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
  int p[2];
  pipe(p);
  fflush(stdout);
  pid_t c = fork();
  if (c == 0) {
    dup2(p[1], 1);
    close(p[0]);
    execl(D "/t", D "/t", "child", (char *)NULL);
    _exit(127);
  }
  close(p[1]);
  size_t got = 0;
  while ((k = read(p[0], out + got, sizeof out - 1 - got)) > 0)
    got += k;
  out[got] = 0;
  close(p[0]);
  int st;
  waitpid(c, &st, 0);
  for (size_t i = 0; i < sizeof want / sizeof *want; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want[i]);
    CHECK(strstr(out, l), "want \"%s\"", want[i]);
  }
  if (fails)
    printf("child said:\n%s", out);

  // 決まりのないプロセスから: onexec (次の exec で aachild)
  char b[128];
  c = fork();
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
