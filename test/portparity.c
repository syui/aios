// 自分で選ぶポートの偶奇 (Linux: TCP の bind(0) と listen は奇数を先に、connect は偶数を先に。UDP は見ない)。
// bind(0) で取って閉じたポートへの connect が、自分に同じポートを選んで自分につながらない (self-connect) こと
// (CI の test/inetsock.c の「connect to a closed port」がまれに通らなかった)
// test/ldiff.sh portparity で Linux とくらべる
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static struct sockaddr_in lo(int port) {
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  return a;
}

static int port_of(int s) {
  struct sockaddr_in a;
  socklen_t l = sizeof a;
  getsockname(s, (struct sockaddr *)&a, &l);
  return ntohs(a.sin_port);
}

int main(void) {
  int bad = 0;
  // TCP の bind(0)
  int odd = 0, n = 50;
  for (int i = 0; i < n; i++) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(0);
    bind(s, (struct sockaddr *)&a, sizeof a);
    odd += port_of(s) & 1;
    close(s);
  }
  printf("tcp bind(0): odd %d of %d\n", odd, n);
  bad |= odd != n;
  // TCP の listen (bind なし)
  odd = 0;
  for (int i = 0; i < n; i++) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    listen(s, 1);
    odd += port_of(s) & 1;
    close(s);
  }
  printf("tcp listen: odd %d of %d\n", odd, n);
  bad |= odd != n;
  // TCP の connect
  int l = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in la = lo(0);
  bind(l, (struct sockaddr *)&la, sizeof la);
  listen(l, 128);
  int lp = port_of(l), even = 0;
  for (int i = 0; i < n; i++) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(lp);
    connect(c, (struct sockaddr *)&a, sizeof a);
    even += !(port_of(c) & 1);
    int x = accept(l, NULL, NULL);
    close(x);
    close(c);
  }
  close(l);
  printf("tcp connect: even %d of %d\n", even, n);
  bad |= even != n;
  // 閉じたばかりのポートへの connect は、いつも ECONNREFUSED (自分につながらない)
  int refused = 0, tries = 2000;
  for (int i = 0; i < tries; i++) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(0);
    bind(s, (struct sockaddr *)&a, sizeof a);
    int p = port_of(s);
    close(s);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    a = lo(p);
    if (connect(c, (struct sockaddr *)&a, sizeof a) < 0 && errno == ECONNREFUSED) refused++;
    close(c);
  }
  printf("connect to a closed port: refused %d of %d\n", refused, tries);
  bad |= refused != tries;
  // UDP は偶奇を見ない (どちらも出る)
  odd = 0;
  for (int i = 0; i < n; i++) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = lo(0);
    bind(s, (struct sockaddr *)&a, sizeof a);
    odd += port_of(s) & 1;
    close(s);
  }
  printf("udp bind(0): both %d\n", odd > 0 && odd < n);
  return bad;
}
