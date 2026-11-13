// signalfd (signalfd4) が Linux と同じに動くか (カーネル: signalfd.rs と signal.rs の take / signalfd_info)。
// ブロックしたシグナルを fd から読む: 欄 (kill、tgkill、sigqueue、SIGCHLD、POSIX タイマー)、まとめ読み、mask の
// 変えなおし、poll と epoll、ブロックして待つ read (ほかのプロセスから起こす、ハンドラで EINTR)、fork した子では
// 子のシグナル、fdinfo の sigmask、まちがった引数。PostgreSQL 18 は、これで SIGURG を待つ (ラッチ)
// (aarch64-linux-gnu-gcc -O2 -static -o signalfd test/signalfd.c。test/ldiff.sh signalfd で Linux とくらべる)
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int rd(int fd, struct signalfd_siginfo *s, int n) {
  ssize_t r = read(fd, s, sizeof *s * n);
  if (r < 0) return -errno;
  return (int)(r / (ssize_t)sizeof *s);
}

static const char *err(int e) {
  switch (e) {
  case -EAGAIN: return "EAGAIN";
  case -EINVAL: return "EINVAL";
  case -EBADF: return "EBADF";
  case -EINTR: return "EINTR";
  case -EFAULT: return "EFAULT";
  case -EPERM: return "EPERM";
  case -ESRCH: return "ESRCH";
  default: return "?";
  }
}

static void show(const char *what, struct signalfd_siginfo *s) {
  printf("%s: signo %u code %d pid %s uid %s int %d ptr %llx status %d tid %s errno %d\n", what, s->ssi_signo,
         s->ssi_code, s->ssi_pid == 0 ? "0" : s->ssi_pid == (unsigned)getpid() ? "self" : "other",
         s->ssi_uid == getuid() ? "self" : "other", s->ssi_int, (unsigned long long)s->ssi_ptr, s->ssi_status,
         s->ssi_tid == 0 ? "0" : "set", s->ssi_errno);
}

static void fdmask(int fd) {
  char p[64], line[256];
  snprintf(p, sizeof p, "/proc/self/fdinfo/%d", fd);
  FILE *f = fopen(p, "r");
  if (!f) {
    printf("fdinfo: %s\n", strerror(errno));
    return;
  }
  while (fgets(line, sizeof line, f))
    if (!strncmp(line, "sigmask:", 8)) printf("fdinfo %s", line);
  fclose(f);
}

static void on_usr2(int sig) { (void)sig; }

int main(void) {
  struct signalfd_siginfo s[4];
  setvbuf(stdout, NULL, _IONBF, 0);
  sigset_t m;
  sigemptyset(&m);
  sigaddset(&m, SIGUSR1);
  sigaddset(&m, SIGURG);
  sigaddset(&m, SIGCHLD);
  sigaddset(&m, SIGRTMIN);
  sigprocmask(SIG_BLOCK, &m, NULL);

  // まちがった引数
  printf("sizemask 4: %s\n", syscall(SYS_signalfd4, -1, &m, 4, 0) < 0 ? err(-errno) : "ok");
  printf("bad flags: %s\n", syscall(SYS_signalfd4, -1, &m, 8, 1) < 0 ? err(-errno) : "ok");
  printf("bad mask ptr: %s\n", syscall(SYS_signalfd4, -1, (void *)8, 8, 0) < 0 ? err(-errno) : "ok");
  printf("bad fd: %s\n", signalfd(1000, &m, 0) < 0 ? err(-errno) : "ok");
  int pf[2];
  pipe(pf);
  printf("not signalfd: %s\n", signalfd(pf[0], &m, 0) < 0 ? err(-errno) : "ok");

  // SIGKILL と SIGSTOP は mask から落ちる
  sigset_t km = m;
  sigaddset(&km, SIGKILL);
  sigaddset(&km, SIGSTOP);
  int fd = signalfd(-1, &km, SFD_NONBLOCK | SFD_CLOEXEC);
  printf("create: %s\n", fd >= 0 ? "ok" : strerror(errno));
  int fl = fcntl(fd, F_GETFL);
  printf("cloexec %d nonblock %d rdwr %d\n", !!(fcntl(fd, F_GETFD) & FD_CLOEXEC), !!(fl & O_NONBLOCK),
         (fl & O_ACCMODE) == O_RDWR);
  fdmask(fd);
  char lnk[64], buf[64] = "";
  snprintf(lnk, sizeof lnk, "/proc/self/fd/%d", fd);
  readlink(lnk, buf, sizeof buf - 1);
  printf("link %s\n", buf);

  printf("empty: %s\n", err(rd(fd, s, 1)));
  printf("short buf: %s\n", read(fd, s, 100) < 0 ? err(-errno) : "ok");
  printf("write: %s\n", write(fd, s, sizeof s[0]) < 0 ? err(-errno) : "ok");

  // kill (SI_USER)
  kill(getpid(), SIGUSR1);
  struct pollfd p = {fd, POLLIN, 0};
  printf("poll %d revents %x\n", poll(&p, 1, 0), p.revents);
  int n = rd(fd, s, 4);
  printf("read %d\n", n);
  if (n > 0) show("kill", &s[0]);
  printf("poll after %d\n", poll(&p, 1, 0));

  // tgkill (SI_TKILL) と sigqueue (SI_QUEUE) を 2 つ、1 回で読む (番号の小さいものから)
  syscall(SYS_tgkill, getpid(), gettid(), SIGURG);
  union sigval v = {.sival_ptr = (void *)4242};
  sigqueue(getpid(), SIGUSR1, v);
  // 並ぶ順は番号順で出す (Linux はスレッドに来たもの (tgkill) を先に、aios は番号の小さいものから)
  n = rd(fd, s, 4);
  printf("read %d\n", n);
  for (int k = 0; k < 2; k++)
    for (int i = 0; i < n; i++)
      if ((int)s[i].ssi_signo == (k ? SIGURG : SIGUSR1)) show(k ? "tgkill" : "sigqueue", &s[i]);
  // ほかのプロセスに SI_QUEUE より上の印 (SI_USER など) を名のって送れない。自分には送れる
  siginfo_t si;
  memset(&si, 0, sizeof si);
  si.si_code = SI_USER;
  printf("fake SI_USER to init: %s\n", syscall(SYS_rt_sigqueueinfo, 1, SIGUSR1, &si) < 0 ? err(-errno) : "ok");
  si.si_code = SI_TKILL;
  printf("fake SI_TKILL to self: %s\n", syscall(SYS_rt_sigqueueinfo, getpid(), SIGURG, &si) < 0 ? err(-errno) : "ok");
  printf("tgsigqueue bad tid: %s\n", syscall(SYS_rt_tgsigqueueinfo, getpid(), 0, SIGUSR1, &si) < 0 ? err(-errno) : "ok");
  si.si_code = SI_QUEUE;
  si.si_value.sival_int = 7;
  printf("tgsigqueue: %s\n", syscall(SYS_rt_tgsigqueueinfo, getpid(), gettid(), SIGUSR1, &si) < 0 ? err(-errno) : "ok");
  n = rd(fd, s, 4);
  printf("read %d\n", n);
  for (int i = 0; i < n; i++) show("queued", &s[i]);
  // 来ているふつうのシグナル (32 より下) をもう一度送っても、はじめのものが残る (2 つめは捨てる)
  v.sival_int = 1;
  sigqueue(getpid(), SIGUSR1, v);
  v.sival_int = 2;
  sigqueue(getpid(), SIGUSR1, v);
  n = rd(fd, s, 4);
  printf("read %d int %d\n", n, n > 0 ? s[0].ssi_int : -1);

  // mask にないものは読めない (ブロックしたまま残る)。mask を変えなおすと読める
  sigset_t m2;
  sigemptyset(&m2);
  sigaddset(&m2, SIGUSR2);
  sigprocmask(SIG_BLOCK, &m2, NULL);
  raise(SIGUSR2);
  printf("not in mask: %s\n", err(rd(fd, s, 1)));
  sigaddset(&m2, SIGUSR1);
  printf("update: %s\n", signalfd(fd, &m2, 0) == fd ? "same fd" : strerror(errno));
  fdmask(fd);
  n = rd(fd, s, 1);
  printf("read %d\n", n);
  if (n > 0) show("after update", &s[0]);
  signalfd(fd, &m, 0);

  // epoll
  int ep = epoll_create1(0);
  struct epoll_event e = {.events = EPOLLIN, .data.u32 = 7}, out[2];
  epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e);
  printf("epoll empty %d\n", epoll_wait(ep, out, 2, 0));
  kill(getpid(), SIGUSR1);
  n = epoll_wait(ep, out, 2, 0);
  printf("epoll %d data %u events %x\n", n, n > 0 ? out[0].data.u32 : 0, n > 0 ? out[0].events : 0);
  rd(fd, s, 1);
  printf("epoll drained %d\n", epoll_wait(ep, out, 2, 0));

  // SIGCHLD (CLD_EXITED) と、epoll で待っていて起きる
  pid_t c = fork();
  if (c == 0) {
    usleep(50000);
    _exit(7);
  }
  n = epoll_wait(ep, out, 2, 5000);
  printf("epoll wait %d\n", n);
  n = rd(fd, s, 1);
  printf("read %d\n", n);
  if (n > 0)
    printf("sigchld: signo %u code %d pid %s status %d\n", s[0].ssi_signo, s[0].ssi_code,
           s[0].ssi_pid == (unsigned)c ? "child" : "other", s[0].ssi_status);
  waitpid(c, NULL, 0);

  // ほかのプロセスからの kill で、epoll_wait と poll (どちらも時間切れなし) が起きる (PostgreSQL のラッチ: SetLatch が
  // 寝ているプロセスに SIGURG を送る)。起きなければ 2 秒の時間切れのあとで見つける (slow)
  for (int k = 0; k < 2; k++) {
    struct timespec a0, a1;
    clock_gettime(CLOCK_MONOTONIC, &a0);
    c = fork();
    if (c == 0) {
      usleep(50000);
      kill(getppid(), SIGURG);
      _exit(0);
    }
    if (k == 0) {
      n = epoll_wait(ep, out, 1, 2000);
      clock_gettime(CLOCK_MONOTONIC, &a1);
      printf("epoll woken by kill %d %s\n", n, a1.tv_sec - a0.tv_sec < 1 ? "fast" : "slow");
    } else {
      p.revents = 0;
      n = poll(&p, 1, 2000);
      clock_gettime(CLOCK_MONOTONIC, &a1);
      printf("poll woken by kill %d revents %x %s\n", n, p.revents, a1.tv_sec - a0.tv_sec < 1 ? "fast" : "slow");
    }
    waitpid(c, NULL, 0);
    rd(fd, s, 4);
  }

  // POSIX タイマー (SI_TIMER)
  struct sigevent ev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGRTMIN, .sigev_value.sival_ptr = (void *)99};
  timer_t t;
  timer_create(CLOCK_MONOTONIC, &ev, &t);
  struct itimerspec it = {.it_value = {0, 1000000}};
  timer_settime(t, 0, &it, NULL);
  p.revents = 0;
  printf("poll timer %d\n", poll(&p, 1, 5000));
  n = rd(fd, s, 1);
  printf("read %d\n", n);
  if (n > 0) {
    show("timer", &s[0]);
    printf("timer id %s overrun %u\n", s[0].ssi_tid == (unsigned)(long)t ? "same" : "other", s[0].ssi_overrun);
  }
  timer_delete(t);

  // fork した子は、同じ fd で子に来たものを読む (親に来ているものは見えない)
  kill(getpid(), SIGUSR1);
  c = fork();
  if (c == 0) {
    int r = rd(fd, s, 1);
    kill(getpid(), SIGURG);
    int r2 = rd(fd, s, 1);
    printf("child: first %s, then %d signo %u\n", err(r), r2, r2 > 0 ? s[0].ssi_signo : 0);
    fflush(stdout);
    _exit(0);
  }
  waitpid(c, NULL, 0);
  n = rd(fd, s, 1);
  printf("parent still %d signo %u\n", n, n > 0 ? s[0].ssi_signo : 0);
  rd(fd, s, 4); // 子の SIGCHLD

  // ブロックする read: ほかのプロセスから送られて起きる (子が終わった SIGCHLD を先に読まないよう、SIGURG だけ)
  sigset_t mu;
  sigemptyset(&mu);
  sigaddset(&mu, SIGURG);
  int bfd = signalfd(-1, &mu, 0);
  c = fork();
  if (c == 0) {
    usleep(50000);
    kill(getppid(), SIGURG);
    _exit(0);
  }
  n = rd(bfd, s, 1);
  printf("blocking read %d signo %u pid %s\n", n, n > 0 ? s[0].ssi_signo : 0,
         n > 0 && s[0].ssi_pid == (unsigned)c ? "child" : "other");
  waitpid(c, NULL, 0);
  rd(fd, s, 4);

  // ブロックする read のあいだに、ブロックしていないシグナル (SA_RESTART なしのハンドラ) が来れば EINTR
  struct sigaction sa = {.sa_handler = on_usr2};
  sigaction(SIGUSR2, &sa, NULL);
  sigprocmask(SIG_UNBLOCK, &m2, NULL); // SIGUSR2 (と SIGUSR1) を通す: 残っていた SIGUSR2 はここでハンドラへ
  sigprocmask(SIG_BLOCK, &m, NULL);
  c = fork();
  if (c == 0) {
    usleep(50000);
    kill(getppid(), SIGUSR2);
    _exit(0);
  }
  printf("interrupted read: %s\n", err(rd(bfd, s, 1)));
  waitpid(c, NULL, 0);

  close(bfd);
  close(fd);
  close(ep);
  printf("done\n");
  return 0;
}
