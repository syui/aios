// TCP のソケットがつながりの表から外れたあと (Linux 7.4 の inet_unhash の unhashed_state) が Linux と同じか:
//   A-D  ノンブロッキングの connect が RST でしくじったものは、エラーを 2 度目の connect、SO_ERROR のどれで受けとっても、
//        受けとらなくても、もう listen できない (EINVAL)。E: もう一度 connect はできる
//   F-G  listen していたものを shutdown か AF_UNSPEC で外すと、もう connect できない (EINVAL)。listen はまたできる
//   H    つながったもの (相手が閉じた) も listen できない
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static const char *en(int r) { return r == 0 ? "0" : strerrorname_np(errno); }

static int mk(void) { return socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0); }

static int conn(int fd, int port) {
  struct sockaddr_in a = {AF_INET, htons(port), {htonl(INADDR_LOOPBACK)}, {0}};
  return connect(fd, (struct sockaddr *)&a, sizeof a);
}

static int myport(int fd) {
  struct sockaddr_in a;
  socklen_t l = sizeof a;
  getsockname(fd, (struct sockaddr *)&a, &l);
  return ntohs(a.sin_port);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // A: 2 度目の connect (自分のポート) でエラーを受けとってから listen
  int fd = mk();
  int r = conn(fd, 1);
  printf("A connect1 %s port %d\n", en(r), myport(fd) != 0);
  int p = myport(fd);
  usleep(10000);
  r = conn(fd, p);
  printf("A connect2 self %s\n", en(r));
  r = listen(fd, -1);
  printf("A listen %s\n", en(r));
  close(fd);
  // B: 2 度目の connect (ほかのポート)
  fd = mk();
  conn(fd, 1);
  usleep(10000);
  r = conn(fd, 2);
  printf("B connect2 other %s\n", en(r));
  r = listen(fd, -1);
  printf("B listen %s\n", en(r));
  close(fd);
  // C: SO_ERROR で受けとってから listen
  fd = mk();
  conn(fd, 1);
  usleep(10000);
  int e = 0;
  socklen_t l = sizeof e;
  getsockopt(fd, SOL_SOCKET, SO_ERROR, &e, &l);
  printf("C so_error %s\n", e ? strerrorname_np(e) : "0");
  r = listen(fd, -1);
  printf("C listen %s\n", en(r));
  close(fd);
  // D: なにも受けとらずに listen
  fd = mk();
  conn(fd, 1);
  usleep(10000);
  r = listen(fd, -1);
  printf("D listen %s\n", en(r));
  close(fd);
  // E: 2 度目の connect のあと 3 度目の connect
  fd = mk();
  conn(fd, 1);
  usleep(10000);
  r = conn(fd, 2);
  printf("E connect2 %s\n", en(r));
  r = conn(fd, 3);
  printf("E connect3 %s\n", en(r));
  close(fd);
  // F: listen してから shutdown で外し、connect と listen
  fd = mk();
  r = listen(fd, 5);
  printf("F listen %s\n", en(r));
  r = shutdown(fd, SHUT_RDWR);
  printf("F shutdown %s\n", en(r));
  r = conn(fd, 1);
  printf("F connect %s\n", en(r));
  r = listen(fd, 5);
  printf("F listen again %s\n", en(r));
  close(fd);
  // G: AF_UNSPEC で listen を外してから connect
  fd = mk();
  listen(fd, 5);
  struct sockaddr un = {AF_UNSPEC, {0}};
  r = connect(fd, &un, sizeof un);
  printf("G unspec %s\n", en(r));
  r = conn(fd, 1);
  printf("G connect %s\n", en(r));
  close(fd);
  // H: つながったものを相手が閉じてから listen
  int ls = mk();
  listen(ls, 5);
  int lp = myport(ls);
  fd = mk();
  conn(fd, lp);
  usleep(10000);
  struct sockaddr_in pa;
  socklen_t pl = sizeof pa;
  int c = accept(ls, (struct sockaddr *)&pa, &pl);
  close(c);
  usleep(10000);
  char b[4];
  printf("H read %ld\n", (long)read(fd, b, 4));
  r = listen(fd, 5);
  printf("H listen %s\n", en(r));
  close(fd);
  close(ls);
  return 0;
}
