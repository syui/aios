// ptrace ののこりが Linux と同じか: fork / exec / exit の知らせ (PTRACE_O_TRACEFORK、TRACEEXEC、TRACEEXIT と GETEVENTMSG、
// 新しい子は自動でつながって SIGSTOP で止まる)、子でない tracee の終わりを tracer が wait で受けとる、POKETEXT でコード
// に書く、GETREGSET の NT_PRFPREG、PTRACE_GET_SYSCALL_INFO (入口、出口、ほか)、入口でシステムコールの番号を変える
// (NT_ARM_SYSTEM_CALL)
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

#ifndef NT_ARM_SYSTEM_CALL
#define NT_ARM_SYSTEM_CALL 0x404
#endif

struct sinfo {
  uint8_t op, pad[3];
  uint32_t arch;
  uint64_t ip, sp;
  union {
    struct {
      uint64_t nr, args[6];
    } entry;
    struct {
      int64_t rval;
      uint8_t is_error;
    } exit;
  };
};

// 知らせが思ったものでない (CI の ctests は終わりの番号だけ見る)
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
    snprintf(r, 64, "stop %d%s", WSTOPSIG(s) & 0x7f, WSTOPSIG(s) & 0x80 ? "|0x80" : "");
  else if (WIFEXITED(s))
    snprintf(r, 64, "exit %d", WEXITSTATUS(s));
  else if (WIFSIGNALED(s))
    snprintf(r, 64, "signal %d", WTERMSIG(s));
  else
    snprintf(r, 64, "? %#x", s);
  if (s >> 16)
    snprintf(r + strlen(r), 20, " event %d", s >> 16);
  return r;
}
static int wait1(pid_t c) {
  int s = 0;
  if (waitpid(c, &s, __WALL) != c)
    return -1;
  return s;
}
static unsigned long msg(pid_t c) {
  unsigned long m = 0;
  P(PTRACE_GETEVENTMSG, c, 0, (long)&m);
  return m;
}

// 子: 孫を fork して、自分は exec する
static void events(void) {
  pid_t c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    pid_t g = fork();
    if (g == 0)
      _exit(5);
    execl("/proc/self/exe", "ptrace2", "exit7", (char *)0);
    _exit(9);
  }
  wait1(c);
  printf("events: setoptions %ld", P(PTRACE_SETOPTIONS, c, 0, PTRACE_O_TRACEFORK | PTRACE_O_TRACEEXEC | PTRACE_O_TRACEEXIT));
  P(PTRACE_CONT, c, 0, 0);
  int s = wait1(c);
  pid_t g = (pid_t)msg(c);
  printf(", child %s", st(s));
  // 孫は自動でつながり、SIGSTOP で止まる
  int gs = wait1(g);
  long tr = -1;
  char p[64], l[256];
  snprintf(p, sizeof p, "/proc/%d/status", g);
  FILE *f = fopen(p, "r");
  while (f && fgets(l, sizeof l, f))
    if (!strncmp(l, "TracerPid:", 10))
      tr = atol(l + 10);
  if (f)
    fclose(f);
  printf(", grandchild %s (tracer is me %d)\n", st(gs), tr == getpid());
  // 孫を続けると、終わる前に exit の知らせ (msg は wait の status)、それから tracer が終わりを受けとる (子でなくても)
  P(PTRACE_CONT, g, 0, 0);
  gs = wait1(g);
  printf("  grandchild: %s msg %#lx", st(gs), msg(g));
  P(PTRACE_CONT, g, 0, 0);
  gs = wait1(g);
  bad |= !(WIFEXITED(gs) && WEXITSTATUS(gs) == 5);
  printf(", then %s\n", st(gs));
  // 子は exec の知らせ (msg は前の tid)、それから exit の知らせ。孫が終わった SIGCHLD で止まることもある (時間しだい):
  // そのシグナルで続けて、書かない
  int sig = 0;
  printf("  child:");
  for (int i = 0; i < 6; i++) {
    P(PTRACE_CONT, c, 0, sig);
    s = wait1(c);
    sig = 0;
    if (WIFSTOPPED(s) && s >> 16 == 0 && WSTOPSIG(s) == SIGCHLD) {
      sig = SIGCHLD;
      continue;
    }
    if (s >> 16 == PTRACE_EVENT_EXEC)
      printf(" %s msg is its pid %d,", st(s), msg(c) == (unsigned long)c);
    else if (s >> 16 == PTRACE_EVENT_EXIT)
      printf(" %s msg %#lx,", st(s), msg(c));
    else {
      printf(" %s\n", st(s));
      break;
    }
  }
}

static int code_marker(int x) { return x * 3 + 1; }

static void text_and_regs(void) {
  pid_t c = fork();
  if (c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    printf("  child: getppid gave its pid %d\n", (pid_t)syscall(SYS_getppid) == getpid());
    _exit(code_marker(1) == 4 ? 0 : 1);
  }
  wait1(c);
  long w = P(PTRACE_PEEKTEXT, c, (long)code_marker, 0);
  long pw = P(PTRACE_POKETEXT, c, (long)code_marker, w);
  bad |= pw != 0;
  printf("text: peek ok %d, poke same word %ld, peek again same %d\n", w != -EIO, pw, P(PTRACE_PEEKTEXT, c, (long)code_marker, 0) == w);
  unsigned char fp[600];
  struct iovec io = {fp, sizeof fp};
  printf("  prfpreg %ld len %zu", P(PTRACE_GETREGSET, c, NT_PRFPREG, (long)&io), io.iov_len);
  int nr = 0;
  struct iovec sc = {&nr, sizeof nr};
  printf(", system_call %ld len %zu\n", P(PTRACE_GETREGSET, c, NT_ARM_SYSTEM_CALL, (long)&sc), sc.iov_len);
  struct sinfo si;
  memset(&si, 0xee, sizeof si);
  long n = P(PTRACE_GET_SYSCALL_INFO, c, sizeof si, (long)&si);
  printf("  syscall_info at a signal stop: %ld op %d arch %#x\n", n, si.op, si.arch);
  // getppid の入口: 番号を getpid に変える
  P(PTRACE_SETOPTIONS, c, 0, PTRACE_O_TRACESYSGOOD);
  for (int i = 0; i < 2; i++) {
    P(PTRACE_SYSCALL, c, 0, 0);
    int s = wait1(c);
    memset(&si, 0xee, sizeof si);
    n = P(PTRACE_GET_SYSCALL_INFO, c, sizeof si, (long)&si);
    if (si.op == 1)
      printf("  %s info %ld op 1 nr is getppid %d", st(s), n, si.entry.nr == SYS_getppid);
    else if (si.op == 2)
      printf("  %s info %ld op 2 rval is child %d err %d", st(s), n, si.exit.rval == c, si.exit.is_error);
    else
      printf("  %s info %ld op %d", st(s), n, si.op);
    if (si.op == 1) {
      nr = SYS_getpid;
      sc.iov_len = sizeof nr;
      printf(", set system_call %ld", P(PTRACE_SETREGSET, c, NT_ARM_SYSTEM_CALL, (long)&sc));
    }
    printf("\n");
  }
  P(PTRACE_CONT, c, 0, 0);
  int e = wait1(c);
  bad |= !(WIFEXITED(e) && WEXITSTATUS(e) == 0);
  printf("  end %s\n", st(e));
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "exit7"))
    return 7;
  setvbuf(stdout, NULL, _IONBF, 0);
  events();
  text_and_regs();
  return bad;
}
