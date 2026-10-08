// seccomp のテスト (カーネル): aios の中で動かす。作りかた (zig は bin/mkpkg.sh が build/zig に取ってくるもの):
//   zig cc -target aarch64-linux-musl -O2 -static -o seccomp-test test/seccomp.c
// フィルタをかけるのに no_new_privs が要ること、ERRNO と子への引き継ぎ、TRAP の SIGSYS と siginfo、KILL、
// 重ねたときにいちばんきびしいものが勝つこと、strict、へんなプログラムを断ること、引数を見るフィルタ
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <linux/audit.h>
#include <stddef.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s (errno %d)\n", name, errno); fails++; } } while (0)

static int install(struct sock_filter *f, int n, int flags) {
  struct sock_fprog p = { .len = n, .filter = f };
  return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, &p);
}
#define NR offsetof(struct seccomp_data, nr)
/* nr が N なら RET を、ほかは許す */
#define ONE(N, RET) struct sock_filter f[] = { BPF_STMT(BPF_LD|BPF_W|BPF_ABS, NR), BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K, N, 0, 1), BPF_STMT(BPF_RET|BPF_K, RET), BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW) }

static volatile int got_sys, got_nr, got_errno; static volatile unsigned got_arch;
static void on_sys(int s, siginfo_t *i, void *u) { got_sys = s; got_nr = i->si_syscall; got_arch = i->si_arch; got_errno = i->si_errno; }

/* 子で fn を動かし、終わりかたを返す */
static int child(void (*fn)(void)) { pid_t p = fork(); if (p == 0) { fn(); _exit(0); } int st; waitpid(p, &st, 0); return st; }

static void t_noprivs(void) { ONE(SYS_getppid, SECCOMP_RET_ERRNO | 7); _exit(install(f, 4, 0) == -1 && errno == EACCES ? 0 : 1); }
static void t_errno(void) {
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  ONE(SYS_getppid, SECCOMP_RET_ERRNO | 7);
  if (install(f, 4, 0)) _exit(2);
  long r = syscall(SYS_getppid); int e = errno;
  if (!(r == -1 && e == 7)) _exit(3);
  if (prctl(PR_GET_SECCOMP) != 2) _exit(4);
  /* 子にも引き継ぐ */
  pid_t p = fork(); if (p == 0) _exit(syscall(SYS_getppid) == -1 && errno == 7 ? 0 : 1);
  int st; waitpid(p, &st, 0); _exit(WEXITSTATUS(st) ? 5 : 0);
}
static void t_trap(void) {
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  struct sigaction sa; memset(&sa, 0, sizeof sa); sa.sa_sigaction = on_sys; sa.sa_flags = SA_SIGINFO; sigaction(SIGSYS, &sa, 0);
  ONE(SYS_getppid, SECCOMP_RET_TRAP | 42);
  if (install(f, 4, 0)) _exit(2);
  long r = syscall(SYS_getppid);
  _exit(got_sys == SIGSYS && got_nr == SYS_getppid && got_arch == AUDIT_ARCH_AARCH64 && got_errno == 42 && r == -1 ? 0 : 3);
}
static void t_kill(void) { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0); ONE(SYS_getppid, SECCOMP_RET_KILL_PROCESS); install(f, 4, 0); syscall(SYS_getppid); _exit(9); }
static void t_stack(void) {
  /* ERRNO 7 のあとに KILL をかさねると、きびしいほう (KILL)。ALLOW のフィルタを足してもゆるまない */
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  { ONE(SYS_getppid, SECCOMP_RET_ERRNO | 7); install(f, 4, 0); }
  { ONE(SYS_getppid, SECCOMP_RET_KILL_PROCESS); install(f, 4, 0); }
  { struct sock_filter a[] = { BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW) }; install(a, 1, SECCOMP_FILTER_FLAG_TSYNC); }
  syscall(SYS_getppid); _exit(9);
}
static void t_strict(void) {
  if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT, 0, 0, 0)) _exit(2);
  write(1, "", 0);   /* write は許される */
  syscall(SYS_getpid); /* ほかは SIGKILL */
  _exit(9);
}
static void t_bad(void) {
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  struct sock_filter nort[] = { BPF_STMT(BPF_LD|BPF_W|BPF_ABS, NR) };          /* RET で終わらない */
  struct sock_filter oob[] = { BPF_STMT(BPF_LD|BPF_W|BPF_ABS, 64), BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW) }; /* data の外 */
  struct sock_filter jmp[] = { BPF_JUMP(BPF_JMP|BPF_JA, 5, 0, 0), BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW) };  /* 外へ跳ぶ */
  int ok = install(nort, 1, 0) == -1 && errno == EINVAL && install(oob, 2, 0) == -1 && errno == EINVAL && install(jmp, 2, 0) == -1 && errno == EINVAL;
  unsigned a = SECCOMP_RET_LOG;
  ok = ok && syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &a) == 0 && prctl(PR_GET_SECCOMP) == 0;
  _exit(ok ? 0 : 1);
}
static void t_args(void) {
  /* 引数を見る: kill(0, 0) は許し、kill(..., 9) は ERRNO 13 */
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  struct sock_filter f[] = {
    BPF_STMT(BPF_LD|BPF_W|BPF_ABS, NR), BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K, SYS_kill, 0, 3),
    BPF_STMT(BPF_LD|BPF_W|BPF_ABS, offsetof(struct seccomp_data, args[1])), BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K, 9, 0, 1),
    BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ERRNO | 13), BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW) };
  install(f, 6, 0);
  int a = kill(getpid(), 0) == 0;
  int b = kill(getpid(), 9) == -1 && errno == 13;
  _exit(a && b ? 0 : 1);
}

int main(void) {
  int st;
  st = child(t_noprivs); CHECK("needs-no-new-privs", WIFEXITED(st) && WEXITSTATUS(st) == 0);
  st = child(t_errno); CHECK("errno-and-inherit", WIFEXITED(st) && WEXITSTATUS(st) == 0);
  st = child(t_trap); CHECK("trap-sigsys-info", WIFEXITED(st) && WEXITSTATUS(st) == 0);
  st = child(t_kill); CHECK("kill-process", WIFSIGNALED(st) && WTERMSIG(st) == SIGSYS);
  st = child(t_stack); CHECK("stack-strictest", WIFSIGNALED(st) && WTERMSIG(st) == SIGSYS);
  st = child(t_strict); CHECK("strict", WIFSIGNALED(st) && (WTERMSIG(st) == SIGKILL || WTERMSIG(st) == SIGSYS));
  st = child(t_bad); CHECK("reject-bad-programs", WIFEXITED(st) && WEXITSTATUS(st) == 0);
  st = child(t_args); CHECK("filter-on-args", WIFEXITED(st) && WEXITSTATUS(st) == 0);
  printf("seccomp: %d failed\n", fails);
  return fails != 0;
}
