// vmsplice が Linux と同じか: たしかめる順 (flags の EINVAL、fd の EBADF、iovec の EINVAL / EFAULT、長さ 0 は
// パイプでなくても 0、パイプでなければ EBADF)。書く口なら手もとのメモリをパイプへ: 入れ物の 1 ページの中のかけらごとに
// 箱ひとつ (16 こで EAGAIN、あとの write は箱に足さない)、あいた箱の分だけ入れる、とちゅうでさわれなければそこまで、
// 読み手がいなければ EPIPE と SIGPIPE、待つのは SPLICE_F_NONBLOCK がないときだけ (O_NONBLOCK は見ない)。
// 読む口ならパイプから手もとへ: 入っている分を入れ物に順に、空なら SPLICE_F_NONBLOCK で EAGAIN、書き手がいなければ 0、
// さわれない入れ物は EFAULT で中身は残る
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <unistd.h>

static long vs(int fd, const struct iovec *iov, unsigned long n, unsigned int fl) {
  long r = vmsplice(fd, iov, n, fl);
  return r < 0 ? -errno : r;
}
static long one(int fd, void *p, size_t len, unsigned int fl) {
  struct iovec v = {p, len};
  return vs(fd, &v, 1, fl);
}
static int inq(int fd) {
  int n = -1;
  ioctl(fd, FIONREAD, &n);
  return n;
}
static void drain(int fd) {
  char b[4096];
  int fl = fcntl(fd, F_GETFL);
  fcntl(fd, F_SETFL, fl | O_NONBLOCK);
  while (read(fd, b, sizeof b) > 0)
    ;
  fcntl(fd, F_SETFL, fl);
}
static void onalrm(int s) { (void)s; }
// 200 ms で SIGALRM (SA_RESTART なし): 待てば EINTR
static void alarm_soon(void) {
  struct itimerval t = {{0, 0}, {0, 200000}};
  setitimer(ITIMER_REAL, &t, NULL);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction sa = {0};
  sa.sa_handler = onalrm;
  sigaction(SIGALRM, &sa, NULL);
  signal(SIGPIPE, SIG_IGN);
  long pg = sysconf(_SC_PAGESIZE);
  char *m = mmap(NULL, 64 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  memset(m, 'x', 64 * pg);
  int p[2];
  if (pipe(p))
    return 1;
  int f = open("/proc/self/stat", O_RDONLY);
  int op = open("/", O_PATH);
  struct iovec z = {m, 0};

  // たしかめる順
  printf("checks: flags %ld, bad fd %ld, flags on good fd %ld, O_PATH %ld\n", one(-1, m, 1, 0x10), one(-1, m, 1, 0), one(p[1], m, 1, 0x10), one(op, m, 1, 0));
  printf("  no segs %ld, empty seg %ld, not a pipe (empty) %ld, not a pipe %ld\n", vs(p[1], NULL, 0, 0), vs(p[1], &z, 1, 0), vs(f, &z, 1, 0), one(f, m, 1, 0));
  struct iovec neg = {m, (size_t)-1};
  struct iovec two[2] = {{m, 1}, {m, (size_t)-1}};
  printf("  1025 segs %ld, bad iov ptr %ld, negative len %ld, negative 2nd %ld, bad base %ld, kernel base %ld\n", vs(p[1], (struct iovec *)m, 1025, 0),
         vs(p[1], (struct iovec *)8, 1, 0), vs(p[1], &neg, 1, 0), vs(p[1], two, 2, 0), one(p[1], (void *)8, 1, 0), one(p[1], (void *)0xffff000000000000ul, 1, 0));
  printf("  read end: flags %ld, bad buf (empty, nonblock) %ld\n", one(p[0], m, 1, 0x10), one(p[0], (void *)8, 1, SPLICE_F_NONBLOCK));

  // 書く口: 中身
  memcpy(m, "hello", 5);
  char b[64] = {0};
  printf("to pipe: %ld, queued %d, read %zd %.5s\n", one(p[1], m, 5, 0), inq(p[0]), read(p[0], b, sizeof b), b);
  // 箱は 1 つの入れ物の 1 ページの中のかけらごと: 1 バイトずつ 16 回で、いっぱい
  int i, ok = 0;
  for (i = 0; i < 16; i++)
    ok += one(p[1], m + i, 1, SPLICE_F_NONBLOCK) == 1;
  fcntl(p[1], F_SETFL, O_NONBLOCK);
  printf("16 one-byte: %d ok, 17th %ld, write after %zd (errno %d), queued %d\n", ok, one(p[1], m, 1, SPLICE_F_NONBLOCK), write(p[1], "y", 1), errno, inq(p[0]));
  // O_NONBLOCK でも、SPLICE_F_NONBLOCK がなければ待つ
  alarm_soon();
  printf("  full, O_NONBLOCK, flags 0: %ld\n", one(p[1], m, 1, 0));
  fcntl(p[1], F_SETFL, 0);
  drain(p[0]);
  // write のあとの vmsplice も箱に足さない
  ok = write(p[1], "a", 1) == 1;
  for (i = 0; i < 15; i++)
    ok += one(p[1], m, 1, SPLICE_F_NONBLOCK) == 1;
  printf("write then 15: %d ok, next %ld\n", ok, one(p[1], m, 1, SPLICE_F_NONBLOCK));
  drain(p[0]);
  // ページのとちゅうから 2 ページこえ (3 箱) を、あと 2 箱のところへ: 2 箱分だけ
  for (i = 0; i < 14; i++)
    one(p[1], m, 1, 0);
  printf("page pieces: %ld (want %ld), queued %d\n", one(p[1], m + 100, 2 * pg, SPLICE_F_NONBLOCK), 2 * pg - 100, inq(p[0]));
  drain(p[0]);
  // 同じページの 2 つの入れ物は 2 箱
  struct iovec same[2] = {{m, 10}, {m + 10, 10}};
  for (i = 0; i < 15; i++)
    one(p[1], m, 1, 0);
  printf("two segs in one page with 1 free: %ld\n", vs(p[1], same, 2, SPLICE_F_NONBLOCK));
  drain(p[0]);
  // 大きな入れ物: 箱 16 こ分
  printf("1 MiB aligned: %ld, queued %d\n", one(p[1], m, 64 * pg < (1 << 20) ? 64 * pg : (1 << 20), SPLICE_F_NONBLOCK), inq(p[0]));
  drain(p[0]);
  // いくつもの入れ物を順に (中身もそのまま)
  memcpy(m, "abc", 3);
  memcpy(m + 5 * pg, "defgh", 5);
  struct iovec segs[3] = {{m, 3}, {m + pg, 0}, {m + 5 * pg, 5}};
  memset(b, 0, sizeof b);
  printf("segs: %ld, read %zd %.8s\n", vs(p[1], segs, 3, 0), read(p[0], b, sizeof b), b);
  // とちゅうでさわれない: 前のページの分まで
  char *h = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  memset(h, 'h', 2 * pg);
  munmap(h + pg, pg);
  printf("fault midway: %ld, queued %d\n", one(p[1], h + pg - 96, 200, 0), inq(p[0]));
  drain(p[0]);
  printf("fault first: %ld\n", one(p[1], h + pg, 10, 0));
  // GIFT もうけつける
  printf("gift: %ld\n", one(p[1], m, 4, SPLICE_F_GIFT | SPLICE_F_MORE | SPLICE_F_MOVE));
  drain(p[0]);

  // 読む口
  write(p[1], "abcdef", 6);
  char r1[3], r2[10] = {0};
  struct iovec rv[2] = {{r1, 3}, {r2, 10}};
  printf("to user: %ld %.3s %.3s, queued %d\n", vs(p[0], rv, 2, 0), r1, r2, inq(p[0]));
  write(p[1], "123456", 6);
  printf("  short buf %ld, queued %d\n", one(p[0], b, 4, 0), inq(p[0]));
  printf("  bad buf %ld, queued %d\n", one(p[0], (void *)8, 4, 0), inq(p[0]));
  drain(p[0]);
  printf("  empty nonblock %ld\n", one(p[0], b, 4, SPLICE_F_NONBLOCK));
  fcntl(p[0], F_SETFL, O_NONBLOCK);
  alarm_soon();
  printf("  empty, O_NONBLOCK, flags 0: %ld\n", one(p[0], b, 4, 0));
  fcntl(p[0], F_SETFL, 0);
  // パイプの中身は、書いたときの順に (vmsplice と write をまぜても)
  memcpy(m, "AB", 2);
  one(p[1], m, 2, 0);
  write(p[1], "CD", 2);
  memset(b, 0, sizeof b);
  printf("  mixed: %ld %.4s\n", one(p[0], b, 8, 0), b);

  // 書き手がいない / 読み手がいない
  close(p[1]);
  printf("  no writers: %ld\n", one(p[0], b, 4, 0));
  int q[2];
  pipe(q);
  close(q[0]);
  sigset_t s, pend;
  sigemptyset(&s);
  sigaddset(&s, SIGPIPE);
  sigprocmask(SIG_BLOCK, &s, NULL);
  long r = one(q[1], m, 1, 0);
  sigpending(&pend);
  printf("no readers: %ld, SIGPIPE pending %d\n", r, sigismember(&pend, SIGPIPE));
  return 0;
}
