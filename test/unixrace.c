// AF_UNIX の stream を、大きなロックなしの read / write (カーネルの unix::fast_rw) と眠る道がまざっても、起こしそこねず、
// 中身も数もそろうか: aios の中で動かす (zig cc -target aarch64-linux-musl -O2 -static -o unixrace test/unixrace.c)。
// どれも別のプロセス (別の CPU) で、相手を待って眠る:
//   pingpong   1 バイトを 2 万回行き来する (読む人は眠り、書く人がロックなしで入れて起こす)
//   flood      4 KiB ずつ 16 MiB を書く (送る余地がなくなって眠る書き手を、ロックなしで読んだ人が起こす)。中身の並びも見る
//   poll       poll で待ってから読み書きする (poll で見ている人を起こす)
//   epoll      epoll_wait で待つ (epoll に入れたソケットを起こす)
//   threads    同じソケットに 2 つのスレッドが書く (fd の表を分けている: 門を通る道)。読む人は数をたしかめる
// 60 秒で止まったら (起こしそこね) しくじり。Linux でも同じに通る。1 行に ok NAME か FAIL NAME、おしまいに unixrace: N failed
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s\n", name); fails++; } } while (0)

static int full_write(int fd, const char *b, size_t n) {
  while (n > 0) {
    ssize_t r = write(fd, b, n);
    if (r < 0) {
      if (errno == EINTR) continue;
      return -1;
    }
    b += r;
    n -= r;
  }
  return 0;
}

// ---- pingpong ----
#define ROUNDS 20000
static int pong(int fd) {
  char c;
  for (int i = 0; i < ROUNDS; i++) {
    if (read(fd, &c, 1) != 1 || c != (char)i) return 1;
    c++;
    if (write(fd, &c, 1) != 1) return 2;
  }
  return 0;
}

static void pingpong(void) {
  int sv[2];
  socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
  pid_t pid = fork();
  if (pid == 0) {
    alarm(60);
    close(sv[0]);
    _exit(pong(sv[1]));
  }
  close(sv[1]);
  alarm(60);
  int ok = 1;
  for (int i = 0; i < ROUNDS && ok; i++) {
    char c = (char)i;
    if (write(sv[0], &c, 1) != 1 || read(sv[0], &c, 1) != 1 || c != (char)(i + 1)) ok = 0;
  }
  alarm(0);
  int ws;
  waitpid(pid, &ws, 0);
  close(sv[0]);
  CHECK("pingpong", ok && WIFEXITED(ws) && WEXITSTATUS(ws) == 0);
}

// ---- flood ----
#define FLOOD (16 << 20)
static int flood_reader(int fd) {
  char b[1000];
  size_t got = 0;
  unsigned char want = 0;
  while (got < FLOOD) {
    ssize_t r = read(fd, b, got % 7 == 0 ? 1 : sizeof b);
    if (r <= 0) return 1;
    for (ssize_t i = 0; i < r; i++, want++)
      if ((unsigned char)b[i] != (unsigned char)(want * 31 + (want >> 3))) return 2;
    got += r;
    // ときどき休む (書き手が送る余地をなくして眠るように)
    if (got % (1 << 20) < (size_t)r) usleep(2000);
  }
  return 0;
}

static void flood(void) {
  int sv[2];
  socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
  pid_t pid = fork();
  if (pid == 0) {
    alarm(60);
    close(sv[0]);
    _exit(flood_reader(sv[1]));
  }
  close(sv[1]);
  alarm(60);
  static char b[4096];
  unsigned char v = 0;
  int ok = 1;
  for (size_t sent = 0; sent < FLOOD && ok; sent += sizeof b) {
    for (size_t i = 0; i < sizeof b; i++, v++) b[i] = (char)(v * 31 + (v >> 3));
    if (full_write(sv[0], b, sizeof b) != 0) ok = 0;
  }
  alarm(0);
  int ws;
  waitpid(pid, &ws, 0);
  close(sv[0]);
  if (!ok || !WIFEXITED(ws) || WEXITSTATUS(ws) != 0) printf("  flood: writer %d, reader status %d\n", ok, WIFEXITED(ws) ? WEXITSTATUS(ws) : -1);
  CHECK("flood", ok && WIFEXITED(ws) && WEXITSTATUS(ws) == 0);
}

// ---- poll ----
static int wait_in(int fd) {
  struct pollfd p = {fd, POLLIN, 0};
  return poll(&p, 1, 20000) == 1 && (p.revents & POLLIN);
}

static int poll_pong(int fd) {
  char c;
  for (int i = 0; i < ROUNDS / 4; i++) {
    if (!wait_in(fd) || read(fd, &c, 1) != 1) return 1;
    if (write(fd, &c, 1) != 1) return 2;
  }
  return 0;
}

static void poll_test(void) {
  int sv[2];
  socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
  pid_t pid = fork();
  if (pid == 0) {
    alarm(60);
    close(sv[0]);
    _exit(poll_pong(sv[1]));
  }
  close(sv[1]);
  int ok = 1;
  for (int i = 0; i < ROUNDS / 4 && ok; i++) {
    char c = 'p';
    if (write(sv[0], &c, 1) != 1 || !wait_in(sv[0]) || read(sv[0], &c, 1) != 1) ok = 0;
  }
  int ws;
  waitpid(pid, &ws, 0);
  close(sv[0]);
  CHECK("poll", ok && WIFEXITED(ws) && WEXITSTATUS(ws) == 0);
}

// ---- epoll ----
static int epoll_pong(int fd) {
  int ep = epoll_create1(0);
  struct epoll_event e = {.events = EPOLLIN, .data.fd = fd};
  epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e);
  char c;
  for (int i = 0; i < ROUNDS / 4; i++) {
    struct epoll_event out;
    if (epoll_wait(ep, &out, 1, 20000) != 1 || read(fd, &c, 1) != 1) return 1;
    if (write(fd, &c, 1) != 1) return 2;
  }
  return 0;
}

static void epoll_test(void) {
  int sv[2];
  socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
  pid_t pid = fork();
  if (pid == 0) {
    alarm(60);
    close(sv[0]);
    _exit(epoll_pong(sv[1]));
  }
  close(sv[1]);
  int ep = epoll_create1(0);
  struct epoll_event e = {.events = EPOLLIN, .data.fd = sv[0]};
  epoll_ctl(ep, EPOLL_CTL_ADD, sv[0], &e);
  int ok = 1;
  for (int i = 0; i < ROUNDS / 4 && ok; i++) {
    char c = 'e';
    struct epoll_event out;
    if (write(sv[0], &c, 1) != 1 || epoll_wait(ep, &out, 1, 20000) != 1 || read(sv[0], &c, 1) != 1) ok = 0;
  }
  int ws;
  waitpid(pid, &ws, 0);
  close(ep);
  close(sv[0]);
  CHECK("epoll", ok && WIFEXITED(ws) && WEXITSTATUS(ws) == 0);
}

// ---- threads ----
#define PER_THREAD (2 << 20)
static int tfd;
static void *writer(void *arg) {
  char b[512];
  memset(b, (int)(long)arg, sizeof b);
  for (size_t s = 0; s < PER_THREAD; s += sizeof b)
    if (full_write(tfd, b, sizeof b) != 0) return (void *)1;
  return 0;
}

static int count_reader(int fd) {
  static char b[3000];
  size_t n[3] = {0, 0, 0};
  size_t total = 0;
  while (total < 2 * PER_THREAD) {
    ssize_t r = read(fd, b, sizeof b);
    if (r <= 0) return 1;
    for (ssize_t i = 0; i < r; i++) {
      if (b[i] != 1 && b[i] != 2) return 2;
      n[(int)b[i]]++;
    }
    total += r;
  }
  return n[1] == PER_THREAD && n[2] == PER_THREAD ? 0 : 3;
}

static void threads_test(void) {
  int sv[2];
  socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
  pid_t pid = fork();
  if (pid == 0) {
    alarm(60);
    close(sv[0]);
    _exit(count_reader(sv[1]));
  }
  close(sv[1]);
  tfd = sv[0];
  alarm(60);
  pthread_t a, b;
  pthread_create(&a, 0, writer, (void *)1);
  pthread_create(&b, 0, writer, (void *)2);
  void *ra, *rb;
  pthread_join(a, &ra);
  pthread_join(b, &rb);
  alarm(0);
  int ws;
  waitpid(pid, &ws, 0);
  close(sv[0]);
  CHECK("threads", !ra && !rb && WIFEXITED(ws) && WEXITSTATUS(ws) == 0);
}

int main(void) {
  setvbuf(stdout, 0, _IOLBF, 0);
  pingpong();
  flood();
  poll_test();
  epoll_test();
  threads_test();
  printf("unixrace: %d failed\n", fails);
  return fails != 0;
}
