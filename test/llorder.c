// landlock と Linux の順 (LSM のフックの順を Linux に合わせたところ。test/ldiff.sh llorder で Linux とくらべる)
//   O_CREAT: 作る (MAKE_REG) は作る前に、開く (READ_FILE / WRITE_FILE) は作ったあとに見る。開けなくても作ったものは残る
//   link: 同じディレクトリなら MAKE_REG だけ、ほかのディレクトリへは REFER も (なければ EXDEV)
//   rename と link でディレクトリをまたぐ: MAKE_* / REMOVE_* がなければ EACCES、REFER がないか、移したものの権利が
//   先でふえるなら EXDEV (Linux の no_more_access。入れかえなら逆も)
//   get_robust_list: 砂場の中から外のプロセスのものは読めない (ptrace_may_access に landlock も)
// それぞれ fork した子で砂場をかける (層が重ならないように)。出すのは 0 か errno の名前と、作ったものがあるか
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/landlock.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static char base[64];

static const char *err(long r) {
  if (r >= 0) return "0";
  switch (errno) {
  case EACCES: return "EACCES";
  case EXDEV: return "EXDEV";
  case EPERM: return "EPERM";
  case ENOENT: return "ENOENT";
  case EEXIST: return "EEXIST";
  default: return strerror(errno);
  }
}

static void path(char *out, const char *name) { snprintf(out, 128, "%s/%s", base, name); }

static int exists(const char *name) {
  char p[128];
  struct stat st;
  path(p, name);
  return stat(p, &st) == 0;
}

// handled のうち、dirs のそれぞれに allowed を許す層をかける (allowed が 0 のものはルールを足さない: Linux も ENOMSG)
static void sandbox(unsigned long long handled, const char **dirs, const unsigned long long *allowed, int n) {
  struct landlock_ruleset_attr attr = {.handled_access_fs = handled};
  int rs = syscall(SYS_landlock_create_ruleset, &attr, sizeof attr, 0);
  if (rs < 0) {
    printf("create_ruleset: %s\n", err(rs));
    exit(2);
  }
  for (int i = 0; i < n; i++) {
    if (!allowed[i]) continue;
    char p[128];
    path(p, dirs[i]);
    struct landlock_path_beneath_attr pb = {.allowed_access = allowed[i], .parent_fd = open(p, O_PATH | O_CLOEXEC)};
    if (syscall(SYS_landlock_add_rule, rs, LANDLOCK_RULE_PATH_BENEATH, &pb, 0) < 0) {
      printf("add_rule %s: %s\n", dirs[i], err(-1));
      exit(2);
    }
    close(pb.parent_fd);
  }
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  if (syscall(SYS_landlock_restrict_self, rs, 0) < 0) {
    printf("restrict_self: %s\n", err(-1));
    exit(2);
  }
  close(rs);
}

static void creat_case(const char *label, const char *name, int flags) {
  char p[128];
  path(p, name);
  int fd = open(p, flags, 0600);
  printf("%s: open %s, exists %d\n", label, err(fd), exists(name));
  if (fd >= 0) close(fd);
}

static void run(void (*f)(void)) {
  pid_t c = fork();
  if (c == 0) {
    f();
    fflush(stdout);
    _exit(0);
  }
  int st;
  waitpid(c, &st, 0);
}

#define RW (LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_WRITE_FILE)

// MAKE_REG だけ: 作れるが開けない (作ったものは残る)
static void make_only(void) {
  const char *d[] = {"a"};
  unsigned long long al[] = {LANDLOCK_ACCESS_FS_MAKE_REG};
  sandbox(RW | LANDLOCK_ACCESS_FS_MAKE_REG, d, al, 1);
  creat_case("make-only wronly", "a/f1", O_CREAT | O_WRONLY);
  creat_case("make-only rdonly", "a/f2", O_CREAT | O_RDONLY);
  creat_case("make-only excl-rdwr", "a/f3", O_CREAT | O_EXCL | O_RDWR);
}

// MAKE_REG と WRITE_FILE: 書くだけなら開ける、読むのもなら開けない
static void make_write(void) {
  const char *d[] = {"b"};
  unsigned long long al[] = {LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_WRITE_FILE};
  sandbox(RW | LANDLOCK_ACCESS_FS_MAKE_REG, d, al, 1);
  creat_case("make-write wronly", "b/f4", O_CREAT | O_WRONLY);
  creat_case("make-write rdwr", "b/f5", O_CREAT | O_RDWR);
}

// 作るのがだめなら作らない
static void no_make(void) {
  const char *d[] = {"b"};
  unsigned long long al[] = {RW};
  sandbox(RW | LANDLOCK_ACCESS_FS_MAKE_REG, d, al, 1);
  creat_case("no-make wronly", "b/f6", O_CREAT | O_WRONLY);
}

// link: 同じディレクトリは MAKE_REG、ほかのディレクトリへは REFER がなければ EXDEV、MAKE_REG がなければ EACCES
static void links(void) {
  const char *d[] = {"c1", "c2", "c3"};
  unsigned long long al[] = {LANDLOCK_ACCESS_FS_MAKE_REG, LANDLOCK_ACCESS_FS_MAKE_REG, 0};
  sandbox(LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_REFER, d, al, 3);
  char s[128], t[128];
  path(s, "c1/x");
  path(t, "c1/y");
  printf("link same dir: %s\n", err(link(s, t)));
  path(t, "c2/z");
  printf("link other dir (no refer): %s\n", err(link(s, t)));
  path(t, "c3/z");
  printf("link other dir (no make): %s\n", err(link(s, t)));
}

#ifndef RENAME_EXCHANGE
#define RENAME_EXCHANGE 2
#endif

static void move(const char *label, const char *from, const char *to, int how) {
  char s[128], t[128];
  path(s, from);
  path(t, to);
  long r = how < 0 ? link(s, t) : syscall(SYS_renameat2, AT_FDCWD, s, AT_FDCWD, t, how);
  printf("%s: %s\n", label, err(r));
}

// d1: 作る・消す・REFER。d2: それと READ_FILE。d3: 作る・消すだけ (REFER なし)。a: なにもなし
static void refer(void) {
  const unsigned long long mv = LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_REMOVE_FILE | LANDLOCK_ACCESS_FS_REFER;
  const char *d[] = {"d1", "d2", "d3"};
  unsigned long long al[] = {mv, mv | LANDLOCK_ACCESS_FS_READ_FILE, mv & ~LANDLOCK_ACCESS_FS_REFER};
  sandbox(mv | LANDLOCK_ACCESS_FS_READ_FILE, d, al, 3);
  move("rename same dir", "d1/r1", "d1/r1b", 0);
  move("rename no make", "d1/r1b", "a/r1", 0);
  move("rename no refer", "d1/r1b", "d3/r1", 0);
  move("rename gains read", "d1/r1b", "d2/r1", 0);
  move("rename loses read", "d2/r2", "d1/r2", 0);
  move("link gains read", "d1/r1b", "d2/l1", -1);
  move("link loses read", "d2/r4", "d1/l2", -1);
  move("exchange one gains", "d1/r3", "d2/r4", RENAME_EXCHANGE);
  move("exchange same rights", "d1/r1b", "d1/r3", RENAME_EXCHANGE);
}

// get_robust_list: 砂場の中から、砂場の外の親のものを
static void robust(void) {
  const char *d[] = {"a"};
  unsigned long long al[] = {RW};
  sandbox(RW, d, al, 1);
  void *head;
  size_t len;
  printf("get_robust_list parent: %s\n", err(syscall(SYS_get_robust_list, getppid(), &head, &len)));
  printf("get_robust_list self: %s\n", err(syscall(SYS_get_robust_list, 0, &head, &len)));
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  snprintf(base, sizeof base, "/tmp/llorder.%d", getpid());
  const char *dirs[] = {"", "/a", "/b", "/c1", "/c2", "/c3", "/d1", "/d2", "/d3"};
  for (unsigned i = 0; i < sizeof dirs / sizeof *dirs; i++) {
    char p[128];
    snprintf(p, sizeof p, "%s%s", base, dirs[i]);
    mkdir(p, 0700);
  }
  const char *files[] = {"c1/x", "d1/r1", "d2/r2", "d1/r3", "d2/r4"};
  for (unsigned i = 0; i < sizeof files / sizeof *files; i++) {
    char x[128];
    path(x, files[i]);
    close(open(x, O_CREAT | O_WRONLY, 0600));
  }
  run(make_only);
  run(make_write);
  run(no_make);
  run(links);
  run(refer);
  run(robust);
  return 0;
}
