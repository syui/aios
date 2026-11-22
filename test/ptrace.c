// ptrace が Linux と同じか: TRACEME と exec の SIGTRAP、止まった知らせ (waitpid の status)、PEEKDATA / POKEDATA、
// GETREGSET (NT_PRSTATUS) と SETREGSET、CONT とシグナルの注入、GETSIGINFO、SYSCALL (入口と出口、TRACESYSGOOD)、
// ATTACH と DETACH、SEIZE と INTERRUPT、KILL、まちがい (ESRCH、EPERM、EIO、EINVAL)、/proc/PID/status の TracerPid
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

struct regs {
  uint64_t x[31], sp, pc, pstate;
};

static volatile long shared = 1234;
// 止まりかたが思ったものでない (CI の ctests は終わりの番号だけ見る)
static int bad;
static volatile int got;
static void on_usr1(int s) { got = s; }

static long P(long req, pid_t pid, long addr, long data) {
  errno = 0;
  long r = ptrace(req, pid, (void *)addr, (void *)data);
  return r == -1 && errno ? -errno : r;
}
// 止まった知らせ: "stop SIG (event E)" か "exit N" か "signal N"
static const char *st(int s) {
  static char b[4][64];
  static int k;
  char *r = b[k++ % 4];
  if (WIFSTOPPED(s))
    snprintf(r, 64, "stop %d%s%s", WSTOPSIG(s) & 0x7f, WSTOPSIG(s) & 0x80 ? "|0x80" : "", s >> 16 ? " event" : "");
  else if (WIFEXITED(s))
    snprintf(r, 64, "exit %d", WEXITSTATUS(s));
  else if (WIFSIGNALED(s))
    snprintf(r, 64, "signal %d", WTERMSIG(s));
  else
    snprintf(r, 64, "? %#x", s);
  if (s >> 16)
    snprintf(r + strlen(r), 20, " %d", s >> 16);
  return r;
}
static int wait1(pid_t c) {
  int s = 0;
  if (waitpid(c, &s, __WALL) != c)
    return -1;
  return s;
}
static long tracer_of(pid_t c) {
  char p[64], l[256];
  snprintf(p, sizeof p, "/proc/%d/status", c);
  FILE *f = fopen(p, "r");
  long t = -1;
  while (f && fgets(l, sizeof l, f))
    if (!strncmp(l, "TracerPid:", 10))
      t = atol(l + 10);
  if (f)
    fclose(f);
  return t;
}

// TRACEME して自分を止め、印を見て、終わる
static void traceme_child(void) {
  signal(SIGUSR1, on_usr1);
  if (ptrace(PTRACE_TRACEME, 0, 0, 0))
    _exit(10);
  printf("  child: second TRACEME %ld\n", P(PTRACE_TRACEME, 0, 0, 0));
  raise(SIGSTOP);
  // 親が shared を書きかえ、SIGUSR1 を注入する
  printf("  child: shared %ld, got %d\n", shared, got);
  _exit(3);
}

static void traceme(void) {
  pid_t c = fork();
  if (c == 0)
    traceme_child();
  int s = wait1(c);
  bad |= !(WIFSTOPPED(s) && WSTOPSIG(s) == SIGSTOP);
  printf("traceme: %s, tracer is me %d\n", st(s), tracer_of(c) == getpid());
  printf("  peek %ld, poke %ld", P(PTRACE_PEEKDATA, c, (long)&shared, 0), P(PTRACE_POKEDATA, c, (long)&shared, 5678));
  printf(", peek bad addr %ld, peekuser %ld\n", P(PTRACE_PEEKDATA, c, 8, 0), P(PTRACE_PEEKUSER, c, 0, 0));
  struct regs r;
  struct iovec io = {&r, sizeof r};
  long g = P(PTRACE_GETREGSET, c, NT_PRSTATUS, (long)&io);
  printf("  getregset %ld len %zu, x8 is kill %d", g, io.iov_len, r.x[8] == SYS_kill || r.x[8] == SYS_tgkill);
  struct iovec small = {&r, 8};
  printf(", small %ld len %zu", P(PTRACE_GETREGSET, c, NT_PRSTATUS, (long)&small), small.iov_len);
  printf(", bad note %ld\n", P(PTRACE_GETREGSET, c, 0x9999, (long)&io));
  siginfo_t si;
  memset(&si, 0, sizeof si);
  printf("  getsiginfo %ld signo %d code %d\n", P(PTRACE_GETSIGINFO, c, 0, (long)&si), si.si_signo, si.si_code);
  // SIGSTOP は捨てて、SIGUSR1 を入れて続ける (ハンドラが動く)
  printf("  cont with SIGUSR1 %ld\n", P(PTRACE_CONT, c, 0, SIGUSR1));
  s = wait1(c);
  // ハンドラを動かす前に、シグナルの知らせで止まる
  printf("  then: %s\n", st(s));
  if (WIFSTOPPED(s)) {
    P(PTRACE_CONT, c, 0, WSTOPSIG(s));
    s = wait1(c);
  }
  printf("  end: %s, peek after exit %ld\n", st(s), P(PTRACE_PEEKDATA, c, (long)&shared, 0));
}

static void exec_trap(void) {
  pid_t c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    execl("/proc/self/exe", "ptrace", "exit7", (char *)0);
    _exit(9);
  }
  int s = wait1(c);
  bad |= !(WIFSTOPPED(s) && WSTOPSIG(s) == SIGTRAP);
  printf("exec: %s", st(s));
  P(PTRACE_CONT, c, 0, 0);
  printf(", then %s\n", st(wait1(c)));
}

static void syscall_stops(void) {
  pid_t c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    syscall(SYS_getppid);
    _exit(0);
  }
  wait1(c);
  printf("syscall: setoptions %ld, bad option %ld\n", P(PTRACE_SETOPTIONS, c, 0, PTRACE_O_TRACESYSGOOD), P(PTRACE_SETOPTIONS, c, 0, 1 << 30));
  // raise の kill の出口、getppid の入口と出口
  for (int i = 0; i < 6; i++) {
    if (P(PTRACE_SYSCALL, c, 0, 0))
      break;
    int s = wait1(c);
    if (!WIFSTOPPED(s)) {
      printf("  %s\n", st(s));
      return;
    }
    struct regs r;
    struct iovec io = {&r, sizeof r};
    P(PTRACE_GETREGSET, c, NT_PRSTATUS, (long)&io);
    int nr = (int)r.x[8];
    const char *name = nr == SYS_getppid ? "getppid" : nr == SYS_exit || nr == SYS_exit_group ? "exit" : "other";
    printf("  %s nr %s x0 is ppid %d\n", st(s), name, nr == SYS_getppid && r.x[0] == (uint64_t)getpid());
  }
  P(PTRACE_CONT, c, 0, 0);
  printf("  end %s\n", st(wait1(c)));
}

static void attach(void) {
  pid_t c = fork();
  if (c == 0) {
    for (;;)
      pause();
  }
  usleep(100000);
  printf("attach: %ld", P(PTRACE_ATTACH, c, 0, 0));
  int s = wait1(c);
  printf(", %s, again %ld, tracer is me %d", st(s), P(PTRACE_ATTACH, c, 0, 0), tracer_of(c) == getpid());
  printf(", detach %ld, tracer %ld", P(PTRACE_DETACH, c, 0, 0), tracer_of(c));
  printf(", peek after detach %ld\n", P(PTRACE_PEEKDATA, c, (long)&shared, 0));
  printf("seize: %ld", P(PTRACE_SEIZE, c, 0, 0));
  printf(", interrupt %ld", P(PTRACE_INTERRUPT, c, 0, 0));
  s = wait1(c);
  printf(", %s", st(s));
  // PTRACE_KILL は、シグナルで止まったのでないとき (INTERRUPT の止まり) は SIGKILL を入れずに続けるだけ: 続いているか
  // (WNOHANG で 0) を見てから、ほんとうに kill する
  printf(", kill %ld", P(PTRACE_KILL, c, 0, 0));
  usleep(100000);
  if (waitpid(c, &s, WNOHANG | __WALL) == 0) {
    printf(", still there");
    kill(c, SIGKILL);
    s = wait1(c);
  }
  printf(", %s\n", st(s));
}

static void errors(void) {
  // init には nobody で (root で ATTACH すると init が止まる)
  pid_t n = fork();
  if (n == 0) {
    if (setresuid(65534, 65534, 65534))
      _exit(1);
    _exit(-P(PTRACE_ATTACH, 1, 0, 0));
  }
  int ns = wait1(n);
  printf("errors: no such pid %ld, init by nobody %d, self %ld, not traced child", P(PTRACE_PEEKDATA, 999999, 0, 0), WEXITSTATUS(ns), P(PTRACE_ATTACH, getpid(), 0, 0));
  pid_t c = fork();
  if (c == 0) {
    pause();
    _exit(0);
  }
  printf(" %ld, bad request %ld", P(PTRACE_PEEKDATA, c, (long)&shared, 0), P(12345, c, 0, 0));
  kill(c, SIGKILL);
  waitpid(c, NULL, 0);
  // 走っている (止まっていない) ものには ESRCH
  c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    for (;;)
      ;
  }
  usleep(100000);
  printf(", running tracee %ld", P(PTRACE_PEEKDATA, c, (long)&shared, 0));
  kill(c, SIGKILL);
  printf(", killed %s\n", st(wait1(c)));
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "exit7"))
    return 7;
  setvbuf(stdout, NULL, _IONBF, 0);
  traceme();
  exec_trap();
  syscall_stops();
  attach();
  errors();
  return bad;
}
