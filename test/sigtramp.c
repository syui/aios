// SA_RESTORER なしのシグナルのハンドラから戻れるか (カーネル: signal.rs と vdso.rs の __kernel_rt_sigreturn)。
// aarch64 の glibc は SA_RESTORER を渡さないので、Linux と同じくカーネルが vDSO の戻り口へ戻す。
// 前は戻り先が 0 で、ハンドラから戻ると番地 0 で SIGSEGV になった (CI の glibc で作った seccomp の trap のテスト)。
// libc を通さずに rt_sigaction を呼んで、SA_RESTORER なしで登録する。aios の中で動かす
// (aarch64-linux-gnu-gcc -O2 -static -o sigtramp test/sigtramp.c)
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

// カーネルの struct sigaction (aarch64): handler、flags、restorer、mask
struct ksigaction {
  void (*handler)(int);
  unsigned long flags;
  void (*restorer)(void);
  unsigned long mask;
};

static volatile int got;

static void on_usr1(int sig) {
  got = sig;
}

int main(void) {
  struct ksigaction a;
  memset(&a, 0, sizeof a);
  a.handler = on_usr1;
  a.flags = 0; // SA_RESTORER なし
  if (syscall(SYS_rt_sigaction, SIGUSR1, &a, NULL, 8) != 0) {
    perror("rt_sigaction");
    return 1;
  }
  for (int i = 0; i < 3; i++) {
    got = 0;
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1);
    if (got != SIGUSR1) {
      printf("FAIL handler did not run (%d)\n", got);
      return 1;
    }
  }
  // ここまで来た: ハンドラから戻れた (戻れなければ SIGSEGV で終わる)
  puts("sigtramp: ok");
  return 0;
}
