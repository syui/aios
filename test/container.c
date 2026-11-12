// container の作りかた (aibox --image と同じ順) を root でない人で: ユーザーの namespace で自分を 65532 に、マウントの
// namespace、根を自分の上に bind して読むだけに、/tmp に tmpfs、ファイルの bind (/etc/hosts と /dev/null)、chroot。
// test/ldiff.sh container で Linux とくらべる (Linux は nobody、aios は ai。どちらも中では 65532)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/landlock.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *err(int r) { return r == 0 ? "0" : strerror(errno); }
static int put(const char *file, const char *s) {
  int fd = open(file, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (fd < 0) return -1;
  int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1;
  close(fd);
  return r;
}
static void show_read(const char *what, const char *path) {
  char b[128] = {0};
  int fd = open(path, O_RDONLY);
  if (fd < 0) { printf("%-40s %s\n", what, strerror(errno)); return; }
  ssize_t n = read(fd, b, sizeof b - 1);
  close(fd);
  if (n < 0) { printf("%-40s read %s\n", what, strerror(errno)); return; }
  b[n] = 0;
  for (char *p = b; *p; p++) if (*p == '\n') *p = '|';
  printf("%-40s %zd \"%s\"\n", what, n, b);
}
static void show_stat(const char *what, const char *path) {
  struct stat st;
  if (stat(path, &st)) { printf("%-40s %s\n", what, strerror(errno)); return; }
  const char *t = S_ISDIR(st.st_mode) ? "dir" : S_ISCHR(st.st_mode) ? "chr" : S_ISREG(st.st_mode) ? "reg" : "other";
  printf("%-40s %s %o uid %u gid %u\n", what, t, st.st_mode & 07777, st.st_uid, st.st_gid);
}
static void show_open(const char *what, const char *path, int flags) {
  int fd = open(path, flags, 0644);
  printf("%-40s %s\n", what, fd < 0 ? strerror(errno) : "0");
  if (fd >= 0) close(fd);
}
static void show_write(const char *what, const char *path) {
  int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (fd < 0) { printf("%-40s %s\n", what, strerror(errno)); return; }
  ssize_t n = write(fd, "x\n", 2);
  close(fd);
  printf("%-40s %zd\n", what, n);
}

int main(void) {
  uid_t me = getuid();
  gid_t mygid = getgid();
  const char *tmp = getenv("TMPDIR") ? getenv("TMPDIR") : "/tmp";
  char top[256], root[300], p[400], m[64];
  snprintf(top, sizeof top, "%s/container-%d", tmp, (int)getpid());
  snprintf(root, sizeof root, "%s/rootfs", top);
  // image の根 (層をひろげたもの) のかわり: etc/hosts と dev/null はかぶせる先 (空のファイル)
  mkdir(top, 0755);
  mkdir(root, 0755);
  const char *dirs[] = {"etc", "tmp", "dev", "home", "home/nonroot", NULL};
  for (int i = 0; dirs[i]; i++) { snprintf(p, sizeof p, "%s/%s", root, dirs[i]); mkdir(p, 0755); }
  snprintf(p, sizeof p, "%s/etc/passwd", root); put(p, "nonroot:x:65532:65532::/home/nonroot:/sbin/nologin\n");
  snprintf(p, sizeof p, "%s/etc/hosts", root); put(p, "");
  snprintf(p, sizeof p, "%s/dev/null", root); put(p, "");
  snprintf(p, sizeof p, "%s/hosts", top); put(p, "127.0.0.1 localhost\n");

  // ユーザーの namespace: 外の自分を中の 65532 に
  printf("%-40s %s\n", "unshare NEWUSER", err(unshare(CLONE_NEWUSER)));
  snprintf(m, sizeof m, "65532 %d 1\n", (int)me);
  printf("%-40s %s\n", "uid_map 65532 me 1", err(put("/proc/self/uid_map", m)));
  printf("%-40s %s\n", "setgroups deny", err(put("/proc/self/setgroups", "deny")));
  snprintf(m, sizeof m, "65532 %d 1\n", (int)mygid);
  printf("%-40s %s\n", "gid_map 65532 me 1", err(put("/proc/self/gid_map", m)));
  printf("%-40s %d %d\n", "getuid getgid", (int)getuid(), (int)getgid());

  printf("%-40s %s\n", "unshare NEWNS", err(unshare(CLONE_NEWNS)));
  printf("%-40s %s\n", "make / private", err(mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)));
  // 根を自分の上に bind して、読むだけに
  printf("%-40s %s\n", "bind root on itself", err(mount(root, root, NULL, MS_BIND | MS_REC, NULL)));
  printf("%-40s %s\n", "remount root ro", err(mount(NULL, root, NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL)));
  snprintf(p, sizeof p, "%s/tmp", root);
  printf("%-40s %s\n", "tmpfs on tmp", err(mount("tmpfs", p, "tmpfs", MS_NOSUID | MS_NODEV, NULL)));
  // ファイルの bind
  char src[400];
  snprintf(src, sizeof src, "%s/hosts", top);
  snprintf(p, sizeof p, "%s/etc/hosts", root);
  printf("%-40s %s\n", "bind file on etc/hosts", err(mount(src, p, NULL, MS_BIND, NULL)));
  snprintf(p, sizeof p, "%s/dev/null", root);
  printf("%-40s %s\n", "bind /dev/null on dev/null", err(mount("/dev/null", p, NULL, MS_BIND, NULL)));
  // ちがう種類にはかぶせられない
  snprintf(p, sizeof p, "%s/home", root);
  printf("%-40s %s\n", "bind file on dir", err(mount(src, p, NULL, MS_BIND, NULL)));
  snprintf(p, sizeof p, "%s/etc/passwd", root);
  printf("%-40s %s\n", "bind dir on file", err(mount(top, p, NULL, MS_BIND, NULL)));
  printf("%-40s %s\n", "bind missing file", err(mount("/nonexistent-file", p, NULL, MS_BIND, NULL)));
  // 読むだけの根と、書ける tmpfs
  snprintf(p, sizeof p, "%s/etc/new", root);
  show_write("write etc/new (ro root)", p);
  snprintf(p, sizeof p, "%s/tmp/new", root);
  show_write("write tmp/new (tmpfs)", p);

  printf("%-40s %s\n", "chroot", err(chroot(root)));
  printf("%-40s %s\n", "chdir /home/nonroot", err(chdir("/home/nonroot")));
  char cwd[256];
  printf("%-40s %s\n", "getcwd", getcwd(cwd, sizeof cwd) ? cwd : strerror(errno));
  show_read("read /etc/hosts (bound file)", "/etc/hosts");
  show_read("read /etc/passwd", "/etc/passwd");
  show_stat("stat /etc/hosts", "/etc/hosts");
  show_stat("stat /dev/null", "/dev/null");
  show_stat("stat /tmp", "/tmp");
  show_stat("stat /home/nonroot", "/home/nonroot");
  show_write("write /dev/null", "/dev/null");
  show_read("read /dev/null", "/dev/null");
  show_write("write /etc/hosts (rw bind on ro root)", "/etc/hosts");
  show_read("read hosts again", "/etc/hosts");
  show_write("write /etc/passwd (ro)", "/etc/passwd");
  show_write("write /home/nonroot/x (ro)", "/home/nonroot/x");
  show_write("write /tmp/x", "/tmp/x");
  show_read("read /tmp/new", "/tmp/new");
  show_stat("stat /hosts (outside root)", "/hosts");
  show_stat("stat ../../.. stays in root", "../../../etc/passwd");
  // 中でもう一度: chroot はできる (ns の中の CAP_SYS_CHROOT)、ユーザーの namespace は chroot の中では作れない
  printf("%-40s %s\n", "chroot again /", err(chroot("/")));
  printf("%-40s %s\n", "unshare NEWUSER in chroot", err(unshare(CLONE_NEWUSER)));
  printf("%-40s %s\n", "umount /etc/hosts", err(umount("/etc/hosts")));
  show_read("read /etc/hosts after umount", "/etc/hosts");
  printf("%-40s %s\n", "umount /etc/passwd (not mounted)", err(umount("/etc/passwd")));
  // 書けるのは /tmp だけの砂場 (landlock) のあとの順: 読むだけのマウントは EROFS が先、書けるマウントは landlock の EACCES
  snprintf(p, sizeof p, "%s/hosts", "/tmp");
  put("/tmp/hosts", "x\n");
  printf("%-40s %s\n", "bind /tmp/hosts on /etc/hosts again", err(mount("/tmp/hosts", "/etc/hosts", NULL, MS_BIND, NULL)));
  struct landlock_ruleset_attr ra = {.handled_access_fs = LANDLOCK_ACCESS_FS_WRITE_FILE | LANDLOCK_ACCESS_FS_TRUNCATE | LANDLOCK_ACCESS_FS_MAKE_REG};
  int rs = syscall(SYS_landlock_create_ruleset, &ra, sizeof ra, 0);
  struct landlock_path_beneath_attr pb = {.allowed_access = ra.handled_access_fs, .parent_fd = open("/tmp", O_PATH | O_CLOEXEC)};
  printf("%-40s %s\n", "landlock add /tmp", err(syscall(SYS_landlock_add_rule, rs, LANDLOCK_RULE_PATH_BENEATH, &pb, 0) == 0 ? 0 : -1));
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  printf("%-40s %s\n", "landlock restrict", err(syscall(SYS_landlock_restrict_self, rs, 0) == 0 ? 0 : -1));
  show_open("ll: O_WRONLY|O_TRUNC /etc/passwd (ro)", "/etc/passwd", O_WRONLY | O_TRUNC);
  show_open("ll: O_WRONLY /etc/passwd (ro)", "/etc/passwd", O_WRONLY);
  show_open("ll: O_RDONLY|O_TRUNC /etc/passwd (ro)", "/etc/passwd", O_RDONLY | O_TRUNC);
  show_open("ll: O_CREAT /etc/new (ro)", "/etc/new", O_WRONLY | O_CREAT);
  show_open("ll: O_WRONLY /etc/hosts (rw bind)", "/etc/hosts", O_WRONLY);
  show_open("ll: O_WRONLY|O_TRUNC /etc/hosts (rw bind)", "/etc/hosts", O_WRONLY | O_TRUNC);
  show_open("ll: O_RDONLY /etc/hosts", "/etc/hosts", O_RDONLY);
  show_open("ll: O_WRONLY|O_TRUNC /tmp/new", "/tmp/new", O_WRONLY | O_TRUNC);
  show_open("ll: O_CREAT /tmp/new2", "/tmp/new2", O_WRONLY | O_CREAT);
  printf("%-40s %s\n", "ll: chroot / (sandboxed)", err(chroot("/")));
  return 0;
}
