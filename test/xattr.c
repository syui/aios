// xattr が Linux と同じか (引数のディレクトリで: tmpfs と ext4): setxattr / getxattr / listxattr / removexattr と l・f の
// もの。名前の長さ (0 と 256 は ERANGE)、知らない名前空間 (EOPNOTSUPP)、"user." だけ (EINVAL)、大きさ (65537 は E2BIG、
// 小さい buf は ERANGE、0 は大きさ)、XATTR_CREATE と XATTR_REPLACE、user.* はふつうのファイルとディレクトリだけ (リンクと
// FIFO は EPERM、読むのは ENODATA)、trusted.* と security.* は root でなければ (EPERM、読むのは ENODATA、一覧に出ない)、
// 書けないファイルは EACCES、O_PATH は EBADF、ext4 は値がブロックに入らなければ ENOSPC、xattr のブロックで st_blocks が
// 増える
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>

static int e(long r) { return r < 0 ? -errno : (int)r; }

static void list(const char *what, const char *p) {
  char b[4096];
  int n = e(llistxattr(p, b, sizeof b));
  printf("%s list %d [", what, n);
  for (int i = 0; i < n; i += strlen(b + i) + 1)
    printf("%s%s", i ? " " : "", b + i);
  printf("]\n");
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char *dir = argc > 1 ? argv[1] : "/tmp";
  char d[512], f[600], sd[600], l[600], fifo[600];
  snprintf(d, sizeof d, "%s/xattrtest.%d", dir, getpid());
  if (mkdir(d, 0755))
    return 1;
  snprintf(f, sizeof f, "%s/f", d);
  snprintf(sd, sizeof sd, "%s/sub", d);
  snprintf(l, sizeof l, "%s/link", d);
  snprintf(fifo, sizeof fifo, "%s/fifo", d);
  close(open(f, O_CREAT | O_WRONLY, 0644));
  mkdir(sd, 0755);
  symlink("f", l);
  mkfifo(fifo, 0644);
  int s = e(setxattr(f, "user.a", "hello", 5, 0));
  printf("set user.a %d\n", s);
  if (s == -EOPNOTSUPP) {
    list("unsupported", f);
    printf("get %d, remove %d\n", e(getxattr(f, "user.a", NULL, 0)), e(removexattr(f, "user.a")));
    return 0;
  }
  char v[256] = "";
  printf("get (size %d, small %d, value %d", e(getxattr(f, "user.a", NULL, 0)), e(getxattr(f, "user.a", v, 3)), e(getxattr(f, "user.a", v, sizeof v)));
  printf(" \"%.5s\", missing %d)\n", v, e(getxattr(f, "user.zz", v, sizeof v)));
  char big[256];
  memset(big, 'n', 255);
  big[255] = 0;
  char *huge = calloc(1, 70000);
  printf("names (empty %d, 255 chars %d, unknown namespace %d, user. alone %d), value 65537 %d, flags 4 %d\n", e(setxattr(f, "", "x", 1, 0)), e(setxattr(f, big, "x", 1, 0)),
         e(setxattr(f, "foo.bar", "x", 1, 0)), e(setxattr(f, "user.", "x", 1, 0)), e(setxattr(f, "user.h", huge, 65537, 0)), e(setxattr(f, "user.a", "x", 1, 4)));
  printf("CREATE existing %d, REPLACE missing %d, REPLACE %d, empty value %d (get %d)\n", e(setxattr(f, "user.a", "x", 1, XATTR_CREATE)), e(setxattr(f, "user.b", "x", 1, XATTR_REPLACE)),
         e(setxattr(f, "user.a", "world", 5, XATTR_REPLACE)), e(setxattr(f, "user.b", "", 0, 0)), e(getxattr(f, "user.b", v, sizeof v)));
  setxattr(f, "user.c", "third", 5, 0);
  list("file", f);
  char lb[8];
  printf("list (size %d, small %d)\n", e(listxattr(f, NULL, 0)), e(listxattr(f, lb, sizeof lb)));
  printf("remove (%d, again %d)\n", e(removexattr(f, "user.b")), e(removexattr(f, "user.b")));
  list("after remove", f);
  // ディレクトリ、リンク、FIFO
  printf("dir %d, link (lset %d, lget %d, follow %d), fifo (set %d, get %d)\n", e(setxattr(sd, "user.d", "dir", 3, 0)), e(lsetxattr(l, "user.l", "x", 1, 0)),
         e(lgetxattr(l, "user.a", v, sizeof v)), e(getxattr(l, "user.a", v, sizeof v)), e(setxattr(fifo, "user.p", "x", 1, 0)), e(getxattr(fifo, "user.p", v, sizeof v)));
  // root でなければ
  if (geteuid() != 0) {
    printf("not root (trusted set %d get %d, security set %d get %d, security.capability set %d)\n", e(setxattr(f, "trusted.t", "x", 1, 0)), e(getxattr(f, "trusted.t", v, sizeof v)),
           e(setxattr(f, "security.s", "x", 1, 0)), e(getxattr(f, "security.s", v, sizeof v)), e(setxattr(f, "security.capability", "x", 1, 0)));
  } else {
    printf("root (trusted set %d get %d, security set %d get %d)\n", e(setxattr(f, "trusted.t", "tt", 2, 0)), e(getxattr(f, "trusted.t", v, sizeof v)), e(setxattr(f, "security.s", "ss", 2, 0)),
           e(getxattr(f, "security.s", v, sizeof v)));
    list("root", f);
  }
  // fd
  int fd = open(f, O_RDONLY), pfd = open(f, O_PATH);
  printf("fd (fset %d, fget %d, flist %d, O_PATH fget %d, bad fd %d)\n", e(fsetxattr(fd, "user.e", "fd", 2, 0)), e(fgetxattr(fd, "user.e", v, sizeof v)), e(flistxattr(fd, NULL, 0)),
         e(fgetxattr(pfd, "user.e", v, sizeof v)), e(fgetxattr(999, "user.e", v, sizeof v)));
  // 書けないファイル
  chmod(f, 0444);
  printf("read-only file (set %d, get %d, remove %d)\n", e(setxattr(f, "user.r", "x", 1, 0)), e(getxattr(f, "user.a", v, sizeof v)), e(removexattr(f, "user.a")));
  chmod(f, 0644);
  // 大きな値と st_blocks
  struct stat st;
  stat(f, &st);
  long b0 = st.st_blocks;
  char *val = malloc(5000);
  memset(val, 'v', 5000);
  int r3 = e(setxattr(f, "user.big", val, 3000, 0));
  stat(f, &st);
  long b1 = st.st_blocks;
  int r5 = e(setxattr(f, "user.bigger", val, 5000, 0));
  printf("big values (3000 %d, blocks +%ld, 5000 %d)\n", r3, b1 - b0, r5);
  printf("get big %d\n", e(getxattr(f, "user.big", NULL, 0)));
  list("final", f);
  removexattr(f, "user.big");
  stat(f, &st);
  printf("after removing big, blocks +%ld\n", (long)st.st_blocks - b0);
  unlink(f);
  unlink(l);
  unlink(fifo);
  rmdir(sd);
  rmdir(d);
  return 0;
}
