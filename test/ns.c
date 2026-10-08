// namespace のテスト (カーネル): aios の中で動かす。作りかたは test/seccomp.c と同じ (zig cc -target aarch64-linux-musl -static)
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <stddef.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/un.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s (errno %d)\n", name, errno); fails++; } } while (0)
static int child(int (*fn)(void)) { pid_t p = fork(); if (p == 0) _exit(fn()); int st; waitpid(p, &st, 0); return WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st); }
static void link_of(const char *path, char *out) { ssize_t n = readlink(path, out, 63); out[n < 0 ? 0 : n] = 0; }

static int uts_noprivs(void) { return getuid() != 0 && unshare(CLONE_NEWUTS) == -1 && errno == EPERM ? 0 : 1; }
static int uts_new(void) {
  char before[64], after[64], self[64]; struct utsname u;
  link_of("/proc/self/ns/uts", before);
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  if (unshare(CLONE_NEWUTS)) return 2;
  link_of("/proc/self/ns/uts", after);
  if (strcmp(before, after) == 0 || strncmp(after, "uts:[", 5)) return 3;
  if (sethostname("box", 3)) return 4;
  uname(&u);
  if (strcmp(u.nodename, "box")) return 5;
  /* 子も同じ UTS */
  pid_t p = fork(); if (p == 0) { struct utsname c; uname(&c); _exit(strcmp(c.nodename, "box") ? 1 : 0); }
  int st; waitpid(p, &st, 0); if (WEXITSTATUS(st)) return 6;
  return 0;
}
static int uts_outside(void) { struct utsname u; uname(&u); return strcmp(u.nodename, "box") == 0 ? 1 : 0; }
static int uts_sethost_root_only(void) { return getuid() != 0 && sethostname("x", 1) == -1 && errno == EPERM ? 0 : 1; }
static int fn_clone(void *a) { struct utsname u; sethostname("cl", 2); uname(&u); return strcmp(u.nodename, "cl") ? 1 : 0; }
static int uts_clone(void) {
  static char stack[65536];
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  pid_t p = clone(fn_clone, stack + sizeof stack, CLONE_NEWUTS | SIGCHLD, 0);
  if (p < 0) return 2;
  int st; waitpid(p, &st, 0); if (!WIFEXITED(st) || WEXITSTATUS(st)) return 3;
  struct utsname u; uname(&u); return strcmp(u.nodename, "cl") == 0 ? 4 : 0;
}


static struct sockaddr_in in4(const char *ip, int port) { struct sockaddr_in a; memset(&a, 0, sizeof a); a.sin_family = AF_INET; a.sin_port = htons(port); inet_pton(AF_INET, ip, &a.sin_addr); return a; }
static int tcp_listen(const char *ip, int port) { int s = socket(AF_INET, SOCK_STREAM, 0); struct sockaddr_in a = in4(ip, port); int one = 1; setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one); if (bind(s, (void *)&a, sizeof a) || listen(s, 4)) return -1; return s; }
static int tcp_connect(const char *ip, int port) { int s = socket(AF_INET, SOCK_STREAM, 0); struct sockaddr_in a = in4(ip, port); return connect(s, (void *)&a, sizeof a) ? -errno : s; }
static int enter_net(void) { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0); return unshare(CLONE_NEWNET); }

static int net_noprivs(void) { return getuid() != 0 && unshare(CLONE_NEWNET) == -1 && errno == EPERM ? 0 : 1; }
/* 外で 127.0.0.1:7301 を待つ。中からはつながらない */
static int net_no_outside_lo(void) {
  int l = tcp_listen("127.0.0.1", 7301); if (l < 0) return 2;
  if (enter_net()) return 3;
  int r = tcp_connect("127.0.0.1", 7301);
  return r == -ECONNREFUSED ? 0 : 4;
}
/* 中どうしはつながる。名前は AF_INET の 127.0.0.1 */
static int net_inside(void) {
  if (enter_net()) return 2;
  int l = tcp_listen("127.0.0.1", 7302); if (l < 0) return 3;
  /* 外にも同じポートがあっても、中では別 (EADDRINUSE にならない) */
  pid_t p = fork();
  if (p == 0) { int c = tcp_connect("127.0.0.1", 7302); if (c < 0) _exit(1); write(c, "ping", 4); char b[8] = {0}; read(c, b, 4); _exit(strcmp(b, "pong") ? 2 : 0); }
  struct sockaddr_in peer; socklen_t pl = sizeof peer;
  int a = accept(l, (void *)&peer, &pl); if (a < 0) return 4;
  if (peer.sin_family != AF_INET || peer.sin_addr.s_addr != htonl(INADDR_LOOPBACK) || ntohs(peer.sin_port) == 0) return 5;
  struct sockaddr_in me; socklen_t ml = sizeof me; getsockname(a, (void *)&me, &ml);
  if (me.sin_family != AF_INET || ntohs(me.sin_port) != 7302) return 6;
  char b[8] = {0}; read(a, b, 4); if (strcmp(b, "ping")) return 7; write(a, "pong", 4);
  int st; waitpid(p, &st, 0); return WEXITSTATUS(st) ? 8 : 0;
}
static int net_no_eth(void) {
  if (enter_net()) return 2;
  if (tcp_connect("10.0.2.2", 80) != -ENETUNREACH) return 3;
  int u = socket(AF_INET, SOCK_DGRAM, 0); struct sockaddr_in d = in4("10.0.2.3", 53);
  if (sendto(u, "x", 1, 0, (void *)&d, sizeof d) != -1 || errno != ENETUNREACH) return 4;
  struct sockaddr_in l = in4("127.0.0.1", 53);
  if (sendto(u, "x", 1, 0, (void *)&l, sizeof l) != -1 || errno != ENETUNREACH) return 5;
  int s = socket(AF_INET, SOCK_STREAM, 0); struct sockaddr_in e = in4("10.0.2.15", 7303);
  if (bind(s, (void *)&e, sizeof e) != -1 || errno != EADDRNOTAVAIL) return 6;
  return 0;
}
/* 抽象名前空間の unix ソケットも分かれる */
static int net_abstract(void) {
  int l = socket(AF_UNIX, SOCK_STREAM, 0); struct sockaddr_un a; memset(&a, 0, sizeof a); a.sun_family = AF_UNIX; strcpy(a.sun_path + 1, "aios-ns-test");
  socklen_t len = offsetof(struct sockaddr_un, sun_path) + 1 + strlen("aios-ns-test");
  if (bind(l, (void *)&a, len) || listen(l, 1)) return 2;
  if (enter_net()) return 3;
  int c = socket(AF_UNIX, SOCK_STREAM, 0);
  return connect(c, (void *)&a, len) == -1 ? 0 : 4;
}
static int net_link(void) { char before[64], after[64]; link_of("/proc/self/ns/net", before); if (enter_net()) return 2; link_of("/proc/self/ns/net", after); return strcmp(before, after) && !strncmp(after, "net:[", 5) ? 0 : 3; }

int main(void) {
  CHECK("uts-needs-no-new-privs", child(uts_noprivs) == 0);
  CHECK("uts-unshare", child(uts_new) == 0);
  CHECK("uts-not-outside", child(uts_outside) == 0);
  CHECK("sethostname-root-only", child(uts_sethost_root_only) == 0);
  CHECK("uts-clone", child(uts_clone) == 0);
  CHECK("net-needs-no-new-privs", child(net_noprivs) == 0);
  CHECK("net-no-outside-loopback", child(net_no_outside_lo) == 0);
  CHECK("net-inside-tcp", child(net_inside) == 0);
  CHECK("net-no-eth-no-udp", child(net_no_eth) == 0);
  CHECK("net-abstract-unix", child(net_abstract) == 0);
  CHECK("net-ns-link", child(net_link) == 0);
  printf("ns: %d failed\n", fails);
  return fails != 0;
}
