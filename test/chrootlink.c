// chroot の中と外で、/ で始まるリンクのたどり先がちがうか (root で): 中ではその根から、外では本当の根から。
// 外が先にたどっても、中が先にたどっても (パスの答えの覚えを、ほかの根の人の答えで使わない)。ちがえば 1 で終わる
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define D "/tmp/crl"

static void put(const char *p, const char *s) {
  int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  if (fd >= 0) {
    write(fd, s, strlen(s));
    close(fd);
  }
}
static const char *get(const char *p) {
  static char b[4][32];
  static int k;
  char *r = b[k++ % 4];
  int fd = open(p, O_RDONLY);
  if (fd < 0)
    return "(none)";
  long n = read(fd, r, 31);
  close(fd);
  r[n < 0 ? 0 : n] = 0;
  return r;
}
static int bad;
static void inside(const char *what, const char *link) {
  pid_t c = fork();
  if (c == 0) {
    if (chroot(D) || chdir("/"))
      _exit(1);
    char p[64];
    snprintf(p, sizeof p, "/%s", link);
    struct stat s;
    long sz = stat(p, &s) ? -1 : (long)s.st_size;
    const char *got = get(p);
    printf("%s: inside %s (size %ld)\n", what, got, sz);
    _exit(strcmp(got, "the inner one") != 0);
  }
  int st;
  waitpid(c, &st, 0);
  bad |= !WIFEXITED(st) || WEXITSTATUS(st) != 0;
}
static void outside(const char *what, const char *link) {
  char p[64];
  snprintf(p, sizeof p, D "/%s", link);
  struct stat s;
  long sz = stat(p, &s) ? -1 : (long)s.st_size;
  const char *got = get(p);
  printf("%s: outside %s (size %ld)\n", what, got, sz);
  bad |= strcmp(got, "outer") != 0;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("chrootlink: not root, skipped\n");
    return 0;
  }
  mkdir(D, 0755);
  mkdir(D "/tmp", 0755);
  put("/tmp/crl-x", "outer");
  put(D "/tmp/crl-x", "the inner one");
  symlink("/tmp/crl-x", D "/a");
  symlink("/tmp/crl-x", D "/b");
  mkdir(D "/d", 0755);
  symlink("/tmp", D "/d/t");
  outside("a, outside first", "a");
  inside("a, then", "a");
  inside("b, inside first", "b");
  outside("b, then", "b");
  inside("d/t/crl-x, inside first", "d/t/crl-x");
  outside("d/t/crl-x, then", "d/t/crl-x");
  unlink(D "/a");
  unlink(D "/b");
  unlink(D "/d/t");
  rmdir(D "/d");
  unlink(D "/tmp/crl-x");
  rmdir(D "/tmp");
  rmdir(D);
  unlink("/tmp/crl-x");
  return bad;
}
