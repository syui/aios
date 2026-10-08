// namespace のテスト (カーネル): aios の中で動かす。作りかたは test/seccomp.c と同じ (zig cc -target aarch64-linux-musl -static)
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s (errno %d)\n", name, errno); fails++; } } while (0)
static int child(int (*fn)(void)) { pid_t p = fork(); if (p == 0) _exit(fn()); int st; waitpid(p, &st, 0); return WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st); }
static void link_of(const char *path, char *out) { ssize_t n = readlink(path, out, 63); out[n < 0 ? 0 : n] = 0; }

static int uts_noprivs(void) { return getuid() != 0 && unshare(CLONE_NEWUTS) == -1 && errno == EPERM ? 0 : 1; }
static int uts_new(void) {
  char before[64], after[64], self[64]; struct utsname u;
  link_of("/proc/self/ns/uts", before);
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  if (unshare(CLONE_NEWUTS)) return 2;
  link_of("/proc/self/ns/uts", after);
  if (strcmp(before, after) == 0 || strncmp(after, "uts:[", 5)) return 3;
  if (sethostname("box", 3)) return 4;
  uname(&u);
  if (strcmp(u.nodename, "box")) return 5;
  /* 子も同じ UTS */
  pid_t p = fork(); if (p == 0) { struct utsname c; uname(&c); _exit(strcmp(c.nodename, "box") ? 1 : 0); }
  int st; waitpid(p, &st, 0); if (WEXITSTATUS(st)) return 6;
  return 0;
}
static int uts_outside(void) { struct utsname u; uname(&u); return strcmp(u.nodename, "box") == 0 ? 1 : 0; }
static int uts_sethost_root_only(void) { return getuid() != 0 && sethostname("x", 1) == -1 && errno == EPERM ? 0 : 1; }
static int fn_clone(void *a) { struct utsname u; sethostname("cl", 2); uname(&u); return strcmp(u.nodename, "cl") ? 1 : 0; }
static int uts_clone(void) {
  static char stack[65536];
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  pid_t p = clone(fn_clone, stack + sizeof stack, CLONE_NEWUTS | SIGCHLD, 0);
  if (p < 0) return 2;
  int st; waitpid(p, &st, 0); if (!WIFEXITED(st) || WEXITSTATUS(st)) return 3;
  struct utsname u; uname(&u); return strcmp(u.nodename, "cl") == 0 ? 4 : 0;
}

int main(void) {
  CHECK("uts-needs-no-new-privs", child(uts_noprivs) == 0);
  CHECK("uts-unshare", child(uts_new) == 0);
  CHECK("uts-not-outside", child(uts_outside) == 0);
  CHECK("sethostname-root-only", child(uts_sethost_root_only) == 0);
  CHECK("uts-clone", child(uts_clone) == 0);
  printf("ns: %d failed\n", fails);
  return fails != 0;
}
