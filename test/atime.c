// atime が Linux と同じか (root で、引数のディレクトリごとに: ext4 と tmpfs): stat の atime は mtime とは別、utimensat で
// 決められる、relatime (atime が mtime か ctime より前なら、読むと変わる。先なら変わらない)、書くのは変えない、
// O_NOATIME、ディレクトリを読む (getdents)、mmap、noatime と strictatime と nodiratime でマウントした tmpfs。
// 手本の Linux の VM は時計が 1970 年の起動からなので、時刻は 0 と「いまより先」だけを使う
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static char dir[256];
static char *P(const char *n) {
  static char b[4][320];
  static int k;
  char *r = b[k++ % 4];
  snprintf(r, 320, "%s/%s", dir, n);
  return r;
}
static void put(const char *p, const char *s) {
  int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  write(fd, s, strlen(s));
  close(fd);
}
static long at(const char *p) {
  struct stat s;
  return stat(p, &s) ? -errno : (long)s.st_atime;
}
static void set(const char *p, long a, long m) {
  struct timespec ts[2] = {{a, 0}, {m, 0}};
  if (m < 0)
    ts[1].tv_nsec = UTIME_OMIT;
  utimensat(AT_FDCWD, p, ts, 0);
}
static void rd(const char *p, int fl) {
  char c;
  int fd = open(p, O_RDONLY | fl);
  read(fd, &c, 1);
  close(fd);
}
// 先: いまから 1 日より先。前: いまから 1 日より前 (手本の Linux の VM の時計は 0 のあたりなので、負にもなる)
static long now_s(void) {
  struct timespec t;
  clock_gettime(CLOCK_REALTIME, &t);
  return t.tv_sec;
}
static long later(void) { return now_s() + 200000; }
static long early(void) { return now_s() - 200000; }
// atime がそのままか / いまに変わったか (前から) / いまにもどったか (先から)
static const char *how(long a, long was) { return a == was ? "kept" : a > was ? "updated" : "back to now"; }

static void one(const char *base) {
  snprintf(dir, sizeof dir, "%s/atime-t", base);
  mkdir(dir, 0755);
  put(P("f"), "hello");
  printf("%s", base[1] == 't' ? "tmpfs" : "ext4");
  set(P("f"), 12345, 67890);
  struct stat s;
  stat(P("f"), &s);
  printf(": set atime %ld mtime %ld", (long)s.st_atime, (long)s.st_mtime);
  long E = early();
  set(P("f"), E, -1);
  rd(P("f"), 0);
  printf(", read after atime early %s", how(at(P("f")), E));
  long L = later();
  set(P("f"), L, -1);
  rd(P("f"), 0);
  printf(", read with atime later %s", how(at(P("f")), L));
  set(P("f"), E, -1);
  int fd = open(P("f"), O_WRONLY);
  write(fd, "x", 1);
  close(fd);
  printf(", write %s", how(at(P("f")), E));
  set(P("f"), E, -1);
  rd(P("f"), O_NOATIME);
  printf(", O_NOATIME %s", how(at(P("f")), E));
  set(P("f"), E, -1);
  fd = open(P("f"), O_RDONLY);
  char *m = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
  close(fd);
  printf(", mmap %s", how(at(P("f")), E));
  if (m != MAP_FAILED)
    munmap(m, 4096);
  set(P("f"), E, -1);
  stat(P("f"), &s);
  printf(", stat %s", how(at(P("f")), E));
  set(dir, E, -1);
  DIR *d = opendir(dir);
  while (readdir(d))
    ;
  closedir(d);
  printf(", readdir %s\n", how(at(dir), E));
  unlink(P("f"));
  rmdir(dir);
}

// tmpfs をマウントの印つきで
static void mounted(const char *what, unsigned long fl) {
  const char *mp = "/tmp/atime-m";
  mkdir(mp, 0755);
  if (mount("tmpfs", mp, "tmpfs", fl, NULL)) {
    printf("%s: mount %d\n", what, -errno);
    return;
  }
  snprintf(dir, sizeof dir, "%s", mp);
  put(P("f"), "hello");
  mkdir(P("d"), 0755);
  long L = later(), E = early();
  set(P("f"), E, -1);
  rd(P("f"), 0);
  printf("%s: read after atime early %s", what, how(at(P("f")), E));
  set(P("f"), L, -1);
  rd(P("f"), 0);
  printf(", later %s", how(at(P("f")), L));
  set(P("d"), E, -1);
  DIR *d = opendir(P("d"));
  while (readdir(d))
    ;
  closedir(d);
  printf(", readdir %s\n", how(at(P("d")), E));
  umount(mp);
  rmdir(mp);
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("atime: not root, skipped\n");
    return 0;
  }
  for (int i = 1; i < argc; i++)
    one(argv[i]);
  if (argc == 1)
    one("/tmp");
  mounted("noatime", MS_NOATIME);
  mounted("strictatime", MS_STRICTATIME);
  mounted("nodiratime", MS_NODIRATIME);
  return 0;
}
