// ユーザーの namespace のテスト (カーネル): aios の中で、root でない人 (ai) で動かす。作りかたは test/seccomp.c と同じ
// (zig cc -target aarch64-linux-musl -O2 -static -o userns-test test/userns.c)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s (errno %d)\n", name, errno); fails++; } } while (0)
static int child(int (*fn)(void)) { pid_t p = fork(); if (p == 0) _exit(fn()); int st; waitpid(p, &st, 0); return WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st); }
static int put(const char *file, const char *s) { int fd = open(file, O_WRONLY); if (fd < 0) return -1; int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1; close(fd); return r; }
static void link_of(const char *path, char *out) { ssize_t n = readlink(path, out, 63); out[n < 0 ? 0 : n] = 0; }
static uid_t me; static gid_t mygid;

/* 自分の uid / gid を中の 0 に */
static int enter_root(void) {
  char m[64];
  if (unshare(CLONE_NEWUSER)) return 1;
  snprintf(m, sizeof m, "0 %d 1\n", me); if (put("/proc/self/uid_map", m)) return 2;
  if (put("/proc/self/setgroups", "deny")) return 3;
  snprintf(m, sizeof m, "0 %d 1\n", mygid); if (put("/proc/self/gid_map", m)) return 4;
  return 0;
}

static int unmapped(void) {
  char before[64], after[64];
  link_of("/proc/self/ns/user", before);
  if (unshare(CLONE_NEWUSER)) return 1;  /* no_new_privs なしで */
  link_of("/proc/self/ns/user", after);
  if (strncmp(after, "user:[", 6) || !strcmp(before, after)) return 2;
  return getuid() == 65534 && getgid() == 65534 ? 0 : 3;
}
static int mapped(void) {
  int r = enter_root(); if (r) return r;
  if (getuid() || geteuid() || getgid() || getegid()) return 5;
  char buf[4096]; int fd = open("/proc/self/status", O_RDONLY); ssize_t n = read(fd, buf, sizeof buf - 1); close(fd); buf[n > 0 ? n : 0] = 0;
  if (!strstr(buf, "Uid:\t0\t0\t0\t0")) return 6;
  fd = open("/proc/self/uid_map", O_RDONLY); n = read(fd, buf, sizeof buf - 1); close(fd); buf[n > 0 ? n : 0] = 0;
  char want[64]; snprintf(want, sizeof want, "%d", me); if (!strstr(buf, want)) return 7;
  /* もう一度は書けない */
  if (put("/proc/self/uid_map", "0 0 1\n") == 0) return 8;
  return 0;
}
static int gid_needs_deny(void) {
  char m[64];
  if (unshare(CLONE_NEWUSER)) return 1;
  snprintf(m, sizeof m, "0 %d 1\n", mygid);
  return put("/proc/self/gid_map", m) == -1 && errno == EPERM ? 0 : 2;
}
static int only_own_id(void) {
  if (unshare(CLONE_NEWUSER)) return 1;
  if (put("/proc/self/uid_map", "0 0 1\n") == 0) return 2;            /* 外の root は地図にできない */
  char m[64]; snprintf(m, sizeof m, "0 %d 2\n", me);
  if (put("/proc/self/uid_map", m) == 0) return 3;                    /* 2 つもだめ */
  return 0;
}
/* 中の root でも、外の自分にできないことはできない */
static int files(void) {
  int r = enter_root(); if (r) return r;
  struct stat st;
  char f[64]; snprintf(f, sizeof f, "/tmp/userns-%d", getpid());
  int fd = open(f, O_CREAT | O_WRONLY, 0644); if (fd < 0) return 5; close(fd);
  if (stat(f, &st) || st.st_uid != 0 || st.st_gid != 0) return 6;      /* 自分のものは 0 に見える */
  if (stat("/etc/passwd", &st) || st.st_uid != 65534) return 7;       /* 外の root のものは 65534 */
  if (open("/etc/passwd", O_WRONLY) >= 0 || errno != EACCES) return 8;
  if (chown(f, 0, 0)) return 9;                                       /* 0 は自分 */
  if (chown(f, 5, -1) == 0 || errno != EINVAL) return 10;             /* 地図にない番号 */
  if (setuid(0)) return 11;
  if (setuid(5) == 0 || errno != EINVAL) return 12;
  unlink(f);
  return 0;
}
/* 中の root はほかの namespace を作れて (no_new_privs なしで)、自分のマウントの namespace でマウントできる */
static int nested_ns(void) {
  int r = enter_root(); if (r) return r;
  if (unshare(CLONE_NEWNS | CLONE_NEWUTS)) return 5;
  if (sethostname("inuser", 6)) return 6;
  char d[64]; snprintf(d, sizeof d, "/tmp/userns-m-%d", getpid()); mkdir(d, 0755);
  if (mount("tmpfs", d, "tmpfs", 0, 0)) return 7;
  umount(d); rmdir(d);
  if (unshare(CLONE_NEWUSER) == 0 || errno != EINVAL) return 8;      /* 入れ子はない */
  return 0;
}
/* setuid のプログラムは中では効かない */
static int no_setuid(void) {
  int r = enter_root(); if (r) return r;
  pid_t p = fork();
  if (p == 0) { int null = open("/dev/null", O_WRONLY); dup2(null, 1); dup2(null, 2); execl("/usr/bin/sudo", "sudo", "-n", "true", (char *)0); _exit(99); }
  int st; waitpid(p, &st, 0);
  return WIFEXITED(st) && WEXITSTATUS(st) != 0 ? 0 : 5;
}

int main(void) {
  me = getuid(); mygid = getgid();
  CHECK("not-root", me != 0);
  CHECK("unmapped", child(unmapped) == 0);
  CHECK("mapped", child(mapped) == 0);
  CHECK("gid-needs-deny", child(gid_needs_deny) == 0);
  CHECK("only-own-id", child(only_own_id) == 0);
  CHECK("files", child(files) == 0);
  CHECK("nested-ns", child(nested_ns) == 0);
  CHECK("no-setuid", child(no_setuid) == 0);
  CHECK("outside-unchanged", getuid() == me);
  printf("userns: %d failed\n", fails);
  return fails;
}
