// fcntl の持ち主 (F_SETOWN / F_GETOWN / F_SETOWN_EX / F_GETOWN_EX)、F_SETSIG / F_GETSIG と O_ASYNC (SIGIO) が Linux と同じか:
//   owner  持ち主 (プロセス、グループ、スレッド)、いないもの (ESRCH)、INT_MIN、0 で消す、ゾンビと終わったプロセス、dup と fork
//   sig    F_SETSIG と F_GETSIG (まちがった番号は EINVAL)
//   flag   O_ASYNC (F_SETFL と FIOASYNC) は fasync のあるもの (パイプ、ソケット、inotify) にだけ残る
//   pipe   書くと読む側に POLL_IN、読むと書く側に POLL_OUT、書く側を閉じると読む側に POLL_IN。F_SETSIG なら si_code、
//          si_band、si_fd (SIGIO はそのまま、SIGBUS などは SI_SIGIO)
//   pgrp   グループの持ち主: グループのみなに
//   tid    スレッドの持ち主: そのスレッドだけに
//   query  F_DUPFD_QUERY、F_CREATED_QUERY、F_GET_RW_HINT / F_SET_RW_HINT
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef F_DUPFD_QUERY
#define F_DUPFD_QUERY 1027
#endif
#ifndef F_CREATED_QUERY
#define F_CREATED_QUERY 1028
#endif
#ifndef F_GET_RW_HINT
#define F_GET_RW_HINT 1035
#define F_SET_RW_HINT 1036
#endif
#define F_GET_FILE_RW_HINT 1037

static int bad;

// libc を通さない fcntl (musl の F_GETOWN は F_GETOWN_EX を使い、-4095〜-1 の答えは errno にしてしまう)
static long raw_fcntl(int fd, long cmd, long arg) {
  register long x8 __asm__("x8") = SYS_fcntl;
  register long x0 __asm__("x0") = fd;
  register long x1 __asm__("x1") = cmd;
  register long x2 __asm__("x2") = arg;
  __asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
  return x0;
}
static long fc(int fd, int cmd, long arg) {
  long r = fcntl(fd, cmd, arg);
  return r < 0 ? -errno : r;
}

// 受けたシグナルを順に覚える
struct ent {
  int signo, code, fd;
  long band;
};
static struct ent ents[64];
static volatile int nent;
static void on_sig(int s, siginfo_t *si, void *u) {
  (void)u;
  int n = nent;
  if (n < 64) {
    ents[n].signo = s;
    ents[n].code = si->si_code;
    ents[n].fd = si->si_fd;
    ents[n].band = si->si_band;
    nent = n + 1;
  }
}

static struct {
  int fd;
  const char *name;
} names[16];
static int nnames;
static void name(int fd, const char *n) {
  for (int i = 0; i < nnames; i++)
    if (names[i].fd == fd) {
      names[i].name = n;
      return;
    }
  names[nnames].fd = fd;
  names[nnames++].name = n;
}
static const char *fdname(int fd) {
  for (int i = 0; i < nnames; i++)
    if (names[i].fd == fd)
      return names[i].name;
  static char b[16];
  snprintf(b, sizeof b, "%d", fd);
  return b;
}
static void signame(int s, char *b) {
  if (s == SIGIO)
    strcpy(b, "SIGIO");
  else if (s == SIGURG)
    strcpy(b, "SIGURG");
  else if (s == SIGBUS)
    strcpy(b, "SIGBUS");
  else if (s >= SIGRTMIN && s <= SIGRTMAX)
    sprintf(b, "RT+%d", s - SIGRTMIN);
  else
    sprintf(b, "sig%d", s);
}
static const char *codename(int c) {
  static const char *poll[] = {"", "POLL_IN", "POLL_OUT", "POLL_MSG", "POLL_ERR", "POLL_PRI", "POLL_HUP"};
  static char b[16];
  if (c >= 1 && c <= 6)
    return poll[c];
  if (c == SI_KERNEL)
    return "kernel";
  if (c == SI_SIGIO)
    return "sigio";
  snprintf(b, sizeof b, "%d", c);
  return b;
}

// シグナルが届くのを待つ (自分あてのものはシステムコールから戻るときに)
static void settle(void) {
  struct timespec t = {0, 20 * 1000 * 1000};
  while (nanosleep(&t, &t) < 0 && errno == EINTR)
    ;
}

static void show(const char *what) {
  settle();
  int n = nent;
  printf("%s: %d", what, n);
  for (int i = 0; i < n; i++) {
    char s[16];
    signame(ents[i].signo, s);
    if (ents[i].code == SI_KERNEL)
      printf(" [%s kernel]", s);
    else
      printf(" [%s %s band %#lx fd %s]", s, codename(ents[i].code), ents[i].band, fdname(ents[i].fd));
  }
  printf("\n");
  nent = 0;
}

static void async_on(int fd, int sig) {
  fcntl(fd, F_SETOWN, getpid());
  fcntl(fd, F_SETSIG, sig);
  fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_ASYNC);
}

static void owner(void) {
  int p[2];
  pipe(p);
  struct f_owner_ex ex = {99, 99};
  long r = fc(p[0], F_GETOWN_EX, (long)&ex);
  printf("owner: new %ld, ex %ld type %d pid %d", raw_fcntl(p[0], F_GETOWN, 0), r, ex.type, ex.pid);
  r = fc(p[0], F_SETOWN, getpid());
  printf(", self %ld get %s", r, raw_fcntl(p[0], F_GETOWN, 0) == getpid() ? "same" : "other");
  fc(p[0], F_GETOWN_EX, (long)&ex);
  printf(" ex type %d %s\n", ex.type, ex.pid == getpid() ? "same" : "other");
  // グループ (自分のグループを作る子で: 手本の Linux の init の子はグループ 0)
  pid_t c = fork();
  if (c == 0) {
    setpgid(0, 0);
    pid_t g = getpgrp();
    r = fc(p[0], F_SETOWN, -g);
    printf("owner: pgrp %ld get %s", r, raw_fcntl(p[0], F_GETOWN, 0) == -g ? "-pgrp" : "other");
    memset(&ex, 0x55, sizeof ex);
    r = fc(p[0], F_GETOWN_EX, (long)&ex);
    printf(" ex %ld type %d %s, libc %s\n", r, ex.type, ex.pid == g ? "same" : "other", fcntl(p[0], F_GETOWN) == -g ? "-pgrp" : "other");
    _exit(0);
  }
  waitpid(c, NULL, 0);
  ex.type = F_OWNER_TID;
  ex.pid = gettid();
  r = fc(p[0], F_SETOWN_EX, (long)&ex);
  printf("owner: tid %ld get %s", r, raw_fcntl(p[0], F_GETOWN, 0) == gettid() ? "same" : "other");
  memset(&ex, 0x55, sizeof ex);
  fc(p[0], F_GETOWN_EX, (long)&ex);
  printf(" ex type %d %s\n", ex.type, ex.pid == gettid() ? "same" : "other");
  printf("owner: none %ld, INT_MIN %ld", fc(p[0], F_SETOWN, 4000000), fc(p[0], F_SETOWN, INT_MIN));
  ex.type = 3;
  ex.pid = getpid();
  printf(", ex type 3 %ld", fc(p[0], F_SETOWN_EX, (long)&ex));
  ex.type = F_OWNER_PGRP;
  ex.pid = 4000000;
  printf(", ex none %ld", fc(p[0], F_SETOWN_EX, (long)&ex));
  ex.type = F_OWNER_PGRP;
  ex.pid = 0;
  r = fc(p[0], F_SETOWN_EX, (long)&ex);
  memset(&ex, 0x55, sizeof ex);
  fc(p[0], F_GETOWN_EX, (long)&ex);
  printf(", ex pgrp 0 %ld get %ld ex type %d pid %d", r, raw_fcntl(p[0], F_GETOWN, 0), ex.type, ex.pid);
  fc(p[0], F_SETOWN, getpid());
  r = fc(p[0], F_SETOWN, 0);
  fc(p[0], F_GETOWN_EX, (long)&ex);
  printf(", zero %ld get %ld ex type %d pid %d\n", r, raw_fcntl(p[0], F_GETOWN, 0), ex.type, ex.pid);
  // ゾンビ (まだ待っていない) と、待ったあと
  c = fork();
  if (c == 0) {
    pause();
    _exit(0);
  }
  r = fc(p[0], F_SETOWN, c);
  long alive = raw_fcntl(p[0], F_GETOWN, 0);
  kill(c, SIGKILL);
  siginfo_t si;
  waitid(P_PID, c, &si, WEXITED | WNOWAIT);
  long zombie = raw_fcntl(p[0], F_GETOWN, 0);
  waitpid(c, NULL, 0);
  long gone = raw_fcntl(p[0], F_GETOWN, 0);
  printf("owner: child %ld alive %s zombie %s reaped %ld, set reaped %ld\n", r, alive == c ? "same" : "other", zombie == c ? "same" : "other", gone, fc(p[0], F_SETOWN, c));
  // dup と fork でわける (開いたファイルのもの)
  fc(p[0], F_SETOWN, getpid());
  int d = dup(p[0]);
  long dd = raw_fcntl(d, F_GETOWN, 0);
  c = fork();
  if (c == 0)
    _exit(raw_fcntl(p[0], F_GETOWN, 0) == getppid() ? 0 : 1);
  int st;
  waitpid(c, &st, 0);
  int m = memfd_create("x", 0);
  long mr = fc(m, F_SETOWN, getpid());
  int op = open("/", O_PATH);
  printf("owner: dup %s, fork child %s, memfd %ld get %s, O_PATH %ld %ld\n", dd == getpid() ? "same" : "other", WEXITSTATUS(st) == 0 ? "same" : "other", mr, raw_fcntl(m, F_GETOWN, 0) == getpid() ? "same" : "other",
         fc(op, F_SETOWN, getpid()), fc(op, F_GETOWN_EX, (long)&ex));
  close(op);
  close(m);
  close(d);
  close(p[0]);
  close(p[1]);
}

static void sig(void) {
  int p[2];
  pipe(p);
  printf("sig: new %ld", fc(p[0], F_GETSIG, 0));
  long r = fc(p[0], F_SETSIG, SIGRTMIN + 1);
  printf(", set %ld get %s", r, fc(p[0], F_GETSIG, 0) == SIGRTMIN + 1 ? "RT+1" : "other");
  printf(", 64 %ld get %ld, 65 %ld, -1 %ld, 0 %ld get %ld, other end %ld\n", fc(p[0], F_SETSIG, 64), fc(p[0], F_GETSIG, 0), fc(p[0], F_SETSIG, 65), fc(p[0], F_SETSIG, -1), fc(p[0], F_SETSIG, 0), fc(p[0], F_GETSIG, 0),
         fc(p[1], F_GETSIG, 0));
  close(p[0]);
  close(p[1]);
}

static int kept(int fd) {
  long r = fc(fd, F_SETFL, fcntl(fd, F_GETFL) | O_ASYNC);
  if (r < 0)
    return (int)r;
  return (fcntl(fd, F_GETFL) & O_ASYNC) != 0;
}

static void flag(void) {
  int p[2], s[2];
  pipe(p);
  socketpair(AF_UNIX, SOCK_STREAM, 0, s);
  int m = memfd_create("x", 0), e = eventfd(0, 0), in = inotify_init1(0);
  printf("flag: pipe %d, memfd %d, eventfd %d, socket %d, inotify %d", kept(p[0]), kept(m), kept(e), kept(s[0]), kept(in));
  int on = 1, off = 0;
  int q[2];
  pipe(q);
  long r1 = ioctl(q[0], FIOASYNC, &on) < 0 ? -errno : 0;
  int k1 = (fcntl(q[0], F_GETFL) & O_ASYNC) != 0;
  long r2 = ioctl(q[0], FIOASYNC, &off) < 0 ? -errno : 0;
  int k2 = (fcntl(q[0], F_GETFL) & O_ASYNC) != 0;
  long r3 = ioctl(m, FIOASYNC, &on) < 0 ? -errno : 0;
  printf(", FIOASYNC pipe %ld kept %d off %ld kept %d memfd %ld", r1, k1, r2, k2, r3);
  fcntl(p[0], F_SETFL, fcntl(p[0], F_GETFL) & ~O_ASYNC);
  int n = open("/dev/null", O_RDONLY | O_ASYNC);
  printf(", setfl off %d, open O_ASYNC %d\n", (fcntl(p[0], F_GETFL) & O_ASYNC) != 0, (fcntl(n, F_GETFL) & O_ASYNC) != 0);
  close(n);
  close(p[0]);
  close(p[1]);
  close(q[0]);
  close(q[1]);
  close(s[0]);
  close(s[1]);
  close(m);
  close(e);
  close(in);
}

static void pipes(void) {
  int p[2];
  pipe(p);
  name(p[0], "r");
  name(p[1], "w");
  fcntl(p[0], F_SETOWN, getpid());
  fcntl(p[0], F_SETFL, O_ASYNC);
  char c = 'x';
  write(p[1], &c, 1);
  show("pipe: write");
  fcntl(p[0], F_SETSIG, SIGRTMIN + 1);
  write(p[1], &c, 1);
  show("pipe: setsig write");
  write(p[1], &c, 0);
  show("pipe: zero write");
  async_on(p[1], SIGRTMIN + 2);
  read(p[0], &c, 1);
  show("pipe: read");
  fcntl(p[0], F_SETFL, O_ASYNC | O_NONBLOCK);
  read(p[0], &c, 1);
  read(p[0], &c, 1);
  show("pipe: read until empty");
  fcntl(p[0], F_SETSIG, SIGIO);
  write(p[1], &c, 1);
  show("pipe: setsig SIGIO");
  fcntl(p[0], F_SETSIG, SIGBUS);
  write(p[1], &c, 1);
  show("pipe: setsig SIGBUS");
  fcntl(p[0], F_SETSIG, SIGRTMIN + 1);
  // ほかの持ち主 (いないプロセスのグループ) なら自分には来ない
  fcntl(p[0], F_SETOWN, 0);
  write(p[1], &c, 1);
  show("pipe: no owner");
  fcntl(p[0], F_SETOWN, getpid());
  close(p[1]);
  show("pipe: close writer");
  close(p[0]);
  // 書く側を閉じたあとの読む側の非同期 (書く人がいなくなっている)、dup してもとを閉じたもの
  pipe(p);
  name(p[0], "r");
  name(p[1], "w");
  async_on(p[0], SIGRTMIN + 1);
  int d = dup(p[0]);
  name(d, "dup");
  close(p[0]);
  write(p[1], &c, 1);
  show("pipe: dup and close the first");
  async_on(p[1], SIGRTMIN + 2);
  close(d);
  show("pipe: close the last reader");
  close(p[1]);
}

static void pgrp(void) {
  pid_t c = fork();
  if (c == 0) {
    setpgid(0, 0);
    int p[2], s[2];
    pipe(p);
    pipe(s);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN + 1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    pid_t g = fork();
    if (g == 0) {
      write(s[1], "r", 1);
      siginfo_t si;
      struct timespec t = {2, 0};
      int r = sigtimedwait(&set, &si, &t);
      _exit(r == SIGRTMIN + 1 && si.si_fd == p[0] && si.si_code == POLL_IN ? 0 : 1);
    }
    char b;
    read(s[0], &b, 1);
    fcntl(p[0], F_SETOWN, -getpgrp());
    fcntl(p[0], F_SETSIG, SIGRTMIN + 1);
    fcntl(p[0], F_SETFL, O_ASYNC);
    write(p[1], "x", 1);
    siginfo_t si;
    struct timespec t = {2, 0};
    int self = sigtimedwait(&set, &si, &t) == SIGRTMIN + 1;
    int st;
    waitpid(g, &st, 0);
    _exit(self | (WIFEXITED(st) && WEXITSTATUS(st) == 0) << 1);
  }
  int st;
  waitpid(c, &st, 0);
  printf("pgrp: self %d, member %d\n", WEXITSTATUS(st) & 1, WEXITSTATUS(st) >> 1 & 1);
}

static int tp[2], tfd;
static volatile pid_t ttid;
static volatile int tgot, tcode, tfdseen;
static void *thr(void *a) {
  (void)a;
  ttid = gettid();
  write(tp[1], "r", 1);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGRTMIN + 1);
  siginfo_t si;
  struct timespec t = {2, 0};
  tgot = sigtimedwait(&set, &si, &t) == SIGRTMIN + 1;
  tcode = si.si_code;
  tfdseen = si.si_fd;
  return NULL;
}

static void tid(void) {
  sigset_t set, old;
  sigemptyset(&set);
  sigaddset(&set, SIGRTMIN + 1);
  pthread_sigmask(SIG_BLOCK, &set, &old);
  pipe(tp);
  int p[2];
  pipe(p);
  tfd = p[0];
  pthread_t t;
  pthread_create(&t, NULL, thr, NULL);
  char b;
  read(tp[0], &b, 1);
  struct f_owner_ex ex = {F_OWNER_TID, ttid};
  fcntl(p[0], F_SETOWN_EX, &ex);
  fcntl(p[0], F_SETSIG, SIGRTMIN + 1);
  fcntl(p[0], F_SETFL, O_ASYNC);
  long get = raw_fcntl(p[0], F_GETOWN, 0);
  write(p[1], "x", 1);
  pthread_join(t, NULL);
  sigset_t pend;
  sigpending(&pend);
  printf("tid: get %s, thread got %d %s fd %s, main pending %d\n", get == ttid ? "tid" : "other", tgot, codename(tcode), tfdseen == p[0] ? "ok" : "other", sigismember(&pend, SIGRTMIN + 1));
  pthread_sigmask(SIG_SETMASK, &old, NULL);
  close(p[0]);
  close(p[1]);
  close(tp[0]);
  close(tp[1]);
}

static void query(void) {
  int a = open("/dev/null", O_RDONLY), b = dup(a), o = open("/dev/null", O_RDONLY);
  close(o);
  int o2 = open("/dev/null", O_RDONLY);
  printf("query: dupfd dup %ld, other %ld, self %ld, closed %ld", fc(a, F_DUPFD_QUERY, b), fc(a, F_DUPFD_QUERY, o2), fc(a, F_DUPFD_QUERY, a), fc(a, F_DUPFD_QUERY, 999));
  char path[] = "/tmp/fasyncXXXXXX";
  int f = mkstemp(path);
  int f2 = open(path, O_RDWR | O_CREAT, 0600), f3 = open(path, O_RDWR);
  unlink(path);
  int f4 = open(path, O_RDWR | O_CREAT, 0600);
  int p[2];
  pipe(p);
  printf(", created mkstemp %ld, O_CREAT existing %ld, open %ld, O_CREAT new %ld, pipe %ld\n", fc(f, F_CREATED_QUERY, 0), fc(f2, F_CREATED_QUERY, 0), fc(f3, F_CREATED_QUERY, 0), fc(f4, F_CREATED_QUERY, 0), fc(p[0], F_CREATED_QUERY, 0));
  unlink(path);
  uint64_t h = 77, set = 2, badh = 6;
  long r0 = fc(f, F_GET_RW_HINT, (long)&h);
  printf("query: rw hint new %ld %llu", r0, (unsigned long long)h);
  long r1 = fc(f, F_SET_RW_HINT, (long)&set);
  h = 77;
  fc(f3, F_GET_RW_HINT, (long)&h);
  printf(", set %ld other fd %llu", r1, (unsigned long long)h);
  printf(", bad %ld, file hint %ld, pipe %ld, fault %ld\n", fc(f, F_SET_RW_HINT, (long)&badh), fc(f, F_GET_FILE_RW_HINT, (long)&h), fc(p[0], F_SET_RW_HINT, (long)&set), fc(f, F_GET_RW_HINT, 8));
  close(p[0]);
  close(p[1]);
  close(f);
  close(f2);
  close(f3);
  close(f4);
  close(a);
  close(b);
  close(o2);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_sig;
  sa.sa_flags = SA_SIGINFO | SA_RESTART;
  int sigs[] = {SIGIO, SIGURG, SIGBUS, SIGRTMIN + 1, SIGRTMIN + 2, SIGRTMIN + 3};
  for (unsigned i = 0; i < sizeof sigs / sizeof *sigs; i++)
    sigaction(sigs[i], &sa, NULL);
  owner();
  sig();
  flag();
  pipes();
  pgrp();
  tid();
  query();
  printf("fasync: %d bad\n", bad);
  return bad;
}
