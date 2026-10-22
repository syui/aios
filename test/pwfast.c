// 大きなロックなしの pwrite (カーネルの vm::write_fast) が、Linux と同じ答えのままか: aios の中で動かす
// (zig cc -target aarch64-linux-musl -O2 -static -pthread -o pwfast test/pwfast.c)。aios は、ext4 のファイルの
// ページキャッシュにあるところを、大きさを変えずに、inode の変わらない秒のうちに書きかえるだけの pwrite を、
// 大きなロックを取らずにページへ写す (書き出しは flush がまとめて)。それでも:
//   cover     でたらめな場所と長さ (ページをまたぐものも) に 2 万回書いて、読んだものが書いたものと同じ。開きなおしても
//   mtime     秒が変わったら、書くと更新時刻が進む (大きさは変わらない)
//   suid      setuid / setgid (グループの実行あり) のファイルに root でない人が書くと、その印が落ちる
//   inotify   IN_MODIFY の見張りを置いたら、書くたびにできごとが来る
//   mmap      MAP_SHARED で写したところに、書いたものがすぐ見える
//   atomic    2 つのスレッドが同じところ (ページをまたぐ) に 'A' と 'B' を書きあい、3 つめが終わりを伸ばす (ふつうの道)。
//             終わったとき、そこはみな 'A' かみな 'B' (書きこみはまざらない: Linux は i_rwsem で 1 つずつ)
//   procs     4 つのプロセスが自分のファイルにでたらめに書いて、読んだものが同じ
//   efault    2 ページめが写っていないバッファから書く: 返した数と、ファイルの中身があう
//   fsync     fsync のあと、書いたページがディスクに書き出されている (/proc/ai/fs の書いたブロックの数が増える。aios だけ)
// aios なら、/proc/bkl のロックなしの pwrite の数が ext4 で増えたか (速い道を通ったか) も見る。Linux でも同じに通る。
// 1 行に ok NAME か FAIL NAME、おしまいに pwfast: N failed
//   pwfast [DIR...]   DIR (既定 /tmp) ごとに、その下に pw-PID-* を作って
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails;
static char label[64];
#define CHECK(name, cond) do { if (cond) printf("ok %s %s\n", label, name); else { printf("FAIL %s %s\n", label, name); fails++; } } while (0)

static unsigned long long rng = 88172645463325252ULL;
static unsigned rnd(void) {
  rng ^= rng << 13;
  rng ^= rng >> 7;
  rng ^= rng << 17;
  return (unsigned)rng;
}

static char path_buf[512];
static const char *name(const char *dir, const char *what) {
  snprintf(path_buf, sizeof path_buf, "%s/pw-%d-%s", dir, (int)getpid(), what);
  return path_buf;
}

// /proc/bkl のロックなしの pwrite の数 (aios でなければ -1)
static long fast_count(void) {
  FILE *f = fopen("/proc/bkl", "r");
  if (!f) return -1;
  char line[512];
  long n = -1;
  while (fgets(line, sizeof line, f)) {
    char *p = strstr(line, "(vm::write_fast): ");
    if (p) n = atol(p + strlen("(vm::write_fast): "));
  }
  fclose(f);
  return n;
}

// /proc/ai/fs の、ファイルの中身として書いたブロックの数を足したもの (aios でなければ -1)
static long data_written(void) {
  FILE *f = fopen("/proc/ai/fs", "r");
  if (!f) return -1;
  static char buf[1 << 16];
  size_t n = fread(buf, 1, sizeof buf - 1, f);
  fclose(f);
  buf[n] = 0;
  long sum = 0;
  int found = 0;
  for (char *p = buf; (p = strstr(p, "\"written_data\":")); p += 15) {
    sum += atol(p + 15);
    found = 1;
  }
  return found ? sum : -1;
}

static int full_pread(int fd, char *b, size_t n, off_t off) {
  while (n > 0) {
    ssize_t r = pread(fd, b, n, off);
    if (r <= 0) return -1;
    b += r;
    n -= r;
    off += r;
  }
  return 0;
}

static int same_file(int fd, const char *want, size_t n) {
  char *got = malloc(n);
  int ok = got && full_pread(fd, got, n, 0) == 0 && memcmp(got, want, n) == 0;
  free(got);
  return ok;
}

// ---- cover ----
#define CSIZE (64 * 1024)
static void cover(const char *dir) {
  const char *p = name(dir, "cover");
  int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  static char want[CSIZE], buf[8192];
  for (int i = 0; i < CSIZE; i++) want[i] = (char)(i * 7);
  int ok = fd >= 0 && pwrite(fd, want, CSIZE, 0) == CSIZE;
  for (int k = 0; k < 20000 && ok; k++) {
    size_t len = 1 + rnd() % (k % 3 == 0 ? 8192 : 600);
    off_t off = rnd() % (CSIZE - len + 1);
    for (size_t i = 0; i < len; i++) buf[i] = (char)rnd();
    if (pwrite(fd, buf, len, off) != (ssize_t)len) ok = 0;
    memcpy(want + off, buf, len);
    // ときどき読んでたしかめる
    if (k % 997 == 0 && !same_file(fd, want, CSIZE)) ok = 0;
  }
  struct stat st;
  CHECK("cover", ok && same_file(fd, want, CSIZE) && fstat(fd, &st) == 0 && st.st_size == CSIZE);
  close(fd);
  fd = open(p, O_RDONLY);
  CHECK("cover-reopen", fd >= 0 && same_file(fd, want, CSIZE));
  close(fd);
  unlink(p);
}

// ---- mtime ----
static void mtime(const char *dir) {
  const char *p = name(dir, "mtime");
  int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  char b[4096];
  memset(b, 'm', sizeof b);
  int ok = fd >= 0 && pwrite(fd, b, sizeof b, 0) == (ssize_t)sizeof b && pwrite(fd, b, 100, 10) == 100;
  struct stat a, z;
  fstat(fd, &a);
  // 次の秒の頭まで待つ
  struct timespec now;
  clock_gettime(CLOCK_REALTIME, &now);
  long ns = 1000000000L - now.tv_nsec + 50000000L;
  struct timespec d = {ns / 1000000000L, ns % 1000000000L};
  nanosleep(&d, 0);
  ok = ok && pwrite(fd, b, 100, 200) == 100 && fstat(fd, &z) == 0;
  if (ok && !(z.st_mtime > a.st_mtime && z.st_ctime >= z.st_mtime && z.st_size == a.st_size))
    printf("  mtime: %ld -> %ld, ctime %ld, size %ld -> %ld\n", (long)a.st_mtime, (long)z.st_mtime, (long)z.st_ctime, (long)a.st_size, (long)z.st_size);
  CHECK("mtime", ok && z.st_mtime > a.st_mtime && z.st_ctime >= z.st_mtime && z.st_size == a.st_size);
  close(fd);
  unlink(p);
}

// ---- suid ----
static void suid(const char *dir) {
  if (geteuid() == 0) {
    printf("ok %s suid (root: skipped)\n", label);
    return;
  }
  const char *p = name(dir, "suid");
  int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  char b[4096];
  memset(b, 's', sizeof b);
  int ok = fd >= 0 && pwrite(fd, b, sizeof b, 0) == (ssize_t)sizeof b && pwrite(fd, b, 10, 0) == 10;
  ok = ok && fchmod(fd, 06755) == 0;
  struct stat st;
  ok = ok && pwrite(fd, b, 10, 20) == 10 && fstat(fd, &st) == 0;
  if (ok && (st.st_mode & 06000)) printf("  suid: mode %o\n", st.st_mode & 07777);
  CHECK("suid", ok && (st.st_mode & 06000) == 0);
  close(fd);
  unlink(p);
}

// ---- inotify ----
static void notify(const char *dir) {
  const char *p = name(dir, "notify");
  char path[512];
  snprintf(path, sizeof path, "%s", p);
  int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
  char b[4096];
  memset(b, 'i', sizeof b);
  int ok = fd >= 0 && pwrite(fd, b, sizeof b, 0) == (ssize_t)sizeof b && pwrite(fd, b, 10, 0) == 10;
  int in = inotify_init1(IN_NONBLOCK);
  ok = ok && in >= 0 && inotify_add_watch(in, path, IN_MODIFY) >= 0;
  int events = 0;
  for (int k = 0; k < 3 && ok; k++) {
    if (pwrite(fd, b, 10, 100 * k) != 10) ok = 0;
    struct pollfd pf = {in, POLLIN, 0};
    char ev[4096] __attribute__((aligned(8)));
    if (poll(&pf, 1, 1000) == 1 && read(in, ev, sizeof ev) > 0 && (((struct inotify_event *)ev)->mask & IN_MODIFY)) events++;
  }
  CHECK("inotify", ok && events == 3);
  close(in);
  close(fd);
  unlink(path);
}

// ---- mmap ----
static void mapped(const char *dir) {
  const char *p = name(dir, "mmap");
  int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  static char b[16384];
  memset(b, 'x', sizeof b);
  int ok = fd >= 0 && pwrite(fd, b, sizeof b, 0) == (ssize_t)sizeof b;
  char *m = ok ? mmap(0, sizeof b, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) : MAP_FAILED;
  ok = ok && m != MAP_FAILED;
  for (int k = 0; k < 2000 && ok; k++) {
    size_t len = 1 + rnd() % 5000;
    off_t off = rnd() % (sizeof b - len + 1);
    char c = (char)('a' + k % 26);
    char w[5000];
    memset(w, c, len);
    if (pwrite(fd, w, len, off) != (ssize_t)len || m[off] != c || m[off + len - 1] != c) ok = 0;
    // 写したところから書いたものも、pread で見える
    m[(off + len) % sizeof b] = 'Q';
    char r;
    if (pread(fd, &r, 1, (off + len) % sizeof b) != 1 || r != 'Q') ok = 0;
  }
  CHECK("mmap", ok);
  if (m != MAP_FAILED) munmap(m, sizeof b);
  close(fd);
  unlink(p);
}

// ---- atomic ----
#define AOFF 6144
#define ALEN 4096
static int afd;
static volatile int astop;
static void *awriter(void *arg) {
  char b[ALEN];
  memset(b, (int)(long)arg, sizeof b);
  for (int k = 0; k < 20000; k++)
    if (pwrite(afd, b, sizeof b, AOFF) != (ssize_t)sizeof b) return (void *)1;
  return 0;
}
static void *agrower(void *arg) {
  (void)arg;
  off_t end = 16384;
  while (!astop) {
    if (pwrite(afd, "g", 1, end++) != 1) return (void *)1;
    usleep(200);
  }
  return 0;
}
static void atomic_test(const char *dir) {
  const char *p = name(dir, "atomic");
  afd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  static char z[16384];
  memset(z, '.', sizeof z);
  int ok = afd >= 0 && pwrite(afd, z, sizeof z, 0) == (ssize_t)sizeof z;
  astop = 0;
  pthread_t a, b, g;
  pthread_create(&g, 0, agrower, 0);
  pthread_create(&a, 0, awriter, (void *)'A');
  pthread_create(&b, 0, awriter, (void *)'B');
  void *ra, *rb, *rg;
  pthread_join(a, &ra);
  pthread_join(b, &rb);
  astop = 1;
  pthread_join(g, &rg);
  char got[ALEN];
  ok = ok && !ra && !rb && !rg && full_pread(afd, got, ALEN, AOFF) == 0;
  int uniform = 1;
  for (int i = 1; i < ALEN; i++)
    if (got[i] != got[0]) uniform = 0;
  if (ok && !uniform) printf("  atomic: mixed ('%c' at 0, '%c' at %d)\n", got[0], got[ALEN - 1], ALEN - 1);
  CHECK("atomic", ok && uniform && (got[0] == 'A' || got[0] == 'B'));
  close(afd);
  unlink(p);
}

// ---- procs ----
#define PSIZE (256 * 1024)
static int proc_body(const char *p, unsigned seed) {
  rng ^= seed * 2654435761u;
  int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  char *want = malloc(PSIZE);
  if (fd < 0 || !want) return 1;
  for (int i = 0; i < PSIZE; i++) want[i] = (char)(i ^ seed);
  if (pwrite(fd, want, PSIZE, 0) != PSIZE) return 2;
  char buf[9000];
  for (int k = 0; k < 5000; k++) {
    size_t len = 1 + rnd() % (k % 4 == 0 ? 9000 : 300);
    off_t off = rnd() % (PSIZE - len + 1);
    for (size_t i = 0; i < len; i++) buf[i] = (char)rnd();
    if (pwrite(fd, buf, len, off) != (ssize_t)len) return 3;
    memcpy(want + off, buf, len);
  }
  int r = same_file(fd, want, PSIZE) ? 0 : 4;
  close(fd);
  unlink(p);
  return r;
}
static void procs(const char *dir) {
  pid_t pid[4];
  for (int i = 0; i < 4; i++) {
    char p[512];
    snprintf(p, sizeof p, "%s/pw-%d-proc%d", dir, (int)getpid(), i);
    pid[i] = fork();
    if (pid[i] == 0) {
      alarm(120);
      _exit(proc_body(p, (unsigned)i + 1));
    }
  }
  int ok = 1;
  for (int i = 0; i < 4; i++) {
    int ws;
    waitpid(pid[i], &ws, 0);
    if (!WIFEXITED(ws) || WEXITSTATUS(ws) != 0) {
      printf("  procs: child %d status %d\n", i, WIFEXITED(ws) ? WEXITSTATUS(ws) : -1);
      ok = 0;
    }
  }
  CHECK("procs", ok);
}

// ---- efault ----
static void efault(const char *dir) {
  const char *p = name(dir, "efault");
  int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  static char z[16384], got[16384];
  memset(z, '.', sizeof z);
  int ok = fd >= 0 && pwrite(fd, z, sizeof z, 0) == (ssize_t)sizeof z && pwrite(fd, z, 10, 0) == 10;
  long pg = sysconf(_SC_PAGESIZE);
  char *m = mmap(0, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ok = ok && m != MAP_FAILED;
  if (ok) {
    memset(m, 'E', pg);
    munmap(m + pg, pg);
    ssize_t r = pwrite(fd, m, 2 * pg, 100);
    int e = errno;
    ok = full_pread(fd, got, sizeof got, 0) == 0;
    // 返した数のぶんだけ 'E' になり、ほかは前のまま
    size_t n = r > 0 ? (size_t)r : 0;
    for (size_t i = 0; i < sizeof got && ok; i++) {
      char w = (i >= 100 && i < 100 + n) ? 'E' : '.';
      if (got[i] != w) {
        printf("  efault: returned %ld (errno %d), byte %zu is '%c'\n", (long)r, e, i, got[i]);
        ok = 0;
      }
    }
    ok = ok && (r == pg || (r < 0 && e == EFAULT));
    munmap(m, pg);
  }
  CHECK("efault", ok);
  close(fd);
  unlink(p);
}

// ---- fsync ----
static void syncs(const char *dir, int ext4) {
  const char *p = name(dir, "fsync");
  int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644);
  static char b[64 * 1024];
  memset(b, 'f', sizeof b);
  int ok = fd >= 0 && pwrite(fd, b, sizeof b, 0) == (ssize_t)sizeof b && fsync(fd) == 0;
  long before = data_written();
  // 16 ページに 1 バイトずつ (どれもページキャッシュにある書きかえ)
  for (int i = 0; i < 16 && ok; i++)
    if (pwrite(fd, "F", 1, i * 4096 + 7) != 1) ok = 0;
  ok = ok && fsync(fd) == 0;
  long after = data_written();
  if (ext4 && before >= 0 && ok && after - before < 16) printf("  fsync: wrote %ld data blocks (want 16)\n", after - before);
  CHECK("fsync", ok && (!ext4 || before < 0 || after - before >= 16));
  close(fd);
  unlink(p);
}

// DIR が ext4 か (statfs の f_type で)
static int is_ext4(const char *dir) {
  struct statfs s;
  return statfs(dir, &s) == 0 && s.f_type == 0xEF53;
}

int main(int argc, char **argv) {
  setvbuf(stdout, 0, _IOLBF, 0);
  int n = argc > 1 ? argc - 1 : 1;
  for (int i = 0; i < n; i++) {
    const char *dir = argc > 1 ? argv[i + 1] : "/tmp";
    int ext4 = is_ext4(dir);
    snprintf(label, sizeof label, "%s", dir);
    long f0 = fast_count();
    cover(dir);
    long f1 = fast_count();
    mtime(dir);
    suid(dir);
    notify(dir);
    mapped(dir);
    atomic_test(dir);
    procs(dir);
    efault(dir);
    syncs(dir, ext4);
    // aios の ext4 なら、cover の書きかえの多くは大きなロックなしの道を通る
    if (f0 >= 0 && ext4) {
      if (f1 - f0 < 10000) printf("  fast: %ld of 20000 pwrites in cover\n", f1 - f0);
      CHECK("fast-path", f1 - f0 >= 10000);
    }
  }
  printf("pwfast: %d failed\n", fails);
  return fails != 0;
}
