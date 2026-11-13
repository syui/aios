// 新しい PID の namespace の中 (自分は pid 1) で、子を作って待つ。Go の os/exec と同じ道:
// pidfd_open(getpid()) と waitid(P_PIDFD) が ECHILD、pidfd_send_signal(0)、clone(CLONE_PIDFD) (CLONE_VM|CLONE_VFORK も)、
// waitid(P_PIDFD) で終わりを待つ。fork と wait4(pid)、pidfd_open(子の pid) も。
// aibox --image の gitea が「git version」を待てなかった (wait: no child processes)
// test/ldiff.sh pidnswait で Linux とくらべる
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef P_PIDFD
#define P_PIDFD 3
#endif

static int bad; // CI (vm-ctests) のため: Linux とちがう答えがあれば 1

static const char *e(long r) { return r < 0 ? strerrorname_np(errno) : "0"; }

static int pidfd_open(pid_t pid) { return syscall(SYS_pidfd_open, pid, 0); }

static void wait_pidfd(const char *what, int fd) {
  siginfo_t si;
  memset(&si, 0, sizeof si);
  int r = waitid(P_PIDFD, fd, &si, WEXITED);
  printf("%s: waitid(P_PIDFD) %s pid %d code %d status %d\n", what, e(r), si.si_pid, si.si_code, si.si_status);
  bad |= r < 0 || si.si_code != CLD_EXITED;
}

static char stack[65536] __attribute__((aligned(16)));
static int child_exit(void *arg) { _exit((int)(long)arg); }
static int child_exec(void *arg) {
  (void)arg;
  char *argv[] = {"pidnswait", "exit11", NULL};
  execv("/proc/self/exe", argv);
  _exit(99);
}

static int inside(void) {
  printf("getpid %d getppid %d\n", getpid(), getppid());
  // Go の checkPidfd: 自分の pidfd、waitid は ECHILD (自分の親ではない)、signal 0
  int self = pidfd_open(getpid());
  printf("pidfd_open(self) %s\n", e(self));
  if (self >= 0) {
    printf("waitid(self) %s\n", e(waitid(P_PIDFD, self, NULL, WEXITED)));
    printf("pidfd_send_signal(self, 0) %s\n", e(syscall(SYS_pidfd_send_signal, self, 0, NULL, 0)));
    close(self);
  }
  // clone(CLONE_PIDFD) (fork と同じ)
  int fd = -1;
  pid_t c = clone(child_exit, stack + sizeof stack, CLONE_PIDFD | SIGCHLD, (void *)7L, &fd);
  printf("clone(CLONE_PIDFD) pid %d fd %s\n", c, fd >= 0 ? "ok" : "none");
  bad |= fd < 0;
  if (c > 0 && fd >= 0) {
    wait_pidfd("clone", fd);
    close(fd);
  }
  // Go の forkExec と同じ: CLONE_VM|CLONE_VFORK|CLONE_PIDFD
  fd = -1;
  c = clone(child_exit, stack + sizeof stack, CLONE_VM | CLONE_VFORK | CLONE_PIDFD | SIGCHLD, (void *)8L, &fd);
  printf("clone(VM|VFORK|PIDFD) pid %d fd %s\n", c, fd >= 0 ? "ok" : "none");
  bad |= fd < 0;
  if (c > 0 && fd >= 0) {
    wait_pidfd("vfork", fd);
    close(fd);
  }
  // fork と pidfd_open(子) と waitid
  c = fork();
  if (c == 0) {
    usleep(100000);
    _exit(9);
  }
  fd = pidfd_open(c);
  printf("fork pid %d pidfd_open %s\n", c, e(fd));
  if (fd >= 0) {
    wait_pidfd("fork", fd);
    close(fd);
  }
  // fork と wait4(pid)
  c = fork();
  if (c == 0) _exit(10);
  int st = 0;
  pid_t w = wait4(c, &st, 0, NULL);
  printf("wait4(pid) same %d status %d\n", w == c, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  // Go の pidfd がないときの道 (pidWait): waitid(P_PID, WEXITED|WNOWAIT) で終わるまで待ち、wait4(pid) で回収する。
  // 子は exec する (Go の forkExec と同じ: CLONE_VM|CLONE_VFORK、自分を exec しなおして 11 で終わる)
  c = clone(child_exec, stack + sizeof stack, CLONE_VM | CLONE_VFORK | SIGCHLD, NULL);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  int r = waitid(P_PID, c, &si, WEXITED | WNOWAIT);
  printf("exec: waitid(P_PID, WNOWAIT) %s pid same %d status %d\n", e(r), si.si_pid == c, si.si_status);
  w = wait4(c, &st, 0, NULL);
  printf("exec: wait4(pid) %s status %d\n", w == c ? "same" : e(w), WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  bad |= w != c || !WIFEXITED(st) || WEXITSTATUS(st) != 11;
  // fork でも同じ道
  c = fork();
  if (c == 0) _exit(12);
  memset(&si, 0, sizeof si);
  r = waitid(P_PID, c, &si, WEXITED | WNOWAIT);
  printf("fork: waitid(P_PID, WNOWAIT) %s pid same %d status %d\n", e(r), si.si_pid == c, si.si_status);
  w = wait4(c, &st, 0, NULL);
  printf("fork: wait4(pid) %s status %d\n", w == c ? "same" : e(w), WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  bad |= w != c || !WIFEXITED(st) || WEXITSTATUS(st) != 12;
  r = wait4(-1, &st, WNOHANG, NULL);
  printf("wait4(-1) %s\n", e(r));
  bad |= !(r < 0 && errno == ECHILD);
  return bad;
}

int main(int argc, char **argv) {
  if (argc > 1 && strcmp(argv[1], "exit11") == 0) return 11;
  setvbuf(stdout, NULL, _IONBF, 0);
  // namespace の外でも clone(CLONE_PIDFD) は pidfd を返すか
  int fd = -1;
  pid_t c = clone(child_exit, stack + sizeof stack, CLONE_PIDFD | SIGCHLD, (void *)6L, &fd);
  printf("outside: clone(CLONE_PIDFD) fd %s\n", fd >= 0 ? "ok" : "none");
  int outside_bad = fd < 0;
  if (fd >= 0) close(fd);
  waitpid(c, NULL, 0);
  if (unshare(CLONE_NEWUSER | CLONE_NEWPID) < 0) {
    printf("unshare %s\n", e(-1));
    return 1;
  }
  pid_t p = fork();
  if (p == 0) _exit(inside());
  int st = 0;
  waitpid(p, &st, 0);
  printf("outer: status %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  return outside_bad || !WIFEXITED(st) || WEXITSTATUS(st) != 0;
}
