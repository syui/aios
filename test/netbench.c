// ループバックの速さ (kernel.json の bench: socket、inet、unix を通るよくある使いかた)。aios の TCP / UDP / AF_UNIX は
// パケットを作らず、相手の受ける行列にじかに入れる (kernel/src/inet.rs、unix.rs) ので、TCP/IP の層を通る Linux と
// くらべてどれだけちがうかを見る。op ごとに 1 回あたりの µs (流しこみは MB/s) を、1 行 1 つの JSON で出す:
//   {"bench":"net","run":0,"op":"tcp_rr","n":5000,"ms":250.0,"us":50.00}
//   *_rr      1 バイトの往復を 2 つのプロセスで (書いて、相手が読んで書きかえし、それを読む)。pipe_rr はくらべるための
//             土台 (プロセスの切りかえと起こしだけ)。tcp_rr_epoll は読む前に epoll_wait で待つ (サーバーのよくある形)
//   *_local   同じ往復を 1 つのプロセスで (切りかえなし: ソケットの道だけ)
//   *_stream  64 KiB (4k は 4 KiB) ずつ書いて、別のプロセスが 64 KiB ずつ読む (MB/s)
//   tcp_connect  listen しているものへ connect して accept し、両方を閉じる (1 つのつながりの µs。TIME_WAIT も残る)
// aios でも Linux でも動く。aish の bench (kind net) が aios と Linux の VM で同じように動かす
//   netbench [-r RUNS] [-o OP,OP...] [SCALE]   RUNS 回くりかえす (run は 0 から)。SCALE (既定 1) で回数を何倍にも
// gcc -O2 -static -o netbench test/netbench.c
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int run, fails;
static const char *only;
static char buf[65536];

static double now_ms(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static int want(const char *op) {
  if (!only)
    return 1;
  size_t n = strlen(op);
  for (const char *p = only; (p = strstr(p, op)); p += n)
    if ((p == only || p[-1] == ',') && (p[n] == 0 || p[n] == ','))
      return 1;
  return 0;
}

static void report(const char *op, long n, double ms) {
  printf("{\"bench\":\"net\",\"run\":%d,\"op\":\"%s\",\"n\":%ld,\"ms\":%.1f,\"us\":%.2f}\n", run, op, n, ms, ms * 1e3 / n);
  fflush(stdout);
}

static void report_mb(const char *op, long bytes, double ms) {
  printf("{\"bench\":\"net\",\"run\":%d,\"op\":\"%s\",\"bytes\":%ld,\"ms\":%.1f,\"mb_s\":%.1f}\n", run, op, bytes, ms, bytes / 1048576.0 / (ms / 1e3));
  fflush(stdout);
}

static void bad(const char *op, const char *what) {
  fails++;
  printf("{\"bench\":\"net\",\"run\":%d,\"op\":\"%s\",\"error\":\"%s: %s\"}\n", run, op, what, strerror(errno));
  fflush(stdout);
}

static struct sockaddr_in lo(void) {
  struct sockaddr_in a;
  memset(&a, 0, sizeof a);
  a.sin_family = AF_INET;
  a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  return a;
}

static void nodelay(int s) {
  int one = 1;
  setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
}

// つながった TCP の 2 つ (127.0.0.1。TCP_NODELAY: 往復を Nagle で待たない)
static int tcp_pair(int sv[2]) {
  struct sockaddr_in a = lo();
  socklen_t l = sizeof a;
  int ls = socket(AF_INET, SOCK_STREAM, 0);
  if (ls < 0 || bind(ls, (struct sockaddr *)&a, sizeof a) || listen(ls, 1) || getsockname(ls, (struct sockaddr *)&a, &l))
    return -1;
  sv[0] = socket(AF_INET, SOCK_STREAM, 0);
  if (sv[0] < 0 || connect(sv[0], (struct sockaddr *)&a, sizeof a))
    return -1;
  sv[1] = accept(ls, NULL, NULL);
  close(ls);
  if (sv[1] < 0)
    return -1;
  nodelay(sv[0]);
  nodelay(sv[1]);
  return 0;
}

// たがいに connect した UDP の 2 つ
static int udp_pair(int sv[2]) {
  struct sockaddr_in a = lo(), b = lo();
  socklen_t l = sizeof a;
  sv[0] = socket(AF_INET, SOCK_DGRAM, 0);
  sv[1] = socket(AF_INET, SOCK_DGRAM, 0);
  if (sv[0] < 0 || sv[1] < 0 || bind(sv[0], (struct sockaddr *)&a, sizeof a) || bind(sv[1], (struct sockaddr *)&b, sizeof b))
    return -1;
  if (getsockname(sv[0], (struct sockaddr *)&a, &l) || (l = sizeof b, getsockname(sv[1], (struct sockaddr *)&b, &l)))
    return -1;
  return connect(sv[0], (struct sockaddr *)&b, sizeof b) || connect(sv[1], (struct sockaddr *)&a, sizeof a) ? -1 : 0;
}

static int pair(const char *kind, int sv[2]) {
  if (!strcmp(kind, "tcp"))
    return tcp_pair(sv);
  if (!strcmp(kind, "udp"))
    return udp_pair(sv);
  return socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
}

// 子は cr から 1 バイト読んで cw に書きかえす ('q' で終わり)。親は pw に書いて pr から読む。epoll なら読む前に待つ
static void rr(const char *op, int pw, int pr, int cr, int cw, long n, int use_epoll) {
  pid_t pid = fork();
  if (pid == 0) {
    int ep = -1;
    struct epoll_event ev = {.events = EPOLLIN}, out;
    if (use_epoll) {
      ep = epoll_create1(0);
      epoll_ctl(ep, EPOLL_CTL_ADD, cr, &ev);
    }
    for (;;) {
      char c;
      if (use_epoll && epoll_wait(ep, &out, 1, -1) != 1)
        _exit(1);
      if (read(cr, &c, 1) != 1 || c == 'q')
        _exit(0);
      if (write(cw, &c, 1) != 1)
        _exit(1);
    }
  }
  int ep = -1;
  struct epoll_event ev = {.events = EPOLLIN}, out;
  if (use_epoll) {
    ep = epoll_create1(0);
    epoll_ctl(ep, EPOLL_CTL_ADD, pr, &ev);
  }
  char c = 'x';
  double t = now_ms();
  long i;
  for (i = 0; i < n; i++) {
    if (write(pw, &c, 1) != 1 || (use_epoll && epoll_wait(ep, &out, 1, -1) != 1) || read(pr, &c, 1) != 1) {
      bad(op, "round trip");
      break;
    }
  }
  double ms = now_ms() - t;
  c = 'q';
  if (write(pw, &c, 1) != 1)
    kill(pid, SIGKILL);
  waitpid(pid, NULL, 0);
  if (ep >= 0)
    close(ep);
  if (i == n)
    report(op, n, ms);
}

static void sock_rr(const char *op, const char *kind, long n, int use_epoll) {
  int sv[2];
  if (pair(kind, sv)) {
    bad(op, "pair");
    return;
  }
  rr(op, sv[0], sv[0], sv[1], sv[1], n, use_epoll);
  close(sv[0]);
  close(sv[1]);
}

static void pipe_rr(long n) {
  int a[2], b[2];
  if (pipe(a) || pipe(b)) {
    bad("pipe_rr", "pipe");
    return;
  }
  rr("pipe_rr", a[1], b[0], a[0], b[1], n, 0);
  close(a[0]);
  close(a[1]);
  close(b[0]);
  close(b[1]);
}

// 1 つのプロセスで往復 (a に書いて b で読み、b に書いて a で読む)
static void local(const char *op, const char *kind, long n) {
  int sv[2];
  if (pair(kind, sv)) {
    bad(op, "pair");
    return;
  }
  char c = 'x';
  double t = now_ms();
  long i;
  for (i = 0; i < n; i++)
    if (write(sv[0], &c, 1) != 1 || read(sv[1], &c, 1) != 1 || write(sv[1], &c, 1) != 1 || read(sv[0], &c, 1) != 1) {
      bad(op, "round trip");
      break;
    }
  double ms = now_ms() - t;
  if (i == n)
    report(op, n, ms);
  close(sv[0]);
  close(sv[1]);
}

// 子が chunk バイトずつ total を書いて閉じ、親が 64 KiB ずつ読みきるまで
static void stream(const char *op, const char *kind, long total, long chunk) {
  int sv[2];
  if (pair(kind, sv)) {
    bad(op, "pair");
    return;
  }
  double t = now_ms();
  pid_t pid = fork();
  if (pid == 0) {
    close(sv[1]);
    for (long done = 0; done < total;) {
      long k = total - done < chunk ? total - done : chunk;
      ssize_t w = write(sv[0], buf, (size_t)k);
      if (w <= 0)
        _exit(1);
      done += w;
    }
    _exit(0);
  }
  close(sv[0]);
  long got = 0;
  for (;;) {
    ssize_t r = read(sv[1], buf, sizeof buf);
    if (r <= 0)
      break;
    got += r;
  }
  double ms = now_ms() - t;
  int st = 0;
  waitpid(pid, &st, 0);
  close(sv[1]);
  if (got != total || !WIFEXITED(st) || WEXITSTATUS(st)) {
    errno = EIO;
    bad(op, "short stream");
    return;
  }
  report_mb(op, total, ms);
}

static void tcp_connect(long n) {
  struct sockaddr_in a = lo();
  socklen_t l = sizeof a;
  int ls = socket(AF_INET, SOCK_STREAM, 0);
  if (ls < 0 || bind(ls, (struct sockaddr *)&a, sizeof a) || listen(ls, 128) || getsockname(ls, (struct sockaddr *)&a, &l)) {
    bad("tcp_connect", "listen");
    return;
  }
  double t = now_ms();
  long i;
  for (i = 0; i < n; i++) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (c < 0 || connect(c, (struct sockaddr *)&a, sizeof a)) {
      bad("tcp_connect", "connect");
      break;
    }
    int s = accept(ls, NULL, NULL);
    if (s < 0) {
      bad("tcp_connect", "accept");
      break;
    }
    close(c);
    close(s);
  }
  double ms = now_ms() - t;
  close(ls);
  if (i == n)
    report("tcp_connect", n, ms);
}

int main(int argc, char **argv) {
  int runs = 1;
  long scale = 1;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "-r") && i + 1 < argc)
      runs = atoi(argv[++i]);
    else if (!strcmp(argv[i], "-o") && i + 1 < argc)
      only = argv[++i];
    else
      scale = atol(argv[i]);
  }
  if (scale < 1)
    scale = 1;
  signal(SIGPIPE, SIG_IGN);
  memset(buf, 'n', sizeof buf);
  long nrr = 5000 * scale, nlocal = 10000 * scale, nconn = 1000 * scale, bytes = (32L << 20) * scale;
  double t0 = now_ms();
  for (run = 0; run < runs; run++) {
    if (want("pipe_rr"))
      pipe_rr(nrr);
    if (want("unix_rr"))
      sock_rr("unix_rr", "unix", nrr, 0);
    if (want("tcp_rr"))
      sock_rr("tcp_rr", "tcp", nrr, 0);
    if (want("udp_rr"))
      sock_rr("udp_rr", "udp", nrr, 0);
    if (want("tcp_rr_epoll"))
      sock_rr("tcp_rr_epoll", "tcp", nrr, 1);
    if (want("unix_local"))
      local("unix_local", "unix", nlocal);
    if (want("tcp_local"))
      local("tcp_local", "tcp", nlocal);
    if (want("udp_local"))
      local("udp_local", "udp", nlocal);
    if (want("unix_stream"))
      stream("unix_stream", "unix", bytes, 65536);
    if (want("tcp_stream"))
      stream("tcp_stream", "tcp", bytes, 65536);
    if (want("tcp_stream_4k"))
      stream("tcp_stream_4k", "tcp", bytes / 4, 4096);
    if (want("tcp_connect"))
      tcp_connect(nconn);
  }
  printf("{\"bench\":\"net\",\"total_ms\":%.1f,\"runs\":%d,\"scale\":%ld,\"fails\":%d}\n", now_ms() - t0, runs, scale, fails);
  return fails != 0;
}
