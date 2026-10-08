// マウントの namespace と mount / umount2 のテスト (カーネル): aios の中で動かす。作りかたは test/seccomp.c と同じ
// (zig cc -target aarch64-linux-musl -O2 -static -o mnt-test test/mnt.c)。root でない人 (ai) で動かす
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s (errno %d)\n", name, errno); fails++; } } while (0)
static int child(int (*fn)(void)) { pid_t p = fork(); if (p == 0) _exit(fn()); int st; waitpid(p, &st, 0); return WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st); }
static void link_of(const char *path, char *out) { ssize_t n = readlink(path, out, 63); out[n < 0 ? 0 : n] = 0; }
static int exists(const char *p) { struct stat st; return stat(p, &st) == 0; }
static int has_line(const char *file, const char *s) {
  char buf[8192]; int fd = open(file, O_RDONLY); if (fd < 0) return 0;
  ssize_t n = read(fd, buf, sizeof buf - 1); close(fd); if (n < 0) return 0; buf[n] = 0; return strstr(buf, s) != 0;
}
static int enter(void) { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0); return unshare(CLONE_NEWNS); }

static char dir[64];  /* /tmp/mnt-PID: ここにマウントしてみる */

static int no_privs(void) { return unshare(CLONE_NEWNS) == -1 && errno == EPERM ? 0 : 1; }
static int no_mount_outside(void) { return mount("tmpfs", dir, "tmpfs", 0, 0) == -1 && errno == EPERM ? 0 : 1; }
static int new_ns(void) {
  char before[64], after[64];
  link_of("/proc/self/ns/mnt", before);
  if (enter()) return 2;
  link_of("/proc/self/ns/mnt", after);
  return strncmp(after, "mnt:[", 5) == 0 && strcmp(before, after) != 0 ? 0 : 3;
}
/* 中で tmpfs をかぶせて書く。中の子にも見え、外には見えない (外の確かめは親で) */
static int tmpfs_inside(void) {
  if (enter()) return 2;
  if (mount("tmpfs", dir, "tmpfs", 0, 0)) return 3;
  char f[96]; snprintf(f, sizeof f, "%s/inside", dir);
  int fd = open(f, O_CREAT | O_WRONLY, 0644); if (fd < 0) return 4; close(fd);
  if (!has_line("/proc/self/mounts", " tmpfs ")) return 5;
  char want[96]; snprintf(want, sizeof want, "tmpfs %s tmpfs", dir);
  if (!has_line("/proc/self/mounts", want)) return 6;
  pid_t p = fork(); if (p == 0) _exit(exists(f) ? 0 : 1);
  int st; waitpid(p, &st, 0); if (WEXITSTATUS(st)) return 7;
  /* 外したら、下 (もとのディレクトリ) が見える */
  if (umount(dir)) return 8;
  return exists(f) ? 9 : 0;
}
/* bind: /etc を dir/b に見せる。2 つ重ねて 1 つ外す。同じところへの bind で止まらない */
static int bind_stack(void) {
  if (enter()) return 2;
  char b[96]; snprintf(b, sizeof b, "%s/b", dir);
  if (mount("/etc", dir, 0, MS_BIND, 0)) return 3;
  if (!exists(strcat(strcpy(b, dir), "/passwd"))) return 4;
  if (mount("tmpfs", dir, "tmpfs", 0, 0)) return 5;
  if (exists(b)) return 6;
  if (umount(dir)) return 7;
  if (!exists(b)) return 8;
  if (umount(dir)) return 9;
  if (exists(b)) return 10;
  if (mount(dir, dir, 0, MS_BIND, 0)) return 11;
  if (!exists(dir)) return 12;
  if (umount(dir)) return 13;
  /* 伝わり方を変える (mount --make-rprivate /) は、何もせずにうまくいく */
  if (mount("none", "/", 0, MS_REC | MS_PRIVATE, 0)) return 14;
  return 0;
}
static int bad(void) {
  if (enter()) return 2;
  if (!(mount("x", dir, "nosuchfs", 0, 0) == -1 && errno == ENODEV)) return 3;
  if (!(umount(dir) == -1 && errno == EINVAL)) return 4;
  return 0;
}

int main(void) {
  snprintf(dir, sizeof dir, "/tmp/mnt-%d", getpid());
  mkdir(dir, 0755);
  char inside[96]; snprintf(inside, sizeof inside, "%s/inside", dir);
  CHECK("proc-mounts", has_line("/proc/mounts", "proc /proc proc") && has_line("/proc/mounts", "sysfs /sys sysfs"));
  CHECK("needs-no-new-privs", getuid() != 0 && child(no_privs) == 0);
  CHECK("no-mount-outside", getuid() != 0 && child(no_mount_outside) == 0);
  CHECK("new-ns", child(new_ns) == 0);
  CHECK("tmpfs-inside", child(tmpfs_inside) == 0);
  CHECK("not-seen-outside", !exists(inside) && !has_line("/proc/mounts", dir));
  CHECK("bind-stack", child(bind_stack) == 0);
  CHECK("bad", child(bad) == 0);
  rmdir(dir);
  printf("mnt: %d failed\n", fails);
  return fails;
}
