// epoll と poll の起こしのテスト (カーネル): aios の中で動かす。作りかたは test/seccomp.c と同じ
// (zig cc -target aarch64-linux-musl -O2 -static -o epoll-test test/epoll.c)。
// 眠っている epoll_wait / poll を、ほかのスレッドが eventfd、パイプ、socketpair に書いて起こす。
// 眠っているあいだに epoll_ctl で足したものでも起きる (眠るときの印に、その epoll 自身も入っている)
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s\n", name); fails++; } } while (0)

static int efd, pfd[2], sv[2], ep, late;
static const uint64_t one = 1;

static void *writer(void *a) {
  (void)a;
  usleep(200000); write(efd, &one, 8);
  usleep(200000); write(pfd[1], "x", 1);
  usleep(200000); write(sv[0], "y", 1);
  // 眠っている epoll に、もう読めるものを足す
  usleep(200000);
  struct epoll_event ev = {EPOLLIN, {.u64 = 4}};
  write(late, &one, 8);
  epoll_ctl(ep, EPOLL_CTL_ADD, late, &ev);
  return 0;
}

static void *poke(void *a) { usleep(200000); write(*(int *)a, &one, 8); return 0; }

/* 1 つ待って、その data */
static int wait1(void) {
  struct epoll_event o;
  int n = epoll_wait(ep, &o, 1, 3000);
  return n == 1 ? (int)o.data.u64 : -1;
}

int main(void) {
  efd = eventfd(0, EFD_NONBLOCK);
  late = eventfd(0, EFD_NONBLOCK);
  pipe(pfd);
  socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
  ep = epoll_create1(0);
  struct epoll_event ev = {EPOLLIN, {.u64 = 1}};
  epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev);
  ev.data.u64 = 2; epoll_ctl(ep, EPOLL_CTL_ADD, pfd[0], &ev);
  ev.data.u64 = 3; epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &ev);
  pthread_t t;
  pthread_create(&t, 0, writer, 0);
  char b[8];
  CHECK("epoll-eventfd", wait1() == 1); read(efd, b, 8);
  CHECK("epoll-pipe", wait1() == 2); read(pfd[0], b, 1);
  CHECK("epoll-socketpair", wait1() == 3); read(sv[1], b, 1);
  CHECK("epoll-ctl-while-waiting", wait1() == 4); read(late, b, 8);
  pthread_join(t, 0);
  // poll で眠っている eventfd
  int e = eventfd(0, 0);
  pthread_create(&t, 0, poke, &e);
  struct pollfd p = {e, POLLIN};
  CHECK("poll-eventfd", poll(&p, 1, 3000) == 1);
  pthread_join(t, 0);
  printf("epoll: %d failed\n", fails);
  return fails;
}
