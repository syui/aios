// ファイルのリース (F_SETLEASE / F_GETLEASE、F_SETDELEG / F_GETDELEG) が Linux と同じか:
//   set     読む口の F_RDLCK、ほかに開いていなければ F_WRLCK、書く口や二つ目の口なら EAGAIN、ディレクトリとパイプと
//           まちがった値は EINVAL、ないものの F_UNLCK は EAGAIN。つけると持ち主が自分になり O_ASYNC の印がつき、外すと
//           持ち主と F_SETSIG も消える
//   break   ほかのプロセスが開く: 読む口は読むリースとぶつからない。書く口 (O_NONBLOCK なら EWOULDBLOCK) はリースを
//           持つものに F_SETSIG のシグナル (POLL_MSG、si_fd) を送り、外すまで待つ。F_GETLEASE は外すべき型。書くリースに
//           読む口なら F_RDLCK に下げるまで待つ。truncate も書く口と同じ。閉じたらリースもなくなる
//   deleg   F_SETDELEG / F_GETDELEG (struct delegation)。d_flags があれば EINVAL
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef F_GETDELEG
#define F_GETDELEG 1039
#define F_SETDELEG 1040
#endif
struct delegation {
  uint32_t d_flags;
  uint16_t d_type;
  uint16_t pad;
};

static int bad;
static char path[64];

static long fc(int fd, int cmd, long arg) {
  long r = fcntl(fd, cmd, arg);
  return r < 0 ? -errno : r;
}

static const char *typ(long t) {
  return t == F_RDLCK ? "RDLCK" : t == F_WRLCK ? "WRLCK" : t == F_UNLCK ? "UNLCK" : "?";
}

static volatile int got, gotcode, gotfd;
static volatile long gotband;
static void on_sig(int s, siginfo_t *si, void *u) {
  (void)s;
  (void)u;
  got++;
  gotcode = si->si_code;
  gotband = si->si_band;
  gotfd = si->si_fd;
}

static void settle(void) {
  struct timespec t = {0, 30 * 1000 * 1000};
  while (nanosleep(&t, &t) < 0 && errno == EINTR)
    ;
}

// シグナルが来るまで (3 秒まで)
static void wait_sig(void) {
  for (int i = 0; i < 100 && !got; i++)
    settle();
  settle();
}

// リースをつけなおす (外すと F_SETSIG も消える)
static long relock(int fd, int t) {
  fc(fd, F_SETLEASE, F_UNLCK);
  fcntl(fd, F_SETSIG, SIGRTMIN + 1);
  return fc(fd, F_SETLEASE, t);
}

static void set(void) {
  int r = open(path, O_RDONLY);
  printf("set: new %s", typ(fc(r, F_GETLEASE, 0)));
  long e = fc(r, F_SETLEASE, F_RDLCK);
  printf(", read %ld get %s", e, typ(fc(r, F_GETLEASE, 0)));
  printf(", owner %s, O_ASYNC %d", syscall(SYS_fcntl, r, F_GETOWN) == getpid() ? "self" : "none", (fcntl(r, F_GETFL) & O_ASYNC) != 0);
  e = fc(r, F_SETLEASE, F_WRLCK);
  printf(", up to write %ld get %s", e, typ(fc(r, F_GETLEASE, 0)));
  fcntl(r, F_SETSIG, SIGRTMIN + 1);
  e = fc(r, F_SETLEASE, F_UNLCK);
  printf(", unlock %ld get %s owner %ld sig %ld O_ASYNC %d", e, typ(fc(r, F_GETLEASE, 0)), (long)syscall(SYS_fcntl, r, F_GETOWN), fc(r, F_GETSIG, 0), (fcntl(r, F_GETFL) & O_ASYNC) != 0);
  printf(", unlock again %ld\n", fc(r, F_SETLEASE, F_UNLCK));
  int r2 = open(path, O_RDONLY);
  printf("set: write with two opens %ld, read %ld", fc(r, F_SETLEASE, F_WRLCK), fc(r, F_SETLEASE, F_RDLCK));
  printf(", second read %ld", fc(r2, F_SETLEASE, F_RDLCK));
  printf(", both %s %s", typ(fc(r, F_GETLEASE, 0)), typ(fc(r2, F_GETLEASE, 0)));
  printf(", write on one of them %ld\n", fc(r2, F_SETLEASE, F_WRLCK));
  fc(r, F_SETLEASE, F_UNLCK);
  fc(r2, F_SETLEASE, F_UNLCK);
  close(r2);
  int w = open(path, O_RDWR);
  printf("set: read lease on a write open %ld, write %ld", fc(w, F_SETLEASE, F_RDLCK), fc(w, F_SETLEASE, F_WRLCK));
  printf(", other read while writer %ld\n", fc(r, F_SETLEASE, F_RDLCK));
  fc(w, F_SETLEASE, F_UNLCK);
  close(w);
  int d = open("/tmp", O_RDONLY | O_DIRECTORY);
  int p[2];
  pipe(p);
  printf("set: dir %ld, pipe %ld, bad 7 %ld, get dir %s\n", fc(d, F_SETLEASE, F_RDLCK), fc(p[0], F_SETLEASE, F_RDLCK), fc(r, F_SETLEASE, 7), typ(fc(d, F_GETLEASE, 0)));
  close(d);
  close(p[0]);
  close(p[1]);
  close(r);
}

// 子が flags で開いて、かかった時間 (ms) と答えを親に知らせる
static pid_t opener(int flags, int pfd) {
  pid_t c = fork();
  if (c == 0) {
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    int fd = flags < 0 ? truncate(path, 0) : open(path, flags);
    int e = fd < 0 ? errno : 0;
    clock_gettime(CLOCK_MONOTONIC, &b);
    long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
    char buf[64];
    int n = snprintf(buf, sizeof buf, "%d %ld", e, ms);
    write(pfd, buf, n);
    _exit(0);
  }
  return c;
}

static void result(int pfd, pid_t c, int *err, long *ms) {
  char buf[64] = {0};
  read(pfd, buf, sizeof buf - 1);
  waitpid(c, NULL, 0);
  sscanf(buf, "%d %ld", err, ms);
}

static void breaks(void) {
  int p[2];
  pipe(p);
  int r = open(path, O_RDONLY);
  fcntl(r, F_SETSIG, SIGRTMIN + 1);
  fc(r, F_SETLEASE, F_RDLCK);
  got = 0;
  int err;
  long ms;
  pid_t c = opener(O_RDONLY, p[1]);
  result(p[0], c, &err, &ms);
  settle();
  printf("break: read open against read lease: err %d, signal %d, lease %s\n", err, got, typ(fc(r, F_GETLEASE, 0)));
  c = opener(O_WRONLY | O_NONBLOCK, p[1]);
  result(p[0], c, &err, &ms);
  settle();
  printf("break: nonblock write open: err %s, signal %d code %d band %#lx fd %s, lease %s\n", err == EWOULDBLOCK ? "EWOULDBLOCK" : strerror(err), got, gotcode, gotband, gotfd == r ? "ok" : "other",
         typ(fc(r, F_GETLEASE, 0)));
  // ブロックする書く口: 親がシグナルを受けて外すまで待つ (外すのを待っているリースには 2 度は知らせないので、つけなおす)
  relock(r, F_RDLCK);
  got = 0;
  c = opener(O_WRONLY, p[1]);
  wait_sig();
  printf("break: blocked write open: signal %d, lease %s", got, typ(fc(r, F_GETLEASE, 0)));
  printf(", unlock %ld", fc(r, F_SETLEASE, F_UNLCK));
  result(p[0], c, &err, &ms);
  printf(", opener err %d waited %s\n", err, ms >= 20 && ms < 5000 ? "a while" : "no");
  // 書くリースと読む口: 下げるまで待つ
  long e = relock(r, F_WRLCK);
  got = 0;
  c = opener(O_RDONLY, p[1]);
  wait_sig();
  printf("break: write lease and read open: set %ld, signal %d, lease %s", e, got, typ(fc(r, F_GETLEASE, 0)));
  printf(", downgrade %ld get %s", fc(r, F_SETLEASE, F_RDLCK), typ(fc(r, F_GETLEASE, 0)));
  result(p[0], c, &err, &ms);
  printf(", opener err %d waited %s\n", err, ms >= 20 && ms < 5000 ? "a while" : "no");
  // truncate も書く口と同じ
  relock(r, F_RDLCK);
  got = 0;
  c = opener(-1, p[1]);
  wait_sig();
  printf("break: truncate: signal %d, lease %s", got, typ(fc(r, F_GETLEASE, 0)));
  fc(r, F_SETLEASE, F_UNLCK);
  result(p[0], c, &err, &ms);
  printf(", truncate err %d\n", err);
  // 閉じたらリースはない: 書く口はすぐ開ける
  fcntl(r, F_SETSIG, SIGRTMIN + 1);
  fc(r, F_SETLEASE, F_RDLCK);
  close(r);
  got = 0;
  c = opener(O_WRONLY | O_NONBLOCK, p[1]);
  result(p[0], c, &err, &ms);
  printf("break: after close: err %d, signal %d\n", err, got);
  // 自分のプロセスが書く口で開いても、自分のリースをこわす
  r = open(path, O_RDONLY);
  fcntl(r, F_SETSIG, SIGRTMIN + 1);
  fc(r, F_SETLEASE, F_RDLCK);
  got = 0;
  int w = open(path, O_WRONLY | O_NONBLOCK);
  settle();
  printf("break: own write open: %s, signal %d, lease %s\n", w < 0 ? (errno == EWOULDBLOCK ? "EWOULDBLOCK" : strerror(errno)) : "opened", got, typ(fc(r, F_GETLEASE, 0)));
  if (w >= 0)
    close(w);
  fc(r, F_SETLEASE, F_UNLCK);
  close(r);
  close(p[0]);
  close(p[1]);
}

static void deleg(void) {
  int r = open(path, O_RDONLY);
  struct delegation d = {0, F_RDLCK, 0};
  long e = fc(r, F_SETDELEG, (long)&d);
  struct delegation g = {0, 99, 0};
  long e2 = fc(r, F_GETDELEG, (long)&g);
  printf("deleg: set %ld, get %ld %s, lease %s", e, e2, typ(g.d_type), typ(fc(r, F_GETLEASE, 0)));
  struct delegation bf = {1, F_RDLCK, 0};
  printf(", flags %ld", fc(r, F_SETDELEG, (long)&bf));
  struct delegation u = {0, F_UNLCK, 0};
  printf(", unlock %ld", fc(r, F_SETDELEG, (long)&u));
  g.d_type = 99;
  fc(r, F_GETDELEG, (long)&g);
  printf(" get %s, get flags %ld\n", typ(g.d_type), (g.d_flags = 1, fc(r, F_GETDELEG, (long)&g)));
  close(r);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_sig;
  sa.sa_flags = SA_SIGINFO | SA_RESTART;
  sigaction(SIGRTMIN + 1, &sa, NULL);
  sigaction(SIGIO, &sa, NULL);
  snprintf(path, sizeof path, "/tmp/leaseXXXXXX");
  close(mkstemp(path));
  FILE *f = fopen("/proc/sys/fs/lease-break-time", "r");
  int bt = -1, en = -1;
  if (f) {
    fscanf(f, "%d", &bt);
    fclose(f);
  }
  f = fopen("/proc/sys/fs/leases-enable", "r");
  if (f) {
    fscanf(f, "%d", &en);
    fclose(f);
  }
  printf("sysctl: lease-break-time %d, leases-enable %d\n", bt, en);
  set();
  breaks();
  deleg();
  unlink(path);
  printf("lease: %d bad\n", bad);
  return bad;
}
