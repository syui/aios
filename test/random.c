// 乱数が Linux と同じにふるまうか (カーネル: rand.rs の ChaCha20、syscall.rs の getrandom、file.rs の /dev/urandom、
// exec.rs の AT_RANDOM、sysctl.rs の /proc/sys/kernel/random)。Linux (このマシン) でも aios でも同じに通る
//   getrandom: flags (知らないもの、GRND_RANDOM|GRND_INSECURE は EINVAL)、0 バイト、1 MiB、書けないところ
//   (EFAULT と、書けないページの手前までの数)、/dev/random と /dev/urandom (読む、だれでも書ける)、
//   AT_RANDOM (exec ごとにちがう)、/proc/sys/kernel/random (entropy_avail と poolsize は 256、boot_id は同じ、
//   uuid は読むたびにちがう)、それらしく見えるか (1 MiB のバイトの χ² と 1 のビットの数、64 KiB に同じ 16 バイトがないか)
// gcc -O2 -static -o random test/random.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define GRND_NONBLOCK_ 1
#define GRND_RANDOM_ 2
#define GRND_INSECURE_ 4

static int fail, n;

static void ok(int cond, const char *what) {
  n++;
  if (!cond) {
    printf("FAIL %s\n", what);
    fail++;
  }
}

static long gr(void *buf, size_t len, unsigned flags) { return syscall(SYS_getrandom, buf, len, flags); }

static int cmp16(const void *a, const void *b) { return memcmp(a, b, 16); }

// ファイルの 1 行め (改行なし)
static int line(const char *path, char *out, int size) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) return -1;
  int k = read(fd, out, size - 1);
  close(fd);
  if (k <= 0) return -1;
  out[k] = 0;
  out[strcspn(out, "\n")] = 0;
  return 0;
}

// UUID の形: 8-4-4-4-12、版 4、variant 10xx
static int is_uuid(const char *s) {
  if (strlen(s) != 36 || s[8] != '-' || s[13] != '-' || s[18] != '-' || s[23] != '-') return 0;
  return s[14] == '4' && strchr("89ab", s[19]) != NULL;
}

// 子 (exec した自分) の AT_RANDOM (16 進)
static int child_at_random(const char *self, char *out) {
  int p[2];
  if (pipe(p) < 0) return -1;
  pid_t pid = fork();
  if (pid == 0) {
    dup2(p[1], 1);
    close(p[0]);
    execl(self, self, "at-random", (char *)0);
    _exit(127);
  }
  close(p[1]);
  int k = read(p[0], out, 33);
  close(p[0]);
  waitpid(pid, 0, 0);
  if (k < 32) return -1;
  out[32] = 0;
  return 0;
}

static void hex16(const unsigned char *p, char *out) {
  for (int i = 0; i < 16; i++) sprintf(out + 2 * i, "%02x", p[i]);
}

int main(int argc, char **argv) {
  if (argc > 1 && strcmp(argv[1], "at-random") == 0) {
    const unsigned char *p = (const unsigned char *)getauxval(AT_RANDOM);
    char h[33];
    if (!p) return 1;
    hex16(p, h);
    printf("%s", h);
    return 0;
  }
  unsigned char a[64], b[64];

  // getrandom
  ok(gr(a, sizeof a, 0) == 64, "getrandom 64");
  ok(gr(b, sizeof b, 0) == 64 && memcmp(a, b, 64) != 0, "getrandom twice differ");
  ok(gr(a, 0, 0) == 0, "getrandom 0 bytes");
  ok(gr(a, 64, GRND_NONBLOCK_) == 64, "GRND_NONBLOCK (ready)");
  ok(gr(a, 64, GRND_RANDOM_) == 64, "GRND_RANDOM");
  ok(gr(a, 64, GRND_INSECURE_) == 64, "GRND_INSECURE");
  errno = 0;
  ok(gr(a, 64, 8) == -1 && errno == EINVAL, "unknown flag: EINVAL");
  errno = 0;
  ok(gr(a, 64, GRND_RANDOM_ | GRND_INSECURE_) == -1 && errno == EINVAL, "GRND_RANDOM|GRND_INSECURE: EINVAL");
  errno = 0;
  ok(gr((void *)8, 16, 0) == -1 && errno == EFAULT, "bad pointer: EFAULT");
  long pg = sysconf(_SC_PAGESIZE);
  unsigned char *m = mmap(0, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ok(m != MAP_FAILED && munmap(m + pg, pg) == 0, "setup: a page and a hole");
  long r = gr(m + pg - 100, 200, 0);
  if (!(r > 0 && r <= 100)) printf("  partial: %ld\n", r);
  ok(r > 0 && r <= 100, "100 bytes before a hole: up to the hole");

  // 1 MiB と、それらしいか: バイトの χ² (自由度 255: 平均 255、σ 22.6)、1 のビット (平均 4194304、σ 1448)
  size_t big = 1 << 20;
  unsigned char *buf = malloc(big);
  ok(gr(buf, big, 0) == (long)big, "getrandom 1 MiB");
  unsigned long cnt[256] = {0};
  long ones = 0;
  for (size_t i = 0; i < big; i++) {
    cnt[buf[i]]++;
    ones += __builtin_popcount(buf[i]);
  }
  double e = big / 256.0, chi = 0;
  for (int i = 0; i < 256; i++) chi += (cnt[i] - e) * (cnt[i] - e) / e;
  if (!(chi > 120 && chi < 420)) printf("  chi-square %.1f\n", chi);
  ok(chi > 120 && chi < 420, "bytes look uniform (chi-square)");
  if (labs(ones - (long)big * 4) >= 6 * 1448) printf("  ones %ld\n", ones);
  ok(labs(ones - (long)big * 4) < 6 * 1448, "bits look balanced");
  qsort(buf, 4096, 16, cmp16);
  int dup = 0;
  for (int i = 1; i < 4096; i++) dup |= memcmp(buf + 16 * (i - 1), buf + 16 * i, 16) == 0;
  ok(!dup, "no 16-byte block twice in 64 KiB");
  free(buf);

  // /dev/urandom と /dev/random (どちらも待たない。だれが書いてもよい)
  int fd = open("/dev/urandom", O_RDWR);
  ok(fd >= 0 && read(fd, a, 64) == 64, "read /dev/urandom");
  ok(fd >= 0 && write(fd, "aios", 4) == 4, "write /dev/urandom");
  close(fd);
  fd = open("/dev/random", O_RDONLY | O_NONBLOCK);
  ok(fd >= 0 && read(fd, b, 64) == 64 && memcmp(a, b, 64) != 0, "read /dev/random");
  close(fd);

  // AT_RANDOM: 自分と、exec した子 2 つで、みなちがう
  const unsigned char *at = (const unsigned char *)getauxval(AT_RANDOM);
  char mine[33] = "", c1[34] = "", c2[34] = "";
  if (at) hex16(at, mine);
  ok(at && child_at_random(argv[0], c1) == 0 && child_at_random(argv[0], c2) == 0, "AT_RANDOM of children");
  ok(strcmp(mine, c1) && strcmp(mine, c2) && strcmp(c1, c2) && strcmp(mine, "00000000000000000000000000000000"),
     "AT_RANDOM differs per exec");

  // /proc/sys/kernel/random
  char s1[64], s2[64];
  ok(line("/proc/sys/kernel/random/entropy_avail", s1, sizeof s1) == 0 && !strcmp(s1, "256"), "entropy_avail 256");
  ok(line("/proc/sys/kernel/random/poolsize", s1, sizeof s1) == 0 && !strcmp(s1, "256"), "poolsize 256");
  ok(line("/proc/sys/kernel/random/boot_id", s1, sizeof s1) == 0 && line("/proc/sys/kernel/random/boot_id", s2, sizeof s2) == 0 &&
         is_uuid(s1) && !strcmp(s1, s2),
     "boot_id stays");
  ok(line("/proc/sys/kernel/random/uuid", s1, sizeof s1) == 0 && line("/proc/sys/kernel/random/uuid", s2, sizeof s2) == 0 &&
         is_uuid(s1) && is_uuid(s2) && strcmp(s1, s2),
     "uuid differs per read");

  printf("random: %d checks, %d failed\n", n, fail);
  return fail != 0;
}
