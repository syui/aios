// 端末の設定をうしろのジョブが変えようとしたら SIGTTOU で止まるか (カーネル: tty.rs の check_change。Linux の
// tty_check_change)。pty を開いてセッションの端末にし、前のグループでないものが tcsetattr、tcflush、tcsetpgrp を
// すると止まる (WUNTRACED で SIGTTOU)。SIGTTOU を無視かブロックしていれば止まらずに通る。読むだけ (tcgetattr) は
// 止まらない。止まったものを前に出して SIGCONT すると、ioctl をやりなおして通る
// (aarch64-linux-gnu-gcc -O2 -static -o ttou test/ttou.c。test/ldiff.sh ttou で Linux とくらべる)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static int sl;
static struct termios t0;

// うしろのグループで op を動かし、どうなったか (stopped SIG / exit N)
static void bg(const char *what, int op, int how) {
  pid_t c = fork();
  if (c == 0) {
    setpgid(0, 0);
    signal(SIGTTOU, how == 1 ? SIG_IGN : SIG_DFL);
    if (how == 2) {
      sigset_t m;
      sigemptyset(&m);
      sigaddset(&m, SIGTTOU);
      sigprocmask(SIG_BLOCK, &m, NULL);
    }
    int r = 0;
    struct termios t;
    switch (op) {
    case 0: r = tcsetattr(sl, TCSANOW, &t0); break;
    case 1: r = tcgetattr(sl, &t); break;
    case 2: r = tcflush(sl, TCIFLUSH); break;
    case 3: r = tcsetpgrp(sl, getpgrp()); break;
    }
    _exit(r < 0 ? 100 + errno : 0);
  }
  setpgid(c, c);
  int st;
  waitpid(c, &st, WUNTRACED);
  if (WIFSTOPPED(st)) {
    printf("%s: stopped %s\n", what, WSTOPSIG(st) == SIGTTOU ? "SIGTTOU" : strsignal(WSTOPSIG(st)));
    // 前に出して続ける: やりなおした ioctl が通る
    tcsetpgrp(sl, c);
    kill(c, SIGCONT);
    waitpid(c, &st, 0);
    printf("%s: after fg exit %d\n", what, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    tcsetpgrp(sl, getpgrp());
  } else {
    printf("%s: exit %d\n", what, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  }
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int m = posix_openpt(O_RDWR | O_NOCTTY);
  if (m < 0 || grantpt(m) < 0 || unlockpt(m) < 0) {
    printf("no pty: %s\n", strerror(errno));
    return 0;
  }
  char *name = ptsname(m);
  pid_t s = fork();
  if (s == 0) {
    setsid();
    sl = open(name, O_RDWR);
    if (sl < 0 || ioctl(sl, TIOCSCTTY, 0) < 0) {
      printf("ctty: %s\n", strerror(errno));
      _exit(1);
    }
    printf("foreground is me: %d\n", tcgetpgrp(sl) == getpgrp());
    tcgetattr(sl, &t0);
    // 前のグループを変えるとき、自分がうしろになっても止まらないように (シェルと同じ)
    signal(SIGTTOU, SIG_IGN);
    bg("bg tcsetattr", 0, 0);
    bg("bg tcsetattr ignored", 0, 1);
    bg("bg tcsetattr blocked", 0, 2);
    bg("bg tcgetattr", 1, 0);
    bg("bg tcflush", 2, 0);
    bg("bg tcsetpgrp", 3, 0);
    printf("fg tcsetattr: %d\n", tcsetattr(sl, TCSANOW, &t0));
    _exit(0);
  }
  int st;
  waitpid(s, &st, 0);
  printf("done %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  return 0;
}
