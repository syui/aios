// 新しい mount の API (fsopen、fsconfig、fsmount、move_mount、open_tree、fspick、mount_setattr) が Linux と同じか。
// root でなければ EPERM だけ。root なら新しいマウントの namespace で: tmpfs を fsopen して fsconfig (知らないものは
// EINVAL、読むと理由)、作って fsmount (はなれたマウント: そこに作れる)、move_mount で置く。open_tree の CLONE は bind、
// CLONE なしは移す。mount_setattr で読むだけに (EROFS)、fspick と RECONFIGURE でファイルシステムを読むだけに
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/sysmacros.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define S_OPEN_TREE 428
#define S_MOVE_MOUNT 429
#define S_FSOPEN 430
#define S_FSCONFIG 431
#define S_FSMOUNT 432
#define S_FSPICK 433
#define S_MOUNT_SETATTR 442

struct attr {
  uint64_t set, clr, prop, userns;
};

static long e(long r) { return r < 0 ? -errno : r; }
static long fsopen_(const char *n, unsigned f) { return e(syscall(S_FSOPEN, n, f)); }
static long fsconfig_(int fd, unsigned cmd, const char *k, const void *v, int aux) { return e(syscall(S_FSCONFIG, fd, cmd, k, v, aux)); }
static long fsmount_(int fd, unsigned f, unsigned a) { return e(syscall(S_FSMOUNT, fd, f, a)); }
static long move_(int fd, const char *fp, int td, const char *tp, unsigned f) { return e(syscall(S_MOVE_MOUNT, fd, fp, td, tp, f)); }
static long open_tree_(int d, const char *p, unsigned f) { return e(syscall(S_OPEN_TREE, d, p, f)); }
static long fspick_(int d, const char *p, unsigned f) { return e(syscall(S_FSPICK, d, p, f)); }
static long setattr_(int d, const char *p, unsigned f, struct attr *a, size_t n) { return e(syscall(S_MOUNT_SETATTR, d, p, f, a, n)); }
static int has(const char *p) { return access(p, F_OK) == 0; }
static long mk(const char *p) {
  int fd = open(p, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0)
    return -errno;
  close(fd);
  return 0;
}
// 開けたら 0 (fd の番号は見ない)
static long ok(long r) { return r < 0 ? r : 0; }
static void msg(int fd) {
  char b[256];
  ssize_t n = read(fd, b, sizeof b - 1);
  b[n < 0 ? 0 : n] = 0;
  if (n > 0 && b[n - 1] == '\n')
    b[n - 1] = 0;
  printf("  log: %s (%ld)\n", n < 0 ? strerror(errno) : b, n < 0 ? -(long)errno : (long)n);
}
static void mounted(const char *at) {
  FILE *f = fopen("/proc/self/mountinfo", "r");
  char l[512], p[256], fs[64], opt[128];
  int n = 0;
  while (f && fgets(l, sizeof l, f)) {
    char *d = strstr(l, " - ");
    if (sscanf(l, "%*s %*s %*s %*s %255s %127s", p, opt) == 2 && !strcmp(p, at) && d && sscanf(d + 3, "%63s", fs) == 1) {
      printf("  %s: %s %s\n", at, fs, opt);
      n++;
    }
  }
  if (!n)
    printf("  %s: not a mount\n", at);
  if (f)
    fclose(f);
}

static long copy(const char *from, const char *to) {
  int a = open(from, O_RDONLY), b = open(to, O_CREAT | O_TRUNC | O_WRONLY, 0755);
  if (a < 0 || b < 0)
    return -errno;
  char buf[65536];
  ssize_t n;
  while ((n = read(a, buf, sizeof buf)) > 0)
    if (write(b, buf, n) != n)
      return -EIO;
  close(a), close(b);
  return 0;
}
// exec して、その答え (動けば 0、だめなら -errno)
static long run(const char *p) {
  pid_t c = fork();
  if (c == 0) {
    execl(p, p, "--child", (char *)NULL);
    _exit(errno);
  }
  int st;
  waitpid(c, &st, 0);
  return WIFEXITED(st) ? -WEXITSTATUS(st) : -100;
}
static long mm(const char *p, int prot) {
  int fd = open(p, O_RDONLY);
  void *a = mmap(NULL, 4096, prot, MAP_PRIVATE, fd, 0);
  long r = a == MAP_FAILED ? -errno : 0;
  if (a != MAP_FAILED)
    munmap(a, 4096);
  close(fd);
  return r;
}

static int body(void) {
  if (unshare(CLONE_NEWNS) || mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL))
    return 1;
  printf("fsopen checks (no such fs %ld, bad flag %ld)\n", fsopen_("nofs", 0), fsopen_("tmpfs", 2));
  int fs = fsopen_("tmpfs", 1);
  printf("fsopen tmpfs %ld\n", ok(fs));
  printf("fsconfig (size %ld, mode %ld, bogus %ld)\n", fsconfig_(fs, 1, "size", "1m", 0), fsconfig_(fs, 1, "mode", "0700", 0), fsconfig_(fs, 1, "bogus", "1", 0));
  msg(fs);
  printf("fsconfig checks (bad cmd %ld, flag with value %ld, string without value %ld, no key %ld, bad fd %ld)\n", fsconfig_(fs, 99, NULL, NULL, 0),
         fsconfig_(fs, 0, "ro", "x", 0), fsconfig_(fs, 1, "size", NULL, 0), fsconfig_(fs, 1, NULL, "1", 0), fsconfig_(0, 6, NULL, NULL, 0));
  printf("fsmount before create %ld\n", fsmount_(fs, 0, 0));
  printf("create %ld, again %ld\n", fsconfig_(fs, 6, NULL, NULL, 0), fsconfig_(fs, 6, NULL, NULL, 0));
  printf("set after create %ld\n", fsconfig_(fs, 1, "size", "2m", 0));
  printf("fsmount checks (bad flag %ld, bad attr %ld, not a context %ld)\n", fsmount_(fs, 0x80, 0), fsmount_(fs, 0, 0x40000000), fsmount_(0, 0, 0));
  int m = fsmount_(fs, 1, 4);
  struct stat st;
  printf("fsmount %ld (dir %d, mode %o)\n", ok(m), m >= 0 && !fstat(m, &st) && S_ISDIR(st.st_mode), m >= 0 ? st.st_mode & 07777 : 0);
  printf("fsmount again %ld\n", fsmount_(fs, 0, 0));
  int x = openat(m, "detached", O_CREAT | O_WRONLY, 0644);
  printf("make in a detached mount %ld\n", ok(e(x)));
  if (x >= 0)
    close(x);
  mkdir("/tmp/nm", 0755);
  printf("move_mount checks (bad flag %ld, no from path %ld, to a file %ld)\n", move_(m, "", AT_FDCWD, "/tmp/nm", 0x1000), move_(m, "", AT_FDCWD, "/tmp/nm", 0),
         move_(m, "", AT_FDCWD, "/proc/self/stat", 4));
  long r = move_(m, "", AT_FDCWD, "/tmp/nm", 4);
  printf("move_mount %ld (detached %d)\n", r, has("/tmp/nm/detached"));
  mounted("/tmp/nm");
  close(m);
  close(fs);

  mkdir("/tmp/nm2", 0755);
  int t = open_tree_(AT_FDCWD, "/tmp/nm", 1 | O_CLOEXEC);
  printf("open_tree clone %ld (stat dir %d)\n", ok(t), t >= 0 && !fstat(t, &st) && S_ISDIR(st.st_mode));
  printf("open_tree checks (bad flag %ld, no such %ld)\n", open_tree_(AT_FDCWD, "/tmp/nm", 4), open_tree_(AT_FDCWD, "/nope", 1));
  r = move_(t, "", AT_FDCWD, "/tmp/nm2", 4);
  printf("bind by move_mount %ld (detached %d)\n", r, has("/tmp/nm2/detached"));
  mounted("/tmp/nm2");
  close(t);

  mkdir("/tmp/nm3", 0755);
  int p = open_tree_(AT_FDCWD, "/tmp/nm2", O_CLOEXEC);
  r = move_(p, "", AT_FDCWD, "/tmp/nm3", 4);
  printf("move by move_mount %ld (old %d, new %d)\n", r, has("/tmp/nm2/detached"), has("/tmp/nm3/detached"));
  mounted("/tmp/nm2");
  mounted("/tmp/nm3");
  close(p);
  r = move_(AT_FDCWD, "/tmp/nm3", AT_FDCWD, "/tmp/nm2", 0);
  printf("move by paths %ld (nm3 %d, nm2 %d)\n", r, has("/tmp/nm3/detached"), has("/tmp/nm2/detached"));

  struct attr a = {1, 0, 0, 0};
  printf("mount_setattr checks (small %ld, bad attr %ld, bad flag %ld, not a mount %ld)\n", setattr_(AT_FDCWD, "/tmp/nm2", 0, &a, 8),
         setattr_(AT_FDCWD, "/tmp/nm2", 0, &(struct attr){0x40000000, 0, 0, 0}, sizeof a), setattr_(AT_FDCWD, "/tmp/nm2", 1, &a, sizeof a),
         setattr_(AT_FDCWD, "/tmp/nm2/detached", 0, &a, sizeof a));
  r = setattr_(AT_FDCWD, "/tmp/nm2", 0, &a, sizeof a);
  printf("mount_setattr ro %ld (make %ld, other mount %ld)\n", r, mk("/tmp/nm2/ro"), mk("/tmp/nm/rw"));
  mounted("/tmp/nm2");
  a = (struct attr){0, 1, 0, 0};
  r = setattr_(AT_FDCWD, "/tmp/nm2", 0, &a, sizeof a);
  printf("mount_setattr rw %ld (make %ld)\n", r, mk("/tmp/nm2/rw2"));

  int k = fspick_(AT_FDCWD, "/tmp/nm", 1);
  printf("fspick %ld, checks (bad flag %ld, not a mount root %ld)\n", ok(k), fspick_(AT_FDCWD, "/tmp/nm", 0x10), fspick_(AT_FDCWD, "/tmp/nm/detached", 0));
  printf("reconfigure ro (flag %ld, cmd %ld) make %ld, in the bind %ld\n", fsconfig_(k, 0, "ro", NULL, 0), fsconfig_(k, 7, NULL, NULL, 0), mk("/tmp/nm/ro2"), mk("/tmp/nm2/ro3"));
  mounted("/tmp/nm");
  printf("reconfigure rw (flag %ld, cmd %ld) make %ld\n", fsconfig_(k, 0, "rw", NULL, 0), fsconfig_(k, 7, NULL, NULL, 0), mk("/tmp/nm/rw3"));
  close(k);
  // 印をまもる: noexec (exec は EACCES、実行できるように写すと EPERM)、nodev (デバイスを開くと EACCES)。ふつうの /tmp では動く
  int fs2 = fsopen_("tmpfs", 1);
  fsconfig_(fs2, 6, NULL, NULL, 0);
  int m2 = fsmount_(fs2, 1, 8 | 4);
  mkdir("/tmp/nx", 0755);
  printf("noexec nodev mount %ld\n", move_(m2, "", AT_FDCWD, "/tmp/nx", 4));
  mounted("/tmp/nx");
  printf("copy (%ld, %ld)\n", copy("/proc/self/exe", "/tmp/nx/prog"), copy("/proc/self/exe", "/tmp/nmprog"));
  printf("exec (noexec %ld, normal %ld)\n", run("/tmp/nx/prog"), run("/tmp/nmprog"));
  printf("mmap (exec on noexec %ld, read on noexec %ld, exec on normal %ld)\n", mm("/tmp/nx/prog", PROT_READ | PROT_EXEC), mm("/tmp/nx/prog", PROT_READ), mm("/tmp/nmprog", PROT_READ | PROT_EXEC));
  long mn = e(mknod("/tmp/nx/null", S_IFCHR | 0666, makedev(1, 3)));
  long mn2 = e(mknod("/tmp/nmnull", S_IFCHR | 0666, makedev(1, 3)));
  int dn = open("/tmp/nx/null", O_RDONLY), dn2 = open("/tmp/nmnull", O_RDONLY);
  printf("nodev (mknod %ld, open %ld, normal mknod %ld, open %ld)\n", mn, dn < 0 ? -(long)errno : 0L, mn2, dn2 < 0 ? -(long)errno : 0L);
  if (dn >= 0)
    close(dn);
  if (dn2 >= 0)
    close(dn2);
  unlink("/tmp/nmprog"), unlink("/tmp/nmnull");
  // fsmount の口が開いているあいだは Linux の umount は EBUSY (aios はまだ見ない) なので、閉じてから
  close(m2), close(fs2);
  printf("umount nx %ld\n", e(umount2("/tmp/nx", 0)));
  printf("umount (nm2 %ld, nm %ld)\n", e(umount2("/tmp/nm2", 0)), e(umount2("/tmp/nm", 0)));
  return 0;
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "--child"))
    return 0;
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("not root (fsopen %ld, open_tree clone %ld, fspick %ld)\n", fsopen_("tmpfs", 0), open_tree_(AT_FDCWD, "/tmp", 1), fspick_(AT_FDCWD, "/", 0));
    return 0;
  }
  pid_t c = fork();
  if (c == 0)
    _exit(body());
  int st;
  waitpid(c, &st, 0);
  printf("child %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st));
  rmdir("/tmp/nm"), rmdir("/tmp/nm2"), rmdir("/tmp/nm3"), rmdir("/tmp/nx");
  return 0;
}
