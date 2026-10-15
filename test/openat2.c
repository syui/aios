// openat2(2) が Linux と同じか (カーネル: sysfile.rs の openat2、vfs.rs の lookup_resolve)。cap-std (uv など) は
// RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS で、ディレクトリの外に出ない open に使う (ENOSYS なら自分でたどる)。
// struct open_how の大きさ、知らない flags、O_CREAT なしの mode、RESOLVE_* (BENEATH IN_ROOT NO_SYMLINKS NO_MAGICLINKS
// NO_XDEV CACHED)、O_CREAT。Linux (このマシン) でも aios でも同じに通る
// aarch64-linux-gnu-gcc -O2 -static -o openat2 test/openat2.c (このマシンでは cc -O2 -o openat2 test/openat2.c)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#define SYS_OPENAT2 437
#define RESOLVE_NO_XDEV 0x01
#define RESOLVE_NO_MAGICLINKS 0x02
#define RESOLVE_NO_SYMLINKS 0x04
#define RESOLVE_BENEATH 0x08
#define RESOLVE_IN_ROOT 0x10
#define RESOLVE_CACHED 0x20

struct how {
  uint64_t flags, mode, resolve;
  uint64_t more; // 版 0 (24 バイト) より大きいとき
};

static int fail, n;

static long oa2(int dirfd, const char *path, uint64_t flags, uint64_t mode, uint64_t resolve, size_t size, uint64_t more) {
  struct how h = {flags, mode, resolve, more};
  return syscall(SYS_OPENAT2, dirfd, path, &h, size);
}

// want: 0 なら開けること (fd を閉じる)、ほかはその errno。want2 も 0 でなければ、どちらでもよい
static void check2(const char *what, long r, int want, int want2) {
  int got = r < 0 ? errno : 0;
  n++;
  if (r >= 0) close((int)r);
  if (got == want || (want2 && got == want2)) return;
  printf("FAIL %s: %s (want %s)\n", what, got ? strerror(got) : "opened", want ? strerror(want) : "opened");
  fail++;
}

static void check(const char *what, long r, int want) { check2(what, r, want, 0); }

static void must(int r, const char *what) {
  if (r < 0) {
    printf("FAIL setup %s: %s\n", what, strerror(errno));
    exit(1);
  }
}

static int opendir_(const char *p) {
  int fd = open(p, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd < 0) {
    printf("FAIL open %s: %s\n", p, strerror(errno));
    exit(1);
  }
  return fd;
}

int main(void) {
  char d[] = "/tmp/openat2.XXXXXX", p[PATH_MAX], up[PATH_MAX];
  if (!mkdtemp(d)) {
    perror("mkdtemp");
    return 1;
  }
  const char *base = strrchr(d, '/') + 1;
#define AT(x) (snprintf(p, sizeof p, "%s/%s", d, x), p)
  must(mkdir(AT("sub"), 0755), "mkdir sub");
  must(mkdir(AT("etc"), 0755), "mkdir etc");
  must(close(open(AT("file"), O_CREAT | O_WRONLY, 0644)), "file");
  must(close(open(AT("sub/file"), O_CREAT | O_WRONLY, 0644)), "sub/file");
  must(symlink("sub/file", AT("link_rel")), "link_rel");
  must(symlink("../file", AT("sub/up")), "sub/up");
  must(symlink("/sub/file", AT("abs")), "abs");
  must(symlink("..", AT("dotdot")), "dotdot");
  must(symlink("new_target", AT("dangle")), "dangle");
  int D = opendir_(d), S = opendir_(AT("sub")), F = open(AT("file"), O_RDONLY | O_CLOEXEC);
  snprintf(up, sizeof up, "../%s/file", base);

  // open_how の形
  check("plain", oa2(D, "file", O_RDONLY, 0, 0, 24, 0), 0);
  check("size 16", oa2(D, "file", O_RDONLY, 0, 0, 16, 0), EINVAL);
  check("size 32, zero tail", oa2(D, "file", O_RDONLY, 0, 0, 32, 0), 0);
  check("size 32, tail set", oa2(D, "file", O_RDONLY, 0, 0, 32, 1), E2BIG);
  check("unknown flag", oa2(D, "file", O_RDONLY | (1u << 30), 0, 0, 24, 0), EINVAL);
  check("mode without O_CREAT", oa2(D, "file", O_RDONLY, 0644, 0, 24, 0), EINVAL);
  check("mode 010000", oa2(D, "x", O_RDONLY | O_CREAT, 010000, 0, 24, 0), EINVAL);
  check("O_PATH|O_RDWR", oa2(D, "file", O_PATH | O_RDWR, 0, 0, 24, 0), EINVAL);
  check("unknown resolve", oa2(D, "file", O_RDONLY, 0, 0x40, 24, 0), EINVAL);
  check("BENEATH|IN_ROOT", oa2(D, "file", O_RDONLY, 0, RESOLVE_BENEATH | RESOLVE_IN_ROOT, 24, 0), EINVAL);
  check("CACHED O_CREAT", oa2(D, "c", O_RDWR | O_CREAT, 0644, RESOLVE_CACHED, 24, 0), EAGAIN);
  check2("CACHED", oa2(D, "file", O_RDONLY, 0, RESOLVE_CACHED, 24, 0), 0, EAGAIN);

  // BENEATH: dirfd の下だけ
  uint64_t B = RESOLVE_BENEATH;
  check("beneath file", oa2(D, "file", O_RDONLY, 0, B, 24, 0), 0);
  check("beneath sub/../file", oa2(D, "sub/../file", O_RDONLY, 0, B, 24, 0), 0);
  check("beneath ../D/file", oa2(D, up, O_RDONLY, 0, B, 24, 0), EXDEV);
  check("beneath absolute", oa2(D, "/etc/passwd", O_RDONLY, 0, B, 24, 0), EXDEV);
  check("beneath absolute link", oa2(D, "abs", O_RDONLY, 0, B, 24, 0), EXDEV);
  check("beneath link inside", oa2(D, "link_rel", O_RDONLY, 0, B, 24, 0), 0);
  check("beneath link ../file from D", oa2(D, "sub/up", O_RDONLY, 0, B, 24, 0), 0);
  check("beneath link ../file from sub", oa2(S, "up", O_RDONLY, 0, B, 24, 0), EXDEV);
  check("beneath link ..", oa2(D, "dotdot/file", O_RDONLY, 0, B, 24, 0), EXDEV);
  check("beneath file/", oa2(D, "file/", O_RDONLY, 0, B, 24, 0), ENOTDIR);
  check("beneath sub/", oa2(D, "sub/", O_RDONLY | O_DIRECTORY, 0, B, 24, 0), 0);
  check("beneath nope", oa2(D, "nope", O_RDONLY, 0, B, 24, 0), ENOENT);
  check("beneath nope/x", oa2(D, "nope/x", O_RDONLY, 0, B, 24, 0), ENOENT);
  check("beneath dirfd a file", oa2(F, "x", O_RDONLY, 0, B, 24, 0), ENOTDIR);
  check("beneath create", oa2(D, "new", O_RDWR | O_CREAT, 0644, B, 24, 0), 0);
  check("beneath create sub/new", oa2(D, "sub/new", O_RDWR | O_CREAT, 0644, B, 24, 0), 0);
  check("beneath create outside", oa2(D, up, O_RDWR | O_CREAT, 0644, B, 24, 0), EXDEV);
  check("beneath create excl exists", oa2(D, "new", O_RDWR | O_CREAT | O_EXCL, 0644, B, 24, 0), EEXIST);
  check("beneath create excl on link", oa2(D, "dangle", O_RDWR | O_CREAT | O_EXCL, 0644, B, 24, 0), EEXIST);
  check("beneath create through link", oa2(D, "dangle", O_RDWR | O_CREAT, 0644, B, 24, 0), 0);
  check("beneath link target made", oa2(D, "new_target", O_RDONLY, 0, B, 24, 0), 0);
  check("beneath create dir/", oa2(D, "newdir/", O_RDWR | O_CREAT, 0644, B, 24, 0), EISDIR);

  // IN_ROOT: dirfd がルート
  uint64_t I = RESOLVE_IN_ROOT;
  check("in_root /sub/file", oa2(D, "/sub/file", O_RDONLY, 0, I, 24, 0), 0);
  check("in_root absolute link", oa2(D, "abs", O_RDONLY, 0, I, 24, 0), 0);
  check("in_root ../../sub/file", oa2(D, "../../sub/file", O_RDONLY, 0, I, 24, 0), 0);
  check("in_root link ..", oa2(D, "dotdot/file", O_RDONLY, 0, I, 24, 0), 0);
  check("in_root /etc/passwd", oa2(D, "/etc/passwd", O_RDONLY, 0, I, 24, 0), ENOENT);

  // NO_SYMLINKS (.. で上に出るのはかまわない)
  check("no_symlinks file", oa2(D, "file", O_RDONLY, 0, RESOLVE_NO_SYMLINKS, 24, 0), 0);
  check("no_symlinks link", oa2(D, "link_rel", O_RDONLY, 0, RESOLVE_NO_SYMLINKS, 24, 0), ELOOP);
  check("no_symlinks sub/up", oa2(D, "sub/up", O_RDONLY, 0, RESOLVE_NO_SYMLINKS, 24, 0), ELOOP);
  check("no_symlinks ../D/file", oa2(D, up, O_RDONLY, 0, RESOLVE_NO_SYMLINKS, 24, 0), 0);

  // 魔法のリンク: /proc/self/fd/N
  int P = opendir_("/proc/self");
  char fdp[32];
  snprintf(fdp, sizeof fdp, "fd/%d", D);
  check("magic plain", oa2(P, fdp, O_RDONLY | O_DIRECTORY, 0, 0, 24, 0), 0);
  check("magic no_magiclinks", oa2(P, fdp, O_RDONLY, 0, RESOLVE_NO_MAGICLINKS, 24, 0), ELOOP);
  check("magic beneath", oa2(P, fdp, O_RDONLY, 0, B, 24, 0), EXDEV);
  check("magic beneath|no_magiclinks", oa2(P, fdp, O_RDONLY, 0, B | RESOLVE_NO_MAGICLINKS, 24, 0), ELOOP);
  check("no_magiclinks plain link", oa2(D, "link_rel", O_RDONLY, 0, RESOLVE_NO_MAGICLINKS, 24, 0), 0);

  // NO_XDEV: /proc はマウント
  int R = opendir_("/"), PR = opendir_("/proc");
  check("no_xdev / proc", oa2(R, "proc", O_RDONLY | O_DIRECTORY, 0, RESOLVE_NO_XDEV, 24, 0), EXDEV);
  check("no_xdev /proc ..", oa2(PR, "..", O_RDONLY | O_DIRECTORY, 0, RESOLVE_NO_XDEV, 24, 0), EXDEV);
  check("xdev /proc ..", oa2(PR, "..", O_RDONLY | O_DIRECTORY, 0, 0, 24, 0), 0);
  check("no_xdev inside", oa2(D, "sub/file", O_RDONLY, 0, RESOLVE_NO_XDEV, 24, 0), 0);

  // 片づけ
  const char *rm[] = {"new_target", "dangle", "sub/new", "new", "dotdot", "abs", "sub/up", "link_rel", "sub/file", "file"};
  for (unsigned i = 0; i < sizeof rm / sizeof rm[0]; i++) unlink(AT(rm[i]));
  rmdir(AT("sub"));
  rmdir(AT("etc"));
  rmdir(d);
  printf("openat2: %d checks, %d failed\n", n, fail);
  return fail != 0;
}
