// rlimit (getrlimit、setrlimit、prlimit64) と /proc/PID/limits が Linux と同じ答えか:
//   defaults   はじめの値 (NOFILE と CORE と STACK などは Linux の INIT_RLIMITS。init が変えたものは、下げられるものは
//              Linux の値に下げてから見る)。NPROC と SIGPENDING はメモリで決まる (max_threads / 2) ので、同じ数かだけ
//   limits     /proc/self/limits の形 (見出し、名前、単位、unlimited) と、getrlimit と同じ数か
//   set        (RLIMIT_RTTIME で。FSIZE を下げると出力のファイルが SIGXFSZ になる) ソフトをハードより上 (EINVAL)、ハードを上げる (EPERM)、NOFILE の nr_open より上 (EPERM)、ない番号 (EINVAL)、
//              INFINITY、下げてから上げる
//   prlimit    自分 (0 と getpid)、子 (同じ人: 読める、書ける。子の /proc/PID/limits にも出る)、init (EPERM)、ない pid (ESRCH)、
//              new の EFAULT (何も変えない)、old の EFAULT (new は変わる)
//   inherit    fork と exec で受けつぐ、スレッドで変えたものはプロセスのもの
//   nofile     ソフトを 10 にすると open は EMFILE、dup2 で 20 へは EBADF
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static long prl(pid_t pid, int res, const void *new, void *old) {
  long r = syscall(SYS_prlimit64, pid, res, new, old);
  return r < 0 ? -errno : r;
}

static const char *val(uint64_t v, char *b) {
  if (v == RLIM_INFINITY)
    return "inf";
  sprintf(b, "%llu", (unsigned long long)v);
  return b;
}

// /proc/PID/limits を、NPROC と SIGPENDING の数を N にして出す (getrlimit と同じなら)
static void show_limits(pid_t pid, const char *tag) {
  char p[64];
  snprintf(p, sizeof p, "/proc/%d/limits", pid);
  FILE *f = fopen(p, "r");
  if (!f) {
    printf("%s: %s\n", p, strerror(errno));
    return;
  }
  char line[256];
  int n = 0;
  while (fgets(line, sizeof line, f)) {
    if (!strncmp(line, "Max processes", 13) || !strncmp(line, "Max pending signals", 19)) {
      // 26 文字目からソフト、47 文字目からハード (どちらも左寄せで 20 文字)
      struct rlimit r;
      getrlimit(!strncmp(line, "Max processes", 13) ? RLIMIT_NPROC : RLIMIT_SIGPENDING, &r);
      unsigned long long s = strtoull(line + 26, NULL, 10), h = strtoull(line + 47, NULL, 10);
      printf("%s: %.26sN(%s) N(%s) %s", tag, line, s == r.rlim_cur ? "same" : "differs", h == r.rlim_max ? "same" : "differs", line + 68);
    } else
      printf("%s: %s", tag, line);
    n++;
  }
  fclose(f);
  printf("%s: %d lines\n", tag, n);
}

static void *thr(void *a) {
  (void)a;
  struct rlimit r = {40, 60};
  setrlimit(RLIMIT_MSGQUEUE, &r);
  return NULL;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc > 1 && !strcmp(argv[1], "exec")) {
    struct rlimit r;
    getrlimit(RLIMIT_LOCKS, &r);
    printf("exec: LOCKS %llu %llu\n", (unsigned long long)r.rlim_cur, (unsigned long long)r.rlim_max);
    return 0;
  }
  // init (systemd なら NOFILE 1024:524288) で変わっているものは、下げられれば Linux のはじめの値に
  {
    struct rlimit d[] = {
        [RLIMIT_NOFILE] = {1024, 4096},
        [RLIMIT_CORE] = {0, RLIM_INFINITY},
        [RLIMIT_STACK] = {8 << 20, RLIM_INFINITY},
        [RLIMIT_MEMLOCK] = {8 << 20, 8 << 20},
    };
    int res[] = {RLIMIT_NOFILE, RLIMIT_CORE, RLIMIT_STACK, RLIMIT_MEMLOCK};
    for (unsigned i = 0; i < sizeof res / sizeof *res; i++) {
      struct rlimit c;
      getrlimit(res[i], &c);
      if (c.rlim_max >= d[res[i]].rlim_max)
        setrlimit(res[i], &d[res[i]]);
    }
  }
  // defaults
  {
    char s[256] = "", a[32], b[32];
    struct rlimit np, sp;
    for (int i = 0; i < 16; i++) {
      struct rlimit r;
      int e = getrlimit(i, &r) ? -errno : 0;
      if (i == RLIMIT_NPROC)
        np = r;
      if (i == RLIMIT_SIGPENDING)
        sp = r;
      if (i == RLIMIT_NPROC || i == RLIMIT_SIGPENDING)
        snprintf(s + strlen(s), sizeof s - strlen(s), " %d:N", i);
      else
        snprintf(s + strlen(s), sizeof s - strlen(s), " %d:%s/%s%s", i, val(r.rlim_cur, a), val(r.rlim_max, b), e ? "!" : "");
    }
    struct rlimit x;
    int e16 = getrlimit(16, &x) ? -errno : 0;
    printf("defaults:%s, 16 %d\n", s, e16);
    check("NPROC and SIGPENDING (the same nonzero, soft = hard)", np.rlim_cur == sp.rlim_cur && np.rlim_cur > 0 && np.rlim_cur == np.rlim_max && sp.rlim_cur == sp.rlim_max);
  }
  show_limits(getpid(), "limits");
  // set
  {
    struct rlimit r = {RLIM_INFINITY, 100}, c;
    int e1 = setrlimit(RLIMIT_RTTIME, &r) ? -errno : 0;
    r = (struct rlimit){100, 200};
    int e2 = setrlimit(RLIMIT_RTTIME, &r) ? -errno : 0;
    r = (struct rlimit){100, 300};
    int e3 = setrlimit(RLIMIT_RTTIME, &r) ? -errno : 0;
    getrlimit(RLIMIT_RTTIME, &c);
    r = (struct rlimit){1024, 2000000};
    int e4 = setrlimit(RLIMIT_NOFILE, &r) ? -errno : 0;
    r = (struct rlimit){1, 1};
    int e5 = setrlimit(16, &r) ? -errno : 0, e6 = setrlimit(100, &r) ? -errno : 0;
    r = (struct rlimit){RLIM_INFINITY, RLIM_INFINITY};
    int e7 = setrlimit(RLIMIT_DATA, &r) ? -errno : 0;
    r = (struct rlimit){0, 0};
    int e8 = setrlimit(RLIMIT_CORE, &r) ? -errno : 0;
    r = (struct rlimit){0, 10};
    int e9 = setrlimit(RLIMIT_CORE, &r) ? -errno : 0;
    printf("set (soft > hard %d, lower hard %d, raise it again %d: %llu %llu, NOFILE above nr_open %d, 16 %d, 100 %d, INFINITY %d, CORE 0 0 %d, then 0 10 %d)\n", e1, e2, e3,
           (unsigned long long)c.rlim_cur, (unsigned long long)c.rlim_max, e4, e5, e6, e7, e8, e9);
    check("set", e1 == -EINVAL && e2 == 0 && e3 == -EPERM && c.rlim_cur == 100 && c.rlim_max == 200 && e4 == -EPERM && e5 == -EINVAL && e6 == -EINVAL && e7 == 0 && e8 == 0 && e9 == -EPERM);
    show_limits(getpid(), "after");
  }
  // prlimit
  {
    struct rlimit r, o;
    long a = prl(0, RLIMIT_STACK, NULL, &o), b = prl(getpid(), RLIMIT_STACK, NULL, &r);
    long c = prl(1, RLIMIT_STACK, NULL, &r), d = prl(999999, RLIMIT_STACK, NULL, &r);
    r = (struct rlimit){50, 60};
    long e = prl(0, RLIMIT_MSGQUEUE, (void *)8, NULL);
    struct rlimit m;
    getrlimit(RLIMIT_MSGQUEUE, &m);
    long f = prl(0, RLIMIT_MSGQUEUE, &r, (void *)8);
    struct rlimit m2;
    getrlimit(RLIMIT_MSGQUEUE, &m2);
    long g = prl(0, 16, NULL, &o);
    printf("prlimit (self %ld %ld, init %ld, none %ld, bad new %ld (msgqueue %llu), bad old %ld (msgqueue %llu %llu), 16 %ld)\n", a, b, c, d, e,
           (unsigned long long)m.rlim_cur, f, (unsigned long long)m2.rlim_cur, (unsigned long long)m2.rlim_max, g);
    check("prlimit", a == 0 && b == 0 && c == -EPERM && d == -ESRCH && e == -EFAULT && m.rlim_cur == 819200 && f == -EFAULT && m2.rlim_cur == 50 && m2.rlim_max == 60 && g == -EINVAL);
  }
  // 子: 読む、書く (子の /proc/PID/limits にも)、子は書かれたものを見る
  {
    int p1[2], p2[2];
    pipe(p1);
    pipe(p2);
    pid_t k = fork();
    if (k == 0) {
      char ch;
      read(p1[0], &ch, 1);
      struct rlimit r;
      getrlimit(RLIMIT_LOCKS, &r);
      printf("child: LOCKS %llu %llu\n", (unsigned long long)r.rlim_cur, (unsigned long long)r.rlim_max);
      write(p2[1], "x", 1);
      read(p1[0], &ch, 1);
      _exit(0);
    }
    struct rlimit r = {77, 88}, o;
    long a = prl(k, RLIMIT_LOCKS, &r, &o);
    long b = prl(k, RLIMIT_LOCKS, NULL, &o);
    printf("prlimit child (set %ld, read %ld: %llu %llu)\n", a, b, (unsigned long long)o.rlim_cur, (unsigned long long)o.rlim_max);
    write(p1[1], "x", 1);
    char ch;
    read(p2[0], &ch, 1);
    char tag[] = "child-limits";
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/limits", k);
    FILE *f = fopen(p, "r");
    char line[256];
    while (f && fgets(line, sizeof line, f))
      if (!strncmp(line, "Max file locks", 14))
        printf("%s: %s", tag, line);
    if (f)
      fclose(f);
    write(p1[1], "x", 1);
    waitpid(k, NULL, 0);
  }
  // inherit: fork、exec、スレッド
  {
    struct rlimit r = {5, 6};
    setrlimit(RLIMIT_LOCKS, &r);
    pid_t k = fork();
    if (k == 0) {
      struct rlimit c;
      getrlimit(RLIMIT_LOCKS, &c);
      printf("fork: LOCKS %llu %llu\n", (unsigned long long)c.rlim_cur, (unsigned long long)c.rlim_max);
      execl("/proc/self/exe", argv[0], "exec", (char *)NULL);
      _exit(1);
    }
    waitpid(k, NULL, 0);
    pthread_t t;
    pthread_create(&t, NULL, thr, NULL);
    pthread_join(t, NULL);
    struct rlimit c;
    getrlimit(RLIMIT_MSGQUEUE, &c);
    printf("thread: MSGQUEUE %llu %llu\n", (unsigned long long)c.rlim_cur, (unsigned long long)c.rlim_max);
    check("thread", c.rlim_cur == 40 && c.rlim_max == 60);
  }
  // nofile
  {
    for (int i = 3; i < 64; i++)
      close(i);
    struct rlimit r = {10, 4096};
    setrlimit(RLIMIT_NOFILE, &r);
    int last = -1, e = 0;
    for (int i = 0; i < 20; i++) {
      int fd = open("/dev/null", O_RDONLY);
      if (fd < 0) {
        e = -errno;
        break;
      }
      last = fd;
    }
    int d = dup2(0, 20) < 0 ? -errno : 0;
    printf("nofile 10 (last fd %d, then %d, dup2 to 20 %d)\n", last, e, d);
    check("nofile", last == 9 && e == -EMFILE && d == -EBADF);
  }
  printf("rlimit: %d failed\n", fails);
  return fails != 0;
}
