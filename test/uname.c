// uname の形 (/proc/sys/kernel/uname: aios か linux) の切りかえ。aios の中で動かす (aarch64-linux-gnu-gcc -O2 -static
// -o uname test/uname.c)。Linux にはこの値がないので、何もしないで通る
//   はじめの形は aios (sysname aios)。子が UTS を分けて (root でなければユーザーの namespace も) linux にすると、子の
//   uname は sysname Linux、release は 7.4.0-aios-...、/proc/sys/kernel/ostype と osrelease、/proc/version もそれ。
//   子の子 (fork と、UTS を分けたもの) は受けつぐ。知らない値は EINVAL。親は aios のまま
// 1 行に ok NAME か FAIL NAME、おしまいに uname: N failed
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s\n", name); fails++; } fflush(stdout); } while (0)

static int put(const char *p, const char *s) {
  int fd = open(p, O_WRONLY);
  if (fd < 0) return -errno;
  int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -errno;
  close(fd);
  return r;
}

static void get(const char *p, char *buf, size_t n) {
  buf[0] = 0;
  int fd = open(p, O_RDONLY);
  if (fd < 0) return;
  ssize_t r = read(fd, buf, n - 1);
  buf[r > 0 ? r : 0] = 0;
  close(fd);
  char *nl = strchr(buf, '\n');
  if (nl) *nl = 0;
}

// 中の番号 inner に自分を割りあてたユーザーの namespace と UTS を作る (root なら UTS だけ)
static int newuts_as(int inner) {
  if (unshare(CLONE_NEWUTS) == 0) return 0;
  char map[64];
  uid_t uid = getuid();
  gid_t gid = getgid();
  if (unshare(CLONE_NEWUSER | CLONE_NEWUTS) < 0) return -1;
  snprintf(map, sizeof map, "%d %d 1\n", inner, (int)uid);
  put("/proc/self/uid_map", map);
  put("/proc/self/setgroups", "deny");
  snprintf(map, sizeof map, "%d %d 1\n", inner, (int)gid);
  put("/proc/self/gid_map", map);
  return 0;
}

static int newuts(void) { return newuts_as(0); }

static int is_linux(void) {
  struct utsname u;
  char ost[64], rel[128], ver[256];
  uname(&u);
  get("/proc/sys/kernel/ostype", ost, sizeof ost);
  get("/proc/sys/kernel/osrelease", rel, sizeof rel);
  get("/proc/version", ver, sizeof ver);
  return !strcmp(u.sysname, "Linux") && !strncmp(u.release, "7.4.", 4) && strstr(u.release, "-aios-") && !strcmp(ost, "Linux") &&
         !strcmp(rel, u.release) && !strncmp(ver, "Linux version 7.4.", 18);
}

int main(void) {
  char mode[32];
  get("/proc/sys/kernel/uname", mode, sizeof mode);
  if (!mode[0]) {
    printf("ok uname (skipped: no /proc/sys/kernel/uname)\nuname: 0 failed\n");
    return 0;
  }
  struct utsname u;
  uname(&u);
  CHECK("default-aios", !strcmp(mode, "aios") && !strcmp(u.sysname, "aios"));
  int p[2];
  if (pipe(p) < 0) return 1;
  pid_t pid = fork();
  if (pid == 0) {
    close(p[0]);
    char r[8] = "-------";
    if (newuts() < 0) {
      r[0] = 's';
      goto out;
    }
    r[0] = put("/proc/sys/kernel/uname", "linux") == 0 ? '1' : '0';
    r[1] = is_linux() ? '1' : '0';
    r[2] = put("/proc/sys/kernel/uname", "bsd") == -EINVAL ? '1' : '0';
    char m[32];
    get("/proc/sys/kernel/uname", m, sizeof m);
    r[3] = !strcmp(m, "linux") ? '1' : '0';
    pid_t g = fork();
    if (g == 0) _exit(is_linux() ? 0 : 1);
    int st;
    waitpid(g, &st, 0);
    r[4] = WIFEXITED(st) && WEXITSTATUS(st) == 0 ? '1' : '0';
    g = fork();
    if (g == 0) _exit(unshare(CLONE_NEWUTS) == 0 && is_linux() ? 0 : 1);
    waitpid(g, &st, 0);
    r[5] = WIFEXITED(st) && WEXITSTATUS(st) == 0 ? '1' : '0';
  out:
    if (write(p[1], r, 6) != 6) _exit(1);
    _exit(0);
  }
  close(p[1]);
  char r[8] = {0};
  if (read(p[0], r, 6) != 6) r[0] = 0;
  waitpid(pid, NULL, 0);
  if (r[0] == 's') {
    printf("ok uname-ns (skipped: unshare)\n");
  } else {
    CHECK("set-linux", r[0] == '1');
    CHECK("linux-fields", r[1] == '1');
    CHECK("bad-value-einval", r[2] == '1');
    CHECK("read-back", r[3] == '1');
    CHECK("fork-inherits", r[4] == '1');
    CHECK("newuts-inherits", r[5] == '1');
  }
  // 中に root の割りあてがない (aibox --image のように image の 65532 だけ) ユーザーの namespace でも、自分の UTS の
  // uname は書ける (sethostname できる人)
  pid = fork();
  if (pid == 0) {
    if (newuts_as(65532) < 0) _exit(2);
    _exit(put("/proc/sys/kernel/uname", "linux") == 0 && is_linux() ? 0 : 1);
  }
  int st;
  waitpid(pid, &st, 0);
  if (WIFEXITED(st) && WEXITSTATUS(st) == 2) {
    printf("ok uname-nonroot-map (skipped: unshare)\n");
  } else {
    CHECK("nonroot-map-set-linux", WIFEXITED(st) && WEXITSTATUS(st) == 0);
  }
  uname(&u);
  get("/proc/sys/kernel/uname", mode, sizeof mode);
  CHECK("parent-still-aios", !strcmp(mode, "aios") && !strcmp(u.sysname, "aios"));
  printf("uname: %d failed\n", fails);
  return fails != 0;
}
