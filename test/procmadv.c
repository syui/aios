// process_madvise と process_mrelease (pidfd で相手のメモリに) が Linux と同じか: flags は 0 だけ、知らない知らせは EINVAL、
// pidfd でない fd は EBADF、自分にはどの知らせも (答えは知らせたバイト数)、ほかのプロセスには CAP_SYS_NICE がなければ
// EPERM、写していないところは ENOMEM。process_mrelease は終わりかけでないものは EINVAL、もう終わったもの (ゾンビ) は 0
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static long pm(int fd, struct iovec *v, size_t n, int adv, unsigned f) {
  long r = syscall(440, fd, v, n, adv, f);
  return r < 0 ? -errno : r;
}
static long mr(int fd, unsigned f) {
  long r = syscall(448, fd, f);
  return r < 0 ? -errno : r;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  char *a = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  munmap(a + 4096, 4096);
  int me = syscall(SYS_pidfd_open, getpid(), 0);
  struct iovec v = {a, 4096}, gap = {a, 3 * 4096}, two[2] = {{a, 4096}, {a + 2 * 4096, 4096}};
  printf("self (WILLNEED %ld, COLD %ld, PAGEOUT %ld, DONTNEED %ld, two %ld, vlen 0 %ld, gap %ld)\n", pm(me, &v, 1, MADV_WILLNEED, 0), pm(me, &v, 1, MADV_COLD, 0),
         pm(me, &v, 1, MADV_PAGEOUT, 0), pm(me, &v, 1, MADV_DONTNEED, 0), pm(me, two, 2, MADV_WILLNEED, 0), pm(me, &v, 0, MADV_WILLNEED, 0), pm(me, &gap, 1, MADV_WILLNEED, 0));
  printf("checks (flags 1 %ld, advice 999 %ld, not a pidfd %ld, bad fd %ld, vlen 1025 %ld, bad iovec %ld)\n", pm(me, &v, 1, MADV_WILLNEED, 1), pm(me, &v, 1, 999, 0),
         pm(0, &v, 1, MADV_WILLNEED, 0), pm(999, &v, 1, MADV_WILLNEED, 0), pm(me, &v, 1025, MADV_WILLNEED, 0), pm(me, (struct iovec *)8, 1, MADV_WILLNEED, 0));
  int p[2];
  if (pipe(p))
    return 1;
  pid_t k = fork();
  if (k == 0) {
    char c;
    close(p[1]);
    if (read(p[0], &c, 1) < 0)
      _exit(1);
    _exit(0);
  }
  int kid = syscall(SYS_pidfd_open, k, 0);
  printf("another process (WILLNEED %ld, COLD %ld, DONTNEED %ld)\n", pm(kid, &v, 1, MADV_WILLNEED, 0), pm(kid, &v, 1, MADV_COLD, 0), pm(kid, &v, 1, MADV_DONTNEED, 0));
  printf("process_mrelease (live %ld, flags 1 %ld, not a pidfd %ld, bad fd %ld)\n", mr(kid, 0), mr(kid, 1), mr(0, 0), mr(999, 0));
  close(p[1]);
  siginfo_t si;
  waitid(P_PID, k, &si, WEXITED | WNOWAIT);
  printf("process_mrelease of a zombie %ld\n", mr(kid, 0));
  waitpid(k, NULL, 0);
  return 0;
}
