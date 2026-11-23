// O_ASYNC の知らせ (SIGIO、SIGURG) をソケット、端末、inotify が Linux と同じに出すか (test/fasync.c の続き):
//   unix    stream: 受けると POLL_IN、EAGAIN のあと読まれて空くと POLL_OUT (1 度)、相手の SHUT_WR で POLL_IN、相手が閉じると
//           POLL_HUP、MSG_OOB で SIGURG (持ち主がいれば O_ASYNC なしでも) と F_SETSIG のシグナルの POLL_PRI。dgram: 受けると
//           POLL_IN。FIOSETOWN / SIOCGPGRP
//   tcp     つながると listen のソケットに POLL_IN、つないだ側に POLL_OUT。受けると POLL_IN、MSG_OOB で SIGURG と POLL_PRI、
//           相手の SHUT_WR で POLL_IN、両方閉じると POLL_HUP
//   udp     受けると POLL_IN
//   tty     O_ASYNC で持ち主が自分に (F_SETOWN なしで)。疑似端末の親は子が書くと POLL_IN。子 (カノニカル) は改行で POLL_IN
//   inotify できごとで POLL_IN (まとめたものは知らせない)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static int bad;

// libc を通さない fcntl (musl の F_GETOWN は F_GETOWN_EX を使い、-4095〜-1 の答えは errno にしてしまう)
static long __attribute__((unused)) raw_fcntl(int fd, long cmd, long arg) {
  register long x8 __asm__("x8") = SYS_fcntl;
  register long x0 __asm__("x0") = fd;
  register long x1 __asm__("x1") = cmd;
  register long x2 __asm__("x2") = arg;
  __asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
  return x0;
}
static long __attribute__((unused)) fc(int fd, int cmd, long arg) {
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
  // 同時に来たものはシグナルの番号の順に届く (同じシグナルは来た順): 別々のシステムコールで来たものもそろえる
  for (int i = 1; i < n; i++)
    for (int j = i; j > 0 && ents[j - 1].signo > ents[j].signo; j--) {
      struct ent t = ents[j];
      ents[j] = ents[j - 1];
      ents[j - 1] = t;
    }
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


static void async_off(int fd) {
  fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) & ~O_ASYNC);
}

static void drain(int fd) {
  char b[4096];
  int fl = fcntl(fd, F_GETFL);
  fcntl(fd, F_SETFL, fl | O_NONBLOCK);
  while (read(fd, b, sizeof b) > 0)
    ;
  fcntl(fd, F_SETFL, fl);
}

static void unix_stream(void) {
  int s[2];
  socketpair(AF_UNIX, SOCK_STREAM, 0, s);
  name(s[0], "a");
  name(s[1], "b");
  async_on(s[0], SIGRTMIN + 1);
  async_on(s[1], SIGRTMIN + 2);
  write(s[1], "x", 1);
  show("unix stream: send");
  char c;
  read(s[0], &c, 1);
  show("unix stream: read");
  // いっぱいにする (受ける側の O_ASYNC は外して)
  async_off(s[0]);
  fcntl(s[1], F_SETFL, fcntl(s[1], F_GETFL) | O_NONBLOCK);
  static char big[65536];
  long r;
  int n = 0;
  while ((r = write(s[1], big, sizeof big)) > 0)
    n++;
  printf("unix stream: filled %s, eagain %d\n", n > 0 ? "yes" : "no", r < 0 && errno == EAGAIN);
  show("unix stream: fill");
  drain(s[0]);
  show("unix stream: drain");
  write(s[1], "y", 1);
  drain(s[0]);
  show("unix stream: send and drain again");
  // MSG_OOB: O_ASYNC なしでも持ち主に SIGURG
  send(s[1], "u", 1, MSG_OOB);
  show("unix stream: oob without O_ASYNC");
  drain(s[0]);
  fcntl(s[0], F_SETFL, fcntl(s[0], F_GETFL) | O_ASYNC);
  send(s[1], "v", 1, MSG_OOB);
  show("unix stream: oob");
  fcntl(s[0], F_SETSIG, 0);
  send(s[1], "w", 1, MSG_OOB);
  show("unix stream: oob without setsig");
  fcntl(s[0], F_SETSIG, SIGRTMIN + 1);
  fcntl(s[0], F_SETOWN, 0);
  async_off(s[0]);
  send(s[1], "z", 1, MSG_OOB);
  show("unix stream: oob without owner");
  async_on(s[0], SIGRTMIN + 1);
  drain(s[0]);
  nent = 0;
  shutdown(s[1], SHUT_WR);
  show("unix stream: peer shutdown write");
  close(s[1]);
  show("unix stream: peer close");
  close(s[0]);
}

static void unix_dgram(void) {
  int s[2];
  socketpair(AF_UNIX, SOCK_DGRAM, 0, s);
  name(s[0], "a");
  name(s[1], "b");
  async_on(s[0], SIGRTMIN + 1);
  send(s[1], "x", 1, 0);
  show("unix dgram: send");
  close(s[1]);
  show("unix dgram: peer close");
  close(s[0]);
  // FIOSETOWN と SIOCGPGRP (ソケットの ioctl)
  socketpair(AF_UNIX, SOCK_STREAM, 0, s);
  int me = getpid(), none = 4000000, got = 0;
  long r1 = ioctl(s[0], FIOSETOWN, &me) < 0 ? -errno : 0;
  long r2 = ioctl(s[0], SIOCGPGRP, &got) < 0 ? -errno : 0;
  long r3 = ioctl(s[0], FIOSETOWN, &none) < 0 ? -errno : 0;
  int got2 = -1;
  long r4 = ioctl(s[0], FIOGETOWN, &got2) < 0 ? -errno : 0;
  int p[2];
  pipe(p);
  long r5 = ioctl(p[0], FIOSETOWN, &me) < 0 ? -errno : 0;
  printf("unix ioctl: FIOSETOWN %ld, SIOCGPGRP %ld %s, none %ld, FIOGETOWN %ld %s, pipe %ld\n", r1, r2, got == me ? "same" : "other", r3, r4, got2 == me ? "same" : "other", r5);
  close(p[0]);
  close(p[1]);
  close(s[0]);
  close(s[1]);
}

static void tcp(void) {
  int l = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  socklen_t al = sizeof a;
  bind(l, (struct sockaddr *)&a, sizeof a);
  listen(l, 4);
  getsockname(l, (struct sockaddr *)&a, &al);
  name(l, "listen");
  async_on(l, SIGRTMIN + 1);
  int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
  name(c, "client");
  async_on(c, SIGRTMIN + 2);
  int r = connect(c, (struct sockaddr *)&a, sizeof a);
  printf("tcp: connect %s\n", r == 0 ? "done" : errno == EINPROGRESS ? "in progress" : "error");
  show("tcp: connect");
  int s = accept(l, NULL, NULL);
  name(s, "server");
  show("tcp: accept");
  async_on(s, SIGRTMIN + 3);
  write(c, "x", 1);
  show("tcp: send");
  drain(s);
  nent = 0;
  send(c, "u", 1, MSG_OOB);
  show("tcp: oob");
  char b;
  recv(s, &b, 1, MSG_OOB);
  drain(s);
  nent = 0;
  shutdown(c, SHUT_WR);
  show("tcp: peer shutdown write");
  close(s);
  show("tcp: close");
  close(c);
  close(l);
}

static void udp(void) {
  int r = socket(AF_INET, SOCK_DGRAM, 0), s = socket(AF_INET, SOCK_DGRAM, 0);
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  socklen_t al = sizeof a;
  bind(r, (struct sockaddr *)&a, sizeof a);
  getsockname(r, (struct sockaddr *)&a, &al);
  name(r, "udp");
  async_on(r, SIGRTMIN + 1);
  sendto(s, "x", 1, 0, (struct sockaddr *)&a, sizeof a);
  show("udp: send");
  close(r);
  close(s);
}

static void tty(void) {
  int m = posix_openpt(O_RDWR | O_NOCTTY);
  grantpt(m);
  unlockpt(m);
  int s = open(ptsname(m), O_RDWR | O_NOCTTY);
  name(m, "master");
  name(s, "slave");
  // F_SETOWN なしの O_ASYNC: 持ち主は自分に (端末の fasync)
  fcntl(m, F_SETFL, fcntl(m, F_GETFL) | O_ASYNC);
  long own = syscall(SYS_fcntl, m, F_GETOWN);
  printf("tty: O_ASYNC kept %d, owner %s\n", (fcntl(m, F_GETFL) & O_ASYNC) != 0, own == getpid() ? "self" : own == 0 ? "none" : "other");
  fcntl(m, F_SETSIG, SIGRTMIN + 1);
  write(s, "x", 1);
  show("tty: slave writes");
  drain(m);
  nent = 0;
  struct termios t;
  tcgetattr(s, &t);
  t.c_lflag &= ~(ECHO | ECHONL);
  tcsetattr(s, TCSANOW, &t);
  async_on(s, SIGRTMIN + 2);
  write(m, "ab", 2);
  show("tty: master writes without newline");
  write(m, "\n", 1);
  show("tty: master writes newline");
  drain(s);
  nent = 0;
  cfmakeraw(&t);
  tcsetattr(s, TCSANOW, &t);
  write(m, "z", 1);
  show("tty: master writes to raw");
  drain(s);
  close(s);
  close(m);
  nent = 0;
}

static void inot(void) {
  char dir[] = "/tmp/fasync2XXXXXX";
  mkdtemp(dir);
  int in = inotify_init1(IN_NONBLOCK);
  name(in, "inotify");
  async_on(in, SIGRTMIN + 1);
  inotify_add_watch(in, dir, IN_CREATE | IN_MODIFY);
  char path[64];
  snprintf(path, sizeof path, "%s/f", dir);
  int f = open(path, O_WRONLY | O_CREAT, 0600);
  show("inotify: create");
  drain(in);
  nent = 0;
  write(f, "x", 1);
  write(f, "y", 1);
  show("inotify: two writes");
  close(f);
  unlink(path);
  rmdir(dir);
  close(in);
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
  signal(SIGPIPE, SIG_IGN);
  int u = open("/dev/urandom", O_RDONLY);
  fcntl(u, F_SETFL, O_ASYNC);
  printf("urandom: O_ASYNC kept %d\n", (fcntl(u, F_GETFL) & O_ASYNC) != 0);
  close(u);
  unix_stream();
  unix_dgram();
  tcp();
  udp();
  tty();
  inot();
  printf("fasync2: %d bad\n", bad);
  return bad;
}
