// どのシステムコールの番号が ENOSYS か (Linux と同じものがあるか): 0〜499 をひとつずつ、引数をみな -1 にして、fork した子で
// 呼ぶ (止まれば 2 秒で SIGALRM)。ENOSYS の番号と、子が落ちたもの・止まったものを出す。exit と exit_group と
// rt_sigreturn と set_tid_address は呼ばない
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  static char enosys[8192], odd[8192];
  int ne = 0, no = 0;
  for (long n = 0; n < 500; n++) {
    if (n == SYS_exit || n == SYS_exit_group || n == SYS_rt_sigreturn || n == SYS_set_tid_address)
      continue;
    int p[2];
    if (pipe(p))
      return 1;
    pid_t k = fork();
    if (k == 0) {
      close(p[0]);
      alarm(2);
      long r = syscall(n, -1L, -1L, -1L, -1L, -1L, -1L);
      int e = r < 0 ? errno : 0;
      if (write(p[1], &e, sizeof e) != sizeof e)
        _exit(1);
      _exit(0);
    }
    close(p[1]);
    int e = -1, st = 0;
    if (read(p[0], &e, sizeof e) != sizeof e)
      e = -1;
    close(p[0]);
    waitpid(k, &st, 0);
    if (e == ENOSYS)
      ne += snprintf(enosys + ne, sizeof enosys - ne, " %ld", n);
    else if (e == -1)
      no += snprintf(odd + no, sizeof odd - no, " %ld(%s)", n, WIFSIGNALED(st) ? (WTERMSIG(st) == SIGALRM ? "hang" : "signal") : "exit");
  }
  printf("ENOSYS:%s\n", enosys);
  printf("no answer:%s\n", odd);
  return 0;
}
