// プロセスのつまみ: personality、タイマーのゆとり (timerslack)、oom_score_adj と、その /proc/PID のファイルが Linux と同じか
//   personality   0xffffffff で読むだけ、ADDR_NO_RANDOMIZE をつける、PER_LINUX32 (CPU に 32 ビットの EL0 があれば受けつける。
//                 QEMU の cortex-a72 と max はある。uname -m は aarch64 のまま)、
//                 /proc/self/personality (%08x)、fork で受けつぐ、exec で READ_IMPLIES_EXEC だけ外れる
//   timerslack    はじめ 50000、prctl の SET / GET、/proc/self/timerslack_ns を読み書き、0 で fork したときの値、
//                 数でなければ EINVAL、init のものは EPERM (CAP_SYS_NICE がない)
//   oom           oom_score_adj と oom_adj (-17..15 との換算)、範囲の外と数でないものは EINVAL、基数 0 (0x、0)、前後の空白、
//                 下げるのは EACCES (下限 0)、oom_score の大きさ (adj 0 で 600 台、1000 で 1300 台、init は 0)、fork で受けつぐ
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/personality.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

// ファイルを読んで、終わりの改行をのぞいた中身。だめなら -errno の文字
static const char *rd(const char *p) {
  static char b[8][128];
  static int k;
  char *s = b[k++ % 8];
  int fd = open(p, O_RDONLY);
  if (fd < 0) {
    sprintf(s, "open %d", -errno);
    return s;
  }
  ssize_t n = read(fd, s, 127);
  if (n < 0)
    sprintf(s, "read %d", -errno);
  else {
    s[n] = 0;
    if (n && s[n - 1] == '\n')
      s[n - 1] = 0;
  }
  close(fd);
  return s;
}

static int wr(const char *p, const char *v) {
  int fd = open(p, O_WRONLY);
  if (fd < 0)
    return -1000 - errno;
  ssize_t n = write(fd, v, strlen(v));
  int e = n < 0 ? -errno : (int)n;
  close(fd);
  return e;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc > 1 && !strcmp(argv[1], "exec")) {
    printf("exec: personality %#x, file %s\n", personality(0xffffffff), rd("/proc/self/personality"));
    return 0;
  }
  // personality
  {
    int a = personality(0xffffffff);
    int b = personality(ADDR_NO_RANDOMIZE | READ_IMPLIES_EXEC);
    int c = personality(0xffffffff);
    const char *f = rd("/proc/self/personality");
    errno = 0;
    int d = personality(PER_LINUX32);
    int de = errno;
    int e = personality(0xffffffff);
    struct utsname u;
    uname(&u);
    printf("personality (start %#x, set returns %#x, now %#x, file %s, PER_LINUX32 %#x errno %d, after %#x, machine %s)\n", a, b, c, f, d, de, e, u.machine);
    check("personality", a == 0 && b == 0 && c == (ADDR_NO_RANDOMIZE | READ_IMPLIES_EXEC) && d == c && de == 0 && e == PER_LINUX32);
    pid_t k = fork();
    if (k == 0) {
      printf("fork: personality %#x\n", personality(0xffffffff));
      execl("/proc/self/exe", argv[0], "exec", (char *)NULL);
      _exit(1);
    }
    waitpid(k, NULL, 0);
    personality(0);
    printf("init personality: %s\n", rd("/proc/1/personality"));
  }
  // timerslack
  {
    int a = prctl(PR_GET_TIMERSLACK);
    const char *f1 = rd("/proc/self/timerslack_ns");
    prctl(PR_SET_TIMERSLACK, 1000);
    int b = prctl(PR_GET_TIMERSLACK);
    const char *f2 = rd("/proc/self/timerslack_ns");
    int w1 = wr("/proc/self/timerslack_ns", "2000\n");
    int c = prctl(PR_GET_TIMERSLACK);
    int w2 = wr("/proc/self/timerslack_ns", "abc");
    int w3 = wr("/proc/self/timerslack_ns", "-5");
    pid_t k = fork();
    if (k == 0) {
      prctl(PR_SET_TIMERSLACK, 7);
      int x = prctl(PR_GET_TIMERSLACK);
      prctl(PR_SET_TIMERSLACK, 0);
      printf("fork: slack %d, set 7 %d, set 0 back to %d\n", c, x, prctl(PR_GET_TIMERSLACK));
      _exit(0);
    }
    waitpid(k, NULL, 0);
    int w4 = wr("/proc/self/timerslack_ns", "0");
    int d = prctl(PR_GET_TIMERSLACK);
    printf("timerslack (start %d file %s, prctl 1000: %d file %s, write 2000 %d: %d, abc %d, -5 %d, 0 %d: %d, init %s, write init %d)\n", a, f1, b, f2, w1, c, w2, w3, w4, d,
           rd("/proc/1/timerslack_ns"), wr("/proc/1/timerslack_ns", "5"));
    check("timerslack", a == 50000 && b == 1000 && c == 2000 && w2 == -EINVAL && w3 == -EINVAL && d == 50000);
  }
  // oom
  {
    const char *adj = "/proc/self/oom_score_adj", *old = "/proc/self/oom_adj";
    printf("oom start (adj %s, oom_adj %s, init adj %s, init score %s)\n", rd(adj), rd(old), rd("/proc/1/oom_score_adj"), rd("/proc/1/oom_score"));
    int s0 = atoi(rd("/proc/self/oom_score"));
    int w1 = wr(adj, "500");
    const char *o1 = rd(old);
    int w2 = wr(adj, "1000");
    const char *o2 = rd(old);
    int s1 = atoi(rd("/proc/self/oom_score"));
    int w3 = wr(adj, "1001"), w4 = wr(adj, "abc"), w5 = wr(adj, "");
    int w6 = wr(adj, " 0x10 \n");
    const char *a6 = rd(adj);
    int w7 = wr(adj, "010");
    const char *a7 = rd(adj);
    int w8 = wr(adj, "-1");
    int w9 = wr(adj, "0");
    int w10 = wr(old, "3");
    const char *a10 = rd(adj), *o10 = rd(old);
    int w11 = wr(old, "-17"), w12 = wr(old, "16"), w13 = wr(old, "15");
    const char *a13 = rd(adj);
    printf("oom (500 %d: oom_adj %s, 1000 %d: oom_adj %s, 1001 %d, abc %d, empty %d, 0x10 %d: %s, 010 %d: %s, -1 %d, 0 %d, oom_adj 3 %d: %s %s, -17 %d, 16 %d, 15 %d: %s)\n", w1, o1, w2, o2, w3,
           w4, w5, w6, a6, w7, a7, w8, w9, w10, a10, o10, w11, w12, w13, a13);
    printf("oom_score (adj 0 in 600..699 %d, adj 1000 in 1300..1399 %d)\n", s0 >= 600 && s0 < 700, s1 >= 1300 && s1 < 1400);
    check("oom", w3 == -EINVAL && w4 == -EINVAL && w5 == -EINVAL && w8 == -EACCES && w11 == -EACCES && w12 == -EINVAL && !strcmp(a13, "1000"));
    pid_t k = fork();
    if (k == 0) {
      printf("fork: adj %s, oom_adj %s\n", rd(adj), rd(old));
      _exit(0);
    }
    waitpid(k, NULL, 0);
    printf("write init adj %d\n", wr("/proc/1/oom_score_adj", "5"));
  }
  printf("procknobs: %d failed\n", fails);
  return fails != 0;
}
