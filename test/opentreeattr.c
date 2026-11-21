// open_tree_attr (Linux 6.15) が Linux と同じか (root で): たしかめる順 (attr がないのに大きさがあれば EINVAL、
// open_tree の flags、たどる、それから attr の大きさ E2BIG / EINVAL、写す EFAULT、知らないところ E2BIG、印 EINVAL)、
// attr がなければ open_tree と同じ O_PATH の口。OPEN_TREE_CLONE なら写したマウントに印をつけて (AT_RECURSIVE なら
// 下のものにも)、move_mount で置くと mountinfo にその印 (もとはそのまま)。CLONE でなければ、置いてあるマウントそのものに
// (マウントの根でなければ EINVAL)。印のあるときは root でなければ EPERM
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/mount.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define A "/tmp/ota/a"
#define B "/tmp/ota/b"

struct attr {
  uint64_t set, clr, prop, userns;
  uint64_t more[4];
};

static long ota(int dfd, const char *p, unsigned fl, void *a, size_t n) {
  long r = syscall(467, dfd, p, fl, a, n);
  return r < 0 ? -errno : r;
}
static long ok(long r) {
  if (r >= 0)
    close(r);
  return r < 0 ? r : 0;
}
// mountinfo の、そのマウント先の行のマウントの印 (6 つめ。1 つの printf で何度も使うので、呼ぶたびに新しい文字)
static const char *opts(const char *p) {
  char o[256];
  FILE *f = fopen("/proc/self/mountinfo", "r");
  char l[1024], pat[256];
  snprintf(pat, sizeof pat, " %s ", p);
  strcpy(o, "-");
  while (f && fgets(l, sizeof l, f)) {
    char *s = strstr(l, pat);
    if (!s)
      continue;
    s += strlen(pat);
    char *e = strchr(s, ' ');
    if (e)
      *e = 0;
    snprintf(o, sizeof o, "%s", s);
  }
  if (f)
    fclose(f);
  return strdup(o);
}
static long mv(int fd, const char *to) {
  return syscall(SYS_move_mount, fd, "", AT_FDCWD, to, MOVE_MOUNT_F_EMPTY_PATH) ? -errno : 0;
}
static int writable(const char *p) {
  int fd = open(p, O_CREAT | O_WRONLY, 0644);
  if (fd < 0)
    return -errno;
  close(fd);
  return 0;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("opentreeattr: not root, skipped\n");
    return 0;
  }
  mkdir("/tmp/ota", 0755);
  mkdir(A, 0755);
  mkdir(B, 0755);
  if (mount("tmpfs", A, "tmpfs", 0, NULL))
    return 1;
  mkdir(A "/sub", 0755);
  mkdir(A "/s", 0755);
  struct attr z = {0}, ro = {0}, big = {0}, bad = {0}, atime = {0}, idmap = {0};
  ro.set = MOUNT_ATTR_RDONLY;
  big.set = MOUNT_ATTR_NOEXEC;
  big.more[0] = 1;
  bad.set = 0x40000000;
  atime.set = MOUNT_ATTR_NOATIME;
  idmap.set = MOUNT_ATTR_IDMAP;

  printf("checks: no attr but size %ld, bad flags %ld, recursive without clone %ld, no such %ld\n", ota(AT_FDCWD, "/nonexistent", 0, NULL, 8),
         ota(AT_FDCWD, A, 0x4000000, &z, 32), ota(AT_FDCWD, A, AT_RECURSIVE, &z, 32), ota(AT_FDCWD, "/nonexistent", 0, &ro, 32));
  printf("  too big %ld, too small %ld, more not zero %ld, more zero %ld, bad ptr %ld\n", ota(AT_FDCWD, A, 0, &ro, 8192), ota(AT_FDCWD, A, 0, &ro, 16),
         ota(AT_FDCWD, A, OPEN_TREE_CLONE, &big, 64), ok(ota(AT_FDCWD, A, OPEN_TREE_CLONE, &z, 64)), ota(AT_FDCWD, A, OPEN_TREE_CLONE, (void *)8, 32));
  printf("  unknown attr %ld, atime without clr %ld, idmap %ld, nop on a non-root %ld\n", ota(AT_FDCWD, A, OPEN_TREE_CLONE, &bad, 32),
         ota(AT_FDCWD, A, OPEN_TREE_CLONE, &atime, 32), ota(AT_FDCWD, A, OPEN_TREE_CLONE, &idmap, 32), ok(ota(AT_FDCWD, A "/sub", 0, &z, 32)));

  long fd = ota(AT_FDCWD, A, O_CLOEXEC, NULL, 0);
  printf("no attr: O_PATH %d, cloexec %d\n", fd >= 0 && (fcntl(fd, F_GETFL) & O_PATH) != 0, fd >= 0 && fcntl(fd, F_GETFD) == FD_CLOEXEC);
  if (fd >= 0)
    close(fd);

  // CLONE: 写したものに印
  fd = ota(AT_FDCWD, A, OPEN_TREE_CLONE, &ro, 32);
  long m = fd >= 0 ? mv(fd, B) : fd;
  printf("clone ro: move %ld, b %s (write %d), a %s (write %d)\n", m, opts(B), writable(B "/x"), opts(A), writable(A "/x"));
  if (fd >= 0)
    close(fd);
  umount2(B, 0);

  struct attr ns = {0};
  ns.set = MOUNT_ATTR_NOSUID | MOUNT_ATTR_NOEXEC | MOUNT_ATTR_NODEV;
  fd = ota(AT_FDCWD, A, OPEN_TREE_CLONE, &ns, 32);
  m = fd >= 0 ? mv(fd, B) : fd;
  printf("clone nosuid,nodev,noexec: move %ld, b %s, a %s\n", m, opts(B), opts(A));
  if (fd >= 0)
    close(fd);
  umount2(B, 0);

  // AT_RECURSIVE: 下のマウントにも
  if (mount("tmpfs", A "/s", "tmpfs", 0, NULL))
    return 1;
  struct attr nd = {0};
  nd.set = MOUNT_ATTR_NODEV;
  fd = ota(AT_FDCWD, A, OPEN_TREE_CLONE | AT_RECURSIVE, &nd, 32);
  m = fd >= 0 ? mv(fd, B) : fd;
  printf("recursive nodev: move %ld, b %s, b/s %s, a/s %s\n", m, opts(B), opts(B "/s"), opts(A "/s"));
  if (fd >= 0)
    close(fd);
  umount2(B, MNT_DETACH);
  umount2(A "/s", 0);

  // CLONE でなければ、置いてあるマウントそのもの
  long r1 = ok(ota(AT_FDCWD, A, 0, &ro, 32));
  const char *o1 = opts(A);
  int w1 = writable(A "/y");
  struct attr rw = {0};
  rw.clr = MOUNT_ATTR_RDONLY;
  long r2 = ok(ota(AT_FDCWD, A, 0, &rw, 32));
  printf("in place: ro %ld %s (write %d), back %ld %s (write %d), not a mount root %ld\n", r1, o1, w1, r2, opts(A), writable(A "/y"),
         ota(AT_FDCWD, A "/sub", 0, &ro, 32));

  // root でないとき: 印があれば EPERM、なければ (CLONE でなければ) 開ける
  pid_t c = fork();
  if (c == 0) {
    if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
      _exit(1);
    printf("not root: with attr %ld, without %ld, nop attr %ld, clone %ld\n", ota(AT_FDCWD, A, 0, &ro, 32), ok(ota(AT_FDCWD, A, 0, NULL, 0)),
           ota(AT_FDCWD, A, 0, &z, 32), ota(AT_FDCWD, A, OPEN_TREE_CLONE, NULL, 0));
    _exit(0);
  }
  waitpid(c, NULL, 0);

  umount2(A, MNT_DETACH);
  rmdir(A);
  rmdir(B);
  rmdir("/tmp/ota");
  return 0;
}
