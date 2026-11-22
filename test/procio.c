// /proc/PID の io、schedstat、wchan、coredump_filter (kernel/src/procfs.rs) が Linux と同じ答えになるか。
// 手本の Linux (test/linux.config) は TASK_IO_ACCOUNTING、TASK_DELAY_ACCT (SCHED_INFO)、KALLSYMS、ELF_CORE を入れて作る。
//   io         7 行の名前と mode 0400、read と write の回数とバイト (rchar、wchar、syscr、syscw: /dev/zero と /dev/null と
//              パイプで。しくじった read も syscr に数える)、DIR (ext4) の新しいファイルに書くと write_bytes、書き出す前に
//              切りつめると cancelled_write_bytes、wait で回収した子の分は親のプロセスの io に足される (Linux の
//              wait_task_zombie)、ほかの人のものは EACCES
//   schedstat  3 つの数。走ると 1 つ目 (sum_exec_runtime) が、眠って起きると 3 つ目 (pcount) が増える
//   wchan      走っているものは 0、nanosleep、wait4、パイプの read で眠っている子の名前
//   coredump_filter  既定 00000033、16 進と 10 進で書ける、下の 9 ビットだけ、fork と exec でうけつぐ、まちがいは EINVAL。
//              PR_SET_THP_DISABLE の印も exec でうけつぐ (自分を exec しなおして聞く)
// 出すだけのテスト: ./procio DIR (既定 .)。1 行に ok NAME か FAIL NAME、おしまいに procio: N failed
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails;
static char buf[1024];

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

struct io {
  unsigned long long v[7];
  int n;
};

// /proc/PID/io を 1 回の read で (read の前に作った答え)。読めなければ -errno
static int readio(const char *path, struct io *io, char *raw, size_t rawlen) {
  int fd = open(path, O_RDONLY);
  if (fd < 0)
    return -errno;
  char b[512];
  ssize_t n = read(fd, b, sizeof b - 1);
  int e = errno;
  close(fd);
  if (n < 0)
    return -e;
  b[n] = 0;
  if (raw)
    snprintf(raw, rawlen, "%s", b);
  static const char *names[7] = {"rchar", "wchar", "syscr", "syscw", "read_bytes", "write_bytes", "cancelled_write_bytes"};
  io->n = 0;
  char *p = b;
  for (int i = 0; i < 7; i++) {
    size_t l = strlen(names[i]);
    if (strncmp(p, names[i], l) || p[l] != ':' || p[l + 1] != ' ')
      return 0;
    io->v[i] = strtoull(p + l + 2, &p, 10);
    if (*p++ != '\n')
      return 0;
    io->n++;
  }
  return *p == 0 ? (int)n : 0;
}

static int readfile(const char *path, char *out, size_t len) {
  int fd = open(path, O_RDONLY);
  if (fd < 0)
    return -errno;
  ssize_t n = read(fd, out, len - 1);
  int e = errno;
  close(fd);
  if (n < 0)
    return -e;
  out[n] = 0;
  return (int)n;
}

static int writefile(const char *path, const char *s) {
  int fd = open(path, O_WRONLY);
  if (fd < 0)
    return -errno;
  ssize_t n = write(fd, s, strlen(s));
  int e = errno;
  close(fd);
  return n < 0 ? -e : (int)n;
}

static void spin(int ms) {
  struct timespec t0, t;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  do
    clock_gettime(CLOCK_MONOTONIC, &t);
  while ((t.tv_sec - t0.tv_sec) * 1000 + (t.tv_nsec - t0.tv_nsec) / 1000000 < ms);
}

// 子が眠りに入るまで待って、wchan を読む
static void wchan_of(pid_t p, char *out, size_t len) {
  char path[64], st[256];
  snprintf(path, sizeof path, "/proc/%d/stat", p);
  for (int i = 0; i < 200; i++) {
    if (readfile(path, st, sizeof st) > 0) {
      char *c = strrchr(st, ')');
      if (c && c[2] == 'S')
        break;
    }
    usleep(10000);
  }
  usleep(50000);
  snprintf(path, sizeof path, "/proc/%d/wchan", p);
  if (readfile(path, out, len) < 0)
    snprintf(out, len, "?");
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // exec しなおした子: coredump_filter と THP_DISABLE を出す
  if (argc > 1 && !strcmp(argv[1], "--report")) {
    char f[32] = "";
    readfile("/proc/self/coredump_filter", f, sizeof f);
    f[strcspn(f, "\n")] = 0;
    printf("%s %d", f, prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0));
    return 0;
  }
  const char *dir = argc > 1 ? argv[1] : ".";

  // io の形
  {
    struct io a;
    char raw[512];
    int r = readio("/proc/self/io", &a, raw, sizeof raw);
    struct stat st;
    int s = stat("/proc/self/io", &st);
    snprintf(buf, sizeof buf, "io has 7 lines (%d, %d) and mode %o", r, a.n, st.st_mode & 07777);
    check(buf, r > 0 && a.n == 7 && s == 0 && (st.st_mode & 07777) == 0400);
    int e = readio("/proc/1/io", &a, NULL, 0);
    snprintf(buf, sizeof buf, "io of another user's process (%d)", e);
    check(buf, getuid() == 0 || e == -EACCES);
  }

  // read と write の回数とバイト
  {
    struct io a, b;
    int na = readio("/proc/self/io", &a, NULL, 0);
    int z = open("/dev/zero", O_RDONLY), nul = open("/dev/null", O_WRONLY);
    char d[4096];
    long rd = 0, wr = 0;
    for (int i = 0; i < 5; i++)
      rd += read(z, d, 1000);
    for (int i = 0; i < 3; i++)
      wr += write(nul, d, 700);
    int pp[2];
    pipe(pp);
    wr += write(pp[1], "hello", 5);
    rd += read(pp[0], d, sizeof d);
    // しくじった read と write も数える (O_NONBLOCK の空のパイプ: EAGAIN)
    fcntl(pp[0], F_SETFL, O_NONBLOCK);
    int ra = (int)read(pp[0], d, 10);
    close(z);
    close(nul);
    close(pp[0]);
    close(pp[1]);
    int nb = readio("/proc/self/io", &b, NULL, 0);
    long dr = (long)(b.v[0] - a.v[0]), dw = (long)(b.v[1] - a.v[1]), sr = (long)(b.v[2] - a.v[2]), sw = (long)(b.v[3] - a.v[3]);
    snprintf(buf, sizeof buf, "rchar +%ld (want %ld), wchar +%ld (want %ld), syscr +%ld (want 8), syscw +%ld (want 4), EAGAIN %d", dr, rd + na, dw, wr, sr, sw,
             ra);
    check(buf, na > 0 && nb > 0 && ra == -1 && dr == rd + na && dw == wr && sr == 8 && sw == 4);
  }

  // ext4 の新しいファイル: 書くと write_bytes、書き出す前に切ると cancelled_write_bytes
  {
    char path[512];
    snprintf(path, sizeof path, "%s/.procio.%d", dir, getpid());
    struct io a, b, c;
    readio("/proc/self/io", &a, NULL, 0);
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    char d[8192];
    memset(d, 'x', sizeof d);
    ssize_t w = write(fd, d, sizeof d);
    readio("/proc/self/io", &b, NULL, 0);
    int t = ftruncate(fd, 0);
    readio("/proc/self/io", &c, NULL, 0);
    close(fd);
    unlink(path);
    long wb = (long)(b.v[5] - a.v[5]), cw = (long)(c.v[6] - b.v[6]);
    snprintf(buf, sizeof buf, "write_bytes +%ld after writing 8192 (%zd), cancelled_write_bytes +%ld after ftruncate (%d)", wb, w, cw, t);
    check(buf, w == 8192 && wb == 8192 && t == 0 && cw == 8192);
  }

  // 子の分: 終わって wait で回収したら、親のプロセスの io に足される (wait_task_zombie: 子の ioac と signal->ioac を親の
  // signal->ioac に)
  {
    struct io a, b;
    int na = readio("/proc/self/io", &a, NULL, 0);
    pid_t p = fork();
    if (p == 0) {
      int z = open("/dev/zero", O_RDONLY);
      char d[1000];
      for (int i = 0; i < 10; i++)
        read(z, d, sizeof d);
      _exit(0);
    }
    waitpid(p, NULL, 0);
    readio("/proc/self/io", &b, NULL, 0);
    snprintf(buf, sizeof buf, "a reaped child's reads are added to ours (rchar +%lld, syscr +%lld)", b.v[0] - a.v[0], b.v[2] - a.v[2]);
    check(buf, b.v[0] - a.v[0] == (unsigned long long)na + 10000 && b.v[2] - a.v[2] == 11);
  }

  // スレッドの io は task/TID に
  {
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d/io", getpid());
    struct io a;
    int r = readio(path, &a, NULL, 0);
    snprintf(buf, sizeof buf, "task/TID/io (%d)", r);
    check(buf, r > 0 && a.n == 7);
  }

  // schedstat
  {
    char s1[128], s2[128];
    unsigned long long r1 = 0, d1 = 0, c1 = 0, r2 = 0, d2 = 0, c2 = 0;
    int a = readfile("/proc/self/schedstat", s1, sizeof s1);
    spin(50);
    usleep(20000);
    int b = readfile("/proc/self/schedstat", s2, sizeof s2);
    int n1 = sscanf(s1, "%llu %llu %llu", &r1, &d1, &c1), n2 = sscanf(s2, "%llu %llu %llu", &r2, &d2, &c2);
    struct stat st;
    stat("/proc/self/schedstat", &st);
    snprintf(buf, sizeof buf, "schedstat (%d %d fields, run +%s, pcount +%s, mode %o)", n1, n2, r2 >= r1 + 40000000 ? ">=40ms" : "?", c2 > c1 ? ">0" : "0",
             st.st_mode & 07777);
    check(buf, a > 0 && b > 0 && n1 == 3 && n2 == 3 && r2 >= r1 + 40000000 && c2 > c1 && d2 >= d1 && (st.st_mode & 07777) == 0444);
  }

  // wchan
  {
    char self[64], w1[128], w2[128], w3[128];
    readfile("/proc/self/wchan", self, sizeof self);
    pid_t s = fork();
    if (s == 0) {
      sleep(30);
      _exit(0);
    }
    wchan_of(s, w1, sizeof w1);
    pid_t w = fork();
    if (w == 0) {
      pid_t c = fork();
      if (c == 0) {
        sleep(30);
        _exit(0);
      }
      waitpid(c, NULL, 0);
      _exit(0);
    }
    wchan_of(w, w2, sizeof w2);
    int pp[2];
    pipe(pp);
    pid_t r = fork();
    if (r == 0) {
      char c;
      read(pp[0], &c, 1);
      _exit(0);
    }
    wchan_of(r, w3, sizeof w3);
    kill(s, SIGKILL);
    kill(w, SIGKILL);
    kill(r, SIGKILL);
    waitpid(s, NULL, 0);
    waitpid(w, NULL, 0);
    waitpid(r, NULL, 0);
    close(pp[0]);
    close(pp[1]);
    snprintf(buf, sizeof buf, "wchan (running %s, nanosleep %s, wait4 %s, pipe read %s)", self, w1, w2, w3);
    check(buf, !strcmp(self, "0") && !strcmp(w1, "hrtimer_nanosleep") && !strcmp(w2, "do_wait") && !strcmp(w3, "anon_pipe_read"));
  }

  // coredump_filter
  {
    char a[32], b[32], c[32], d[32], e[32];
    readfile("/proc/self/coredump_filter", a, sizeof a);
    int w1 = writefile("/proc/self/coredump_filter", "0x7f");
    readfile("/proc/self/coredump_filter", b, sizeof b);
    int w2 = writefile("/proc/self/coredump_filter", "1023");
    readfile("/proc/self/coredump_filter", c, sizeof c);
    int w3 = writefile("/proc/self/coredump_filter", "junk"), w4 = writefile("/proc/self/coredump_filter", "-1");
    writefile("/proc/self/coredump_filter", "0x21");
    int pp[2];
    pipe(pp);
    pid_t p = fork();
    if (p == 0) {
      char x[32] = "";
      readfile("/proc/self/coredump_filter", x, sizeof x);
      write(pp[1], x, strlen(x));
      _exit(0);
    }
    close(pp[1]);
    waitpid(p, NULL, 0);
    ssize_t n = read(pp[0], d, sizeof d - 1);
    d[n > 0 ? n : 0] = 0;
    close(pp[0]);
    // exec したあとも (自分を --report で exec しなおして読む)。THP_DISABLE の印も
    int ts = prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0), tg = prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0);
    pipe(pp);
    p = fork();
    if (p == 0) {
      dup2(pp[1], 1);
      execl("/proc/self/exe", "procio", "--report", (char *)NULL);
      _exit(127);
    }
    close(pp[1]);
    waitpid(p, NULL, 0);
    n = read(pp[0], e, sizeof e - 1);
    e[n > 0 ? n : 0] = 0;
    close(pp[0]);
    struct stat st;
    stat("/proc/self/coredump_filter", &st);
    char want[32];
    snprintf(want, sizeof want, "00000021 %d", tg);
    snprintf(buf, sizeof buf, "coredump_filter (%.8s, 0x7f %d %.8s, 1023 %d %.8s, junk %d, -1 %d, fork %.8s, exec %s (THP_DISABLE %d %d), mode %o)", a, w1, b, w2, c, w3, w4,
             d, e, ts, tg, st.st_mode & 07777);
    check(buf, !strcmp(a, "00000033\n") && w1 == 4 && !strcmp(b, "0000007f\n") && w2 == 4 && !strcmp(c, "000001ff\n") && w3 == -EINVAL && w4 == -EINVAL &&
                   !strcmp(d, "00000021\n") && !strcmp(e, want) && (st.st_mode & 07777) == 0644);
  }

  printf("procio: %d failed\n", fails);
  return fails != 0;
}
