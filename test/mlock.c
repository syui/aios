// mlock の仲間が Linux と同じか: mlock (ページにそろえる、すきまは ENOMEM でそこまではかける、RLIMIT_MEMLOCK をこえると
// ENOMEM、0 なら EPERM、ページを先に写す)、munlock、mlock2 (MLOCK_ONFAULT は写さない、知らない flags は EINVAL)、
// mlockall (MCL_FUTURE は新しい mmap にも、知らない flags は EINVAL)、munlockall、mmap の MAP_LOCKED (こえると EAGAIN)、
// fork した子はかけない、/proc/self/status の VmLck と smaps の Locked と VmFlags の lo
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096

static int e(long r) { return r < 0 ? -errno : (int)r; }

static long vmlck(void) {
  FILE *f = fopen("/proc/self/status", "r");
  char l[256];
  long v = -1;
  while (f && fgets(l, sizeof l, f))
    if (!strncmp(l, "VmLck:", 6))
      v = atol(l + 6);
  if (f)
    fclose(f);
  return v;
}

// smaps の a から始まる領域の Locked (kB) と VmFlags の lo
static void smaps(const char *what, void *a) {
  FILE *f = fopen("/proc/self/smaps", "r");
  char l[512], want[32];
  snprintf(want, sizeof want, "%lx-", (unsigned long)a);
  int in = 0;
  long locked = -1;
  int lo = -1;
  while (f && fgets(l, sizeof l, f)) {
    // 領域の頭の行は小文字の 16 進の番地から (中の行は大文字の名前から)
    if ((l[0] >= '0' && l[0] <= '9') || (l[0] >= 'a' && l[0] <= 'f'))
      in = 0;
    if (!strncmp(l, want, strlen(want)))
      in = 1;
    if (in && !strncmp(l, "Locked:", 7))
      locked = atol(l + 7);
    if (in && !strncmp(l, "VmFlags:", 8))
      lo = strstr(l, " lo") != NULL;
  }
  if (f)
    fclose(f);
  printf("%s: smaps Locked %ld kB, lo %d\n", what, locked, lo);
}

static int resident(char *a, int n) {
  unsigned char v[16];
  if (mincore(a, n * PG, v))
    return -errno;
  int c = 0;
  for (int i = 0; i < n; i++)
    c += v[i] & 1;
  return c;
}

static char *anon(int n, int prot) { return mmap(NULL, n * PG, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0); }

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct rlimit rl;
  getrlimit(RLIMIT_MEMLOCK, &rl);
  printf("RLIMIT_MEMLOCK %ld %ld, VmLck %ld\n", (long)rl.rlim_cur, (long)rl.rlim_max, vmlck());
  char *a = anon(4, PROT_READ | PROT_WRITE);
  int r1 = e(mlock(a, 3 * PG));
  printf("mlock 3 pages (%d, VmLck %ld, resident %d)\n", r1, vmlck(), resident(a, 4));
  smaps("locked area", a);
  smaps("the page after", a + 3 * PG);
  printf("munlock (%d, VmLck %ld)\n", e(munlock(a, 3 * PG)), vmlck());
  printf("unaligned (%d, VmLck %ld) ", e(mlock(a + 1, PG)), vmlck());
  printf("len 0 (%d), len -1 (%d), len -8192 (%d), VmLck %ld\n", e(mlock(a, 0)), e(mlock(a, (size_t)-1)), e(mlock(a, (size_t)-8192)), vmlck());
  munlock(a, 4 * PG);
  // すきま
  munmap(a + 2 * PG, PG);
  printf("gap (mlock %d, VmLck %ld, munlock %d, VmLck %ld)\n", e(mlock(a, 4 * PG)), vmlck(), e(munlock(a, 4 * PG)), vmlck());
  printf("unmapped start (mlock %d)\n", e(mlock(a + 2 * PG, PG)));
  munlock(a, 2 * PG);
  // PROT_NONE と読むだけ
  char *none = anon(1, PROT_NONE), *ro = anon(1, PROT_READ);
  printf("PROT_NONE (mlock %d, VmLck %ld), read-only (mlock %d, resident %d, VmLck %ld)\n", e(mlock(none, PG)), vmlck(), e(mlock(ro, PG)), resident(ro, 1), vmlck());
  munlock(none, PG);
  munlock(ro, PG);
  // mlock2
  char *b = anon(2, PROT_READ | PROT_WRITE);
  printf("mlock2 (flags 2 %d, ONFAULT %d, resident %d, VmLck %ld) ", e(syscall(SYS_mlock2, b, PG, 2)), e(syscall(SYS_mlock2, b, 2 * PG, MLOCK_ONFAULT)), resident(b, 2), vmlck());
  b[0] = 1;
  printf("touched resident %d\n", resident(b, 2));
  smaps("onfault area", b);
  munlock(b, 2 * PG);
  // fork した子
  mlock(a, PG);
  int p[2];
  if (pipe(p))
    return 1;
  pid_t k = fork();
  if (k == 0) {
    long v = vmlck();
    if (write(p[1], &v, sizeof v) != sizeof v)
      _exit(1);
    _exit(0);
  }
  long cv = -2;
  if (read(p[0], &cv, sizeof cv) != sizeof cv)
    cv = -3;
  waitpid(k, NULL, 0);
  printf("fork (parent VmLck %ld, child VmLck %ld)\n", vmlck(), cv);
  munlock(a, PG);
  // MAP_LOCKED
  char *ml = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
  printf("MAP_LOCKED (%d, resident %d, VmLck %ld)\n", ml != MAP_FAILED, ml != MAP_FAILED ? resident(ml, 2) : -1, vmlck());
  smaps("MAP_LOCKED area", ml);
  munmap(ml, 2 * PG);
  printf("after munmap VmLck %ld\n", vmlck());
  // mlockall
  printf("mlockall (0 %d, ONFAULT %d, 8 %d, FUTURE %d) ", e(mlockall(0)), e(mlockall(MCL_ONFAULT)), e(mlockall(8)), e(mlockall(MCL_FUTURE)));
  char *f = anon(2, PROT_READ | PROT_WRITE);
  printf("new mmap (resident %d, VmLck %ld) ", resident(f, 2), vmlck());
  printf("munlockall (%d, VmLck %ld)\n", e(munlockall()), vmlck());
  printf("FUTURE|ONFAULT (%d) ", e(mlockall(MCL_FUTURE | MCL_ONFAULT)));
  char *g = anon(2, PROT_READ | PROT_WRITE);
  printf("new mmap (resident %d, VmLck %ld)\n", resident(g, 2), vmlck());
  munlockall();
  // 限り
  rl.rlim_cur = 2 * PG;
  setrlimit(RLIMIT_MEMLOCK, &rl);
  char *c = anon(4, PROT_READ | PROT_WRITE);
  printf("limit 2 pages (3 pages %d, 2 pages %d, again overlapping %d, 1 more %d, VmLck %ld)\n", e(mlock(c, 3 * PG)), e(mlock(c, 2 * PG)), e(mlock(c, 2 * PG)), e(mlock(c + 2 * PG, PG)),
         vmlck());
  char *ml2 = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
  printf("MAP_LOCKED over the limit (%d)\n", ml2 == MAP_FAILED ? -errno : 0);
  munlock(c, 4 * PG);
  rl.rlim_cur = 0;
  setrlimit(RLIMIT_MEMLOCK, &rl);
  char *ml3 = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
  printf("limit 0 (mlock %d, mlock2 %d, mlockall %d, MAP_LOCKED %d, munlock %d, munlockall %d)\n", e(mlock(c, PG)), e(syscall(SYS_mlock2, c, PG, 0)), e(mlockall(MCL_CURRENT)),
         ml3 == MAP_FAILED ? -errno : 0, e(munlock(c, PG)), e(munlockall()));
  return 0;
}
