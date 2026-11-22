// ptrace のハードウェアのもの (arm64 のデバッグの仕組み) が Linux と同じか:
//   step    PTRACE_SINGLESTEP で 1 命令ずつ (SIGTRAP の TRAP_TRACE、si_addr は次の pc)。分岐、svc (Linux はシステムコールの
//           出口で SI_USER の SIGTRAP: 疑似のステップ)、止まったままの b . も。終わりにレジスタ
//   sigstep シグナルをつけて SINGLESTEP すると、ハンドラの 1 命令目で止まる (si_code は SIGTRAP の 5)
//   info    NT_ARM_HW_BREAK と NT_ARM_HW_WATCH の dbg_info (デバッグの版とスロットの数) と、iov_len の切りつめ。版は CPU
//           しだい (aios の VM は cortex-a72 で 6、手本の Linux は max で 9) なので、v8 の 6 より上かだけ
//   bp      ハードウェアのブレークポイント (TRAP_HWBKPT、si_addr はその番地)。そのまま続けるとまた止まり、消すと進む
//   wp      ウォッチポイント: 書きこみ (書く前に止まる)、1 バイトの読み。番地と長さの読みかえし (8 バイトにそろえる)
//   bad     まちがった ctrl (種類、長さ)、カーネルの番地、スロットの数より多く
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef NT_ARM_HW_BREAK
#define NT_ARM_HW_BREAK 0x402
#endif
#ifndef NT_ARM_HW_WATCH
#define NT_ARM_HW_WATCH 0x403
#endif

struct hwdebug {
  uint32_t info;
  uint32_t pad;
  struct {
    uint64_t addr;
    uint32_t ctrl;
    uint32_t pad;
  } r[16];
};

// 1 命令ずつのところ: mov、add、b (1 つ飛ばす)、add、mov x8 と svc (getpid)、mov、b .
extern char seq[], seq_svc[], seq_end[], bpfn[];
__asm__(".text\n.global seq\n.global seq_svc\n.global seq_end\n.global bpfn\n"
        "seq:\n"
        "  mov x9, #1\n"
        "  add x9, x9, #2\n"
        "  b 1f\n"
        "  nop\n"
        "1: add x9, x9, #4\n"
        "  mov x8, #172\n"
        "seq_svc:\n"
        "  svc #0\n"
        "  mov x10, x0\n"
        "seq_end:\n"
        "  b seq_end\n"
        "bpfn:\n"
        "  nop\n"
        "  ret\n");

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
static uint64_t regs[34];
static long getregs(pid_t c) {
  struct iovec io = {regs, sizeof regs};
  return P(PTRACE_GETREGSET, c, NT_PRSTATUS, (long)&io);
}
static long setregs(pid_t c) {
  struct iovec io = {regs, sizeof regs};
  return P(PTRACE_SETREGSET, c, NT_PRSTATUS, (long)&io);
}
static long sethw(pid_t c, int note, struct hwdebug *h, size_t len) {
  struct iovec io = {h, len};
  return P(PTRACE_SETREGSET, c, note, (long)&io);
}
static long gethw(pid_t c, int note, struct hwdebug *h, size_t *len) {
  memset(h, 0xee, sizeof *h);
  struct iovec io = {h, *len};
  long r = P(PTRACE_GETREGSET, c, note, (long)&io);
  *len = io.iov_len;
  return r;
}

// ctrl: BAS (長さ) << 5、種類 << 3 (0 実行、1 読み、2 書き、3 両方)、EL0 (2) << 1、有効 1
#define CTRL(bas, type) ((uint32_t)(bas) << 5 | (type) << 3 | 2 << 1 | 1)

static void stopped_child(pid_t *c, void (*after)(void)) {
  *c = fork();
  if (*c == 0) {
    ptrace(PTRACE_TRACEME, 0, 0, 0);
    raise(SIGSTOP);
    after();
    _exit(0);
  }
  wait1(*c);
}

static void loop(void) {
  for (;;)
    pause();
}

static volatile int handled;
static void on_usr1(int s) {
  (void)s;
  handled++;
}

static void step(void) {
  struct sigaction sa = {.sa_handler = on_usr1};
  sigaction(SIGUSR1, &sa, 0);
  pid_t c;
  stopped_child(&c, loop);
  getregs(c);
  regs[32] = (uint64_t)seq;
  long sr = setregs(c);
  printf("step: setregs %ld:", sr);
  for (int i = 0; i < 9; i++) {
    long r = P(PTRACE_SINGLESTEP, c, 0, 0);
    int s = wait1(c);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    P(PTRACE_GETSIGINFO, c, 0, (long)&si);
    getregs(c);
    long pc = (long)(regs[32] - (uint64_t)seq);
    if (si.si_code > 0)
      printf(" [%ld %s pc+%ld code %d addr+%ld]", r, st(s), pc, si.si_code, (long)((uint64_t)si.si_addr - (uint64_t)seq));
    else
      printf(" [%ld %s pc+%ld code %d pid %d]", r, st(s), pc, si.si_code, si.si_pid);
  }
  getregs(c);
  printf(" x9 %llu, x10 is the pid %d, x8 %llu\n", (unsigned long long)regs[9], regs[10] == (uint64_t)c, (unsigned long long)regs[8]);
  bad |= !(regs[9] == 7 && regs[10] == (uint64_t)c && regs[32] == (uint64_t)seq_end);
  // シグナルをつけて 1 命令: ハンドラの 1 命令目で止まる
  long r = P(PTRACE_SINGLESTEP, c, 0, SIGUSR1);
  int s = wait1(c);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  P(PTRACE_GETSIGINFO, c, 0, (long)&si);
  getregs(c);
  int at = regs[32] == (uint64_t)on_usr1;
  printf("sigstep: %ld %s, at the handler %d, code %d, pid is the child %d", r, st(s), at, si.si_code, si.si_pid == c);
  bad |= !at;
  // もう 1 つ進め、それから続けると、ハンドラから戻って b . にもどる
  r = P(PTRACE_SINGLESTEP, c, 0, 0);
  s = wait1(c);
  getregs(c);
  printf(", then %ld %s pc is +%ld in the handler\n", r, st(s), (long)(regs[32] - (uint64_t)on_usr1));
  // まちがったもの: シグナルの番号、止まっていない子
  long e1 = P(PTRACE_SINGLESTEP, c, 0, 65);
  P(PTRACE_CONT, c, 0, 0);
  long e2 = P(PTRACE_SINGLESTEP, c, 0, 0);
  printf("step: signal 65 %ld, running %ld\n", e1, e2);
  kill(c, SIGKILL);
  wait1(c);
}

static void info(void) {
  pid_t c;
  stopped_child(&c, loop);
  struct hwdebug h;
  printf("info:");
  int notes[2] = {NT_ARM_HW_BREAK, NT_ARM_HW_WATCH};
  for (int k = 0; k < 2; k++) {
    size_t len = sizeof h + 64;
    long r = gethw(c, notes[k], &h, &len);
    printf(" [%s %ld len %zu arch v8+ %d slots %u pad %#x r0 %#llx/%#x r15 %#llx/%#x]", k ? "watch" : "break", r, len, (h.info >> 8 & 0xff) >= 6, h.info & 0xff, h.pad,
           (unsigned long long)h.r[0].addr, h.r[0].ctrl, (unsigned long long)h.r[15].addr, h.r[15].ctrl);
  }
  size_t l4 = 4, l6 = 6, l0 = 0;
  long a = gethw(c, NT_ARM_HW_BREAK, &h, &l4);
  printf(" len 4: %ld %zu info %s,", a, l4, (h.info & 0xff) ? "set" : "0");
  long b = gethw(c, NT_ARM_HW_BREAK, &h, &l6);
  printf(" len 6: %ld %zu,", b, l6);
  long d = gethw(c, NT_ARM_HW_BREAK, &h, &l0);
  printf(" len 0: %ld %zu\n", d, l0);
  kill(c, SIGKILL);
  wait1(c);
}

static volatile uint64_t wvar;
static volatile unsigned char wbuf[16];
static void bpchild(void) {
  ((void (*)(void))bpfn)();
  wvar = 42;
  unsigned char x = wbuf[1];
  _exit(x == 0 ? 0 : 1);
}

static void bp(void) {
  pid_t c;
  stopped_child(&c, bpchild);
  struct hwdebug h;
  memset(&h, 0, sizeof h);
  h.r[0].addr = (uint64_t)bpfn;
  h.r[0].ctrl = CTRL(0xf, 0);
  long sr = sethw(c, NT_ARM_HW_BREAK, &h, 8 + 16);
  size_t len = sizeof h;
  gethw(c, NT_ARM_HW_BREAK, &h, &len);
  printf("bp: set %ld, read +%lld ctrl %#x r1 %#llx/%#x", sr, (long long)(h.r[0].addr - (uint64_t)bpfn), h.r[0].ctrl, (unsigned long long)h.r[1].addr, h.r[1].ctrl);
  P(PTRACE_CONT, c, 0, 0);
  int s = wait1(c);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  P(PTRACE_GETSIGINFO, c, 0, (long)&si);
  getregs(c);
  printf(", hit %s code %d errno %d addr+%ld pc+%ld", st(s), si.si_code, si.si_errno, (long)((uint64_t)si.si_addr - (uint64_t)bpfn), (long)(regs[32] - (uint64_t)bpfn));
  bad |= !(WIFSTOPPED(s) && WSTOPSIG(s) == SIGTRAP && si.si_code == 4 && regs[32] == (uint64_t)bpfn);
  // 消さずに続けると、また同じところで
  P(PTRACE_CONT, c, 0, 0);
  s = wait1(c);
  getregs(c);
  printf(", again %s pc+%ld", st(s), (long)(regs[32] - (uint64_t)bpfn));
  // 消して (ctrl 0)、書きこみのウォッチポイントを
  memset(&h, 0, sizeof h);
  long cr = sethw(c, NT_ARM_HW_BREAK, &h, 8 + 16);
  h.r[0].addr = (uint64_t)&wvar;
  h.r[0].ctrl = CTRL(0xff, 2);
  long wr = sethw(c, NT_ARM_HW_WATCH, &h, 8 + 16);
  len = sizeof h;
  gethw(c, NT_ARM_HW_WATCH, &h, &len);
  printf(", clear %ld\n", cr);
  printf("wp: set %ld, read +%lld ctrl %#x", wr, (long long)(h.r[0].addr - (uint64_t)&wvar), h.r[0].ctrl);
  P(PTRACE_CONT, c, 0, 0);
  s = wait1(c);
  memset(&si, 0, sizeof si);
  P(PTRACE_GETSIGINFO, c, 0, (long)&si);
  errno = 0;
  long before = ptrace(PTRACE_PEEKDATA, c, (void *)&wvar, 0);
  printf(", hit %s code %d addr+%ld, value before %ld", st(s), si.si_code, (long)((uint64_t)si.si_addr - (uint64_t)&wvar), before);
  bad |= !(WIFSTOPPED(s) && si.si_code == 4 && before == 0);
  // 消して 1 命令進めると書かれる
  memset(&h, 0, sizeof h);
  sethw(c, NT_ARM_HW_WATCH, &h, 8 + 16);
  P(PTRACE_SINGLESTEP, c, 0, 0);
  s = wait1(c);
  long after = ptrace(PTRACE_PEEKDATA, c, (void *)&wvar, 0);
  printf(", step %s value %ld\n", st(s), after);
  // 1 バイトの読み (wbuf[1])。番地を先に変えるので、スロット 0 (いまは 8 バイト) では、ずらすと 8 バイトをはみ出して
  // EINVAL (Linux 7.4)。新しいスロット 1 (はじめは 4 バイト) なら入り、番地は 8 バイトにそろえて長さをずらして読みかえす
  h.r[0].addr = (uint64_t)&wbuf[1];
  h.r[0].ctrl = CTRL(0x1, 1);
  long r8 = sethw(c, NT_ARM_HW_WATCH, &h, 8 + 16);
  memset(&h, 0, sizeof h);
  h.r[1].addr = (uint64_t)&wbuf[1];
  h.r[1].ctrl = CTRL(0x1, 1);
  long rr = sethw(c, NT_ARM_HW_WATCH, &h, 8 + 32);
  len = sizeof h;
  gethw(c, NT_ARM_HW_WATCH, &h, &len);
  printf("wp: 1 byte in slot 0 %ld, in slot 1 %ld, read back +%lld ctrl %#x, slot 0 %#llx/%#x", r8, rr, (long long)(h.r[1].addr - (uint64_t)&wbuf[1]), h.r[1].ctrl,
         (unsigned long long)h.r[0].addr, h.r[0].ctrl);
  P(PTRACE_CONT, c, 0, 0);
  s = wait1(c);
  memset(&si, 0, sizeof si);
  P(PTRACE_GETSIGINFO, c, 0, (long)&si);
  printf(", hit %s code %d addr+%ld", st(s), si.si_code, (long)((uint64_t)si.si_addr - (uint64_t)&wbuf[1]));
  memset(&h, 0, sizeof h);
  sethw(c, NT_ARM_HW_WATCH, &h, 8 + 32);
  P(PTRACE_CONT, c, 0, 0);
  s = wait1(c);
  printf(", then %s\n", st(s));
  bad |= !(WIFEXITED(s) && WEXITSTATUS(s) == 0);
}

static void badctl(void) {
  pid_t c;
  stopped_child(&c, loop);
  struct hwdebug h;
  size_t len = sizeof h;
  gethw(c, NT_ARM_HW_BREAK, &h, &len);
  unsigned nb = h.info & 0xff;
  len = sizeof h;
  gethw(c, NT_ARM_HW_WATCH, &h, &len);
  unsigned nw = h.info & 0xff;
  if (nb == 0 || nb > 15 || nw == 0 || nw > 15) {
    printf("bad: slots %u %u\n", nb, nw);
    kill(c, SIGKILL);
    wait1(c);
    return;
  }
  struct {
    const char *name;
    int note;
    uint64_t addr;
    uint32_t ctrl;
  } t[] = {
      {"break store", NT_ARM_HW_BREAK, (uint64_t)bpfn, CTRL(0xf, 2)},
      {"break len 2", NT_ARM_HW_BREAK, (uint64_t)bpfn, CTRL(0x3, 0)},
      {"break bas 5", NT_ARM_HW_BREAK, (uint64_t)bpfn, CTRL(0x5, 0)},
      {"break +2", NT_ARM_HW_BREAK, (uint64_t)bpfn + 2, CTRL(0xf, 0)},
      {"watch exec", NT_ARM_HW_WATCH, (uint64_t)&wvar, CTRL(0xff, 0)},
      {"watch bas 0", NT_ARM_HW_WATCH, (uint64_t)&wvar, CTRL(0, 2)},
      {"watch bas 0x81", NT_ARM_HW_WATCH, (uint64_t)&wvar, CTRL(0x81, 2)},
      {"watch kernel", NT_ARM_HW_WATCH, 0xffff800080000000ull, CTRL(0xff, 3)},
      {"break kernel off", NT_ARM_HW_BREAK, 0xffff800080000000ull, 0},
      {"watch el1", NT_ARM_HW_WATCH, (uint64_t)&wvar, CTRL(0xff, 2) & ~(3u << 1) | 1 << 1},
  };
  printf("bad:");
  for (unsigned k = 0; k < sizeof t / sizeof *t; k++) {
    memset(&h, 0, sizeof h);
    h.r[0].addr = t[k].addr;
    h.r[0].ctrl = t[k].ctrl;
    long r = sethw(c, t[k].note, &h, 8 + 16);
    len = sizeof h;
    gethw(c, t[k].note, &h, &len);
    printf(" [%s %ld: %s%#x]", t[k].name, r, h.r[0].addr == t[k].addr ? "" : h.r[0].addr ? "moved " : "0 ", h.r[0].ctrl);
    memset(&h, 0, sizeof h);
    sethw(c, t[k].note, &h, 8 + 16);
  }
  // iov_len が 4 の倍でない、dbg_info だけ、スロットの途中まで
  memset(&h, 0, sizeof h);
  long e1 = sethw(c, NT_ARM_HW_BREAK, &h, 10);
  long e2 = sethw(c, NT_ARM_HW_BREAK, &h, 8);
  long e3 = sethw(c, NT_ARM_HW_BREAK, &h, 8 + 8);
  long e4 = sethw(c, NT_ARM_HW_BREAK, &h, 8 + 12);
  printf(" iov 10 %ld, 8 %ld, 16 %ld, 20 %ld\n", e1, e2, e3, e4);
  // スロットの数まで、その 1 つ先まで (みな消したまま、それから 1 つ先だけ有効に)
  long n1 = sethw(c, NT_ARM_HW_BREAK, &h, 8 + 16 * nb);
  long n2 = sethw(c, NT_ARM_HW_BREAK, &h, 8 + 16 * (nb + 1));
  h.r[nb].addr = (uint64_t)bpfn;
  h.r[nb].ctrl = CTRL(0xf, 0);
  long n3 = sethw(c, NT_ARM_HW_BREAK, &h, 8 + 16 * (nb + 1));
  memset(&h, 0, sizeof h);
  long n4 = sethw(c, NT_ARM_HW_WATCH, &h, 8 + 16 * (nw + 1));
  long n5 = sethw(c, NT_ARM_HW_WATCH, &h, sizeof h);
  printf("slots: break %ld (+1 %ld, +1 on %ld), watch +1 %ld, all 16 %ld\n", n1, n2, n3, n4, n5);
  kill(c, SIGKILL);
  wait1(c);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  step();
  info();
  bp();
  badctl();
  printf("ptrace4: %d bad\n", bad);
  return bad;
}
