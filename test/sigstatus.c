// /proc/self/status のシグナルの行 (SigQ の数、SigPnd、ShdPnd、SigBlk、SigIgn、SigCgt) と、受けとる順が Linux と同じか
// (カーネル: signal.rs の status_text と next_sig、proc.rs の sig_shared)。スレッドに来たもの (raise、tgkill) は SigPnd、
// プロセスに来たもの (kill、sigqueue) は ShdPnd。受けとるのはスレッドに来たものが先で、その中ではフォールトの
// シグナル (SIGSEGV など) が先、あとは番号の小さいものから
// (aarch64-linux-gnu-gcc -O2 -static -o sigstatus test/sigstatus.c。test/ldiff.sh sigstatus で Linux とくらべる)
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static void on_term(int sig) { (void)sig; }

static void show(const char *what) {
  char line[256];
  FILE *f = fopen("/proc/self/status", "r");
  printf("%s:", what);
  while (fgets(line, sizeof line, f)) {
    if (!strncmp(line, "SigQ:", 5)) {
      // 上限はマシンでちがう (Linux は RLIMIT_SIGPENDING): 数だけ
      unsigned q = 0;
      sscanf(line + 5, "%u", &q);
      printf(" SigQ=%u", q);
    } else if (!strncmp(line, "Sig", 3) || !strncmp(line, "ShdPnd:", 7)) {
      char k[16], v[32];
      if (sscanf(line, "%15[^:]:%31s", k, v) == 2) printf(" %s=%s", k, v);
    }
  }
  fclose(f);
  printf("\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  sigset_t m;
  sigemptyset(&m);
  sigaddset(&m, SIGUSR1);
  sigaddset(&m, SIGUSR2);
  sigaddset(&m, SIGHUP);
  sigaddset(&m, SIGSEGV);
  sigaddset(&m, SIGALRM);
  sigprocmask(SIG_BLOCK, &m, NULL);
  signal(SIGPIPE, SIG_IGN);
  struct sigaction sa = {.sa_handler = on_term};
  sigaction(SIGTERM, &sa, NULL);
  show("start");

  raise(SIGUSR2);                         // スレッドに (tgkill)
  kill(getpid(), SIGUSR1);                // プロセスに
  union sigval v = {.sival_int = 1};
  sigqueue(getpid(), SIGHUP, v);          // プロセスに
  syscall(SYS_tgkill, getpid(), gettid(), SIGALRM); // スレッドに
  show("pending");

  // 同じものがスレッドにもプロセスにも来ていれば、スレッドのものとして (1 つ)
  raise(SIGUSR1);
  show("again");

  // フォールトのシグナルは、スレッドの中で番号より先に
  raise(SIGSEGV);
  for (int i = 0; i < 6; i++) {
    siginfo_t si;
    struct timespec t = {0, 0};
    int s = sigtimedwait(&m, &si, &t);
    printf("take %d code %d\n", s, s > 0 ? si.si_code : 0);
  }
  show("empty");
  return 0;
}
