// パスの覚え (カーネルの dentry キャッシュ、vfs.rs の PATHS) が古い答えを出さないか: aios の中で動かす
// (zig cc -target aarch64-linux-musl -O2 -static -o pathcache-test test/pathcache.c)。
// 覚えさせて (同じパスを何度か stat)、名前を変える・消して作りなおす・リンクを付けかえる・権限を変える・
// 開いたまま消す、マウントする (root のとき)、のあとで、新しい答えになるか
// root でも root でなくても動かす (権限は root でないとき、マウントは root のとき)
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s\n", name); fails++; } } while (0)

static int st(const char *p, struct stat *s) { struct stat x; for (int i = 0; i < 3; i++) if (stat(p, &x) != 0) return -errno; if (s) *s = x; return 0; }
static void touch(const char *p) { int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644); write(fd, "x", 1); close(fd); }

int main(void) {
  char base[64];
  snprintf(base, sizeof base, "/tmp/pc-%d", getpid());
  char a[128], f[128], c[128], cf[128], l[128], g[128];
  snprintf(a, sizeof a, "%s/a", base); snprintf(f, sizeof f, "%s/a/f", base);
  snprintf(c, sizeof c, "%s/c", base); snprintf(cf, sizeof cf, "%s/c/f", base);
  snprintf(l, sizeof l, "%s/l", base); snprintf(g, sizeof g, "%s/g", base);
  mkdir(base, 0755); mkdir(a, 0755); touch(f);
  struct stat s1, s2;
  CHECK("cached", st(f, &s1) == 0);
  // 名前を変える
  rename(a, c);
  CHECK("rename-old-gone", st(f, 0) == -ENOENT);
  CHECK("rename-new-there", st(cf, &s2) == 0 && s2.st_ino == s1.st_ino);
  // 消して作りなおす (別の inode)
  unlink(cf);
  CHECK("unlink-gone", st(cf, 0) == -ENOENT);
  touch(g);
  rename(g, cf);
  CHECK("recreated", st(cf, &s2) == 0);
  // リンクを付けかえる
  touch(f); // a はもうないので作れない: c の中に
  char f1[128], f2[128];
  snprintf(f1, sizeof f1, "%s/f1", base); snprintf(f2, sizeof f2, "%s/f2", base);
  touch(f1); touch(f2);
  struct stat t1, t2, sl;
  st(f1, &t1); st(f2, &t2);
  symlink("f1", l);
  CHECK("link-first", st(l, &sl) == 0 && sl.st_ino == t1.st_ino);
  unlink(l); symlink("f2", l);
  CHECK("link-retargeted", st(l, &sl) == 0 && sl.st_ino == t2.st_ino);
  // 権限を変える: 持ち主でも x がなければ通れない (root なら通れるので、root のときはとばす)
  if (getuid() != 0) {
    CHECK("perm-before", st(cf, 0) == 0);
    chmod(c, 0600);
    CHECK("perm-denied", st(cf, 0) == -EACCES);
    chmod(c, 0755);
    CHECK("perm-again", st(cf, 0) == 0);
  }
  // 2 段上が変わる: base/d1/d2/f を覚えてから d1 の名前を変える、d1 の x を外す
  {
    char d1[128], d2[128], df[128], e1[128], ef[128];
    snprintf(d1, sizeof d1, "%s/d1", base); snprintf(d2, sizeof d2, "%s/d1/d2", base); snprintf(df, sizeof df, "%s/d1/d2/f", base);
    snprintf(e1, sizeof e1, "%s/e1", base); snprintf(ef, sizeof ef, "%s/e1/d2/f", base);
    mkdir(d1, 0755); mkdir(d2, 0755); touch(df);
    CHECK("deep-cached", st(df, 0) == 0);
    rename(d1, e1);
    CHECK("deep-rename-gone", st(df, 0) == -ENOENT);
    CHECK("deep-rename-there", st(ef, 0) == 0);
    if (getuid() != 0) {
      chmod(e1, 0644);
      CHECK("deep-perm-denied", st(ef, 0) == -EACCES);
      chmod(e1, 0755);
    }
    unlink(ef); snprintf(d2, sizeof d2, "%s/e1/d2", base); rmdir(d2); rmdir(e1);
  }
  // 開いたまま消したものは、消したあと名前では引けない
  int fd = open(f1, O_RDONLY);
  st(f1, 0);
  unlink(f1);
  CHECK("open-unlinked-gone", st(f1, 0) == -ENOENT);
  struct stat fs_;
  CHECK("open-unlinked-fd", fstat(fd, &fs_) == 0 && fs_.st_nlink == 0);
  close(fd);
  // マウント (root のとき): かぶせると下のものは見えず、外すとまた見える
  if (getuid() == 0) {
    char m[128], mx[128], my[128];
    snprintf(m, sizeof m, "%s/m", base); snprintf(mx, sizeof mx, "%s/m/x", base); snprintf(my, sizeof my, "%s/m/y", base);
    mkdir(m, 0755); touch(mx);
    CHECK("mount-before", st(mx, 0) == 0);
    CHECK("mount", mount("none", m, "tmpfs", 0, 0) == 0);
    CHECK("mount-hides", st(mx, 0) == -ENOENT);
    touch(my);
    CHECK("mount-new", st(my, 0) == 0);
    CHECK("umount", umount(m) == 0);
    CHECK("umount-shows", st(mx, 0) == 0);
    CHECK("umount-hides-new", st(my, 0) == -ENOENT);
    unlink(mx); rmdir(m);
  }
  // 片づけ
  unlink(cf); unlink(f2); unlink(l); rmdir(c); rmdir(base);
  CHECK("cleanup", st(base, 0) == -ENOENT);
  printf("pathcache: %d failed\n", fails);
  return fails;
}
