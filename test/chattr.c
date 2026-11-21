// chattr の印 (FS_IOC_GETFLAGS / SETFLAGS、FS_IOC_FSGETXATTR / FSSETXATTR、file_getattr / file_setattr) が Linux と
// 同じか (root で、引数のディレクトリごとに: ext4 と tmpfs)。はじめの印、知らない印、+i (変えられない: 書く口、
// 切りつめ、消す、名前を変える、リンク、属性、xattr)、+a (足すだけ: O_APPEND だけ、O_TRUNC、F_SETFL、共有の写し)、
// ディレクトリの +i と +a、受けつぐ印、+A (noatime)、持ち主でない人と CAP_LINUX_IMMUTABLE のない人、file_getattr の
// 大きさと印。ひとつずつ順に呼んで出す
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#define GETFLAGS 0x80086601UL
#define SETFLAGS 0x40086602UL
#define FSGETXATTR 0x801c581fUL
#define FSSETXATTR 0x401c5820UL
#define IMM 0x10
#define APP 0x20
#define NODUMP 0x40
#define NOATIME 0x80

struct fsx {
  uint32_t xflags, extsize, nextents, projid, cowextsize;
  unsigned char pad[8];
};
struct fattr {
  uint64_t xflags;
  uint32_t extsize, nextents, projid, cowextsize;
};

static char dir[256];
// 印がついたのに守られなかった (CI の ctests は終わりの番号だけ見る)
static int bad;
static char *P(const char *n) {
  static char b[4][320];
  static int k;
  char *r = b[k++ % 4];
  snprintf(r, 320, "%s/%s", dir, n);
  return r;
}
static long E(long r) { return r < 0 ? -errno : r; }
// ", 名前 値" (値はエラーなら -errno)
static void say(const char *k, long v) { printf(", %s %ld", k, v); }
// ", 名前 印" (16 進。エラーなら 10 進の -errno)
static void sayf(const char *k, long v) {
  if (v < 0)
    printf(", %s %ld", k, v);
  else
    printf(", %s %#lx", k, v);
}
static long getfl(const char *p) {
  int fd = open(p, O_RDONLY | O_NONBLOCK);
  if (fd < 0)
    return -errno;
  unsigned v = 0;
  long r = ioctl(fd, GETFLAGS, &v) ? -errno : (long)v;
  close(fd);
  return r;
}
static long setfl(const char *p, unsigned v) {
  int fd = open(p, O_RDONLY | O_NONBLOCK);
  if (fd < 0)
    return -errno;
  long r = ioctl(fd, SETFLAGS, &v) ? -errno : 0;
  close(fd);
  return r;
}
static long getfl_at(int dfd, const char *n) {
  int fd = openat(dfd, n, O_RDONLY | O_NONBLOCK);
  if (fd < 0)
    return -errno;
  unsigned v = 0;
  long r = ioctl(fd, GETFLAGS, &v) ? -errno : (long)v;
  close(fd);
  return r;
}
static long setfl_at(int dfd, const char *n, unsigned v) {
  int fd = openat(dfd, n, O_RDONLY | O_NONBLOCK);
  if (fd < 0)
    return -errno;
  long r = ioctl(fd, SETFLAGS, &v) ? -errno : 0;
  close(fd);
  return r;
}
static long op(const char *p, int fl) {
  int fd = open(p, fl, 0644);
  if (fd < 0)
    return -errno;
  close(fd);
  return 0;
}
static long fga(int dfd, const char *p, void *fa, size_t n, unsigned fl) { return syscall(468, dfd, p, fa, n, fl) ? -errno : 0; }
static long fsa(int dfd, const char *p, void *fa, size_t n, unsigned fl) { return syscall(469, dfd, p, fa, n, fl) ? -errno : 0; }
static void put(const char *p, const char *s) {
  int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  write(fd, s, strlen(s));
  close(fd);
}
static long size(const char *p) {
  struct stat s;
  return stat(p, &s) ? -errno : (long)s.st_size;
}
static long map(int fd, int prot, int fl) {
  void *m = mmap(NULL, 4096, prot, fl, fd, 0);
  if (m == MAP_FAILED)
    return -errno;
  munmap(m, 4096);
  return 0;
}

static void flags_and_xattr(void) {
  put(P("f"), "hello");
  mkdir(P("d"), 0755);
  symlink("f", P("l"));
  printf("flags");
  sayf("file", getfl(P("f")));
  sayf("dir", getfl(P("d")));
  sayf("symlink", getfl(P("l")));
  printf("\n");
  int fd = open(P("f"), O_RDONLY);
  struct fsx x;
  memset(&x, 0xee, sizeof x);
  long r = ioctl(fd, FSGETXATTR, &x) ? -errno : 0;
  printf("fsgetxattr %ld", r);
  if (!r)
    printf(": xflags %#x extsize %u nextents %u projid %u cowextsize %u", x.xflags, x.extsize, x.nextents, x.projid, x.cowextsize);
  printf("\n");
  struct fattr fa;
  memset(&fa, 0xee, sizeof fa);
  r = fga(AT_FDCWD, P("f"), &fa, sizeof fa, 0);
  printf("file_getattr %ld", r);
  if (!r)
    printf(": xflags %#llx extsize %u nextents %u projid %u cowextsize %u", (unsigned long long)fa.xflags, fa.extsize, fa.nextents, fa.projid, fa.cowextsize);
  printf("\n ");
  unsigned char big[64];
  say("size 16", fga(AT_FDCWD, P("f"), &fa, 16, 0));
  memset(big, 0xee, sizeof big);
  say("32", fga(AT_FDCWD, P("f"), big, 32, 0));
  say("tail", big[24]);
  say("8192", fga(AT_FDCWD, P("f"), &fa, 8192, 0));
  say("flags 1", fga(AT_FDCWD, P("f"), &fa, sizeof fa, 1));
  say("empty path by fd", fga(fd, "", &fa, sizeof fa, AT_EMPTY_PATH));
  say("empty path no flag", fga(fd, "", &fa, sizeof fa, 0));
  say("bad fd", fga(999, "", &fa, sizeof fa, AT_EMPTY_PATH));
  say("null path no flag", fga(fd, NULL, &fa, sizeof fa, 0));
  int op_fd = open(P("f"), O_PATH);
  say("O_PATH fd", fga(op_fd, "", &fa, sizeof fa, AT_EMPTY_PATH));
  close(op_fd);
  printf("\n ");
  say("symlink nofollow", fga(AT_FDCWD, P("l"), &fa, sizeof fa, AT_SYMLINK_NOFOLLOW));
  say("follow", fga(AT_FDCWD, P("l"), &fa, sizeof fa, 0));
  say("dir", fga(AT_FDCWD, P("d"), &fa, sizeof fa, 0));
  say("missing", fga(AT_FDCWD, P("nope"), &fa, sizeof fa, 0));
  say("/proc/self/status", fga(AT_FDCWD, "/proc/self/status", &fa, sizeof fa, 0));
  say("bad buffer", fga(AT_FDCWD, P("f"), (void *)8, sizeof fa, 0));
  printf("\n");
  close(fd);
}

static void setting(void) {
  long base = getfl(P("f"));
  printf("set");
  say("unknown 0x40000000", setfl(P("f"), (unsigned)base | 0x40000000));
  say("0x1", setfl(P("f"), (unsigned)base | 1));
  sayf("->", getfl(P("f")));
  say("0x100", setfl(P("f"), (unsigned)base | 0x100));
  sayf("->", getfl(P("f")));
  say("dirsync on a file", setfl(P("f"), (unsigned)base | 0x10000));
  say("nodump", setfl(P("f"), (unsigned)base | NODUMP));
  sayf("->", getfl(P("f")));
  say("off", setfl(P("f"), (unsigned)base));
  sayf("->", getfl(P("f")));
  printf("\nfile_setattr");
  struct fattr fa = {0};
  fga(AT_FDCWD, P("f"), &fa, sizeof fa, 0);
  uint64_t x0 = fa.xflags;
  fa.xflags = x0 | 0x80;
  say("nodump", fsa(AT_FDCWD, P("f"), &fa, sizeof fa, 0));
  sayf("->", getfl(P("f")));
  fa.xflags = x0 | (1ULL << 40);
  say("bit 40", fsa(AT_FDCWD, P("f"), &fa, sizeof fa, 0));
  fa.xflags = x0 | 0x2;
  say("prealloc (read only)", fsa(AT_FDCWD, P("f"), &fa, sizeof fa, 0));
  sayf("->", getfl(P("f")));
  fa.xflags = x0;
  fa.projid = 7;
  say("projid 7", fsa(AT_FDCWD, P("f"), &fa, sizeof fa, 0));
  fa.projid = 0;
  fa.extsize = 4096;
  say("extsize", fsa(AT_FDCWD, P("f"), &fa, sizeof fa, 0));
  fa.extsize = 0;
  say("back", fsa(AT_FDCWD, P("f"), &fa, sizeof fa, 0));
  sayf("->", getfl(P("f")));
  unsigned char big[40] = {0};
  memcpy(big, &fa, sizeof fa);
  big[30] = 1;
  say("40 with junk", fsa(AT_FDCWD, P("f"), big, 40, 0));
  big[30] = 0;
  say("40 zero", fsa(AT_FDCWD, P("f"), big, 40, 0));
  say("16", fsa(AT_FDCWD, P("f"), &fa, 16, 0));
  say("flags 1", fsa(AT_FDCWD, P("f"), &fa, sizeof fa, 1));
  say("bad buffer", fsa(AT_FDCWD, P("f"), (void *)8, sizeof fa, 0));
  printf("\nfssetxattr");
  int fd = open(P("f"), O_RDONLY);
  struct fsx x;
  ioctl(fd, FSGETXATTR, &x);
  x.xflags |= 0x80;
  say("nodump", ioctl(fd, FSSETXATTR, &x) ? -errno : 0);
  sayf("->", getfl(P("f")));
  x.xflags = 0x40000000;
  say("unknown", ioctl(fd, FSSETXATTR, &x) ? -errno : 0);
  x.xflags = 0;
  say("off", ioctl(fd, FSSETXATTR, &x) ? -errno : 0);
  sayf("->", getfl(P("f")));
  printf("\n");
  close(fd);
}

static void immutable(void) {
  put(P("i"), "data");
  int before = open(P("i"), O_RDWR);
  long base = getfl(P("i"));
  printf("+i");
  long set = setfl(P("i"), (unsigned)base | IMM);
  say("set", set);
  sayf("->", getfl(P("i")));
  say("nodump too", setfl(P("i"), (unsigned)base | IMM | NODUMP));
  long w = op(P("i"), O_WRONLY);
  bad |= set == 0 && w == 0;
  say("open w", w);
  say("r", op(P("i"), O_RDONLY));
  say("write by an old fd", E(write(before, "x", 1)));
  say("truncate", E(truncate(P("i"), 0)));
  say("ftruncate", E(ftruncate(before, 0)));
  say("unlink", E(unlink(P("i"))));
  say("rename", E(rename(P("i"), P("i2"))));
  say("link", E(link(P("i"), P("i3"))));
  printf("\n ");
  struct timespec ts[2] = {{1000, 0}, {1000, 0}};
  say("chmod", E(chmod(P("i"), 0600)));
  say("chown", E(chown(P("i"), 1, 1)));
  say("utimes now", E(utimensat(AT_FDCWD, P("i"), NULL, 0)));
  say("utimes set", E(utimensat(AT_FDCWD, P("i"), ts, 0)));
  say("setxattr", E(setxattr(P("i"), "user.x", "1", 1, 0)));
  say("fallocate", E(fallocate(before, 0, 0, 8192)));
  say("mmap shared w", map(before, PROT_READ | PROT_WRITE, MAP_SHARED));
  say("size", size(P("i")));
  printf("\n ");
  say("off", setfl(P("i"), (unsigned)base));
  sayf("->", getfl(P("i")));
  say("unlink", E(unlink(P("i"))));
  printf("\n");
  close(before);
}

static void append(void) {
  put(P("a"), "ab");
  long base = getfl(P("a"));
  printf("+a");
  long set = setfl(P("a"), (unsigned)base | APP);
  say("set", set);
  sayf("->", getfl(P("a")));
  long w = op(P("a"), O_WRONLY);
  bad |= set == 0 && w == 0;
  say("open w", w);
  int fd = open(P("a"), O_WRONLY | O_APPEND);
  say("w|append", fd >= 0);
  say("w|append|trunc", op(P("a"), O_WRONLY | O_APPEND | O_TRUNC));
  say("r", op(P("a"), O_RDONLY));
  say("write", E(write(fd, "cd", 2)));
  // +a なら pwrite も終わりに (印がなければ、aios は O_APPEND でも位置に書く: aios がえらんだちがい pwrite-append)
  if (set == 0)
    say("pwrite at 0", E(pwrite(fd, "ef", 2, 0)));
  say("size", size(P("a")));
  say("F_SETFL 0", E(fcntl(fd, F_SETFL, 0)));
  say("F_SETFL append", E(fcntl(fd, F_SETFL, O_APPEND)));
  printf("\n ");
  int rw = open(P("a"), O_RDWR | O_APPEND);
  struct timespec ts[2] = {{1000, 0}, {1000, 0}};
  say("truncate", E(truncate(P("a"), 0)));
  say("ftruncate", E(ftruncate(fd, 0)));
  say("unlink", E(unlink(P("a"))));
  say("rename", E(rename(P("a"), P("a2"))));
  say("link", E(link(P("a"), P("a3"))));
  say("chmod", E(chmod(P("a"), 0600)));
  say("utimes now", E(utimensat(AT_FDCWD, P("a"), NULL, 0)));
  say("utimes set", E(utimensat(AT_FDCWD, P("a"), ts, 0)));
  say("setxattr", E(setxattr(P("a"), "user.x", "1", 1, 0)));
  say("mmap shared r", map(rw, PROT_READ, MAP_SHARED));
  say("mmap private r", map(rw, PROT_READ, MAP_PRIVATE));
  say("fallocate", E(fallocate(rw, 0, 0, 8192)));
  say("punch", E(fallocate(rw, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, 1)));
  printf("\n ");
  char buf[16] = {0};
  pread(rw, buf, 15, 0);
  bad |= set == 0 && strcmp(buf, "abcdef") != 0;
  printf(", content %s", buf);
  say("off", setfl(P("a"), (unsigned)base));
  say("unlink", E(unlink(P("a"))));
  printf("\n");
  close(fd);
  close(rw);
}

static void dirs(void) {
  mkdir(P("di"), 0755);
  put(P("di/x"), "x");
  long base = getfl(P("di"));
  printf("dir +i");
  say("set", setfl(P("di"), (unsigned)base | IMM));
  sayf("->", getfl(P("di")));
  say("create", op(P("di/y"), O_CREAT | O_WRONLY));
  say("mkdir", E(mkdir(P("di/z"), 0755)));
  say("unlink", E(unlink(P("di/x"))));
  say("rename in", E(rename(P("di/x"), P("di/x2"))));
  say("write the file in it", op(P("di/x"), O_WRONLY));
  say("rmdir", E(rmdir(P("di"))));
  say("off", setfl(P("di"), (unsigned)base));
  printf("\ndir +a");
  say("set", setfl(P("di"), (unsigned)base | APP));
  sayf("->", getfl(P("di")));
  say("create", op(P("di/y"), O_CREAT | O_WRONLY));
  say("unlink new", E(unlink(P("di/y"))));
  say("unlink old", E(unlink(P("di/x"))));
  say("rename", E(rename(P("di/x"), P("di/x2"))));
  say("rename into", E(rename(P("f"), P("di/f"))));
  say("off", setfl(P("di"), (unsigned)base));
  printf("\n inherit");
  // 新しいものは +d と +A を受けつぐか
  setfl(P("di"), (unsigned)base | NODUMP | NOATIME | 0x10000);
  put(P("di/n"), "n");
  mkdir(P("di/nd"), 0755);
  symlink("n", P("di/nl"));
  sayf("file", getfl(P("di/n")));
  sayf("dir", getfl(P("di/nd")));
  struct fattr fa = {0};
  say("symlink file_getattr", fga(AT_FDCWD, P("di/nl"), &fa, sizeof fa, AT_SYMLINK_NOFOLLOW));
  printf("\n");
  setfl(P("di"), (unsigned)base);
}

// +A (noatime) は印が入るか (aios は atime を持たないので、読んで変わらないかは見ない)
static void noatime(void) {
  put(P("t"), "t");
  long base = getfl(P("t"));
  printf("+A");
  say("set", setfl(P("t"), (unsigned)base | NOATIME));
  sayf("->", getfl(P("t")));
  say("off", setfl(P("t"), (unsigned)base));
  printf("\n");
  unlink(P("t"));
}

static int others(void) {
  put(P("o"), "o");
  chmod(dir, 0777);
  // 上のディレクトリが通れなくても (aios の家は 0700)、開いた口から入る
  int dfd = open(dir, O_RDONLY | O_DIRECTORY);
  pid_t c = fork();
  if (c == 0) {
    if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
      _exit(1);
    close(openat(dfd, "mine", O_CREAT | O_WRONLY, 0644));
    long base = getfl_at(dfd, "mine");
    struct fattr fa = {0};
    printf("nobody");
    sayf("get root's", getfl_at(dfd, "o"));
    say("set root's", setfl_at(dfd, "o", (unsigned)getfl_at(dfd, "o") | NODUMP));
    say("set mine nodump", setfl_at(dfd, "mine", (unsigned)base | NODUMP));
    say("mine +i", setfl_at(dfd, "mine", (unsigned)base | IMM));
    say("mine +a", setfl_at(dfd, "mine", (unsigned)base | APP));
    say("file_setattr root's", fsa(dfd, "o", &fa, sizeof fa, 0));
    say("file_getattr root's", fga(dfd, "o", &fa, sizeof fa, 0));
    printf("\n");
    unlinkat(dfd, "mine", 0);
    _exit(0);
  }
  int s;
  waitpid(c, &s, 0);
  close(dfd);
  unlink(P("o"));
  return WIFEXITED(s) ? WEXITSTATUS(s) : 100;
}

// 前の残りも (印を外してから)。aios の VM は動かすたびに起こしなおさない
static void clean(void) {
  const char *names[] = {"di/f", "di/n", "di/nl", "di/x", "di/x2", "di/y", "f", "l", "i", "i2", "i3", "a", "a2", "a3", "t", "o", "mine"};
  const char *dirs[] = {"di/nd", "di/z", "di", "d"};
  for (unsigned k = 0; k < sizeof names / sizeof *names; k++) {
    setfl(P(names[k]), 0x80000 & (unsigned)getfl(P(names[k])));
    unlink(P(names[k]));
  }
  for (unsigned k = 0; k < sizeof dirs / sizeof *dirs; k++) {
    setfl(P(dirs[k]), 0x80000 & (unsigned)getfl(P(dirs[k])));
    rmdir(P(dirs[k]));
  }
}

static int one(const char *base) {
  snprintf(dir, sizeof dir, "%s/chattr-t", base);
  mkdir(dir, 0755);
  clean();
  printf("%s\n", base[1] == 't' ? "tmpfs" : "ext4");
  flags_and_xattr();
  setting();
  immutable();
  append();
  dirs();
  noatime();
  int r = others();
  clean();
  rmdir(dir);
  return r | bad;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("chattr: not root, skipped\n");
    return 0;
  }
  int r = 0;
  for (int i = 1; i < argc; i++)
    r |= one(argv[i]);
  return argc > 1 ? r : one("/tmp");
}
