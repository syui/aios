// 0.0.0.0 (INADDR_ANY) で待つ TCP に、127.0.0.1 からつなぐ (サーバーのほとんどはこれ: node、Rust の serve、sshd)。
// accept したソケットの番地、送り受け、スレッドで accept するもの (Rust の std の serve と同じ形) も。IPv6 の :: も。
// test/ldiff.sh anybind で Linux とくらべる
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static const char *e(int r) { return r < 0 ? strerror(errno) : "0"; }

static int listen_on(int family, const char *ip, int port) {
  int s = socket(family, SOCK_STREAM, 0);
  int one = 1;
  setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  int r;
  if (family == AF_INET) {
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    r = bind(s, (void *)&a, sizeof a);
  } else {
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
    inet_pton(AF_INET6, ip, &a.sin6_addr);
    r = bind(s, (void *)&a, sizeof a);
  }
  printf("bind %s:%d %s\n", ip, port, e(r));
  printf("listen %s\n", e(listen(s, 8)));
  return s;
}

/* つないで、accept を待ち (2 秒)、ping / pong */
static void talk(int l, int family, const char *ip, int port) {
  int c = socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0);
  int r;
  if (family == AF_INET) {
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    r = connect(c, (void *)&a, sizeof a);
  } else {
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
    inet_pton(AF_INET6, ip, &a.sin6_addr);
    r = connect(c, (void *)&a, sizeof a);
  }
  printf("connect %s:%d %s\n", ip, port, r < 0 && errno == EINPROGRESS ? "EINPROGRESS" : e(r));
  struct pollfd p = {.fd = l, .events = POLLIN};
  int n = poll(&p, 1, 2000);
  printf("listener readable %d\n", n);
  if (n <= 0) {
    close(c);
    return;
  }
  struct sockaddr_storage peer;
  socklen_t pl = sizeof peer;
  int a = accept(l, (void *)&peer, &pl);
  printf("accept %s\n", e(a));
  if (a < 0) return;
  struct sockaddr_storage me;
  socklen_t ml = sizeof me;
  getsockname(a, (void *)&me, &ml);
  char buf[64];
  if (me.ss_family == AF_INET) inet_ntop(AF_INET, &((struct sockaddr_in *)&me)->sin_addr, buf, sizeof buf);
  else inet_ntop(AF_INET6, &((struct sockaddr_in6 *)&me)->sin6_addr, buf, sizeof buf);
  printf("accepted local %s port ok %d\n", buf, ntohs(((struct sockaddr_in *)&me)->sin_port) == port);
  struct pollfd w = {.fd = c, .events = POLLOUT};
  printf("client writable %d\n", poll(&w, 1, 2000));
  printf("send %zd\n", send(c, "ping", 4, 0));
  char b[8] = {0};
  printf("recv %zd %s\n", recv(a, b, 4, 0), b);
  printf("send back %zd\n", send(a, "pong", 4, 0));
  struct pollfd rr = {.fd = c, .events = POLLIN};
  poll(&rr, 1, 2000);
  memset(b, 0, sizeof b);
  printf("recv back %zd %s\n", recv(c, b, 4, 0), b);
  close(a);
  close(c);
}

/* スレッドで accept してブロックする (Rust の serve と同じ形) */
static int tl;
static void *acceptor(void *x) {
  (void)x;
  int a = accept(tl, NULL, NULL);
  if (a >= 0) {
    send(a, "hi", 2, 0);
    close(a);
  }
  return NULL;
}

int main(void) {
  int l = listen_on(AF_INET, "0.0.0.0", 7501);
  talk(l, AF_INET, "127.0.0.1", 7501);
  close(l);
  l = listen_on(AF_INET, "127.0.0.1", 7502);
  talk(l, AF_INET, "127.0.0.1", 7502);
  close(l);
  l = listen_on(AF_INET6, "::", 7503);
  talk(l, AF_INET, "127.0.0.1", 7503);
  talk(l, AF_INET6, "::1", 7503);
  close(l);
  /* ブロックする accept をスレッドで待っているところへ */
  tl = listen_on(AF_INET, "0.0.0.0", 7504);
  pthread_t t;
  pthread_create(&t, NULL, acceptor, NULL);
  usleep(200000);
  int c = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(7504)};
  inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
  struct timeval tv = {.tv_sec = 2};
  setsockopt(c, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
  printf("thread: connect %s\n", e(connect(c, (void *)&a, sizeof a)));
  char b[8] = {0};
  printf("thread: recv %zd %s\n", recv(c, b, 2, 0), b);
  pthread_join(t, NULL);
  return 0;
}
