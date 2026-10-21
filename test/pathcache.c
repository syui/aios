// パスの覚え (カーネルの dentry キャッシュ、vfs.rs の PATHS) が古い答えを出さないか: aios の中で動かす
// (zig cc -target aarch64-linux-musl -O2 -static -o pathcache-test test/pathcache.c)。
// 覚えさせて (同じパスを何度か stat)、名前を変える・消して作りなおす・リンクを付けかえる・権限を変える・
// 開いたまま消す、マウントする (root のとき)、のあとで、新しい答えになるか。だれでもは通れないディレクトリ
// (0700 など) の下も覚えて、覚えたのがほかの人でも、通れない人には EACCES になるか (root のとき、uid 1000 と 1001 の子で)。
// ないという答えも覚えるので、作る・名前を変えて入れる・リンクを張る・途中のディレクトリを作る・リンクの先を作る・
// マウントする、のあとで見えるか
// root でも root でなくても動かす (権限は root でないとき、マウントとほかの人は root のとき。vm-ctests.sh は両方)。
//   pathcache [DIR...]   DIR (既定 /tmp) ごとに、その下に pc-PID を作ってぜんぶを (tmpfs と ext4 の両方で見るため)
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s\n", name); fails++; } } while (0)

static int st(const char *p, struct stat *s) { struct stat x; for (int i = 0; i < 3; i++) if (stat(p, &x) != 0) return -errno; if (s) *s = x; return 0; }
static void touch(const char *p) { int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644); write(fd, "x", 1); close(fd); }
// 3 度引いて 3 度とも ENOENT か (2 度目からは覚えから)
static int miss3(const char *p) { struct stat x; for (int i = 0; i < 3; i++) if (stat(p, &x) == 0 || errno != ENOENT) return 0; return 1; }

// uid と gid (ほかのグループは sup だけ。-1 ならなし) になった子で st (root のとき)
static int st_as(uid_t uid, gid_t gid, int sup, const char *p) {
  pid_t pid = fork();
  if (pid == 0) {
    gid_t g = sup;
    if (setgroups(sup < 0 ? 0 : 1, &g) != 0 || setgid(gid) != 0 || setuid(uid) != 0) _exit(255);
    _exit(-st(p, 0));
  }
  int ws;
  if (pid < 0 || waitpid(pid, &ws, 0) != pid || !WIFEXITED(ws) || WEXITSTATUS(ws) == 255) return -9999;
  return -WEXITSTATUS(ws);
}

// パスの覚えから答えた数 (aios の /proc/bkl。なければ -1)
static long hits(void) {
  FILE *f = fopen("/proc/bkl", "r");
  char line[512];
  long n = -1;
  while (f && fgets(line, sizeof line, f)) {
    char *s = strstr(line, "覚えから答えた ");
    if (s) n = atol(s + strlen("覚えから答えた "));
  }
  if (f) fclose(f);
  return n;
}

static void run(const char *dir) {
  printf("pathcache: %s\n", dir);
  char base[256];
  snprintf(base, sizeof base, "%s/pc-%d", dir, getpid());
  char a[512], f[512], c[512], cf[512], l[512], g[512];
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
  char f1[512], f2[512];
  snprintf(f1, sizeof f1, "%s/f1", base); snprintf(f2, sizeof f2, "%s/f2", base);
  touch(f1); touch(f2);
  struct stat t1, t2, sl;
  st(f1, &t1); st(f2, &t2);
  symlink("f1", l);
  CHECK("link-first", st(l, &sl) == 0 && sl.st_ino == t1.st_ino);
  unlink(l); symlink("f2", l);
  CHECK("link-retargeted", st(l, &sl) == 0 && sl.st_ino == t2.st_ino);
  // ないもの: ないという答えも覚えるので、入れたあとで見えるか
  {
    char n1[512], n2[512], n3[512], n4[512], nd[512], ndf[512], nl[512], nt[512];
    snprintf(n1, sizeof n1, "%s/n1", base); snprintf(n2, sizeof n2, "%s/n2", base); snprintf(n3, sizeof n3, "%s/n3", base);
    snprintf(n4, sizeof n4, "%s/n4", base); snprintf(nd, sizeof nd, "%s/nd", base); snprintf(ndf, sizeof ndf, "%s/nd/f", base);
    snprintf(nl, sizeof nl, "%s/nl", base); snprintf(nt, sizeof nt, "%s/nt", base);
    long h0 = hits();
    CHECK("missing", miss3(n1));
    if (h0 >= 0) CHECK("missing-cached", hits() - h0 >= 2);
    touch(n1);
    CHECK("missing-created", st(n1, 0) == 0);
    CHECK("missing-rename", miss3(n2));
    touch(n3); rename(n3, n2);
    CHECK("missing-renamed-in", st(n2, 0) == 0);
    CHECK("missing-link", miss3(n4));
    link(n1, n4);
    CHECK("missing-linked-in", st(n4, 0) == 0);
    // 途中のディレクトリがない
    CHECK("missing-dir", miss3(ndf));
    mkdir(nd, 0755); touch(ndf);
    CHECK("missing-dir-made", st(ndf, 0) == 0);
    // 先のないリンク: 先を作れば見える
    symlink("nt", nl);
    CHECK("missing-target", miss3(nl));
    touch(nt);
    CHECK("missing-target-made", st(nl, 0) == 0);
    unlink(n1); unlink(n2); unlink(n4); unlink(ndf); rmdir(nd); unlink(nl); unlink(nt);
  }
  // 権限を変える: 持ち主でも x がなければ通れない (root なら通れるので、root のときはとばす)
  if (getuid() != 0) {
    CHECK("perm-before", st(cf, 0) == 0);
    chmod(c, 0600);
    CHECK("perm-denied", st(cf, 0) == -EACCES);
    chmod(c, 0755);
    CHECK("perm-again", st(cf, 0) == 0);
    // 持ち主だけが通れる (0700。ホームなど) 下も覚えて、x を外したら通れない
    chmod(c, 0700);
    long h0 = hits();
    CHECK("owner-only", st(cf, 0) == 0);
    if (h0 >= 0) CHECK("owner-only-cached", hits() - h0 >= 2);
    chmod(c, 0600);
    CHECK("owner-only-denied", st(cf, 0) == -EACCES);
    chmod(c, 0700);
    CHECK("owner-only-again", st(cf, 0) == 0);
    chmod(c, 0755);
  }
  // 2 段上が変わる: base/d1/d2/f を覚えてから d1 の名前を変える、d1 の x を外す
  {
    char d1[512], d2[512], df[512], e1[512], ef[512];
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
  // だれでもは通れないディレクトリ (root のとき): 覚えたのがだれでも、引く人が通れなければ EACCES
  // (覚えはだれが引いても同じものを返すので、通れるかは引くたびに確かめる)
  if (getuid() == 0) {
    char p[512], pf[512], pn[512];
    snprintf(p, sizeof p, "%s/p", base); snprintf(pf, sizeof pf, "%s/p/f", base); snprintf(pn, sizeof pn, "%s/p/none", base);
    mkdir(p, 0700); touch(pf);
    CHECK("gate-chown", chown(p, 1000, 1000) == 0);
    CHECK("gate-root", st(pf, 0) == 0);
    // ないという答えも、通れない人には EACCES (ないかどうかも見せない)
    CHECK("gate-missing-root", miss3(pn));
    CHECK("gate-missing-other", st_as(1001, 1001, -1, pn) == -EACCES);
    CHECK("gate-missing-owner", st_as(1000, 1000, -1, pn) == -ENOENT);
    CHECK("gate-other", st_as(1001, 1001, -1, pf) == -EACCES);
    CHECK("gate-owner", st_as(1000, 1000, -1, pf) == 0);
    CHECK("gate-other-again", st_as(1001, 1001, -1, pf) == -EACCES);
    // グループ: 0750 ならグループの人 (ほかのグループとしても) は通れる
    chmod(p, 0750);
    CHECK("gate-group", st_as(1001, 1000, -1, pf) == 0);
    CHECK("gate-group-other", st_as(1001, 1001, -1, pf) == -EACCES);
    CHECK("gate-group-sup", st_as(1001, 1001, 1000, pf) == 0);
    // 0701: ほかの人は通れても、グループの人はグループのビットで通れない
    chmod(p, 0701);
    CHECK("gate-others", st_as(1001, 1001, -1, pf) == 0);
    CHECK("gate-group-denied", st_as(1001, 1000, -1, pf) == -EACCES);
    CHECK("gate-group-sup-denied", st_as(1001, 1001, 1000, pf) == -EACCES);
    // 覚えた親のディレクトリから先だけを引くときも: root が p/s/a を引いて p と p/s を覚えたあと、p を通れない人は
    // p/s のまだ引いていない名前でも EACCES、通れる人には ENOENT とあるもの
    char s_[512], sa[512], sb[512];
    snprintf(s_, sizeof s_, "%s/p/s", base); snprintf(sa, sizeof sa, "%s/p/s/a", base); snprintf(sb, sizeof sb, "%s/p/s/b", base);
    chmod(p, 0700); mkdir(s_, 0755); touch(sa);
    CHECK("gate-parent-root", st(sa, 0) == 0);
    CHECK("gate-parent-other", st_as(1001, 1001, -1, sb) == -EACCES);
    CHECK("gate-parent-owner-missing", st_as(1000, 1000, -1, sb) == -ENOENT);
    touch(sb);
    CHECK("gate-parent-other-new", st_as(1001, 1001, -1, sb) == -EACCES);
    CHECK("gate-parent-owner-new", st_as(1000, 1000, -1, sb) == 0);
    unlink(sa); unlink(sb); rmdir(s_);
    unlink(pf); rmdir(p);
  }
  // 作ったものはその場で覚える (Linux の d_instantiate) ので、そのあとの消す・名前を変える・上書きする rename が
  // 見えるか。名前ごとに捨てるので、となりの名前の覚えは残り、上書きされた名前は新しいものになる
  {
    char na[512], nb[512], nd[512], ndf[600];
    snprintf(na, sizeof na, "%s/na", base); snprintf(nb, sizeof nb, "%s/nb", base); snprintf(nd, sizeof nd, "%s/nd", base);
    snprintf(ndf, sizeof ndf, "%s/nd/f", base);
    touch(na);
    struct stat a1, b1, a2;
    CHECK("new-file", st(na, &a1) == 0);
    touch(nb);
    CHECK("new-sibling", st(nb, &b1) == 0 && st(na, &a2) == 0 && a2.st_ino == a1.st_ino);
    CHECK("new-rename-over", rename(nb, na) == 0);
    CHECK("new-rename-over-new", st(na, &a2) == 0 && a2.st_ino == b1.st_ino);
    CHECK("new-rename-over-gone", st(nb, 0) == -ENOENT);
    unlink(na);
    CHECK("new-unlinked", st(na, 0) == -ENOENT);
    CHECK("new-dir", mkdir(nd, 0755) == 0 && st(nd, &a1) == 0 && S_ISDIR(a1.st_mode));
    touch(ndf);
    CHECK("new-dir-file", st(ndf, 0) == 0);
    unlink(ndf);
    CHECK("new-dir-file-gone", st(ndf, 0) == -ENOENT);
    rmdir(nd);
    CHECK("new-dir-gone", st(nd, 0) == -ENOENT && st(ndf, 0) == -ENOENT);
    touch(nd);
    CHECK("new-dir-now-file", st(nd, &a1) == 0 && S_ISREG(a1.st_mode) && st(ndf, 0) == -ENOTDIR);
    unlink(nd);
  }
  // 覚えたファイルを消すと、場所がすぐ戻る (覚えが inode を持ったままにしない)。リンクのディレクトリを通った
  // パスで覚えていても
  {
    char bd[512], bl[512], bf[600], blf[600];
    snprintf(bd, sizeof bd, "%s/bd", base); snprintf(bl, sizeof bl, "%s/bl", base);
    snprintf(bf, sizeof bf, "%s/big", bd); snprintf(blf, sizeof blf, "%s/big", bl);
    mkdir(bd, 0755); symlink("bd", bl);
    for (int round = 0; round < 2; round++) {
      struct statvfs v0, v1, v2;
      sync(); statvfs(bd, &v0);
      int fd = open(bf, O_CREAT | O_WRONLY | O_TRUNC, 0644);
      static char chunk[1 << 16];
      memset(chunk, 'z', sizeof chunk);
      for (int i = 0; i < 128; i++) if (write(fd, chunk, sizeof chunk) != (ssize_t)sizeof chunk) break;
      fsync(fd); close(fd);
      st(bf, 0);
      if (round == 1) st(blf, 0);
      statvfs(bd, &v1);
      unlink(bf);
      sync(); statvfs(bd, &v2);
      double used = (double)(v0.f_bfree - v1.f_bfree) * v0.f_frsize, back = (double)(v2.f_bfree - v1.f_bfree) * v0.f_frsize;
      CHECK(round ? "unlink-frees-via-link" : "unlink-frees", used >= 4e6 && back >= used * 0.9);
    }
    unlink(bl); rmdir(bd);
  }
  // リンクのディレクトリを覚えたあとの、その中の名前 (リンクの先を親として、そこから引く)
  {
    char ld[512], ll[512], lx[512], ly[512], le[512];
    snprintf(ld, sizeof ld, "%s/ld", base); snprintf(ll, sizeof ll, "%s/ll", base); snprintf(lx, sizeof lx, "%s/ll/x", base);
    snprintf(ly, sizeof ly, "%s/ll/y", base); snprintf(le, sizeof le, "%s/le", base);
    mkdir(ld, 0755); symlink("ld", ll);
    char ldx[600]; snprintf(ldx, sizeof ldx, "%s/x", ld); touch(ldx);
    CHECK("link-dir", st(ll, 0) == 0);
    CHECK("link-parent", st(lx, 0) == 0);
    CHECK("link-parent-missing", miss3(ly));
    char ldy[600]; snprintf(ldy, sizeof ldy, "%s/y", ld); touch(ldy);
    CHECK("link-parent-new", st(ly, 0) == 0);
    rename(ld, le);
    CHECK("link-parent-renamed", st(lx, 0) == -ENOENT);
    rename(le, ld);
    CHECK("link-parent-back", st(lx, 0) == 0);
    unlink(ldx); unlink(ldy); unlink(ll); rmdir(ld);
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
    char m[512], mx[512], my[512];
    snprintf(m, sizeof m, "%s/m", base); snprintf(mx, sizeof mx, "%s/m/x", base); snprintf(my, sizeof my, "%s/m/y", base);
    mkdir(m, 0755); touch(mx);
    CHECK("mount-before", st(mx, 0) == 0);
    CHECK("mount-missing-before", miss3(my));
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
}

int main(int argc, char **argv) {
  if (argc < 2) run("/tmp");
  for (int i = 1; i < argc; i++) run(argv[i]);
  printf("pathcache: %d failed\n", fails);
  return fails;
}
