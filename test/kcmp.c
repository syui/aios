// CHECKPOINT_RESTORE のもの (Linux の kcmp、/proc/PID/timers、/proc/PID/task/TID/children、/proc/sys/kernel/ns_last_pid) が
// Linux と同じか:
//   kcmp      KCMP_FILE (dup は同じ、別に開いたものはちがう)、KCMP_FILES・VM・FS・SIGHAND (スレッドと同じ、fork の子とは
//             ちがう、CLONE_FILES の子とは表が同じ)、KCMP_EPOLL_TFD (epoll に入れたものだけ)、まちがった型・fd・pid
//   timers    timer_create の ID、シグナルと値、notify (signal、none、thread と pid / tid)、時計
//   children  task/TID/children に子の pid がならぶ
//   last_pid  ns_last_pid を読める。root は書けて、次の fork はその次の pid (ほかは EPERM)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define KCMP_FILE 0
#define KCMP_VM 1
#define KCMP_FILES 2
#define KCMP_FS 3
#define KCMP_SIGHAND 4
#define KCMP_EPOLL_TFD 7

static int bad;

static long kcmp(pid_t a, pid_t b, int type, unsigned long x, unsigned long y) {
  return syscall(SYS_kcmp, a, b, type, x, y);
}

// 同じなら eq、ちがえば ne (1 と 2 は並びで、ポインタをまぜたものなので見ない)、だめなら errno の名前
static const char *cmp(pid_t a, pid_t b, int type, unsigned long x, unsigned long y) {
  long r = kcmp(a, b, type, x, y);
  if (r == 0)
    return "eq";
  if (r == 1 || r == 2)
    return "ne";
  if (r == 3)
    return "3";
  switch (errno) {
  case EBADF:
    return "EBADF";
  case EINVAL:
    return "EINVAL";
  case ESRCH:
    return "ESRCH";
  case EPERM:
    return "EPERM";
  case ENOENT:
    return "ENOENT";
  case ENOSYS:
    return "ENOSYS";
  default:
    return "other";
  }
}

static volatile int stop;
static pid_t thr_tid;
static void *thr(void *a) {
  (void)a;
  thr_tid = gettid();
  while (!stop)
    usleep(1000);
  return NULL;
}

static int child_fn(void *a) {
  (void)a;
  pause();
  return 0;
}

// /proc/self/timers の pid と tid を名前に
static void show_timers(pid_t me, pid_t tid) {
  FILE *f = fopen("/proc/self/timers", "r");
  if (!f) {
    printf("timers: open %s\n", strerror(errno));
    return;
  }
  char l[256];
  while (fgets(l, sizeof l, f)) {
    char *dot = strrchr(l, '.');
    if (!strncmp(l, "notify:", 7) && dot) {
      int n = atoi(dot + 1);
      sprintf(dot + 1, "%s\n", n == me ? "ME" : n == tid ? "TID" : "?");
    }
    printf("timers: %s", l);
  }
  fclose(f);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  pid_t me = getpid();
  // kcmp KCMP_FILE
  int a = open("/proc/self/stat", O_RDONLY), b = dup(a), c = open("/proc/self/stat", O_RDONLY);
  printf("file: dup %s, other open %s, bad fd %s, bad type %s, no pid %s\n", cmp(me, me, KCMP_FILE, a, b), cmp(me, me, KCMP_FILE, a, c), cmp(me, me, KCMP_FILE, a, 999), cmp(me, me, 99, 0, 0), cmp(me, 999999, KCMP_VM, 0, 0));
  // スレッド
  pthread_t t;
  pthread_create(&t, NULL, thr, NULL);
  while (!thr_tid)
    usleep(1000);
  printf("thread: files %s, vm %s, fs %s, sighand %s, file %s\n", cmp(me, thr_tid, KCMP_FILES, 0, 0), cmp(me, thr_tid, KCMP_VM, 0, 0), cmp(me, thr_tid, KCMP_FS, 0, 0), cmp(me, thr_tid, KCMP_SIGHAND, 0, 0), cmp(me, thr_tid, KCMP_FILE, a, b));
  // fork の子 (fd は同じ開いたもの)
  pid_t k = fork();
  if (k == 0) {
    pause();
    _exit(0);
  }
  printf("fork: files %s, vm %s, fs %s, sighand %s, inherited file %s\n", cmp(me, k, KCMP_FILES, 0, 0), cmp(me, k, KCMP_VM, 0, 0), cmp(me, k, KCMP_FS, 0, 0), cmp(me, k, KCMP_SIGHAND, 0, 0), cmp(me, k, KCMP_FILE, a, a));
  // CLONE_FILES | CLONE_FS の子
  static char stk[65536];
  pid_t cl = clone(child_fn, stk + sizeof stk, CLONE_FILES | CLONE_FS | SIGCHLD, NULL);
  printf("clone files|fs: files %s, fs %s, vm %s\n", cmp(me, cl, KCMP_FILES, 0, 0), cmp(me, cl, KCMP_FS, 0, 0), cmp(me, cl, KCMP_VM, 0, 0));
  // children
  char path[64], buf[256] = "";
  snprintf(path, sizeof path, "/proc/self/task/%d/children", me);
  int cf = open(path, O_RDONLY);
  int n = cf < 0 ? -1 : read(cf, buf, sizeof buf - 1);
  if (n >= 0)
    buf[n] = 0;
  char want[128];
  snprintf(want, sizeof want, "%d %d ", k, cl);
  char want2[128];
  snprintf(want2, sizeof want2, "%d %d ", cl, k);
  printf("children: open %d, two children %d\n", cf >= 0, !strcmp(buf, want) || !strcmp(buf, want2));
  if (cf >= 0)
    close(cf);
  snprintf(path, sizeof path, "/proc/self/task/%d/children", thr_tid);
  cf = open(path, O_RDONLY);
  n = cf < 0 ? -1 : read(cf, buf, sizeof buf - 1);
  printf("children of thread: %d bytes\n", n);
  if (cf >= 0)
    close(cf);
  kill(k, SIGKILL);
  kill(cl, SIGKILL);
  waitpid(k, NULL, 0);
  waitpid(cl, NULL, 0);
  // epoll
  int ep = epoll_create1(0);
  int pfd[2];
  pipe(pfd);
  struct epoll_event ev = {EPOLLIN, {0}};
  epoll_ctl(ep, EPOLL_CTL_ADD, pfd[0], &ev);
  struct {
    unsigned int efd, tfd, toff;
  } slot = {ep, pfd[0], 0}, slot2 = {ep, pfd[1], 0}, slot3 = {a, pfd[0], 0};
  printf("epoll: in %s, not in %s, not epoll %s\n", cmp(me, me, KCMP_EPOLL_TFD, pfd[0], (unsigned long)&slot), cmp(me, me, KCMP_EPOLL_TFD, pfd[1], (unsigned long)&slot2), cmp(me, me, KCMP_EPOLL_TFD, pfd[0], (unsigned long)&slot3));
  // 1 は root のもの
  printf("init: %s\n", getuid() == 0 ? "eq-or-ne" : cmp(me, 1, KCMP_VM, 0, 0));

  // timers
  struct sigevent sev;
  timer_t t0, t1, t2;
  memset(&sev, 0, sizeof sev);
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGRTMIN;
  sev.sigev_value.sival_int = 7;
  timer_create(CLOCK_MONOTONIC, &sev, &t0);
  sev.sigev_notify = SIGEV_NONE;
  sev.sigev_signo = SIGUSR1;
  sev.sigev_value.sival_int = 0x1234;
  timer_create(CLOCK_REALTIME, &sev, &t1);
  sev.sigev_notify = SIGEV_THREAD_ID;
  sev.sigev_signo = SIGUSR2;
  sev.sigev_value.sival_int = 9;
  sev._sigev_un._tid = thr_tid;
  timer_create(CLOCK_BOOTTIME, &sev, &t2);
  show_timers(me, thr_tid);
  timer_delete(t1);
  show_timers(me, thr_tid);
  timer_create(CLOCK_PROCESS_CPUTIME_ID, NULL, &t1);
  show_timers(me, thr_tid);
  stop = 1;
  pthread_join(t, NULL);
  // sevp なし: SIGALRM で、値は timer_t (ID)
  timer_t t3;
  timer_create(CLOCK_MONOTONIC, NULL, &t3);
  sigset_t al;
  sigemptyset(&al);
  sigaddset(&al, SIGALRM);
  sigprocmask(SIG_BLOCK, &al, NULL);
  struct itimerspec one = {{0, 0}, {0, 1000000}};
  timer_settime(t3, 0, &one, NULL);
  siginfo_t si;
  int got = sigwaitinfo(&al, &si);
  printf("null sevp: id %ld, sig %d, code SI_TIMER %d, value is id %d, tid is id %d\n", (long)(intptr_t)t3, got, si.si_code == SI_TIMER, si.si_value.sival_int == (int)(intptr_t)t3, si.si_timerid == (int)(intptr_t)t3);
  timer_delete(t3);
  timer_delete(t1);
  timer_delete(t0);
  timer_delete(t2);

  // ns_last_pid
  int lf = open("/proc/sys/kernel/ns_last_pid", O_RDWR);
  int ro = lf < 0 ? open("/proc/sys/kernel/ns_last_pid", O_RDONLY) : lf;
  char lb[32] = "";
  n = ro < 0 ? -1 : pread(ro, lb, sizeof lb - 1, 0);
  printf("ns_last_pid: open rw %s, read %d\n", lf >= 0 ? "ok" : strerror(errno), n > 0 && atoi(lb) > 0);
  if (lf >= 0) {
    char w[32];
    int want_pid = atoi(lb) + 100;
    int len = snprintf(w, sizeof w, "%d", want_pid - 1);
    long wr = pwrite(lf, w, len, 0);
    pid_t z = fork();
    if (z == 0)
      _exit(0);
    waitpid(z, NULL, 0);
    printf("ns_last_pid write: %ld, next fork is that + 1 %d\n", wr, z == want_pid);
  }
  printf("kcmp: %d bad\n", bad);
  return bad;
}
