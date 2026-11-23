// ptrace ののこり (ソフトウェアのもの) が Linux と同じか:
//   siginfo    PTRACE_SETSIGINFO でシグナルの知らせの siginfo を書きかえる (GETSIGINFO と、届いたハンドラの si_code と値)
//   peek       PTRACE_PEEKSIGINFO でまだ届いていないシグナルの列を読む (スレッドのものと、PEEKSIGINFO_SHARED でプロセスのもの、
//              off、nr、まちがった flags)
//   vforkdone  PTRACE_O_TRACEVFORKDONE: vfork の子が終わると PTRACE_EVENT_VFORK_DONE (GETEVENTMSG は子の番号)
//   seccomp    PTRACE_O_TRACESECCOMP: SECCOMP_RET_TRACE で PTRACE_EVENT_SECCOMP (GETEVENTMSG は RET_DATA)。そのまま続けると
//              ふつうに、番号を -1 にすると x0 を答えにして飛ばす。tracer がいなければ ENOSYS
//   listen     PTRACE_LISTEN: SEIZE したものがグループの止まりにいるとき、止まったまま SIGCONT で起きられる (起きたら
//              PTRACE_EVENT_STOP でまた知らせる)。ほかの止まりでも、SEIZE でなくても EIO
//   options    PTRACE_O_SUSPEND_SECCOMP (CAP_SYS_ADMIN がなければ EPERM)、知らない印 (SETOPTIONS は EINVAL、SEIZE は EIO)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef NT_ARM_SYSTEM_CALL
#define NT_ARM_SYSTEM_CALL 0x404
#endif
#ifndef PTRACE_PEEKSIGINFO_SHARED
#define PTRACE_PEEKSIGINFO_SHARED 1
#endif

static int bad;
static long P(long req, pid_t pid, long addr, long data) {
  errno = 0;
  long r = ptrace(req, pid, (void *)addr, (void *)data);
  return r == -1 && errno ? -errno : r;
}
static const char *st(int s) {
  static char b[4][64];
  static int k;
  char *r = b[k++ % 4];
  if (WIFSTOPPED(s))
    snprintf(r, 64, "stop %d", WSTOPSIG(s) & 0x7f);
  else if (WIFEXITED(s))
    snprintf(r, 64, "exit %d", WEXITSTATUS(s));
  else if (WIFSIGNALED(s))
    snprintf(r, 64, "killed %d", WTERMSIG(s));
  else
    snprintf(r, 64, "? %#x", s);
  if (s >> 16)
    snprintf(r + strlen(r), 20, " event %#x", s >> 16);
  return r;
}
static int wait1(pid_t c) {
  int s = 0;
  if (waitpid(c, &s, __WALL) != c)
    return -1;
  return s;
}

// siginfo: 子の SIGUSR1 のハンドラが見た si_code と値
static volatile int got_code, got_int;
static void on_usr1(int sig, siginfo_t *i, void *u) {
  (void)sig;
  (void)u;
  got_code = i->si_code;
  got_int = i->si_value.sival_int;
}

static void siginfo(void) {
  pid_t c = fork();
  if (c == 0) {
    struct sigaction sa = {.sa_sigaction = on_usr1, .sa_flags = SA_SIGINFO};
    sigaction(SIGUSR1, &sa, 0);
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGUSR1);
    _exit(got_code == SI_QUEUE && got_int == 1234 ? 0 : 1);
  }
  int s = wait1(c);
  siginfo_t a, b;
  memset(&a, 0, sizeof a);
  long g = P(PTRACE_GETSIGINFO, c, 0, (long)&a);
  siginfo_t n = a;
  n.si_code = SI_QUEUE;
  n.si_value.sival_int = 1234;
  long w = P(PTRACE_SETSIGINFO, c, 0, (long)&n);
  memset(&b, 0, sizeof b);
  P(PTRACE_GETSIGINFO, c, 0, (long)&b);
  long e = P(PTRACE_SETSIGINFO, c, 0, 8);
  P(PTRACE_CONT, c, 0, SIGUSR1);
  int x = wait1(c);
  printf("siginfo: %s, get %ld signo %d code %d, set %ld, get again code %d int %d, bad pointer %ld, handler saw it: %s\n", st(s), g, a.si_signo, a.si_code, w,
         b.si_code, b.si_value.sival_int, e, st(x));
  bad |= !(WIFEXITED(x) && WEXITSTATUS(x) == 0);
}

struct peekargs {
  uint64_t off;
  uint32_t flags;
  int32_t nr;
};

static void peek(void) {
  pid_t c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGUSR2);
    sigaddset(&m, SIGRTMIN);
    sigaddset(&m, SIGRTMIN + 1);
    sigprocmask(SIG_BLOCK, &m, 0);
    // プロセスへ: SIGUSR2、SIGRTMIN を 2 つ (値 1 と 2)。スレッドへ: SIGRTMIN+1 (値 3)
    kill(getpid(), SIGUSR2);
    union sigval v = {.sival_int = 1};
    sigqueue(getpid(), SIGRTMIN, v);
    v.sival_int = 2;
    sigqueue(getpid(), SIGRTMIN, v);
    siginfo_t t = {.si_signo = SIGRTMIN + 1, .si_code = SI_QUEUE, .si_pid = getpid(), .si_uid = getuid()};
    t.si_value.sival_int = 3;
    syscall(SYS_rt_tgsigqueueinfo, getpid(), gettid(), SIGRTMIN + 1, &t);
    raise(SIGSTOP);
    _exit(0);
  }
  wait1(c);
  siginfo_t q[8];
  printf("peek:");
  struct peekargs a[] = {{0, 0, 8}, {0, PTRACE_PEEKSIGINFO_SHARED, 8}, {1, PTRACE_PEEKSIGINFO_SHARED, 1}, {0, PTRACE_PEEKSIGINFO_SHARED, 0}, {5, PTRACE_PEEKSIGINFO_SHARED, 8},
                         {0, 2, 8}, {0, 0, -1}};
  for (unsigned k = 0; k < sizeof a / sizeof *a; k++) {
    memset(q, 0, sizeof q);
    long r = P(PTRACE_PEEKSIGINFO, c, (long)&a[k], (long)q);
    printf(" [off %llu flags %u nr %d: %ld", (unsigned long long)a[k].off, a[k].flags, a[k].nr, r);
    for (long i = 0; i < r && i < 8; i++)
      printf(" %d/%d/%d", q[i].si_signo == SIGRTMIN ? 100 : q[i].si_signo == SIGRTMIN + 1 ? 101 : q[i].si_signo, q[i].si_code, q[i].si_value.sival_int);
    printf("]");
  }
  long f = P(PTRACE_PEEKSIGINFO, c, 8, (long)q);
  printf(" bad args %ld\n", f);
  kill(c, SIGKILL);
  wait1(c);
}

static void vforkdone(void) {
  pid_t c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    pid_t g = vfork();
    if (g == 0)
      _exit(3);
    int s;
    waitpid(g, &s, 0);
    _exit(WEXITSTATUS(s) == 3 ? 0 : 1);
  }
  wait1(c);
  long o = P(PTRACE_SETOPTIONS, c, 0, PTRACE_O_TRACEVFORKDONE);
  P(PTRACE_CONT, c, 0, 0);
  int s = wait1(c);
  unsigned long m = 0;
  P(PTRACE_GETEVENTMSG, c, 0, (long)&m);
  P(PTRACE_CONT, c, 0, 0);
  int x = wait1(c);
  // vfork の子の SIGCHLD で止まることもある
  while (WIFSTOPPED(x)) {
    P(PTRACE_CONT, c, 0, WSTOPSIG(x) == SIGCHLD ? SIGCHLD : 0);
    x = wait1(c);
  }
  printf("vforkdone: setoptions %ld, %s msg is a pid %d, then %s\n", o, st(s), m > 0 && m != (unsigned long)c, st(x));
  bad |= !(s >> 16 == PTRACE_EVENT_VFORK_DONE && WIFEXITED(x) && WEXITSTATUS(x) == 0);
}

static int install_trace_filter(void) {
  struct sock_filter f[] = {
      BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRACE | 42),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
  };
  struct sock_fprog p = {.len = 4, .filter = f};
  if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))
    return -1;
  return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &p);
}

static void seccomp_trace(void) {
  pid_t c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    pid_t pp = getppid();
    if (install_trace_filter())
      _exit(9);
    long a = syscall(SYS_getppid), b = syscall(SYS_getppid);
    _exit((a == pp ? 0 : 1) | (b == 777 ? 0 : 2));
  }
  wait1(c);
  long o = P(PTRACE_SETOPTIONS, c, 0, PTRACE_O_TRACESECCOMP);
  P(PTRACE_CONT, c, 0, 0);
  int s1 = wait1(c);
  unsigned long m1 = 0;
  P(PTRACE_GETEVENTMSG, c, 0, (long)&m1);
  // そのまま続ける
  P(PTRACE_CONT, c, 0, 0);
  int s2 = wait1(c);
  unsigned long m2 = 0;
  P(PTRACE_GETEVENTMSG, c, 0, (long)&m2);
  // 2 回目は番号を -1 にして飛ばし、x0 を 777 に
  int nr = -1;
  struct iovec io = {&nr, sizeof nr};
  long r1 = P(PTRACE_SETREGSET, c, NT_ARM_SYSTEM_CALL, (long)&io);
  uint64_t regs[34];
  struct iovec rv = {regs, sizeof regs};
  P(PTRACE_GETREGSET, c, NT_PRSTATUS, (long)&rv);
  regs[0] = 777;
  long r2 = P(PTRACE_SETREGSET, c, NT_PRSTATUS, (long)&rv);
  P(PTRACE_CONT, c, 0, 0);
  int x = wait1(c);
  printf("seccomp: setoptions %ld, %s msg %lu, %s msg %lu, skip %ld %ld, then %s\n", o, st(s1), m1, st(s2), m2, r1, r2, st(x));
  bad |= !(WIFEXITED(x) && WEXITSTATUS(x) == 0);
  // tracer がいなければ ENOSYS
  pid_t d = fork();
  if (d == 0) {
    if (install_trace_filter())
      _exit(9);
    errno = 0;
    long r = syscall(SYS_getppid);
    _exit(r == -1 && errno == ENOSYS ? 0 : 1);
  }
  int y = wait1(d);
  printf("seccomp: RET_TRACE without a tracer: %s\n", st(y));
  bad |= !(WIFEXITED(y) && WEXITSTATUS(y) == 0);
}

static void listen_(void) {
  pid_t c = fork();
  if (c == 0) {
    for (;;)
      pause();
  }
  long sz = P(PTRACE_SEIZE, c, 0, 0);
  kill(c, SIGSTOP);
  int s1 = wait1(c);
  // シグナルの知らせ (SIGSTOP) を続けると、グループの止まり (PTRACE_EVENT_STOP)
  long e1 = P(PTRACE_LISTEN, c, 0, 0);
  P(PTRACE_CONT, c, 0, SIGSTOP);
  int s2 = wait1(c);
  long l = P(PTRACE_LISTEN, c, 0, 0);
  kill(c, SIGCONT);
  int s3 = wait1(c);
  P(PTRACE_CONT, c, 0, 0);
  // SIGCONT の知らせ
  int s4 = wait1(c);
  P(PTRACE_CONT, c, 0, WIFSTOPPED(s4) ? WSTOPSIG(s4) : 0);
  printf("listen: seize %ld, %s, listen there %ld, %s, listen %ld, after SIGCONT %s, then %s\n", sz, st(s1), e1, st(s2), l, st(s3), st(s4));
  bad |= !(l == 0 && s2 >> 16 == PTRACE_EVENT_STOP && s3 >> 16 == PTRACE_EVENT_STOP);
  kill(c, SIGKILL);
  wait1(c);
  // SEIZE でなければ EIO
  pid_t d = fork();
  if (d == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    _exit(0);
  }
  wait1(d);
  long e2 = P(PTRACE_LISTEN, d, 0, 0);
  long so = P(PTRACE_SETOPTIONS, d, 0, PTRACE_O_SUSPEND_SECCOMP);
  printf("listen: not seized %ld; options: SUSPEND_SECCOMP %ld\n", e2, so);
  long su = P(PTRACE_SETOPTIONS, d, 0, 1L << 30);
  kill(d, SIGKILL);
  wait1(d);
  pid_t f = fork();
  if (f == 0) {
    pause();
    _exit(0);
  }
  long zu = P(PTRACE_SEIZE, f, 0, 1L << 30);
  long zs = P(PTRACE_SEIZE, f, 0, PTRACE_O_SUSPEND_SECCOMP);
  printf("options: SETOPTIONS 1<<30 %ld, SEIZE 1<<30 %ld, SEIZE SUSPEND_SECCOMP %ld\n", su, zu, zs);
  kill(f, SIGKILL);
  wait1(f);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  siginfo();
  peek();
  vforkdone();
  seccomp_trace();
  listen_();
  return bad;
}
